//! Application entry: IPP server, discovery, state.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};

use ipp_printer_app::{
    DeviceBackend, DiscoveredDevice, JobContext, JobFailure, JobOutcome, PollStatus, PrinterConfig,
    PrinterReason, PrinterRegistry, ReadyMedia, Server, ServerOptions, default_state_path,
};
use parking_lot::RwLock;
use supvan_proto::rfid::{RfidMaterial, heat_presets};
use supvan_proto::status::MaterialInfo;

use crate::ipp_job::{config_from_family, run_cups_raster_job};
use supvan_discover::ble::BleCandidate;
use supvan_discover::bt::BtCandidate;
use supvan_discover::models;
use supvan_discover::usb::UsbCandidate;

/// Labels remaining at or below which `printer-state-reasons` gets `media-low`.
/// Conservative — most label-printer ops want a few minutes of warning.
const MEDIA_LOW_THRESHOLD: u32 = 20;

/// Per-printer last-seen RFID tag identifiers; used to log roll swaps and
/// (in a future phase) refresh `media-col-ready`.
#[derive(Default, Clone, PartialEq)]
struct RollFingerprint {
    uuid: String,
    code: String,
    width_mm: u8,
    height_mm: u8,
}

impl From<&MaterialInfo> for RollFingerprint {
    fn from(m: &MaterialInfo) -> Self {
        Self {
            uuid: m.uuid.clone(),
            code: m.code.clone(),
            width_mm: m.width_mm,
            height_mm: m.height_mm,
        }
    }
}

/// Last-seen roll per printer name.
static ROLLS: LazyLock<Mutex<HashMap<String, RollFingerprint>>> = LazyLock::new(Default::default);

/// The current config of the printer named `name`.
fn config_by_name(registry: &PrinterRegistry, name: &str) -> Option<PrinterConfig> {
    registry
        .read()
        .iter()
        .find(|p| p.config.name == name)
        .map(|p| p.config.clone())
}

pub struct SupvanDeviceBackend;

/// Slugify a printer-reported name into something CUPS can use as a queue
/// name. Lowercase ASCII alphanumerics; everything else becomes a hyphen.
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s: String = s
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if s.is_empty() {
        "printer".to_string()
    } else {
        s
    }
}

