pub fn decompress(input: &[u8], width: usize, height: usize, bpp: usize) -> Vec<u8> {
    let bytes_per_pixel = match bpp {
        15 | 16 => 2,
        24 => 3,
        32 => 4,
        _ => 1,
    };
    let output_size = width
        .saturating_mul(height)
        .saturating_mul(bytes_per_pixel);
    if output_size > 32 * 1024 * 1024 {
        log::warn!("rle::decompress: suspiciously large output_size={}, skipping", output_size);
        return vec![];
    }
    let mut output = vec![0u8; output_size];
    let mut pos = 0usize;
    let mut out_pos = 0usize;

    let mut fg = vec![0xFFu8; bytes_per_pixel];
    let bg = vec![0x00u8; bytes_per_pixel];

    while pos < input.len() && out_pos < output_size {
        let order = input[pos];
        pos += 1;

        let (order_type, mut run_length) = decode_order(order, input, &mut pos, bytes_per_pixel);
        if run_length == 0 { run_length = 8; }

        match order_type {
            0xF0 | 0x00 => {
                for _ in 0..run_length {
                    if out_pos + bytes_per_pixel <= output_size {
                        output[out_pos..out_pos + bytes_per_pixel].copy_from_slice(&bg);
                        out_pos += bytes_per_pixel;
                    }
                }
            }
            0xF1 | 0x01 => {
                for _ in 0..run_length {
                    if out_pos + bytes_per_pixel <= output_size {
                        output[out_pos..out_pos + bytes_per_pixel].copy_from_slice(&fg);
                        out_pos += bytes_per_pixel;
                    }
                }
            }
            0xFD | 0x0D => {
                if pos + bytes_per_pixel <= input.len() {
                    fg = input[pos..pos + bytes_per_pixel].to_vec();
                    pos += bytes_per_pixel;
                }
                for _ in 0..run_length {
                    if out_pos + bytes_per_pixel <= output_size {
                        output[out_pos..out_pos + bytes_per_pixel].copy_from_slice(&fg);
                        out_pos += bytes_per_pixel;
                    }
                }
            }
            0xF2 | 0x02 => {
                decode_fgbg(input, &mut pos, &mut output, &mut out_pos, run_length, &fg, &bg, bytes_per_pixel, output_size);
            }
            0xFC | 0x0C => {
                if pos + bytes_per_pixel <= input.len() {
                    fg = input[pos..pos + bytes_per_pixel].to_vec();
                    pos += bytes_per_pixel;
                }
                decode_fgbg(input, &mut pos, &mut output, &mut out_pos, run_length, &fg, &bg, bytes_per_pixel, output_size);
            }
            0xF3 | 0x03 => {
                if pos + bytes_per_pixel <= input.len() {
                    let color = input[pos..pos + bytes_per_pixel].to_vec();
                    pos += bytes_per_pixel;
                    for _ in 0..run_length {
                        if out_pos + bytes_per_pixel <= output_size {
                            output[out_pos..out_pos + bytes_per_pixel].copy_from_slice(&color);
                            out_pos += bytes_per_pixel;
                        }
                    }
                }
            }
            0xF4 | 0x04 => {
                for _ in 0..run_length {
                    if pos + bytes_per_pixel <= input.len() && out_pos + bytes_per_pixel <= output_size {
                        output[out_pos..out_pos + bytes_per_pixel].copy_from_slice(&input[pos..pos + bytes_per_pixel]);
                        pos += bytes_per_pixel;
                        out_pos += bytes_per_pixel;
                    }
                }
            }
            0xF8 | 0xF9 => {
                decode_fgbg(input, &mut pos, &mut output, &mut out_pos, 8, &fg, &bg, bytes_per_pixel, output_size);
            }
            0xFA => {
                if out_pos + bytes_per_pixel <= output_size {
                    let px = vec![0xFFu8; bytes_per_pixel];
                    output[out_pos..out_pos + bytes_per_pixel].copy_from_slice(&px);
                    out_pos += bytes_per_pixel;
                }
            }
            0xFB => {
                if out_pos + bytes_per_pixel <= output_size {
                    let px = vec![0x00u8; bytes_per_pixel];
                    output[out_pos..out_pos + bytes_per_pixel].copy_from_slice(&px);
                    out_pos += bytes_per_pixel;
                }
            }
            _ => {}
        }
    }

    output
}

fn decode_order(order: u8, input: &[u8], pos: &mut usize, _bpp: usize) -> (u8, usize) {
    match order {
        0xF0..=0xFF => {
            let len = if *pos + 1 < input.len() {
                let lo = input[*pos] as usize;
                let hi = input[*pos + 1] as usize;
                *pos += 2;
                lo | (hi << 8)
            } else { 0 };
            (order, len)
        }
        _ => {
            let run = (order & 0x1F) as usize;
            (order & 0xF0, run)
        }
    }
}

fn decode_fgbg(
    input: &[u8],
    pos: &mut usize,
    output: &mut Vec<u8>,
    out_pos: &mut usize,
    count: usize,
    fg: &[u8],
    bg: &[u8],
    bpp: usize,
    max: usize,
) {
    let bytes_needed = (count + 7) / 8;
    let mut written = 0;
    for _ in 0..bytes_needed {
        let byte = if *pos < input.len() {
            let b = input[*pos];
            *pos += 1;
            b
        } else { 0 };
        for bit in 0..8 {
            if written >= count { break; }
            if *out_pos + bpp <= max {
                if (byte >> bit) & 1 != 0 {
                    output[*out_pos..*out_pos + bpp].copy_from_slice(&fg[..bpp]);
                } else {
                    output[*out_pos..*out_pos + bpp].copy_from_slice(&bg[..bpp]);
                }
                *out_pos += bpp;
            }
            written += 1;
        }
    }
}
