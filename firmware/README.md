# ppd-firmware

Bare-metal Rust firmware for the Adafruit PyPortal M4 Express (ATSAMD51J20A).
Brings up the parallel ILI9341 display and a USB CDC-ACM serial port, then
renders messages sent as newline-delimited JSON over that serial port. See
`../cli` for the host-side tool that talks to it.

## Prerequisites

Install the ARM Cortex-M4F target and the two flashing tools once:

```sh
rustup target add thumbv7em-none-eabihf
cargo install cargo-hf2   # provides `cargo run` / `cargo hf2`
cargo install hf2-cli     # provides the `hf2` binary the .cargo/config.toml runner calls
```

The board's UF2 bootloader talks HF2 over USB HID, so no separate debug
probe (J-Link, etc.) is needed — just the USB cable.

## Putting the board into bootloader mode

Double-tap the small reset button next to the PyPortal's USB port. A
`PORTALBOOT` drive should mount, and its LED will pulse. You can verify from
the host:

```sh
ls /Volumes/PORTALBOOT      # macOS
cat /Volumes/PORTALBOOT/INFO_UF2.TXT
```

If nothing mounts, check the USB cable actually carries data (some
cables/ports are power-only) and that the PyPortal's power light is on.

## Building

```sh
cargo build --release
```

The target and linker flags are pinned in `.cargo/config.toml`
(`thumbv7em-none-eabihf`, `--nmagic`, `-Tlink.x`), so a plain `cargo build`
from this directory does the right thing. `memory.x` (reserving the first
16K of flash for the bootloader) comes from the `pyportal` board-support
crate's own build script — this crate doesn't need its own copy.

## Flashing

With the board in bootloader mode (see above):

```sh
cargo run --release
```

This builds and then invokes the `hf2 elf <path>` runner configured in
`.cargo/config.toml`, which flashes over HF2 and resets the board into the
new firmware. The `PORTALBOOT` drive disappears and a new
`/dev/cu.usbmodem*` (macOS) / `/dev/ttyACM*` (Linux) serial port appears
once the firmware boots and enumerates its CDC-ACM interface.

If flashing fails with a "device not found" style error, the board likely
reset out of bootloader mode (e.g. from a previous run) — double-tap reset
again and retry.

## Talking to it