#[async_trait::async_trait]
impl DeviceBackend for SupvanDeviceBackend {
    async fn list(&self) -> Vec<DiscoveredDevice> {
        if supvan_discover::is_mock_mode() {
            let family = models::default_family();
            let driver = &family.driver_name;
            let device_id = format!("MFG:Supvan;MDL:{};CMD:KASCRIPT;", family.make_and_model);
            log::info!("mock discovery: emitted mock://t50-001 (driver={driver})");
            return vec![DiscoveredDevice {
                info: "Supvan Mock".to_string(),
                uri: "mock://t50-001".to_string(),
                device_id,
            }];
        }

        // USB reads the printer's serial over RETURN_MAT; BT and BLE take the
        // advertised name, which is the same serial. BLE is a no-op without
        // the `ble` feature.
        let supvan_discover::Candidates { usb, bt, ble } = supvan_discover::scan_all().await;

        // Group by printer-reported name. USB candidates carry their
        // `device_sn` (parsed from `RETURN_MAT` at offset 40); BT and BLE carry
        // it as the advertised name. When they match, we collapse the
        // transports into one logical printer.
        //
        // If a USB candidate failed to surface its serial (e.g. the device
        // was busy and RETURN_MAT didn't reply), fall back to its bus URI
        // as the group key. A final 1-USB-only + 1-BT-only sweep merges
        // them under the BT name to keep single-printer households tidy.
        type Group = (
            Option<UsbCandidate>,
            Option<BtCandidate>,
            Option<BleCandidate>,
        );
        let mut by_name: BTreeMap<String, Group> = BTreeMap::new();
        for u in usb {
            let key = u.printer_name.clone().unwrap_or_else(|| u.port_key.clone());
            by_name.entry(key).or_default().0 = Some(u);
        }
        for b in bt {
            let key = b.name.clone();
            by_name.entry(key).or_default().1 = Some(b);
        }
        for e in ble {
            let key = e.name.clone();
            by_name.entry(key).or_default().2 = Some(e);
        }

        let usb_only: Vec<String> = by_name
            .iter()
            .filter(|(_, (u, b, e))| u.is_some() && b.is_none() && e.is_none())
            .map(|(k, _)| k.clone())
            .collect();
        let bt_only: Vec<String> = by_name
            .iter()
            .filter(|(_, (u, b, e))| u.is_none() && b.is_some() && e.is_none())
            .map(|(k, _)| k.clone())
            .collect();
        if usb_only.len() == 1 && bt_only.len() == 1 {
            let usb_key = usb_only.into_iter().next().unwrap();
            let bt_key = bt_only.into_iter().next().unwrap();
            log::info!(
                "discover: USB probe failed; cardinality fallback merging {usb_key} + {bt_key} under {bt_key}"
            );
            let usb_entry = by_name.remove(&usb_key).unwrap().0;
            by_name.get_mut(&bt_key).unwrap().0 = usb_entry;
        }

        let mut out = Vec::new();
        for (name, (usb, bt, ble)) in by_name {
            // USB reports its model over the wire; BT and BLE only give us the
            // advertised serial name, which the registry maps back to a model.
            let model = usb
                .as_ref()
                .map(|u| u.model_name.clone())
                .or_else(|| models::bt_model_for_name(&name).map(str::to_string))
                .unwrap_or_else(|| "T50 Series".to_string());
            let info = format!("Supvan {model} {name}");
            let slug = slug(&name);
            let uri = format!("{}{slug}", crate::device::SUPVAN_SCHEME);
            let device_id = format!("MFG:Supvan;MDL:{model};CMD:SUPVAN;");
            log::info!(
                "discover: emitting {uri} (usb={}, bt={}, ble={})",
                usb.is_some(),
                bt.is_some(),
                ble.is_some(),
            );
            // Register the name → transport mapping so open_supvan can resolve it.
            crate::device::register_supvan(
                &slug,
                usb.as_ref().map(|u| u.hidraw_path.clone()),
                bt.as_ref().map(|b| b.address.clone()),
                ble.as_ref().map(|e| e.address.clone()),
            );
            out.push(DiscoveredDevice {
                info,
                uri,
                device_id,
            });
        }
        out
    }

    async fn poll_status(&self, config: &PrinterConfig) -> Option<PollStatus> {
        collect_status(config, !crate::device::on_demand()).await
    }

    async fn identify(&self, config: &PrinterConfig, actions: &[String]) {
        // Map Identify-Printer to a physical beep via CHECK_DEVICE. Any action
        // keyword (display/sound/flash) triggers the same buzzer. Mock devices
        // no-op on identify.
        if let Some(dev) = crate::device::open_uri(&config.device_uri).await {
            log::info!("identify {} (actions={actions:?})", config.name);
            dev.identify().await;
        }
    }

    fn driver_for_device(&self, device_id: &str, device_uri: &str) -> Option<String> {
        if !device_id.is_empty()
            && let Some(mdl) = models::parse_mdl(device_id)
        {
            let family = models::family_for_model_hint(mdl);
            return Some(family.driver_name.clone());
        }
        if crate::device::is_ours(device_uri) {
            return Some(models::default_family().driver_name.clone());
        }
        None
    }
}

