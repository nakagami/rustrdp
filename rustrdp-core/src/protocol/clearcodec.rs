/// ClearCodec decoder (MS-RDPEGFX 2.2.4.1 / 3.3.8.1).
use super::nscodec::decode_nscodec;

pub const CLEARCODEC_FLAG_GLYPH_INDEX: u8 = 0x01;
pub const CLEARCODEC_FLAG_GLYPH_HIT: u8 = 0x02;
pub const CLEARCODEC_FLAG_CACHE_RESET: u8 = 0x04;

pub const CLEARCODEC_VBAR_CACHE_SIZE: usize = 32768;

#[derive(Clone)]
pub struct VBarEntry {
    pub pixels: Vec<u8>, // BGRA 4 bytes per pixel
    pub height: usize,
}

pub struct ClearCodecContext {
    pub vbar_cache: Vec<Option<VBarEntry>>,
    pub vbar_cache_index: usize,
}

impl Default for ClearCodecContext {
    fn default() -> Self {
        Self::new()
    }
}

impl ClearCodecContext {
    pub fn new() -> Self {
        ClearCodecContext {
            vbar_cache: vec![None; CLEARCODEC_VBAR_CACHE_SIZE],
            vbar_cache_index: 0,
        }
    }

    pub fn reset_cache(&mut self) {
        self.vbar_cache.fill(None);
        self.vbar_cache_index = 0;
    }

    pub fn decode(
        &mut self,
        data: &[u8],
        width: usize,
        height: usize,
        out: &mut [u8],
        x_start: usize,
        y_start: usize,
        surf_w: usize,
        surf_h: usize,
    ) -> bool {
        if data.len() < 2 || width == 0 || height == 0 {
            return false;
        }

        let flags = data[0];
        let seq_num = data[1];
        let _ = seq_num;

        if (flags & CLEARCODEC_FLAG_CACHE_RESET) != 0 {
            self.reset_cache();
        }

        let mut offset = 2;

        if (flags & CLEARCODEC_FLAG_GLYPH_HIT) != 0 {
            if offset + 2 > data.len() {
                return false;
            }
            let glyph_idx = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
            if glyph_idx >= CLEARCODEC_VBAR_CACHE_SIZE {
                return false;
            }
            if let Some(entry) = &self.vbar_cache[glyph_idx] {
                if entry.height != height || entry.pixels.len() < width * height * 4 {
                    return false;
                }
                for y in 0..height {
                    let dst_y = y_start + y;
                    if dst_y >= surf_h {
                        continue;
                    }
                    for x in 0..width {
                        let dst_x = x_start + x;
                        if dst_x >= surf_w {
                            continue;
                        }
                        let src_i = (y * width + x) * 4;
                        let dst_i = (dst_y * surf_w + dst_x) * 4;
                        if dst_i + 4 <= out.len() && src_i + 4 <= entry.pixels.len() {
                            out[dst_i..dst_i + 4].copy_from_slice(&entry.pixels[src_i..src_i + 4]);
                        }
                    }
                }
                return true;
            } else {
                return false;
            }
        }

        // Subcodec residual, bands, and subcodecs
        if offset >= data.len() {
            return true;
        }

        // Decode Residual data if present
        // Read subcodecs
        // Structure:
        // [ResidualLen (4)] [BandsLen (4)] [SubcodecsLen (4)]
        if offset + 12 > data.len() {
            return false;
        }

        let residual_len = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        let bands_len = u32::from_le_bytes([
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]) as usize;
        let subcodecs_len = u32::from_le_bytes([
            data[offset + 8],
            data[offset + 9],
            data[offset + 10],
            data[offset + 11],
        ]) as usize;
        offset += 12;

        if offset + residual_len + bands_len + subcodecs_len > data.len() {
            return false;
        }

        let residual_data = &data[offset..offset + residual_len];
        offset += residual_len;
        let bands_data = &data[offset..offset + bands_len];
        offset += bands_len;
        let subcodecs_data = &data[offset..offset + subcodecs_len];

        // 1. Decode Residual Data if any
        if residual_len > 0 {
            self.decode_residual(residual_data, width, height, out, x_start, y_start, surf_w, surf_h);
        }

        // 2. Decode Bands Data if any
        if bands_len > 0 {
            self.decode_bands(bands_data, width, height, out, x_start, y_start, surf_w, surf_h);
        }

        // 3. Decode Subcodecs Data if any
        if subcodecs_len > 0 {
            self.decode_subcodecs(subcodecs_data, out, x_start, y_start, surf_w, surf_h);
        }

        // If CLEARCODEC_FLAG_GLYPH_INDEX is set, store into vbar_cache
        if (flags & CLEARCODEC_FLAG_GLYPH_INDEX) != 0 {
            let mut entry_pixels = vec![0u8; width * height * 4];
            for y in 0..height {
                let dst_y = y_start + y;
                if dst_y >= surf_h {
                    continue;
                }
                for x in 0..width {
                    let dst_x = x_start + x;
                    if dst_x >= surf_w {
                        continue;
                    }
                    let src_i = (dst_y * surf_w + dst_x) * 4;
                    let dst_i = (y * width + x) * 4;
                    if src_i + 4 <= out.len() && dst_i + 4 <= entry_pixels.len() {
                        entry_pixels[dst_i..dst_i + 4].copy_from_slice(&out[src_i..src_i + 4]);
                    }
                }
            }
            let idx = self.vbar_cache_index;
            self.vbar_cache[idx] = Some(VBarEntry {
                pixels: entry_pixels,
                height,
            });
            self.vbar_cache_index = (self.vbar_cache_index + 1) % CLEARCODEC_VBAR_CACHE_SIZE;
        }

        true
    }

