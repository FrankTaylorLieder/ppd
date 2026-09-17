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
use core::mem::MaybeUninit;

use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;

use heapless::Vec as HVec;
use serde::Deserialize;

use panic_halt as _;

const LINE_CAPACITY: usize = 256;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Command<'a> {
    Text { msg: &'a str },
}

static mut USB_ALLOCATOR: MaybeUninit<UsbBusAllocator<UsbBus>> = MaybeUninit::uninit();
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

    let style = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    disp.clear(Rgb565::BLACK).unwrap();
    Text::new("waiting for message...", Point::new(10, 100), style)
        .draw(&mut disp)
        .unwrap();

    let bus_allocator = unsafe {
        USB_ALLOCATOR.write(bsp::usb_allocator(
            pins.usb.usb_dm,
            pins.usb.usb_dp,
            peripherals.usb,
            &mut clocks,
            &mut peripherals.mclk,
        ))
    };

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

    unsafe {
        core.NVIC.set_priority(interrupt::USB_OTHER, 1);
        core.NVIC.set_priority(interrupt::USB_TRCPT0, 1);
        core.NVIC.set_priority(interrupt::USB_TRCPT1, 1);
        NVIC::unmask(interrupt::USB_OTHER);
        NVIC::unmask(interrupt::USB_TRCPT0);
        NVIC::unmask(interrupt::USB_TRCPT1);
    }

    loop {
        let line = cortex_m::interrupt::free(|cs| PENDING_LINE.borrow(cs).borrow_mut().take());

        if let Some(line) = line {
            red_led.set_high().unwrap();
            if let Ok(text) = core::str::from_utf8(&line) {
                if let Ok((Command::Text { msg }, _)) =
                    serde_json_core::from_str::<Command>(text)
                {
                    disp.clear(Rgb565::BLACK).unwrap();
                    Text::new(msg, Point::new(10, 100), style)
                        .draw(&mut disp)
                        .unwrap();
                }
            }
            red_led.set_low().unwrap();
        }
    }
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
