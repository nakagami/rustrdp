/// Planar codec (RDP 6.0 Bitmap Codec, MS-RDPEGDI 2.2.2.5).

const PLANAR_HEADER_CLL_MASK: u8 = 0x07;
const PLANAR_HEADER_CS: u8 = 0x08;
const PLANAR_HEADER_RLE: u8 = 0x10;
const PLANAR_HEADER_NA: u8 = 0x20;

#[inline]
fn clamp_byte(val: i32) -> u8 {
    val.clamp(0, 255) as u8
}

pub fn decode_planar(data: &[u8], w: usize, h: usize) -> Vec<u8> {
    let plane_size = w * h;
    if data.is_empty() || w == 0 || h == 0 {
        return vec![0u8; plane_size * 4];
    }

    let header = data[0];
    let cll = header & PLANAR_HEADER_CLL_MASK;
    let cs = (header & PLANAR_HEADER_CS) != 0;
    let rle = (header & PLANAR_HEADER_RLE) != 0;
    let no_alpha = (header & PLANAR_HEADER_NA) != 0;

    let sub_w = (w + 1) / 2;
    let sub_h = (h + 1) / 2;
    let sub_size = sub_w * sub_h;
    let mut offset = 1;

    let (alpha_plane, red_plane, green_plane, blue_plane) = if !rle {
        let alpha = if !no_alpha {
            let (p, next_off) = read_raw_plane(data, offset, plane_size);
            offset = next_off;
            Some(p)
        } else {
            None
        };
        let (r, next_off) = read_raw_plane(data, offset, plane_size);
        offset = next_off;
        let (g, next_off) = if cs {
            read_raw_plane(data, offset, sub_size)
        } else {
            read_raw_plane(data, offset, plane_size)
        };
        offset = next_off;
        let (b, _) = if cs {
            read_raw_plane(data, offset, sub_size)
        } else {
            read_raw_plane(data, offset, plane_size)
        };
        (alpha, r, g, b)
    } else {
        let alpha = if !no_alpha {
            let (p, next_off) = decode_plane_rle(data, offset, w, h);
            offset = next_off;
            Some(p)
        } else {
            None
        };
        let (r, next_off) = decode_plane_rle(data, offset, w, h);
        offset = next_off;
        let (g, next_off) = if cs {
            decode_plane_rle(data, offset, sub_w, sub_h)
        } else {
            decode_plane_rle(data, offset, w, h)
        };
        offset = next_off;
        let (b, _) = if cs {
            decode_plane_rle(data, offset, sub_w, sub_h)
        } else {
            decode_plane_rle(data, offset, w, h)
        };
        (alpha, r, g, b)
    };

    let mut out = vec![0u8; plane_size * 4];

    if cll == 0 {
        // Standard RGB
        let rp = &red_plane;
        let gp = &green_plane;
        let bp = &blue_plane;
        let has_alpha = !no_alpha && alpha_plane.is_some();
        let ap = alpha_plane.as_deref().unwrap_or(&[]);

        for i in 0..plane_size {
            let j = i * 4;
            let b_val = if i < bp.len() { bp[i] } else { 0 };
            let g_val = if i < gp.len() { gp[i] } else { 0 };
            let r_val = if i < rp.len() { rp[i] } else { 0 };
            let a_val = if has_alpha && i < ap.len() { ap[i] } else { 0xFF };

            out[j] = b_val;
            out[j + 1] = g_val;
            out[j + 2] = r_val;
            out[j + 3] = a_val;
        }
    } else {
        // YCoCg
        let yp = &red_plane;
        let gp = &green_plane;
        let bp = &blue_plane;
        let has_alpha = !no_alpha && alpha_plane.is_some();
        let ap = alpha_plane.as_deref().unwrap_or(&[]);
        let cll_shift = cll - 1;

        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let (co_val, cg_val) = if cs {
                    let sub_idx = (y / 2) * sub_w + (x / 2);
                    let co = if sub_idx < gp.len() { gp[sub_idx] } else { 0 };
                    let cg = if sub_idx < bp.len() { bp[sub_idx] } else { 0 };
                    (co, cg)
                } else {
                    let co = if i < gp.len() { gp[i] } else { 0 };
                    let cg = if i < bp.len() { bp[i] } else { 0 };
                    (co, cg)
                };

                let y_val = if i < yp.len() { yp[i] as i32 } else { 0 };
                let co = ((co_val as i8) as i32) << cll_shift;
                let cg = ((cg_val as i8) as i32) << cll_shift;

                let r = clamp_byte(y_val + co - cg);
                let g = clamp_byte(y_val + cg);
                let b = clamp_byte(y_val - co - cg);
                let a = if has_alpha && i < ap.len() { ap[i] } else { 0xFF };

                let j = i * 4;
                out[j] = b;
                out[j + 1] = g;
                out[j + 2] = r;
                out[j + 3] = a;
            }
        }
    }

    out
}