/// Read the printer's status and loaded roll, publish it as an event, and
/// return it in the framework's terms.
///
/// With `dial` unset a wireless printer is only asked if its link is already
/// up; otherwise it is left alone and reported as idle with nothing wrong, so
/// the queue keeps accepting jobs — the job itself dials, and is held if the
/// printer turns out to be unreachable.
async fn collect_status(config: &PrinterConfig, dial: bool) -> Option<PollStatus> {
    let dev = if dial {
        crate::device::open_uri(&config.device_uri).await
    } else {
        crate::device::open_uri_passive(&config.device_uri).await
    };
    let Some(dev) = dev else {
        crate::events::emit(&crate::events::Event::Status {
            printer: &config.name,
            connected: false,
            reasons: Vec::new(),
            media_mm: None,
            gap_mm: None,
            roll_locked: None,
            remaining: None,
        });
        if !dial {
            return Some(PollStatus::default());
        }
        // Device unreachable (powered off / unplugged / BT down). Report
        // OFFLINE so the framework marks us printer-state=stopped and CUPS
        // holds queued jobs until it's back — instead of accepting a job
        // we can't print and dropping it.
        return Some(PollStatus {
            reasons: PrinterReason::OFFLINE,
            ..Default::default()
        });
    };

    let mut media_mm = None;
    let mut gap_mm = None;
    let mut roll_locked = None;
    let mut remaining_labels = None;
    let mut reasons = dev.status().await;
    let mut ready_media = None;
    let mut supply_percent = None;

    // Material query: surfaces labels-remaining + roll-swap detection.
    // Skipped on mock devices (dev.material() returns None).
    if let Some(mat) = dev.material().await {
        let fp = RollFingerprint::from(&mat);
        let mut cache = ROLLS.lock().unwrap();
        let key = config.name.clone();
        match cache.get(&key) {
            Some(prev) if *prev != fp && !prev.uuid.is_empty() => {
                log::info!(
                    "{}: roll swap detected — was {}x{}mm uuid={} -> now {}x{}mm uuid={}",
                    key,
                    prev.width_mm,
                    prev.height_mm,
                    prev.uuid,
                    fp.width_mm,
                    fp.height_mm,
                    fp.uuid,
                );
            }
            None => {
                log::info!(
                    "{}: roll registered — {}x{}mm uuid={} remaining={:?}",
                    key,
                    fp.width_mm,
                    fp.height_mm,
                    fp.uuid,
                    mat.remaining,
                );
            }
            _ => {}
        }
        cache.insert(key, fp);

        // Publish the loaded roll as the dynamic media-ready / media-col-ready.
        // PWG self-describing name uses the om_ (metric) class; size in
        // hundredths of a millimetre.
        //
        // The roll's true width goes out here, unclamped: the stock the
        // 48mm head cannot reach on a 50mm roll is reported as a hard
        // margin instead (see `side_margin_hmm` in the IPP layer), so a
        // client composes into the imageable area rather than having the
        // driver crop a finished page.
        let (w, h) = (mat.width_mm as i32, mat.height_mm as i32);
        remaining_labels = mat.remaining;
        gap_mm = Some(mat.gap_mm);
        roll_locked = Some(!record_is_ours(&mat.uuid, &mat.code));
        if w > 0 && h > 0 {
            media_mm = Some([mat.width_mm, mat.height_mm]);
            ready_media = Some(ReadyMedia {
                name: format!("om_{w}x{h}mm_{w}x{h}mm"),
                size_hmm: [w * 100, h * 100],
                media_type: "labels".to_string(),
            });
        }

        if let Some(remaining) = mat.remaining {
            if remaining == 0 {
                reasons |= PrinterReason::MEDIA_EMPTY;
            } else if remaining <= MEDIA_LOW_THRESHOLD {
                reasons |= PrinterReason::MEDIA_LOW;
            }
            // The firmware reports remaining *labels*, not a percentage, and
            // we don't know the roll's original count. Clamp to 0–100 as a
            // gauge: full while plenty remain, counting down near empty.
            supply_percent = Some(remaining.min(100) as u8);
        }
    }

    crate::events::emit(&crate::events::Event::Status {
        printer: &config.name,
        connected: !dev.is_mock(),
        reasons: crate::events::reason_names(reasons),
        media_mm,
        gap_mm,
        roll_locked,
        remaining: remaining_labels,
    });

    Some(PollStatus {
        reasons,
        ready_media,
        supply_percent,
    })
}

