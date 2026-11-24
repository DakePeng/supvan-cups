//! USB HID I/O through `hidapi` for platforms without hidraw (macOS, Windows).
//!
//! Same surface as `hidraw.rs` — 64-byte report write/read with a timeout
//! and a drain — so `usb_transport.rs` compiles unchanged. A device is named
//! by the opaque path hidapi enumerates it under (macOS: `DevSrvsID:<n>`).

use crate::error::{Error, Result};
use hidapi::{HidApi, HidDevice, HidError};
use std::ffi::CString;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// HID report size for the Supvan T50 Pro USB interface.
pub(crate) const HID_REPORT_SIZE: usize = 64;

fn hid_err(e: HidError) -> Error {
    Error::Io(std::io::Error::other(e.to_string()))
}

/// The process-wide hidapi context. `hidapi` wants exactly one; enumeration
/// refreshes it in place.
fn api() -> Result<MutexGuard<'static, Option<HidApi>>> {
    static API: OnceLock<Mutex<Option<HidApi>>> = OnceLock::new();
    let mut guard = API
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if guard.is_none() {
        *guard = Some(HidApi::new().map_err(hid_err)?);
    }
    Ok(guard)
}

/// One HID interface hidapi knows about.
#[derive(Debug, Clone)]
pub struct HidDeviceInfo {
    /// Opaque, stable-while-plugged path to hand to [`crate::printer::Printer::open_usb`].
    pub path: String,
    pub vid: u16,
    pub pid: u16,
    pub serial: Option<String>,
    pub product: Option<String>,
}

/// Every HID interface with vendor id `vid`.
pub fn enumerate(vid: u16) -> Result<Vec<HidDeviceInfo>> {
    let mut guard = api()?;
    let api = guard.as_mut().expect("initialised above");
    api.reset_devices().map_err(hid_err)?;
    api.add_devices(vid, 0).map_err(hid_err)?;
    Ok(api
        .device_list()
        .map(|d| HidDeviceInfo {
            path: d.path().to_string_lossy().into_owned(),
            vid: d.vendor_id(),
            pid: d.product_id(),
            serial: d
                .serial_number()
                .map(str::to_string)
                .filter(|s| !s.is_empty()),
            product: d.product_string().map(str::to_string),
        })
        .collect())
}

/// A HID device opened via hidapi.
pub(crate) struct HidrawDevice {
    // hidapi's handle is `Send` but not `Sync`; the transport is shared
    // across tasks, so serialise access here.
    dev: Mutex<HidDevice>,
}

impl HidrawDevice {
    /// Open a device by its hidapi path (from [`enumerate`]).
    pub(crate) fn open(path: &str) -> Result<Self> {
        let cpath = CString::new(path)
            .map_err(|_| Error::InvalidParam(format!("HID path has a NUL byte: {path:?}")))?;
        let dev = api()?
            .as_ref()
            .expect("initialised")
            .open_path(&cpath)
            .map_err(hid_err)?;
        dev.set_blocking_mode(true).map_err(hid_err)?;
        Ok(Self {
            dev: Mutex::new(dev),
        })
    }

    fn dev(&self) -> MutexGuard<'_, HidDevice> {
        self.dev.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Write a 64-byte output report, zero-padding short data. hidapi takes
    /// the report ID as byte 0 — `0x00` for this ID-less device — and sends
    /// the 64 bytes after it, matching what the hidraw path puts on the wire.
    pub(crate) fn write_report(&self, data: &[u8]) -> Result<()> {
        let mut report = [0u8; 1 + HID_REPORT_SIZE];
        let len = data.len().min(HID_REPORT_SIZE);
        report[1..=len].copy_from_slice(&data[..len]);
        self.dev().write(&report).map_err(hid_err)?;
        Ok(())
    }

    /// Read an input report (up to 64 bytes); `None` on timeout.
    pub(crate) fn read_report(&self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let mut buf = [0u8; HID_REPORT_SIZE];
        let n = self.dev().read_timeout(&mut buf, ms).map_err(hid_err)?;
        Ok((n > 0).then(|| buf[..n].to_vec()))
    }

    /// Discard input reports already queued (see `hidraw.rs`).
    pub(crate) fn drain(&self) -> Result<()> {
        while let Some(stale) = self.read_report(Duration::ZERO)? {
            log::debug!(
                "USB drained stale report: {:02x?}",
                &stale[..stale.len().min(8)]
            );
        }
        Ok(())
    }
}
