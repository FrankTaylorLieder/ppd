# Temperature Probe

## Requirements

1. Periodically (every 1 minute) read the internal temperature sensor of the PyPortal.
1. Store the results in persistent storage on the PyPortal.
  1. Persistent storage should roll off old readings older than 1 year.
1. Provide an API (extending the current API) to pull all readings after a specific datetime.
1. Display the current temp on the main messages screen.
1. On a screen tap, switch to a temperature view showing:
  1. The current temp (larger)
  1. A graph of the last 24h temp.
1. On a second tap, move back to the main messages screen.
1. After 10 minutes of no message updates, switch to the temperature screen.
1. When being on the temp screen for 5 minutes (or more), switch back to the messages screen when a message update comes in.


## Current state (relevant to this feature)

- `firmware/src/main.rs` is a single loop: take a pending JSON line from the
  USB interrupt, render it, animate the badge, `delay_ms(40)`. There is no
  timekeeping beyond that delay, and no device→host traffic at all.
- The firmware is **stateless**: it draws a command and forgets it. Switching
  away to a temperature screen and back means it now has to remember the last
  message command so it can redraw it.
- No real-time clock is set. Timestamps today (`updated_at`) are opaque strings
  from the host.
- The `pyportal` BSP (0.11) exposes the I2C pins (PB02/PB03, commented
  "ADT7410 temperature sensor") and the four resistive-touch pins, but has no
  drivers for either. It does **not** expose the 8MB QSPI flash pins.

## Key decisions and assumptions

1. **Which sensor.** *(Confirmed.)* The on-board **ADT7410** (I2C address `0x48`, 13/16-bit, ±0.5°C). The SAMD51
   also has an on-die sensor, but it measures chip temperature and is accurate
   only to a few degrees. Note that the ADT7410 sits on the PCB and will read a
   few degrees above room temperature because of heat from the board and
   backlight. Plan for a fixed calibration offset (a constant to start with;
   could be set over the API later). *(Done: −8.2°C, from 29.2°C read in a
   21.0°C room, as `TEMP_OFFSET_CENTI`. Readings stored before it were
   adjusted once by bumping the log format to v2; see `migrate_v1` in
   `src/templog.rs`.)*
2. **Time.** Sampling every minute, the 10/5-minute screen timers, and
   "readings after a datetime" all need a clock. Plan:
   - Run the SAMD51 **RTC peripheral in 32-bit counter mode** at 1Hz as a
     monotonic seconds-since-boot clock. It runs independently of how long
     rendering takes, unlike counting loop iterations.
   - Wall-clock time = `boot_epoch + rtc_seconds`. The host supplies
     `boot_epoch` by sending the current Unix time. The simplest way: add an
     optional `now` (epoch seconds) field to every existing command (the CLI
     already sends `updated_at`), plus an explicit `{"time":{"now":...}}`
     command.
   - The board has no battery, so time is lost on every power cycle. Readings
     taken before the first sync of a boot are kept in RAM with boot-relative
     timestamps and back-dated once a sync arrives. If no sync ever arrives,
     they are still shown on screen but not written to flash. *(Confirmed
     acceptable.)*
   - The RTC is clocked from the internal 32kHz oscillator (the clock setup
     uses `with_internal_32kosc`), so it will drift. Re-syncing on every
     command keeps this bounded. Drift only matters if the host goes quiet
     for days.
3. **Storage medium.** One reading a minute for a year is ~525,600 records.
   - Internal flash (1MB, shared with the firmware) can't hold a year even at
     2 bytes per record → **use the on-board 8MB QSPI NOR flash** (GD25Q64)
     through the HAL's `qspi` module. The BSP doesn't expose those pins, so we
     either take them from `hal::gpio::Pins` directly or patch/vendor the BSP.
     This needs a spike (see Step 4).
   - This erases anything already on that flash. *(Confirmed fine: the
     CircuitPython filesystem has already been replaced.)*
   - The SD-card slot is an alternative, but it depends on a card being
     present and adds a FAT stack. Not planned.
