/// NSCodec decoder for Remote Desktop Protocol (MS-RDPNSC).
/// Also used as subcodec 1 in ClearCodec (MS-RDPEGFX 2.2.4.1.1.4).

pub fn decode_nscodec(
    data: &[u8],
    width: usize,
    height: usize,
    out: &mut [u8],
    x_start: usize,
    y_start: usize,
    surf_w: usize,
    surf_h: usize,
) -> bool {
    if data.len() < 20 || width == 0 || height == 0 {
        return false;
    }

    let l0 = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let l1 = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let l2 = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let l3 = u32::from_le_bytes([data[12], data[13], data[14], data[15]]) as usize;
    let mut color_loss_level = data[16];
    let chroma_subsampling = data[17];

    if !(1..=7).contains(&color_loss_level) {
        color_loss_level = 1;
    }

    let rw = (width + 7) & !7;
    let rh = (height + 1) & !1;

    let org_sizes = if chroma_subsampling != 0 {
        [rw * height, (rw / 2) * (rh / 2), (rw / 2) * (rh / 2), width * height]
    } else {
        [width * height, width * height, width * height, width * height]
    };

    let plane_lens = [l0, l1, l2, l3];
    let mut planes = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];

    let mut off = 20;
    for i in 0..4 {
        let orig_size = org_sizes[i];
        let p_len = plane_lens[i];
        let mut plane = vec![0u8; orig_size];

        if p_len == 0 {
            plane.fill(0xFF);
        } else if p_len < orig_size {
            if off + p_len > data.len() {
                return false;
            }
            if !nsc_rle_decode(&data[off..off + p_len], &mut plane, orig_size) {
                return false;
            }
            off += p_len;
        } else {
            if off + orig_size > data.len() {
                return false;
            }
            plane.copy_from_slice(&data[off..off + orig_size]);
            off += p_len;
        }
        planes[i] = plane;
    }

    let shift = color_loss_level - 1;
    let y_plane = &planes[0];
    let co_plane = &planes[1];
    let cg_plane = &planes[2];
    let a_plane = &planes[3];

    for y in 0..height {
        let dst_y = y_start + y;
        if dst_y >= surf_h {
            continue;
        }

        let (y_row, co_row, cg_row) = if chroma_subsampling != 0 {
            let y_start_idx = y * rw;
            let co_offset = (y >> 1) * (rw >> 1);
            let sub_len = rw >> 1;
            (
                &y_plane[y_start_idx..y_start_idx + rw],
                &co_plane[co_offset..co_offset + sub_len],
                &cg_plane[co_offset..co_offset + sub_len],
            )
        } else {
            let row_start = y * width;
            (
                &y_plane[row_start..row_start + width],
                &co_plane[row_start..row_start + width],
                &cg_plane[row_start..row_start + width],
            )
        };
        let a_row = &a_plane[y * width..(y + 1) * width];

        for x in 0..width {
            let dst_x = x_start + x;
            if dst_x >= surf_w {
                continue;
            }

            let y_val = y_row[x] as i16;
            let (co_idx, cg_idx) = if chroma_subsampling != 0 {
                (x >> 1, x >> 1)
            } else {
                (x, x)
            };

            let co_val = ((co_row[co_idx] << shift) as i8) as i16;
            let cg_val = ((cg_row[cg_idx] << shift) as i8) as i16;

            let r_val = y_val + co_val - cg_val;
            let g_val = y_val + cg_val;
            let b_val = y_val - co_val - cg_val;

            let dst_idx = (dst_y * surf_w + dst_x) * 4;
            if dst_idx + 4 <= out.len() {
                out[dst_idx] = b_val.clamp(0, 255) as u8;
                out[dst_idx + 1] = g_val.clamp(0, 255) as u8;
                out[dst_idx + 2] = r_val.clamp(0, 255) as u8;
                out[dst_idx + 3] = a_row[x];
            }
        }
    }

    true
}

pub fn nsc_rle_decode(input: &[u8], out: &mut [u8], original_size: usize) -> bool {
    let mut left = original_size;
    let mut in_pos = 0;
    let mut out_pos = 0;

    while left > 4 {
        if in_pos >= input.len() {
            return false;
        }
        let val = input[in_pos];
        in_pos += 1;

        if left == 5 {
            if out_pos >= out.len() {
                return false;
            }
            out[out_pos] = val;
            out_pos += 1;
            left -= 1;
        } else if in_pos >= input.len() {
            return false;
        } else if val == input[in_pos] {
            in_pos += 1;
            if in_pos >= input.len() {
                return false;
            }
            let run_len = if input[in_pos] < 0xFF {
                let r = input[in_pos] as usize + 2;
                in_pos += 1;
                r
            } else {
                if in_pos + 5 > input.len() {
                    return false;
                }
                in_pos += 1; // skip 0xFF
                let r = u32::from_le_bytes([
                    input[in_pos],
                    input[in_pos + 1],
                    input[in_pos + 2],
                    input[in_pos + 3],
                ]) as usize;
                in_pos += 4;
                r
            };

            if out_pos + run_len > out.len() || left < run_len {
                return false;
            }
            out[out_pos..out_pos + run_len].fill(val);
            out_pos += run_len;
            left -= run_len;
        } else {
            if out_pos >= out.len() {
                return false;
            }
            out[out_pos] = val;
            out_pos += 1;
            left -= 1;
        }
    }

    if out_pos + 4 > out.len() || left < 4 || in_pos + 4 > input.len() {
        return false;
    }
    out[out_pos..out_pos + 4].copy_from_slice(&input[in_pos..in_pos + 4]);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nscodec_decode() {
        let w = 8;
        let h = 4;

        let rw = (w + 7) & !7;
        let rh = (h + 1) & !1;

        let y_len = rw * h;
        let co_len = (rw / 2) * (rh / 2);
        let cg_len = (rw / 2) * (rh / 2);
        let a_len = w * h;

        let y_plane = vec![150u8; y_len];
        let co_plane = vec![156u8; co_len]; // 256 - 100
        let cg_plane = vec![0u8; cg_len];
        let a_plane = vec![255u8; a_len];

        let mut payload = vec![0u8; 20];
        payload[0..4].copy_from_slice(&(y_len as u32).to_le_bytes());
        payload[4..8].copy_from_slice(&(co_len as u32).to_le_bytes());
        payload[8..12].copy_from_slice(&(cg_len as u32).to_le_bytes());
        payload[12..16].copy_from_slice(&(a_len as u32).to_le_bytes());
        payload[16] = 1;
        payload[17] = 1;

        payload.extend_from_slice(&y_plane);
        payload.extend_from_slice(&co_plane);
        payload.extend_from_slice(&cg_plane);
        payload.extend_from_slice(&a_plane);

        let mut out = vec![0u8; w * h * 4];
        let ok = decode_nscodec(&payload, w, h, &mut out, 0, 0, w, h);
        assert!(ok);

        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) * 4;
                let b = out[idx];
                let g = out[idx + 1];
                let r = out[idx + 2];
                let a = out[idx + 3];
                assert_eq!((b, g, r, a), (250, 150, 50, 255));
            }
        }
    }

    #[test]
    fn test_nscodec_rle() {
        let encoded = [5u8, 5, 4, 1, 2, 3, 4];
        let mut out = vec![0u8; 10];
        let ok = nsc_rle_decode(&encoded, &mut out, 10);
        assert!(ok);
        assert_eq!(&out, &[5, 5, 5, 5, 5, 5, 1, 2, 3, 4]);
    }
}
