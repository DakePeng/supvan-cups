//! Classic-Bluetooth discovery on macOS: the devices the OS has paired.
//!
//! IOBluetooth has no socket, but it does know every paired device, and a
//! Supvan printer is recognisable from its name and OUI alone (the same test
//! BlueZ discovery applies). So a candidate is a paired device the model
//! registry accepts, addressed by its Bluetooth address, which
//! [`supvan_proto::printer::Printer::open_bt`] dials through IOBluetooth.
//!
//! Pairing stays with the user (System Settings → Bluetooth); nothing here
//! runs an inquiry or pairs. `SUPVAN_SERIAL_PORTS` still adds serial ports
//! for the odd setup that needs them.

pub use crate::bt_serial::BtCandidate;
use crate::models;

/// Paired Supvan printers. Blocking-shaped to match the BlueZ scanner, though
/// the paired list comes back at once.
pub fn list_candidates() -> Vec<BtCandidate> {
    let mut out = crate::bt_serial::env_port_candidates();
    for dev in supvan_proto::paired_bt_devices() {
        if !models::is_matching_bt_device(&dev.address, &dev.name) {
            log::debug!(
                "bt discover: {} ({}) — not a Supvan printer, skipping",
                dev.name,
                dev.address
            );
            continue;
        }
        log::info!("bt discover: reporting {} ({})", dev.name, dev.address);
        out.push(BtCandidate {
            address: dev.address,
            name: dev.name,
        });
    }
    out
}
