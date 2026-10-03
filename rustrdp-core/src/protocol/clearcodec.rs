/// ClearCodec decoder (MS-RDPEGFX 2.2.4.1 / 3.3.8.1).
use super::nscodec::decode_nscodec;
use std::collections::HashMap;

pub const CLEARCODEC_FLAG_GLYPH_INDEX: u8 = 0x01;
pub const CLEARCODEC_FLAG_GLYPH_HIT: u8 = 0x02;
pub const CLEARCODEC_FLAG_CACHE_RESET: u8 = 0x04;

pub const CLEARCODEC_VBAR_CACHE_SIZE: usize = 32768;
pub const CLEARCODEC_SHORT_VBAR_CACHE_SIZE: usize = 16384;

static CLEAR_LOG2_FLOOR: [usize; 256] = [
    0, 0, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
    5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
    6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
    6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
];

#[derive(Clone, Default)]
pub struct VBarEntry {
    pub pixels: Vec<u32>, // BGRA 0xAARRGGBB in u32
}

pub struct ClearCodecContext {
    pub vbar_storage: Vec<VBarEntry>,
    pub short_vbar_storage: Vec<VBarEntry>,
    pub glyph_storage: HashMap<u16, Vec<u8>>,
    pub vbar_cursor: usize,
    pub short_vbar_cursor: usize,
}

impl Default for ClearCodecContext {
    fn default() -> Self {
        Self::new()
    }
}

impl ClearCodecContext {
    pub fn new() -> Self {
        ClearCodecContext {
            vbar_storage: vec![VBarEntry::default(); CLEARCODEC_VBAR_CACHE_SIZE],
            short_vbar_storage: vec![VBarEntry::default(); CLEARCODEC_SHORT_VBAR_CACHE_SIZE],
            glyph_storage: HashMap::new(),
            vbar_cursor: 0,
            short_vbar_cursor: 0,
        }
    }

    pub fn reset_cache(&mut self) {
        self.vbar_storage.fill(VBarEntry::default());
        self.short_vbar_storage.fill(VBarEntry::default());
        self.glyph_storage.clear();
        self.vbar_cursor = 0;
        self.short_vbar_cursor = 0;
    }

    pub fn reset_vbar(&mut self) {
        self.vbar_storage.fill(VBarEntry::default());
        self.short_vbar_storage.fill(VBarEntry::default());
        self.vbar_cursor = 0;
        self.short_vbar_cursor = 0;
    }

    pub fn decode(
        &mut self,
        data: &[u8],
        width: usize,
        height: usize,
        out: &mut [u8],
        _x_start: usize,
        _y_start: usize,
        _surf_w: usize,
        _surf_h: usize,
    ) -> bool {
        if data.len() < 2 || width == 0 || height == 0 {
            return false;
        }

        let flags = data[0];
        let _seq_num = data[1];

        if (flags & CLEARCODEC_FLAG_CACHE_RESET) != 0 {
            self.reset_vbar();
        }

        let mut offset = 2;
        let mut glyph_index: u16 = 0;
        let mut has_glyph_index = false;

        if (flags & (CLEARCODEC_FLAG_GLYPH_INDEX | CLEARCODEC_FLAG_GLYPH_HIT)) != 0 {
            if offset + 2 <= data.len() {
                glyph_index = u16::from_le_bytes([data[offset], data[offset + 1]]);
                has_glyph_index = true;
                offset += 2;
            }
        }

        if (flags & CLEARCODEC_FLAG_GLYPH_HIT) != 0 {
            if has_glyph_index {
                if let Some(cached) = self.glyph_storage.get(&glyph_index) {
                    if cached.len() == width * height * 4 && out.len() >= cached.len() {
                        out[..cached.len()].copy_from_slice(cached);
                        return true;
                    }
                }
            }
            return false;
        }

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

        log::debug!(
            "[clearcodec] decode {}x{} flags=0x{:02X} res_len={} bands_len={} subcodecs_len={}",
            width, height, flags, residual_len, bands_len, subcodecs_len
        );

        if offset + residual_len > data.len() {
            return false;
        }
        if residual_len > 0 {
            decode_residual(&data[offset..offset + residual_len], width, height, out);
        }
        offset += residual_len;

        if offset + bands_len > data.len() {
            return false;
        }
        if bands_len > 0 {
            self.decode_bands(&data[offset..offset + bands_len], width, height, out);
        }
        offset += bands_len;

        if offset + subcodecs_len > data.len() {
            return false;
        }
        if subcodecs_len > 0 {
            self.decode_subcodecs(&data[offset..offset + subcodecs_len], width, height, out);
        }

        if (flags & CLEARCODEC_FLAG_GLYPH_INDEX) != 0 && has_glyph_index {
            let needed = width * height * 4;
            if out.len() >= needed {
                self.glyph_storage.insert(glyph_index, out[..needed].to_vec());
            }
        }

        true
    }