pub async fn run_server(host: &str, port: u16) -> std::io::Result<()> {
    models::load();

    let registry: PrinterRegistry = Arc::new(RwLock::new(Vec::new()));
    let state_path = default_state_path("supvan-printer-app");
    let backend = Arc::new(SupvanDeviceBackend);

    Server::bootstrap_printers(&registry, backend.as_ref(), &state_path, config_from_family).await;

    prune_stale_supvan(&registry);
    Server::persist(&registry, &state_path);

    let registry_print = registry.clone();
    let print_job = Arc::new(
        move |ctx: JobContext, raster: Arc<[u8]>, copies: u32| -> ipp_printer_app::PrintJobFuture {
            let registry_print = registry_print.clone();
            Box::pin(async move {
                let Some(cfg) = config_by_name(&registry_print, &ctx.printer_name) else {
                    return JobOutcome::Failed(JobFailure::other(format!(
                        "printer not found: {}",
                        ctx.printer_name
                    )));
                };
                let spec = crate::ipp_job::JobSpec {
                    config: &cfg,
                    copies,
                    print_quality: ctx.print_quality,
                };
                // image/jpeg is decoded in-process (run_jpeg_job); everything else
                // is CUPS/PWG raster (CUPS' driverless path already rasterizes).
                let (printer, job) = (ctx.printer_name.as_str(), ctx.id);
                crate::events::emit(&crate::events::Event::JobStarted { printer, job });
                let result = if ctx.document_format == "image/jpeg" {
                    // Fallback when the config carries no media size: 40×30 mm,
                    // expressed in hundredths of a millimetre.
                    const DEFAULT_MEDIA_SIZE_HMM: [i32; 2] = [4000, 3000];
                    let media_size = cfg
                        .media_sizes
                        .first()
                        .copied()
                        .unwrap_or(DEFAULT_MEDIA_SIZE_HMM);
                    crate::ipp_job::run_jpeg_job(&spec, media_size, &raster).await
                } else {
                    run_cups_raster_job(&spec, &raster).await
                };
                match &result {
                    Ok(()) => crate::events::emit(&crate::events::Event::JobDone { printer, job }),
                    Err(f) if f.printer_reasons.is_recoverable() => {
                        crate::events::emit(&crate::events::Event::JobHeld {
                            printer,
                            job,
                            reasons: crate::events::reason_names(f.printer_reasons),
                            message: &f.message,
                        })
                    }
                    Err(f) => crate::events::emit(&crate::events::Event::JobFailed {
                        printer,
                        job,
                        reasons: crate::events::reason_names(f.printer_reasons),
                        message: &f.message,
                    }),
                }
                match result {
                    Ok(()) => JobOutcome::Completed,
                    // A clearable physical condition — printer off / BT down, paper
                    // jam, out of labels, cover open — should HOLD the job and let
                    // the framework retry until it's resolved, not drop it (the way
                    // a real printer holds a job through a jam). Anything else is a
                    // permanent failure for this document.
                    Err(f) if f.printer_reasons.is_recoverable() => JobOutcome::DeviceUnavailable {
                        reasons: f.printer_reasons,
                    },
                    Err(f) => JobOutcome::Failed(f),
                }
            })
        },
    );

    // CUPS-managed-queue model (IPP Everywhere / Printer Application): we do
    // NOT create or own a CUPS queue. We are a self-contained IPP Everywhere
    // server that advertises over DNS-SD; CUPS discovers us and spins up a
    // temporary on-demand queue (auto-removed when idle), exactly as it does
    // for an AirPrint printer. This requires `cups-browsed` to be off — it
    // would otherwise build a broken same-host `implicitclass://` queue from
    // our advert (it's legacy; modern cupsd does driverless natively).
    let registry_media = registry.clone();
    let media_change: ipp_printer_app::MediaChangeFn = Arc::new(
        move |printer_name: String, media: ipp_printer_app::ReadyMedia| {
            let registry_media = registry_media.clone();
            Box::pin(async move {
                let config = config_by_name(&registry_media, &printer_name)
                    .ok_or_else(|| format!("no printer named {printer_name}"))?;
                apply_media(&printer_name, &config.device_uri, &media).await
            })
        },
    );

    // Front-end plumbing: announce what we serve, enforce the idle hang-up,
    // and take connect/disconnect/quit from the supervisor.
    crate::events::emit(&crate::events::Event::Printers {
        printers: registry
            .read()
            .iter()
            .map(|p| crate::events::PrinterInfo {
                name: p.config.name.clone(),
                info: p.config.display_name.clone(),
                uri: format!("ipp://localhost:{port}/ipp/print/{}", p.config.name),
            })
            .collect(),
    });
    crate::device::spawn_idle_reaper();
    if let Some(mut commands) = crate::events::commands() {
        let registry_cmd = registry.clone();
        tokio::spawn(async move {
            // One look at the printer on startup, so the queue advertises the
            // roll that is actually loaded. Silent if it isn't reachable.
            refresh_all(&registry_cmd, false).await;
            while let Some(cmd) = commands.recv().await {
                match cmd {
                    crate::events::Command::Connect => refresh_all(&registry_cmd, true).await,
                    crate::events::Command::Disconnect => {
                        crate::device::disconnect_all();
                    }
                    crate::events::Command::SetLabel {
                        width_mm,
                        length_mm,
                        gap_mm,
                    } => set_label_all(&registry_cmd, width_mm, length_mm, gap_mm).await,
                    crate::events::Command::PrintPgm { density, path } => {
                        print_pgm_all(&registry_cmd, density, &path).await
                    }
                    crate::events::Command::SetDarkness(d) => {
                        crate::ipp_job::set_darkness(i32::from(d));
                    }
                    crate::events::Command::SetDither(mode) => crate::job::set_dither_mode(mode),
                    crate::events::Command::Quit => {
                        log::info!("supervisor asked us to quit");
                        crate::device::disconnect_all();
                        std::process::exit(0);
                    }
                }
            }
        });
    }

    let advertise_mdns = !std::env::var("SUPVAN_MDNS").is_ok_and(|v| v == "0");

    Server::run(ServerOptions {
        host: host.to_string(),
        port,
        printers: registry,
        device_backend: backend,
        print_job,
        media_change: Some(media_change),
        state_path,
        // Advertise the DNS-SD service directly at bind time. No queue UUID to
        // stamp (we own no queue), so there's nothing to coordinate first.
        // `SUPVAN_MDNS=0` turns it off for a loopback-only instance whose
        // advert nobody could reach.
        advertise_mdns,
    })
    .await
}

