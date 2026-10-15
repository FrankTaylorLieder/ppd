use std::io::{BufRead, BufReader};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Days, Local, NaiveDate, NaiveDateTime, NaiveTime, SecondsFormat, TimeZone};
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

/// How long to wait for each line of a `readings` reply. Finding where to
/// start in a full year of readings takes the device a moment.
const READINGS_LINE_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Print the stored temperature readings since a given time, one JSON
    /// object per line, oldest first: {"t":"<local ISO 8601 date-time>",
    /// "c":<°C>}
    Readings {
        /// How far back to go: a duration back from now (30m, 24h, 7d, 2w),
        /// today or yesterday (from midnight), plain English ("yesterday
        /// 6pm", "last monday", "3 days ago"), or a local date/time
        /// (2026-10-01, 2026-10-01T12:00, or RFC 3339 with an offset).
        /// Readings over a year old are never returned.
        #[arg(long)]
        since: String,
        /// Print times in UTC (2026-10-15T10:12:18Z), exactly as the device
        /// sends them, rather than local time with its offset
        #[arg(long)]
        utc: bool,
    },
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
    Readings {
        /// UTC, e.g. 2026-10-14T00:00:00Z.
        since: String,
        #[serde(flatten)]
        time: TimeSync,
    },
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
        Command::Readings { since, utc } => {
            let since = DateTime::from_timestamp(parse_since(&since, local_now)?, 0)
                .context("--since is out of range")?
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            // Drop anything stale so the first line read is the reply.
            port.clear(serialport::ClearBuffer::Input)
                .context("clearing serial input")?;
            port.set_timeout(READINGS_LINE_TIMEOUT)?;
            send(&mut *port, &Message::Readings { since, time })?;
            receive_readings(port, utc)?;
        }
        Command::List => unreachable!(),
    }

    Ok(())
}

/// Parses `--since`, trying in turn: a duration back from `now` (a number
/// followed by s, m, h, d or w); "today" or "yesterday" (from midnight);
/// RFC 3339 or a local date/date-time in ISO order; then free-form English
/// such as "yesterday 6pm", "last monday" or "3 days ago".
fn parse_since(text: &str, now: DateTime<Local>) -> Result<i64> {
    let text = text.trim();
    if let Some(unit) = text.chars().last().filter(char::is_ascii_alphabetic) {
        if let Ok(amount) = text[..text.len() - 1].parse::<i64>() {
            let secs = match unit {
                's' => 1,
                'm' => 60,
                'h' => 60 * 60,
                'd' => 24 * 60 * 60,
                'w' => 7 * 24 * 60 * 60,
                _ => bail!("unknown duration unit {unit:?} in {text:?}; use s, m, h, d or w"),
            };
            return Ok(now.timestamp() - amount * secs);
        }
    }

    // `interim` reads these as now and 24h ago; from the start of the day
    // is what's wanted for "since".
    let days_back = match text.to_ascii_lowercase().as_str() {
        "today" => Some(0),
        "yesterday" => Some(1),
        _ => None,
    };
    if let Some(days) = days_back {
        let date = now.date_naive() - Days::new(days);
        return local_timestamp(date.and_time(NaiveTime::MIN), text);
    }

    if let Ok(time) = DateTime::parse_from_rfc3339(text) {
        return Ok(time.timestamp());
    }
    let naive = [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|format| NaiveDateTime::parse_from_str(text, format).ok())
    .or_else(|| {
        NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .ok()
            .map(|date| date.and_time(NaiveTime::MIN))
    });
    if let Some(naive) = naive {
        return local_timestamp(naive, text);
    }

    let english = |text: &str| interim::parse_date_string(text, now, interim::Dialect::Uk).ok();
    let Some(time) = english(text) else {
        bail!(
            "can't parse --since {text:?}; try e.g. 24h, 7d, yesterday, \
             \"yesterday 6pm\", \"last monday\", \"3 days ago\" or 2026-10-01T12:00"
        );
    };
    if time <= now {
        return Ok(time.timestamp());
    }
    // A bare weekday ("monday") means the next one; for "since", the most
    // recent one is what's wanted.
    match english(&format!("last {text}")) {
        Some(time) if time <= now => Ok(time.timestamp()),
        _ => bail!("--since {text:?} is in the future"),
    }
}

fn local_timestamp(naive: NaiveDateTime, text: &str) -> Result<i64> {
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|time| time.timestamp())
        .ok_or_else(|| anyhow!("{text:?} doesn't exist in the local time zone"))
}

/// Prints reading lines from the device until its closing
/// `{"done":{"count":N}}`, then reports the count on stderr. Converts each
/// reading's UTC time to local unless `utc`.
fn receive_readings(port: Box<dyn serialport::SerialPort>, utc: bool) -> Result<()> {
    let mut reader = BufReader::new(port);
    let mut line = String::new();
    loop {
        line.clear();
        reader
            .read_line(&mut line)
            .context("reading from the device (it stopped responding?)")?;
        let line = line.trim();
        let mut value: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("unexpected reply from device: {line:?}"))?;
        if let Some(count) = value.get("done").and_then(|done| done.get("count")) {
            eprintln!("{count} readings");
            return Ok(());
        }
        if let Some(error) = value.get("error").and_then(|error| error.as_str()) {
            bail!("device: {error}");
        }
        if utc {
            println!("{line}");
            continue;
        }
        let local = value["t"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .with_context(|| format!("unexpected reading from device: {line:?}"))?
            .with_timezone(&Local);
        value["t"] = local.to_rfc3339_opts(SecondsFormat::Secs, false).into();
        println!("{value}");
    }
}

fn send(port: &mut dyn serialport::SerialPort, payload: &Message) -> Result<()> {
    let mut line = serde_json::to_string(payload)?;
    line.push('\n');
    port.write_all(line.as_bytes())
        .context("writing to serial port")
}
