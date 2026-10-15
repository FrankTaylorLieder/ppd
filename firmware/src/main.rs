#![no_std]
#![no_main]

use pyportal as bsp;

use bsp::hal;
use bsp::{entry, pin_alias, DisplaySize240x320, Orientation};

use hal::clock::GenericClockController;
use hal::delay::Delay;
use hal::ehal::i2c::I2c as EhalI2c;
use hal::pac::{interrupt, CorePeripherals, Peripherals};
use hal::prelude::*;
use hal::rtc::{Count32Mode, Rtc};
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
use embedded_graphics::text::{Alignment, Baseline, Text, TextStyleBuilder};
use embedded_graphics::Pixel;

use heapless::{String as HString, Vec as HVec};
use micromath::F32Ext;
use serde::Deserialize;
use static_cell::StaticCell;

use panic_halt as _;

const LINE_CAPACITY: usize = 512;
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

// On-board ADT7410 temperature sensor, on the BSP's I2C pins.
const ADT7410_ADDR: u8 = 0x48;
const ADT7410_REG_TEMP: u8 = 0x00;
const ADT7410_REG_CONFIG: u8 = 0x03;
const ADT7410_REG_ID: u8 = 0x0B;
// Upper 5 bits of the ID register are the fixed manufacturer ID; the low 3
// are a silicon revision we don't care about.
const ADT7410_ID_MASK: u8 = 0xF8;
const ADT7410_ID: u8 = 0xC8;
// Config register value selecting 16-bit resolution (1/128 °C per LSB),
// continuous conversion.
const ADT7410_CONFIG_16BIT: u8 = 0x80;
// A 16-bit conversion takes up to 240ms; wait this long after configuring
// before the first read so it isn't stale.
const ADT7410_FIRST_CONVERSION_MS: u16 = 250;

const TEMP_SAMPLE_INTERVAL_S: u64 = 60;

// `with_internal_32kosc` routes the internal ultra-low-power oscillator's
// 1.024kHz output to the RTC, which counts it with no prescaler.
const RTC_HZ: u32 = 1024;

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
        #[serde(default)]
        now: Option<i64>,
        #[serde(default)]
        utc_offset: Option<i32>,
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
        #[serde(default)]
        now: Option<i64>,
        #[serde(default)]
        utc_offset: Option<i32>,
    },
    /// Set the device clock without changing what's on screen. `text` and
    /// `badge` take the same optional `now`/`utc_offset` fields, so a host
    /// that sends those regularly never needs this.
    Time {
        /// Current Unix time, in seconds.
        now: i64,
        /// Local time's offset from UTC in seconds (e.g. 3600 for UTC+1),
        /// used only for displaying local time. Readings are kept in UTC.
        #[serde(default)]
        utc_offset: Option<i32>,
    },
}

/// Seconds since boot, from the RTC, plus the wall-clock time once the host
/// has told us what it is. There's no battery, so the wall-clock time is
/// lost on every power cycle until the host sends it again.
struct Clock {
    rtc: Rtc<Count32Mode>,
    // The 32-bit counter wraps after ~48 days at 1024Hz, so it's extended to
    // 64 bits here. `ticks` must be called more often than that, which the
    // main loop does every frame.
    last_count: u32,
    total_ticks: u64,
    /// Unix time at uptime zero, once synced.
    boot_epoch: Option<i64>,
    utc_offset: i32,
}

impl Clock {
    fn new(rtc: Rtc<Count32Mode>) -> Self {
        let last_count = rtc.count32();
        Clock {
            rtc,
            last_count,
            total_ticks: 0,
            boot_epoch: None,
            utc_offset: 0,
        }
    }

    fn ticks(&mut self) -> u64 {
        let count = self.rtc.count32();
        self.total_ticks += count.wrapping_sub(self.last_count) as u64;
        self.last_count = count;
        self.total_ticks
    }

    fn uptime_secs(&mut self) -> u64 {
        self.ticks() / RTC_HZ as u64
    }

    fn sync(&mut self, now: i64, utc_offset: Option<i32>) {
        self.boot_epoch = Some(now - self.uptime_secs() as i64);
        if let Some(offset) = utc_offset {
            self.utc_offset = offset;
        }
    }

    /// Current Unix time, or None if the host hasn't sent it since boot.
    fn now(&mut self) -> Option<i64> {
        let uptime = self.uptime_secs() as i64;
        self.boot_epoch.map(|boot| boot + uptime)
    }
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
    let gray = Rgb565::new(18, 36, 18);
    let gray_style = MonoTextStyle::new(&FONT_6X10, gray);
    let temp_style = MonoTextStyle::new(&FONT_10X20, gray);

    disp.clear(Rgb565::BLACK).unwrap();
    Text::new("waiting for message...", Point::new(10, 100), style)
        .draw(&mut disp)
        .unwrap();
    let mut mode = Mode::Static;

    // `count32_mode` alone leaves COUNT read-synchronization off, in which
    // case the SAMD51 doesn't guarantee reads of the counter are correct;
    // `into_count32_mode` turns it on.
    let rtc =
        Rtc::count32_mode(peripherals.rtc, RTC_HZ.Hz(), &mut peripherals.mclk).into_count32_mode();
    let mut clock = Clock::new(rtc);

    let mut i2c = bsp::i2c_master(
        &mut clocks,
        100.kHz(),
        peripherals.sercom5,
        &mut peripherals.mclk,
        pins.i2c.sda,
        pins.i2c.scl,
    );
    let sensor_ok = adt7410_init(&mut i2c);
    if sensor_ok {
        delay.delay_ms(ADT7410_FIRST_CONVERSION_MS);
    }
    let mut temperature = if sensor_ok {
        adt7410_read(&mut i2c)
    } else {
        None
    };
    draw_temperature(&mut disp, temperature, temp_style);

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
    let mut next_sample_secs = TEMP_SAMPLE_INTERVAL_S;
    // What the debug clock last drew; None forces a redraw.
    let mut shown_clock: Option<Option<i64>> = None;