/// Read a binary PGM (`P5`, maxval 255): `(pixels, width, height)`.
fn read_pgm(path: &str) -> Result<(Vec<u8>, u32, u32), String> {
    let data = std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let mut pos = 0;
    let mut field = || -> Option<&[u8]> {
        while data.get(pos)?.is_ascii_whitespace() {
            pos += 1;
        }
        let start = pos;
        while pos < data.len() && !data[pos].is_ascii_whitespace() {
            pos += 1;
        }
        Some(&data[start..pos])
    };
    let number = |f: Option<&[u8]>| -> Option<u32> { std::str::from_utf8(f?).ok()?.parse().ok() };
    let magic = field().map(<[u8]>::to_vec);
    let (width, height, max) = (number(field()), number(field()), number(field()));
    let (Some(width), Some(height)) = (width, height) else {
        return Err(format!("{path}: not a PGM"));
    };
    if magic.as_deref() != Some(b"P5") || max != Some(255) || width == 0 || height == 0 {
        return Err(format!("{path}: need a binary 8-bit PGM"));
    }
    let pixels = data
        .get(pos + 1..pos + 1 + (width * height) as usize)
        .ok_or_else(|| format!("{path}: truncated"))?;
    Ok((pixels.to_vec(), width, height))
}

/// Print a front end's own page on every printer we serve, at one density.
async fn print_pgm_all(registry: &PrinterRegistry, density: u8, path: &str) {
    let configs: Vec<PrinterConfig> = registry.read().iter().map(|p| p.config.clone()).collect();
    for config in configs {
        let result = print_pgm(
            &config.device_uri,
            config.printhead_width_dots,
            density,
            path,
        )
        .await;
        if let Err(e) = &result {
            log::warn!("{}: {path} not printed: {e}", config.name);
        }
        crate::events::emit(&crate::events::Event::TestPrint {
            printer: &config.name,
            ok: result.is_ok(),
            message: result.as_ref().err().map_or("", String::as_str),
        });
    }
}

