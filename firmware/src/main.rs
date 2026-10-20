#![no_std]
#![no_main]

mod datetime;
mod flash;
mod templog;

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
use usb_device::UsbError;
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
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::{fonts, FontRenderer};

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
// Added to every reading, in centi-°C. The sensor sits on the board and
// reads warm from the board's and backlight's own heat: it read 29.2°C in a
// 21.0°C room. A fixed offset is an approximation, since that self-heating
// varies with the room and how long the board has been on.
const TEMP_OFFSET_CENTI: i16 = -820;

const TEMP_SAMPLE_INTERVAL_S: u64 = 60;
// The temperature screen graphs this many of the most recent samples (24h).
const HISTORY_LEN: usize = (24 * 60 * 60 / TEMP_SAMPLE_INTERVAL_S) as usize;

// Screen switching: show the temperature after this long with no message
// update, and only leave it for a new message once it's been up this long.
const IDLE_TO_TEMP_S: u64 = 10 * 60;
const MIN_TEMP_SCREEN_S: u64 = 5 * 60;

// Temperature screen layout. The big reading sits above the graph,
// left-aligned, with the 24h max and min to its right; the graph's labels
// sit to its left and below it, with the clock bottom-right.
//
// In `u8g2_font_logisoso58_tf` the digits are 58px tall and "°" rises to
// 66px above the baseline. Its glyphs start ~4px right of the draw point,
// so the ink lines up with the 10px left margin used elsewhere.
const BIG_TEMP_BASELINE: i32 = 74;
const BIG_TEMP_ASCENT: i32 = 66;
const BIG_TEMP_LEFT: i32 = 6;
// The min line shares the big reading's baseline; max sits this far above.
const MIN_MAX_PITCH: i32 = 28;
const GRAPH_LEFT: i32 = 34;
const GRAPH_RIGHT_MARGIN: i32 = 6;
const GRAPH_TOP: i32 = 96;
const GRAPH_BOTTOM: i32 = 216;
// Smallest temperature range the graph's y-axis spans, in centi-°C (10°C),
// centred on the readings, so small swings in a steady room stay small
// rather than filling the whole height.
const GRAPH_MIN_RANGE: i32 = 1000;
// Guide lines behind the data: one per hour back from now, and one per
// whole degree, widened to 2°C or 5°C if lines would be under 6px apart.
const GRAPH_GRID_COLOR: Rgb565 = Rgb565::new(4, 8, 4);
const GRAPH_GRID_STEPS: [i32; 3] = [100, 200, 500];
const GRAPH_GRID_MIN_SPACING: i32 = 6;

// A press must read the same for this many consecutive frames to count, to
// ride out contact bounce as a finger lands or lifts.
const TOUCH_DEBOUNCE_FRAMES: u8 = 2;

// Readings older than this are never returned, even while still in the log.
const READINGS_MAX_AGE_S: i64 = 365 * 24 * 60 * 60;
// While sending readings, give up if the host takes no data for about this
// long (e.g. it closed the port mid-stream), rather than hang.
const SERIAL_WRITE_TIMEOUT_MS: u32 = 2000;

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
    /// Acts as if the screen had been tapped: switches between the messages
    /// and temperature screens. Useful for testing without touching it.
    Tap {},
    /// Send back the stored readings taken at or after `since`, a UTC
    /// date-time like `2026-10-14T00:00:00Z` (see `datetime::parse_utc`):
    /// one `{"t":"<UTC date-time>","c":<°C>}` line each, oldest first, then
    /// `{"done":{"count":<n>}}`. A malformed `since` gets
    /// `{"error":"<message>"}` instead. Nothing else runs while they're sent.
    Readings {
        since: &'a str,
        #[serde(default)]
        now: Option<i64>,
        #[serde(default)]
        utc_offset: Option<i32>,
    },
}

/// What a received line asks the main loop to do. Extracted from `Command`
/// so the line it borrows from can then be kept (as the last message).
enum Event {
    /// A `text` or `badge` command, carrying optional clock sync.
    Message {
        /// Whether there's anything to show: always for `text`; for
        /// `badge`, a count of 1 or more (0 is "nothing to do").
        has_messages: bool,
        now: Option<i64>,
        utc_offset: Option<i32>,
    },
    Time {
        now: i64,
        utc_offset: Option<i32>,
    },
    Tap,
    Readings {
        /// Unix time, or None if it couldn't be parsed.
        since: Option<i64>,
        now: Option<i64>,
        utc_offset: Option<i32>,
    },
}

