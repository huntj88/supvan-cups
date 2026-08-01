//! Application entry: IPP server, discovery, state.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use ipp_printer_app::{
    DeviceBackend, DiscoveredDevice, JobContext, JobFailure, JobOutcome, PersistedState,
    PollStatus, PrinterConfig, PrinterReason, PrinterRegistry, ReadyMedia, Server, ServerOptions,
    default_state_path,
};
use futures::stream::StreamExt;
use parking_lot::RwLock;
use supvan_proto::profile::PrintProfile;

use crate::ble_discover::BleCandidate;
use crate::discover::BtCandidate;
use crate::ipp_job::{JobTarget, config_from_family, run_cups_raster_job};
use crate::models;
use crate::usb_discover::UsbCandidate;
use crate::util::slug;

/// Threshold below which the printer-state-reasons gets the MEDIA_LOW flag.
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

fn roll_cache() -> &'static Mutex<HashMap<String, RollFingerprint>> {
    static C: OnceLock<Mutex<HashMap<String, RollFingerprint>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The device URI [`SupvanDeviceBackend::list`](SupvanDeviceBackend) emits for a printer whose
/// firmware self-id is `name`. Also what the framework persists and matches an
/// already-configured printer on, so it is the join key between a live
/// candidate and the saved registry.
fn device_uri_for(name: &str) -> String {
    format!("supvan://{}", slug(name))
}

pub struct SupvanDeviceBackend {
    /// Device URIs already in the persisted registry, snapshotted at startup.
    ///
    /// Only used to decide which candidates are worth an `RD_DEV_NAME` probe.
    /// `Server::bootstrap_printers` loads the saved printers *before* calling
    /// [`DeviceBackend::list`] and then drops any discovered device whose URI
    /// it already holds, so for these the probe's answer is computed and
    /// thrown away — it feeds `MDL:`, which feeds `driver_for_device`, which
    /// only runs for devices that are new.
    known_uris: HashSet<String>,
}

impl SupvanDeviceBackend {
    /// Read the persisted registry to seed [`Self::known_uris`]. A missing or
    /// unreadable state file yields an empty set, which only means the first
    /// run probes every candidate — the correct behaviour, since nothing is
    /// yet known about any of them.
    pub fn new(state_path: &std::path::Path) -> Self {
        let known_uris: HashSet<String> = PersistedState::load(state_path)
            .printers
            .into_iter()
            .map(|p| p.device_uri)
            .collect();
        log::debug!(
            "discover: {} printer(s) already configured",
            known_uris.len()
        );
        Self { known_uris }
    }

    /// Whether a BT candidate is worth spending an `RD_DEV_NAME` probe on.
    ///
    /// The probe is the only way to learn a BT-only printer's model — BlueZ
    /// exposes just the firmware serial — and the model picks the driver
    /// family, hence the wire protocol. But it costs an exclusive RFCOMM
    /// connection, which locks out the vendor app and blocks for the kernel's
    /// connect timeout on a printer that is switched off. So it is only spent
    /// where it can still change the outcome:
    ///
    /// - An **already-configured** printer keeps the driver it was added with;
    ///   `bootstrap_printers` discards this candidate wholesale.
    /// - A name `bt_patterns` pins to the **T-series** flow has already
    ///   decided its protocol, and no model name can move it.
    ///
    /// Everything else is probed: a name that pins nothing could be an
    /// E-series unit with a hardware code the registry doesn't list, and a
    /// name that pins the E-series still gets a sharper `MDL:` from its
    /// marketing name.
    fn needs_model_probe(&self, candidate: &BtCandidate) -> bool {
        if self.known_uris.contains(&device_uri_for(&candidate.name)) {
            log::debug!(
                "discover: {} ({}) is already configured, skipping the RD_DEV_NAME probe",
                candidate.name,
                candidate.address
            );
            return false;
        }
        match models::family_from_bt_patterns(&candidate.name) {
            Some(f) if f.print_profile == PrintProfile::TSeries => {
                log::debug!(
                    "discover: {} ({}) is pinned to the T-series flow by bt_patterns, \
                     skipping the RD_DEV_NAME probe",
                    candidate.name,
                    candidate.address
                );
                false
            }
            _ => true,
        }
    }

    /// Cap on in-flight probes: each may dial a fresh RFCOMM socket and hold
    /// it for [`PROBE_TIMEOUT`] on the blocking pool.
    const MAX_CONCURRENT_PROBES: usize = 4;

