/// RemoteFX Progressive Codec decoder (MS-RDPRFX / MS-RDPEGFX 2.2.4).
/// Handles RDPGFX_CODECID_CAPROGRESSIVE (0x0009).

use std::collections::HashMap;
use super::rfx::{
    parse_rfx_quant, rfx_inverse_dwt_2d, rfx_place_tile_abs, rfx_shift_slice, RfxQuant,
    RFX_TILE_SIZE,
};
use super::rfx_rlgr::rlgr1_decode;

const PROG_WBT_SYNC: u16 = 0xCCC0;
const PROG_WBT_FRAME_BEGIN: u16 = 0xCCC1;
const PROG_WBT_FRAME_END: u16 = 0xCCC2;
const PROG_WBT_CONTEXT: u16 = 0xCCC3;
const PROG_WBT_REGION: u16 = 0xCCC4;
const PROG_WBT_TILE_SIMPLE: u16 = 0xCCC5;
const PROG_WBT_TILE_FIRST: u16 = 0xCCC6;
const PROG_WBT_TILE_UPGRADE: u16 = 0xCCC7;

#[derive(Clone, Copy, Debug, Default)]
pub struct RfxProgQuant {
    pub quality: u8,
    pub y_quant: RfxQuant,
    pub cb_quant: RfxQuant,
    pub cr_quant: RfxQuant,
}

#[derive(Clone, Default)]
pub struct RfxTileCoeffs {
    pub y: Vec<i16>,
    pub cb: Vec<i16>,
    pub cr: Vec<i16>,
    pub sign_y: Vec<i16>,
    pub sign_cb: Vec<i16>,
    pub sign_cr: Vec<i16>,
    pub y_bit_pos: RfxQuant,
    pub cb_bit_pos: RfxQuant,
    pub cr_bit_pos: RfxQuant,
}

pub struct RfxProgressiveDecoder {
    tile_cache: HashMap<u32, RfxTileCoeffs>,
    quants: Vec<RfxQuant>,
    prog_quants: Vec<RfxProgQuant>,
}

