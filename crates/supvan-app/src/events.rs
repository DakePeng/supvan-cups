//! Machine-readable events and control commands for a supervising front end.
//!
//! With `SUPVAN_EVENTS=1` the app writes one JSON object per line to stdout
//! (logs stay on stderr) and reads one command per line from stdin:
//! `connect`, `disconnect`, `quit`. EOF on stdin means the supervisor is gone
//! and the app exits, so a crashed front end never leaves a daemon holding the
//! printer. The macOS menu-bar app (`macos/`) is the one consumer; without the
//! variable none of this is active and stdin is never touched.

use std::io::Write;
use std::sync::OnceLock;

use ipp_printer_app::PrinterReason;
use serde::Serialize;

/// Whether the event stream / command channel is on.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("SUPVAN_EVENTS").is_ok_and(|v| v == "1"))
}

#[derive(Serialize)]
pub struct PrinterInfo {
    pub name: String,
    pub info: String,
    /// The IPP URI a local CUPS queue should point at.
    pub uri: String,
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event<'a> {
    /// The printers this app serves. Sent once at startup.
    Printers {
        printers: Vec<PrinterInfo>,
    },
    /// A Bluetooth link came up or went down.
    Link {
        address: &'a str,
        connected: bool,
    },
    /// The result of a status read (or of finding the printer unreachable).
    Status {
        printer: &'a str,
        connected: bool,
        reasons: Vec<&'static str>,
        /// Loaded label, millimetres.
        media_mm: Option<[u8; 2]>,
        /// Gap between labels, millimetres, as the roll's record states it.
        gap_mm: Option<u8>,
        /// The roll carries a genuine tag and describes itself; its record
        /// cannot be changed from the host.
        roll_locked: Option<bool>,
        remaining: Option<u32>,
    },
    /// The outcome of a `print_pgm` command.
    TestPrint {
        printer: &'a str,
        ok: bool,
        message: &'a str,
    },
    /// The outcome of a `set_label` command.
    LabelSet {
        printer: &'a str,
        ok: bool,
        message: &'a str,
    },
    /// An explicit `connect` could not reach the printer.
    ConnectFailed {
        printer: &'a str,
    },
    JobStarted {
        printer: &'a str,
        job: u32,
    },
    JobDone {
        printer: &'a str,
        job: u32,
    },
    /// The job hit a clearable condition and will be retried.
    JobHeld {
        printer: &'a str,
        job: u32,
        reasons: Vec<&'static str>,
        message: &'a str,
    },
    /// The job was aborted.
    JobFailed {
        printer: &'a str,
        job: u32,
        reasons: Vec<&'static str>,
        message: &'a str,
    },
}

/// Write `event` as one JSON line, if the stream is on.
pub fn emit(event: &Event<'_>) {
    if !enabled() {
        return;
    }
    if let Ok(line) = serde_json::to_string(event) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

/// IPP keyword names for the reasons a front end may want to explain.
pub fn reason_names(r: PrinterReason) -> Vec<&'static str> {
    [
        (PrinterReason::OFFLINE, "offline"),
        (PrinterReason::COVER_OPEN, "cover-open"),
        (PrinterReason::MEDIA_JAM, "media-jam"),
        (PrinterReason::MEDIA_EMPTY, "media-empty"),
        (PrinterReason::MEDIA_NEEDED, "media-needed"),
        (PrinterReason::MEDIA_LOW, "media-low"),
        (PrinterReason::OTHER, "other"),
    ]
    .into_iter()
    .filter_map(|(flag, name)| r.contains(flag).then_some(name))
    .collect()
}

/// A command from the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Connect,
    Disconnect,
    Quit,
    /// `set_label <width> <length> <gap>`, millimetres: write the label
    /// geometry to the printer (untagged stock only).
    SetLabel {
        width_mm: u8,
        length_mm: u8,
        gap_mm: u8,
    },
    /// `print_pgm <density 0-15> <path>`: print a binary PGM the size of the
    /// label, in dots, as line art at exactly that density — bypassing the
    /// print queue and the darkness setting. For calibration pages.
    PrintPgm {
        density: u8,
        path: String,
    },
    /// `set_darkness <0-100>`: the baseline print darkness.
    SetDarkness(u8),
    /// `set_dither <name>`: how grey is turned into dots.
    SetDither(supvan_proto::dither::DitherMode),
}

