use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use supvan_proto::error::{Error as ProtoError, Result as ProtoResult};
use supvan_proto::printer::Printer;
use supvan_proto::profile::PrintProfile;
use supvan_proto::status::PrinterStatus;
use tokio::sync::Mutex;

use crate::util::is_mock_mode;

/// A Printer that may be owned by this `KsDevice` (USB, fresh BT) or shared
/// behind a `Mutex` so multiple short-lived `KsDevice` instances can reuse a
/// single open RFCOMM socket without reconnecting (which makes the BT
/// firmware beep).
pub enum PrinterHandle {
    /// `KsDevice` is the only owner; transport closes when dropped.
    Owned(Printer),
    /// Shared with [`crate::device`]'s cache. Drop is a no-op for the
    /// connection.
    Shared(Arc<Mutex<Printer>>),
}

impl PrinterHandle {
    /// Query INQUIRY_STA, returning a parsed [`PrinterStatus`].
    pub async fn query_status(&self) -> ProtoResult<Option<PrinterStatus>> {
        match self {
            Self::Owned(p) => p.query_status().await,
            Self::Shared(arc) => arc.lock().await.query_status().await,
        }
    }

    /// Query RETURN_MAT (loaded label + RFID + remaining count).
    pub async fn query_material(&self) -> ProtoResult<Option<supvan_proto::status::MaterialInfo>> {
        match self {
            Self::Owned(p) => p.query_material().await,
            Self::Shared(arc) => arc.lock().await.query_material().await,
        }
    }

    /// Run a page through the device's print flow. Compression and the
    /// per-buffer split are the profile's business, so the buffers go across
    /// uncompressed — see [`Printer::print_page`].
    pub async fn print_page(&self, buffers: &[Vec<u8>]) -> ProtoResult<()> {
        match self {
            Self::Owned(p) => p.print_page(buffers).await,
            Self::Shared(arc) => arc.lock().await.print_page(buffers).await,
        }
    }

    /// CHECK_DEVICE — poke the device to confirm presence.
    pub async fn check_device(&self) -> ProtoResult<bool> {
        match self {
            Self::Owned(p) => p.check_device().await,
            Self::Shared(arc) => arc.lock().await.check_device().await,
        }
    }
}

/// Opaque device handle wrapping a connected Printer (or None in mock mode).
pub struct KsDevice {
    pub printer: Option<PrinterHandle>,
    /// Guards status queries during active raster transfer.
    pub printing: AtomicBool,
    /// Wire-protocol variant this printer speaks, resolved from its driver
    /// family when the device was opened. Status decoding and page layout
    /// both depend on it, so it is carried here rather than re-derived.
    profile: PrintProfile,
}

impl KsDevice {
    /// Wrap a printer that lives in the BT cache.
    pub fn from_shared(printer: Arc<Mutex<Printer>>, profile: PrintProfile) -> Self {
        KsDevice {
            printer: Some(PrinterHandle::Shared(printer)),
            printing: AtomicBool::new(false),
            profile,
        }
    }

    /// Construct a mock device — no transport, status driven by [`crate::mock`].
    pub fn open_mock() -> Self {
        log::info!("KsDevice::open_mock: synthetic mock device");
        KsDevice {
            printer: None,
            printing: AtomicBool::new(false),
            profile: PrintProfile::default(),
        }
    }

    /// The wire-protocol variant this device speaks.
    pub fn profile(&self) -> PrintProfile {
        self.profile
    }

    /// Open a USB HID connection to the printer at `hidraw_path` (e.g. "/dev/hidraw7").
    pub fn open_usb(hidraw_path: &str, profile: PrintProfile) -> Option<Box<Self>> {
        if is_mock_mode() {
            log::info!("KsDevice::open_usb: MOCK mode — skipping USB open to {hidraw_path}");
            return Some(Box::new(KsDevice {
                printer: None,
                printing: AtomicBool::new(false),
                profile,
            }));
        }

        log::info!("KsDevice::open_usb: opening {hidraw_path}");
        let mut printer = match Printer::open_usb(hidraw_path) {
            Ok(p) => p,
            Err(e) => {
                log::error!("KsDevice::open_usb: hidraw open failed: {e}");
                return None;
            }
        };
        printer.set_profile(profile);

        log::debug!("KsDevice::open_usb: opened {hidraw_path}");
        Some(Box::new(KsDevice {
            printer: Some(PrinterHandle::Owned(printer)),
            printing: AtomicBool::new(false),
            profile,
        }))
    }

    /// Query printer status and return typed reason flags.
    ///
    /// Returns `PrinterReason::empty()` while the printing flag is set
    /// (so we don't query a printer mid-transfer).
    pub async fn status(&self) -> ipp_printer_app::PrinterReason {
        use ipp_printer_app::PrinterReason;

        let printer = match &self.printer {
            Some(p) => p,
            None => return crate::mock::controller().current_reasons(),
        };

        if self.printing.load(Ordering::Acquire) {
            return PrinterReason::empty();
        }

        let status = match printer.query_status().await {
            Ok(Some(s)) => s,
            Ok(None) => return PrinterReason::OTHER,
            Err(ProtoError::Io(_)) => {
                // Socket likely dropped under us; let the next open_bt
                // detect it and reconnect rather than reporting OFFLINE
                // on a transient blip.
                log::warn!("KsDevice::status: transport I/O error; will refresh on next open");
                return PrinterReason::empty();
            }
            Err(e) => {
                log::warn!("KsDevice::status: query failed: {e}");
                return PrinterReason::OTHER;
            }
        };

        crate::job::reasons_from_status(&status, self.profile)
    }

    /// Check if this is a mock device (no real printer connection).
    pub fn is_mock(&self) -> bool {
        self.printer.is_none()
    }

    /// Query loaded material / RFID tag info. Returns `None` if mock, mid-print,
    /// or the transport errored.
    pub async fn material(&self) -> Option<supvan_proto::status::MaterialInfo> {
        let printer = self.printer.as_ref()?;
        if self.printing.load(Ordering::Acquire) {
            return None;
        }
        match printer.query_material().await {
            Ok(m) => m,
            Err(e) => {
                log::debug!("KsDevice::material: query failed: {e}");
                None
            }
        }
    }

    /// Identify-Printer: poke the device so it makes itself known. We send
    /// CHECK_DEVICE — the only presence primitive in the protocol; on Supvan
    /// hardware exercising the link makes the unit chirp. No-op on mock.
    pub async fn identify(&self) {
        let Some(printer) = self.printer.as_ref() else {
            return;
        };
        match printer.check_device().await {
            Ok(present) => log::info!("KsDevice::identify: check_device -> present={present}"),
            Err(e) => log::warn!("KsDevice::identify: check_device failed: {e}"),
        }
    }
}

impl Drop for KsDevice {
    fn drop(&mut self) {
        // Owned connections close their socket here; shared cache entries
        // persist for the next caller, so we don't even log on drop.
        if let Some(PrinterHandle::Owned(_)) = &self.printer {
            log::debug!("KsDevice: closing owned connection");
        }
    }
}