impl Default for RfxProgressiveDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl RfxProgressiveDecoder {
    pub fn new() -> Self {
        RfxProgressiveDecoder {
            tile_cache: HashMap::new(),
            quants: Vec::new(),
            prog_quants: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.tile_cache.clear();
        self.quants.clear();
        self.prog_quants.clear();
    }

    pub fn decode(
        &mut self,
        data: &[u8],
        left: usize,
        top: usize,
        surf_data: &mut [u8],
        surf_w: usize,
        surf_h: usize,
    ) -> Vec<(usize, usize, usize, usize)> {
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 6 <= data.len() {
            let block_type = u16::from_le_bytes([data[offset], data[offset + 1]]);
            let block_len = u32::from_le_bytes([
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
                data[offset + 5],
            ]) as usize;

            if block_len < 6 || offset + block_len > data.len() {
                break;
            }

            let content = &data[offset + 6..offset + block_len];

            match block_type {
                PROG_WBT_SYNC | PROG_WBT_FRAME_BEGIN | PROG_WBT_FRAME_END => {}
                PROG_WBT_CONTEXT => {
                    self.parse_context(content);
                }
                PROG_WBT_REGION => {
                    let region_rects = self.parse_region(content, left, top, surf_data, surf_w, surf_h);
                    rects.extend(region_rects);
                }
                PROG_WBT_TILE_SIMPLE => {
                    if let Some(rect) = self.decode_tile_simple(content, left, top, surf_data, surf_w, surf_h, false) {
                        rects.push(rect);
                    }
                }
                PROG_WBT_TILE_FIRST => {
                    if let Some(rect) = self.decode_tile_first(content, left, top, surf_data, surf_w, surf_h, false) {
                        rects.push(rect);
                    }
                }
                PROG_WBT_TILE_UPGRADE => {
                    if let Some(rect) = self.decode_tile_upgrade(content, left, top, surf_data, surf_w, surf_h, false) {
                        rects.push(rect);
                    }
                }
                _ => {}
            }

            offset += block_len;
        }
        rects
    }

    fn parse_region(
        &mut self,
        data: &[u8],
        left: usize,
        top: usize,
        surf_data: &mut [u8],
        surf_w: usize,
        surf_h: usize,
    ) -> Vec<(usize, usize, usize, usize)> {
        if data.len() < 12 {
            return Vec::new();
        }

        let num_rects = u16::from_le_bytes([data[1], data[2]]) as usize;
        let num_quant = data[3] as usize;
        let num_prog_quant = data[4] as usize;
        let region_flags = data[5];
        let extrapolate = (region_flags & 0x01) != 0;

        log::debug!(
            "[rfx_prog] parse_region: num_rects={} num_quant={} num_prog_quant={} extrapolate={} left={} top={}",
            num_rects, num_quant, num_prog_quant, extrapolate, left, top
        );

        let mut offset = 12;

        let mut rects = Vec::with_capacity(num_rects);
        for _ in 0..num_rects {
            if offset + 8 > data.len() {
                return rects;
            }
            let rx = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
            let ry = u16::from_le_bytes([data[offset + 2], data[offset + 3]]) as usize;
            let rw = u16::from_le_bytes([data[offset + 4], data[offset + 5]]) as usize;
            let rh = u16::from_le_bytes([data[offset + 6], data[offset + 7]]) as usize;
            let abs_x = left + rx;
            let abs_y = top + ry;
            if abs_x < surf_w && abs_y < surf_h {
                let w = rw.min(surf_w - abs_x);
                let h = rh.min(surf_h - abs_y);
                if w > 0 && h > 0 {
                    rects.push((abs_x, abs_y, w, h));
                }
            }
            offset += 8;
        }

        if num_quant > 0 {
            self.quants.clear();
            for _ in 0..num_quant {
                if offset + 5 > data.len() {
                    break;
                }
                self.quants.push(parse_rfx_quant(&data[offset..offset + 5]));
                offset += 5;
            }
        }

        if num_prog_quant > 0 {
            self.prog_quants.clear();
            for _ in 0..num_prog_quant {
                if offset + 16 > data.len() {
                    break;
                }
                let quality = data[offset];
                let y_quant = parse_rfx_quant(&data[offset + 1..offset + 6]);
                let cb_quant = parse_rfx_quant(&data[offset + 6..offset + 11]);
                let cr_quant = parse_rfx_quant(&data[offset + 11..offset + 16]);
                self.prog_quants.push(RfxProgQuant {
                    quality,
                    y_quant,
                    cb_quant,
                    cr_quant,
                });
                offset += 16;
            }
        }

        let mut decoded_tile_rects = Vec::new();
        while offset + 6 <= data.len() {
            let tile_type = u16::from_le_bytes([data[offset], data[offset + 1]]);
            let tile_len = u32::from_le_bytes([
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
                data[offset + 5],
            ]) as usize;

            if tile_len < 6 || offset + tile_len > data.len() {
                break;
            }

            let tile_data = &data[offset + 6..offset + tile_len];
            match tile_type {
                PROG_WBT_TILE_SIMPLE => {
                    log::debug!("[rfx_prog] TILE_SIMPLE len={}", tile_len);
                    if let Some(rect) = self.decode_tile_simple(tile_data, left, top, surf_data, surf_w, surf_h, extrapolate) {
                        decoded_tile_rects.push(rect);
                    }
                }
                PROG_WBT_TILE_FIRST => {
                    log::debug!("[rfx_prog] TILE_FIRST len={}", tile_len);
                    if let Some(rect) = self.decode_tile_first(tile_data, left, top, surf_data, surf_w, surf_h, extrapolate) {
                        decoded_tile_rects.push(rect);
                    }
                }
                PROG_WBT_TILE_UPGRADE => {
                    log::debug!("[rfx_prog] TILE_UPGRADE len={}", tile_len);
                    if let Some(rect) = self.decode_tile_upgrade(tile_data, left, top, surf_data, surf_w, surf_h, extrapolate) {
                        decoded_tile_rects.push(rect);
                    }
                }
                _ => {
                    log::debug!("[rfx_prog] unknown tile_type 0x{:04X} len={}", tile_type, tile_len);
                }
            }
            offset += tile_len;
        }

        if rects.is_empty() {
            decoded_tile_rects
        } else {
            rects
        }
    }

    fn parse_context(&mut self, data: &[u8]) {
        if data.len() < 4 {
            return;
        }
        // ctx_id = data[0], flags = data[1], tileSize = data[2], numProgQuant = data[3]
        let num_prog_quant = data[3] as usize;
        let mut off = 4;
        if num_prog_quant > 0 {
            self.prog_quants.clear();
            for _ in 0..num_prog_quant {
                if off + 16 > data.len() {
                    break;
                }
                let quality = data[off];
                let y_quant = parse_rfx_quant(&data[off + 1..off + 6]);
                let cb_quant = parse_rfx_quant(&data[off + 6..off + 11]);
                let cr_quant = parse_rfx_quant(&data[off + 11..off + 16]);
                self.prog_quants.push(RfxProgQuant {
                    quality,
                    y_quant,
                    cb_quant,
                    cr_quant,
                });
                off += 16;
            }
        }
    }

    fn decode_tile_simple(
        &mut self,
        data: &[u8],
        left: usize,
        top: usize,
        surf_data: &mut [u8],
        surf_w: usize,
        surf_h: usize,
        extrapolate: bool,
    ) -> Option<(usize, usize, usize, usize)> {
        if data.len() < 16 {
            return None;
        }

        let q_idx_y = data[0] as usize;
        let q_idx_cb = data[1] as usize;
        let q_idx_cr = data[2] as usize;
        let x_idx = u16::from_le_bytes([data[3], data[4]]) as usize;
        let y_idx = u16::from_le_bytes([data[5], data[6]]) as usize;
        let flags = data[7];
        let y_len = u16::from_le_bytes([data[8], data[9]]) as usize;
        let cb_len = u16::from_le_bytes([data[10], data[11]]) as usize;
        let cr_len = u16::from_le_bytes([data[12], data[13]]) as usize;

        let is_diff = (flags & 0x01) != 0;

        let mut off = 16;
        if off + y_len + cb_len + cr_len > data.len() {
            return None;
        }

        let y_data = &data[off..off + y_len];
        off += y_len;
        let cb_data = &data[off..off + cb_len];
        off += cb_len;
        let cr_data = &data[off..off + cr_len];

        let q_y = self.quants.get(q_idx_y).copied().unwrap_or_default();
        let q_cb = self.quants.get(q_idx_cb).copied().unwrap_or_default();
        let q_cr = self.quants.get(q_idx_cr).copied().unwrap_or_default();

        let shift_y = prog_quant_shift(q_y, RfxQuant::default(), false);
        let shift_cb = prog_quant_shift(q_cb, RfxQuant::default(), false);
        let shift_cr = prog_quant_shift(q_cr, RfxQuant::default(), false);

        let tile_key = ((y_idx as u32) << 16) | (x_idx as u32);
        let prev_coeffs = if is_diff {
            self.tile_cache.get(&tile_key).cloned()
        } else {
            None
        };

        let (y_pix, y_deq, y_sgn) = decode_comp_prog(
            y_data,
            shift_y,
            prev_coeffs.as_ref().map(|c| &c.y[..]),
            is_diff,
            extrapolate,
        );
        let (cb_pix, cb_deq, cb_sgn) = decode_comp_prog(
            cb_data,
            shift_cb,
            prev_coeffs.as_ref().map(|c| &c.cb[..]),
            is_diff,
            extrapolate,
        );
        let (cr_pix, cr_deq, cr_sgn) = decode_comp_prog(
            cr_data,
            shift_cr,
            prev_coeffs.as_ref().map(|c| &c.cr[..]),
            is_diff,
            extrapolate,
        );

        let abs_x = left + x_idx * RFX_TILE_SIZE;
        let abs_y = top + y_idx * RFX_TILE_SIZE;
        rfx_place_tile_abs(
            &y_pix, &cb_pix, &cr_pix, abs_x, abs_y, surf_data, surf_w, surf_h,
        );

        self.tile_cache.insert(
            tile_key,
            RfxTileCoeffs {
                y: y_deq,
                cb: cb_deq,
                cr: cr_deq,
                sign_y: y_sgn,
                sign_cb: cb_sgn,
                sign_cr: cr_sgn,
                y_bit_pos: q_y,
                cb_bit_pos: q_cb,
                cr_bit_pos: q_cr,
            },
        );

        if abs_x < surf_w && abs_y < surf_h {
            let w = RFX_TILE_SIZE.min(surf_w - abs_x);
            let h = RFX_TILE_SIZE.min(surf_h - abs_y);
            Some((abs_x, abs_y, w, h))
        } else {
            None
        }
    }

    fn decode_tile_first(
        &mut self,
        data: &[u8],
        left: usize,
        top: usize,
        surf_data: &mut [u8],
        surf_w: usize,
        surf_h: usize,
        extrapolate: bool,
    ) -> Option<(usize, usize, usize, usize)> {
        if data.len() < 17 {
            return None;
        }

        let q_idx_y = data[0] as usize;
        let q_idx_cb = data[1] as usize;
        let q_idx_cr = data[2] as usize;
        let x_idx = u16::from_le_bytes([data[3], data[4]]) as usize;
        let y_idx = u16::from_le_bytes([data[5], data[6]]) as usize;
        let flags = data[7];
        let quality = data[8] as usize;
        let y_len = u16::from_le_bytes([data[9], data[10]]) as usize;
        let cb_len = u16::from_le_bytes([data[11], data[12]]) as usize;
        let cr_len = u16::from_le_bytes([data[13], data[14]]) as usize;

        let is_diff = (flags & 0x01) != 0;

        let mut off = 17;
        if off + y_len + cb_len + cr_len > data.len() {
            return None;
        }

        let y_data = &data[off..off + y_len];
        off += y_len;
        let cb_data = &data[off..off + cb_len];
        off += cb_len;
        let cr_data = &data[off..off + cr_len];

        let q_y = self.quants.get(q_idx_y).copied().unwrap_or_default();
        let q_cb = self.quants.get(q_idx_cb).copied().unwrap_or_default();
        let q_cr = self.quants.get(q_idx_cr).copied().unwrap_or_default();

        let has_pq = quality < self.prog_quants.len();
        let (pq_y, pq_cb, pq_cr) = if has_pq {
            let pq = &self.prog_quants[quality];
            (pq.y_quant, pq.cb_quant, pq.cr_quant)
        } else {
            (RfxQuant::default(), RfxQuant::default(), RfxQuant::default())
        };

        let shift_y = prog_quant_shift(q_y, pq_y, has_pq);
        let shift_cb = prog_quant_shift(q_cb, pq_cb, has_pq);
        let shift_cr = prog_quant_shift(q_cr, pq_cr, has_pq);

        let bit_pos_y = quant_add(q_y, pq_y);
        let bit_pos_cb = quant_add(q_cb, pq_cb);
        let bit_pos_cr = quant_add(q_cr, pq_cr);

        let tile_key = ((y_idx as u32) << 16) | (x_idx as u32);
        let prev_coeffs = if is_diff {
            self.tile_cache.get(&tile_key).cloned()
        } else {
            None
        };

        let (y_pix, y_deq, y_sgn) = decode_comp_prog(
            y_data,
            shift_y,
            prev_coeffs.as_ref().map(|c| &c.y[..]),
            is_diff,
            extrapolate,
        );
        let (cb_pix, cb_deq, cb_sgn) = decode_comp_prog(
            cb_data,
            shift_cb,
            prev_coeffs.as_ref().map(|c| &c.cb[..]),
            is_diff,
            extrapolate,
        );
        let (cr_pix, cr_deq, cr_sgn) = decode_comp_prog(
            cr_data,
            shift_cr,
            prev_coeffs.as_ref().map(|c| &c.cr[..]),
            is_diff,
            extrapolate,
        );

        let abs_x = left + x_idx * RFX_TILE_SIZE;
        let abs_y = top + y_idx * RFX_TILE_SIZE;
        rfx_place_tile_abs(
            &y_pix, &cb_pix, &cr_pix, abs_x, abs_y, surf_data, surf_w, surf_h,
        );

        self.tile_cache.insert(
            tile_key,
            RfxTileCoeffs {
                y: y_deq,
                cb: cb_deq,
                cr: cr_deq,
                sign_y: y_sgn,
                sign_cb: cb_sgn,
                sign_cr: cr_sgn,
                y_bit_pos: bit_pos_y,
                cb_bit_pos: bit_pos_cb,
                cr_bit_pos: bit_pos_cr,
            },
        );

        if abs_x < surf_w && abs_y < surf_h {
            let w = RFX_TILE_SIZE.min(surf_w - abs_x);
            let h = RFX_TILE_SIZE.min(surf_h - abs_y);
            Some((abs_x, abs_y, w, h))
        } else {
            None
        }
    }

    fn decode_tile_upgrade(
        &mut self,
        data: &[u8],
        left: usize,
        top: usize,
        surf_data: &mut [u8],
        surf_w: usize,
        surf_h: usize,
        extrapolate: bool,
    ) -> Option<(usize, usize, usize, usize)> {
        if data.len() < 20 {
            return None;
        }

        let q_idx_y = data[0] as usize;
        let q_idx_cb = data[1] as usize;
        let q_idx_cr = data[2] as usize;
        let x_idx = u16::from_le_bytes([data[3], data[4]]) as usize;
        let y_idx = u16::from_le_bytes([data[5], data[6]]) as usize;
        let quality = data[7] as usize;
        let y_srl_len = u16::from_le_bytes([data[8], data[9]]) as usize;
        let y_raw_len = u16::from_le_bytes([data[10], data[11]]) as usize;
        let cb_srl_len = u16::from_le_bytes([data[12], data[13]]) as usize;
        let cb_raw_len = u16::from_le_bytes([data[14], data[15]]) as usize;
        let cr_srl_len = u16::from_le_bytes([data[16], data[17]]) as usize;
        let cr_raw_len = u16::from_le_bytes([data[18], data[19]]) as usize;

        let mut off = 20;
        let y_srl = &data[off..off + y_srl_len]; off += y_srl_len;
        let y_raw = &data[off..off + y_raw_len]; off += y_raw_len;
        let cb_srl = &data[off..off + cb_srl_len]; off += cb_srl_len;
        let cb_raw = &data[off..off + cb_raw_len]; off += cb_raw_len;
        let cr_srl = &data[off..off + cr_srl_len]; off += cr_srl_len;
        let cr_raw = &data[off..off + cr_raw_len];

        let q_y = self.quants.get(q_idx_y).copied().unwrap_or_default();
        let q_cb = self.quants.get(q_idx_cb).copied().unwrap_or_default();
        let q_cr = self.quants.get(q_idx_cr).copied().unwrap_or_default();

        let (pq_y, pq_cb, pq_cr) = if let Some(pq) = self.prog_quants.get(quality) {
            (pq.y_quant, pq.cb_quant, pq.cr_quant)
        } else {
            (RfxQuant::default(), RfxQuant::default(), RfxQuant::default())
        };

        let tile_key = ((y_idx as u32) << 16) | (x_idx as u32);
        let cached = match self.tile_cache.get_mut(&tile_key) {
            Some(c) => c,
            None => return None,
        };

        let new_y_bit_pos = quant_add(q_y, pq_y);
        let new_cb_bit_pos = quant_add(q_cb, pq_cb);
        let new_cr_bit_pos = quant_add(q_cr, pq_cr);

        let num_bits_y = quant_sub(cached.y_bit_pos, new_y_bit_pos);
        let num_bits_cb = quant_sub(cached.cb_bit_pos, new_cb_bit_pos);
        let num_bits_cr = quant_sub(cached.cr_bit_pos, new_cr_bit_pos);

        let shift_y = prog_quant_shift(new_y_bit_pos, RfxQuant::default(), false);
        let shift_cb = prog_quant_shift(new_cb_bit_pos, RfxQuant::default(), false);
        let shift_cr = prog_quant_shift(new_cr_bit_pos, RfxQuant::default(), false);

        cached.y_bit_pos = new_y_bit_pos;
        cached.cb_bit_pos = new_cb_bit_pos;
        cached.cr_bit_pos = new_cr_bit_pos;

        let y_pix = upgrade_component(y_srl, y_raw, shift_y, num_bits_y, &mut cached.y, &mut cached.sign_y, extrapolate);
        let cb_pix = upgrade_component(cb_srl, cb_raw, shift_cb, num_bits_cb, &mut cached.cb, &mut cached.sign_cb, extrapolate);
        let cr_pix = upgrade_component(cr_srl, cr_raw, shift_cr, num_bits_cr, &mut cached.cr, &mut cached.sign_cr, extrapolate);

        let abs_x = left + x_idx * RFX_TILE_SIZE;
        let abs_y = top + y_idx * RFX_TILE_SIZE;
        rfx_place_tile_abs(
            &y_pix, &cb_pix, &cr_pix, abs_x, abs_y, surf_data, surf_w, surf_h,
        );

        if abs_x < surf_w && abs_y < surf_h {
            let w = RFX_TILE_SIZE.min(surf_w - abs_x);
            let h = RFX_TILE_SIZE.min(surf_h - abs_y);
            Some((abs_x, abs_y, w, h))
        } else {
            None
        }
    }
}

fn rfx_shift_val(v: u8) -> u8 {
    if v > 0 {
        v - 1
    } else {
        0
    }
}

fn prog_quant_shift(q: RfxQuant, pq: RfxQuant, has_pq: bool) -> RfxQuant {
    if !has_pq {
        RfxQuant {
            ll3: rfx_shift_val(q.ll3),
            lh3: rfx_shift_val(q.lh3),
            hl3: rfx_shift_val(q.hl3),
            hh3: rfx_shift_val(q.hh3),
            lh2: rfx_shift_val(q.lh2),
            hl2: rfx_shift_val(q.hl2),
            hh2: rfx_shift_val(q.hh2),
            lh1: rfx_shift_val(q.lh1),
            hl1: rfx_shift_val(q.hl1),
            hh1: rfx_shift_val(q.hh1),
        }
    } else {
        RfxQuant {
            ll3: rfx_shift_val(q.ll3 + pq.ll3),
            lh3: rfx_shift_val(q.lh3 + pq.lh3),
            hl3: rfx_shift_val(q.hl3 + pq.hl3),
            hh3: rfx_shift_val(q.hh3 + pq.hh3),
            lh2: rfx_shift_val(q.lh2 + pq.lh2),
            hl2: rfx_shift_val(q.hl2 + pq.hl2),
            hh2: rfx_shift_val(q.hh2 + pq.hh2),
            lh1: rfx_shift_val(q.lh1 + pq.lh1),
            hl1: rfx_shift_val(q.hl1 + pq.hl1),
            hh1: rfx_shift_val(q.hh1 + pq.hh1),
        }
    }
}

fn quant_add(q1: RfxQuant, q2: RfxQuant) -> RfxQuant {
    RfxQuant {
        ll3: q1.ll3 + q2.ll3,
        lh3: q1.lh3 + q2.lh3,
        hl3: q1.hl3 + q2.hl3,
        hh3: q1.hh3 + q2.hh3,
        lh2: q1.lh2 + q2.lh2,
        hl2: q1.hl2 + q2.hl2,
        hh2: q1.hh2 + q2.hh2,
        lh1: q1.lh1 + q2.lh1,
        hl1: q1.hl1 + q2.hl1,
        hh1: q1.hh1 + q2.hh1,
    }
}

fn quant_sub(q1: RfxQuant, q2: RfxQuant) -> RfxQuant {
    RfxQuant {
        ll3: q1.ll3.saturating_sub(q2.ll3),
        lh3: q1.lh3.saturating_sub(q2.lh3),
        hl3: q1.hl3.saturating_sub(q2.hl3),
        hh3: q1.hh3.saturating_sub(q2.hh3),
        lh2: q1.lh2.saturating_sub(q2.lh2),
        hl2: q1.hl2.saturating_sub(q2.hl2),
        hh2: q1.hh2.saturating_sub(q2.hh2),
        lh1: q1.lh1.saturating_sub(q2.lh1),
        hl1: q1.hl1.saturating_sub(q2.hl1),
        hh1: q1.hh1.saturating_sub(q2.hh1),
    }
}

fn decode_comp_prog(
    data: &[u8],
    shift: RfxQuant,
    prev: Option<&[i16]>,
    is_diff: bool,
    extrapolate: bool,
) -> (Vec<i16>, Vec<i16>, Vec<i16>) {
    let mut work = vec![0i16; 4096];
    let mut raw_sign = vec![0i16; 4096];

    if !data.is_empty() {
        rlgr1_decode(data, &mut work);
        raw_sign.copy_from_slice(&work);
    }

    if !extrapolate {
        // Differential decode LL3 (64 elements starting at offset 4032)
        for i in 4033..4096 {
            work[i] = work[i].wrapping_add(work[i - 1]);
        }

        // Dequantize all 10 subbands with progressive shift
        rfx_shift_slice(&mut work[0..1024], shift.hl1);
        rfx_shift_slice(&mut work[1024..2048], shift.lh1);
        rfx_shift_slice(&mut work[2048..3072], shift.hh1);
        rfx_shift_slice(&mut work[3072..3328], shift.hl2);
        rfx_shift_slice(&mut work[3328..3584], shift.lh2);
        rfx_shift_slice(&mut work[3584..3840], shift.hh2);
        rfx_shift_slice(&mut work[3840..3904], shift.hl3);
        rfx_shift_slice(&mut work[3904..3968], shift.lh3);
        rfx_shift_slice(&mut work[3968..4032], shift.hh3);
        rfx_shift_slice(&mut work[4032..4096], shift.ll3);
    } else {
        // Differential decode LL3 band (81 elements starting at offset 4015).
        for i in 4016..4096 {
            work[i] = work[i].wrapping_add(work[i - 1]);
        }

        // Dequantize all 10 subbands with progressive shift
        rfx_shift_slice(&mut work[0..1023], shift.hl1);
        rfx_shift_slice(&mut work[1023..2046], shift.lh1);
        rfx_shift_slice(&mut work[2046..3007], shift.hh1);
        rfx_shift_slice(&mut work[3007..3279], shift.hl2);
        rfx_shift_slice(&mut work[3279..3551], shift.lh2);
        rfx_shift_slice(&mut work[3551..3807], shift.hh2);
        rfx_shift_slice(&mut work[3807..3879], shift.hl3);
        rfx_shift_slice(&mut work[3879..3951], shift.lh3);
        rfx_shift_slice(&mut work[3951..4015], shift.hh3);
        rfx_shift_slice(&mut work[4015..4096], shift.ll3);
    }

    if is_diff {
        if let Some(p) = prev {
            for i in 0..4096 {
                work[i] = (work[i] as i32 + p[i] as i32).clamp(-32768, 32767) as i16;
            }
        }
    }

    let dequant = work.clone();
    if !extrapolate {
        rfx_inverse_dwt_2d(&mut work);
    } else {
        rfx_inverse_dwt_extrapolate(&mut work);
    }

    (work, dequant, raw_sign)
}

struct RfxBitStream<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bits: u64,
    bit_len: usize,
}