async fn print_pgm(
    device_uri: &str,
    head_dots: u32,
    density: u8,
    path: &str,
) -> Result<(), String> {
    use supvan_proto::buffer::Density;

    let (gray, width, height) = read_pgm(path)?;
    let page = supvan_proto::page::threshold_image(
        &gray,
        width,
        height,
        head_dots,
        Density::uniform(density),
    )
    .map_err(|e| e.to_string())?;
    let dev = crate::device::open_uri(device_uri)
        .await
        .ok_or_else(|| format!("cannot reach {device_uri}"))?;
    let printer = dev
        .printer()
        .await
        .ok_or("mock device: nothing to print on")?;
    log::info!("print_pgm: {width}x{height} dots at density {density}");
    printer
        .print_buffers(&page)
        .await
        .map_err(|e| format!("print failed: {e}"))
}

/// Write a label geometry to every printer we serve, report each outcome,
/// and publish the status that results.
async fn set_label_all(registry: &PrinterRegistry, width_mm: u8, length_mm: u8, gap_mm: u8) {
    let configs: Vec<PrinterConfig> = registry.read().iter().map(|p| p.config.clone()).collect();
    for config in configs {
        let result = apply_label(
            &config.name,
            &config.device_uri,
            width_mm,
            length_mm,
            Some(gap_mm),
        )
        .await;
        if let Err(e) = &result {
            log::warn!("{}: label not set: {e}", config.name);
        }
        crate::events::emit(&crate::events::Event::LabelSet {
            printer: &config.name,
            ok: result.is_ok(),
            message: result.as_ref().err().map_or("", String::as_str),
        });
        collect_status(&config, true).await;
    }
}

/// Dial every printer we serve and publish its status. `announce_failure`
/// adds a `connect_failed` event for the ones that can't be reached.
async fn refresh_all(registry: &PrinterRegistry, announce_failure: bool) {
    let configs: Vec<PrinterConfig> = registry.read().iter().map(|p| p.config.clone()).collect();
    for config in configs {
        collect_status(&config, true).await;
        if announce_failure
            && !config.device_uri.starts_with(crate::device::MOCK_SCHEME)
            && !crate::device::any_connected()
        {
            crate::events::emit(&crate::events::Event::ConnectFailed {
                printer: &config.name,
            });
        }
    }
}

/// Heat times to write when provisioning, from `SUPVAN_HEAT` as `heat5:heat40`.
///
/// The material record carries heat but `RETURN_MAT` never returns it
/// (`MaterialInfo` has no such field), so we cannot preserve what is already on
/// the roll — a provision necessarily sets it. Taking it from the environment
/// keeps that an explicit choice rather than a silent reset, and matches how the
/// app takes the rest of its configuration.
fn configured_heat() -> (u16, u16) {
    crate::env::parsed("SUPVAN_HEAT", "`heat5:heat40`", parse_heat)
        .unwrap_or(heat_presets::STANDARD)
}

fn parse_heat(raw: &str) -> Option<(u16, u16)> {
    let (h5, h40) = raw.split_once(':')?;
    Some((h5.trim().parse().ok()?, h40.trim().parse().ok()?))
}