4. **Record format and roll-off.** Use a **circular log of fixed-size records**
   across a reserved flash region, written one record at a time (NOR allows
   programming bytes within an erased page).
   - Record: `u32` epoch seconds + `i16` temperature in centi-°C + `u16`
     CRC/marker = 8 bytes. At 8 bytes per record a year is ~4.2MB, which fits
     in 8MB with room to spare. Size the ring at, say, 400 days.
   - Roll-off falls out of the ring: when the write head reaches a 4KB sector
     that still holds old data, erase it first. To meet "older than 1 year"
     exactly, queries also filter by timestamp, so slightly older data that is
     still physically present is never returned.
   - Wear: see "Flash wear" below.
   - On boot: scan for the write head (newest valid record). Records that
     are erased (`0xFF…`) or fail the CRC mark the end of the log, which
     also covers losing power halfway through a write.
5. **Device→host API.** The serial protocol is currently one-way. Add a
   request/response command, e.g.
   `{"readings":{"since":"2026-10-14T00:00:00Z"}}` → the device streams
   **one JSON object per line** per reading,
   `{"t":"2026-10-15T10:12:18Z","c":21.43}` (UTC ISO 8601, °C), followed by
   a terminating `{"done":{"count":N}}` line. *(JSON confirmed; ISO 8601
   rather than Unix time, changed after review.)* Lines are ~38 bytes, so a
   full year is ~20MB, which takes roughly a minute over USB full-speed. That's acceptable for an
   occasional pull, and the CLI can request incrementally using `since`. The badge animation pauses while
   streaming. Writes from the main loop must handle `WouldBlock` (USB is
   serviced in the interrupt), so streaming is a small loop that retries
   until the host drains the data.
6. **Touch.** The touchscreen is 4-wire resistive, read via ADC with pin
   toggling. We only need "was the screen tapped", not coordinates, so a
   pressure/contact check with debounce (press → release = one tap) is enough.
   No calibration needed.
7. **Graph data.** Keep the last 24h (1,440 readings × 2 bytes ≈ 3KB) in a RAM
   ring so drawing never touches flash. Refill it from flash on boot. Plot it
   downsampled to the graph width (~300px → ~5 min per column, drawing the
   min–max range per column).
8. **Units.** **°C** *(confirmed)*, displayed to one decimal place.

## Flash wear

Not a concern with this design:

- **Which chip.** The log lives on the separate 8MB GD25Q64C QSPI chip, not
  the microcontroller's internal flash (which holds the firmware). The
  GD25Q64C is rated for at least 100,000 erase cycles per 4KB sector, with
  ~20 years' data retention.
- **Erases per sector.** Writing moves round the ring evenly, so wear is
  spread across every sector. Each sector is erased twice per lap: once
  when it becomes the sector kept erased ahead of the write position, and
  once more when writing moves into it. A lap takes ~370 days at one reading
  a minute, so that's ~2 erases per sector per year: 100,000 cycles would
  last tens of thousands of years.
- **Writes per record.** Each 8-byte record is programmed exactly once
  between erases, never rewritten in place.
- **Boot.** Nothing is written at boot: it only reads to find the end of
  the log.
- **Format.** The 4.2MB log area is only erased when the storage format
  changes (one erase per sector each time), which is negligible.
- **Internal flash.** The microcontroller's own flash is only written when
  new firmware is flashed, so logging adds no wear there.

For comparison, CircuitPython's FAT filesystem on the same chip rewrote the
same few sectors on every file save, which wore it far more.

## Screen state machine (requirements 5–9)

```
            tap                                  tap
Messages ─────────────▶ Temp           Temp ─────────────▶ Messages
Messages ── 10 min since last msg update ──▶ Temp
Temp (on it ≥ 5 min) ── msg update with messages ──▶ Messages
Temp (on it < 5 min, or update has no messages) ── msg update ──▶ stay on Temp; store it for later
```

