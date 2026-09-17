use std::io::Write;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serialport::SerialPortType;

/// USB VID:PID the firmware identifies itself with (pid.codes test allocation).
const VID: u16 = 0x1209;
const PID: u16 = 0x0001;

#[derive(Parser)]
#[command(name = "mme-cli", about = "Send commands to the PyPortal MME display")]
struct Cli {
    /// Serial port to use (auto-detected if omitted)
    #[arg(long, global = true)]
    port: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Display a text message on the screen
    Text { message: String },
    /// List candidate serial ports
    List,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Message<'a> {
    Text { msg: &'a str },
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

    match cli.command {
        Command::Text { message } => {
            let payload = Message::Text { msg: &message };
            let mut line = serde_json::to_string(&payload)?;
            line.push('\n');
            port.write_all(line.as_bytes())
                .context("writing to serial port")?;
        }
        Command::List => unreachable!(),
    }

    Ok(())
}