    loop {
        let line = cortex_m::interrupt::free(|cs| PENDING_LINE.borrow(cs).borrow_mut().take());

        if let Some(line) = line {
            red_led.set_high().unwrap();
            if let Ok(text) = core::str::from_utf8(&line) {
                if let Ok((cmd, _)) = serde_json_core::from_str::<Command>(text) {
                    match cmd {
                        Command::Text {
                            msg,
                            updated_at,
                            now,
                            utc_offset,
                        } => {
                            if let Some(now) = now {
                                clock.sync(now, utc_offset);
                            }
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
                            now,
                            utc_offset,
                        } => {
                            if let Some(now) = now {
                                clock.sync(now, utc_offset);
                            }
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
                        Command::Time { now, utc_offset } => clock.sync(now, utc_offset),
                    }
                }
            }
            // `text`/`badge` clear the screen, so put the temperature and
            // clock back. Harmless after `time`, which doesn't.
            draw_temperature(&mut disp, temperature, temp_style);
            shown_clock = None;
            red_led.set_low().unwrap();
        }

        if clock.uptime_secs() >= next_sample_secs {
            next_sample_secs += TEMP_SAMPLE_INTERVAL_S;
            // Retry the init each time if the sensor wasn't found at boot, so
            // a transient I2C failure doesn't leave it blank until reset.
            temperature = if sensor_ok || adt7410_init(&mut i2c) {
                adt7410_read(&mut i2c)
            } else {
                None
            };
            draw_temperature(&mut disp, temperature, temp_style);
        }

        // Debug clock: redraw only when the displayed second changes.
        let local_now = clock.now().map(|t| t + clock.utc_offset as i64);
        if shown_clock != Some(local_now) {
            draw_debug_clock(&mut disp, size, local_now, gray_style);
            shown_clock = Some(local_now);
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

/// Temporary: draws the device's own idea of the local time (HH:MM:SS) in
/// the bottom-right corner, or "--:--:--" before the host has sent it, so
/// the RTC can be checked against the host's clock. To be removed once the
/// temperature screen uses the clock.
fn draw_debug_clock<D>(
    disp: &mut D,
    size: Size,
    local_secs: Option<i64>,
    style: MonoTextStyle<Rgb565>,
) where
    D: DrawTarget<Color = Rgb565>,
{
    let w = size.width as i32;
    let h = size.height as i32;
    // Room for "HH:MM:SS" in FONT_6X10, below the last preview line.
    let _ = Rectangle::new(Point::new(w - 54, h - 14), Size::new(54, 14))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(disp);

    let mut text: HString<8> = HString::new();
    match local_secs {
        Some(t) => {
            let day_secs = t.rem_euclid(86_400);
            let _ = write!(
                text,
                "{:02}:{:02}:{:02}",
                day_secs / 3600,
                day_secs / 60 % 60,
                day_secs % 60
            );
        }
        None => {
            let _ = text.push_str("--:--:--");
        }
    }
    let right_aligned = TextStyleBuilder::new()
        .alignment(Alignment::Right)
        .baseline(Baseline::Bottom)
        .build();
    let _ = Text::with_text_style(
        text.as_str(),
        Point::new(w - 4, h - 3),
        style,
        right_aligned,
    )
    .draw(disp);
}

/// Confirms an ADT7410 is present and switches it to 16-bit mode. Returns
/// false if it doesn't respond or isn't the expected part.
fn adt7410_init(i2c: &mut bsp::I2c) -> bool {
    let mut id = [0u8; 1];
    // Fully qualified: `hal::prelude` also brings the embedded-hal 0.2 I2C
    // traits into scope, which have methods of the same names.
    if EhalI2c::write_read(i2c, ADT7410_ADDR, &[ADT7410_REG_ID], &mut id).is_err()
        || id[0] & ADT7410_ID_MASK != ADT7410_ID
    {
        return false;
    }
    EhalI2c::write(
        i2c,
        ADT7410_ADDR,
        &[ADT7410_REG_CONFIG, ADT7410_CONFIG_16BIT],
    )
    .is_ok()
}

/// Latest reading in hundredths of a degree Celsius, or None on I2C error.
fn adt7410_read(i2c: &mut bsp::I2c) -> Option<i16> {
    let mut raw = [0u8; 2];
    EhalI2c::write_read(i2c, ADT7410_ADDR, &[ADT7410_REG_TEMP], &mut raw).ok()?;
    // 16-bit two's complement, 1/128 °C per LSB.
    let raw = i16::from_be_bytes(raw) as i32;
    Some((raw * 100 / 128) as i16)
}

/// Draws the temperature in the top-left corner, opposite `draw_timestamp`
/// on the right. Clears its own area first so it can be
/// updated without redrawing the rest of the screen.
fn draw_temperature<D>(disp: &mut D, centi_c: Option<i16>, style: MonoTextStyle<Rgb565>)
where
    D: DrawTarget<Color = Rgb565>,
{
    // Sized for FONT_10X20 and the longest plausible reading, e.g. "-40.0C".
    // Stays clear of the badge area, which starts at x=90.
    let _ = Rectangle::new(Point::new(0, 0), Size::new(66, 24))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(disp);

    let mut text: HString<8> = HString::new();
    match centi_c {
        Some(c) => {
            let _ = write!(text, "{:.1}C", c as f32 / 100.0);
        }
        None => {
            let _ = text.push_str("--.-C");
        }
    }
    let _ = Text::with_baseline(text.as_str(), Point::new(4, 2), style, Baseline::Top).draw(disp);
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
