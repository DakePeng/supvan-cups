//! Supvan IPP Everywhere printer application.
//!
//! Wires the `ipp-printer-app` framework (the IPP/HTTP server + job model) to
//! the Supvan device layer (`supvan-proto`): discovers USB + Bluetooth printers
//! and unifies them into one `supvan://` device, decodes incoming jobs
//! (PWG/CUPS raster or `image/jpeg`) to the printhead bitmap, and drives the
//! transfer. An in-process registrar auto-creates the direct CUPS queue and
//! coexists with `cups-browsed` via a matching mDNS `UUID=` key. The binary
//! takes no arguments; it is configured via `SUPVAN_*` environment variables.

#[cfg(target_os = "linux")]
mod battery_provider;
/// Battery reporting rides BlueZ's BatteryProvider1; nowhere else has one.
#[cfg(not(target_os = "linux"))]
mod battery_provider {
    pub struct BatteryProviderHandle;
    impl BatteryProviderHandle {
        pub fn add_device(&self, _addr: &str, _percentage: u8) {}
    }
    pub fn handle() -> Option<&'static BatteryProviderHandle> {
        None
    }
}
mod device;
mod dump;
mod env;
mod events;
mod ipp_job;
mod ipp_server;
mod job;
mod mock;
mod printer_device;

/// IPP port when `SUPVAN_PORT` isn't set — the printer-app convention's 8631.
const DEFAULT_PORT: u16 = 8631;

async fn async_main() -> i32 {
    let host = std::env::var("SUPVAN_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let port =
        env::parsed("SUPVAN_PORT", "a port number", |s| s.parse().ok()).unwrap_or(DEFAULT_PORT);

    match ipp_server::run_server(&host, port).await {
        Ok(()) => 0,
        Err(e) => {
            log::error!("server error: {e}");
            1
        }
    }
}

fn main() {
    let _ = env_logger::try_init();
    // The runtime lives on a worker thread: on macOS the main thread must run
    // the system run loop for IOBluetooth (a no-op wrapper elsewhere).
    let code = supvan_proto::run_with_main_loop(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(async_main())
    });
    std::process::exit(code);
}