    /// Fill in [`BtCandidate::model_name`] for the candidates that need it.
    ///
    /// Probes run concurrently — a house with several powered-off printers
    /// would otherwise pay [`PROBE_TIMEOUT`] once per device — but no more
    /// than [`Self::MAX_CONCURRENT_PROBES`] at a time. Order is preserved, so
    /// results still line up with `candidates`.
    async fn probe_bt_models(&self, candidates: &mut [BtCandidate]) {
        // Decided up front so the futures below borrow nothing.
        let wanted: Vec<Option<String>> = candidates
            .iter()
            .map(|c| self.needs_model_probe(c).then(|| c.address.clone()))
            .collect();
        let names: Vec<Option<String>> = futures::stream::iter(wanted)
            .map(|addr| async move {
                match addr {
                    Some(a) => crate::device::probe_bt_model_name(&a).await,
                    None => None,
                }
            })
            .buffered(Self::MAX_CONCURRENT_PROBES)
            .collect()
            .await;
        for (c, name) in candidates.iter_mut().zip(names) {
            c.model_name = name;
        }
    }
}

#[async_trait::async_trait]
impl DeviceBackend for SupvanDeviceBackend {
    async fn list(&self) -> Vec<DiscoveredDevice> {
        if crate::util::is_mock_mode() {
            let family = models::default_family();
            let driver = family.driver_name.to_string_lossy();
            let mdl = String::from_utf8_lossy(&family.make_and_model).into_owned();
            let device_id = format!("MFG:Supvan;MDL:{mdl};CMD:KASCRIPT;");
            log::info!("mock discovery: emitted mock://t50-001 (driver={driver})");
            return vec![DiscoveredDevice {
                info: "Supvan Mock".to_string(),
                uri: "mock://t50-001".to_string(),
                device_id,
            }];
        }

        // Collect all candidates. USB probes RD_DEV_NAME silently per device;
        // BT pulls the firmware-reported name from BlueZ; BLE scans for
        // E11/E12-class advertisers (no-op without the `ble` feature).
        let usb = crate::usb_discover::list_candidates().await;
        let mut bt = crate::discover::list_candidates();
        let ble = crate::ble_discover::list_candidates().await;

        // Enumeration is probe-free; ask the wire for a model name only where
        // it can still decide something.
        self.probe_bt_models(&mut bt).await;

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
            let key = u.printer_name.clone().unwrap_or_else(|| u.uri_id.clone());
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
            let model = usb
                .as_ref()
                .map(|u| u.model_name.clone())
                // `MDL:` picks the driver family, and BT-only printers report
                // their model on the wire.
                .or_else(|| bt.as_ref().and_then(|b| b.model_name.clone()))
                // No `RD_DEV_NAME` (probe skipped or the unit was busy): the
                // advertised name still carries the hardware code
                // `bt_patterns` routes on (`T0143…` is an E-series). Naming a
                // family outright instead sends every unprobed T80/G/TP/E unit
                // to the T-series flow, which prints the E-series blank.
                .or_else(|| bt.as_ref().map(|_| name.clone()))
                // BLE discovery only ever scans for the E-series, and
                // `e-series` is a `bt_patterns` key that routes there.
                .or_else(|| ble.as_ref().map(|_| "E-Series".to_string()))
                .unwrap_or_else(|| "T50 Series".to_string());
            // The model stands in for itself when it *is* the advertised name.
            let info = if model == name {
                format!("Supvan {name}")
            } else {
                format!("Supvan {model} {name}")
            };
            let uri = device_uri_for(&name);
            let device_id = format!("MFG:Supvan;MDL:{model};CMD:SUPVAN;");
            log::info!(
                "discover: emitting {uri} (usb={}, bt={}, ble={})",
                usb.is_some(),
                bt.is_some(),
                ble.is_some(),
            );
            // Register the name → transport mapping so open_supvan can resolve it.
            crate::device::register_supvan(
                &slug(&name),
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
        let dev = crate::device::open_uri(
            &config.device_uri,
            models::profile_for_driver(&config.driver_name),
        )
        .await;
        let Some(dev) = dev else {
            // Device unreachable (powered off / unplugged / BT down). Report
            // OFFLINE so the framework marks us printer-state=stopped and CUPS
            // holds queued jobs until it's back — instead of accepting a job
            // we can't print and dropping it.
            return Some(PollStatus {
                reasons: PrinterReason::OFFLINE,
                ..Default::default()
            });
        };

        // Status decoding is model-specific (the E-series flags ribbon_end
        // during normal prints), so resolve the family's profile first.
        let mut reasons = dev.status().await;
        let mut ready_media = None;
        let mut supply_percent = None;

        // Material query: surfaces labels-remaining + roll-swap detection.
        // Skipped on mock devices (dev.material() returns None).
        if let Some(mat) = dev.material().await {
            let fp = RollFingerprint {
                uuid: mat.uuid.clone(),
                code: mat.code.clone(),
                width_mm: mat.width_mm,
                height_mm: mat.height_mm,
            };
            let mut cache = roll_cache().lock().unwrap();
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
            let (w, h) = (mat.width_mm as i32, mat.height_mm as i32);
            if w > 0 && h > 0 {
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
                    reasons |= PrinterReason::MARKER_SUPPLY_LOW;
                }
                // The firmware reports remaining *labels*, not a percentage, and
                // we don't know the roll's original count. Clamp to 0–100 as a
                // gauge: full while plenty remain, counting down near empty.
                supply_percent = Some(remaining.min(100) as u8);
            }
        }

        Some(PollStatus {
            reasons,
            ready_media,
            supply_percent,
        })
    }

