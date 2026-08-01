//! High-level printer operations.
//!
//! Implements the print flow from T50PlusPrint.doPrint():
//! CHECK_DEVICE -> poll ready -> START_PRINT -> poll printing ->
//! transfer buffers -> poll complete.

use crate::cmd::*;
use crate::compress::compress_buffers;
use crate::data::DATA_PAYLOAD_SIZE;
use crate::error::{Error, Result};
use crate::profile::PrintProfile;
use crate::speed::calc_speed;
use crate::status::{MaterialInfo, PrinterStatus};
use crate::transport::Transport;
use std::time::Duration;

/// Status-poll attempt budgets for the print state machine; each is multiplied
/// by the poll interval inside its wait loop.
const READY_ATTEMPTS: usize = 60;
const PRINTING_ATTEMPTS: usize = 60;
const BUFFER_READY_ATTEMPTS: usize = 200;

/// Wait-for-completion budget: COMPLETION_POLLS × COMPLETION_POLL_INTERVAL = 30s.
const COMPLETION_POLL_INTERVAL: Duration = Duration::from_millis(100);
const COMPLETION_POLLS: usize = 300;

/// High-level printer interface over a pluggable transport.
pub struct Printer {
    transport: Box<dyn Transport>,
    /// Wire-protocol variant this device speaks. A property of the printer,
    /// not of any one call, so every step of the print flow reads it from
    /// here rather than taking it as an argument.
    profile: PrintProfile,
}

