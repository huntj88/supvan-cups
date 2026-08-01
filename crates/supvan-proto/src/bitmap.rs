/// T50 Pro: 8 dots/mm, 48mm printhead.
pub const DOTS_PER_MM: u32 = 8;
pub const PRINTHEAD_WIDTH_MM: u32 = 48;
pub const PRINTHEAD_WIDTH_DOTS: u32 = PRINTHEAD_WIDTH_MM * DOTS_PER_MM;
pub const PRINTHEAD_BYTES_PER_LINE: u32 = PRINTHEAD_WIDTH_DOTS / 8;
pub const DEFAULT_MARGIN_DOTS: u16 = 8;

/// Convert a row-major MSB-first 1bpp bitmap (standard raster format) into
/// column-major LSB-first 1bpp format suitable for the printer.
///
/// The input bitmap is `width` x `height` pixels in row-major order with
/// MSB-first bit packing (standard CUPS/image convention: leftmost pixel
/// is the most significant bit).
///
/// The printer expects column-major LSB-first: each "column" of the output
/// corresponds to a column of dots in the printed label. After a -90 degree
/// rotation, the output has `height` columns, each `ceil(width/8)` bytes
/// wide with LSB-first packing.
///
/// This effectively rotates the image -90 degrees and repacks the bits.
///
/// Returns `(output_data, output_cols, bytes_per_line)`.
pub fn raster_to_column_major(input: &[u8], width: u32, height: u32) -> (Vec<u8>, u32, u32) {
    let in_bytes_per_row = width.div_ceil(8);
    let out_bytes_per_line = width.div_ceil(8); // printhead width packed
    let out_cols = height;

    let mut output = vec![0u8; out_cols as usize * out_bytes_per_line as usize];

    for y in 0..height {
        for x in 0..width {
            // Read pixel from row-major MSB-first input
            let in_byte_idx = y as usize * in_bytes_per_row as usize + (x / 8) as usize;
            let in_bit = 7 - (x % 8); // MSB-first
            if in_byte_idx >= input.len() {
                continue;
            }
            let pixel = (input[in_byte_idx] >> in_bit) & 1;

            if pixel != 0 {
                // Write to column-major LSB-first output
                // After -90 rotation: output column = y, row position = x
                let out_byte_idx = y as usize * out_bytes_per_line as usize + (x / 8) as usize;
                let out_bit = x % 8; // LSB-first
                output[out_byte_idx] |= 1 << out_bit;
            }
        }
    }

    (output, out_cols, out_bytes_per_line)
}

/// Center image data in a full-width printhead canvas.
///
/// The printhead is a fixed physical bar (384 dots / 48 mm on the T50) and the
/// media runs centred under it, so a page is always placed centred in a
/// head-width canvas regardless of the label's own width.
///
/// A page *wider* than the head is cropped symmetrically rather than padded.
/// That is reachable in normal operation — the T50 family advertises 50 mm
/// media on a 48 mm head — and CUPS renders the full media width because the
/// IPP layer declares zero hard margins on all four sides.
///
/// Input: column-major LSB-first data with `input_bytes_per_line` per column.
/// Output: column-major LSB-first data with `canvas_bytes_per_line` per column.
pub fn center_in_printhead(
    input: &[u8],
    num_cols: u32,
    input_width_dots: u32,
    canvas_width_dots: u32,
) -> (Vec<u8>, u32) {
    let canvas_bytes_per_line = (canvas_width_dots / 8) as usize;
    let input_bytes_per_line = input_width_dots.div_ceil(8) as usize;
    // A head whose width isn't a whole number of bytes (the G series is 190
    // dots) can only carry `canvas_bytes_per_line * 8`. Every dot decision
    // below measures against that, not the nominal width, which would walk
    // off the end of the last column.
    let usable_width_dots = (canvas_bytes_per_line * 8) as u32;

    if input_width_dots >= usable_width_dots {
        // Wider than the head: keep the *middle* of the image, because the
        // media runs centred under the printhead. Copying the leading bytes
        // instead drops one whole edge — a 50 mm label on the T50's 48 mm
        // head lost 2 mm off the right rather than 1 mm off each side.
        let lost = input_width_dots - usable_width_dots;
        if lost > 0 {
            let left = lost / 2;
            log::warn!(
                "center_in_printhead: image is {input_width_dots} dots but the head can \
                 carry {usable_width_dots} (nominal {canvas_width_dots}); cropping {lost} \
                 dots ({left} left, {} right)",
                lost - left
            );
        }
        let x_offset_dots = lost / 2;
        let mut output = vec![0u8; num_cols as usize * canvas_bytes_per_line];
        for col in 0..num_cols as usize {
            for dot in 0..usable_width_dots {
                // The crop offset is rarely byte-aligned, so shift bit by bit.
                let src_dot = x_offset_dots + dot;
                let in_byte = col * input_bytes_per_line + (src_dot / 8) as usize;
                if in_byte >= input.len() {
                    continue;
                }
                if (input[in_byte] >> (src_dot % 8)) & 1 != 0 {
                    let out_byte = col * canvas_bytes_per_line + (dot / 8) as usize;
                    output[out_byte] |= 1 << (dot % 8);
                }
            }
        }
        return (output, canvas_bytes_per_line as u32);
    }

    let x_offset_dots = (usable_width_dots - input_width_dots) / 2;
    let mut output = vec![0u8; num_cols as usize * canvas_bytes_per_line];

    for col in 0..num_cols as usize {
        for dot in 0..input_width_dots {
            // Read from input (LSB-first)
            let in_byte = col * input_bytes_per_line + (dot / 8) as usize;
            let in_bit = dot % 8;
            if in_byte >= input.len() {
                continue;
            }
            let pixel = (input[in_byte] >> in_bit) & 1;

            if pixel != 0 {
                // Write to output at offset position (LSB-first)
                let out_dot = x_offset_dots + dot;
                let out_byte = col * canvas_bytes_per_line + (out_dot / 8) as usize;
                let out_bit = out_dot % 8;
                if out_byte < output.len() {
                    output[out_byte] |= 1 << out_bit;
                }
            }
        }
    }

    (output, canvas_bytes_per_line as u32)
}