impl<'a> RfxBitStream<'a> {
    fn new(data: &'a [u8]) -> Self {
        let mut bs = RfxBitStream {
            data,
            byte_pos: 0,
            bits: 0,
            bit_len: 0,
        };
        bs.fill();
        bs
    }

    fn fill(&mut self) {
        while self.bit_len <= 56 && self.byte_pos < self.data.len() {
            self.bits |= (self.data[self.byte_pos] as u64) << (56 - self.bit_len);
            self.byte_pos += 1;
            self.bit_len += 8;
        }
    }

    fn read_bit(&mut self) -> u32 {
        if self.bit_len == 0 {
            self.fill();
            if self.bit_len == 0 {
                return 0;
            }
        }
        let bit = (self.bits >> 63) as u32;
        self.bits <<= 1;
        self.bit_len -= 1;
        bit
    }

    fn read_bits(&mut self, n: usize) -> u32 {
        if n == 0 {
            return 0;
        }
        if self.bit_len < n {
            self.fill();
            if self.bit_len < n {
                let actual_n = self.bit_len;
                if actual_n == 0 {
                    return 0;
                }
                let val = (self.bits >> (64 - actual_n)) as u32;
                self.bits = 0;
                self.bit_len = 0;
                return val;
            }
        }
        let val = (self.bits >> (64 - n)) as u32;
        self.bits <<= n;
        self.bit_len -= n;
        val
    }
}

