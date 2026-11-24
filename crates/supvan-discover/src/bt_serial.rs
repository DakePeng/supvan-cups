//! Classic-Bluetooth discovery without a Bluetooth stack API: the serial
//! ports the OS creates for paired SPP devices.
//!
//! Pairing an SPP printer makes some systems publish it as a serial port
//! (macOS: `/dev/cu.<Name>`, the device name with spaces removed, sometimes
//! followed by `-<ServiceName>`). A "Bluetooth candidate" here is a port
//! whose name the model registry recognises, and its address *is* the port
//! path, which [`supvan_proto::printer::Printer::open_bt`] opens as a serial
//! device.
//!
//! This is the fallback for platforms with neither BlueZ nor IOBluetooth; on
//! macOS only [`env_port_candidates`] is used (the Bluetooth port there has
//! proven unreliable, see `supvan_proto::serial`). Ports can always be added
//! with `SUPVAN_SERIAL_PORTS=/dev/cu.A,/dev/cu.B`.

use crate::models;

/// Where the OS publishes serial devices.
const DEV_DIR: &str = "/dev";
/// Call-out device prefix (macOS). The `tty.` twin blocks on carrier detect.
const CALLOUT_PREFIX: &str = "cu.";
/// Extra ports to treat as printers, comma-separated.
const PORTS_ENV: &str = "SUPVAN_SERIAL_PORTS";

/// Ports the OS itself owns, never a printer.
const SYSTEM_PORTS: &[&str] = &["Bluetooth-Incoming-Port", "debug-console", "wlan-debug"];

/// Service-name suffixes macOS appends to a port when the SDP record names
/// its channel. Stripped so the name joins with the serial USB reports.
const SERVICE_SUFFIXES: &[&str] = &["-spp", "-serialport", "-serial-port", "-port", "-devb"];

/// One BT-attached Supvan candidate.
///
/// `address` is the serial device path to open; `name` is the printer's
/// advertised name as far as the port name preserves it, so it cross-correlates
/// with the serial USB reports.
pub struct BtCandidate {
    pub address: String,
    pub name: String,
}

/// The printer name a port was made from: the `cu.` stem minus any service
/// suffix the OS appended.
fn name_from_stem(stem: &str) -> &str {
    let lower = stem.to_ascii_lowercase();
    for suffix in SERVICE_SUFFIXES {
        if lower.ends_with(suffix) && lower.len() > suffix.len() {
            return &stem[..stem.len() - suffix.len()];
        }
    }
    stem
}

/// Whether a port name looks like a Supvan printer. Known prefixes from the
/// registry, or an unlisted firmware serial name (`T0117A2410211517`) long
/// enough not to be a coincidence — without an address there is no OUI to
/// lean on, so the loose form needs the full serial.
fn looks_like_printer(name: &str) -> bool {
    models::is_matching_bt_name(name)
        || (models::is_supvan_serial_name(name)
            && name.len() >= 12
            && name.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn candidate_from_path(path: &str) -> Option<BtCandidate> {
    let file = path.rsplit('/').next()?;
    let stem = file.strip_prefix(CALLOUT_PREFIX).unwrap_or(file);
    let name = name_from_stem(stem);
    (!name.is_empty()).then(|| BtCandidate {
        address: path.to_string(),
        name: name.to_string(),
    })
}

/// The ports named in `SUPVAN_SERIAL_PORTS`, as candidates.
pub(crate) fn env_port_candidates() -> Vec<BtCandidate> {
    let mut out: Vec<BtCandidate> = Vec::new();
    if let Ok(list) = std::env::var(PORTS_ENV) {
        for port in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match candidate_from_path(port) {
                Some(c) if std::path::Path::new(port).exists() => {
                    log::info!("bt discover: {PORTS_ENV} port {port} ({})", c.name);
                    out.push(c);
                }
                Some(_) => log::warn!("bt discover: {PORTS_ENV} port {port} does not exist"),
                None => log::warn!("bt discover: {PORTS_ENV} port {port} has no usable name"),
            }
        }
    }
    out
}

/// Paired Supvan printers, as the serial ports the OS created for them.
/// Cheap and non-blocking — a directory listing — but kept blocking-shaped
/// to match the BlueZ scanner it replaces.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn list_candidates() -> Vec<BtCandidate> {
    let mut out = env_port_candidates();

    let Ok(entries) = std::fs::read_dir(DEV_DIR) else {
        return out;
    };
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = file.strip_prefix(CALLOUT_PREFIX) else {
            continue;
        };
        if SYSTEM_PORTS.contains(&stem) {
            continue;
        }
        let path = format!("{DEV_DIR}/{file}");
        if out.iter().any(|c| c.address == path) {
            continue;
        }
        let name = name_from_stem(stem);
        if !looks_like_printer(name) {
            log::debug!("bt discover: {path} — not a Supvan name, skipping");
            continue;
        }
        log::info!("bt discover: reporting {name} ({path})");
        out.push(BtCandidate {
            address: path,
            name: name.to_string(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_service_suffix_case_insensitively() {
        assert_eq!(name_from_stem("T0046A2410211517-SPP"), "T0046A2410211517");
        assert_eq!(name_from_stem("T0046A2410211517-DevB"), "T0046A2410211517");
        assert_eq!(name_from_stem("T0046A2410211517"), "T0046A2410211517");
        // A suffix alone is not a name.
        assert_eq!(name_from_stem("-SPP"), "-SPP");
    }

    #[test]
    fn loose_serial_names_need_the_full_serial() {
        models::load_embedded_for_tests();
        assert!(looks_like_printer("T0999Z2410211517"));
        assert!(!looks_like_printer("T09"));
        assert!(!looks_like_printer("Bluetooth-Incoming-Port"));
        assert!(!looks_like_printer("DJIMicMini-A3447C"));
    }

    #[test]
    fn candidate_keeps_path_as_address() {
        let c = candidate_from_path("/dev/cu.T50MPro-SPP").unwrap();
        assert_eq!(c.address, "/dev/cu.T50MPro-SPP");
        assert_eq!(c.name, "T50MPro");
    }
}
