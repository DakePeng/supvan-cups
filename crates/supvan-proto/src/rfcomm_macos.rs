//! Classic-Bluetooth RFCOMM on macOS through IOBluetooth.
//!
//! Wraps the Objective-C shim in `macos/rfcomm_shim.m` (built by `build.rs`),
//! which owns the run-loop thread IOBluetooth needs and hands us a blocking
//! byte pipe. The frame handling is the shared [`crate::byte_link`] code, so
//! this link behaves exactly like the Linux RFCOMM socket.

use crate::byte_link::ByteLink;
use crate::error::{Error, Result};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr::NonNull;
use std::time::Duration;

/// The printers' SPP channel (SDP: "SPP slave", channel 1).
const RFCOMM_CHANNEL: u8 = 1;
/// How long an open may take: paging a sleeping printer is slow.
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);

unsafe extern "C" {
    fn sv_rfcomm_open(
        addr: *const c_char,
        channel: u8,
        timeout_ms: c_int,
        err: *mut c_char,
        errlen: usize,
    ) -> *mut c_void;
    fn sv_rfcomm_write(h: *mut c_void, buf: *const u8, len: usize) -> isize;
    fn sv_rfcomm_read(h: *mut c_void, buf: *mut u8, len: usize, timeout_ms: c_int) -> isize;
    fn sv_rfcomm_close(h: *mut c_void);
    fn sv_bt_paired_devices(
        cb: extern "C" fn(*const c_char, *const c_char, *mut c_void),
        ctx: *mut c_void,
    ) -> c_int;
    fn sv_run_main_loop();
    fn sv_stop_main_loop();
}

/// Run `body` on a worker thread while the calling (main) thread services the
/// macOS main run loop, which IOBluetooth needs drained; returns `body`'s exit
/// code once it finishes.
///
/// IOBluetooth's first use blocks until a CoreBluetooth callback arrives via
/// the main dispatch queue, so a binary whose main thread sits in a tokio
/// `block_on` would hang on its first Bluetooth call. Call this from `main`
/// before anything else.
pub fn run_with_main_loop<F>(body: F) -> i32
where
    F: FnOnce() -> i32 + Send + 'static,
{
    let worker = std::thread::Builder::new()
        .name("supvan-main".into())
        .spawn(move || {
            let code = body();
            unsafe { sv_stop_main_loop() };
            code
        })
        .expect("spawn main worker thread");
    unsafe { sv_run_main_loop() };
    worker.join().unwrap_or(1)
}

/// An open RFCOMM channel to a printer.
pub(crate) struct MacRfcomm {
    h: NonNull<c_void>,
}

// SAFETY: the shim serialises every IOBluetooth call onto its own thread and
// guards the receive buffer with a mutex; the handle is just a token.
unsafe impl Send for MacRfcomm {}
unsafe impl Sync for MacRfcomm {}

impl MacRfcomm {
    /// Dial `addr` (`AA:BB:CC:DD:EE:FF`) on the SPP channel.
    pub(crate) fn connect(addr: &str) -> Result<Self> {
        let caddr = CString::new(addr)
            .map_err(|_| Error::InvalidParam(format!("address has a NUL byte: {addr:?}")))?;
        let mut err = [0 as c_char; 256];
        let h = unsafe {
            sv_rfcomm_open(
                caddr.as_ptr(),
                RFCOMM_CHANNEL,
                OPEN_TIMEOUT.as_millis() as c_int,
                err.as_mut_ptr(),
                err.len(),
            )
        };
        match NonNull::new(h) {
            Some(h) => Ok(Self { h }),
            None => {
                let msg = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy();
                Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    format!("{addr}: {msg}"),
                )))
            }
        }
    }
}

impl Drop for MacRfcomm {
    fn drop(&mut self) {
        unsafe { sv_rfcomm_close(self.h.as_ptr()) };
    }
}

impl ByteLink for MacRfcomm {
    fn recv(&self, buf: &mut [u8], timeout: Duration) -> Option<usize> {
        let ms = c_int::try_from(timeout.as_millis()).unwrap_or(c_int::MAX);
        let n = unsafe { sv_rfcomm_read(self.h.as_ptr(), buf.as_mut_ptr(), buf.len(), ms) };
        usize::try_from(n).ok().filter(|&n| n > 0)
    }

    fn send_all(&self, data: &[u8]) -> Result<()> {
        let n = unsafe { sv_rfcomm_write(self.h.as_ptr(), data.as_ptr(), data.len()) };
        if n < 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "RFCOMM write failed (channel closed?)",
            )));
        }
        Ok(())
    }
}

/// A Classic-Bluetooth device the OS has paired.
#[derive(Debug, Clone)]
pub struct PairedDevice {
    /// `AA:BB:CC:DD:EE:FF`, upper-case.
    pub address: String,
    pub name: String,
}

extern "C" fn collect(addr: *const c_char, name: *const c_char, ctx: *mut c_void) {
    // SAFETY: the shim passes valid NUL-terminated strings and our Vec pointer.
    let out = unsafe { &mut *(ctx as *mut Vec<PairedDevice>) };
    let address = unsafe { CStr::from_ptr(addr) }
        .to_string_lossy()
        .into_owned();
    let name = unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned();
    out.push(PairedDevice { address, name });
}

/// Every Classic-Bluetooth device paired with this Mac.
pub fn paired_devices() -> Vec<PairedDevice> {
    let mut out: Vec<PairedDevice> = Vec::new();
    unsafe { sv_bt_paired_devices(collect, (&mut out) as *mut Vec<PairedDevice> as *mut c_void) };
    out
}