struct SrlState<'a> {
    srl: RfxBitStream<'a>,
    raw: RfxBitStream<'a>,
    kp: u32,
    nz: usize,
    mode: bool,
}

impl<'a> SrlState<'a> {
    fn new(srl_data: &'a [u8], raw_data: &'a [u8]) -> Self {
        SrlState {
            srl: RfxBitStream::new(srl_data),
            raw: RfxBitStream::new(raw_data),
            kp: 8,
            nz: 0,
            mode: false,
        }
    }

    fn read_srl(&mut self, num_bits: u32) -> i16 {
        if self.nz > 0 {
            self.nz -= 1;
            return 0;
        }
        let k = (self.kp / 8) as usize;
        if !self.mode {
            let bit = self.srl.read_bit();
            if bit == 0 {
                self.nz = (1 << k) - 1;
                self.kp = (self.kp + 4).min(80);
                return 0;
            } else {
                self.nz = 0;
                self.mode = true;
                if k > 0 {
                    self.nz = self.srl.read_bits(k) as usize;
                }
                if self.nz > 0 {
                    self.nz -= 1;
                    return 0;
                }
            }
        }
        self.mode = false;
        let sign = self.srl.read_bit();
        self.kp = self.kp.saturating_sub(6);

        if num_bits == 1 {
            return if sign != 0 { -1 } else { 1 };
        }

        let mut mag = 1u32;
        let max_val = (1u32 << num_bits) - 1;
        while mag < max_val {
            let bit = self.srl.read_bit();
            if bit != 0 {
                break;
            }
            mag += 1;
        }

        if sign != 0 {
            -(mag as i16)
        } else {
            mag as i16
        }
    }

