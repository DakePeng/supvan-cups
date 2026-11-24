//! Device open helpers for `supvan://` and `mock://` URIs.
//!
//! `supvan://<name>` is the unified scheme — discovery cross-correlates USB,
//! Bluetooth and BLE candidates by the printer's self-reported name and
//! registers a per-name transport map via [`register_supvan`]. At open time
//! [`open_uri`] resolves the name to USB (preferred when present), then
//! Bluetooth, then BLE.
//!
//! Wireless connections are cached per address: reopening reuses the link
//! instead of redialing the printer, which would beep on every connect. Each
//! open validates the cached link with a CHECK_DEVICE round-trip; if that
//! fails the entry is evicted and a fresh link is dialed (which beeps once,
//! as expected for "printer came back").
//!
//! By default the link is held for as long as the app runs. Two settings
//! change that, for a printer that other hosts (a phone) also need:
//! `SUPVAN_ON_DEMAND=1` makes only jobs and explicit requests dial — status
//! polls use the link if it is up and otherwise leave the printer alone — and
//! `SUPVAN_IDLE_DISCONNECT_SECS=N` hangs up once nothing has asked for the
//! link for N seconds. [`disconnect_all`] hangs up at once.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use supvan_proto::printer::Printer;

use crate::battery_provider;
use crate::printer_device::{KsDevice, SharedPrinter};

pub const SUPVAN_SCHEME: &str = "supvan://";
pub const MOCK_SCHEME: &str = "mock://";

/// Whether this app serves `uri`.
pub fn is_ours(uri: &str) -> bool {
    uri.starts_with(SUPVAN_SCHEME) || uri.starts_with(MOCK_SCHEME)
}

/// Open connections, keyed by address. The map lock is held only briefly;
/// each printer sits behind its own async mutex.
type ConnCache = LazyLock<Mutex<HashMap<String, SharedPrinter>>>;

static BT_CONNS: ConnCache = LazyLock::new(Default::default);
#[cfg(all(feature = "ble", target_os = "linux"))]
static BLE_CONNS: ConnCache = LazyLock::new(Default::default);

/// Whether Bluetooth links are opened only for jobs and explicit requests
/// (`SUPVAN_ON_DEMAND=1`), never by a status poll.
pub fn on_demand() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("SUPVAN_ON_DEMAND").is_ok_and(|v| v == "1"))
}

/// How long an unused link is kept (`SUPVAN_IDLE_DISCONNECT_SECS`); `None`
/// keeps it for good.
fn idle_disconnect() -> Option<Duration> {
    static IDLE: OnceLock<Option<Duration>> = OnceLock::new();
    *IDLE.get_or_init(|| {
        crate::env::parsed("SUPVAN_IDLE_DISCONNECT_SECS", "a number of seconds", |s| {
            s.parse::<u64>().ok()
        })
        .filter(|&secs| secs > 0)
        .map(Duration::from_secs)
    })
}

/// When each link was last asked for by something that is allowed to dial.
static LAST_USED: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Default::default);

fn touch(addr: &str) {
    LAST_USED
        .lock()
        .unwrap()
        .insert(addr.to_string(), Instant::now());
}

fn link_event(addr: &str, connected: bool) {
    crate::events::emit(&crate::events::Event::Link {
        address: addr,
        connected,
    });
}

/// Drop `addr` from `cache`, announcing the hang-up if it was there.
fn evict(cache: &ConnCache, addr: &str) {
    if cache.lock().unwrap().remove(addr).is_some() {
        link_event(addr, false);
    }
}

/// Reuse the cached connection to `addr` if it still answers, else `dial` a
/// new one and cache it.
async fn cached_or_dial(
    cache: &ConnCache,
    addr: &str,
    dial: impl AsyncFnOnce() -> Option<Printer>,
) -> Option<SharedPrinter> {
    touch(addr);
    let cached = cache.lock().unwrap().get(addr).cloned();
    if let Some(printer) = cached {
        if printer.lock().await.check_device().await.unwrap_or(false) {
            log::debug!("device: reusing the cached link to {addr}");
            return Some(printer);
        }
        log::info!("device: cached link to {addr} is dead, reconnecting");
        evict(cache, addr);
    }
    log::info!("device: dialing {addr}");
    let printer = Arc::new(tokio::sync::Mutex::new(dial().await?));
    cache
        .lock()
        .unwrap()
        .insert(addr.to_string(), printer.clone());
    touch(addr);
    link_event(addr, true);
    Some(printer)
}

