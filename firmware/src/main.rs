#![no_std]
#![no_main]

use pyportal as bsp;

use bsp::hal;
use bsp::{entry, pin_alias, DisplaySize240x320, Orientation};

use hal::clock::GenericClockController;
use hal::delay::Delay;
use hal::pac::{interrupt, CorePeripherals, Peripherals};
use hal::prelude::*;
use hal::usb::UsbBus;

use usb_device::bus::UsbBusAllocator;
use usb_device::prelude::*;
use usbd_serial::{SerialPort, USB_CLASS_CDC};

use cortex_m::interrupt::Mutex;
use cortex_m::peripheral::NVIC;

use core::cell::RefCell;
use core::fmt::Write as _;
use core::mem::MaybeUninit;

use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_6X10};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Alignment, Text, TextStyleBuilder};
use embedded_graphics::Pixel;

use heapless::{String as HString, Vec as HVec};
use micromath::F32Ext;
use serde::Deserialize;
use static_cell::StaticCell;

use panic_halt as _;

const LINE_CAPACITY: usize = 256;
// Wire-protocol cap on how many preview messages a command may carry. This is
// deliberately larger than what the screen can show at once (see
// `PREVIEW_LINE_PITCH` / the space check in the render loop below) — extras
// beyond what fits are silently ignored rather than the whole command
// failing to parse.
const MAX_PREVIEW_MESSAGES: usize = 16;
const PREVIEW_LINE_PITCH: i32 = 16;
const PREVIEW_BOTTOM_MARGIN: i32 = 8;

// Off-screen back buffer for the animated badge, sized just large enough to
// cover its motion range. A full 320x240 framebuffer would be ~150KB, most
// of this chip's 192KB RAM; scoping the buffer to only the animated region
// keeps double-buffering cheap while still eliminating on-glass flicker.
//
// Kept small (vs. an earlier, larger badge) to leave room for
// MAX_PREVIEW_MESSAGES lines of text below it.
const BADGE_W: usize = 140;
const BADGE_H: usize = 100;
const BADGE_DIAMETER: u32 = 56;

// Paces the main loop: ~25fps, smooth enough for the badge's slow wobble
// while keeping the SPI/CPU cost of redrawing it low. Also sets the wobble's
// real-time speed, since `tick` (and so the animation phase) advances once
// per loop iteration regardless of mode.
const FRAME_DELAY_MS: u16 = 40;

struct BadgeBuf {
    pixels: [Rgb565; BADGE_W * BADGE_H],
}

impl BadgeBuf {
    fn new() -> Self {
        BadgeBuf {
            pixels: [Rgb565::BLACK; BADGE_W * BADGE_H],
        }
    }

    fn clear_to_black(&mut self) {
        self.pixels.fill(Rgb565::BLACK);
    }
}

impl OriginDimensions for BadgeBuf {
    fn size(&self) -> Size {
        Size::new(BADGE_W as u32, BADGE_H as u32)
    }
}