    fn decode_residual(
        &self,
        data: &[u8],
        width: usize,
        height: usize,
        out: &mut [u8],
        x_start: usize,
        y_start: usize,
        surf_w: usize,
        surf_h: usize,
    ) -> bool {
        let mut r_pos = 0;
        let mut x = 0;
        let mut y = 0;

        while r_pos < data.len() && y < height {
            let b = data[r_pos];
            let g = if r_pos + 1 < data.len() { data[r_pos + 1] } else { 0 };
            let r = if r_pos + 2 < data.len() { data[r_pos + 2] } else { 0 };
            r_pos += 3;

            let mut run_len = 1usize;
            if r_pos < data.len() {
                let count_byte = data[r_pos];
                if count_byte == 0xFF {
                    if r_pos + 3 <= data.len() {
                        run_len = u16::from_le_bytes([data[r_pos + 1], data[r_pos + 2]]) as usize;
                        r_pos += 3;
                    } else {
                        r_pos += 1;
                    }
                } else if count_byte >= 0x01 {
                    run_len = count_byte as usize;
                    r_pos += 1;
                }
            }

            for _ in 0..run_len {
                if y >= height {
                    break;
                }
                let dst_y = y_start + y;
                let dst_x = x_start + x;
                if dst_x < surf_w && dst_y < surf_h {
                    let idx = (dst_y * surf_w + dst_x) * 4;
                    if idx + 4 <= out.len() {
                        out[idx] = b;
                        out[idx + 1] = g;
                        out[idx + 2] = r;
                        out[idx + 3] = 0xFF;
                    }
                }
                x += 1;
                if x >= width {
                    x = 0;
                    y += 1;
                }
            }
        }
        true
    }