/// Whether this material record is ours to overwrite.
///
/// Three cases reach us, and only one must be protected:
///
/// - **Blank stock** — all-zero UUID. The vendor's own gate
///   (`UUID.indexOf("00000000") == -1`) and plainly ours to define.
/// - **A record we wrote** — a synthetic UUID like `30000000000000`. Not
///   all-zero, so the vendor's test alone would refuse it and a roll could be
///   set exactly once, then never corrected.
/// - **A genuine consumable** — carries an 8-byte tag signature in `code`,
///   which is what separates it from the other two: `getT50PlusRFIDData` never
///   populates `MatCode`, so anything we write has `code` all zeros, and blank
///   stock has nothing to read.
///
/// So the signature is the discriminator, not the UUID.
///
/// Caveat worth stating: this assumes a genuine Supvan roll really does carry a
/// non-zero signature. That is what the field is for and what the vendor's
/// anti-counterfeit path reads, but it is unverified here — the bench has only
/// third-party stock. Erring the other way (refusing on any non-zero UUID)
/// would make the feature single-use, so this is the deliberate trade.
fn record_is_ours(uuid: &str, code: &str) -> bool {
    let zeroed = |s: &str| s.is_empty() || s.chars().all(|c| c == '0');

    // A signature is enough on its own to mean hands off, whatever the UUID.
    if !zeroed(code) {
        return false;
    }
    if zeroed(uuid) {
        return true;
    }

    // Ours are `RfidMaterial::uuid_bytes()` output: a decimal catalogue code
    // (u16, so at most five digits) right-padded with zeros to 14 hex chars.
    // Strip the padding and what remains must be those few digits — a real
    // 7-byte tag UID is hex and would not reduce to that.
    let stem = uuid.trim_end_matches('0');
    stem.len() <= 5 && stem.chars().all(|c| c.is_ascii_digit())
}

/// Push operator-set media geometry to the printer as a synthetic material
/// record.
///
/// **Refuses when a genuine RFID tag is present.** A real consumable describes
/// itself, and that description is what `RETURN_MAT` feeds into `media-ready`,
/// the remaining-labels gauge and roll-swap detection. Overwriting it would
/// replace measured truth with a typed-in guess, and 0x5D offers no way back.
/// Only blank stock — which reads as an all-zero UUID — is ours to define.
async fn apply_media(
    printer_name: &str,
    device_uri: &str,
    media: &ipp_printer_app::ReadyMedia,
) -> Result<(), String> {
    apply_label(
        printer_name,
        device_uri,
        (media.size_hmm[0] / 100) as u8,
        (media.size_hmm[1] / 100) as u8,
        None,
    )
    .await
}

