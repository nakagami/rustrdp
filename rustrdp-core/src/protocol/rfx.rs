/// Non-progressive RemoteFX (RFX) codec decoder (MS-RDPRFX).
/// Used for RDPGFX_CODECID_CAVIDEO (0x0003).

use super::rfx_rlgr::{rlgr1_decode, rlgr3_decode};

pub const RFX_TILE_SIZE: usize = 64;

const WBT_SYNC: u16 = 0xCCC0;
const WBT_CODEC_VERSIONS: u16 = 0xCCC1;
const WBT_CHANNELS: u16 = 0xCCC2;
const WBT_CONTEXT: u16 = 0xCCC3;
const WBT_FRAME_BEGIN: u16 = 0xCCC4;
const WBT_FRAME_END: u16 = 0xCCC5;
const WBT_REGION: u16 = 0xCCC6;
const WBT_EXTENSION: u16 = 0xCCC7;

#[allow(dead_code)]
const CBT_REGION: u16 = 0xCAC1;
const CBT_TILESET: u16 = 0xCAC2;
const CBT_TILE: u16 = 0xCAC3;

#[derive(Clone, Copy, Debug, Default)]
pub struct RfxQuant {
    pub ll3: u8,
    pub lh3: u8,
    pub hl3: u8,
    pub hh3: u8,
    pub lh2: u8,
    pub hl2: u8,
    pub hh2: u8,
    pub lh1: u8,
    pub hl1: u8,
    pub hh1: u8,
}

pub fn parse_rfx_quant(data: &[u8]) -> RfxQuant {
    if data.len() < 5 {
        return RfxQuant::default();
    }
    RfxQuant {
        ll3: data[0] & 0x0F,
        lh3: (data[0] >> 4) & 0x0F,
        hl3: data[1] & 0x0F,
        hh3: (data[1] >> 4) & 0x0F,
        lh2: data[2] & 0x0F,
        hl2: (data[2] >> 4) & 0x0F,
        hh2: data[3] & 0x0F,
        lh1: (data[3] >> 4) & 0x0F,
        hl1: data[4] & 0x0F,
        hh1: (data[4] >> 4) & 0x0F,
    }
}

pub struct RfxDecoder {
    quants: Vec<RfxQuant>,
}

impl Default for RfxDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl RfxDecoder {
    pub fn new() -> Self {
        RfxDecoder {
            quants: Vec::new(),
        }
    }

