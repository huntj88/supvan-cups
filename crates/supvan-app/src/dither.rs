use std::borrow::Cow;

/// Thermal-compensated sRGB-to-dither LUT.
///
/// Combines standard sRGB linearization (gamma ~2.2) with a thermal bleed
/// compensation curve (gamma correction factor = 5.0). This pushes mid-tones
/// significantly lighter to compensate for dot spread on thermal printers,
/// where anything above ~50% dot density appears solid black.
///
/// Key mappings (W colorspace: 0=black, 255=white):
///   W=  0 -> 100% dots, W= 48 -> 50%, W=128 -> 25%, W=192 -> 12.5%, W=255 -> 0%
static SRGB_TO_LINEAR: [u8; 256] = [
    0, 50, 58, 63, 67, 70, 72, 74, 76, 78, 80, 82, 83, 85, 86, 88, 89, 90, 92, 93, 95, 96, 97, 98,
    100, 101, 102, 103, 105, 106, 107, 108, 109, 110, 112, 113, 114, 115, 116, 117, 118, 119, 120,
    121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 135, 136, 137, 138,
    139, 140, 141, 142, 142, 143, 144, 145, 146, 147, 148, 148, 149, 150, 151, 152, 152, 153, 154,
    155, 156, 156, 157, 158, 159, 159, 160, 161, 162, 162, 163, 164, 165, 165, 166, 167, 167, 168,
    169, 170, 170, 171, 172, 172, 173, 174, 174, 175, 176, 177, 177, 178, 179, 179, 180, 181, 181,
    182, 183, 183, 184, 184, 185, 186, 186, 187, 188, 188, 189, 190, 190, 191, 191, 192, 193, 193,
    194, 195, 195, 196, 196, 197, 198, 198, 199, 199, 200, 201, 201, 202, 202, 203, 203, 204, 205,
    205, 206, 206, 207, 207, 208, 209, 209, 210, 210, 211, 211, 212, 213, 213, 214, 214, 215, 215,
    216, 216, 217, 217, 218, 219, 219, 220, 220, 221, 221, 222, 222, 223, 223, 224, 224, 225, 225,
    226, 226, 227, 227, 228, 228, 229, 230, 230, 231, 231, 232, 232, 233, 233, 234, 234, 235, 235,
    236, 236, 237, 237, 238, 238, 238, 239, 239, 240, 240, 241, 241, 242, 242, 243, 243, 244, 244,
    245, 245, 246, 246, 247, 247, 248, 248, 249, 249, 249, 250, 250, 251, 251, 252, 252, 253, 253,
    254, 254, 255, 255,
];

/// 4x4 Bayer ordered dither threshold matrix, scaled 0-255.
static BAYER4: [[u8; 4]; 4] = [
    [8, 136, 40, 168],
    [200, 72, 232, 104],
    [56, 184, 24, 152],
    [248, 120, 216, 88],
];

/// Dither an 8bpp sRGB grayscale line to 1bpp MSB-first, with horizontal mirror.
///
/// `line`: input grayscale pixels (0x00 = black, 0xFF = white / W colorspace), length >= `width`.
/// `width`: number of pixels.
/// `y`: current scanline index (for Bayer pattern row selection).
/// `mono`: output 1bpp buffer, length >= `(width + 7) / 8`. Caller must zero it first.
pub fn dither_line(line: &[u8], width: u32, y: u32, mono: &mut [u8]) {
    let bayer_row = &BAYER4[(y & 3) as usize];
    for x in 0..width {
        let mx = width - 1 - x; // mirror
        let linear = SRGB_TO_LINEAR[line[x as usize] as usize];
        if linear < bayer_row[(mx & 3) as usize] {
            mono[(mx / 8) as usize] |= 0x80 >> (mx & 7);
        }
    }
}

