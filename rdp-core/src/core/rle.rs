/// Process one colour plane for the 32-bpp plane-encoded decompressor.
/// Matches grdp's `processPlane` exactly.
/// `j` is the byte offset within each BGRA pixel (0=B, 1=G, 2=R, 3=A).
fn process_plane(input: &mut &[u8], width: usize, height: usize, output: &mut [u8], j: usize) {
    let total = width * height * 4;
    let mut lastline: usize = 0; // 0 = sentinel "no previous row"

    for indexh in 0..height {
        let thisline = j + total - (indexh + 1) * width * 4;
        let mut color: u8 = 0; // delta (non-first rows) or absolute value (first row)
        let mut indexw: usize = 0;
        let mut i = thisline;

        while indexw < width {
            if input.is_empty() {
                return;
            }
            let code = input[0] as usize;
            *input = &input[1..];

            let mut replen = code & 0xf;
            let mut collen = (code >> 4) & 0xf;
            let revcode = (replen << 4) | collen;
            if revcode >= 16 && revcode <= 47 {
                replen = revcode;
                collen = 0;
            }

            if lastline == 0 {
                // First row: absolute pixel values
                while collen > 0 && indexw < width {
                    if input.is_empty() {
                        return;
                    }
                    color = input[0];
                    *input = &input[1..];
                    output[i] = color;
                    i += 4;
                    indexw += 1;
                    collen -= 1;
                }
                while replen > 0 && indexw < width {
                    output[i] = color;
                    i += 4;
                    indexw += 1;
                    replen -= 1;
                }
            } else {
                // Subsequent rows: delta from previous row
                while collen > 0 && indexw < width {
                    if input.is_empty() {
                        return;
                    }
                    let x = input[0];
                    *input = &input[1..];
                    // Signed delta encoded as: if odd → -(x>>1)-1; if even → x>>1
                    color = if x & 1 != 0 {
                        0u8.wrapping_sub((x >> 1).wrapping_add(1))
                    } else {
                        x >> 1
                    };
                    output[i] = output[indexw * 4 + lastline].wrapping_add(color);
                    i += 4;
                    indexw += 1;
                    collen -= 1;
                }
                while replen > 0 && indexw < width {
                    // Repeat the same delta for consecutive previous-row pixels
                    output[i] = output[indexw * 4 + lastline].wrapping_add(color);
                    i += 4;
                    indexw += 1;
                    replen -= 1;
                }
            }
        }

        lastline = thisline;
    }
}

/// Decompress a 32-bpp RDP plane-encoded bitmap (MS-RDPBCGR §3.1.9.1.4).
/// Matches grdp's `decompress4`.
fn decompress_32bpp(input: &[u8], width: usize, height: usize) -> Vec<u8> {
    let total = width * height * 4;
    if total == 0 || input.is_empty() {
        return vec![0u8; total];
    }

    let flags = input[0];
    let rle = flags & 0x10 != 0;
    let no_alpha = flags & 0x20 != 0;

    if !rle {
        // Not RLE-encoded; return zeroed buffer
        return vec![0u8; total];
    }

    let mut out = vec![0u8; total];
    let mut data = &input[1..];

    if no_alpha {
        // No alpha plane in stream; fill alpha channel with 0xFF
        for i in (3..total).step_by(4) {
            out[i] = 0xFF;
        }
    } else {
        process_plane(&mut data, width, height, &mut out, 3); // Alpha
    }
    process_plane(&mut data, width, height, &mut out, 2); // Red
    process_plane(&mut data, width, height, &mut out, 1); // Green
    process_plane(&mut data, width, height, &mut out, 0); // Blue

    out
}