Details:
- The "10 min since last message update" timer also restarts when the user
  taps back to Messages. Otherwise a tap back after a long idle would bounce
  straight back to Temp.
- *(Confirmed.)* Every received `text`/`badge` command counts as a "message
  update", including one identical to the last. No de-duplication: while the
  host keeps sending, the display stays on Messages.
- Time-sync and `readings` requests are *not* message updates.
- *(Added after review.)* Leaving the temperature screen also needs the
  update to have something to show: any `text`, or a `badge` with count ≥ 1.
  A `badge` with count 0 stays on the temperature screen, however long it's
  been up. It also doesn't restart the 10-minute idle timer, so "nothing to
  do" gives way to the temperature screen 10 minutes after the last update
  with messages (or straight away, if that was already longer ago).

## Implementation steps

Each step builds, flashes, and is usable on its own.

### Step 1 — Read the ADT7410 and show it on the messages screen *(done)*
- Set up SERCOM5 as I2C master on PB02/PB03 (the `I2cSercom` alias already
  exists in the BSP) at 100–400kHz.
- Minimal ADT7410 driver (a few functions in `main.rs`, or a small `temp`
  module): read ID register `0x0B` at boot to confirm the part is there, set
  16-bit mode in config register `0x03`, read temp registers `0x00/0x01`,
  convert (16-bit: value / 128 = °C).
- Timing for this step only: just count loop iterations (~1,500 × 40ms ≈ 1
  min, plus one read at boot). It's imprecise but needs no new peripherals.
  Step 2 replaces it.
- Show it small and gray in the **top-left**, mirroring `updated_at`
  top-right (e.g. `21.4°C`; FONT_6X10 has no `°`, so either `21.4C` or a small
  drawn circle).
- Redraw only that corner: fill a small black rectangle and draw the text, so
  it doesn't flash the rest of the screen. Also redraw it after every
  `text`/`badge` command, since those clear the screen. It doesn't overlap the
  badge area (x≈90–230) or the message text.
- If the sensor doesn't respond, show `--.-C` rather than panicking.

### Step 2 — Proper timekeeping *(done)*
- RTC in counter mode as the monotonic clock. Swap the loop-counter timing
  from Step 1 for "sample when `rtc_seconds` crosses the next minute".
- Optional `now` field on `text`/`badge`, plus a standalone `time` command.
  The CLI sends `now` on every invocation and gets a `ppd-cli sync-time`
  subcommand.
- Doing this before the screens means Steps 3 and 5 can use real
  seconds for their timers.
- As built: the CLI also sends `utc_offset` (seconds east of UTC) so the
  device can show local time. A debug clock (bottom-right) shows the device's
  local time; it was kept in Step 3 as part of the display. `LINE_CAPACITY` was
  raised from 256 to 512 bytes, since the time fields add ~36 bytes to every
  command.

### Step 3 — Temperature screen and screen switching *(done)*
- Keep the last message command (copy the raw line, ≤256 bytes, and re-parse
  it to redraw). This is the "remember what was on screen" change described
  above.
- Add a 24h RAM ring of readings (only since boot until Step 4 adds
  persistence).
- Temp screen: large current temp centered (FONT_10X20 scaled, or a bigger
  `embedded-graphics`-compatible font, e.g. the `profont` crate), with a 24h
  line/range graph below it: min/max labels, "now" at the right edge.
- Add a `Screen { Messages, Temp }` state alongside the existing
  `Mode` (Static/Animated applies only to Messages).
- Implement the state machine above for the timers *only* (10 min idle →
  Temp; ≥5 min on Temp + update → Messages), using a stub tap source for
  now. For testing, a debug serial command such as `{"screen":"temp"}` can
  stand in for taps.
- As built: the stand-in is `{"tap":{}}`, which acts exactly like a tap. The
  big reading uses `u8g2_font_logisoso58_tf` from the `u8g2-fonts` crate. The
  clock from Step 2 is kept as a permanent feature rather than removed as
  first planned: HH:MM:SS bottom-right on both screens, standing in for a
  "now" label at the graph's right-hand end. The graph draws each pixel column as a bar
  from the min to the max of the ~5 samples it covers.

