use crate::profile::PrintProfile;

/// Max image data bytes per print buffer (from Android R2.drawable.sf5334_).
/// Applies to [`PrintProfile::TSeries`]; other profiles derive their own limit
/// from [`ProfileParams::max_buf_data`](crate::profile::ProfileParams::max_buf_data).
pub const MAX_BUF_DATA: usize = 4074;

/// Print buffer size for [`PrintProfile::TSeries`]. Model-specific sizes come
/// from [`ProfileParams::buf_size`](crate::profile::ProfileParams::buf_size).
pub const PRINT_BUF_SIZE: usize = 4096;

/// Header size in print buffer.
pub const PRINT_BUF_HEADER: usize = 14;

/// Margin clamp range (dots) for the print-buffer header.
const MARGIN_MAX_DOTS: u16 = 900;

/// The firmware re-reads the running checksum at every Nth byte; the builder
/// folds in the byte just before each boundary.
const CHECKSUM_STRIDE: usize = 256;

/// Parameters for PAGE_REG_BITS construction.
#[derive(Debug, Clone, Default)]
pub struct PageRegBits {
    pub page_st: bool,
    pub page_end: bool,
    pub prt_end: bool,
    pub cut: u8,
    pub savepaper: bool,
    pub first_cut: u8,
    pub nodu: u8,
    pub mat: u8,
}

/// Build PAGE_REG_BITS (2 bytes) for a print buffer header.
///
/// Byte 0:
///   bit 1: PageSt (first buffer of page)
///   bit 2: PageEnd (last buffer of page)
///   bit 3: PrtEnd (end of print job)
///   bits 4-6: Cut mode (3 bits)
///   bit 7: Savepaper
///
/// Byte 1:
///   bits 0-1: FirstCut
///   bits 2-5: Nodu (density, 0-15)
///   bits 6-7: Mat (material type)
pub fn build_page_reg_bits(p: &PageRegBits) -> [u8; 2] {
    let mut b0: u8 = 0;
    if p.page_st {
        b0 |= 0x02;
    }
    if p.page_end {
        b0 |= 0x04;
    }
    if p.prt_end {
        b0 |= 0x08;
    }
    b0 &= 0x0F;
    b0 |= (p.cut & 0x07) << 4;
    if p.savepaper {
        b0 |= 0x80;
    }

    let mut b1: u8 = 0;
    b1 |= p.first_cut & 0x03;
    b1 |= (p.nodu & 0x0F) << 2;
    b1 |= (p.mat & 0x03) << 6;

    [b0, b1]
}

/// Parameters for building a print buffer.
pub struct PrintBufferParams<'a> {
    pub image_data: &'a [u8],
    pub per_line_byte: u8,
    pub cols_in_buf: u16,
    pub page_st: bool,
    pub page_end: bool,
    pub prt_end: bool,
    pub margin_top: u16,
    pub margin_bottom: u16,
    pub density: u8,
    /// Model-specific constants (buffer size, `mat`/`nodu`, density cap).
    pub profile: PrintProfile,
}

/// Build one print buffer, sized by [`ProfileParams::buf_size`](crate::profile::ProfileParams::buf_size).
///
/// Layout:
///   [0..1]   Checksum (LE)
///   [2..3]   PAGE_REG_BITS
///   [4..5]   Column count (LE)
///   [6]      Bytes per line
///   [7]      Reserved (0)
///   [8..9]   Margin top (LE, 1-900 dots)
///   [10..11] Margin bottom (LE, 1-900 dots)
///   [12]     Density / red deepness (capped per profile)
///   [13]     0
///   [14..]   Image data
pub fn build_print_buffer(p: &PrintBufferParams) -> Vec<u8> {
    let params = p.profile.params();
    let buf_size = params.buf_size;
    let mut buf = vec![0u8; buf_size];

    // Capped once up here: `nodu` mirrors the density on models that don't
    // pin it, so a header must not carry a clamped [12] beside a raw `nodu`.
    let density = p.density.min(params.max_density);

    // PAGE_REG_BITS
    let page_bits = build_page_reg_bits(&PageRegBits {
        page_st: p.page_st,
        page_end: p.page_end,
        prt_end: p.prt_end,
        nodu: params.nodu.unwrap_or(density),
        mat: params.mat,
        ..Default::default()
    });
    buf[2] = page_bits[0];
    buf[3] = page_bits[1];

    // Column count
    buf[4..6].copy_from_slice(&p.cols_in_buf.to_le_bytes());

    // Bytes per line
    buf[6] = p.per_line_byte;

    // Margins (clamped 1..=MARGIN_MAX_DOTS)
    let mt = p.margin_top.clamp(1, MARGIN_MAX_DOTS);
    let mb = p.margin_bottom.clamp(1, MARGIN_MAX_DOTS);
    buf[8..10].copy_from_slice(&mt.to_le_bytes());
    buf[10..12].copy_from_slice(&mb.to_le_bytes());

    // Density
    buf[12] = density;

    // Image data at offset 14, bounded by the profile's declared data area
    // (eight bytes under what fits on the T series) rather than by what fits,
    // so bypassing `split_into_buffers` can't emit an invalid buffer.
    let data_len = p.image_data.len().min(params.max_buf_data);
    buf[PRINT_BUF_HEADER..PRINT_BUF_HEADER + data_len].copy_from_slice(&p.image_data[..data_len]);

    // Checksum: sum(buf[2..14]) + sum of bytes at each 256-byte boundary
    let data_end = (p.cols_in_buf as usize) * (p.per_line_byte as usize) + PRINT_BUF_HEADER;
    let mut chk: u32 = buf[2..14].iter().map(|&b| b as u32).sum();
    let n_strides = data_end / CHECKSUM_STRIDE;
    for i in 1..=n_strides {
        let idx = i * CHECKSUM_STRIDE - 1;
        if idx < buf.len() {
            chk += buf[idx] as u32;
        }
    }
    buf[0..2].copy_from_slice(&(chk as u16).to_le_bytes());

    buf
}

