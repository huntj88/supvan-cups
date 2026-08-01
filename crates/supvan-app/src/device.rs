//! Device open helpers for `supvan://`, `mock://`, and legacy
//! `btrfcomm://` / `usbhid://` URIs.
//!
//! BT connections are cached per address: opening the same address twice
//! reuses the existing RFCOMM socket instead of redialing the printer, which
//! would beep on every connect. Each call validates the cached socket with
//! a CHECK_DEVICE round-trip; if that fails the entry is evicted and a fresh
//! socket is dialed (which beeps once, as expected for "printer came back").
//!
//! `supvan://<name>` is the unified scheme — discovery cross-correlates USB
//! and BT candidates by the printer's self-reported name and registers a
//! per-name transport mapping via [`register_supvan`]. At open time
//! [`open_supvan`] resolves the name to USB (preferred when present) or BT.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use supvan_proto::printer::Printer;
use tokio::sync::Mutex as AsyncMutex;

use crate::battery_provider;
use crate::printer_device::KsDevice;
use supvan_proto::profile::PrintProfile;

/// BT printer connection cache, keyed by address. Persists across `open_bt`
/// calls so the status poller and print jobs reuse one RFCOMM socket per
/// printer. The outer `Mutex` guards the map (held briefly, sync); each printer
/// sits behind an async `Mutex` so a device op can be awaited while held.
fn bt_cache() -> &'static Mutex<HashMap<String, Arc<AsyncMutex<Printer>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<Printer>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How long discovery will wait on a single printer's model-name probe.
/// Comfortably above a healthy connect + `RD_DEV_NAME` round trip, well below
/// the kernel's RFCOMM connect timeout for an absent device.
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);

fn dial_bt(addr: &str) -> Option<Printer> {
    log::info!("device::open_bt: dialing {addr} (no cache entry)");
    match Printer::open_bt(addr) {
        Ok(p) => Some(p),
        Err(e) => {
            log::error!("device::open_bt: RFCOMM connect failed for {addr}: {e}");
            None
        }
    }
}

/// Open `btrfcomm://host/path/AA:BB:CC:DD:EE:FF`, reusing a cached RFCOMM
/// socket when one is available. Drops the cache entry and reconnects if the
/// existing socket no longer responds.
pub async fn open_bt(uri: &str, profile: PrintProfile) -> Option<Box<KsDevice>> {
    let addr = uri
        .strip_prefix("btrfcomm://")
        .and_then(|rest| rest.find('/').map(|pos| &rest[pos + 1..]))?;

    // `await` can't appear in a match guard, so validate the cached socket
    // before deciding whether to reuse it.
    let cached = bt_cache().lock().unwrap().get(addr).cloned();
    let printer = match cached {
        Some(arc) => {
            if arc.lock().await.check_device().await.unwrap_or(false) {
                log::debug!("device::open_bt: reusing cached socket for {addr}");
                arc
            } else {
                log::info!("device::open_bt: cached socket for {addr} is dead, reconnecting");
                bt_cache().lock().unwrap().remove(addr);
                dial_and_cache(addr).await?
            }
        }
        None => dial_and_cache(addr).await?,
    };

    if let Some(h) = battery_provider::handle() {
        h.add_device(addr, 100);
    }
    // The socket is cached across jobs, so re-assert the profile: this printer
    // may have been dialled by a bare `RD_DEV_NAME` probe that knew no model.
    printer.lock().await.set_profile(profile);
    Some(Box::new(KsDevice::from_shared(printer, profile)))
}