The firmware enumerates as a USB CDC serial device with VID:PID
`0x1209:0x0001` (a [pid.codes](https://pid.codes) test allocation — fine for
personal projects, not for redistribution) and accepts one JSON object per
line. For messages it just renders whatever the last command said; there's
no on-device message queue, so the caller owns any notion of how many
messages are "waiting".

Two commands:

```json
{"text":{"msg":"Hello, world!"}}
```

Shows a single message full-screen.

```json
{"badge":{"count":3,"messages":["Alice: hi","Bob: meeting at 3"]}}
```

Shows a bouncing badge with `count` on it, plus a preview list below it —
as many `messages` lines as fit on screen (currently 7); any beyond that are
silently ignored. The wire protocol itself caps at `MAX_PREVIEW_MESSAGES` in
`src/main.rs` (currently 16) — sending more than that makes the whole
command fail to parse and get silently dropped. A `count` of `0` shows a
static "nothing to do" icon instead of the animation, ignoring `messages`.

Both commands also take an optional `updated_at` string, drawn small and
gray in the top-right corner (e.g. `"14:32:05"`). This is opaque display
text supplied by the caller, not something the firmware computes itself;
omitting it (or sending `""`) draws nothing.

### Setting the clock

The board has no battery-backed clock, so the host tells it the time. Both
commands above also take optional `now` (Unix time in seconds, UTC) and
`utc_offset` (local offset from UTC in seconds, e.g. `3600` for UTC+1, used
only to display local time) fields. To set the clock without changing the
screen:

```json
{"time":{"now":1760000000,"utc_offset":3600}}
```

The device counts seconds since boot itself, using its RTC, and adds them to
the last time it was sent. It forgets the time on every power cycle. The
RTC runs from an internal oscillator and will drift, so the CLI sends `now`
with every command. The temperature is read once a minute either way.

Both screens show the device's local time (HH:MM:SS) small and gray in the
bottom-right corner, or `--:--:--` until the clock has been set.

### Temperature screen

The on-board ADT7410 sensor is read once a minute, with a fixed calibration
offset (`TEMP_OFFSET_CENTI` in `src/main.rs`, currently −8.2°C) for the
heat of the board and backlight around it. The messages screen shows
the latest reading small in the top-left corner. A second screen shows it
large, with a graph of the last 24h below it.

The display switches between the two screens by itself:

- After 10 minutes with no `text`/`badge` command, it shows the temperature
  screen.
- On the temperature screen, a new message switches back to the messages
  screen, but only once the temperature screen has been up for at least
  5 minutes. Before that, the message is stored and shown on the next switch
  back.

Tapping the screen anywhere switches between the two (and restarts the
10-minute timer when switching back to messages). This command does the same,
for testing without touching it:

```json
{"tap":{}}
```

Use `../cli` rather than crafting this by hand — it auto-detects the port by
VID:PID and handles the framing:

```sh
ppd-cli text "Hello, world!"
ppd-cli badge 3 --message "Alice: hi" --message "Bob: meeting at 3"
ppd-cli badge 0
ppd-cli sync-time
ppd-cli tap
ppd-cli readings --since 24h
```

For manual testing you can also just write a line to the serial port, e.g.:

```sh
echo '{"text":{"msg":"hi"}}' > /dev/cu.usbmodem2101
```

### Stored readings

Once the clock has been set, each reading is also written to the board's
8MB QSPI flash chip (the one CircuitPython used for its `CIRCUITPY` drive),
as a ring of 8-byte records covering about 370 days, so older readings are
overwritten. Readings taken before the clock is set after power-up aren't
stored, since there's no time to store with them. The 24h graph is refilled
from flash at the first time sync after boot, so it survives a restart.

The first boot with this firmware, or after a change to the storage format,
wipes the flash and shows "preparing storage (one-time)..." for several
seconds. Anything else on the flash, such as an old CircuitPython
filesystem, is lost. See `src/templog.rs` for the layout.

To pull stored readings:

```json
{"readings":{"since":"2026-10-14T00:00:00Z"}}
```

`since` is a UTC date-time in ISO 8601 form: `YYYY-MM-DD`, optionally
followed by `THH:MM` or `THH:MM:SS`, optionally followed by `Z`. It's
always taken as UTC; other offsets aren't accepted. Like the other
commands, this also takes optional `now`/`utc_offset` (which stay Unix
seconds). This is the one command the device replies to: one line per
reading at or after `since`, oldest first, then a count:

```json
{"t":"2026-10-15T10:12:18Z","c":21.43}
{"t":"2026-10-15T10:13:17Z","c":21.44}
{"done":{"count":2}}
```

`t` is the reading's time in UTC and `c` the temperature in °C. A malformed
`since` gets `{"error":"<message>"}` instead. Readings over a year old are
never returned, even while still in flash. The display freezes while the
reply is sent (a day's readings take well under a second; a full year,
~20MB, roughly a minute), and a command sent meanwhile may be lost. If the
host stops reading for 2 seconds, the device gives up and carries on.

The CLI prints the same lines with `t` converted to local time
(`2026-10-15T11:12:18+01:00`), or exactly as sent with `--utc`. Its
`--since` takes a duration back from now (`30m`, `24h`, `7d`, `2w`), `today`
or `yesterday` (from midnight), plain English (`"yesterday 6pm"`, `"last
monday"`, `"3 days ago"`; bare weekdays mean the most recent one, and dates
like `01/10/2026` are read day-first), or a local date/time (`2026-10-01`,
`2026-10-01T12:00`, or RFC 3339 with an offset). The count goes to stderr,
so stdout is just the readings:

```sh
ppd-cli readings --since yesterday > since-yesterday.jsonl
```

## Troubleshooting

- **Nothing shows up on USB at all, even bootloader mode**: check for a
  physical connector fault — the PyPortal's micro-USB port is soldered
  directly to the PCB with no strain relief and is a known weak point.
- **Board enumerates but the CLI can't find it**: run `ppd-cli list` from
  the `cli` crate to see what the OS actually sees; on macOS both a
  `/dev/cu.*` and `/dev/tty.*` node show up for the same device, which is
  expected.
- **Garbled or ignored messages**: the firmware's line buffer is 512 bytes
  (`LINE_CAPACITY` in `src/main.rs`); longer lines are silently dropped.