    fn upgrade_block(&mut self, cur: &mut [i16], sgn: &mut [i16], shift: u8, num_bits: u8, is_ll: bool) {
        if num_bits == 0 {
            return;
        }
        let len = cur.len().min(sgn.len());
        if is_ll {
            for i in 0..len {
                let input = self.raw.read_bits(num_bits as usize) as i32;
                cur[i] = (cur[i] as i32 + (input << shift)).clamp(-32768, 32767) as i16;
            }
        } else {
            for i in 0..len {
                let input = if sgn[i] > 0 {
                    self.raw.read_bits(num_bits as usize) as i32
                } else if sgn[i] < 0 {
                    -(self.raw.read_bits(num_bits as usize) as i32)
                } else {
                    let srl_val = self.read_srl(num_bits as u32);
                    sgn[i] = srl_val;
                    srl_val as i32
                };
                cur[i] = (cur[i] as i32 + (input << shift)).clamp(-32768, 32767) as i16;
            }
        }
    }
}

fn upgrade_component(
    srl_data: &[u8],
    raw_data: &[u8],
    shift: RfxQuant,
    num_bits: RfxQuant,
    cur: &mut [i16],
    sgn: &mut [i16],
    extrapolate: bool,
) -> Vec<i16> {
    let mut state = SrlState::new(srl_data, raw_data);

    if !extrapolate {
        state.upgrade_block(&mut cur[0..1024], &mut sgn[0..1024], shift.hl1, num_bits.hl1, false);
        state.upgrade_block(&mut cur[1024..2048], &mut sgn[1024..2048], shift.lh1, num_bits.lh1, false);
        state.upgrade_block(&mut cur[2048..3072], &mut sgn[2048..3072], shift.hh1, num_bits.hh1, false);
        state.upgrade_block(&mut cur[3072..3328], &mut sgn[3072..3328], shift.hl2, num_bits.hl2, false);
        state.upgrade_block(&mut cur[3328..3584], &mut sgn[3328..3584], shift.lh2, num_bits.lh2, false);
        state.upgrade_block(&mut cur[3584..3840], &mut sgn[3584..3840], shift.hh2, num_bits.hh2, false);
        state.upgrade_block(&mut cur[3840..3904], &mut sgn[3840..3904], shift.hl3, num_bits.hl3, false);
        state.upgrade_block(&mut cur[3904..3968], &mut sgn[3904..3968], shift.lh3, num_bits.lh3, false);
        state.upgrade_block(&mut cur[3968..4032], &mut sgn[3968..4032], shift.hh3, num_bits.hh3, false);
        state.upgrade_block(&mut cur[4032..4096], &mut sgn[4032..4096], shift.ll3, num_bits.ll3, true);
    } else {
        state.upgrade_block(&mut cur[0..1023], &mut sgn[0..1023], shift.hl1, num_bits.hl1, false);
        state.upgrade_block(&mut cur[1023..2046], &mut sgn[1023..2046], shift.lh1, num_bits.lh1, false);
        state.upgrade_block(&mut cur[2046..3007], &mut sgn[2046..3007], shift.hh1, num_bits.hh1, false);
        state.upgrade_block(&mut cur[3007..3279], &mut sgn[3007..3279], shift.hl2, num_bits.hl2, false);
        state.upgrade_block(&mut cur[3279..3551], &mut sgn[3279..3551], shift.lh2, num_bits.lh2, false);
        state.upgrade_block(&mut cur[3551..3807], &mut sgn[3551..3807], shift.hh2, num_bits.hh2, false);
        state.upgrade_block(&mut cur[3807..3879], &mut sgn[3807..3879], shift.hl3, num_bits.hl3, false);
        state.upgrade_block(&mut cur[3879..3951], &mut sgn[3879..3951], shift.lh3, num_bits.lh3, false);
        state.upgrade_block(&mut cur[3951..4015], &mut sgn[3951..4015], shift.hh3, num_bits.hh3, false);
        state.upgrade_block(&mut cur[4015..4096], &mut sgn[4015..4096], shift.ll3, num_bits.ll3, true);
    }

    let mut work = cur.to_vec();
    if !extrapolate {
        rfx_inverse_dwt_2d(&mut work);
    } else {
        rfx_inverse_dwt_extrapolate(&mut work);
    }
    work
}

