//! Persistent log of temperature readings in the QSPI flash: a ring of
//! fixed-size records covering a little over a year, oldest overwritten
//! first.
//!
//! Layout: the first 64KB block holds only a header identifying the format;
//! the log is the next `LOG_BLOCKS` blocks. Each record is 8 bytes: Unix
//! time (u32 LE), temperature in centi-°C (i16 LE), and a CRC-16 of those 6
//! bytes (u16 LE).
//!
//! Finding the end of the log at boot: whenever writing moves into a new
//! sector, the sector after it is erased too. So the sectors in use form one
//! run ending at the sector being written, followed by an erased one, and
//! the newest record is the last non-erased slot in the last sector of that
//! run. A record half-written when power was lost is left non-erased but
//! fails its CRC, so it's skipped on reading and writing carries on after
//! it.

use crate::flash::{self, Flash};

/// Identifies the format. Change it whenever the layout or record format
/// changes, so old data is wiped rather than misread.
const HEADER: &[u8] = b"ppd-templog-v1";
const HEADER_ADDR: u32 = 0;

const LOG_START: u32 = flash::BLOCK_SIZE;
// 65 blocks = 1040 sectors = 532,480 records: a minute apiece is ~370 days,
// leaving over a year after the sector kept erased and the one part-written.
const LOG_BLOCKS: u32 = 65;
const LOG_SECTORS: u32 = LOG_BLOCKS * flash::BLOCK_SIZE / flash::SECTOR_SIZE;
const RECORD_SIZE: u32 = 8;
const RECORDS_PER_SECTOR: u32 = flash::SECTOR_SIZE / RECORD_SIZE;
const LOG_RECORDS: u32 = LOG_SECTORS * RECORDS_PER_SECTOR;

const ERASED: [u8; RECORD_SIZE as usize] = [0xFF; RECORD_SIZE as usize];

pub struct TempLog {
    flash: Flash,
    /// Index of the next record slot to write, 0..LOG_RECORDS.
    head: u32,
}

impl TempLog {
    /// Opens the log, first formatting the flash if it doesn't hold one
    /// (calling `on_format` beforehand, as that takes several seconds).
    /// Returns None if the flash chip doesn't respond.
    pub fn open(mut flash: Flash, on_format: impl FnOnce()) -> Option<Self> {
        let id = flash.jedec_id();
        if id == [0x00; 3] || id == [0xFF; 3] {
            return None;
        }

        let mut header = [0u8; HEADER.len()];
        flash.read(HEADER_ADDR, &mut header);
        if header != HEADER {
            on_format();
            format(&mut flash);
        }

        let head = find_head(&mut flash);
        Some(TempLog { flash, head })
    }

    pub fn append(&mut self, unix_time: u32, centi_c: i16) {
        let sector = self.head / RECORDS_PER_SECTOR;
        if self.head % RECORDS_PER_SECTOR == 0 {
            // Entering a new sector. It should already be erased (it was
            // the one kept erased ahead), but erasing it again costs ~50ms
            // every ~8.5h and recovers from anything unexpected there. Then
            // keep the next one erased, which is how `find_head` finds the
            // end of the log.
            self.flash.erase_sector(sector_addr(sector));
            self.flash
                .erase_sector(sector_addr((sector + 1) % LOG_SECTORS));
        }
        self.flash
            .program(record_addr(self.head), &encode(unix_time, centi_c));
        self.head = (self.head + 1) % LOG_RECORDS;
    }

    /// Calls `f` with each reading `(unix_time, centi_c)`, newest first,
    /// until `f` returns false or the log runs out.
    pub fn for_each_newest_first(&mut self, mut f: impl FnMut(u32, i16) -> bool) {
        for back in 1..LOG_RECORDS {
            let index = (self.head + LOG_RECORDS - back) % LOG_RECORDS;
            let mut record = [0u8; RECORD_SIZE as usize];
            self.flash.read(record_addr(index), &mut record);
            // Going backwards from the head, the first erased slot is the
            // sector kept erased ahead of it (or never-written flash), so
            // everything older has been overwritten.
            if record == ERASED {
                return;
            }
            if let Some((unix_time, centi_c)) = decode(&record) {
                if !f(unix_time, centi_c) {
                    return;
                }
            }
        }
    }
}

fn sector_addr(sector: u32) -> u32 {
    LOG_START + sector * flash::SECTOR_SIZE
}

fn record_addr(index: u32) -> u32 {
    LOG_START + index * RECORD_SIZE
}

fn is_erased(flash: &mut Flash, index: u32) -> bool {
    let mut record = [0u8; RECORD_SIZE as usize];
    flash.read(record_addr(index), &mut record);
    record == ERASED
}

/// Erases the header and the whole log, then writes the header. The header
/// goes last so that an interrupted format is redone on the next boot.
fn format(flash: &mut Flash) {
    flash.erase_sector(HEADER_ADDR);
    for block in 0..LOG_BLOCKS {
        flash.erase_block(LOG_START + block * flash::BLOCK_SIZE);
    }
    flash.program(HEADER_ADDR, HEADER);
}

/// Finds the next slot to write: just after the last non-erased slot of
/// the last in-use sector before an erased one. An empty log starts at 0.
fn find_head(flash: &mut Flash) -> u32 {
    let mut in_use: [bool; LOG_SECTORS as usize] = [false; LOG_SECTORS as usize];
    for (sector, used) in in_use.iter_mut().enumerate() {
        *used = !is_erased(flash, sector as u32 * RECORDS_PER_SECTOR);
    }
    let Some(sector) =
        (0..LOG_SECTORS).find(|&s| in_use[s as usize] && !in_use[((s + 1) % LOG_SECTORS) as usize])
    else {
        return 0;
    };

    let first = sector * RECORDS_PER_SECTOR;
    let last_used = (first..first + RECORDS_PER_SECTOR)
        .rev()
        .find(|&i| !is_erased(flash, i))
        .unwrap_or(first);
    (last_used + 1) % LOG_RECORDS
}

fn encode(unix_time: u32, centi_c: i16) -> [u8; RECORD_SIZE as usize] {
    let mut record = [0u8; RECORD_SIZE as usize];
    record[0..4].copy_from_slice(&unix_time.to_le_bytes());
    record[4..6].copy_from_slice(&centi_c.to_le_bytes());
    let crc = crc16(&record[0..6]);
    record[6..8].copy_from_slice(&crc.to_le_bytes());
    record
}

fn decode(record: &[u8; RECORD_SIZE as usize]) -> Option<(u32, i16)> {
    if crc16(&record[0..6]).to_le_bytes() != record[6..8] {
        return None;
    }
    let unix_time = u32::from_le_bytes([record[0], record[1], record[2], record[3]]);
    let centi_c = i16::from_le_bytes([record[4], record[5]]);
    Some((unix_time, centi_c))
}

/// CRC-16/CCITT-FALSE.
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}