### Step 4 — Touch input *(done)*
- Resistive touch read via the ADC: drive one axis, sample the other to
  detect contact. Debounce, emit one tap per press→release.
- Wire it into the state machine.
- As built: no ADC needed. XL is held low and YU has the internal pull-up,
  so a press reads as YU going low. A press counts once stable for 2 frames
  (80ms), triggering on press rather than release. The `{"tap":{}}` command
  is kept for testing remotely.

### Step 5 — Persistent storage (QSPI flash) *(done)*
- Spike first: bring up QSPI on the PyPortal (pin access outside the BSP;
  JEDEC ID read to confirm the GD25Q64). If the HAL's `qspi` module proves
  troublesome, fall back to bit-banged/regular SPI to the same chip.
- Circular log as described above: boot-time scan for the head, append a
  record each minute (once time is synced), erase a sector ahead of the head.
- On boot, refill the 24h RAM ring from the log.
- Reserve a header sector holding a magic number and format version, so a
  future format change can detect and reformat old data.
- As built: the spike read JEDEC ID `C84017` (GD25Q64C) and a quad
  erase/program/read round trip worked. The QSPI pins are taken by getting
  the PORT back from the BSP (`unsafe fn port()`) and splitting it again.
  Driver in `src/flash.rs`, log in `src/templog.rs`: header in block 0, log
  in the next 65 blocks (1,040 sectors, ~370 days), records of u32 Unix time,
  i16 centi-°C and CRC-16. Each move into a new sector erases it and the one
  after, so the boot scan finds the head at the in-use → erased boundary.
  The 24h graph is refilled at the first time sync after boot.

### Step 6 — Readings API + CLI *(done)*
- `{"readings":{"since":<epoch>}}` streaming JSON-lines response as
  described above.
- CLI: `ppd-cli readings --since <datetime|duration>` printing the JSON lines
  (optionally converting `t` to local time); it parses the human-friendly time
  with `chrono`.
- Update both READMEs (wire protocol, new commands, storage caveats).
- As built: the log reads flash a 256-byte page (32 records) at a time and
  finds the start by stepping back from the newest record. Serial writes
  retry while the USB interrupts drain the buffer, giving up after 2s with
  no progress. After a long reply, temperature sampling skips the missed
  minutes rather than catching up. CLI output goes to stdout as JSON lines,
  with the count on stderr.
- Changed after review: the readings API uses ISO 8601 UTC date-times
  instead of Unix time, both for `since` (`"2026-10-14T00:00:00Z"`, with
  `YYYY-MM-DD[THH:MM[:SS]][Z]` accepted) and for each reading's `t`. UTC
  rather than local, because the device only knows the current UTC offset,
  which would be wrong for readings from before a daylight-saving change.
  The conversion is in `src/datetime.rs`, checked against `chrono` on the
  host. The CLI shows local time by default (`--utc` for the raw lines) and
  accepts English for `--since` ("yesterday", "last monday", "3 days ago")
  via the `interim` crate. `now`/`utc_offset` on other commands stay Unix
  seconds.

Why this order: Step 1 is what you asked for first and needs only I2C. Time
(2) is a dependency of nearly everything else. Screens (3) and touch (4) are
user-visible and self-contained. Storage (5) is the riskiest piece (QSPI
bring-up outside the BSP), and the API (6) depends on it. 5 and 6 could move
ahead of 3 and 4 if data collection matters more to you than the UI.

## Resolved questions

1. Sensor: ADT7410.
2. Units: °C.
3. Every `text`/`badge` command counts as a message update, including
   repeats; while the host is sending, stay on the messages screen.
4. Wiping the QSPI flash is fine.
5. Readings taken before the first time sync after power-up are not stored.
6. API format: JSON, one object per line.