/// Columns of image data that fit in one print buffer.
///
/// Shared with [`crate::bitmap::create_test_pattern`], whose whole purpose is
/// drawing where the buffer boundaries land — it has to split identically.
pub fn max_cols_per_buffer(per_line_byte: u8, profile: PrintProfile) -> u16 {
    (profile.params().max_buf_data / per_line_byte as usize) as u16
}

/// Split column-major image data into print buffers ready for compression.
///
/// Buffer size and header constants come from `profile`; the number of columns
/// per buffer falls out of how many fit in the model's buffer.
pub fn split_into_buffers(
    image_data: &[u8],
    per_line_byte: u8,
    total_cols: u16,
    margin_top: u16,
    margin_bottom: u16,
    density: u8,
    profile: PrintProfile,
) -> Vec<Vec<u8>> {
    let max_cols = max_cols_per_buffer(per_line_byte, profile);
    let image_cols = total_cols - margin_top - margin_bottom;
    let mut buffers = Vec::new();
    let mut cols_remaining = image_cols;
    let mut current_col: u16 = 0;

    while cols_remaining > 0 {
        let cols_in_buf = cols_remaining.min(max_cols);
        let is_first = current_col == 0;
        let is_last = cols_remaining <= max_cols;

        let img_start = (margin_top + current_col) as usize * per_line_byte as usize;
        let img_end = img_start + cols_in_buf as usize * per_line_byte as usize;
        let img_chunk = &image_data[img_start..img_end.min(image_data.len())];

        let buf = build_print_buffer(&PrintBufferParams {
            image_data: img_chunk,
            per_line_byte,
            cols_in_buf,
            page_st: is_first,
            page_end: is_last,
            prt_end: is_last,
            margin_top,
            margin_bottom,
            // The E-series carries density only on the leading buffer.
            density: if is_first || !profile.params().density_on_first_buffer_only {
                density
            } else {
                0
            },
            profile,
        });
        buffers.push(buf);
        current_col += cols_in_buf;
        cols_remaining -= cols_in_buf;
    }

    buffers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_page_reg_bits_defaults() {
        let bits = build_page_reg_bits(&PageRegBits {
            nodu: 4,
            mat: 1,
            ..Default::default()
        });
        // b0: no flags, cut=0, savepaper=0 -> 0x00
        assert_eq!(bits[0], 0x00);
        // b1: first_cut=0, nodu=4 (<<2 = 0x10), mat=1 (<<6 = 0x40) -> 0x50
        assert_eq!(bits[1], 0x50);
    }

    #[test]
    fn test_build_page_reg_bits_first_last() {
        let bits = build_page_reg_bits(&PageRegBits {
            page_st: true,
            page_end: true,
            prt_end: true,
            nodu: 4,
            mat: 1,
            ..Default::default()
        });
        // b0: PageSt=0x02, PageEnd=0x04, PrtEnd=0x08 = 0x0E
        assert_eq!(bits[0], 0x0E);
        assert_eq!(bits[1], 0x50);
    }

    #[test]
    fn test_build_print_buffer_checksum() {
        let data = vec![0u8; 84 * 48]; // 84 cols * 48 bytes/line
        let buf = build_print_buffer(&PrintBufferParams {
            image_data: &data,
            per_line_byte: 48,
            cols_in_buf: 84,
            page_st: true,
            page_end: true,
            prt_end: true,
            margin_top: 8,
            margin_bottom: 8,
            density: 4,
            profile: PrintProfile::TSeries,
        });
        // Verify buffer structure
        assert_eq!(buf.len(), PRINT_BUF_SIZE);
        assert_eq!(buf[6], 48); // bytes per line
        assert_eq!(buf[4], 84); // cols low
        assert_eq!(buf[5], 0); // cols high
        assert_eq!(buf[8], 8); // margin top
        assert_eq!(buf[12], 4); // density
        // Checksum should be non-zero (at least header bytes contribute)
        let chk = buf[0] as u16 | ((buf[1] as u16) << 8);
        assert!(chk > 0);
    }

    #[test]
    fn test_split_into_buffers() {
        // 48 bytes/line, total 240 cols, margins 8+8 = 224 image cols
        // max_cols = 4074/48 = 84
        // 224 / 84 = 2 full + 56 remainder = 3 buffers
        let per_line_byte = 48u8;
        let total_cols = 240u16;
        let image_data = vec![0u8; total_cols as usize * per_line_byte as usize];
        let bufs = split_into_buffers(
            &image_data,
            per_line_byte,
            total_cols,
            8,
            8,
            4,
            PrintProfile::TSeries,
        );
        assert_eq!(bufs.len(), 3);
    }

    /// Reproduce the exact split an E10pro received from the vendor Android
    /// app: a 373-column page at 12 bytes/line became two buffers of 332 and
    /// 41 columns, headers `02 10 4c 01 0c 00 01 00 01 00 13 00` and
    /// `04 10 29 00 0c 00 01 00 01 00 00 00`.
    #[test]
    fn e10_split_matches_captured_vendor_page() {
        const COLS: u16 = 373;
        const PER_LINE: u8 = 12;
        let margin = PrintProfile::ESeries.params().margin_dots;
        let total = COLS + margin * 2;
        let image = vec![0u8; total as usize * PER_LINE as usize];

        let bufs = split_into_buffers(
            &image,
            PER_LINE,
            total,
            margin,
            margin,
            19,
            PrintProfile::ESeries,
        );

        assert_eq!(bufs.len(), 2);
        for b in &bufs {
            assert_eq!(b.len(), 4000, "E10 buffers are 4000 bytes, not 4096");
            assert_eq!(b[6], PER_LINE);
            assert_eq!(u16::from_le_bytes([b[8], b[9]]), 1, "margin_top");
            assert_eq!(u16::from_le_bytes([b[10], b[11]]), 1, "margin_bottom");
            // nodu is pinned at 4 and mat at 0, independent of density.
            assert_eq!(b[3], 0x10, "page_reg high byte: nodu=4, mat=0");
        }

        // First buffer: page_st only, carries the density.
        assert_eq!(u16::from_le_bytes([bufs[0][4], bufs[0][5]]), 332);
        assert_eq!(bufs[0][2], 0x02);
        assert_eq!(bufs[0][12], 19);

        // Last buffer: page_end + prt_end, density zeroed.
        assert_eq!(u16::from_le_bytes([bufs[1][4], bufs[1][5]]), 41);
        assert_eq!(bufs[1][2], 0x04 | 0x08);
        assert_eq!(bufs[1][12], 0);
    }

    /// Density 19 exceeds the T-series cap of 15 and must survive on E10.
    #[test]
    fn e10_density_is_not_clamped_to_the_t_series_maximum() {
        let params = |profile| PrintBufferParams {
            image_data: &[],
            per_line_byte: 12,
            cols_in_buf: 1,
            page_st: true,
            page_end: true,
            prt_end: true,
            margin_top: 1,
            margin_bottom: 1,
            density: 19,
            profile,
        };
        assert_eq!(build_print_buffer(&params(PrintProfile::ESeries))[12], 19);
        assert_eq!(build_print_buffer(&params(PrintProfile::TSeries))[12], 15);
    }

    /// `nodu` mirrors the density on the T series, so a header with a clamped
    /// `[12]` beside a raw `nodu` would describe two different densities.
    #[test]
    fn nodu_mirrors_the_capped_density_not_the_raw_one() {
        let buf = build_print_buffer(&PrintBufferParams {
            image_data: &[],
            per_line_byte: 12,
            cols_in_buf: 1,
            page_st: true,
            page_end: false,
            prt_end: false,
            margin_top: 1,
            margin_bottom: 1,
            density: 19,
            profile: PrintProfile::TSeries,
        });
        assert_eq!(buf[12], 15, "density byte is capped");
        // page_reg high byte packs nodu at bits 2..6.
        assert_eq!((buf[3] >> 2) & 0x0F, 15, "nodu tracks the capped density");
    }

    /// A buffer carries what the profile *declares*, which on the T series is
    /// eight bytes under what would otherwise fit.
    #[test]
    fn image_data_is_bounded_by_the_profiles_declared_data_area() {
        for profile in [PrintProfile::TSeries, PrintProfile::ESeries] {
            let params = profile.params();
            // The header plus the declared data area must fit the buffer, or
            // the copy below would be writing out of bounds.
            assert!(
                PRINT_BUF_HEADER + params.max_buf_data <= params.buf_size,
                "{profile:?}: header + data area overruns the buffer"
            );
            let oversized = vec![0xAAu8; params.buf_size];
            let buf = build_print_buffer(&PrintBufferParams {
                image_data: &oversized,
                per_line_byte: 12,
                cols_in_buf: 1,
                page_st: true,
                page_end: false,
                prt_end: false,
                margin_top: 1,
                margin_bottom: 1,
                density: 1,
                profile,
            });
            assert_eq!(buf.len(), params.buf_size);
            let copied = buf[PRINT_BUF_HEADER..].iter().filter(|&&b| b == 0xAA).count();
            assert_eq!(copied, params.max_buf_data, "{profile:?}: wrong data length");
        }
    }
}