fn read_raw_plane(data: &[u8], offset: usize, size: usize) -> (Vec<u8>, usize) {
    let mut plane = vec![0u8; size];
    let end = (offset + size).min(data.len());
    if offset < end {
        let copy_len = end - offset;
        plane[..copy_len].copy_from_slice(&data[offset..end]);
    }
    (plane, offset + size)
}

fn decode_plane_rle(data: &[u8], mut offset: usize, w: usize, h: usize) -> (Vec<u8>, usize) {
    let plane_size = w * h;
    let mut out = vec![0u8; plane_size];
    if offset >= data.len() || plane_size == 0 {
        return (out, offset);
    }

    for y in 0..h {
        let row_start = y * w;
        let mut x = 0;
        let mut pixel: i16 = 0;

        while x < w && offset < data.len() {
            let ctrl = data[offset];
            offset += 1;

            let mut run_len = (ctrl & 0x0F) as usize;
            let mut raw_bytes = ((ctrl >> 4) & 0x0F) as usize;

            if run_len == 1 {
                run_len = raw_bytes + 16;
                raw_bytes = 0;
            } else if run_len == 2 {
                run_len = raw_bytes + 32;
                raw_bytes = 0;
            }

            if y == 0 {
                // Scanline 0: absolute values
                while raw_bytes > 0 && x < w && offset < data.len() {
                    pixel = data[offset] as i16;
                    offset += 1;
                    out[row_start + x] = pixel as u8;
                    x += 1;
                    raw_bytes -= 1;
                }
                while run_len > 0 && x < w {
                    out[row_start + x] = pixel as u8;
                    x += 1;
                    run_len -= 1;
                }
            } else {
                // Scanline > 0: delta values relative to previous scanline
                let prev_row_start = (y - 1) * w;
                while raw_bytes > 0 && x < w && offset < data.len() {
                    let delta_value = data[offset] as i16;
                    offset += 1;
                    if (delta_value & 1) != 0 {
                        pixel = -((delta_value >> 1) + 1);
                    } else {
                        pixel = delta_value >> 1;
                    }
                    let prev_val = out[prev_row_start + x] as i16;
                    out[row_start + x] = (prev_val + pixel) as u8;
                    x += 1;
                    raw_bytes -= 1;
                }
                while run_len > 0 && x < w {
                    let prev_val = out[prev_row_start + x] as i16;
                    out[row_start + x] = (prev_val + pixel) as u8;
                    x += 1;
                    run_len -= 1;
                }
            }
        }
    }

    (out, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_planar_raw_no_alpha() {
        let w = 2;
        let h = 2;
        let header = PLANAR_HEADER_NA; // 0x20
        let red = [10u8, 20, 30, 40];
        let green = [50u8, 60, 70, 80];
        let blue = [90u8, 100, 110, 120];

        let mut data = vec![header];
        data.extend_from_slice(&red);
        data.extend_from_slice(&green);
        data.extend_from_slice(&blue);

        let out = decode_planar(&data, w, h);
        assert_eq!(out.len(), w * h * 4);

        for i in 0..4 {
            let b = out[i * 4];
            let g = out[i * 4 + 1];
            let r = out[i * 4 + 2];
            let a = out[i * 4 + 3];
            assert_eq!(b, blue[i]);
            assert_eq!(g, green[i]);
            assert_eq!(r, red[i]);
            assert_eq!(a, 0xFF);
        }
    }

    #[test]
    fn test_decode_planar_rle_no_alpha() {
        let w = 4;
        let h = 2;
        let header = PLANAR_HEADER_RLE | PLANAR_HEADER_NA;

        let red_rle = [0x13u8, 10, 0x13, 0];
        let green_rle = [0x40u8, 20, 21, 22, 23, 0x13, 2];
        let blue_rle = [0x13u8, 30, 0x13, 1];

        let mut data = vec![header];
        data.extend_from_slice(&red_rle);
        data.extend_from_slice(&green_rle);
        data.extend_from_slice(&blue_rle);

        let out = decode_planar(&data, w, h);

        let expected = [
            (30, 20, 10), (30, 21, 10), (30, 22, 10), (30, 23, 10),
            (29, 21, 10), (29, 22, 10), (29, 23, 10), (29, 24, 10),
        ];

        for (i, &(eb, eg, er)) in expected.iter().enumerate() {
            let b = out[i * 4];
            let g = out[i * 4 + 1];
            let r = out[i * 4 + 2];
            let a = out[i * 4 + 3];
            assert_eq!((b, g, r, a), (eb, eg, er, 0xFF));
        }
    }
}