fn parse_command(line: &[u8]) -> Option<Command<'_>> {
    let text = core::str::from_utf8(line).ok()?;
    serde_json_core::from_str::<Command>(text)
        .ok()
        .map(|(cmd, _)| cmd)
}

fn parse_event(line: &[u8]) -> Option<Event> {
    Some(match parse_command(line)? {
        Command::Text {
            now, utc_offset, ..
        } => Event::Message {
            has_messages: true,
            now,
            utc_offset,
        },
        Command::Badge {
            count,
            now,
            utc_offset,
            ..
        } => Event::Message {
            has_messages: count > 0,
            now,
            utc_offset,
        },
        Command::Time { now, utc_offset } => Event::Time { now, utc_offset },
        Command::Tap {} => Event::Tap,
        Command::Readings {
            since,
            now,
            utc_offset,
        } => Event::Readings {
            since: datetime::parse_utc(since),
            now,
            utc_offset,
        },
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Messages,
    Temp,
}

impl Screen {
    /// The screen a tap switches to.
    fn other(self) -> Self {
        match self {
            Screen::Messages => Screen::Temp,
            Screen::Temp => Screen::Messages,
        }
    }
}

/// The last 24h of readings, one per sample interval, oldest overwritten
/// first. Only covers time since boot; earlier slots are None.
struct History {
    samples: [Option<i16>; HISTORY_LEN],
    next: usize,
}

impl History {
    fn new() -> Self {
        History {
            samples: [None; HISTORY_LEN],
            next: 0,
        }
    }

    fn push(&mut self, centi_c: Option<i16>) {
        self.samples[self.next] = centi_c;
        self.next = (self.next + 1) % HISTORY_LEN;
    }

    /// Stores a reading `age` samples older than the newest one, unless
    /// that slot already holds one. Used to fill in from the flash log.
    fn fill_if_empty(&mut self, age: usize, centi_c: i16) {
        let slot = &mut self.samples[(self.next + HISTORY_LEN - 1 - age) % HISTORY_LEN];
        if slot.is_none() {
            *slot = Some(centi_c);
        }
    }

    /// Sample `i` counting from the oldest (0) to the newest
    /// (HISTORY_LEN - 1).
    fn get(&self, i: usize) -> Option<i16> {
        self.samples[(self.next + i) % HISTORY_LEN]
    }
}

/// Detects presses on the resistive touchscreen, without working out where.
///
/// XL (one end of the X layer) is held low and YU (one end of the Y layer)
/// is pulled up by the chip's internal pull-up; the other two pins are left
/// floating. Untouched, the layers don't connect and YU reads high. Pressed,
/// the layers touch and YU is pulled low through the X layer, whose few
/// hundred ohms easily overpower the ~40k pull-up. That's a plain digital
/// read, so no ADC is needed.
struct Touch {
    // Only held to keep it driven low.
    _xl: hal::gpio::Pin<hal::gpio::PB01, hal::gpio::PushPullOutput>,
    yu: hal::gpio::Pin<hal::gpio::PA06, hal::gpio::PullUpInput>,
    /// Debounced state.
    pressed: bool,
    /// Consecutive frames the raw reading has disagreed with `pressed`.
    changing_frames: u8,
}

impl Touch {
    fn new(touchscreen: bsp::Touchscreen) -> Self {
        let mut xl = touchscreen.xl.into_push_pull_output();
        let _ = xl.set_low();
        Touch {
            _xl: xl,
            yu: touchscreen.yu.into_pull_up_input(),
            pressed: false,
            changing_frames: 0,
        }
    }

    /// Call once per frame. Returns true on the frame a new press is
    /// confirmed; holding the press doesn't repeat it.
    fn poll(&mut self) -> bool {
        let raw_pressed = self.yu.is_low().unwrap_or(false);
        if raw_pressed == self.pressed {
            self.changing_frames = 0;
            return false;
        }
        self.changing_frames += 1;
        if self.changing_frames < TOUCH_DEBOUNCE_FRAMES {
            return false;
        }
        self.pressed = raw_pressed;
        self.changing_frames = 0;
        self.pressed
    }
}

/// Fixed screen geometry and text styles, shared by the drawing functions.
struct Layout {
    size: Size,
    cx: i32,
    cy: i32,
    badge_area: Rectangle,
    /// Preview lines start just below the badge area.
    badge_bottom: i32,
    style: MonoTextStyle<'static, Rgb565>,
    small_style: MonoTextStyle<'static, Rgb565>,
    gray_style: MonoTextStyle<'static, Rgb565>,
    temp_style: MonoTextStyle<'static, Rgb565>,
    gray: Rgb565,
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
    /// Notification badge showing this count, redrawn with a wobble each
    /// frame.
    Animated(u32),
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

    let mut bsp_pins = bsp::Pins::new(peripherals.port);
    // The BSP has no names for the QSPI flash pins, so it drops them. Take
    // the PORT back and split it again to get at just those six. `unsafe`
    // because two `Pins` sets now exist; it's sound because the BSP's set
    // doesn't contain any of these six, so each pin still has one owner,
    // and every other pin in this second set is dropped unused.
    let qspi_pins = hal::gpio::Pins::new(unsafe { bsp_pins.port() });
    let pins = bsp_pins.split();
    let mut delay = Delay::new(core.SYST, &mut clocks);

    let mut red_led: bsp::RedLed = pin_alias!(pins.red_led).into();
    let mut touch = Touch::new(pins.touchscreen);

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

    // Blank the panel while the sensor and USB come up; the messages screen
    // is drawn once that's done.
    disp.clear(Rgb565::BLACK).unwrap();

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
    let flash = flash::Flash::new(
        &mut peripherals.mclk,
        peripherals.qspi,
        qspi_pins.pb10,
        qspi_pins.pb11,
        qspi_pins.pa08,
        qspi_pins.pa09,
        qspi_pins.pa10,
        qspi_pins.pa11,
    );
    let mut log = templog::TempLog::open(
        flash,
        || {
            let _ = Text::new(
                "preparing storage (one-time)...",
                Point::new(10, 140),
                small_style,
            )
            .draw(&mut disp);
        },
        // Readings logged before the offset existed get it applied once.
        TEMP_OFFSET_CENTI,
    );

    let mut history = History::new();
    history.push(temperature);

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
    let layout = Layout {
        size,
        cx,
        cy,
        badge_area: Rectangle::new(
            Point::new(cx - (BADGE_W / 2) as i32, badge_top),
            Size::new(BADGE_W as u32, BADGE_H as u32),
        ),
        badge_bottom: badge_top + BADGE_H as i32,
        style,
        small_style,
        gray_style,
        temp_style,
        gray,
    };

    // The raw line of the last `text`/`badge` command, re-parsed to redraw
    // the messages screen when switching back to it.
    let mut last_message: Option<HVec<u8, LINE_CAPACITY>> = None;
    let mut screen = Screen::Messages;
    let mut mode = draw_messages_screen(&mut disp, &layout, None, temperature);
    // Uptime of the last message update with something to show (or tap back
    // to the messages screen), and of when the temperature screen was last
    // shown.
    let mut last_message_secs: u64 = 0;
    let mut temp_since_secs: u64 = 0;

    let mut tick: u32 = 0;
    let mut next_sample_secs = TEMP_SAMPLE_INTERVAL_S;
    // The local time the clock last drew, to the second; None forces a
    // redraw, needed after anything that draws over its corner.
    let mut shown_clock: Option<Option<i64>> = None;
    // Whether the 24h history has been filled in from the flash log. That
    // waits for the first time sync, since placing logged readings on the
    // graph needs the current time.
    let mut history_loaded = false;

    loop {
        let uptime = clock.uptime_secs();
        let mut switch_to: Option<Screen> = None;

        let line = cortex_m::interrupt::free(|cs| PENDING_LINE.borrow(cs).borrow_mut().take());
        if let Some(line) = line {
            red_led.set_high().unwrap();
            match parse_event(&line) {
                Some(Event::Message {
                    has_messages,
                    now,
                    utc_offset,
                }) => {
                    if let Some(now) = now {
                        clock.sync(now, utc_offset);
                    }
                    last_message = Some(line);
                    // A count-0 badge ("nothing to do") doesn't hold off
                    // the switch to the temperature screen.
                    if has_messages {
                        last_message_secs = uptime;
                    }
                    match screen {
                        Screen::Messages => {
                            mode = draw_messages_screen(
                                &mut disp,
                                &layout,
                                last_message.as_deref(),
                                temperature,
                            );
                            tick = 0;
                            shown_clock = None;
                        }
                        // Only leave the temperature screen if it's been up
                        // a while and there's something to show; otherwise
                        // the update waits until the next switch back.
                        Screen::Temp
                            if has_messages && uptime - temp_since_secs >= MIN_TEMP_SCREEN_S =>
                        {
                            switch_to = Some(Screen::Messages);
                        }
                        Screen::Temp => {}
                    }
                }
                Some(Event::Time { now, utc_offset }) => clock.sync(now, utc_offset),
                Some(Event::Readings {
                    since,
                    now,
                    utc_offset,
                }) => {
                    if let Some(now) = now {
                        clock.sync(now, utc_offset);
                    }
                    match since {
                        Some(since) => {
                            let oldest = clock.now().map_or(0, |now| now - READINGS_MAX_AGE_S);
                            send_readings(log.as_mut(), since.max(oldest).max(0) as u32);
                        }
                        None => {
                            serial_write_all(
                                b"{\"error\":\"since must be a UTC date-time like 2026-10-14T00:00:00Z\"}\n",
                            );
                        }
                    }
                }
                Some(Event::Tap) => switch_to = Some(screen.other()),
                None => {}
            }
            red_led.set_low().unwrap();
        }

        if !history_loaded {
            if let Some(now) = clock.now() {
                history_loaded = true;
                if let Some(log) = log.as_mut() {
                    load_history(log, &mut history, now);
                    if screen == Screen::Temp {
                        draw_temp_readings(&mut disp, &layout, temperature, &history);
                        shown_clock = None;
                    }
                }
            }
        }

        if touch.poll() {
            switch_to = Some(screen.other());
        }

        if screen == Screen::Messages && uptime - last_message_secs >= IDLE_TO_TEMP_S {
            switch_to = Some(Screen::Temp);
        }

        if let Some(next) = switch_to {
            screen = next;
            match next {
                Screen::Messages => {
                    // Restart the idle timer, or a tap back after a long
                    // idle would bounce straight back to the temperature.
                    last_message_secs = uptime;
                    mode = draw_messages_screen(
                        &mut disp,
                        &layout,
                        last_message.as_deref(),
                        temperature,
                    );
                    tick = 0;
                }
                Screen::Temp => {
                    temp_since_secs = uptime;
                    disp.clear(Rgb565::BLACK).unwrap();
                    draw_temp_readings(&mut disp, &layout, temperature, &history);
                }
            }
            shown_clock = None;
        }

        if uptime >= next_sample_secs {
            // Skip any intervals missed while the loop was busy (e.g.
            // sending a long run of readings) rather than catching up with
            // several readings in a row.
            next_sample_secs +=
                TEMP_SAMPLE_INTERVAL_S * ((uptime - next_sample_secs) / TEMP_SAMPLE_INTERVAL_S + 1);
            // Retry the init each time if the sensor wasn't found at boot, so
            // a transient I2C failure doesn't leave it blank until reset.
            temperature = if sensor_ok || adt7410_init(&mut i2c) {
                adt7410_read(&mut i2c)
            } else {
                None
            };
            history.push(temperature);
            // Only logged once the clock is set: without it there's no
            // timestamp to store.
            if let (Some(log), Some(centi_c), Some(now)) = (log.as_mut(), temperature, clock.now())
            {
                log.append(now as u32, centi_c);
            }
            match screen {
                Screen::Messages => draw_temperature(&mut disp, temperature, temp_style),
                Screen::Temp => {
                    draw_temp_readings(&mut disp, &layout, temperature, &history);
                    shown_clock = None;
                }
            }
        }

        // Redraw the clock only when the displayed second changes.
        let local_now = clock.now().map(|t| t + clock.utc_offset as i64);
        if shown_clock != Some(local_now) {
            draw_clock(&mut disp, size, local_now, gray_style);
            shown_clock = Some(local_now);
        }

        if screen == Screen::Messages {
            if let Mode::Animated(count) = mode {
                // Compose the badge into an off-screen buffer, then push
                // it to the panel in a single contiguous transfer. This
                // avoids both the on-glass black-then-red flash of
                // clear+draw, and the visible "wipe" of drawing a circle
                // pixel-by-pixel directly over the bus.
                badge_buf.clear_to_black();
                draw_notification(
                    badge_buf,
                    (BADGE_W / 2) as i32,
                    (BADGE_H / 2) as i32,
                    tick,
                    count,
                    style,
                );
                disp.fill_contiguous(&layout.badge_area, badge_buf.pixels.iter().copied())
                    .unwrap();
                tick = tick.wrapping_add(1);
            }
        }

        delay.delay_ms(FRAME_DELAY_MS);
    }
}

/// Fills the 24h history's gaps with readings from the flash log, placing
/// each by its age relative to `now`.
fn load_history(log: &mut templog::TempLog, history: &mut History, now: i64) {
    log.for_each_newest_first(|unix_time, centi_c| {
        let age = (now - unix_time as i64).max(0) as u64 / TEMP_SAMPLE_INTERVAL_S;
        if age >= HISTORY_LEN as u64 {
            return false;
        }
        history.fill_if_empty(age as usize, centi_c);
        true
    });
}

/// Redraws the whole messages screen from the last `text`/`badge` command's
/// raw line, or the boot placeholder if there hasn't been one yet. Returns
/// whether the badge needs animating.
fn draw_messages_screen<D>(
    disp: &mut D,
    layout: &Layout,
    last_message: Option<&[u8]>,
    temperature: Option<i16>,
) -> Mode
where
    D: DrawTarget<Color = Rgb565>,
{
    let _ = disp.clear(Rgb565::BLACK);
    let mode = match last_message.and_then(parse_command) {
        Some(Command::Text {
            msg, updated_at, ..
        }) => {
            let _ = Text::new(msg, Point::new(10, 100), layout.style).draw(disp);
            draw_timestamp(disp, layout.size, updated_at, layout.gray_style);
            Mode::Static
        }
        Some(Command::Badge {
            count,
            messages,
            updated_at,
            ..
        }) => {
            let mode = if count == 0 {
                draw_sleep_icon(disp, layout.cx, layout.cy, layout.style);
                Mode::Static
            } else {
                // Fill the remaining screen space with as many preview
                // lines as fit, ignoring any beyond that.
                let mut y = layout.badge_bottom + 15;
                let bottom_limit = layout.size.height as i32 - PREVIEW_BOTTOM_MARGIN;
                for m in messages.iter() {
                    if y + FONT_6X10.character_size.height as i32 > bottom_limit {
                        break;
                    }
                    let _ = Text::new(m, Point::new(10, y), layout.small_style).draw(disp);
                    y += PREVIEW_LINE_PITCH;
                }
                Mode::Animated(count)
            };
            draw_timestamp(disp, layout.size, updated_at, layout.gray_style);
            mode
        }
        _ => {
            let _ =
                Text::new("waiting for message...", Point::new(10, 100), layout.style).draw(disp);
            Mode::Static
        }
    };
    draw_temperature(disp, temperature, layout.temp_style);
    mode
}

/// Draws the temperature screen's changing parts, the big current reading and
/// the 24h graph, clearing each area first so it can be redrawn in place.
fn draw_temp_readings<D>(disp: &mut D, layout: &Layout, temperature: Option<i16>, history: &History)
where
    D: DrawTarget<Color = Rgb565>,
{
    let width = layout.size.width as i32;
    let black = PrimitiveStyle::with_fill(Rgb565::BLACK);
    let graph_right = width - GRAPH_RIGHT_MARGIN;

    let valid = || {
        (0..HISTORY_LEN)
            .filter_map(|i| history.get(i))
            .map(i32::from)
    };
    let min_max = valid().min().zip(valid().max());

    // Big current reading, with the 24h max and min to its right.
    let _ = Rectangle::new(
        Point::new(0, BIG_TEMP_BASELINE - BIG_TEMP_ASCENT - 1),
        Size::new(layout.size.width, BIG_TEMP_ASCENT as u32 + 3),
    )
    .into_styled(black)
    .draw(disp);
    let mut text: HString<12> = HString::new();
    match temperature {
        Some(c) => {
            let _ = write!(text, "{:.1}°C", c as f32 / 100.0);
        }
        None => {
            let _ = text.push_str("--.-°C");
        }
    }
    let _ = FontRenderer::new::<fonts::u8g2_font_logisoso58_tf>().render_aligned(
        text.as_str(),
        // Positioned by baseline: `Top` would align to the font's tallest
        // glyph (accented capitals), not the digits.
        Point::new(BIG_TEMP_LEFT, BIG_TEMP_BASELINE),
        VerticalPosition::Baseline,
        HorizontalAlignment::Left,
        FontColor::Transparent(Rgb565::WHITE),
        disp,
    );
    // Right-aligned with the graph. "max 21.6°C" is ~100px wide, leaving a
    // gap of ~20px after a typical big reading (~190px); only a reading of
    // -10°C or below would be wide enough to run into it.
    if let Some((min, max)) = min_max {
        let small = FontRenderer::new::<fonts::u8g2_font_logisoso16_tf>();
        for (label, centi_c, baseline) in [
            ("max", max, BIG_TEMP_BASELINE - MIN_MAX_PITCH),
            ("min", min, BIG_TEMP_BASELINE),
        ] {
            let mut line: HString<16> = HString::new();
            let _ = write!(line, "{label} {:.1}°C", centi_c as f32 / 100.0);
            let _ = small.render_aligned(
                line.as_str(),
                Point::new(graph_right, baseline),
                VerticalPosition::Baseline,
                HorizontalAlignment::Right,
                FontColor::Transparent(layout.gray),
                disp,
            );
        }
    }

    // Graph area, including its labels.
    let _ = Rectangle::new(
        Point::new(0, GRAPH_TOP - 8),
        Size::new(
            layout.size.width,
            layout.size.height - (GRAPH_TOP - 8) as u32,
        ),
    )
    .into_styled(black)
    .draw(disp);

    let graph_w = graph_right - GRAPH_LEFT;
    let graph_h = GRAPH_BOTTOM - GRAPH_TOP;
    let _ = Rectangle::new(
        Point::new(GRAPH_LEFT - 1, GRAPH_TOP - 1),
        Size::new(graph_w as u32 + 2, graph_h as u32 + 2),
    )
    .into_styled(PrimitiveStyle::with_stroke(Rgb565::new(6, 12, 6), 1))
    .draw(disp);

    let centered = TextStyleBuilder::new().alignment(Alignment::Center).build();
    let right_aligned = TextStyleBuilder::new().alignment(Alignment::Right).build();
    let label_y = GRAPH_BOTTOM + 14;
    let _ = Text::with_text_style(
        "-24h",
        Point::new(GRAPH_LEFT, label_y),
        layout.gray_style,
        centered,
    )
    .draw(disp);
    let _ = Text::with_text_style(
        "-12h",
        Point::new(GRAPH_LEFT + graph_w / 2, label_y),
        layout.gray_style,
        centered,
    )
    .draw(disp);
    // No "now" label at the right end: the clock sits there instead.

    let grid = PrimitiveStyle::with_fill(GRAPH_GRID_COLOR);
    let hours = (HISTORY_LEN as u64 * TEMP_SAMPLE_INTERVAL_S / 3600) as i32;
    for hour in 1..hours {
        let _ = Rectangle::new(
            Point::new(GRAPH_LEFT + graph_w * hour / hours, GRAPH_TOP),
            Size::new(1, graph_h as u32),
        )
        .into_styled(grid)
        .draw(disp);
    }

    let Some((mut lo, mut hi)) = min_max else {
        let _ = Text::with_text_style(
            "no readings yet",
            Point::new(GRAPH_LEFT + graph_w / 2, GRAPH_TOP + graph_h / 2),
            layout.gray_style,
            centered,
        )
        .draw(disp);
        return;
    };
    if hi - lo < GRAPH_MIN_RANGE {
        let mid = (hi + lo) / 2;
        lo = mid - GRAPH_MIN_RANGE / 2;
        hi = lo + GRAPH_MIN_RANGE;
    }

    let mut label: HString<8> = HString::new();
    let _ = write!(label, "{:.1}", hi as f32 / 100.0);
    let _ = Text::with_text_style(
        &label,
        Point::new(GRAPH_LEFT - 4, GRAPH_TOP + 8),
        layout.gray_style,
        right_aligned,
    )
    .draw(disp);
    label.clear();
    let _ = write!(label, "{:.1}", lo as f32 / 100.0);
    let _ = Text::with_text_style(
        &label,
        Point::new(GRAPH_LEFT - 4, GRAPH_BOTTOM),
        layout.gray_style,
        right_aligned,
    )
    .draw(disp);

    let to_y = |c: i32| GRAPH_BOTTOM - 1 - (c - lo) * (graph_h - 1) / (hi - lo);

    let step = GRAPH_GRID_STEPS
        .into_iter()
        .find(|&step| step * (graph_h - 1) / (hi - lo) >= GRAPH_GRID_MIN_SPACING)
        .unwrap_or(GRAPH_GRID_STEPS[GRAPH_GRID_STEPS.len() - 1]);
    let mut degree = (lo + step - 1).div_euclid(step) * step;
    while degree <= hi {
        let y = to_y(degree);
        // Skip lines that would sit on the frame.
        if y > GRAPH_TOP && y < GRAPH_BOTTOM - 1 {
            let _ = Rectangle::new(Point::new(GRAPH_LEFT, y), Size::new(graph_w as u32, 1))
                .into_styled(grid)
                .draw(disp);
        }
        degree += step;
    }

    // One pixel column per group of samples, drawn as a bar spanning that
    // group's min..max so short spikes aren't lost to downsampling.
    let line = PrimitiveStyle::with_fill(Rgb565::new(31, 40, 0));
    for col in 0..graph_w {
        let start = col as usize * HISTORY_LEN / graph_w as usize;
        let end = (col as usize + 1) * HISTORY_LEN / graph_w as usize;
        let group = || (start..end).filter_map(|i| history.get(i)).map(i32::from);
        if let (Some(min), Some(max)) = (group().min(), group().max()) {
            let top = to_y(max);
            let _ = Rectangle::new(
                Point::new(GRAPH_LEFT + col, top),
                Size::new(1, (to_y(min) - top + 1) as u32),
            )
            .into_styled(line)
            .draw(disp);
        }
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

/// Draws the local time (HH:MM:SS) small in the bottom-right corner, given
/// seconds since the Unix epoch in local time, or "--:--:--" before the host
/// has sent the time. Shown on both screens; on the temperature screen it
/// also marks the right-hand ("now") end of the graph.
fn draw_clock<D>(disp: &mut D, size: Size, local_secs: Option<i64>, style: MonoTextStyle<Rgb565>)
where
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
    Some((raw * 100 / 128) as i16 + TEMP_OFFSET_CENTI)
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

/// Streams the readings at or after `since` to the host (see
/// `Command::Readings`). With no working flash, just sends a count of 0.
fn send_readings(log: Option<&mut templog::TempLog>, since: u32) {
    let mut count: u32 = 0;
    if let Some(log) = log {
        log.for_each_since(since, |unix_time, centi_c| {
            let mut line: HString<48> = HString::new();
            let sign = if centi_c < 0 { "-" } else { "" };
            let abs = (centi_c as i32).abs();
            let _ = write!(
                line,
                "{{\"t\":\"{}\",\"c\":{sign}{}.{:02}}}\n",
                datetime::format_utc(unix_time as i64),
                abs / 100,
                abs % 100
            );
            count += 1;
            serial_write_all(line.as_bytes())
        });
    }
    let mut done: HString<32> = HString::new();
    let _ = write!(done, "{{\"done\":{{\"count\":{count}}}}}\n");
    serial_write_all(done.as_bytes());
}

/// Writes all of `data` to the USB serial port, waiting for the host to
/// take it. The USB interrupts do the actual sending, so this just retries
/// outside the critical section until there's room. Returns false if the
/// host stops taking data for SERIAL_WRITE_TIMEOUT_MS.
fn serial_write_all(mut data: &[u8]) -> bool {
    // ~0.1ms at the 120MHz core clock.
    const RETRY_CYCLES: u32 = 12_000;
    let mut idle_retries: u32 = 0;
    while !data.is_empty() {
        let result =
            cortex_m::interrupt::free(|cs| match USB_SERIAL.borrow(cs).borrow_mut().as_mut() {
                Some(serial) => serial.write(data),
                None => Err(UsbError::InvalidState),
            });
        match result {
            Ok(written) => {
                data = &data[written..];
                idle_retries = 0;
            }
            Err(UsbError::WouldBlock) => {
                idle_retries += 1;
                if idle_retries > SERIAL_WRITE_TIMEOUT_MS * 10 {
                    return false;
                }
                cortex_m::asm::delay(RETRY_CYCLES);
            }
            Err(_) => return false,
        }
    }
    true
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