#[inline(always)]
fn clampi16(val: i32) -> i16 {
    val.clamp(-32768, 32767) as i16
}

fn rfx_idwt_x(
    p_low: &[i16],
    p_high: &[i16],
    p_dst: &mut [i16],
    n_low_step: usize,
    n_high_step: usize,
    n_dst_step: usize,
    n_low_count: usize,
    n_high_count: usize,
    n_dst_count: usize,
) {
    for i in 0..n_dst_count {
        let mut p_l = i * n_low_step;
        let mut p_h = i * n_high_step;
        let mut p_x = i * n_dst_step;

        let mut h0 = p_high[p_h] as i32;
        p_h += 1;
        let mut l0 = p_low[p_l] as i32;
        p_l += 1;

        let mut x0 = clampi16(l0 - h0);
        let mut x2 = x0;

        for _ in 0..n_high_count.saturating_sub(1) {
            let h1 = p_high[p_h] as i32;
            p_h += 1;
            l0 = p_low[p_l] as i32;
            p_l += 1;
            x2 = clampi16(l0 - ((h0 + h1) / 2));
            let x1 = clampi16(((x0 as i32 + x2 as i32) / 2) + (2 * h0));
            p_dst[p_x] = x0;
            p_dst[p_x + 1] = x1;
            p_x += 2;
            x0 = x2;
            h0 = h1;
        }

        if n_low_count <= n_high_count + 1 {
            if n_low_count <= n_high_count {
                p_dst[p_x] = x2;
                p_dst[p_x + 1] = clampi16(x2 as i32 + 2 * h0);
            } else {
                l0 = p_low[p_l] as i32;
                x0 = clampi16(l0 - h0);
                p_dst[p_x] = x2;
                p_dst[p_x + 1] = clampi16(((x0 as i32 + x2 as i32) / 2) + 2 * h0);
                p_dst[p_x + 2] = x0;
            }
        } else {
            l0 = p_low[p_l] as i32;
            p_l += 1;
            x0 = clampi16(l0 - (h0 / 2));
            p_dst[p_x] = x2;
            p_dst[p_x + 1] = clampi16(((x0 as i32 + x2 as i32) / 2) + 2 * h0);
            p_dst[p_x + 2] = x0;
            l0 = p_low[p_l] as i32;
            p_dst[p_x + 3] = clampi16((x0 as i32 + l0) / 2);
        }
    }
}