/// Flatten one contone scanline to the 8-bit grey [`dither_line`] expects.
///
/// CUPS picks the colour space from the URF list the IPP layer advertises
/// (`W8,SRGB24,…`), so a Ghostscript job arrives as 24-bit sRGB as often as
/// 8-bit grey — the driver does not get to choose. `None` for a depth that
/// is not contone; the caller handles 1 bpp or rejects it.
///
/// 24 bpp uses the same Rec. 709 weights as `image`'s `to_luma8()`, so a
/// picture prints identically via raster or JPEG. Always returns exactly
/// `width` bytes — short lines pad white — so [`dither_line`] can index
/// `0..width` unchecked.
pub fn to_gray_line<'a>(bits_per_pixel: u32, line: &'a [u8], width: u32) -> Option<Cow<'a, [u8]>> {
    match bits_per_pixel {
        8 if line.len() >= width as usize => Some(Cow::Borrowed(&line[..width as usize])),
        8 => {
            let mut padded = vec![0xff; width as usize];
            padded[..line.len()].copy_from_slice(line);
            Some(Cow::Owned(padded))
        }
        24 => Some(Cow::Owned(
            (0..width as usize)
                .map(|x| match line.get(x * 3..x * 3 + 3) {
                    Some(px) => {
                        let (r, g, b) = (px[0] as u32, px[1] as u32, px[2] as u32);
                        ((r * 2126 + g * 7152 + b * 722) / 10000) as u8
                    }
                    None => 0xff,
                })
                .collect(),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dither_all_black() {
        let line = vec![0x00; 8]; // all black (W colorspace)
        let mut mono = vec![0u8; 1];
        dither_line(&line, 8, 0, &mut mono);
        // All pixels are 0 -> LUT[0]=0 < any threshold -> all bits set
        assert_eq!(mono[0], 0xFF);
    }

    #[test]
    fn test_dither_all_white() {
        let line = vec![0xFF; 8]; // all white (W colorspace)
        let mut mono = vec![0u8; 1];
        dither_line(&line, 8, 0, &mut mono);
        // All pixels are 255 -> LUT[255]=255 < threshold never -> all bits clear
        assert_eq!(mono[0], 0x00);
    }

    #[test]
    fn test_dither_midtone_lighter_than_50_percent() {
        // With thermal compensation, sRGB mid-gray should produce well under 50% dots
        let line = vec![0x80; 32]; // sRGB 128 → LUT value 188 → ~25% dots
        let mut total_bits = 0u32;
        for y in 0..4 {
            let mut mono = vec![0u8; 4];
            dither_line(&line, 32, y, &mut mono);
            for &b in &mono {
                total_bits += b.count_ones();
            }
        }
        // With gamma=5 compensation, sRGB 0x80 → ~25% coverage (32 of 128)
        assert!(
            total_bits > 16 && total_bits < 64,
            "expected ~25% bits set, got {total_bits}/128"
        );
    }

    #[test]
    fn test_dither_output_size() {
        let line = vec![0x80; 13]; // non-aligned width
        let bpl = 13_usize.div_ceil(8);
        let mut mono = vec![0u8; bpl];
        dither_line(&line, 13, 0, &mut mono);
        // Just verify it doesn't panic
    }

    #[test]
    fn gray_line_from_rgb_matches_image_crate_weights() {
        // Pure white / black must survive exactly, or a mostly-white label
        // dithers to solid ink — which is what a 24 bpp raster fed straight
        // into the 1-bit page buffer produced.
        let g = |px: &[u8], w| to_gray_line(24, px, w).unwrap().into_owned();
        assert_eq!(g(&[0xff, 0xff, 0xff], 1), vec![0xff]);
        assert_eq!(g(&[0x00, 0x00, 0x00], 1), vec![0x00]);
        // Rec. 709: green dominates, blue barely registers.
        assert_eq!(g(&[0, 0xff, 0], 1), vec![182]);
        assert_eq!(g(&[0, 0, 0xff], 1), vec![18]);
    }

    #[test]
    fn gray_line_pads_short_rgb_lines_with_white() {
        // One complete pixel, then a truncated scanline: the rest must read as
        // blank paper rather than ink.
        let out = to_gray_line(24, &[0, 0, 0], 3).unwrap();
        assert_eq!(&*out, &[0x00, 0xff, 0xff]);
    }

    #[test]
    fn gray_line_passes_8bpp_through_and_rejects_other_depths() {
        let out = to_gray_line(8, &[1, 2, 3, 4], 3).unwrap();
        assert_eq!(&*out, &[1, 2, 3], "8 bpp is already grey, and is clipped");
        assert!(to_gray_line(1, &[0xff], 8).is_none());
        assert!(to_gray_line(16, &[0xff; 4], 2).is_none());
    }

    #[test]
    fn gray_line_pads_short_8bpp_lines_with_white() {
        // A truncated grey scanline must still cover the full width, or
        // `dither_line` indexes past the end of the slice and panics.
        let out = to_gray_line(8, &[0x10, 0x20], 4).unwrap();
        assert_eq!(&*out, &[0x10, 0x20, 0xff, 0xff]);
    }

    #[test]
    fn dither_line_survives_short_contone_input() {
        for bpp in [8, 24] {
            let input = to_gray_line(bpp, &[0x00], 16).unwrap();
            let mut mono = vec![0u8; 2];
            dither_line(&input, 16, 0, &mut mono);
        }
    }
}
