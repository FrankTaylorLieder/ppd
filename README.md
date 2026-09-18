# ppd

A small "waiting messages" display built on the [Adafruit PyPortal M4
Express](https://www.adafruit.com/product/4116). Two crates:

- **[`firmware/`](firmware)** — bare-metal Rust firmware that runs on the
  PyPortal. It drives the display and exposes a USB CDC-ACM serial port that
  accepts newline-delimited JSON commands: show a full-screen text message,
  or show a bouncing "N waiting" badge with optional preview lines. It's
  stateless — it just renders whatever the last command said.
- **[`cli/`](cli)** — a host-side command-line tool (`ppd-cli`) that
  auto-detects the PyPortal over USB and sends it those commands, so you
  don't have to craft the JSON by hand.

```sh
ppd-cli text "Hello, world!"
ppd-cli badge 3 --message "Alice: hi" --message "Bob: meeting at 3"
```

See [`firmware/README.md`](firmware/README.md) for build/flash instructions,
the wire protocol, and troubleshooting. The `cli` crate builds normally with
`cargo build --release` from within `cli/`.