fn rfx_idwt_y(
    p_low: &[i16],
    p_high: &[i16],
    p_dst: &mut [i16],
    n_low_step: usize,
    n_high_step: usize,
    n_dst_step: usize,
    n_low_count: usize,
    n_high_count: usize,
    n_dst_count: usize,
) {
    for i in 0..n_dst_count {
        let mut p_l = i;
        let mut p_h = i;
        let mut p_x = i;

        let mut h0 = p_high[p_h] as i32;
        p_h += n_high_step;
        let mut l0 = p_low[p_l] as i32;
        p_l += n_low_step;

        let mut x0 = clampi16(l0 - h0);
        let mut x2 = x0;

        for _ in 0..n_high_count.saturating_sub(1) {
            let h1 = p_high[p_h] as i32;
            p_h += n_high_step;
            l0 = p_low[p_l] as i32;
            p_l += n_low_step;
            x2 = clampi16(l0 - ((h0 + h1) / 2));
            let x1 = clampi16(((x0 as i32 + x2 as i32) / 2) + 2 * h0);
            p_dst[p_x] = x0;
            p_x += n_dst_step;
            p_dst[p_x] = x1;
            p_x += n_dst_step;
            x0 = x2;
            h0 = h1;
        }

        if n_low_count <= n_high_count + 1 {
            if n_low_count <= n_high_count {
                p_dst[p_x] = x2;
                p_x += n_dst_step;
                p_dst[p_x] = clampi16(x2 as i32 + 2 * h0);
            } else {
                l0 = p_low[p_l] as i32;
                x0 = clampi16(l0 - h0);
                p_dst[p_x] = x2;
                p_x += n_dst_step;
                p_dst[p_x] = clampi16(((x0 as i32 + x2 as i32) / 2) + 2 * h0);
                p_x += n_dst_step;
                p_dst[p_x] = x0;
            }
        } else {
            l0 = p_low[p_l] as i32;
            p_l += n_low_step;
            x0 = clampi16(l0 - (h0 / 2));
            p_dst[p_x] = x2;
            p_x += n_dst_step;
            p_dst[p_x] = clampi16(((x0 as i32 + x2 as i32) / 2) + 2 * h0);
            p_x += n_dst_step;
            p_dst[p_x] = x0;
            p_x += n_dst_step;
            l0 = p_low[p_l] as i32;
            p_dst[p_x] = clampi16((x0 as i32 + l0) / 2);
        }
    }
}