    fn decode_bands(
        &mut self,
        data: &[u8],
        width: usize,
        _height: usize,
        out: &mut [u8],
        x_start: usize,
        y_start: usize,
        surf_w: usize,
        surf_h: usize,
    ) -> bool {
        let mut off = 0;
        let mut cur_y = 0;

        while off < data.len() {
            if off + 2 > data.len() {
                break;
            }
            let band_height = data[off] as usize;
            let vbar_count = data[off + 1] as usize;
            off += 2;

            let mut cur_x = 0;
            for _ in 0..vbar_count {
                if off >= data.len() || cur_x >= width {
                    break;
                }
                let vbar_hdr = data[off];
                off += 1;

                let is_hit = (vbar_hdr & 0x80) != 0;
                let vbar_entry = if is_hit {
                    if off >= data.len() {
                        break;
                    }
                    let idx_low = (vbar_hdr & 0x7F) as usize;
                    let idx_high = data[off] as usize;
                    off += 1;
                    let cache_idx = (idx_high << 7) | idx_low;
                    if cache_idx < CLEARCODEC_VBAR_CACHE_SIZE {
                        self.vbar_cache[cache_idx].clone()
                    } else {
                        None
                    }
                } else {
                    let color_count = (vbar_hdr & 0x7F) as usize;
                    let mut pixels = vec![0u8; band_height * 4];
                    let mut p_off = 0;
                    for _ in 0..color_count {
                        if off + 3 > data.len() {
                            break;
                        }
                        let b = data[off];
                        let g = data[off + 1];
                        let r = data[off + 2];
                        off += 3;

                        let mut len = 1;
                        if off < data.len() && color_count > 1 {
                            len = data[off] as usize;
                            off += 1;
                        }
                        for _ in 0..len {
                            if p_off + 4 <= pixels.len() {
                                pixels[p_off] = b;
                                pixels[p_off + 1] = g;
                                pixels[p_off + 2] = r;
                                pixels[p_off + 3] = 0xFF;
                                p_off += 4;
                            }
                        }
                    }
                    let entry = VBarEntry {
                        pixels,
                        height: band_height,
                    };
                    let c_idx = self.vbar_cache_index;
                    self.vbar_cache[c_idx] = Some(entry.clone());
                    self.vbar_cache_index = (self.vbar_cache_index + 1) % CLEARCODEC_VBAR_CACHE_SIZE;
                    Some(entry)
                };

                if let Some(entry) = vbar_entry {
                    for y in 0..band_height.min(entry.height) {
                        let dst_y = y_start + cur_y + y;
                        let dst_x = x_start + cur_x;
                        if dst_y < surf_h && dst_x < surf_w {
                            let dst_idx = (dst_y * surf_w + dst_x) * 4;
                            let src_idx = y * 4;
                            if dst_idx + 4 <= out.len() && src_idx + 4 <= entry.pixels.len() {
                                out[dst_idx..dst_idx + 4].copy_from_slice(&entry.pixels[src_idx..src_idx + 4]);
                            }
                        }
                    }
                }
                cur_x += 1;
            }
            cur_y += band_height;
        }
        true
    }