fn parse_command(line: &str) -> Option<Command> {
    let mut words = line.split_whitespace();
    match words.next()? {
        "connect" => Some(Command::Connect),
        "disconnect" => Some(Command::Disconnect),
        "quit" => Some(Command::Quit),
        "set_label" => {
            let mut mm = || words.next()?.parse::<u8>().ok();
            let (width_mm, length_mm, gap_mm) = (mm()?, mm()?, mm()?);
            Some(Command::SetLabel {
                width_mm,
                length_mm,
                gap_mm,
            })
        }
        "print_pgm" => {
            let density = words.next()?.parse::<u8>().ok().filter(|d| *d <= 15)?;
            // The path is the rest of the line: it may contain spaces.
            let rest = line.trim().strip_prefix("print_pgm")?.trim_start();
            let path = rest.split_once(char::is_whitespace)?.1.trim();
            (!path.is_empty()).then(|| Command::PrintPgm {
                density,
                path: path.to_string(),
            })
        }
        "set_darkness" => words
            .next()?
            .parse::<u8>()
            .ok()
            .filter(|d| *d <= 100)
            .map(Command::SetDarkness),
        "set_dither" => words.next()?.parse().ok().map(Command::SetDither),
        _ => None,
    }
}

/// Read commands from stdin on a dedicated thread. EOF is delivered as
/// [`Command::Quit`]. `None` when the channel is off.
pub fn commands() -> Option<tokio::sync::mpsc::UnboundedReceiver<Command>> {
    if !enabled() {
        return None;
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("supvan-stdin".into())
        .spawn(move || {
            for line in std::io::stdin().lines() {
                let Ok(line) = line else { break };
                match parse_command(&line) {
                    Some(cmd) => {
                        if tx.send(cmd).is_err() {
                            return;
                        }
                    }
                    None if line.trim().is_empty() => {}
                    None => log::warn!("events: unknown command {line:?}"),
                }
            }
            let _ = tx.send(Command::Quit);
        })
        .ok()?;
    Some(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(parse_command("connect\n"), Some(Command::Connect));
        assert_eq!(parse_command("  disconnect "), Some(Command::Disconnect));
        assert_eq!(parse_command("quit"), Some(Command::Quit));
        assert_eq!(parse_command("reboot"), None);
        assert_eq!(
            parse_command("set_label 40 30 3"),
            Some(Command::SetLabel {
                width_mm: 40,
                length_mm: 30,
                gap_mm: 3
            })
        );
        assert_eq!(parse_command("set_label 40 30"), None);
        assert_eq!(parse_command("set_label 400 30 3"), None);
        assert_eq!(
            parse_command("set_darkness 35"),
            Some(Command::SetDarkness(35))
        );
        assert_eq!(parse_command("set_darkness 101"), None);
        assert_eq!(
            parse_command("set_dither threshold"),
            Some(Command::SetDither(
                supvan_proto::dither::DitherMode::Threshold
            ))
        );
        assert_eq!(parse_command("set_dither sepia"), None);
        assert_eq!(
            parse_command("print_pgm 5 /tmp/a b/page.pgm\n"),
            Some(Command::PrintPgm {
                density: 5,
                path: "/tmp/a b/page.pgm".into()
            })
        );
        assert_eq!(parse_command("print_pgm 16 /tmp/page.pgm"), None);
        assert_eq!(parse_command("print_pgm 5"), None);
    }

    #[test]
    fn events_are_tagged_json_lines() {
        let e = Event::Status {
            printer: "p",
            connected: true,
            reasons: reason_names(PrinterReason::MEDIA_JAM | PrinterReason::COVER_OPEN),
            media_mm: Some([50, 70]),
            gap_mm: Some(3),
            roll_locked: Some(true),
            remaining: Some(960),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.starts_with(r#"{"event":"status","printer":"p","connected":true"#));
        assert!(s.contains(r#""reasons":["cover-open","media-jam"]"#));
        assert!(s.contains(r#""media_mm":[50,70]"#));
        assert!(!s.contains('\n'));
    }
}