/// [`apply_media`] in millimetres, optionally setting the inter-label gap too
/// (`None` keeps the gap the roll's record already has).
async fn apply_label(
    printer_name: &str,
    device_uri: &str,
    width_mm: u8,
    height_mm: u8,
    gap_mm: Option<u8>,
) -> Result<(), String> {
    use supvan_proto::rfid::{MAX_LABEL_LENGTH_MM, MAX_LABEL_WIDTH_MM};
    if width_mm == 0
        || width_mm > MAX_LABEL_WIDTH_MM
        || height_mm == 0
        || height_mm > MAX_LABEL_LENGTH_MM
    {
        return Err(format!(
            "{width_mm}x{height_mm}mm is out of range (up to {MAX_LABEL_WIDTH_MM}x{MAX_LABEL_LENGTH_MM}mm)"
        ));
    }
    let dev = crate::device::open_uri(device_uri)
        .await
        .ok_or_else(|| format!("cannot reach {device_uri}"))?;
    let printer = dev
        .printer()
        .await
        .ok_or("mock device: nothing to provision")?;

    let current = printer
        .query_material()
        .await
        .map_err(|e| format!("cannot read the loaded material: {e}"))?;

    if let Some(ref m) = current
        && !record_is_ours(&m.uuid, &m.code)
    {
        return Err(format!(
            "a genuine RFID roll is loaded (UUID {}, signature {}, {}x{}mm) and describes \
             itself — its own geometry is authoritative. Only blank stock, or a record this \
             app wrote, can be set here.",
            m.uuid, m.code, m.width_mm, m.height_mm
        ));
    }

    if current.as_ref().is_some_and(|m| {
        m.width_mm == width_mm && m.height_mm == height_mm && gap_mm.is_none_or(|g| g == m.gap_mm)
    }) {
        log::info!("{printer_name}: media already {width_mm}x{height_mm}mm, not rewriting");
        return Ok(());
    }

    // Carry over everything the read *does* expose, so setting a size doesn't
    // quietly discard the rest of the record.
    let heat = configured_heat();
    let mut mat = RfidMaterial {
        width_mm,
        length_mm: height_mm,
        heat_time_5: heat.0,
        heat_time_40: heat.1,
        ..Default::default()
    };
    if let Some(ref m) = current {
        if m.gap_mm > 0 {
            mat.gap_mm = m.gap_mm;
        }
        if m.label_type > 0 {
            mat.mat_type = m.label_type;
        }
        if let Some(left) = m.remaining.filter(|&n| n > 0) {
            mat.remaining = left;
        }
    }
    if let Some(gap) = gap_mm {
        mat.gap_mm = gap;
    }

    let written = printer
        .provision_material(&mat)
        .await
        .map_err(|e| format!("the printer did not take the record: {e}"))?;

    // Record the fingerprint we just wrote, so the status poller doesn't report
    // our own change as an operator swapping the roll.
    ROLLS
        .lock()
        .unwrap()
        .insert(printer_name.to_string(), RollFingerprint::from(&written));
    log::info!(
        "{printer_name}: media set to {}x{}mm gap {}mm, heat {}/{}",
        written.width_mm,
        written.height_mm,
        written.gap_mm,
        heat.0,
        heat.1
    );
    Ok(())
}

/// Drop persisted entries whose URI scheme this build no longer recognises
/// (e.g. legacy `usbhid://` / `btrfcomm://` from before the supvan:// unification).
/// Live `supvan://` entries are kept; the next discovery cycle re-registers
/// the transport mapping.
fn prune_stale_supvan(registry: &PrinterRegistry) {
    let mut guard = registry.write();
    guard.retain(|p| {
        let uri = &p.config.device_uri;
        let keep = crate::device::is_ours(uri);
        if !keep {
            log::info!("pruning legacy-scheme printer: {uri}");
        }
        keep
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ownership test is the whole safety gate. It has to say yes to blank
    /// stock and to records we wrote — otherwise a roll could be set once and
    /// never corrected — while still refusing a genuine signed consumable.
    #[test]
    fn ownership_gate_protects_only_genuine_tags() {
        // Blank stock.
        assert!(record_is_ours("00000000000000", "0000000000000000"));
        assert!(record_is_ours("", ""));

        // Records we wrote: a synthetic UUID, but never a signature. Observed
        // live on a T50M Pro after `provision --code 30000`.
        assert!(record_is_ours("30000000000000", "0000000000000000"));
        assert!(record_is_ours("56180000000000", "0000000000000000"));

        // A genuine consumable carries a signature — the one case to refuse.
        assert!(!record_is_ours("A1B2C3D4E5F607", "1122334455667788"));
        assert!(
            !record_is_ours("00000000000000", "0000000000000001"),
            "a signature alone is enough to mean hands off"
        );
    }

    /// Heat is unreadable from the device, so a bad `SUPVAN_HEAT` must fall back
    /// rather than fail a media change — and must never silently yield 0/0,
    /// which would print nothing at all.
    #[test]
    fn heat_config_rejects_junk() {
        assert_eq!(parse_heat("1700:1200"), Some((1700, 1200)));
        assert_eq!(parse_heat(" 1700 : 1200 "), Some((1700, 1200)));
        assert_eq!(parse_heat("nonsense"), None);
        assert_eq!(parse_heat("1700"), None);
    }
}
