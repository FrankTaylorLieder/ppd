use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serialport::SerialPortType;

/// USB VID:PID the firmware identifies itself with (pid.codes test allocation).
const VID: u16 = 0x1209;
const PID: u16 = 0x0001;

/// Must match `MAX_PREVIEW_MESSAGES` in the firmware; the device silently
/// drops the whole command if more are sent. This is a generous
/// wire-protocol cap, not the number of lines actually shown — the firmware
/// only draws as many as fit on screen and ignores the rest.
const MAX_PREVIEW_MESSAGES: usize = 16;

#[derive(Parser)]
#[command(name = "ppd-cli", about = "Send commands to the PyPortal MME display")]
struct Cli {
    /// Serial port to use (auto-detected if omitted)
    #[arg(long, global = true)]
    port: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show a single message full-screen
    Text { message: String },
    /// Show the "N waiting" badge animation with an optional preview list;
    /// count 0 shows a static "nothing to do" icon instead
    Badge {
        count: u32,
        /// Preview line to show below the badge (repeatable, max 16; only as
        /// many as fit on screen are actually shown)
        #[arg(long = "message", value_name = "MESSAGE")]
        messages: Vec<String>,
    },
    /// Set the device's clock to this machine's time without changing
    /// what's on screen (every other command also does this)
    SyncTime,
    /// Switch between the messages and temperature screens, as a tap on the
    /// screen would
    Tap,
    /// List candidate serial ports
    List,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Message<'a> {
    Text {
        msg: &'a str,
        updated_at: &'a str,
        #[serde(flatten)]
        time: TimeSync,
    },
    Badge {
        count: u32,
        messages: &'a [&'a str],
        updated_at: &'a str,
        #[serde(flatten)]
        time: TimeSync,
    },
    Time(TimeSync),
    Tap {},
}

/// Keeps the device's clock in step with this machine's: Unix time (UTC),
/// plus the local offset from UTC so it can display local time.
#[derive(serde::Serialize)]
struct TimeSync {
    now: i64,
    utc_offset: i32,
}

fn find_port() -> Result<String> {
    let ports = serialport::available_ports().context("listing serial ports")?;
    let matches: Vec<_> = ports
        .into_iter()
        .filter(|p| {
            matches!(&p.port_type, SerialPortType::UsbPort(info) if info.vid == VID && info.pid == PID)
        })
        .collect();

    // macOS exposes both a /dev/cu.* and /dev/tty.* node for the same USB
    // device; prefer the "cu" (call-up, no-carrier-detect) one when both match.
    if matches.len() > 1 {
        let cu_only: Vec<_> = matches
            .iter()
            .filter(|p| p.port_name.contains("/cu."))
            .cloned()
            .collect();
        if cu_only.len() == 1 {
            return Ok(cu_only[0].port_name.clone());
        }
    }

    match matches.len() {
        0 => bail!("no MME display found on any serial port; pass --port explicitly"),
        1 => Ok(matches[0].port_name.clone()),
        _ => {
            let names: Vec<_> = matches.iter().map(|p| p.port_name.as_str()).collect();
            bail!("multiple MME displays found ({names:?}); pass --port to pick one")
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if matches!(cli.command, Command::List) {
        for p in serialport::available_ports().context("listing serial ports")? {
            println!("{} ({:?})", p.port_name, p.port_type);
        }
        return Ok(());
    }

    let port_name = match cli.port {
        Some(p) => p,
        None => find_port()?,
    };

    let mut port = serialport::new(&port_name, 115_200)
        .timeout(Duration::from_secs(2))
        .open()
        .with_context(|| format!("opening {port_name}"))?;

    let local_now = chrono::Local::now();
    let updated_at = local_now.format("%H:%M:%S").to_string();
    let time = TimeSync {
        now: local_now.timestamp(),
        utc_offset: local_now.offset().local_minus_utc(),
    };

    match cli.command {
        Command::Text { message } => send(
            &mut *port,
            &Message::Text {
                msg: &message,
                updated_at: &updated_at,
                time,
            },
        )?,
        Command::Badge { count, messages } => {
            if messages.len() > MAX_PREVIEW_MESSAGES {
                bail!(
                    "at most {MAX_PREVIEW_MESSAGES} preview messages are supported, got {}",
                    messages.len()
                );
            }
            let refs: Vec<&str> = messages.iter().map(String::as_str).collect();
            send(
                &mut *port,
                &Message::Badge {
                    count,
                    messages: &refs,
                    updated_at: &updated_at,
                    time,
                },
            )?
        }
        Command::SyncTime => send(&mut *port, &Message::Time(time))?,
        Command::Tap => send(&mut *port, &Message::Tap {})?,
        Command::List => unreachable!(),
    }

    Ok(())
}

fn send(port: &mut dyn serialport::SerialPort, payload: &Message) -> Result<()> {
    let mut line = serde_json::to_string(payload)?;
    line.push('\n');
    port.write_all(line.as_bytes())
        .context("writing to serial port")
}
