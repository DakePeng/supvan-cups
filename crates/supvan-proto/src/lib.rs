//! Supvan T-series wire protocol — the reusable, transport-agnostic core.
//!
//! Speaks the framed command/response protocol the printers use over both
//! Bluetooth SPP (`0x7E5A` framing, little-endian params) and USB HID
//! (`0xC040` framing, big-endian params), behind a single [`transport::Transport`]
//! trait. SPP rides a raw RFCOMM socket on Linux, IOBluetooth on macOS, or a
//! serial port the OS created for the paired printer ([`serial`]); USB HID is
//! hidraw on Linux and `hidapi` on macOS/Windows. On top of that it provides the print pipeline: 1-bit bitmap packing
//! into the printhead's column-major layout ([`bitmap`]), LZMA1-"alone"
//! compression of the print buffers ([`compress`]), status and loaded-material
//! decoding ([`status`]), and the high-level print flow ([`printer::Printer`]).
//!
//! This crate has no IPP/CUPS knowledge — `supvan-app` layers that on via the
//! `ipp-printer-app` framework. See `docs/PROTOCOL.md` for the wire format.

pub mod bitmap;
pub mod ble;
pub mod buffer;
mod byte_link;
pub mod cmd;
pub mod compress;
pub mod data;
pub mod dither;
pub mod error;
#[cfg(target_os = "linux")]
#[path = "hidraw.rs"]
mod hidraw;
#[cfg(not(target_os = "linux"))]
#[path = "hid_portable.rs"]
mod hidraw;
#[cfg(not(target_os = "linux"))]
pub use hidraw::{HidDeviceInfo, enumerate as enumerate_hid};
pub mod page;
pub mod printer;
#[cfg(target_os = "linux")]
mod rfcomm;
#[cfg(target_os = "macos")]
mod rfcomm_macos;
#[cfg(target_os = "macos")]
pub use rfcomm_macos::{PairedDevice, paired_devices as paired_bt_devices, run_with_main_loop};

/// Run `body` and return its exit code. On macOS this is
/// [`rfcomm_macos::run_with_main_loop`], which keeps the main thread's run
/// loop alive for IOBluetooth; elsewhere `body` runs inline. Binaries call
/// it from `main` and build their async runtime inside `body`.
#[cfg(not(target_os = "macos"))]
pub fn run_with_main_loop<F>(body: F) -> i32
where
    F: FnOnce() -> i32 + Send + 'static,
{
    body()
}
pub mod rfid;
pub mod serial;
mod speed;
mod spp_pipe;
pub mod status;
pub mod transport;
pub mod twocolor;
mod usb_transport;
