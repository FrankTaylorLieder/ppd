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
   could be set over the API later).
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
   - Wear: each sector is erased about once per lap (~once a year), far within
     NOR endurance.
   - On boot: scan for the write head (newest valid record). Records that
     are erased (`0xFF…`) or fail the CRC mark the end of the log, which
     also covers losing power halfway through a write.
5. **Device→host API.** The serial protocol is currently one-way. Add a
   request/response command, e.g.
   `{"readings":{"since":1760000000}}` → the device streams **one JSON object
   per line** per reading, `{"t":1760000060,"c":21.43}` (epoch seconds, °C),
   followed by a terminating `{"done":{"count":N}}` line. *(JSON confirmed.)*
   Short keys keep each line to ~28 bytes, so a full year is ~15MB, which
   takes up to about a minute over USB full-speed. That's acceptable for an
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

## Screen state machine (requirements 5–9)

```
            tap                                  tap
Messages ─────────────▶ Temp           Temp ─────────────▶ Messages
Messages ── 10 min since last msg update ──▶ Temp
Temp (on it ≥ 5 min) ── msg update ──▶ Messages
Temp (on it < 5 min) ── msg update ──▶ stay on Temp; store message for later
```

Details:
- The "10 min since last message update" timer also restarts when the user
  taps back to Messages. Otherwise a tap back after a long idle would bounce
  straight back to Temp.
- *(Confirmed.)* Every received `text`/`badge` command counts as a "message
  update", including one identical to the last. No de-duplication: while the
  host keeps sending, the display stays on Messages.
- Time-sync and `readings` requests are *not* message updates.

## Implementation steps

Each step builds, flashes, and is usable on its own.

### Step 1 — Read the ADT7410 and show it on the messages screen
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

### Step 2 — Proper timekeeping
- RTC in counter mode as the monotonic clock. Swap the loop-counter timing
  from Step 1 for "sample when `rtc_seconds` crosses the next minute".
- Optional `now` field on `text`/`badge`, plus a standalone `time` command.
  The CLI sends `now` on every invocation and gets a `ppd-cli sync-time`
  subcommand.
- Doing this before the screens means Steps 3 and 5 can use real
  seconds for their timers.

### Step 3 — Temperature screen and screen switching
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

### Step 4 — Touch input
- Resistive touch read via the ADC: drive one axis, sample the other to
  detect contact. Debounce, emit one tap per press→release.
- Wire it into the state machine.

### Step 5 — Persistent storage (QSPI flash)
- Spike first: bring up QSPI on the PyPortal (pin access outside the BSP;
  JEDEC ID read to confirm the GD25Q64). If the HAL's `qspi` module proves
  troublesome, fall back to bit-banged/regular SPI to the same chip.
- Circular log as described above: boot-time scan for the head, append a
  record each minute (once time is synced), erase a sector ahead of the head.
- On boot, refill the 24h RAM ring from the log.
- Reserve a header sector holding a magic number and format version, so a
  future format change can detect and reformat old data.

### Step 6 — Readings API + CLI
- `{"readings":{"since":<epoch>}}` streaming JSON-lines response as
  described above.
- CLI: `ppd-cli readings --since <datetime|duration>` printing the JSON lines
  (optionally converting `t` to local time); it parses the human-friendly time
  with `chrono`.
- Update both READMEs (wire protocol, new commands, storage caveats).

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