    pub fn decode(
        &mut self,
        data: &[u8],
        left: usize,
        top: usize,
        surf_data: &mut [u8],
        surf_w: usize,
        surf_h: usize,
    ) {
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

            let header_len = if (WBT_CONTEXT..=WBT_EXTENSION).contains(&block_type) {
                8
            } else {
                6
            };

            if block_len < header_len {
                break;
            }

            let content = &data[offset + header_len..offset + block_len];

            match block_type {
                WBT_SYNC | WBT_CODEC_VERSIONS | WBT_CHANNELS | WBT_CONTEXT | WBT_FRAME_BEGIN
                | WBT_FRAME_END | WBT_REGION => {}
                WBT_EXTENSION => {
                    self.decode_tileset(content, left, top, surf_data, surf_w, surf_h);
                }
                _ => {}
            }

            offset += block_len;
        }
    }

    fn decode_tileset(
        &mut self,
        data: &[u8],
        left: usize,
        top: usize,
        surf_data: &mut [u8],
        surf_w: usize,
        surf_h: usize,
    ) {
        if data.len() < 14 {
            return;
        }

        let subtype = u16::from_le_bytes([data[0], data[1]]);
        if subtype != CBT_TILESET {
            return;
        }

        let properties = u16::from_le_bytes([data[4], data[5]]);
        let num_quant = data[6] as usize;
        let num_tiles = u16::from_le_bytes([data[8], data[9]]) as usize;

        let rlgr_mode = if ((properties >> 10) & 0x0F) == 0x04 { 3 } else { 1 };

        let mut off = 14;
        if off + num_quant * 5 > data.len() {
            return;
        }

        self.quants.clear();
        for _ in 0..num_quant {
            self.quants.push(parse_rfx_quant(&data[off..off + 5]));
            off += 5;
        }

        for _ in 0..num_tiles {
            if off + 6 > data.len() {
                break;
            }
            let tile_type = u16::from_le_bytes([data[off], data[off + 1]]);
            let tile_len = u32::from_le_bytes([
                data[off + 2],
                data[off + 3],
                data[off + 4],
                data[off + 5],
            ]) as usize;

            if tile_type != CBT_TILE || tile_len < 19 || off + tile_len > data.len() {
                break;
            }

            let tile_content = &data[off + 6..off + tile_len];
            self.decode_tile(tile_content, rlgr_mode, left, top, surf_data, surf_w, surf_h);
            off += tile_len;
        }
    }

    fn decode_tile(
        &self,
        data: &[u8],
        rlgr_mode: usize,
        left: usize,
        top: usize,
        surf_data: &mut [u8],
        surf_w: usize,
        surf_h: usize,
    ) {
        if data.len() < 13 {
            return;
        }

        let q_idx_y = data[0] as usize;
        let q_idx_cb = data[1] as usize;
        let q_idx_cr = data[2] as usize;
        let x_idx = u16::from_le_bytes([data[3], data[4]]) as usize;
        let y_idx = u16::from_le_bytes([data[5], data[6]]) as usize;
        let y_len = u16::from_le_bytes([data[7], data[8]]) as usize;
        let cb_len = u16::from_le_bytes([data[9], data[10]]) as usize;
        let cr_len = u16::from_le_bytes([data[11], data[12]]) as usize;

        let mut off = 13;
        if off + y_len + cb_len + cr_len > data.len() {
            return;
        }

        let y_data = &data[off..off + y_len];
        off += y_len;
        let cb_data = &data[off..off + cb_len];
        off += cb_len;
        let cr_data = &data[off..off + cr_len];

        let q_y = self.quants.get(q_idx_y).copied().unwrap_or_default();
        let q_cb = self.quants.get(q_idx_cb).copied().unwrap_or_default();
        let q_cr = self.quants.get(q_idx_cr).copied().unwrap_or_default();

        let mut y_coeffs = vec![0i16; 4096];
        let mut cb_coeffs = vec![0i16; 4096];
        let mut cr_coeffs = vec![0i16; 4096];

        rfx_decode_component(y_data, q_y, rlgr_mode, &mut y_coeffs);
        rfx_decode_component(cb_data, q_cb, rlgr_mode, &mut cb_coeffs);
        rfx_decode_component(cr_data, q_cr, rlgr_mode, &mut cr_coeffs);

        let abs_x = left + x_idx * RFX_TILE_SIZE;
        let abs_y = top + y_idx * RFX_TILE_SIZE;
        rfx_place_tile_abs(
            &y_coeffs,
            &cb_coeffs,
            &cr_coeffs,
            abs_x,
            abs_y,
            surf_data,
            surf_w,
            surf_h,
        );
    }
}

pub fn rfx_decode_component(data: &[u8], quant: RfxQuant, rlgr_mode: usize, work: &mut [i16]) {
    if rlgr_mode == 3 {
        rlgr3_decode(data, work);
    } else {
        rlgr1_decode(data, work);
    }

    // Differential decode LL3
    for i in 4033..4096 {
        work[i] = work[i].wrapping_add(work[i - 1]);
    }

    // Dequantize (shift left)
    rfx_shift_slice(&mut work[0..1024], quant.hl1);
    rfx_shift_slice(&mut work[1024..2048], quant.lh1);
    rfx_shift_slice(&mut work[2048..3072], quant.hh1);
    rfx_shift_slice(&mut work[3072..3328], quant.hl2);
    rfx_shift_slice(&mut work[3328..3584], quant.lh2);
    rfx_shift_slice(&mut work[3584..3840], quant.hh2);
    rfx_shift_slice(&mut work[3840..3904], quant.hl3);
    rfx_shift_slice(&mut work[3904..3968], quant.lh3);
    rfx_shift_slice(&mut work[3968..4032], quant.hh3);
    rfx_shift_slice(&mut work[4032..4096], quant.ll3);

    // Inverse 2D DWT
    rfx_inverse_dwt_2d(work);
}

pub fn rfx_shift_slice(data: &mut [i16], shift: u8) {
    if shift == 0 {
        return;
    }
    for v in data.iter_mut() {
        *v <<= shift;
    }
}

#[inline(always)]
fn clampi16(val: i32) -> i16 {
    val.clamp(-32768, 32767) as i16
}

pub fn rfx_idwt_subband(low: &[i16], high: &[i16], dst: &mut [i16], count: usize) {
    let half = count / 2;
    if half == 0 {
        return;
    }
    let mut h0 = high[0] as i32;
    let mut l0 = low[0] as i32;
    let mut x0 = clampi16(l0 - h0);
    let mut x2 = x0;

    for j in 0..(half - 1) {
        let h1 = high[j + 1] as i32;
        l0 = low[j + 1] as i32;
        x2 = clampi16(l0 - ((h0 + h1) / 2));
        let x1 = clampi16(((x0 as i32 + x2 as i32) / 2) + 2 * h0);
        dst[j * 2] = x0;
        dst[j * 2 + 1] = x1;
        x0 = x2;
        h0 = h1;
    }

    dst[(half - 1) * 2] = x2;
    dst[(half - 1) * 2 + 1] = clampi16(x2 as i32 + 2 * h0);
}