/// Dial a fresh RFCOMM socket for `addr` and insert it into the connection
/// cache, returning the shared handle.
///
/// `libc::connect` on an RFCOMM socket has no timeout of its own: for a
/// powered-off printer it blocks until the kernel's page timeout, tens of
/// seconds later. It runs on the blocking pool so a caller that gives up
/// first — see [`probe_bt_model_name`] — still releases its task on time.
///
/// The blocking call is *not* cancelled, so N unreachable printers tie up N
/// pool threads until the kernel returns (nothing is cached or leaked). That
/// is bounded — 512 threads by default, and only BlueZ-advertised candidates
/// are probed — so it is accepted; fixing it properly means a non-blocking
/// connect plus `poll` in [`supvan_proto::rfcomm`].
async fn dial_and_cache(addr: &str) -> Option<Arc<AsyncMutex<Printer>>> {
    let owned = addr.to_string();
    let printer = tokio::task::spawn_blocking(move || dial_bt(&owned))
        .await
        .ok()
        .flatten()?;
    let arced = Arc::new(AsyncMutex::new(printer));
    bt_cache()
        .lock()
        .unwrap()
        .insert(addr.to_string(), arced.clone());
    Some(arced)
}

/// Read the firmware's own model name over BT (`RD_DEV_NAME`, e.g. `E10pro`).
///
/// BlueZ only exposes the firmware *serial* name (`T0143F2408183024`), the
/// cross-transport join key, which says nothing about the model. The marketing
/// name is only reachable on the wire and becomes the `MDL:` field that picks
/// the driver family — without it every E-series unit lands on the T50 driver
/// and prints nothing.
///
/// Reuses the cached RFCOMM socket, dialling and caching on first sight. An
/// RFCOMM socket is exclusive on most of these printers, so holding one locks
/// the vendor app out: probe only where the model changes the outcome, per
/// [`SupvanDeviceBackend::needs_model_probe`](crate::ipp_server::SupvanDeviceBackend).
///
/// Bounded by [`PROBE_TIMEOUT`]. Giving up costs only the model name, but the
/// underlying connect keeps running on the blocking pool — see
/// [`dial_and_cache`]. BT-only: the 8-byte USB HID status frame can't carry a
/// string (see [`crate::usb_discover`]).
pub async fn probe_bt_model_name(addr: &str) -> Option<String> {
    match tokio::time::timeout(PROBE_TIMEOUT, probe_bt_model_name_inner(addr)).await {
        Ok(name) => name,
        Err(_) => {
            log::warn!(
                "device::probe_bt_model_name: {addr}: gave up after {PROBE_TIMEOUT:?}; \
                 the driver family will fall back to its default"
            );
            None
        }
    }
}

async fn probe_bt_model_name_inner(addr: &str) -> Option<String> {
    let cached = bt_cache().lock().unwrap().get(addr).cloned();
    let printer = match cached {
        Some(arc) => arc,
        None => dial_and_cache(addr).await?,
    };
    let name = match printer.lock().await.read_device_name().await {
        Ok(n) => n,
        Err(e) => {
            log::warn!("device::probe_bt_model_name: {addr}: RD_DEV_NAME failed: {e}");
            return None;
        }
    };
    let name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    log::info!("device::probe_bt_model_name: {addr} -> {name:?}");
    name
}

/// Open a device from its URI, dispatching on the scheme: `supvan://` resolves
/// through the discovery transport map, `mock://` yields a simulator device.
/// Any other scheme is unsupported and returns `None`.
pub async fn open_uri(uri: &str, profile: PrintProfile) -> Option<KsDevice> {
    if uri.starts_with("supvan://") {
        open_supvan(uri, profile).await
    } else if uri.starts_with("mock://") {
        open_mock(uri)
    } else {
        None
    }
}

/// Open `mock://ID`. Always succeeds with a no-connection KsDevice driven by
/// the [`crate::mock`] controller. Only registered when `SUPVAN_MOCK=1`.
pub fn open_mock(_uri: &str) -> Option<KsDevice> {
    // Simulate powered-off / unplugged hardware: the device can't be opened,
    // so poll_status reports OFFLINE and the print path holds the job.
    if crate::mock::controller().is_unreachable() {
        log::info!("mock: device unreachable (SUPVAN_MOCK_UNREACHABLE)");
        return None;
    }
    Some(KsDevice::open_mock())
}

/// Transport map for `supvan://NAME` URIs, populated by discovery and
/// consulted by [`open_supvan`] / [`poll_status`].
#[derive(Clone, Default)]
struct SupvanTransports {
    hidraw_path: Option<String>,
    bt_address: Option<String>,
    ble_address: Option<String>,
}