    async fn identify(&self, config: &PrinterConfig, actions: &[String]) {
        // Map Identify-Printer to a physical beep via CHECK_DEVICE. Any action
        // keyword (display/sound/flash) triggers the same buzzer. Mock devices
        // no-op on identify.
        if let Some(dev) = crate::device::open_uri(
            &config.device_uri,
            models::profile_for_driver(&config.driver_name),
        )
        .await
        {
            log::info!("identify {} (actions={actions:?})", config.name);
            dev.identify().await;
        }
    }

    fn driver_for_device(&self, device_id: &str, device_uri: &str) -> Option<String> {
        if !device_id.is_empty()
            && let Some(mdl) = models::parse_mdl(device_id)
        {
            let family = models::family_for_model_hint(mdl);
            return Some(family.driver_name.to_string_lossy().into_owned());
        }
        if device_uri.starts_with("supvan://") || device_uri.starts_with("mock://") {
            return Some(
                models::default_family()
                    .driver_name
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        None
    }
}

pub async fn run_server(host: &str, port: u16) -> std::io::Result<()> {
    models::load();

    let registry: PrinterRegistry = Arc::new(RwLock::new(Vec::new()));
    let state_path = default_state_path("supvan-printer-app");
    let backend = Arc::new(SupvanDeviceBackend::new(&state_path));

    Server::bootstrap_printers(&registry, backend.as_ref(), &state_path, config_from_family).await;

    prune_stale_supvan(&registry);
    Server::persist(&registry, &state_path);

    let registry_print = registry.clone();
    let print_job = Arc::new(
        move |ctx: JobContext, raster: Arc<[u8]>, copies: u32| -> ipp_printer_app::PrintJobFuture {
            let registry_print = registry_print.clone();
            Box::pin(async move {
                let cfg = {
                    let guard = registry_print.read();
                    match guard.iter().find(|p| p.config.name == ctx.printer_name) {
                        Some(p) => p.config.clone(),
                        None => {
                            return JobOutcome::Failed(JobFailure::other(format!(
                                "printer not found: {}",
                                ctx.printer_name
                            )));
                        }
                    }
                };
                // image/jpeg is decoded in-process (run_jpeg_job); everything else
                // is CUPS/PWG raster (CUPS' driverless path already rasterizes).
                let result = if ctx.document_format == "image/jpeg" {
                    // Fallback when the config carries no media size: 40×30 mm,
                    // expressed in hundredths of a millimetre.
                    const DEFAULT_MEDIA_SIZE_HMM: [i32; 2] = [4000, 3000];
                    let media_size = cfg
                        .media_sizes
                        .first()
                        .copied()
                        .unwrap_or(DEFAULT_MEDIA_SIZE_HMM);
                    crate::ipp_job::run_jpeg_job(JobTarget::from(&cfg), media_size, &raster, copies)
                        .await
                } else {
                    run_cups_raster_job(JobTarget::from(&cfg), &raster, copies).await
                };
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
    Server::run(ServerOptions {
        host: host.to_string(),
        port,
        printers: registry,
        device_backend: backend,
        print_job,
        state_path,
        // Advertise the DNS-SD service directly at bind time. No queue UUID to
        // stamp (we own no queue), so there's nothing to coordinate first.
        advertise_mdns: true,
    })
    .await
}

/// Drop persisted entries whose URI scheme this build no longer recognises
/// (e.g. legacy `usbhid://` / `btrfcomm://` from before the supvan:// unification).
/// Live `supvan://` entries are kept; the next discovery cycle re-registers
/// the transport mapping.
fn prune_stale_supvan(registry: &PrinterRegistry) {
    let mut guard = registry.write();
    guard.retain(|p| {
        let uri = &p.config.device_uri;
        let keep = uri.starts_with("supvan://") || uri.starts_with("mock://");
        if !keep {
            log::info!("pruning legacy-scheme printer: {uri}");
        }
        keep
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::tests::load_once;

    fn backend(known: &[&str]) -> SupvanDeviceBackend {
        SupvanDeviceBackend {
            known_uris: known.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn candidate(name: &str) -> BtCandidate {
        BtCandidate {
            address: "A4:93:40:5D:71:B6".to_string(),
            name: name.to_string(),
            model_name: None,
        }
    }

    /// The probe is skipped wherever `bt_patterns` already pins the family, so
    /// an unprobed candidate is the normal case — its `MDL:` still has to
    /// route. Naming a family outright collapses them all onto the T-series.
    #[test]
    fn an_unprobed_bt_name_still_routes_to_its_own_family() {
        load_once();
        for (name, expect) in [
            ("T0143F2408183024", "supvan_e"),
            ("E10pro", "supvan_e"),
            ("T80", "supvan_t80"),
            ("G15Mini", "supvan_g"),
            ("TP76", "supvan_tp76"),
            ("T0117A2410211517", "supvan_t50"),
        ] {
            let family = models::family_for_model_hint(name);
            assert_eq!(
                family.driver_name.to_string_lossy(),
                expect,
                "{name} routed to the wrong family"
            );
        }
        // What the old fallback substituted for every one of them.
        assert_eq!(
            models::family_for_model_hint("T50 Series")
                .driver_name
                .to_string_lossy(),
            "supvan_t50"
        );
    }

    /// An RFCOMM socket is exclusive on these printers, so the probe is only
    /// worth spending where it can still change the driver decision.
    #[test]
    fn model_probe_is_skipped_for_names_already_pinned_to_the_t_series() {
        load_once();
        let b = backend(&[]);
        // Hardware codes and marketing names that `bt_patterns` resolves to a
        // T-series family: RD_DEV_NAME cannot change the answer.
        for name in ["T0117A2410211517", "T50M Pro", "T80", "G15Mini", "TP76"] {
            assert!(
                !b.needs_model_probe(&candidate(name)),
                "{name} is already pinned to the T-series flow"
            );
        }
    }

    #[test]
    fn model_probe_still_runs_where_it_can_change_the_answer() {
        load_once();
        let b = backend(&[]);
        // Pinned to the E-series: the marketing name still refines `MDL:`.
        for name in ["T0143F2408183024", "E10pro", "E12"] {
            assert!(
                b.needs_model_probe(&candidate(name)),
                "{name} is an E-series unit"
            );
        }
        // Pins nothing — reached discovery via the generic serial-name
        // fallback, so it could be an E-series unit with an unlisted code.
        for name in ["T0199Z2501010001", "D42unknown", ""] {
            assert!(
                b.needs_model_probe(&candidate(name)),
                "{name:?} pins no family, so its model is still unknown"
            );
        }
    }

    /// `bootstrap_printers` drops a discovered device whose URI it already
    /// holds, so probing one is pure cost: an exclusive RFCOMM socket, and up
    /// to the kernel's connect timeout if the unit has been switched off.
    #[test]
    fn model_probe_is_skipped_for_an_already_configured_printer() {
        load_once();
        // An E-series unit — probed when unknown, skipped once configured.
        let c = candidate("T0143F2408183024");
        assert!(backend(&[]).needs_model_probe(&c));
        assert!(!backend(&[&device_uri_for(&c.name)]).needs_model_probe(&c));
        // ... and the join key really is the emitted URI, slug and all.
        assert_eq!(
            device_uri_for("T0143F2408183024"),
            "supvan://t0143f2408183024"
        );
        assert!(!backend(&["supvan://t0143f2408183024"]).needs_model_probe(&c));
    }

    /// A different printer being configured must not suppress this one's
    /// probe, or the first E-series unit added would blind every later one.
    #[test]
    fn another_printers_uri_does_not_suppress_the_probe() {
        load_once();
        let c = candidate("T0143F2408183024");
        assert!(backend(&["supvan://t0117a2410211517"]).needs_model_probe(&c));
    }
}