fn rfx_dwt_2d_decode_block(buffer: &mut [i16], temp: &mut [i16], level: usize) {
    let n_band_l = (64 >> level) + 1;
    let n_band_h = if level == 1 {
        (64 >> 1) - 1
    } else {
        (64 + (1 << (level - 1))) >> level
    };

    let mut offset = 0;
    let hl_end = offset + n_band_h * n_band_l;
    let hl = buffer[offset..hl_end].to_vec();
    offset = hl_end;

    let lh_end = offset + n_band_l * n_band_h;
    let lh = buffer[offset..lh_end].to_vec();
    offset = lh_end;

    let hh_end = offset + n_band_h * n_band_h;
    let hh = buffer[offset..hh_end].to_vec();
    offset = hh_end;

    let ll_end = offset + n_band_l * n_band_l;
    let ll = buffer[offset..ll_end].to_vec();

    let n_dst_step = n_band_l + n_band_h;

    let l_end = n_band_l * n_dst_step;
    let h_end = (n_band_l + n_band_h) * n_dst_step;

    // horizontal (LL + HL -> L)
    rfx_idwt_x(&ll, &hl, &mut temp[0..l_end], n_band_l, n_band_h, n_dst_step, n_band_l, n_band_h, n_band_l);

    // horizontal (LH + HH -> H)
    rfx_idwt_x(&lh, &hh, &mut temp[l_end..h_end], n_band_l, n_band_h, n_dst_step, n_band_l, n_band_h, n_band_h);

    // vertical (L + H -> buffer)
    let (temp_l, temp_h) = temp.split_at(l_end);
    rfx_idwt_y(
        &temp_l[0..l_end],
        &temp_h[0..h_end - l_end],
        &mut buffer[0..n_dst_step * n_dst_step],
        n_dst_step,
        n_dst_step,
        n_dst_step,
        n_band_l,
        n_band_h,
        n_band_l + n_band_h,
    );
}

pub fn rfx_inverse_dwt_extrapolate(buffer: &mut [i16]) {
    let mut temp = vec![0i16; 4096];
    rfx_dwt_2d_decode_block(&mut buffer[3807..], &mut temp, 3);
    rfx_dwt_2d_decode_block(&mut buffer[3007..], &mut temp, 2);
    rfx_dwt_2d_decode_block(&mut buffer[0..], &mut temp, 1);
}