pub fn rfx_inverse_dwt_2d_level(buffer: &mut [i16], size: usize) {
    let half = size / 2;
    let quarter = half * half;
    let mut tmp = vec![0i16; size * size];

    // Subband regions in buffer:
    // HL: buffer[0..quarter]
    // LH: buffer[quarter..2*quarter]
    // HH: buffer[2*quarter..3*quarter]
    // LL: buffer[3*quarter..4*quarter]

    // Step 1: 1D IDWT vertically on (LL, LH) -> low_cols and (HL, HH) -> high_cols
    for col in 0..half {
        let mut low_col = vec![0i16; half];
        let mut high_col = vec![0i16; half];
        let mut dst_col_l = vec![0i16; size];
        let mut dst_col_h = vec![0i16; size];

        for r in 0..half {
            low_col[r] = buffer[3 * quarter + r * half + col];
            high_col[r] = buffer[quarter + r * half + col];
        }
        rfx_idwt_subband(&low_col, &high_col, &mut dst_col_l, size);

        for r in 0..half {
            low_col[r] = buffer[r * half + col];
            high_col[r] = buffer[2 * quarter + r * half + col];
        }
        rfx_idwt_subband(&low_col, &high_col, &mut dst_col_h, size);

        for r in 0..size {
            tmp[r * size + col] = dst_col_l[r];
            tmp[r * size + half + col] = dst_col_h[r];
        }
    }

    // Step 2: 1D IDWT horizontally on rows
    for r in 0..size {
        let (low_row, high_row) = tmp[r * size..(r + 1) * size].split_at(half);
        let mut dst_row = vec![0i16; size];
        rfx_idwt_subband(low_row, high_row, &mut dst_row, size);
        buffer[3 * quarter + r * size..3 * quarter + (r + 1) * size].copy_from_slice(&dst_row);
    }
}

pub fn rfx_inverse_dwt_2d(buffer: &mut [i16]) {
    // Level 3: 8x8 subbands inside 4096 (offset 4096 - 64*4 = 3840)
    let mut lev3_buf = vec![0i16; 256];
    lev3_buf.copy_from_slice(&buffer[3840..4096]);
    rfx_inverse_dwt_2d_level(&mut lev3_buf, 8);
    buffer[3840..4096].copy_from_slice(&lev3_buf);

    // Level 2: 16x16 subbands inside 4096 (offset 4096 - 256*4 = 3072)
    let mut lev2_buf = vec![0i16; 1024];
    lev2_buf.copy_from_slice(&buffer[3072..4096]);
    rfx_inverse_dwt_2d_level(&mut lev2_buf, 16);
    buffer[3072..4096].copy_from_slice(&lev2_buf);

    // Level 1: 32x32 subbands -> 64x64 output (offset 0..4096)
    rfx_inverse_dwt_2d_level(buffer, 64);
}

pub fn rfx_place_tile_abs(
    y_coeffs: &[i16],
    cb_coeffs: &[i16],
    cr_coeffs: &[i16],
    x_start: usize,
    y_start: usize,
    surf: &mut [u8],
    surf_w: usize,
    surf_h: usize,
) {
    for y in 0..RFX_TILE_SIZE {
        let dst_y = y_start + y;
        if dst_y >= surf_h {
            continue;
        }

        let row_start = y * RFX_TILE_SIZE;
        for x in 0..RFX_TILE_SIZE {
            let dst_x = x_start + x;
            if dst_x >= surf_w {
                continue;
            }

            let idx = row_start + x;
            let y_val = y_coeffs[idx] as i32;
            let cb_val = cb_coeffs[idx] as i32;
            let cr_val = cr_coeffs[idx] as i32;

            let ys = (y_val + 4096) << 16;
            let b = ((cb_val * 115992 + ys) >> 21).clamp(0, 255) as u8;
            let g = ((ys - cb_val * 22527 - cr_val * 46819) >> 21).clamp(0, 255) as u8;
            let r = ((cr_val * 91916 + ys) >> 21).clamp(0, 255) as u8;

            let dst_idx = (dst_y * surf_w + dst_x) * 4;
            if dst_idx + 4 <= surf.len() {
                surf[dst_idx] = b;
                surf[dst_idx + 1] = g;
                surf[dst_idx + 2] = r;
                surf[dst_idx + 3] = 0xFF;
            }
        }
    }
}