/// Decompress an RDP RLE-compressed bitmap tile.
///
/// For bpp=32 uses plane-encoded decompression (MS-RDPBCGR §3.1.9.1.4).
/// For bpp=8/15/16/24 uses the scanline-based RLE algorithm (§3.1.9.2).
/// Output is in top-down, left-to-right order (first byte = top-left pixel).
pub fn decompress(input: &[u8], width: usize, height: usize, bpp: usize) -> Vec<u8> {
    if bpp == 32 {
        return decompress_32bpp(input, width, height);
    }

    let bpp_bytes: usize = match bpp {
        8 => 1,
        15 | 16 => 2,
        24 => 3,
        _ => return vec![],
    };
    let stride = width * bpp_bytes;
    let total = stride * height;
    if total == 0 || total > 32 * 1024 * 1024 {
        return vec![];
    }

    let mut out = vec![0u8; total];

    let mut pos: usize = 0;
    // Scanline tracking (same as grdp decompress1/2/3):
    //   x = column; starts at width to force first line advance before any output.
    //   h = rows remaining (counts down from height to 0).
    //   line = byte offset of current row in out[].
    //   prevline = byte offset of previous row (value 0 = sentinel "no previous row").
    let mut x: usize = width;
    let mut h: usize = height;
    let mut line: usize = 0;
    let mut prevline: usize = 0;

    let mut mix = vec![0xFFu8; bpp_bytes];
    let mut colour1 = vec![0u8; bpp_bytes];
    let mut colour2 = vec![0u8; bpp_bytes];

    let mut last_opcode: i32 = -1;
    let mut insertmix = false;
    let mut bicolour = false;
    let mut mixmask: u8 = 0;
    let mut mask: u8 = 0;
    let mut fom_mask: u8 = 0;

    while pos < input.len() {
        fom_mask = 0;
        let code = input[pos] as usize;
        pos += 1;

        let mut opcode = code >> 4;
        let offset: usize;
        let mut count: usize;

        match opcode {
            0xC | 0xD | 0xE => {
                opcode -= 6;
                count = code & 0xF;
                offset = 16;
            }
            0xF => {
                opcode = code & 0xF;
                if opcode < 9 {
                    if pos + 1 >= input.len() {
                        break;
                    }
                    let lo = input[pos] as usize;
                    let hi = input[pos + 1] as usize;
                    pos += 2;
                    count = lo | (hi << 8);
                } else if opcode < 0xB {
                    count = 8;
                } else {
                    count = 1;
                }
                offset = 0;
            }
            _ => {
                opcode >>= 1;
                count = code & 0x1F;
                offset = 32;
            }
        }

        let isfillormix = opcode == 2 || opcode == 7;
        if offset != 0 {
            if count == 0 {
                if pos >= input.len() {
                    break;
                }
                if isfillormix {
                    count = input[pos] as usize + 1;
                } else {
                    count = input[pos] as usize + offset;
                }
                pos += 1;
            } else if isfillormix {
                count <<= 3;
            }
        }

        // Read opcode-specific preliminary data
        match opcode {
            0 => {
                if last_opcode == 0 && !((x == width) && (prevline == 0)) {
                    insertmix = true;
                }
            }
            3 => {
                if pos + bpp_bytes > input.len() {
                    break;
                }
                colour2[..bpp_bytes].copy_from_slice(&input[pos..pos + bpp_bytes]);
                pos += bpp_bytes;
            }
            6 => {
                if pos + bpp_bytes > input.len() {
                    break;
                }
                mix[..bpp_bytes].copy_from_slice(&input[pos..pos + bpp_bytes]);
                pos += bpp_bytes;
                opcode = 1;
            }
            7 => {
                if pos + bpp_bytes > input.len() {
                    break;
                }
                mix[..bpp_bytes].copy_from_slice(&input[pos..pos + bpp_bytes]);
                pos += bpp_bytes;
                opcode = 2;
            }
            8 => {
                if pos + bpp_bytes * 2 > input.len() {
                    break;
                }
                colour1[..bpp_bytes].copy_from_slice(&input[pos..pos + bpp_bytes]);
                pos += bpp_bytes;
                colour2[..bpp_bytes].copy_from_slice(&input[pos..pos + bpp_bytes]);
                pos += bpp_bytes;
            }
            9 => {
                mask = 0x03;
                opcode = 2;
                fom_mask = 3;
            }
            0xA => {
                mask = 0x05;
                opcode = 2;
                fom_mask = 5;
            }
            _ => {}
        }

        last_opcode = opcode as i32;
        mixmask = 0;

        // Output body: process `count` pixels, advancing scanlines as needed
        while count > 0 {
            if x >= width {
                if h == 0 {
                    return out;
                }
                x = 0;
                h -= 1;
                prevline = line;
                line = h * stride;
            }

            match opcode {
                0 => {
                    // Fill: copy from previous scanline (or zero if none)
                    if insertmix {
                        // Insert one mix pixel before the fill run
                        let base = line + x * bpp_bytes;
                        if prevline == 0 {
                            out[base..base + bpp_bytes].copy_from_slice(&mix[..bpp_bytes]);
                        } else {
                            let pbase = prevline + x * bpp_bytes;
                            for i in 0..bpp_bytes {
                                out[base + i] = out[pbase + i] ^ mix[i];
                            }
                        }
                        insertmix = false;
                        count -= 1;
                        x += 1;
                        // Fall through: write fill pixels for remaining count
                        // (no line-advance check here, matching grdp behaviour)
                    }
                    let n = count.min(width - x);
                    if n > 0 {
                        let base = line + x * bpp_bytes;
                        if prevline == 0 {
                            out[base..base + n * bpp_bytes].fill(0);
                        } else {
                            let pbase = prevline + x * bpp_bytes;
                            out.copy_within(pbase..pbase + n * bpp_bytes, base);
                        }
                        count -= n;
                        x += n;
                    }
                }
                1 => {
                    // Mix: XOR previous scanline with mix value (or write mix if no prev row)
                    let n = count.min(width - x);
                    let base = line + x * bpp_bytes;
                    if prevline == 0 {
                        for i in 0..n {
                            out[base + i * bpp_bytes..base + (i + 1) * bpp_bytes]
                                .copy_from_slice(&mix[..bpp_bytes]);
                        }
                    } else {
                        let pbase = prevline + x * bpp_bytes;
                        for i in 0..n * bpp_bytes {
                            out[base + i] = out[pbase + i] ^ mix[i % bpp_bytes];
                        }
                    }
                    count -= n;
                    x += n;
                }
                2 => {
                    // FillOrMix: per-pixel bitmask selects Fill (0) or Mix (1)
                    while count > 0 && x < width {
                        mixmask <<= 1;
                        if mixmask == 0 {
                            if fom_mask != 0 {
                                mask = fom_mask;
                            } else {
                                if pos >= input.len() {
                                    return out;
                                }
                                mask = input[pos];
                                pos += 1;
                            }
                            mixmask = 1;
                        }
                        let base = line + x * bpp_bytes;
                        if mask & mixmask != 0 {
                            if prevline == 0 {
                                out[base..base + bpp_bytes].copy_from_slice(&mix[..bpp_bytes]);
                            } else {
                                let pbase = prevline + x * bpp_bytes;
                                for i in 0..bpp_bytes {
                                    out[base + i] = out[pbase + i] ^ mix[i];
                                }
                            }
                        } else if prevline == 0 {
                            out[base..base + bpp_bytes].fill(0);
                        } else {
                            let pbase = prevline + x * bpp_bytes;
                            out.copy_within(pbase..pbase + bpp_bytes, base);
                        }
                        count -= 1;
                        x += 1;
                    }
                }
                3 => {
                    // Colour: fill with colour2
                    let n = count.min(width - x);
                    let base = line + x * bpp_bytes;
                    for i in 0..n {
                        out[base + i * bpp_bytes..base + (i + 1) * bpp_bytes]
                            .copy_from_slice(&colour2[..bpp_bytes]);
                    }
                    count -= n;
                    x += n;
                }
                4 => {
                    // Copy: literal pixels from input
                    while count > 0 && x < width {
                        if pos + bpp_bytes > input.len() {
                            return out;
                        }
                        let base = line + x * bpp_bytes;
                        out[base..base + bpp_bytes].copy_from_slice(&input[pos..pos + bpp_bytes]);
                        pos += bpp_bytes;
                        count -= 1;
                        x += 1;
                    }
                }
                8 => {
                    // Bicolour: alternate colour1 and colour2
                    while count > 0 && x < width {
                        let base = line + x * bpp_bytes;
                        if bicolour {
                            out[base..base + bpp_bytes].copy_from_slice(&colour2[..bpp_bytes]);
                            bicolour = false;
                        } else {
                            out[base..base + bpp_bytes].copy_from_slice(&colour1[..bpp_bytes]);
                            bicolour = true;
                            count += 1;
                        }
                        count -= 1;
                        x += 1;
                    }
                }
                0xD => {
                    // White
                    let n = count.min(width - x);
                    let base = line + x * bpp_bytes;
                    out[base..base + n * bpp_bytes].fill(0xFF);
                    count -= n;
                    x += n;
                }
                0xE => {
                    // Black
                    let n = count.min(width - x);
                    let base = line + x * bpp_bytes;
                    out[base..base + n * bpp_bytes].fill(0x00);
                    count -= n;
                    x += n;
                }
                _ => {
                    log::warn!("[rle] unknown opcode 0x{:x}", opcode);
                    count -= 1;
                    x += 1;
                }
            }
        }
    }

    out
}