    fn decode_bands(
        &mut self,
        data: &[u8],
        surf_w: usize,
        surf_h: usize,
        out: &mut [u8],
    ) {
        let mut off = 0;
        while off + 11 <= data.len() {
            let x_start = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
            let x_end = u16::from_le_bytes([data[off + 2], data[off + 3]]) as usize;
            let y_start = u16::from_le_bytes([data[off + 4], data[off + 5]]) as usize;
            let y_end = u16::from_le_bytes([data[off + 6], data[off + 7]]) as usize;
            let blue_bg = data[off + 8];
            let green_bg = data[off + 9];
            let red_bg = data[off + 10];
            off += 11;

            if x_end < x_start || y_end < y_start {
                continue;
            }
            let band_height = y_end - y_start + 1;
            let vbar_count = x_end - x_start + 1;
            let color_bkg = (blue_bg as u32)
                | ((green_bg as u32) << 8)
                | ((red_bg as u32) << 16)
                | (0xFFu32 << 24);

            for i in 0..vbar_count {
                if off + 2 > data.len() {
                    return;
                }
                let vbar_header = u16::from_le_bytes([data[off], data[off + 1]]);
                off += 2;

                let mut cur_vbar: Option<VBarEntry> = None;
                let mut vbar_short_entry: Option<VBarEntry> = None;
                let mut vbar_update = false;
                let mut vbar_y_on = 0usize;
                let mut vbar_short_pixel_count = 0usize;

                if (vbar_header & 0xC000) == 0x4000 {
                    // SHORT_VBAR_CACHE_HIT
                    let vbar_index = (vbar_header & 0x3FFF) as usize;
                    if vbar_index < self.short_vbar_storage.len() {
                        vbar_short_entry = Some(self.short_vbar_storage[vbar_index].clone());
                    }
                    if off >= data.len() {
                        return;
                    }
                    vbar_y_on = data[off] as usize;
                    off += 1;
                    vbar_short_pixel_count = vbar_short_entry.as_ref().map_or(0, |e| e.pixels.len());
                    vbar_update = true;
                } else if (vbar_header & 0xC000) == 0x0000 {
                    // SHORT_VBAR_CACHE_MISS
                    vbar_y_on = (vbar_header & 0xFF) as usize;
                    let vbar_y_off = ((vbar_header >> 8) & 0x3F) as usize;
                    if vbar_y_off >= vbar_y_on {
                        vbar_short_pixel_count = vbar_y_off - vbar_y_on;
                    }
                    if off + vbar_short_pixel_count * 3 > data.len() {
                        return;
                    }
                    let mut short_pixels = vec![0u32; vbar_short_pixel_count];
                    for p in 0..vbar_short_pixel_count {
                        let b = data[off];
                        let g = data[off + 1];
                        let r = data[off + 2];
                        off += 3;
                        short_pixels[p] = (b as u32) | ((g as u32) << 8) | ((r as u32) << 16) | (0xFFu32 << 24);
                    }
                    let entry = VBarEntry { pixels: short_pixels };
                    vbar_short_entry = Some(entry.clone());
                    let cursor = self.short_vbar_cursor;
                    self.short_vbar_storage[cursor] = entry;
                    self.short_vbar_cursor = (cursor + 1) % self.short_vbar_storage.len();
                    vbar_update = true;
                } else if (vbar_header & 0x8000) == 0x8000 {
                    // VBAR_CACHE_HIT
                    let vbar_index = (vbar_header & 0x7FFF) as usize;
                    if vbar_index < self.vbar_storage.len() {
                        cur_vbar = Some(self.vbar_storage[vbar_index].clone());
                    }
                    if cur_vbar.as_ref().map_or(true, |v| v.pixels.is_empty()) {
                        cur_vbar = Some(VBarEntry {
                            pixels: vec![color_bkg; band_height],
                        });
                    }
                } else {
                    return;
                }

                if vbar_update {
                    let mut vbar_pixels = vec![color_bkg; band_height];
                    if let Some(short_entry) = vbar_short_entry {
                        for y in 0..band_height {
                            if y >= vbar_y_on && y < vbar_y_on + vbar_short_pixel_count {
                                if let Some(&px) = short_entry.pixels.get(y - vbar_y_on) {
                                    vbar_pixels[y] = px;
                                }
                            }
                        }
                    }
                    let entry = VBarEntry { pixels: vbar_pixels };
                    let cursor = self.vbar_cursor;
                    self.vbar_storage[cursor] = entry.clone();
                    self.vbar_cursor = (cursor + 1) % self.vbar_storage.len();
                    cur_vbar = Some(entry);
                }

                // Render column x = x_start + i
                if let Some(vbar) = cur_vbar {
                    let dst_x = x_start + i;
                    if dst_x < surf_w {
                        for (y, &pixel) in vbar.pixels.iter().enumerate() {
                            let dst_y = y_start + y;
                            if dst_y < surf_h {
                                let dst_idx = (dst_y * surf_w + dst_x) * 4;
                                if dst_idx + 4 <= out.len() {
                                    out[dst_idx..dst_idx + 4].copy_from_slice(&pixel.to_le_bytes());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn decode_subcodecs(
        &mut self,
        data: &[u8],
        surf_w: usize,
        surf_h: usize,
        out: &mut [u8],
    ) {
        let mut off = 0;
        while off + 13 <= data.len() {
            let x_start = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
            let y_start = u16::from_le_bytes([data[off + 2], data[off + 3]]) as usize;
            let width = u16::from_le_bytes([data[off + 4], data[off + 5]]) as usize;
            let height = u16::from_le_bytes([data[off + 6], data[off + 7]]) as usize;
            let bmp_len = u32::from_le_bytes([
                data[off + 8],
                data[off + 9],
                data[off + 10],
                data[off + 11],
            ]) as usize;
            let subcodec_id = data[off + 12];
            log::debug!(
                "[clearcodec] subcodec id={} rect=({},{} {}x{}) bmp_len={}",
                subcodec_id, x_start, y_start, width, height, bmp_len
            );
            off += 13;

            if off + bmp_len > data.len() {
                break;
            }
            let bmp_data = &data[off..off + bmp_len];
            off += bmp_len;

            if subcodec_id == 0 {
                // Uncompressed BGR24
                let mut src_off = 0;
                for y in 0..height {
                    let dst_y = y_start + y;
                    for x in 0..width {
                        if src_off + 3 > bmp_data.len() {
                            break;
                        }
                        let b = bmp_data[src_off];
                        let g = bmp_data[src_off + 1];
                        let r = bmp_data[src_off + 2];
                        src_off += 3;
                        let dst_x = x_start + x;
                        if dst_x < surf_w && dst_y < surf_h {
                            let dst_idx = (dst_y * surf_w + dst_x) * 4;
                            if dst_idx + 4 <= out.len() {
                                out[dst_idx] = b;
                                out[dst_idx + 1] = g;
                                out[dst_idx + 2] = r;
                                out[dst_idx + 3] = 0xFF;
                            }
                        }
                    }
                }
            } else if subcodec_id == 1 {
                // NSCodec
                decode_nscodec(bmp_data, width, height, out, x_start, y_start, surf_w, surf_h);
            } else if subcodec_id == 2 {
                // RLEX
                decode_rlex(bmp_data, width, height, out, x_start, y_start, surf_w, surf_h);
            }
        }
    }
}

fn decode_residual(
    data: &[u8],
    w: usize,
    h: usize,
    out: &mut [u8],
) {
    let total_pixels = w * h;
    let mut pixel_index = 0usize;
    let mut off = 0usize;

    while off + 4 <= data.len() && pixel_index < total_pixels {
        let b = data[off];
        let g = data[off + 1];
        let r = data[off + 2];
        let mut run_len = data[off + 3] as usize;
        off += 4;

        if run_len >= 0xFF {
            if off + 2 > data.len() {
                break;
            }
            run_len = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
            off += 2;
            if run_len >= 0xFFFF {
                if off + 4 > data.len() {
                    break;
                }
                run_len = u32::from_le_bytes([
                    data[off],
                    data[off + 1],
                    data[off + 2],
                    data[off + 3],
                ]) as usize;
                off += 4;
            }
        }

        let pixel_u32 = (b as u32) | ((g as u32) << 8) | ((r as u32) << 16) | (0xFFu32 << 24);
        let end = (pixel_index + run_len).min(total_pixels);
        for p in pixel_index..end {
            let px = p % w;
            let py = p / w;
            let dst_idx = (py * w + px) * 4;
            if dst_idx + 4 <= out.len() {
                out[dst_idx..dst_idx + 4].copy_from_slice(&pixel_u32.to_le_bytes());
            }
        }
        pixel_index = end;
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
) {
    if data.is_empty() {
        return;
    }
    let palette_count = data[0] as usize;
    if palette_count < 1 || palette_count > 127 || 1 + palette_count * 3 > data.len() {
        return;
    }
    let mut palette = vec![0u32; palette_count];
    let mut p_off = 1;
    for p in 0..palette_count {
        let b = data[p_off];
        let g = data[p_off + 1];
        let r = data[p_off + 2];
        p_off += 3;
        palette[p] = (b as u32) | ((g as u32) << 8) | ((r as u32) << 16) | (0xFFu32 << 24);
    }

    let num_bits = CLEAR_LOG2_FLOOR[palette_count - 1] + 1;
    let depth_mask = (1usize << (8 - num_bits)) - 1;
    let stop_mask = (1usize << num_bits) - 1;

    let pixel_count = width * height;
    let mut pixel_index = 0usize;
    let mut x = 0usize;
    let mut y = 0usize;

    log::debug!(
        "[clearcodec] decode_rlex: {}x{} (total={}) palette_count={} num_bits={} data_len={}",
        width, height, pixel_count, palette_count, num_bits, data.len()
    );

    while p_off < data.len() && pixel_index < pixel_count {
        if p_off + 2 > data.len() {
            break;
        }
        let tmp = data[p_off] as usize;
        let mut run_length_factor = data[p_off + 1] as usize;
        p_off += 2;

        let suite_depth = (tmp >> num_bits) & depth_mask;
        let stop_index = tmp & stop_mask;
        if stop_index < suite_depth {
            break;
        }
        let start_index = stop_index - suite_depth;

        if run_length_factor >= 0xFF {
            if p_off + 2 > data.len() {
                break;
            }
            run_length_factor = u16::from_le_bytes([data[p_off], data[p_off + 1]]) as usize;
            p_off += 2;
            if run_length_factor >= 0xFFFF {
                if p_off + 4 > data.len() {
                    break;
                }
                run_length_factor = u32::from_le_bytes([
                    data[p_off],
                    data[p_off + 1],
                    data[p_off + 2],
                    data[p_off + 3],
                ]) as usize;
                p_off += 4;
            }
        }

        if start_index >= palette_count || stop_index >= palette_count {
            break;
        }

        let color = palette[start_index];
        for _ in 0..run_length_factor {
            if pixel_index >= pixel_count {
                break;
            }
            let dst_x = x_start + x;
            let dst_y = y_start + y;
            if dst_x < surf_w && dst_y < surf_h {
                let dst_idx = (dst_y * surf_w + dst_x) * 4;
                if dst_idx + 4 <= out.len() {
                    out[dst_idx..dst_idx + 4].copy_from_slice(&color.to_le_bytes());
                }
            }
            x += 1;
            if x >= width {
                x = 0;
                y += 1;
            }
            pixel_index += 1;
        }

        for s in 0..=suite_depth {
            if pixel_index >= pixel_count {
                break;
            }
            let idx = start_index + s;
            if idx < palette_count {
                let ccolor = palette[idx];
                let dst_x = x_start + x;
                let dst_y = y_start + y;
                if dst_x < surf_w && dst_y < surf_h {
                    let dst_idx = (dst_y * surf_w + dst_x) * 4;
                    if dst_idx + 4 <= out.len() {
                        out[dst_idx..dst_idx + 4].copy_from_slice(&ccolor.to_le_bytes());
                    }
                }
            }
            x += 1;
            if x >= width {
                x = 0;
                y += 1;
            }
            pixel_index += 1;
        }
    }

    log::debug!(
        "[clearcodec] decode_rlex done: {} / {} pixels",
        pixel_index, pixel_count
    );
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

    #[test]
    fn test_clearcodec_subcodec_rlex() {
        let mut ctx = ClearCodecContext::new();
        let w = 4;
        let h = 2;

        let bmp_data = vec![
            2, // paletteCount
            100, 150, 200, // color 0 (B,G,R)
            50, 60, 70, // color 1 (B,G,R)
            0, 3, // tmp=0, runLen=3
            1, 3, // tmp=1, runLen=3
        ];

        let mut subcodec_payload = vec![0u8; 13 + bmp_data.len()];
        subcodec_payload[0..2].copy_from_slice(&0u16.to_le_bytes()); // xStart
        subcodec_payload[2..4].copy_from_slice(&0u16.to_le_bytes()); // yStart
        subcodec_payload[4..6].copy_from_slice(&(w as u16).to_le_bytes());
        subcodec_payload[6..8].copy_from_slice(&(h as u16).to_le_bytes());
        subcodec_payload[8..12].copy_from_slice(&(bmp_data.len() as u32).to_le_bytes());
        subcodec_payload[12] = 2; // RLEX
        subcodec_payload[13..].copy_from_slice(&bmp_data);

        let mut payload = vec![0u8; 14 + subcodec_payload.len()];
        payload[0] = 0;
        payload[1] = 0;
        payload[2..6].copy_from_slice(&0u32.to_le_bytes()); // residualLen
        payload[6..10].copy_from_slice(&0u32.to_le_bytes()); // bandsLen
        payload[10..14].copy_from_slice(&(subcodec_payload.len() as u32).to_le_bytes()); // subcodecLen
        payload[14..].copy_from_slice(&subcodec_payload);

        let mut out = vec![0u8; w * h * 4];
        let ok = ctx.decode(&payload, w, h, &mut out, 0, 0, w, h);
        assert!(ok);

        for i in 0..4 {
            let idx = i * 4;
            assert_eq!((out[idx], out[idx + 1], out[idx + 2]), (100, 150, 200));
        }
        for i in 4..8 {
            let idx = i * 4;
            assert_eq!((out[idx], out[idx + 1], out[idx + 2]), (50, 60, 70));
        }
    }
}
