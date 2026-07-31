use std::sync::atomic::Ordering;
use std::time::Instant;

use ipp_printer_app::{JobFailure, JobOptions, PrinterHandle, PrinterReason, RasterDriver};
use supvan_proto::bitmap::{center_in_printhead, raster_to_column_major};
use supvan_proto::buffer::split_into_buffers;
use supvan_proto::error::Error as ProtoError;
use supvan_proto::profile::PrintProfile;
use supvan_proto::status::PrinterStatus;

use crate::dither::{dither_line, to_gray_line};
use crate::dump::{JobDump, JobManifest, PgmAccumulator, dumps_enabled};
use crate::mock;
use crate::printer_device::KsDevice;

/// Poll cadence and budget while waiting for print completion
/// (COMPLETION_POLLS × COMPLETION_POLL_INTERVAL = 30s).
const COMPLETION_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
const COMPLETION_POLLS: u32 = 300;

/// Minimal RFC-3339-ish timestamp without pulling chrono.
fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

/// Map raw printer status flags to IPP `printer-state-reasons`.
///
/// Single source of truth, shared by the terminal job-failure path
/// ([`failure_from_status`]), live status polling ([`KsDevice::status`]), and
/// the mock simulator. Returns the raw reason bits with no fallback — callers
/// decide how an empty set is treated (live polling leaves it empty = nothing
/// wrong).
pub(crate) fn reasons_from_status(s: &PrinterStatus, profile: PrintProfile) -> PrinterReason {
    let mut reasons = PrinterReason::empty();
    if s.cover_open {
        reasons |= PrinterReason::COVER_OPEN;
    }
    if s.label_end || s.label_not_installed {
        reasons |= PrinterReason::MEDIA_EMPTY;
    }
    if s.label_rw_error || s.label_mode_error || s.ribbon_rw_error {
        reasons |= PrinterReason::MEDIA_JAM;
    }
    // The E10pro asserts ribbon_end during prints the vendor app completes,
    // so surfacing it there would hold every job forever.
    if s.ribbon_end && profile.params().ribbon_end_is_fatal {
        reasons |= PrinterReason::MEDIA_NEEDED;
    }
    if s.head_temp_high {
        reasons |= PrinterReason::OTHER;
    }
    reasons
}

/// Build a terminal [`JobFailure`] from a status, or `None` when `profile`
/// sees nothing wrong with it.
///
/// The description covers only the bits this profile would actually abort on:
/// an E-series job that fails for an unrelated reason must not blame the
/// permanently-asserted `ribbon_end` bit the same profile deliberately
/// ignores. `reasons_from_status` gates on the same bit, so an empty reason
/// set and an absent description always agree.
pub fn failure_from_status(
    s: &PrinterStatus,
    context: &str,
    profile: PrintProfile,
) -> Option<JobFailure> {
    let desc = s.error_description(profile)?;
    Some(JobFailure::new(
        reasons_from_status(s, profile),
        format!("{context}: {desc}"),
    ))
}

fn failure_from_proto(e: ProtoError, context: &str) -> JobFailure {
    let reasons = match &e {
        ProtoError::Io(_) => PrinterReason::OFFLINE,
        _ => PrinterReason::OTHER,
    };
    JobFailure::new(reasons, format!("{context}: {e}"))
}

pub struct KsJob {
    pub width: u32,
    pub height: u32,
    pub bytes_per_line: u32,
    pub raster_data: Vec<u8>,
    pub lines_received: u32,
    pub density: u8,
    pub printhead_width_dots: u32,
    /// Wire-protocol variant for the target model.
    pub profile: PrintProfile,
    pub pgm_acc: Option<PgmAccumulator>,
}

impl KsJob {
    /// Allocate the page buffer for an incoming raster.
    ///
    /// The buffer is always tightly packed 1 bpp at `ceil(w/8)`, whatever
    /// `options` says: contone lines are dithered before `append_line`, and
    /// [`raster_to_column_major`] re-reads the buffer at that same hard-coded
    /// stride. Sizing from `options.bytes_per_line` instead copies raw RGB
    /// into the bitmap (white `0xFF` becomes eight set dots — a black label),
    /// or, for a padded 1 bpp source, shears every row. `append_line`
    /// truncates the padding away as each line arrives.
    pub fn start(
        _dev: &KsDevice,
        options: &JobOptions,
        density: u8,
        printhead_width_dots: u32,
        profile: PrintProfile,
    ) -> Result<Self, JobFailure> {
        let (w, h, bpp) = (options.width, options.height, options.bits_per_pixel);
        let bpl = w.div_ceil(8);
        if bpp == 1 && options.bytes_per_line > bpl {
            log::debug!(
                "KsJob::start: source rows are padded to {} bytes, packing to {bpl}",
                options.bytes_per_line
            );
        }
        log::info!(
            "KsJob::start: {w}x{h}, {bpp}bpp in, bpl={bpl}, \
             density={density}, printhead={printhead_width_dots}, profile={profile:?}"
        );
        Ok(KsJob {
            width: w,
            height: h,
            bytes_per_line: bpl,
            raster_data: vec![0u8; (h * bpl) as usize],
            lines_received: 0,
            density,
            profile,
            printhead_width_dots,
            pgm_acc: None,
        })
    }