/// The cached connection to `addr` if there is one and it still answers.
/// Never dials. A link busy with a job is taken as alive.
async fn cached_alive(cache: &ConnCache, addr: &str) -> Option<SharedPrinter> {
    let printer = cache.lock().unwrap().get(addr).cloned()?;
    let alive = match printer.try_lock() {
        Ok(p) => p.check_device().await.unwrap_or(false),
        Err(_) => true,
    };
    if !alive {
        log::info!("device: link to {addr} has dropped");
        evict(cache, addr);
        return None;
    }
    Some(printer)
}

/// Hang up every wireless link now. A job in progress keeps its own handle,
/// so its link closes when the job ends. Returns how many were dropped.
pub fn disconnect_all() -> usize {
    let dropped: Vec<(String, SharedPrinter)> = BT_CONNS.lock().unwrap().drain().collect();
    for (addr, _) in &dropped {
        log::info!("device: disconnecting {addr}");
        link_event(addr, false);
    }
    dropped.len()
}

/// Whether any wireless link is currently up.
pub fn any_connected() -> bool {
    !BT_CONNS.lock().unwrap().is_empty()
}

/// Hang up links nothing has asked for within the idle window. A link a job
/// or poll is holding right now is left alone.
fn reap_idle(idle: Duration) {
    let expired: Vec<(String, SharedPrinter)> = {
        let last = LAST_USED.lock().unwrap();
        let mut conns = BT_CONNS.lock().unwrap();
        let addrs: Vec<String> = conns
            .iter()
            .filter(|(addr, printer)| {
                Arc::strong_count(printer) == 1
                    && last.get(*addr).is_none_or(|t| t.elapsed() >= idle)
            })
            .map(|(addr, _)| addr.clone())
            .collect();
        addrs
            .into_iter()
            .filter_map(|a| conns.remove(&a).map(|p| (a, p)))
            .collect()
    };
    for (addr, _) in &expired {
        log::info!("device: {addr} idle for {idle:?}, disconnecting");
        link_event(addr, false);
    }
}

/// Start the task that enforces `SUPVAN_IDLE_DISCONNECT_SECS`, if set.
pub fn spawn_idle_reaper() {
    let Some(idle) = idle_disconnect() else {
        return;
    };
    log::info!("device: idle links are dropped after {idle:?}");
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            tokio::task::block_in_place(|| reap_idle(idle));
        }
    });
}

/// A Classic-Bluetooth printer by address — an RFCOMM address on Linux, the
/// serial port the OS created for the paired printer elsewhere.
async fn open_bt(addr: &str, dial: bool) -> Option<KsDevice> {
    if !dial {
        return cached_alive(&BT_CONNS, addr).await.map(KsDevice::real);
    }
    let printer = cached_or_dial(&BT_CONNS, addr, async || {
        // Connecting blocks for up to the page timeout (an RFCOMM dial, or
        // the OS bringing the link up behind a serial open).
        let target = addr.to_string();
        match tokio::task::spawn_blocking(move || Printer::open_bt(&target)).await {
            Ok(Ok(p)) => {
                if let Some(h) = battery_provider::handle() {
                    h.add_device(addr, 100);
                }
                Some(p)
            }
            Ok(Err(e)) => {
                log::error!("device: RFCOMM connect to {addr} failed: {e}");
                None
            }
            Err(e) => {
                log::error!("device: RFCOMM connect task for {addr} failed: {e}");
                None
            }
        }
    })
    .await?;
    Some(KsDevice::real(printer))
}