/// Create a test pattern matching the Python reference implementation.
///
/// Returns (image_bytes, canvas_width_dots, height_dots, bytes_per_line).
pub fn create_test_pattern(label_width_mm: u32, height_mm: u32) -> (Vec<u8>, u32, u32, u32) {
    let canvas_width_dots = PRINTHEAD_WIDTH_DOTS;
    let height_dots = height_mm * DOTS_PER_MM;
    let bytes_per_line = PRINTHEAD_BYTES_PER_LINE;
    let label_width_dots = label_width_mm * DOTS_PER_MM;
    let x_offset = (canvas_width_dots - label_width_dots) / 2;

    let margin_top = DEFAULT_MARGIN_DOTS as u32;
    let margin_bottom = DEFAULT_MARGIN_DOTS as u32;
    let max_cols = (crate::buffer::MAX_BUF_DATA / bytes_per_line as usize) as u32;

    // Compute buffer regions
    let mut buf_regions: Vec<(u32, u32)> = Vec::new();
    let mut col = margin_top;
    while col < height_dots - margin_bottom {
        let end = (col + max_cols).min(height_dots - margin_bottom);
        buf_regions.push((col, end));
        col = end;
    }

    // Column-major LSB-first output
    let mut buf = vec![0u8; bytes_per_line as usize * height_dots as usize];

    for col in 0..height_dots {
        for row in 0..canvas_width_dots {
            let mut pixel = false;

            let label_row = row as i32 - x_offset as i32;
            if label_row >= 0 && (label_row as u32) < label_width_dots {
                let lr = label_row as u32;

                // Outer border (2px)
                if lr < 2 || lr >= label_width_dots - 2 || col < 2 || col >= height_dots - 2 {
                    pixel = true;
                }

                // Per-buffer patterns
                for (i, &(bs, be)) in buf_regions.iter().enumerate() {
                    if col >= bs && col < be {
                        let bh = be - bs;
                        let bw = label_width_dots;
                        let local_col = col - bs;

                        // Buffer top/bottom border
                        if local_col < 2 || local_col >= bh - 2 {
                            pixel = true;
                        }

                        // X cross diagonals
                        if let Some(expected_row_1) = (local_col * bw).checked_div(bh) {
                            if (lr as i32 - expected_row_1 as i32).unsigned_abs() < 2 {
                                pixel = true;
                            }
                            let expected_row_2 = bw - 1 - expected_row_1;
                            if (lr as i32 - expected_row_2 as i32).unsigned_abs() < 2 {
                                pixel = true;
                            }
                        }

                        // Buffer number dots
                        for d in 0..=i as u32 {
                            let dx = 10 + d * 12;
                            let dy: u32 = 10;
                            if lr >= dx && lr < dx + 8 && local_col >= dy && local_col < dy + 8 {
                                pixel = true;
                            }
                        }
                        break;
                    }
                }
            }

            if pixel {
                let byte_idx = col as usize * bytes_per_line as usize + (row / 8) as usize;
                let bit_idx = row % 8; // LSB-first
                buf[byte_idx] |= 1 << bit_idx;
            }
        }
    }

    (buf, canvas_width_dots, height_dots, bytes_per_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raster_to_column_major_simple() {
        // 8x2 image: first row all black, second row all white
        // MSB-first: 0xFF (row 0), 0x00 (row 1)
        let input = [0xFF, 0x00];
        let (output, cols, bpl) = raster_to_column_major(&input, 8, 2);
        assert_eq!(cols, 2);
        assert_eq!(bpl, 1);
        // Column 0 (y=0): all 8 pixels set -> LSB-first = 0xFF
        assert_eq!(output[0], 0xFF);
        // Column 1 (y=1): all 8 pixels clear -> 0x00
        assert_eq!(output[1], 0x00);
    }

    #[test]
    fn test_center_in_printhead() {
        // 8 dot wide input centered in 24 dot canvas
        let input = vec![0xFF; 2]; // 2 columns, 1 byte each
        let (output, bpl) = center_in_printhead(&input, 2, 8, 24);
        assert_eq!(bpl, 3); // 24/8 = 3 bytes per line
        // 8 dots centered in 24 -> offset = 8 dots = 1 byte
        // Col 0: byte 0 = 0x00, byte 1 = 0xFF, byte 2 = 0x00
        assert_eq!(output[0], 0x00);
        assert_eq!(output[1], 0xFF);
        assert_eq!(output[2], 0x00);
    }

    #[test]
    fn test_create_test_pattern_dimensions() {
        let (data, w, h, bpl) = create_test_pattern(40, 30);
        assert_eq!(w, 384);
        assert_eq!(h, 240);
        assert_eq!(bpl, 48);
        assert_eq!(data.len(), 240 * 48);
    }

    /// A page wider than the head must lose the same amount from both sides,
    /// because the media runs centred under the printhead. Taking the leading
    /// bytes instead drops one edge entirely — a 50 mm label on the T50's
    /// 48 mm head lost 2 mm off the right rather than 1 mm off each side.
    #[test]
    fn oversized_input_is_cropped_symmetrically() {
        // One column, 120 dots wide: set only the outermost dot on each side.
        let input_w = 120u32;
        let head_w = 96u32;
        let bpl = (input_w / 8) as usize;
        let mut input = vec![0u8; bpl];
        let set = |buf: &mut [u8], dot: u32| buf[(dot / 8) as usize] |= 1 << (dot % 8);
        set(&mut input, 0);
        set(&mut input, input_w - 1);
        // ... and one dot just inside the expected crop window on each side.
        let margin = (input_w - head_w) / 2; // 12
        set(&mut input, margin);
        set(&mut input, input_w - 1 - margin);

        let (out, out_bpl) = center_in_printhead(&input, 1, input_w, head_w);
        assert_eq!(out_bpl, head_w / 8);
        let get = |buf: &[u8], dot: u32| (buf[(dot / 8) as usize] >> (dot % 8)) & 1 == 1;

        // The two outermost dots fall outside the window and are dropped.
        // The two just inside it survive, landing at the window's edges.
        assert!(
            get(&out, 0),
            "dot {margin} should map to the first head dot"
        );
        assert!(
            get(&out, head_w - 1),
            "the mirror-side dot should survive too"
        );
        // Nothing else should have been lit.
        let lit = (0..head_w).filter(|d| get(&out, *d)).count();
        assert_eq!(lit, 2, "exactly the two in-window dots should be set");
    }

    /// A head whose width is not a whole number of bytes — the G series is
    /// 190 dots — carries only `190 / 8 * 8 = 184` of them, because that is
    /// all the returned buffer has room for. Walking the nominal 190 wrote
    /// past the end of the last column: silent corruption of the next column
    /// for every column but the last, and an index-out-of-bounds panic on it.
    #[test]
    fn head_width_that_is_not_a_whole_number_of_bytes_stays_in_bounds() {
        const HEAD: u32 = 190;
        const COLS: u32 = 3;
        let in_bpl = HEAD.div_ceil(8) as usize;
        // Light every dot, so any reachable output byte would be written.
        let input = vec![0xffu8; COLS as usize * in_bpl];

        let (out, out_bpl) = center_in_printhead(&input, COLS, HEAD, HEAD);
        assert_eq!(out_bpl, 23, "184 usable dots, not 190");
        assert_eq!(out.len(), COLS as usize * 23);
        // Exactly the usable dots, and no bleed into a neighbouring column.
        assert!(out.iter().all(|&b| b == 0xff));
    }

    /// The same head must also centre a narrower page inside its *usable*
    /// width rather than its nominal one, or the guard in the centring branch
    /// silently eats the dots past 184.
    #[test]
    fn undersized_input_centres_within_the_usable_head_width() {
        const HEAD: u32 = 190;
        let input = vec![0xffu8; 2]; // 16 dots
        let (out, out_bpl) = center_in_printhead(&input, 1, 16, HEAD);
        assert_eq!(out_bpl, 23);
        let lit = (0..23 * 8)
            .filter(|d| (out[d / 8] >> (d % 8)) & 1 == 1)
            .count();
        assert_eq!(lit, 16, "every input dot lands inside the usable width");
    }
}
