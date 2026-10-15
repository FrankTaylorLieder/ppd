//! Driver for the PyPortal's on-board 8MB QSPI NOR flash (a GD25Q64C or
//! compatible W25Q64), on top of the HAL's one-shot QSPI peripheral.
//!
//! NOR flash basics this relies on: erasing sets a whole 4KB sector to 0xFF;
//! programming can only clear bits (1 -> 0), up to one 256-byte page per
//! command; erase and program both run in the background on the chip, so
//! every one is preceded by a write-enable and followed by waiting for the
//! chip to report it's no longer busy.

use crate::hal;
use hal::gpio::{AnyPin, PA08, PA09, PA10, PA11, PB10, PB11};
use hal::pac;
use hal::qspi::{Command, OneShot, Qspi};

pub const SECTOR_SIZE: u32 = 4096;
pub const BLOCK_SIZE: u32 = 64 * 1024;
pub const PAGE_SIZE: u32 = 256;

// Status register 1 bit 0: an erase or program is still in progress.
const STATUS_BUSY: u8 = 0x01;
// Status register 2 bit 1: quad I/O enabled, needed by the HAL's quad
// read/program commands.
const STATUS2_QUAD_ENABLE: u8 = 0x02;

pub struct Flash {
    qspi: Qspi<OneShot>,
}

impl Flash {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mclk: &mut pac::Mclk,
        qspi: pac::Qspi,
        sck: impl AnyPin<Id = PB10>,
        cs: impl AnyPin<Id = PB11>,
        io0: impl AnyPin<Id = PA08>,
        io1: impl AnyPin<Id = PA09>,
        io2: impl AnyPin<Id = PA10>,
        io3: impl AnyPin<Id = PA11>,
    ) -> Self {
        let mut flash = Flash {
            qspi: Qspi::new(mclk, qspi, sck, cs, io0, io1, io2, io3),
        };
        flash.ensure_quad_enabled();
        flash
    }

    /// Manufacturer, memory type and capacity bytes, e.g. `[0xC8, 0x40,
    /// 0x17]` for a GD25Q64C.
    pub fn jedec_id(&self) -> [u8; 3] {
        let mut id = [0u8; 3];
        let _ = self.qspi.read_command(Command::ReadId, &mut id);
        id
    }

    pub fn read(&mut self, addr: u32, buf: &mut [u8]) {
        self.qspi.read_memory(addr, buf);
    }

    /// Erases the 4KB sector containing `addr` to all 0xFF.
    pub fn erase_sector(&mut self, addr: u32) {
        self.write_enable();
        let _ = self
            .qspi
            .erase_command(Command::EraseSector, addr & !(SECTOR_SIZE - 1));
        self.wait_ready();
    }

    /// Erases the 64KB block containing `addr` to all 0xFF. Much faster
    /// than erasing its 16 sectors one by one.
    pub fn erase_block(&mut self, addr: u32) {
        self.write_enable();
        let _ = self
            .qspi
            .erase_command(Command::EraseBlock, addr & !(BLOCK_SIZE - 1));
        self.wait_ready();
    }

    /// Programs `data` at `addr`, which must already be erased. Must not
    /// cross a 256-byte page boundary: the chip would wrap round to the
    /// start of the page.
    pub fn program(&mut self, addr: u32, data: &[u8]) {
        debug_assert!(addr % PAGE_SIZE + data.len() as u32 <= PAGE_SIZE);
        self.write_enable();
        self.qspi.write_memory(addr, data);
        self.wait_ready();
    }

    fn status(&self, command: Command) -> u8 {
        let mut status = [0u8; 1];
        let _ = self.qspi.read_command(command, &mut status);
        status[0]
    }

    fn write_enable(&self) {
        let _ = self.qspi.run_command(Command::WriteEnable);
    }

    fn wait_ready(&self) {
        while self.status(Command::ReadStatus) & STATUS_BUSY != 0 {}
    }

    /// Sets the (non-volatile) quad-enable bit if it isn't already. Written
    /// via the two-byte form of Write Status (0x01), which both GigaDevice
    /// and Winbond parts accept.
    fn ensure_quad_enabled(&mut self) {
        let status2 = self.status(Command::ReadStatus2);
        if status2 & STATUS2_QUAD_ENABLE != 0 {
            return;
        }
        let status1 = self.status(Command::ReadStatus);
        self.write_enable();
        let _ = self.qspi.write_command(
            Command::WriteStatus,
            &[status1, status2 | STATUS2_QUAD_ENABLE],
        );
        self.wait_ready();
    }
}