/// A BLE printer by address.
#[cfg(all(feature = "ble", target_os = "linux"))]
async fn open_ble(addr: &str) -> Option<KsDevice> {
    let printer = cached_or_dial(&BLE_CONNS, addr, async || {
        Printer::open_ble(addr)
            .await
            .inspect_err(|e| log::error!("device: GATT connect to {addr} failed: {e}"))
            .ok()
    })
    .await?;
    Some(KsDevice::real(printer))
}

#[cfg(not(all(feature = "ble", target_os = "linux")))]
async fn open_ble(addr: &str) -> Option<KsDevice> {
    log::warn!("device: BLE address {addr} registered but the `ble` feature is off");
    None
}

/// A USB printer at a hidraw path. Not cached: reopening USB is silent.
fn open_usb(path: &str) -> Option<KsDevice> {
    log::info!("device: opening {path}");
    let printer = Printer::open_usb(path)
        .inspect_err(|e| log::error!("device: hidraw open of {path} failed: {e}"))
        .ok()?;
    Some(KsDevice::real(Arc::new(tokio::sync::Mutex::new(printer))))
}

/// Open a device from its URI, dialing a wireless printer if need be. `None`
/// for an unknown scheme or a printer that can't be reached.
pub async fn open_uri(uri: &str) -> Option<KsDevice> {
    open_uri_with(uri, true).await
}

/// Like [`open_uri`], but a wireless printer is only used if its link is
/// already up — for status polls under [`on_demand`]. USB and the mock open
/// as usual (neither disturbs anything).
pub async fn open_uri_passive(uri: &str) -> Option<KsDevice> {
    open_uri_with(uri, false).await
}

async fn open_uri_with(uri: &str, dial: bool) -> Option<KsDevice> {
    if let Some(slug) = uri.strip_prefix(SUPVAN_SCHEME) {
        open_supvan(slug, dial).await
    } else if uri.starts_with(MOCK_SCHEME) {
        open_mock()
    } else {
        None
    }
}

/// The mock device, unless the simulator says the hardware is unplugged —
/// then the open fails like real hardware would, so status reports OFFLINE
/// and the print path holds the job.
fn open_mock() -> Option<KsDevice> {
    if crate::mock::controller().is_unreachable() {
        log::info!("mock: device unreachable (SUPVAN_MOCK_UNREACHABLE)");
        return None;
    }
    Some(KsDevice::mock())
}

/// Transports discovery found for one `supvan://NAME` printer.
#[derive(Clone, Default)]
struct SupvanTransports {
    hidraw_path: Option<String>,
    bt_address: Option<String>,
    ble_address: Option<String>,
}

static SUPVAN_MAP: LazyLock<Mutex<HashMap<String, SupvanTransports>>> =
    LazyLock::new(Default::default);

/// Record the transports discovery found for `supvan://<slug>`. Called after
/// each discovery cycle.
pub fn register_supvan(
    slug: &str,
    hidraw_path: Option<String>,
    bt_address: Option<String>,
    ble_address: Option<String>,
) {
    SUPVAN_MAP.lock().unwrap().insert(
        slug.to_string(),
        SupvanTransports {
            hidraw_path,
            bt_address,
            ble_address,
        },
    );
}

/// Open `supvan://<slug>`: USB when present, else Bluetooth, else BLE. With
/// `dial` unset, Bluetooth is used only if already connected and BLE not at
/// all.
async fn open_supvan(slug: &str, dial: bool) -> Option<KsDevice> {
    let entry = SUPVAN_MAP.lock().unwrap().get(slug).cloned()?;

    if let Some(path) = entry.hidraw_path.as_deref() {
        if let Some(dev) = open_usb(path) {
            return Some(dev);
        }
        log::warn!("open_supvan: USB open failed for {slug} ({path}), falling back to BT");
    }
    if let Some(addr) = entry.bt_address.as_deref() {
        if let Some(dev) = open_bt(addr, dial).await {
            return Some(dev);
        }
        if !dial {
            return None;
        }
        log::warn!("open_supvan: BT open failed for {slug} ({addr}), trying BLE");
    }
    if !dial {
        return None;
    }
    if let Some(addr) = entry.ble_address.as_deref() {
        return open_ble(addr).await;
    }
    log::warn!("open_supvan: no transports for {slug}");
    None
}