impl Printer {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            profile: PrintProfile::default(),
        }
    }

    /// Select the wire-protocol variant. Callers that know the model (from
    /// `RD_DEV_NAME` or the driver registry) set it once after opening.
    ///
    /// DEBUG, not INFO: a cached BT socket re-asserts the profile on every
    /// open, so status polling alone logs this twice a minute per printer.
    pub fn set_profile(&mut self, profile: PrintProfile) {
        log::debug!("profile: {profile:?}");
        self.profile = profile;
    }

    /// The wire-protocol variant currently selected.
    pub fn profile(&self) -> PrintProfile {
        self.profile
    }

    /// Open a USB HID printer at the given `/dev/hidrawN` path.
    pub fn open_usb(path: &str) -> Result<Self> {
        let dev = crate::hidraw::HidrawDevice::open(path)?;
        Ok(Self::new(Box::new(
            crate::usb_transport::UsbHidTransport::new(dev),
        )))
    }

    /// Open a Bluetooth printer at the given RFCOMM address (`AA:BB:CC:DD:EE:FF`).
    pub fn open_bt(addr: &str) -> Result<Self> {
        let sock = crate::rfcomm::RfcommSocket::connect_default(addr)?;
        Ok(Self::new(Box::new(crate::spp_pipe::SppCodec::new(sock))))
    }

    /// Open a BLE GATT printer by address (E11/E12-class hardware). Async
    /// because the `bluer` GATT client is natively async. Requires the `ble`
    /// feature.
    #[cfg(feature = "ble")]
    pub async fn open_ble(addr: &str) -> Result<Self> {
        let pipe = crate::ble::BlePipe::connect(addr).await?;
        Ok(Self::new(Box::new(crate::spp_pipe::SppCodec::new(pipe))))
    }

    /// Open a printer from a target string: a `/dev/hidrawN` path selects USB
    /// HID, anything else is treated as a Bluetooth address.
    pub fn open_target(target: &str) -> Result<Self> {
        if target.starts_with("/dev/hidraw") {
            Self::open_usb(target)
        } else {
            Self::open_bt(target)
        }
    }

    /// CHECK_DEVICE (0x12) - verify printer is present.
    pub async fn check_device(&self) -> Result<bool> {
        log::info!("CHECK_DEVICE");
        let resp = self.transport.send_cmd(CMD_CHECK_DEVICE, 0).await?;
        Ok(resp.is_some_and(|r| self.transport.validate_response(&r, CMD_CHECK_DEVICE)))
    }

    /// INQUIRY_STA (0x11) - query printer status.
    pub async fn query_status(&self) -> Result<Option<PrinterStatus>> {
        let resp = self.transport.send_cmd(CMD_INQUIRY_STA, 0).await?;
        Ok(resp.and_then(|r| self.transport.parse_status_response(&r)))
    }

    /// RETURN_MAT (0x30) - query material/label info.
    pub async fn query_material(&self) -> Result<Option<MaterialInfo>> {
        log::info!("RETURN_MAT");
        let resp = self.transport.send_cmd(CMD_RETURN_MAT, 0).await?;
        Ok(resp.and_then(|r| self.transport.parse_material_response(&r)))
    }

    /// RD_DEV_NAME (0x16) - read device name.
    pub async fn read_device_name(&self) -> Result<Option<String>> {
        log::info!("RD_DEV_NAME");
        let resp = self.transport.send_cmd(CMD_RD_DEV_NAME, 0).await?;
        Ok(resp.and_then(|r| self.transport.parse_device_name_response(&r)))
    }

    /// READ_FWVER (0xC5) - read firmware version.
    pub async fn read_firmware_version(&self) -> Result<Option<u8>> {
        log::info!("READ_FWVER");
        let resp = self.transport.send_cmd(CMD_READ_FWVER, 0).await?;
        Ok(resp.and_then(|r| self.transport.parse_firmware_version_response(&r)))
    }

    /// READ_REV (0x17) - read protocol version.
    pub async fn read_version(&self) -> Result<Option<String>> {
        log::info!("READ_REV");
        let resp = self.transport.send_cmd(CMD_READ_REV, 0).await?;
        Ok(resp.and_then(|r| self.transport.parse_version_response(&r)))
    }

    /// START_PRINT (0x13).
    pub async fn start_print(&self) -> Result<Option<Vec<u8>>> {
        log::info!("START_PRINT");
        self.transport.send_cmd(CMD_START_PRINT, 0).await
    }

    /// STOP_PRINT (0x14).
    pub async fn stop_print(&self) -> Result<Option<Vec<u8>>> {
        log::info!("STOP_PRINT");
        self.transport.send_cmd(CMD_STOP_PRINT, 0).await
    }

    /// PAPER_SKIP (0x2E) — feed/advance one blank label. Returns `Ok(())` once
    /// the device acks; errors if there is no response.
    pub async fn paper_skip(&self) -> Result<()> {
        log::info!("PAPER_SKIP");
        let resp = self.transport.send_cmd(CMD_PAPER_SKIP, 0).await?;
        if resp.is_some_and(|r| self.transport.validate_response(&r, CMD_PAPER_SKIP)) {
            Ok(())
        } else {
            Err(Error::InvalidResponse("PAPER_SKIP: no ack".into()))
        }
    }

    /// Wait for device to be idle (not busy, not printing).
    pub async fn wait_ready(&self, max_attempts: usize) -> Result<Option<PrinterStatus>> {
        for _ in 0..max_attempts {
            let st = self.query_status().await?;
            if let Some(ref s) = st
                && !s.device_busy
                && !s.printing
            {
                return Ok(st);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(None)
    }

    /// Wait for printing station to become active.
    ///
    /// Aborts early via `Error::InvalidResponse` if the printer raises an
    /// error flag (label end, cover open, mode mismatch, etc.) — those
    /// states cause the firmware to drop the BT link and beep, and there's
    /// no point continuing the print.
    pub async fn wait_printing(&self, max_attempts: usize) -> Result<Option<PrinterStatus>> {
        for _ in 0..max_attempts {
            let st = self.query_status().await?;
            if let Some(ref s) = st {
                if let Some(why) = s.error_description(self.profile) {
                    return Err(Error::InvalidResponse(format!(
                        "printer error after START_PRINT: {why}"
                    )));
                }
                if s.printing {
                    return Ok(st);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(None)
    }

    /// Wait for buffer space available (buf_full == false).
    pub async fn wait_buffer_ready(&self, max_attempts: usize) -> Result<Option<PrinterStatus>> {
        for i in 0..max_attempts {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let st = self.query_status().await?;
            if let Some(ref s) = st {
                if let Some(why) = s.error_description(self.profile) {
                    return Err(Error::InvalidResponse(format!(
                        "printer error while waiting for buffer: {why}"
                    )));
                }
                if !s.buf_full {
                    return Ok(st);
                }
            }
            if i % 10 == 0 && i > 0 {
                log::debug!("waiting for buffer space... ({i})");
            }
        }
        Ok(None)
    }

    /// Transfer one LZMA stream: NEXT_ZIPPEDBULK -> data packets -> BUF_FULL.
    ///
    /// What a stream covers is the profile's business — see
    /// [`print_page`](Self::print_page). On the T-series flow it is the whole
    /// page, and the printer's decoder splits the decompressed result on
    /// print-buffer boundaries internally; on the E-series it is exactly one
    /// print buffer.
    ///
    /// `compressed_len` is only advisory: each transport encodes
    /// `NEXT_ZIPPEDBULK` in its own convention (SPP framing always carries a
    /// literal 512-byte block size plus the packet count, USB HID carries the
    /// total compressed length), so this is not a per-model parameter.
    pub async fn transfer_compressed(&self, compressed: &[u8], speed: u16) -> Result<()> {
        let compressed_len = compressed.len() as u16;

        // CMD_NEXT_ZIPPEDBULK (0x5C): each transport encodes the header in its
        // own convention (SPP: block_size=512 + packet count; USB: total length).
        let num_packets = compressed.len().div_ceil(DATA_PAYLOAD_SIZE);
        log::info!(
            "transfer: {} bytes, {} packets, speed={}",
            compressed.len(),
            num_packets,
            speed
        );
        let resp = self
            .transport
            .send_bulk_header(compressed_len, num_packets)
            .await?;
        if resp.is_none() {
            return Err(Error::InvalidResponse(
                "no response to NEXT_ZIPPEDBULK".into(),
            ));
        }

        // Send data packets via the transport. We do NOT read a response
        // after the last frame: the protocol acks the bulk only via the
        // BUF_FULL reply that follows. Polling for a non-existent response
        // here blocks for the read timeout (2s on BT), during which the
        // printer queues the bytes, times out waiting for BUF_FULL, errors
        // (3-beep) and drops the RFCOMM link before BUF_FULL arrives.
        self.transport.send_bulk_data(compressed, false).await?;

        // 20ms delay after last data packet
        tokio::time::sleep(Duration::from_millis(20)).await;

        // CMD_BUF_FULL: the T-series reports (length, speed); the E-series sends zeroes.
        let (p1, p2) = if self.profile.params().buf_full_reports_length {
            (compressed_len, speed)
        } else {
            (0, 0)
        };
        log::info!("BUF_FULL: param={p1}, param2={p2}");
        self.transport.send_cmd_two(CMD_BUF_FULL, p1, p2).await?;

        Ok(())
    }

    /// Execute a full print job from built print buffers.
    ///
    /// This is the main print flow from T50PlusPrint.doPrint():
    /// 0. Compress
    /// 1. CHECK_DEVICE
    /// 2. Wait ready
    /// 3. START_PRINT (bracketed by the profile's vendor commands)
    /// 4. Wait printing station
    /// 5. Wait buffer ready + transfer
    /// 6. Wait completion
    ///
    /// Compression happens here rather than in the caller because the split is
    /// profile-dependent: the T-series concatenates the whole page into one LZMA
    /// stream, the E-series ships one stream per buffer. It is done up front,
    /// before `CHECK_DEVICE`, so that no LZMA pass ever runs between
    /// `START_PRINT` and the first data packet — the printer is live from that
    /// point on and times out waiting for data.
    ///
    /// Every buffer must be exactly the profile's `buf_size`: the firmware
    /// splits the decompressed stream on that fixed stride and reads a
    /// 14-byte header at each boundary, so a wrongly-sized buffer compresses
    /// and transfers fine but decodes as garbage. [`split_into_buffers`](crate::buffer::split_into_buffers) always
    /// emits the right size; this rejects anything else rather than printing it.
    pub async fn print_page(&self, buffers: &[Vec<u8>]) -> Result<()> {
        if buffers.is_empty() {
            return Err(Error::InvalidParam("no print buffers".into()));
        }
        let buf_size = self.profile.params().buf_size;
        if let Some((i, bad)) = buffers
            .iter()
            .enumerate()
            .find(|(_, b)| b.len() != buf_size)
        {
            return Err(Error::InvalidParam(format!(
                "print buffer {i} is {} bytes, but {:?} requires exactly {buf_size}",
                bad.len(),
                self.profile
            )));
        }

        // Step 0: Compress every chunk before the printer is started.
        let per_chunk = if self.profile.params().per_buffer_transfer {
            1
        } else {
            buffers.len()
        };
        let mut streams = Vec::new();
        for chunk in buffers.chunks(per_chunk) {
            let (compressed, avg) = compress_buffers(chunk)?;
            streams.push((compressed, calc_speed(avg)));
        }
        log::info!(
            "print_page: {} buffers -> {} LZMA stream(s)",
            buffers.len(),
            streams.len()
        );

        // Step 1: Check device
        if !self.check_device().await? {
            return Err(Error::InvalidResponse("CHECK_DEVICE failed".into()));
        }

        // Step 2: Wait ready
        let status = self
            .wait_ready(READY_ATTEMPTS)
            .await?
            .ok_or_else(|| Error::InvalidResponse("timeout waiting for device ready".into()))?;
        if let Some(why) = status.error_description(self.profile) {
            return Err(Error::InvalidResponse(format!("printer error: {why}")));
        }

        // Step 3: Start print, bracketed by the profile's vendor commands.
        if let Some((cmd, param)) = self.profile.params().pre_start_cmd {
            log::info!("pre-start 0x{cmd:02X}({param})");
            self.transport.send_cmd(cmd, param).await?;
        }
        self.start_print().await?;

        // Step 4: Wait printing station
        self.wait_printing(PRINTING_ATTEMPTS)
            .await?
            .ok_or_else(|| Error::InvalidResponse("timeout waiting for printing station".into()))?;

        if let Some((cmd, param)) = self.profile.params().post_start_cmd {
            log::info!("post-start 0x{cmd:02X}({param})");
            self.transport.send_cmd(cmd, param).await?;
        }

        // Step 5: Transfer, one LZMA stream per chunk, each with its own
        // BUF_FULL.
        for (compressed, speed) in &streams {
            let buf_status = self
                .wait_buffer_ready(BUFFER_READY_ATTEMPTS)
                .await?
                .ok_or_else(|| Error::InvalidResponse("timeout waiting for buffer space".into()))?;
            if let Some(why) = buf_status.error_description(self.profile) {
                self.stop_print().await?;
                return Err(Error::InvalidResponse(format!("printer error: {why}")));
            }
            self.transfer_compressed(compressed, *speed).await?;
        }

        // Step 6: Wait completion
        for _ in 0..COMPLETION_POLLS {
            tokio::time::sleep(COMPLETION_POLL_INTERVAL).await;
            if let Some(s) = self.query_status().await?
                && !s.printing
                && !s.device_busy
            {
                log::info!("print complete");
                return Ok(());
            }
        }

        log::warn!("timeout waiting for print completion");
        Err(Error::Timeout("print completion"))
    }

    /// Full test print workflow: generate test pattern, build buffers, print.
    ///
    /// Uses [`ProfileParams::default_printhead_dots`](crate::profile::ProfileParams::default_printhead_dots)
    /// for the canvas, which is right for the reference model of each profile.
    /// `supvan-printer-app` does not use this path — it takes the head width
    /// from the driver registry, which knows the difference between a T50 and
    /// a TP80.
    pub async fn test_print(&self, mat: &MaterialInfo, density: u8) -> Result<()> {
        use crate::bitmap::create_test_pattern;
        use crate::buffer::split_into_buffers;

        let printhead_dots = self.profile.params().default_printhead_dots;
        // Cap at the model's own head, not the 48 mm T50 assumption.
        let label_width_mm = (mat.width_mm as u32).min(printhead_dots / crate::bitmap::DOTS_PER_MM);
        let height_mm = if mat.height_mm == 0 {
            crate::status::DEFAULT_LABEL_HEIGHT_MM as u32
        } else {
            mat.height_mm as u32
        };

        log::info!(
            "test print: {}mm x {}mm, density={}",
            label_width_mm,
            height_mm,
            density
        );

        let (image_data, _w, h, bpl) =
            create_test_pattern(label_width_mm, height_mm, printhead_dots, self.profile);
        let margin = self.profile.params().margin_dots;
        let buffers = split_into_buffers(
            &image_data,
            bpl as u8,
            h as u16,
            margin,
            margin,
            density,
            self.profile,
        );
        log::info!("{} print buffers", buffers.len());

        self.print_page(&buffers).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{max_cols_per_buffer, split_into_buffers};

    /// The guard rejects before any I/O, so every method may panic.
    struct NeverCalled;

    #[async_trait::async_trait]
    impl Transport for NeverCalled {
        async fn send_cmd(&self, _: u8, _: u16) -> Result<Option<Vec<u8>>> {
            unreachable!("print_page must reject bad buffers before talking to the printer")
        }
        async fn send_cmd_two(&self, _: u8, _: u16, _: u16) -> Result<Option<Vec<u8>>> {
            unreachable!()
        }
        async fn send_bulk_header(&self, _: u16, _: usize) -> Result<Option<Vec<u8>>> {
            unreachable!()
        }
        async fn send_bulk_data(&self, _: &[u8], _: bool) -> Result<Option<Vec<u8>>> {
            unreachable!()
        }
        fn parse_status_response(&self, _: &[u8]) -> Option<PrinterStatus> {
            None
        }
        fn parse_material_response(&self, _: &[u8]) -> Option<MaterialInfo> {
            None
        }
        fn validate_response(&self, _: &[u8], _: u8) -> bool {
            false
        }
        fn parse_device_name_response(&self, _: &[u8]) -> Option<String> {
            None
        }
        fn parse_firmware_version_response(&self, _: &[u8]) -> Option<u8> {
            None
        }
        fn parse_version_response(&self, _: &[u8]) -> Option<String> {
            None
        }
    }

    fn printer(profile: PrintProfile) -> Printer {
        let mut p = Printer::new(Box::new(NeverCalled));
        p.set_profile(profile);
        p
    }

    /// `supvan-proto` does not pull in tokio's `macros` feature, so tests
    /// drive the futures on a current-thread runtime themselves.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(fut)
    }

    #[test]
    fn print_page_rejects_buffers_sized_for_another_profile() {
        // A T-series page handed to an E-series printer: compression would
        // succeed, but the firmware splits the stream every 4000 bytes.
        let t_buffers = vec![vec![0u8; PrintProfile::TSeries.params().buf_size]];
        let err = block_on(printer(PrintProfile::ESeries).print_page(&t_buffers)).unwrap_err();
        assert!(
            matches!(&err, Error::InvalidParam(m) if m.contains("4096") && m.contains("4000")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn print_page_rejects_empty_buffer_list() {
        assert!(matches!(
            block_on(printer(PrintProfile::TSeries).print_page(&[])),
            Err(Error::InvalidParam(_))
        ));
    }

    #[test]
    fn split_into_buffers_output_passes_the_guard() {
        // The guard must not reject the one producer the driver actually uses,
        // including the short final buffer of a page.
        for profile in [PrintProfile::TSeries, PrintProfile::ESeries] {
            let per_line = 12u8;
            let margin = profile.params().margin_dots;
            let cols = max_cols_per_buffer(per_line, profile) + margin * 2 + 7;
            let image = vec![0u8; cols as usize * per_line as usize];
            let buffers = split_into_buffers(&image, per_line, cols, margin, margin, 4, profile);
            assert!(buffers.len() > 1, "{profile:?} should need several buffers");
            let buf_size = profile.params().buf_size;
            assert!(buffers.iter().all(|b| b.len() == buf_size));
        }
    }
}