    pub fn append_line(&mut self, y: u32, line: &[u8]) -> bool {
        if y >= self.height {
            return false;
        }
        let copy_len = line.len().min(self.bytes_per_line as usize);
        let offset = (y * self.bytes_per_line) as usize;
        self.raster_data[offset..offset + copy_len].copy_from_slice(&line[..copy_len]);
        self.lines_received += 1;
        true
    }

    pub async fn transfer_page(&mut self, dev: &KsDevice) -> Result<(), JobFailure> {
        let is_mock = dev.is_mock();
        let started = Instant::now();
        log::info!(
            "KsJob::transfer_page: {}x{}, {} lines, mock={}",
            self.width,
            self.height,
            self.lines_received,
            is_mock,
        );

        // Allocate one dump seq per page so all per-page artefacts share NNNN.
        let dump = JobDump::allocate();

        if let Some(acc) = self.pgm_acc.take() {
            dump.pgm(&acc);
        }
        dump.pbm(
            &self.raster_data,
            self.width,
            self.height,
            self.bytes_per_line,
        );

        let (col_data, num_cols, _) =
            raster_to_column_major(&self.raster_data, self.width, self.height);
        let (canvas, canvas_bpl) =
            center_in_printhead(&col_data, num_cols, self.width, self.printhead_width_dots);
        dump.printhead_pbm(&canvas, num_cols, canvas_bpl, self.printhead_width_dots);

        let margin = self.profile.params().margin_dots;
        let buffers = split_into_buffers(
            &canvas,
            canvas_bpl as u8,
            num_cols as u16,
            margin,
            margin,
            self.density,
            self.profile,
        );

        let outcome: Result<(), JobFailure> = if let Some(ref printer) = dev.printer {
            dev.printing.store(true, Ordering::Release);
            let result = printer.print_page(&buffers).await;
            dev.printing.store(false, Ordering::Release);
            match result {
                Ok(()) => Ok(()),
                Err(ProtoError::InvalidResponse(msg)) => Err(match printer.query_status().await {
                    Ok(Some(s)) => failure_from_status(&s, "print_page", self.profile)
                        .unwrap_or_else(|| JobFailure::other(msg)),
                    _ => JobFailure::other(msg),
                }),
                Err(e) => Err(failure_from_proto(e, "print_page")),
            }
        } else {
            // Mock device: simulate the print delay, then check the simulator
            // for a queued failure. Dumps already happened above so the operator
            // can still inspect the output even on a simulated abort.
            tokio::time::sleep(mock::controller().delay()).await;
            match mock::controller().take_print_failure() {
                Some(f) => Err(f),
                None => {
                    log::info!("KsJob::transfer_page: mock — dumped, no transfer");
                    Ok(())
                }
            }
        };

        // Manifest reflects what really happened (real or simulated).
        let (sim_outcome, _len) = match &outcome {
            Ok(()) => ("completed".to_string(), 0usize),
            Err(f) => (format!("aborted: {}", f.message), 0),
        };
        dump.manifest(&JobManifest {
            timestamp: now_iso(),
            width: self.width,
            height: self.height,
            bytes_per_line: self.bytes_per_line,
            density: self.density,
            printhead_width_dots: self.printhead_width_dots,
            copies: 1,
            mock: is_mock,
            simulated_outcome: sim_outcome,
            elapsed_ms: started.elapsed().as_millis(),
        });

        outcome
    }

    pub fn clear_page(&mut self) {
        self.raster_data.fill(0);
        self.lines_received = 0;
    }

    pub async fn end(self, dev: &KsDevice) {
        if let Some(ref printer) = dev.printer {
            let mut settled = false;
            for i in 0..COMPLETION_POLLS {
                tokio::time::sleep(COMPLETION_POLL_INTERVAL).await;
                match printer.query_status().await {
                    Ok(Some(s)) if !s.printing && !s.device_busy => {
                        log::info!("KsJob::end: complete after {i} polls");
                        settled = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("KsJob::end: status error: {e}");
                        settled = true;
                        break;
                    }
                }
            }
            if !settled {
                log::warn!("KsJob::end: timeout waiting for completion");
            }
            dev.printing.store(false, Ordering::Release);
        }
    }
}