impl DrawTarget for BadgeBuf {
    type Color = Rgb565;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            if point.x >= 0
                && (point.x as usize) < BADGE_W
                && point.y >= 0
                && (point.y as usize) < BADGE_H
            {
                self.pixels[point.y as usize * BADGE_W + point.x as usize] = color;
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Command<'a> {
    /// Show a single message full-screen.
    Text {
        msg: &'a str,
        /// Shown small and gray in the top-right corner. The firmware has
        /// no real-time clock, so the caller supplies an already-formatted
        /// string; empty (the default) draws nothing.
        #[serde(default)]
        updated_at: &'a str,
    },
    /// Show the "N waiting" badge animation with a small preview of
    /// messages below it. A count of 0 shows a static "nothing to do" icon
    /// instead of the animation. `messages` beyond how many lines fit on
    /// screen are silently ignored.
    Badge {
        count: u32,
        #[serde(default)]
        messages: HVec<&'a str, MAX_PREVIEW_MESSAGES>,
        #[serde(default)]
        updated_at: &'a str,
    },
}

enum Mode {
    /// Nothing animating; whatever was last drawn stays on screen as-is.
    Static,
    /// Notification badge, redrawn with a wobble each frame.
    Animated,
}

// `static mut`: BADGE_BUF is only ever touched from `main`'s own
// (non-returning) loop, never from an interrupt, so it doesn't need
// `'static`-reference safety the way USB_ALLOCATOR below does. It's a
// static purely to keep its ~28KB (BADGE_W * BADGE_H * size_of::<Rgb565>())
// out of the call stack rather than to satisfy the borrow checker; `unsafe`
// is how we assert we're only initializing it once, up front.
static mut BADGE_BUF: MaybeUninit<BadgeBuf> = MaybeUninit::uninit();
// `UsbDevice`/`SerialPort` (built from this allocator, below) borrow it with
// a genuine `'static` lifetime per the `usb-device` API, which a
// `Mutex<RefCell<_>>` guard (as used for USB_BUS/USB_SERIAL/etc.) can't
// provide — a guard's lifetime is tied to its borrow, not `'static`.
// `StaticCell` gives out that `&'static mut` safely (it panics rather than
// aliasing if `init` is ever called twice), so no `unsafe` is needed here.
static USB_ALLOCATOR: StaticCell<UsbBusAllocator<UsbBus>> = StaticCell::new();
static USB_BUS: Mutex<RefCell<Option<UsbDevice<UsbBus>>>> = Mutex::new(RefCell::new(None));
static USB_SERIAL: Mutex<RefCell<Option<SerialPort<UsbBus>>>> = Mutex::new(RefCell::new(None));
static LINE_BUF: Mutex<RefCell<HVec<u8, LINE_CAPACITY>>> = Mutex::new(RefCell::new(HVec::new()));
static PENDING_LINE: Mutex<RefCell<Option<HVec<u8, LINE_CAPACITY>>>> =
    Mutex::new(RefCell::new(None));

#[entry]
fn main() -> ! {
    let mut peripherals = Peripherals::take().unwrap();
    let mut core = CorePeripherals::take().unwrap();
    let mut clocks = GenericClockController::with_internal_32kosc(
        peripherals.gclk,
        &mut peripherals.mclk,
        &mut peripherals.osc32kctrl,
        &mut peripherals.oscctrl,
        &mut peripherals.nvmctrl,
    );

    let pins = bsp::Pins::new(peripherals.port).split();
    let mut delay = Delay::new(core.SYST, &mut clocks);

    let mut red_led: bsp::RedLed = pin_alias!(pins.red_led).into();

    let (mut disp, backlight, _tft_te) = pins.display.init(
        DisplaySize240x320,
        Orientation::LandscapeFlipped,
        &mut delay,
    );
    backlight.into_push_pull_output().set_high().unwrap();

    let size = disp.bounding_box().size;
    let cx = size.width as i32 / 2;
    let cy = size.height as i32 / 2;
    let style = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    let small_style = MonoTextStyle::new(&FONT_6X10, Rgb565::WHITE);
    let gray_style = MonoTextStyle::new(&FONT_6X10, Rgb565::new(18, 36, 18));

    disp.clear(Rgb565::BLACK).unwrap();
    Text::new("waiting for message...", Point::new(10, 100), style)
        .draw(&mut disp)
        .unwrap();
    let mut mode = Mode::Static;

    let bus_allocator = USB_ALLOCATOR.init(bsp::usb_allocator(
        pins.usb.usb_dm,
        pins.usb.usb_dp,
        peripherals.usb,
        &mut clocks,
        &mut peripherals.mclk,
    ));

    cortex_m::interrupt::free(|cs| {
        USB_SERIAL
            .borrow(cs)
            .replace(Some(SerialPort::new(bus_allocator)));
        USB_BUS.borrow(cs).replace(Some(
            UsbDeviceBuilder::new(bus_allocator, UsbVidPid(0x1209, 0x0001))
                .device_class(USB_CLASS_CDC)
                .strings(&[StringDescriptors::new(LangID::EN)
                    .manufacturer("mme-display")
                    .product("MME Display")
                    .serial_number("0")])
                .unwrap()
                .build(),
        ));
    });

    // `unsafe` unconditionally, straight from `cortex-m`: setting interrupt
    // priority and unmasking are raw NVIC register writes, and there's no
    // safe wrapper — the caller has to be trusted to not enable an interrupt
    // whose handler would violate what `cortex_m::interrupt::Mutex`'s
    // critical sections assume elsewhere in this file.
    unsafe {
        core.NVIC.set_priority(interrupt::USB_OTHER, 1);
        core.NVIC.set_priority(interrupt::USB_TRCPT0, 1);
        core.NVIC.set_priority(interrupt::USB_TRCPT1, 1);
        NVIC::unmask(interrupt::USB_OTHER);
        NVIC::unmask(interrupt::USB_TRCPT0);
        NVIC::unmask(interrupt::USB_TRCPT1);
    }

    // Safe: this is the only place BADGE_BUF is initialized, and it happens
    // once before the loop below (which never returns) starts using it.
    let badge_buf: &mut BadgeBuf = unsafe { BADGE_BUF.write(BadgeBuf::new()) };
    let badge_top = cy - 110;
    let badge_area = Rectangle::new(
        Point::new(cx - (BADGE_W / 2) as i32, badge_top),
        Size::new(BADGE_W as u32, BADGE_H as u32),
    );
    // Preview lines start just below the badge area.
    let badge_bottom = badge_top + BADGE_H as i32;

    let mut badge_count: u32 = 0;
    let mut tick: u32 = 0;

    loop {
        let line = cortex_m::interrupt::free(|cs| PENDING_LINE.borrow(cs).borrow_mut().take());

        if let Some(line) = line {
            red_led.set_high().unwrap();
            if let Ok(text) = core::str::from_utf8(&line) {
                if let Ok((cmd, _)) = serde_json_core::from_str::<Command>(text) {
                    match cmd {
                        Command::Text { msg, updated_at } => {
                            disp.clear(Rgb565::BLACK).unwrap();
                            Text::new(msg, Point::new(10, 100), style)
                                .draw(&mut disp)
                                .unwrap();
                            draw_timestamp(&mut disp, size, updated_at, gray_style);
                            mode = Mode::Static;
                        }
                        Command::Badge {
                            count,
                            messages,
                            updated_at,
                        } => {
                            disp.clear(Rgb565::BLACK).unwrap();
                            if count == 0 {
                                draw_sleep_icon(&mut disp, cx, cy, style);
                                mode = Mode::Static;
                            } else {
                                // Fill the remaining screen space with as many preview
                                // lines as fit, ignoring any beyond that.
                                let mut y = badge_bottom + 15;
                                let bottom_limit = size.height as i32 - PREVIEW_BOTTOM_MARGIN;
                                for m in messages.iter() {
                                    if y + FONT_6X10.character_size.height as i32 > bottom_limit {
                                        break;
                                    }
                                    Text::new(m, Point::new(10, y), small_style)
                                        .draw(&mut disp)
                                        .unwrap();
                                    y += PREVIEW_LINE_PITCH;
                                }
                                badge_count = count;
                                tick = 0;
                                mode = Mode::Animated;
                            }
                            draw_timestamp(&mut disp, size, updated_at, gray_style);
                        }
                    }
                }
            }
            red_led.set_low().unwrap();
        }

        if let Mode::Animated = mode {
            // Compose the badge into an off-screen buffer, then push it
            // to the panel in a single contiguous transfer. This avoids
            // both the on-glass black-then-red flash of clear+draw, and
            // the visible "wipe" of drawing a circle pixel-by-pixel
            // directly over the bus.
            badge_buf.clear_to_black();
            draw_notification(
                badge_buf,
                (BADGE_W / 2) as i32,
                (BADGE_H / 2) as i32,
                tick,
                badge_count,
                style,
            );
            disp.fill_contiguous(&badge_area, badge_buf.pixels.iter().copied())
                .unwrap();
            tick = tick.wrapping_add(1);
        }

        delay.delay_ms(FRAME_DELAY_MS);
    }
}

fn draw_notification<D>(
    disp: &mut D,
    cx: i32,
    baseline_y: i32,
    tick: u32,
    count: u32,
    style: MonoTextStyle<Rgb565>,
) where
    D: DrawTarget<Color = Rgb565>,
{
    // Lissajous-ish wobble: more horizontal swing than vertical bob, kept
    // within the dirty rectangle cleared by the caller.
    let bob_y = ((tick as f32) * 0.2).sin() * 14.0;
    let bob_x = ((tick as f32) * 0.12).cos() * 30.0;
    let badge_center = Point::new(cx + bob_x as i32, baseline_y + bob_y as i32);

    let _ = Circle::with_center(badge_center, BADGE_DIAMETER)
        .into_styled(PrimitiveStyle::with_fill(Rgb565::RED))
        .draw(disp);

    let mut count_text: HString<8> = HString::new();
    if count > 99 {
        let _ = count_text.push_str("99+");
    } else {
        let _ = write!(count_text, "{count}");
    }

    let centered = TextStyleBuilder::new().alignment(Alignment::Center).build();
    let _ = Text::with_text_style(
        count_text.as_str(),
        badge_center + Point::new(0, 7),
        style,
        centered,
    )
    .draw(disp);
}

fn draw_timestamp<D>(disp: &mut D, size: Size, text: &str, style: MonoTextStyle<Rgb565>)
where
    D: DrawTarget<Color = Rgb565>,
{
    if text.is_empty() {
        return;
    }
    let right_aligned = TextStyleBuilder::new().alignment(Alignment::Right).build();
    let _ = Text::with_text_style(
        text,
        Point::new(size.width as i32 - 4, 12),
        style,
        right_aligned,
    )
    .draw(disp);
}

fn draw_sleep_icon<D>(disp: &mut D, cx: i32, cy: i32, style: MonoTextStyle<Rgb565>)
where
    D: DrawTarget<Color = Rgb565>,
{
    let moon_center = Point::new(cx, cy - 20);

    let _ = Circle::with_center(moon_center, 120)
        .into_styled(PrimitiveStyle::with_fill(Rgb565::new(6, 12, 14)))
        .draw(disp);
    // Offset dark circle carves the crescent shape out of the moon above.
    let _ = Circle::with_center(Point::new(moon_center.x + 45, moon_center.y - 35), 100)
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(disp);

    let centered = TextStyleBuilder::new().alignment(Alignment::Center).build();
    let _ = Text::with_text_style("Zzz", Point::new(cx - 60, cy - 95), style, centered).draw(disp);
    let _ =
        Text::with_text_style("nothing to do", Point::new(cx, cy + 90), style, centered).draw(disp);
}

fn poll_usb() {
    cortex_m::interrupt::free(|cs| {
        if let Some(usb_dev) = USB_BUS.borrow(cs).borrow_mut().as_mut() {
            if let Some(serial) = USB_SERIAL.borrow(cs).borrow_mut().as_mut() {
                usb_dev.poll(&mut [serial]);

                let mut buf = [0u8; 64];
                if let Ok(count) = serial.read(&mut buf) {
                    let mut line_buf = LINE_BUF.borrow(cs).borrow_mut();
                    for &b in &buf[..count] {
                        match b {
                            b'\n' => {
                                let line = line_buf.clone();
                                line_buf.clear();
                                PENDING_LINE.borrow(cs).replace(Some(line));
                            }
                            b'\r' => {}
                            _ => {
                                if line_buf.push(b).is_err() {
                                    line_buf.clear();
                                }
                            }
                        }
                    }
                }
            }
        }
    });
}

#[interrupt]
fn USB_OTHER() {
    poll_usb();
}

#[interrupt]
fn USB_TRCPT0() {
    poll_usb();
}

#[interrupt]
fn USB_TRCPT1() {
    poll_usb();
}