fn supvan_map() -> &'static Mutex<HashMap<String, SupvanTransports>> {
    static MAP: OnceLock<Mutex<HashMap<String, SupvanTransports>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record the active USB and/or BT transports for a `supvan://<slug>` printer.
/// Called from [`SupvanDeviceBackend::list`](crate::ipp_server::SupvanDeviceBackend) after each
/// discovery cycle.
pub fn register_supvan(
    slug: &str,
    hidraw_path: Option<String>,
    bt_address: Option<String>,
    ble_address: Option<String>,
) {
    supvan_map().lock().unwrap().insert(
        slug.to_string(),
        SupvanTransports {
            hidraw_path,
            bt_address,
            ble_address,
        },
    );
}

/// Open `supvan://<slug>`. Prefers USB when available, falls back to the
/// cached BT socket. Returns `None` if neither transport is registered or
/// both fail to open.
pub async fn open_supvan(uri: &str, profile: PrintProfile) -> Option<KsDevice> {
    let slug = uri.strip_prefix("supvan://")?;
    let entry = supvan_map().lock().unwrap().get(slug).cloned()?;

    if let Some(path) = entry.hidraw_path.as_deref() {
        if let Some(dev) = KsDevice::open_usb(path, profile) {
            return Some(*dev);
        }
        log::warn!("open_supvan: USB open failed for {slug} ({path}), falling back to BT");
    }
    if let Some(addr) = entry.bt_address.as_deref() {
        // open_bt expects a full URI; synthesize one.
        let uri = format!("btrfcomm://bt/{addr}");
        if let Some(dev) = open_bt(&uri, profile).await {
            return Some(*dev);
        }
        log::warn!("open_supvan: BT open failed for {slug} ({addr}), trying BLE");
    }
    if let Some(addr) = entry.ble_address.as_deref() {
        return open_ble_addr(addr, profile).await.map(|b| *b);
    }
    log::warn!("open_supvan: no transports for {slug}");
    None
}

/// Open a BLE printer by address, reusing a cached GATT connection. Stub
/// (returns `None`) without the `ble` feature.
#[cfg(feature = "ble")]
async fn open_ble_addr(addr: &str, profile: PrintProfile) -> Option<Box<KsDevice>> {
    let cached = ble_cache().lock().unwrap().get(addr).cloned();
    let printer = match cached {
        Some(arc) => {
            if arc.lock().await.check_device().await.unwrap_or(false) {
                log::debug!("device::open_ble: reusing cached connection for {addr}");
                arc
            } else {
                log::info!("device::open_ble: cached connection for {addr} dead, reconnecting");
                ble_cache().lock().unwrap().remove(addr);
                dial_ble_and_cache(addr).await?
            }
        }
        None => dial_ble_and_cache(addr).await?,
    };
    printer.lock().await.set_profile(profile);
    Some(Box::new(KsDevice::from_shared(printer, profile)))
}

#[cfg(not(feature = "ble"))]
async fn open_ble_addr(addr: &str, _profile: PrintProfile) -> Option<Box<KsDevice>> {
    log::warn!("device: BLE address {addr} registered but the `ble` feature is off");
    None
}

/// BLE printer connection cache, mirroring [`bt_cache`].
#[cfg(feature = "ble")]
fn ble_cache() -> &'static Mutex<HashMap<String, Arc<AsyncMutex<Printer>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<Printer>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(feature = "ble")]
async fn dial_ble_and_cache(addr: &str) -> Option<Arc<AsyncMutex<Printer>>> {
    log::info!("device::open_ble: connecting {addr} (no cache entry)");
    let printer = match Printer::open_ble(addr).await {
        Ok(p) => p,
        Err(e) => {
            log::error!("device::open_ble: GATT connect failed for {addr}: {e}");
            return None;
        }
    };
    let arced = Arc::new(AsyncMutex::new(printer));
    ble_cache()
        .lock()
        .unwrap()
        .insert(addr.to_string(), arced.clone());
    Some(arced)
}