#[async_trait::async_trait]
impl RasterDriver for KsJob {
    type Device = KsDevice;

    fn start_job(
        printer: &PrinterHandle<'_>,
        options: &JobOptions,
        dev: &Self::Device,
    ) -> Result<Self, JobFailure> {
        let w = options.width;
        let h = options.height;

        // The device knows its own wire protocol; it was resolved from the
        // driver family when the transport was opened.
        let profile = dev.profile();

        let darkness = printer.darkness();
        // darkness is 0-100%; scale onto the model's density range, rounding
        // to nearest. The E-series accepts a wider range than the T50's 0-15.
        let max_density = profile.params().max_density as i32;
        let density = ((darkness * max_density + 50) / 100) as u8;
        let printhead_width_dots = printer.printhead_width_dots();

        let mut ks = KsJob::start(dev, options, density, printhead_width_dots, profile)?;
        if options.bits_per_pixel > 1 && dumps_enabled() {
            ks.pgm_acc = Some(PgmAccumulator::new(w, h));
        }
        Ok(ks)
    }

    fn write_line(&mut self, options: &JobOptions, y: u32, line: &[u8]) -> Result<(), JobFailure> {
        // Anything deeper than 1 bpp is a contone raster we have to dither
        // down to the printhead's 1-bit dots.
        if let Some(input) = to_gray_line(options.bits_per_pixel, line, options.width) {
            let width = options.width;
            if let Some(ref mut acc) = self.pgm_acc {
                acc.push_line(y, &input);
            }
            let bpl_1bpp = width.div_ceil(8) as usize;
            let mut mono = vec![0u8; bpl_1bpp];
            dither_line(&input, width, y, &mut mono);
            if !self.append_line(y, &mono) {
                return Err(JobFailure::other(format!(
                    "write_line: y={y} out of bounds"
                )));
            }
            return Ok(());
        }
        if options.bits_per_pixel != 1 {
            return Err(JobFailure::other(format!(
                "write_line: unsupported raster depth {} bpp",
                options.bits_per_pixel
            )));
        }
        if !self.append_line(y, line) {
            return Err(JobFailure::other(format!(
                "write_line: y={y} out of bounds"
            )));
        }
        Ok(())
    }

    async fn end_page(
        &mut self,
        options: &JobOptions,
        _page: u32,
        dev: &Self::Device,
    ) -> Result<(), JobFailure> {
        let copies = options.copies;
        for copy in 0..copies {
            if copies > 1 {
                log::info!("end_page: copy {}/{copies}", copy + 1);
            }
            self.transfer_page(dev).await?;
        }
        self.clear_page();
        Ok(())
    }

    async fn end_job(self, dev: &Self::Device) {
        self.end(dev).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CUPS may pad a 1 bpp scanline wider than the page needs; keeping that
    /// padding shears every row, since `raster_to_column_major` re-reads the
    /// buffer at `ceil(width/8)`.
    #[test]
    fn start_packs_padded_1bpp_rows_to_the_page_stride() {
        let dev = KsDevice::open_mock();
        let (w, h) = (100u32, 4u32);
        let packed = w.div_ceil(8); // 13
        let padded = 16; // what CUPS handed us

        let options = JobOptions::from_cups_v1(w, h, 1, padded, 1);
        let mut ks =
            KsJob::start(&dev, &options, 0, 128, PrintProfile::TSeries).unwrap();
        assert_eq!(ks.bytes_per_line, packed);
        assert_eq!(ks.raster_data.len(), (h * packed) as usize);

        // Every row arrives at the source stride, with the padding set.
        for y in 0..h {
            let mut line = vec![0x00u8; padded as usize];
            line[0] = 0xA5;
            for b in line.iter_mut().skip(packed as usize) {
                *b = 0xFF;
            }
            assert!(ks.append_line(y, &line));
        }

        // Each row starts where the packed stride says it does, and none of
        // the source padding leaked in.
        for y in 0..h {
            let row = &ks.raster_data[(y * packed) as usize..((y + 1) * packed) as usize];
            assert_eq!(row[0], 0xA5, "row {y} misaligned");
            assert!(row[1..].iter().all(|&b| b == 0x00), "row {y} kept padding");
        }
    }

    #[test]
    fn start_sizes_contone_pages_from_the_dithered_width() {
        let dev = KsDevice::open_mock();
        let options = JobOptions::from_cups_v1(100, 4, 24, 300, 1);
        let ks = KsJob::start(&dev, &options, 0, 128, PrintProfile::TSeries).unwrap();
        assert_eq!(ks.bytes_per_line, 13);
    }
}
