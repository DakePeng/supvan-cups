//! Bluetooth SPP over the serial port the OS creates for a paired printer.
//!
//! On macOS, pairing a Classic-Bluetooth device that offers the Serial Port
//! Profile makes the system create `/dev/cu.<Name>` (and a matching `tty.`);
//! opening the port dials the RFCOMM channel and closing it hangs up. Linux
//! offers the same through `rfcomm bind` (`/dev/rfcommN`). Either way the
//! bytes are the raw SPP stream behind a file descriptor; the link mechanics
//! live in [`crate::byte_link`].
//!
//! Caveat: on macOS 26 the Bluetooth `cu.` port has been seen to open without
//! bringing the link up (writes vanish, nothing is ever read), so macOS
//! defaults to IOBluetooth ([`crate::rfcomm_macos`]) and uses this only for
//! an explicit `/dev/...` target.

use crate::error::{Error, Result};
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

fn os_err<T>() -> Result<T> {
    Err(Error::Io(std::io::Error::last_os_error()))
}

/// A raw-mode serial port carrying the SPP stream.
pub(crate) struct SerialPort {
    fd: OwnedFd,
}

impl SerialPort {
    /// Open and configure `path` (e.g. `/dev/cu.T0046A2410211517`). On a
    /// Bluetooth port this blocks while the OS connects to the printer, and
    /// fails if the printer is off or out of range.
    pub(crate) fn open(path: &str) -> Result<Self> {
        let cpath = CString::new(path)
            .map_err(|_| Error::InvalidParam(format!("serial path has a NUL byte: {path:?}")))?;
        // O_NONBLOCK so open() doesn't wait for carrier; cleared again below
        // so writes block normally — reads go through poll().
        let raw = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK,
            )
        };
        if raw < 0 {
            return os_err();
        }
        // SAFETY: `raw` is a freshly opened descriptor nothing else owns.
        let port = Self {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        };

        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(raw, &mut tio) } < 0 {
            return os_err();
        }
        unsafe { libc::cfmakeraw(&mut tio) };
        tio.c_cflag |= libc::CLOCAL | libc::CREAD;
        tio.c_cc[libc::VMIN] = 0;
        tio.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(raw, libc::TCSANOW, &tio) } < 0 {
            return os_err();
        }

        let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
            return os_err();
        }
        unsafe { libc::tcflush(raw, libc::TCIOFLUSH) };
        Ok(port)
    }

    fn raw(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    /// Wait up to `timeout` for input, then one `read`; `None` on timeout,
    /// error or EOF.
    fn recv_once(&self, buf: &mut [u8], timeout: Duration) -> Option<usize> {
        let mut pfd = libc::pollfd {
            fd: self.raw(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
        let ready = unsafe { libc::poll(&mut pfd, 1, ms) };
        if ready <= 0 || pfd.revents & libc::POLLIN == 0 {
            return None;
        }
        let n = unsafe { libc::read(self.raw(), buf.as_mut_ptr().cast(), buf.len()) };
        usize::try_from(n).ok().filter(|&n| n > 0)
    }
}

impl crate::byte_link::ByteLink for SerialPort {
    fn recv(&self, buf: &mut [u8], timeout: Duration) -> Option<usize> {
        self.recv_once(buf, timeout)
    }

    fn send_all(&self, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            let n = unsafe { libc::write(self.raw(), data.as_ptr().cast(), data.len()) };
            let Ok(n) = usize::try_from(n) else {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(Error::Io(err));
            };
            data = &data[n..];
        }
        Ok(())
    }
}

/// Whether `target` names a serial device rather than a Bluetooth address or
/// HID node: `/dev/cu.*` and `/dev/tty.*` (macOS), `/dev/rfcommN` and
/// `/dev/ttyUSB*`-style nodes (Linux).
pub fn is_serial_path(target: &str) -> bool {
    target.starts_with("/dev/") && !target.starts_with("/dev/hidraw")
}
