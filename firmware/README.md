# mme-display-firmware

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
line:

```json
{"text":{"msg":"Hello, world!"}}
```

Use `../cli` (`mme-cli text "Hello, world!"`) rather than crafting this by
hand — it auto-detects the port by VID:PID and handles the framing. For
manual testing you can also just write a line to the serial port, e.g.:

```sh
echo '{"text":{"msg":"hi"}}' > /dev/cu.usbmodem2101
```

## Troubleshooting

- **Nothing shows up on USB at all, even bootloader mode**: check for a
  physical connector fault — the PyPortal's micro-USB port is soldered
  directly to the PCB with no strain relief and is a known weak point.
- **Board enumerates but the CLI can't find it**: run `mme-cli list` from
  the `cli` crate to see what the OS actually sees; on macOS both a
  `/dev/cu.*` and `/dev/tty.*` node show up for the same device, which is
  expected.
- **Garbled or ignored messages**: the firmware's line buffer is 256 bytes
  (`LINE_CAPACITY` in `src/main.rs`); longer lines are silently dropped.