    fn decode_subcodecs(
        &mut self,
        data: &[u8],
        out: &mut [u8],
        x_start: usize,
        y_start: usize,
        surf_w: usize,
        surf_h: usize,
    ) -> bool {
        let mut off = 0;
        while off < data.len() {
            if off + 13 > data.len() {
                break;
            }
            let sub_x = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
            let sub_y = u16::from_le_bytes([data[off + 2], data[off + 3]]) as usize;
            let sub_w = u16::from_le_bytes([data[off + 4], data[off + 5]]) as usize;
            let sub_h = u16::from_le_bytes([data[off + 6], data[off + 7]]) as usize;
            let sub_len = u32::from_le_bytes([
                data[off + 8],
                data[off + 9],
                data[off + 10],
                data[off + 11],
            ]) as usize;
            let subcodec_id = data[off + 12];
            off += 13;

            if off + sub_len > data.len() {
                break;
            }

            let sub_data = &data[off..off + sub_len];
            off += sub_len;

            let abs_x = x_start + sub_x;
            let abs_y = y_start + sub_y;

            match subcodec_id {
                0 => {
                    // Uncompressed bitmap (32bpp BGRA or 24bpp BGR)
                    if sub_data.len() >= sub_w * sub_h * 3 {
                        let is_32bpp = sub_data.len() >= sub_w * sub_h * 4;
                        let src_bpp = if is_32bpp { 4 } else { 3 };
                        for y in 0..sub_h {
                            let dst_y = abs_y + y;
                            if dst_y >= surf_h {
                                continue;
                            }
                            for x in 0..sub_w {
                                let dst_x = abs_x + x;
                                if dst_x >= surf_w {
                                    continue;
                                }
                                let src_i = (y * sub_w + x) * src_bpp;
                                let dst_i = (dst_y * surf_w + dst_x) * 4;
                                if dst_i + 4 <= out.len() && src_i + src_bpp <= sub_data.len() {
                                    out[dst_i] = sub_data[src_i];
                                    out[dst_i + 1] = sub_data[src_i + 1];
                                    out[dst_i + 2] = sub_data[src_i + 2];
                                    out[dst_i + 3] = if is_32bpp { sub_data[src_i + 3] } else { 0xFF };
                                }
                            }
                        }
                    }
                }
                1 => {
                    // NSCodec
                    decode_nscodec(sub_data, sub_w, sub_h, out, abs_x, abs_y, surf_w, surf_h);
                }
                2 => {
                    // CLEARCODEC_SUBCODEC_RLEX
                    decode_rlex(sub_data, sub_w, sub_h, out, abs_x, abs_y, surf_w, surf_h);
                }
                _ => {}
            }
        }
        true
    }
}

fn decode_rlex(
    data: &[u8],
    width: usize,
    height: usize,
    out: &mut [u8],
    x_start: usize,
    y_start: usize,
    surf_w: usize,
    surf_h: usize,
) -> bool {
    if data.len() < 1 {
        return false;
    }
    let palette_count = data[0] as usize;
    let mut off = 1;
    if off + palette_count * 3 > data.len() {
        return false;
    }

    let mut palette = Vec::with_capacity(palette_count);
    for _ in 0..palette_count {
        let b = data[off];
        let g = data[off + 1];
        let r = data[off + 2];
        palette.push((b, g, r));
        off += 3;
    }

    let mut cur_x = 0;
    let mut cur_y = 0;

    while off < data.len() && cur_y < height {
        let p_idx = data[off] as usize;
        off += 1;
        if p_idx >= palette.len() {
            break;
        }
        let (b, g, r) = palette[p_idx];

        let mut run_len = 1;
        if off < data.len() {
            let run_byte = data[off];
            if run_byte == 0xFF {
                if off + 3 <= data.len() {
                    run_len = u16::from_le_bytes([data[off + 1], data[off + 2]]) as usize;
                    off += 3;
                } else {
                    off += 1;
                }
            } else if run_byte >= 0x01 {
                run_len = run_byte as usize;
                off += 1;
            }
        }

        for _ in 0..run_len {
            if cur_y >= height {
                break;
            }
            let dst_y = y_start + cur_y;
            let dst_x = x_start + cur_x;
            if dst_y < surf_h && dst_x < surf_w {
                let idx = (dst_y * surf_w + dst_x) * 4;
                if idx + 4 <= out.len() {
                    out[idx] = b;
                    out[idx + 1] = g;
                    out[idx + 2] = r;
                    out[idx + 3] = 0xFF;
                }
            }
            cur_x += 1;
            if cur_x >= width {
                cur_x = 0;
                cur_y += 1;
            }
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clearcodec_basic() {
        let mut ctx = ClearCodecContext::new();
        let mut out = vec![0u8; 16 * 16 * 4];
        let mut data = vec![0u8; 14];
        data[0] = 0; // flags
        data[1] = 0; // seq
        // residual_len = 0, bands_len = 0, subcodecs_len = 0
        let ok = ctx.decode(&data, 16, 16, &mut out, 0, 0, 16, 16);
        assert!(ok);
    }
}
