//! SPP link mechanics over any blocking byte pipe.
//!
//! The RFCOMM socket, a serial port and the macOS IOBluetooth channel all
//! carry the same raw SPP stream and need the same handling around it —
//! drain stale input before every write, pace writes, poll for the reply
//! until a `7E 5A` frame is complete. [`ByteLink`] is the few primitives a
//! pipe must provide; [`LinkPipe`] turns one into an [`SppPipe`].

use crate::cmd::{CmdFrame, frame_len};
use crate::data::DataFrame;
use crate::error::Result;
use crate::spp_pipe::{SPP_CHUNK, SPP_CHUNK_DELAY, SppPipe};
use std::time::{Duration, Instant};

/// Reply budget for a command.
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// How often a reply poll wakes to check for bytes (the vendor's 20 ms).
const REPLY_POLL: Duration = Duration::from_millis(20);
/// Grace window for stragglers of a reply that isn't a `7E 5A` frame.
const UNFRAMED_TRAILING_WAIT: Duration = Duration::from_millis(50);

/// A blocking, bidirectional byte stream to the printer.
pub(crate) trait ByteLink: Send + Sync {
    /// Wait up to `timeout` for input, then read once; `None` on timeout,
    /// error or EOF.
    fn recv(&self, buf: &mut [u8], timeout: Duration) -> Option<usize>;
    /// Write all of `data`.
    fn send_all(&self, data: &[u8]) -> Result<()>;
}

/// An [`SppPipe`] over a [`ByteLink`].
pub(crate) struct LinkPipe<L: ByteLink>(pub(crate) L);

impl<L: ByteLink> LinkPipe<L> {
    /// Discard anything already received — a stale reply would otherwise be
    /// read as the answer to the next command.
    fn drain(&self) {
        let mut buf = [0u8; 1024];
        while self.0.recv(&mut buf, Duration::ZERO).is_some() {}
    }

    /// Poll for a reply. Returns as soon as a `7E 5A` frame is complete — a
    /// fixed grace wait here cost ~50 ms per data-packet ack, enough to starve
    /// the printhead mid-label over Bluetooth.
    fn read_reply(&self) -> Result<Option<Vec<u8>>> {
        let deadline = Instant::now() + IO_TIMEOUT;
        let mut response = Vec::new();
        let mut buf = [0u8; 512];
        while Instant::now() < deadline {
            let Some(n) = self.0.recv(&mut buf, REPLY_POLL) else {
                if !response.is_empty() && frame_len(&response).is_none() {
                    break;
                }
                continue;
            };
            response.extend_from_slice(&buf[..n]);
            match frame_len(&response) {
                Some(len) if response.len() >= len => break,
                Some(_) => {}
                None if response.len() < 4 => {}
                None => {
                    if let Some(n) = self.0.recv(&mut buf, UNFRAMED_TRAILING_WAIT) {
                        response.extend_from_slice(&buf[..n]);
                    }
                    break;
                }
            }
        }
        Ok((!response.is_empty()).then_some(response))
    }

    fn send_cmd_blocking(&self, frame: &CmdFrame) -> Result<Option<Vec<u8>>> {
        log::debug!("TX: {frame:02x?}");
        self.drain();
        self.0.send_all(frame)?;
        std::thread::sleep(SPP_CHUNK_DELAY);
        let resp = self.read_reply()?;
        match &resp {
            Some(data) => log::debug!("RX: {:02x?}", &data[..data.len().min(40)]),
            None => log::debug!("RX: (no response)"),
        }
        Ok(resp)
    }

    /// Send a data frame as paced 128-byte chunks (`transferSplitData`).
    fn send_data_frame_blocking(
        &self,
        frame: &DataFrame,
        read_response: bool,
    ) -> Result<Option<Vec<u8>>> {
        for chunk in frame.chunks(SPP_CHUNK) {
            std::thread::sleep(SPP_CHUNK_DELAY);
            self.drain();
            self.0.send_all(chunk)?;
        }
        if read_response {
            self.read_reply()
        } else {
            Ok(None)
        }
    }
}

#[async_trait::async_trait]
impl<L: ByteLink> SppPipe for LinkPipe<L> {
    async fn send_cmd_frame(&self, frame: &CmdFrame) -> Result<Option<Vec<u8>>> {
        // Blocking I/O: run in place so concurrent tasks (status polling, IPP
        // serving) migrate off this worker during the round-trip.
        tokio::task::block_in_place(|| self.send_cmd_blocking(frame))
    }

    async fn send_data_frame(
        &self,
        frame: &DataFrame,
        read_response: bool,
    ) -> Result<Option<Vec<u8>>> {
        tokio::task::block_in_place(|| self.send_data_frame_blocking(frame, read_response))
    }
}
