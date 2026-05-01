/// RDPGFX (MS-RDPEGFX) protocol handler.
///
/// Receives raw RDPGFX payload bytes (already ZGFX-decompressed),
/// dispatches PDU commands, and returns decoded bitmap tiles.

use std::collections::HashMap;
use crate::bitmap::Bitmap;
use crate::protocol::zgfx::ZgfxContext;

// ── RDPGFX command IDs ────────────────────────────────────────────────────────
const CMDID_WIRE_TO_SURFACE_1:     u16 = 0x0001;
const CMDID_SOLID_FILL:            u16 = 0x0004;
const CMDID_SURFACE_TO_CACHE:      u16 = 0x0006;
const CMDID_CACHE_TO_SURFACE:      u16 = 0x0007;
const CMDID_EVICT_CACHE_ENTRY:     u16 = 0x0008;
const CMDID_CREATE_SURFACE:        u16 = 0x0009;
const CMDID_DELETE_SURFACE:        u16 = 0x000A;
const CMDID_START_FRAME:           u16 = 0x000B;
const CMDID_END_FRAME:             u16 = 0x000C;
const CMDID_FRAME_ACKNOWLEDGE:     u16 = 0x000D;
const CMDID_RESET_GRAPHICS:        u16 = 0x000E;
const CMDID_MAP_SURFACE_TO_OUTPUT: u16 = 0x000F;
const CMDID_CACHE_IMPORT_OFFER:    u16 = 0x0010;
const CMDID_CACHE_IMPORT_REPLY:    u16 = 0x0011;
const CMDID_CAPS_ADVERTISE:        u16 = 0x0012;
const CMDID_CAPS_CONFIRM:          u16 = 0x0013;
const CMDID_MAP_SURFACE_SCALED:    u16 = 0x0015;
const CMDID_MAP_SURFACE_SCALED_W:  u16 = 0x0016;
const CMDID_MAP_SURFACE_SCALED_V2: u16 = 0x0017;

// ── Codec IDs ─────────────────────────────────────────────────────────────────
const CODEC_UNCOMPRESSED: u16 = 0x0000;
const CODEC_AVC420:       u16 = 0x000B;
const CODEC_AVC444:       u16 = 0x000E;
const CODEC_AVC444V2:     u16 = 0x000F;

// ── Capability versions ───────────────────────────────────────────────────────
const CAP_VERSION_8:   u32 = 0x00080004;
const CAP_VERSION_81:  u32 = 0x00080105;
const CAP_VERSION_10:  u32 = 0x000A0002;
const CAP_VERSION_101: u32 = 0x000A0100;
const CAP_VERSION_102: u32 = 0x000A0200;
const CAP_VERSION_103: u32 = 0x000A0301;
const CAP_VERSION_104: u32 = 0x000A0400;
const CAP_VERSION_105: u32 = 0x000A0502;
const CAP_VERSION_106: u32 = 0x000A0600;
const CAP_VERSION_107: u32 = 0x000A0701;

#[allow(dead_code)]
const CAP_FLAG_THIN_CLIENT:    u32 = 0x00000001;
#[allow(dead_code)]
const CAP_FLAG_SMALL_CACHE:    u32 = 0x00000002;
#[allow(dead_code)]
const CAP_FLAG_AVC420_ENABLED: u32 = 0x00000010;
#[allow(dead_code)]
const CAP_FLAG_AVC_DISABLED:   u32 = 0x00000020;

const GFX_HEADER_SIZE: usize = 8;

struct Surface {
    width: u16,
    height: u16,
    data: Vec<u8>,   // BGRA pixels
    output_x: u32,
    output_y: u32,
    mapped: bool,
}

struct CacheEntry {
    data: Vec<u8>,
    width: i32,
    height: i32,
}

pub struct RdpgfxHandler {
    surfaces: HashMap<u16, Surface>,
    cache: HashMap<u16, CacheEntry>,
    zgfx: ZgfxContext,
    frames_decoded: u32,
    /// Set when the H264 decoder is waiting for a keyframe (IDR) after failures.
    /// Signals to the caller that a force-refresh (suppress→allow) should be sent.
    needs_force_refresh: bool,
    #[cfg(feature = "h264")]
    h264_dec: Option<crate::h264::H264Decoder>,
}

impl RdpgfxHandler {
    pub fn new() -> Self {
        RdpgfxHandler {
            surfaces: HashMap::new(),
            cache: HashMap::new(),
            zgfx: ZgfxContext::new(),
            frames_decoded: 0,
            needs_force_refresh: false,
            #[cfg(feature = "h264")]
            h264_dec: crate::h264::H264Decoder::new(),
        }
    }

    /// Process a raw RDPGFX payload (ZGFX-compressed).
    /// Returns (decoded bitmaps, outgoing PDUs to send back via DVC, needs_force_refresh).
    /// `needs_force_refresh` is true when the H264 decoder is waiting for an IDR keyframe;
    /// the caller should send a SuppressOutput (suppress→allow) PDU to request one.
    pub fn process(&mut self, data: &[u8]) -> (Vec<Bitmap>, Vec<Vec<u8>>, bool) {
        // ZGFX decompress
        let decompressed = self.zgfx.decompress(data);
        if decompressed.is_empty() {
            return (vec![], vec![], false);
        }
        let (bitmaps, responses) = self.dispatch_pdus(&decompressed);
        let force_refresh = self.needs_force_refresh;
        self.needs_force_refresh = false;
        (bitmaps, responses, force_refresh)
    }

    /// Called when the DVC channel was just created.
    /// Returns CAPS_ADVERTISE PDU(s) to send.
    pub fn on_channel_created(&mut self) -> Vec<Vec<u8>> {
        vec![build_caps_advertise()]
    }

    // ── PDU dispatcher ─────────────────────────────────────────────────────────

    fn dispatch_pdus(&mut self, data: &[u8]) -> (Vec<Bitmap>, Vec<Vec<u8>>) {
        let mut bitmaps = Vec::new();
        let mut outgoing = Vec::new();
        let mut offset = 0;

        while offset + GFX_HEADER_SIZE <= data.len() {
            let cmd_id = u16::from_le_bytes([data[offset], data[offset + 1]]);
            let _flags = u16::from_le_bytes([data[offset + 2], data[offset + 3]]);
            let pdu_len = u32::from_le_bytes([
                data[offset + 4], data[offset + 5],
                data[offset + 6], data[offset + 7],
            ]) as usize;
            if pdu_len < GFX_HEADER_SIZE || offset + pdu_len > data.len() {
                log::warn!("[rdpgfx] bad pduLength {} at offset {}", pdu_len, offset);
                break;
            }
            let body = &data[offset + GFX_HEADER_SIZE..offset + pdu_len];
            self.dispatch_one(cmd_id, body, &mut bitmaps, &mut outgoing);
            offset += pdu_len;
        }

        (bitmaps, outgoing)
    }

    fn dispatch_one(
        &mut self,
        cmd_id: u16,
        data: &[u8],
        bitmaps: &mut Vec<Bitmap>,
        outgoing: &mut Vec<Vec<u8>>,
    ) {
        log::debug!("[rdpgfx] cmd 0x{:04X} len={}", cmd_id, data.len());
        match cmd_id {
            CMDID_CAPS_CONFIRM => {
                self.on_caps_confirm(data);
            }
            CMDID_CAPS_ADVERTISE => {
                // Server→Client direction is non-standard but handle gracefully
                log::debug!("[rdpgfx] received CAPS_ADVERTISE from server (unexpected)");
            }
            CMDID_CREATE_SURFACE => {
                self.on_create_surface(data);
            }
            CMDID_DELETE_SURFACE => {
                self.on_delete_surface(data);
            }
            CMDID_MAP_SURFACE_TO_OUTPUT => {
                self.on_map_surface_to_output(data);
            }
            CMDID_MAP_SURFACE_SCALED |
            CMDID_MAP_SURFACE_SCALED_W |
            CMDID_MAP_SURFACE_SCALED_V2 => {
                self.on_map_surface_to_scaled_output(data);
            }
            CMDID_START_FRAME => {
                log::debug!("[rdpgfx] START_FRAME");
            }
            CMDID_END_FRAME => {
                log::debug!("[rdpgfx] END_FRAME");
                if let Some(ack) = self.on_end_frame(data) {
                    outgoing.push(ack);
                }
            }
            CMDID_WIRE_TO_SURFACE_1 => {
                self.on_wire_to_surface_1(data, bitmaps);
            }
            CMDID_SOLID_FILL => {
                self.on_solid_fill(data, bitmaps);
            }
            CMDID_SURFACE_TO_CACHE => {
                self.on_surface_to_cache(data);
            }
            CMDID_CACHE_TO_SURFACE => {
                self.on_cache_to_surface(data, bitmaps);
            }
            CMDID_EVICT_CACHE_ENTRY => {
                self.on_evict_cache_entry(data);
            }
            CMDID_RESET_GRAPHICS => {
                self.on_reset_graphics(data);
            }
            CMDID_CACHE_IMPORT_OFFER => {
                log::debug!("[rdpgfx] CACHE_IMPORT_OFFER → replying");
                outgoing.push(build_cache_import_reply());
            }
            _ => {
                log::debug!("[rdpgfx] unhandled cmd 0x{:04X}", cmd_id);
            }
        }
    }

    // ── Command handlers ───────────────────────────────────────────────────────

    fn on_caps_confirm(&self, data: &[u8]) {
        if data.len() >= 8 {
            let version = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            let data_len = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
            let flags = if data_len >= 4 && data.len() >= 12 {
                u32::from_le_bytes([data[8], data[9], data[10], data[11]])
            } else {
                0
            };
            log::debug!(
                "[rdpgfx] CAPS_CONFIRM version=0x{:08X} flags=0x{:08X}",
                version, flags
            );
        }
    }

    fn on_create_surface(&mut self, data: &[u8]) {
        if data.len() < 7 { return; }
        let id     = u16::from_le_bytes([data[0], data[1]]);
        let width  = u16::from_le_bytes([data[2], data[3]]);
        let height = u16::from_le_bytes([data[4], data[5]]);
        log::debug!("[rdpgfx] CREATE_SURFACE id={} w={} h={}", id, width, height);
        self.surfaces.insert(id, Surface {
            width,
            height,
            data: vec![0u8; width as usize * height as usize * 4],
            output_x: 0,
            output_y: 0,
            mapped: false,
        });
    }

    fn on_delete_surface(&mut self, data: &[u8]) {
        if data.len() < 2 { return; }
        let id = u16::from_le_bytes([data[0], data[1]]);
        self.surfaces.remove(&id);
    }

    fn on_map_surface_to_output(&mut self, data: &[u8]) {
        if data.len() < 12 { return; }
        let id = u16::from_le_bytes([data[0], data[1]]);
        // data[2..4] = reserved
        let ox = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let oy = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        log::debug!("[rdpgfx] MAP_SURFACE id={} ox={} oy={}", id, ox, oy);
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.output_x = ox;
            s.output_y = oy;
            s.mapped = true;
        }
    }

    fn on_map_surface_to_scaled_output(&mut self, data: &[u8]) {
        if data.len() < 12 { return; }
        let id = u16::from_le_bytes([data[0], data[1]]);
        let ox = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let oy = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.output_x = ox;
            s.output_y = oy;
            s.mapped = true;
        }
    }

    fn on_end_frame(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < 4 { return None; }
        let frame_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        self.frames_decoded += 1;
        // queue_depth=0: no pending frames in decode queue.
        // totalFramesDecoded: cumulative count of decoded frames.
        Some(build_frame_ack(frame_id, 0, self.frames_decoded))
    }

    fn on_reset_graphics(&mut self, data: &[u8]) {
        if data.len() < 8 { return; }
        let w = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let h = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        log::debug!("[rdpgfx] RESET_GRAPHICS {}x{}", w, h);
        self.surfaces.clear();
        self.frames_decoded = 0;
        #[cfg(feature = "h264")]
        {
            self.h264_dec = crate::h264::H264Decoder::new();
        }
    }

    fn on_wire_to_surface_1(&mut self, data: &[u8], bitmaps: &mut Vec<Bitmap>) {
        // MS-RDPEGFX §2.2.2.1: surfaceId(2)+codecId(2)+pixelFormat(1)+destRect(8)+bitmapDataLength(4) = 17 bytes
        if data.len() < 17 { return; }
        let surf_id   = u16::from_le_bytes([data[0], data[1]]);
        let codec_id  = u16::from_le_bytes([data[2], data[3]]);
        let _pix_fmt  = data[4];
        let dest_left   = u16::from_le_bytes([data[5],  data[6]]);
        let dest_top    = u16::from_le_bytes([data[7],  data[8]]);
        let dest_right  = u16::from_le_bytes([data[9],  data[10]]);
        let dest_bottom = u16::from_le_bytes([data[11], data[12]]);
        let bmp_len  = u32::from_le_bytes([data[13], data[14], data[15], data[16]]) as usize;
        if data.len() < 17 + bmp_len { return; }
        let bmp_data = &data[17..17 + bmp_len];

        let w = dest_right.saturating_sub(dest_left) as i32;
        let h = dest_bottom.saturating_sub(dest_top) as i32;
        if w <= 0 || h <= 0 { return; }

        log::debug!("[rdpgfx] WTS1: surf={} codec=0x{:04X} {}x{} at ({},{}) data_len={}",
            surf_id, codec_id, w, h, dest_left, dest_top, bmp_len);

        let (output_x, output_y) = match self.surfaces.get(&surf_id) {
            Some(s) if s.mapped => (s.output_x, s.output_y),
            Some(_) => {
                log::debug!("[rdpgfx] WTS1: surface {} not yet mapped, skipping", surf_id);
                return;
            }
            None => {
                log::debug!("[rdpgfx] WTS1: surface {} not found", surf_id);
                return;
            }
        };

        let abs_x = output_x as i32 + dest_left as i32;
        let abs_y = output_y as i32 + dest_top as i32;

        match codec_id {
            CODEC_UNCOMPRESSED => {
                let expected = w as usize * h as usize * 4;
                if bmp_data.len() < expected { return; }
                let pixels = bmp_data[..expected].to_vec();
                self.blit_to_surface(surf_id, dest_left as i32, dest_top as i32, w, h, &pixels);
                bitmaps.push(make_bitmap(abs_x, abs_y, w, h, pixels));
            }
            CODEC_AVC420 => {
                if let Some((pixels, fw, fh, regions)) = self.decode_avc420(bmp_data) {
                    let (fw, fh) = (fw as i32, fh as i32);
                    let (ew, eh) = (fw.min(w), fh.min(h));
                    blit_avc_frame(surf_id, &mut self.surfaces, bitmaps,
                        &pixels, fw, fh, ew, eh,
                        dest_left as i32, dest_top as i32, abs_x, abs_y, &regions);
                }
            }
            CODEC_AVC444 | CODEC_AVC444V2 => {
                if let Some((pixels, fw, fh, regions)) = self.decode_avc444(bmp_data) {
                    let (fw, fh) = (fw as i32, fh as i32);
                    let (ew, eh) = (fw.min(w), fh.min(h));
                    log::debug!("[rdpgfx] AVC444 decoded {}x{} → blit {}x{} regions={} at ({},{}) abs=({},{})",
                        fw, fh, ew, eh, regions.len(), dest_left, dest_top, abs_x, abs_y);
                    blit_avc_frame(surf_id, &mut self.surfaces, bitmaps,
                        &pixels, fw, fh, ew, eh,
                        dest_left as i32, dest_top as i32, abs_x, abs_y, &regions);
                }
            }
            _ => {
                log::debug!(
                    "[rdpgfx] WTS1: unsupported codec 0x{:04X} surf={} {}x{}",
                    codec_id, surf_id, w, h
                );
            }
        }
    }

    fn on_solid_fill(&mut self, data: &[u8], bitmaps: &mut Vec<Bitmap>) {
        if data.len() < 8 { return; }
        let surf_id    = u16::from_le_bytes([data[0], data[1]]);
        let b = data[2]; let g = data[3]; let r = data[4]; // _xa = data[5]
        let fill_count = u16::from_le_bytes([data[6], data[7]]) as usize;

        let pixel = [b, g, r, 0xFF];
        let mut off = 8;
        for _ in 0..fill_count {
            if off + 8 > data.len() { break; }
            let left   = u16::from_le_bytes([data[off],     data[off + 1]]) as i32;
            let top    = u16::from_le_bytes([data[off + 2], data[off + 3]]) as i32;
            let right  = u16::from_le_bytes([data[off + 4], data[off + 5]]) as i32;
            let bottom = u16::from_le_bytes([data[off + 6], data[off + 7]]) as i32;
            off += 8;
            let w = (right - left).max(0);
            let h = (bottom - top).max(0);
            if w == 0 || h == 0 { continue; }

            let mut fill_data = vec![0u8; (w * h * 4) as usize];
            for i in 0..(w * h) as usize {
                fill_data[i * 4..i * 4 + 4].copy_from_slice(&pixel);
            }

            self.blit_to_surface(surf_id, left, top, w, h, &fill_data);

            if let Some(s) = self.surfaces.get(&surf_id) {
                if s.mapped {
                    let ax = s.output_x as i32 + left;
                    let ay = s.output_y as i32 + top;
                    bitmaps.push(make_bitmap(ax, ay, w, h, fill_data));
                }
            }
        }
    }

    fn on_surface_to_cache(&mut self, data: &[u8]) {
        if data.len() < 12 { return; }
        let surf_id    = u16::from_le_bytes([data[0], data[1]]);
        let cache_slot = u16::from_le_bytes([data[2], data[3]]);
        let left   = u16::from_le_bytes([data[4],  data[5]])  as i32;
        let top    = u16::from_le_bytes([data[6],  data[7]])  as i32;
        let right  = u16::from_le_bytes([data[8],  data[9]])  as i32;
        let bottom = u16::from_le_bytes([data[10], data[11]]) as i32;
        let w = (right - left).max(0);
        let h = (bottom - top).max(0);
        if w == 0 || h == 0 { return; }

        let region = match self.surfaces.get(&surf_id) {
            Some(s) => extract_region(&s.data, s.width as i32, left, top, w, h),
            None => return,
        };
        self.cache.insert(cache_slot, CacheEntry { data: region, width: w, height: h });
    }

    fn on_cache_to_surface(&mut self, data: &[u8], bitmaps: &mut Vec<Bitmap>) {
        if data.len() < 6 { return; }
        let cache_slot = u16::from_le_bytes([data[0], data[1]]);
        let surf_id    = u16::from_le_bytes([data[2], data[3]]);
        let dest_count = u16::from_le_bytes([data[4], data[5]]) as usize;

        let (ce_data, ce_w, ce_h) = match self.cache.get(&cache_slot) {
            Some(ce) => (ce.data.clone(), ce.width, ce.height),
            None => return,
        };

        let mut off = 6;
        for _ in 0..dest_count {
            if off + 4 > data.len() { break; }
            let dx = u16::from_le_bytes([data[off], data[off + 1]]) as i32;
            let dy = u16::from_le_bytes([data[off + 2], data[off + 3]]) as i32;
            off += 4;

            self.blit_to_surface(surf_id, dx, dy, ce_w, ce_h, &ce_data);
            if let Some(s) = self.surfaces.get(&surf_id) {
                if s.mapped {
                    let ax = s.output_x as i32 + dx;
                    let ay = s.output_y as i32 + dy;
                    bitmaps.push(make_bitmap(ax, ay, ce_w, ce_h, ce_data.clone()));
                }
            }
        }
    }

    fn on_evict_cache_entry(&mut self, data: &[u8]) {
        if data.len() < 2 { return; }
        let slot = u16::from_le_bytes([data[0], data[1]]);
        self.cache.remove(&slot);
    }

    // ── H.264 / AVC decode helpers ─────────────────────────────────────────────

    fn decode_avc420(&mut self, data: &[u8]) -> Option<(Vec<u8>, u32, u32, Vec<AvcRect>)> {
        let stream = parse_avc420(data)?;
        let regions = stream.regions;
        self.decode_h264(&stream.h264_data)
            .map(|(pixels, w, h)| (pixels, w, h, regions))
    }

    fn decode_avc444(&mut self, data: &[u8]) -> Option<(Vec<u8>, u32, u32, Vec<AvcRect>)> {
        let (stream, lc) = parse_avc444(data)?;
        log::debug!("[rdpgfx] AVC444 lc={} h264_data_len={}", lc, stream.h264_data.len());
        let regions = stream.regions;
        match lc {
            2 => None,  // auxiliary-only, skip
            _ => self.decode_h264(&stream.h264_data)
                .map(|(pixels, w, h)| (pixels, w, h, regions)),
        }
    }

    #[allow(unused_variables)]
    fn decode_h264(&mut self, h264_data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        #[cfg(feature = "h264")]
        {
            if let Some(ref mut dec) = self.h264_dec {
                let result = dec.decode(h264_data);
                if result.is_none() {
                    // Only request a force-refresh when the decoder has lost sync and
                    // is actively waiting for an IDR.  Don't trigger on the very first
                    // frame (frames_decoded == 0) because the initial IDR is on its way.
                    if dec.needs_keyframe() && self.frames_decoded > 0 {
                        log::debug!("[rdpgfx] H264 decoder waiting for IDR — requesting force refresh");
                        self.needs_force_refresh = true;
                    }
                }
                log::debug!("[rdpgfx] H264 decode {} bytes → {}",
                    h264_data.len(),
                    if result.is_some() { "frame" } else { "none" });
                return result;
            } else {
                log::debug!("[rdpgfx] H264 decoder is None (init failed)");
            }
        }
        #[cfg(not(feature = "h264"))]
        log::debug!("[rdpgfx] H.264 data received but h264 feature not enabled");
        None
    }

    // ── Surface blit helper ────────────────────────────────────────────────────

    fn blit_to_surface(&mut self, surf_id: u16, x: i32, y: i32, w: i32, h: i32, src: &[u8]) {
        let s = match self.surfaces.get_mut(&surf_id) {
            Some(s) => s,
            None => return,
        };
        let stride = s.width as i32 * 4;
        for row in 0..h {
            let dy = y + row;
            if dy < 0 || dy >= s.height as i32 { continue; }
            let src_off = (row * w * 4) as usize;
            let dst_off = (dy * stride + x * 4) as usize;
            let n = (w * 4) as usize;
            if src_off + n <= src.len() && dst_off + n <= s.data.len() {
                s.data[dst_off..dst_off + n].copy_from_slice(&src[src_off..src_off + n]);
            }
        }
    }
}

/// Blit a decoded AVC frame into the surface and emit a bitmap update.
/// Always blits the full effective frame: with the H.264 flush bug fixed,
/// SKIP macroblocks decode to the correct reference-frame pixels, so a
/// full blit always produces a correct image.
#[allow(clippy::too_many_arguments)]
fn blit_avc_frame(
    surf_id:  u16,
    surfaces: &mut std::collections::HashMap<u16, Surface>,
    bitmaps:  &mut Vec<Bitmap>,
    pixels:   &[u8],
    fw: i32, fh: i32,   // decoded frame dimensions (may include macroblock padding)
    ew: i32, eh: i32,   // effective (clipped to dest rect) dimensions
    dx: i32, dy: i32,   // destination offset on surface
    ax: i32, ay: i32,   // absolute screen position
    _regions: &[AvcRect],
) {
    let cropped = crop_bgra(pixels, fw, fh, ew, eh);
    if let Some(s) = surfaces.get_mut(&surf_id) {
        let stride = s.width as i32 * 4;
        for row in 0..eh {
            let sy = dy + row;
            if sy < 0 || sy >= s.height as i32 { continue; }
            let src_off = (row * ew * 4) as usize;
            let dst_off = (sy * stride + dx * 4) as usize;
            let n = (ew * 4) as usize;
            if src_off + n <= cropped.len() && dst_off + n <= s.data.len() {
                s.data[dst_off..dst_off + n].copy_from_slice(&cropped[src_off..src_off + n]);
            }
        }
    }
    bitmaps.push(make_bitmap(ax, ay, ew, eh, cropped));
}


#[derive(Clone)]
struct AvcRect {
    left:   u16,
    top:    u16,
    right:  u16,
    bottom: u16,
}

struct Avc420Stream {
    regions:  Vec<AvcRect>,
    h264_data: Vec<u8>,
}

fn parse_avc420(data: &[u8]) -> Option<Avc420Stream> {
    if data.len() < 4 { return None; }
    let num_regions = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if num_regions > 65536 { return None; }
    // 4 header + 8 bytes rect + 2 bytes quant per region
    let meta_size = 4 + num_regions * 10;
    if meta_size > data.len() { return None; }

    let mut regions = Vec::with_capacity(num_regions);
    let mut off = 4usize;
    // Region rects come first (8 bytes each), then quant/quality (2 bytes each)
    for _ in 0..num_regions {
        let left   = u16::from_le_bytes([data[off],   data[off+1]]);
        let top    = u16::from_le_bytes([data[off+2], data[off+3]]);
        let right  = u16::from_le_bytes([data[off+4], data[off+5]]);
        let bottom = u16::from_le_bytes([data[off+6], data[off+7]]);
        regions.push(AvcRect { left, top, right, bottom });
        off += 8;
    }
    // skip quant/quality bytes (2 per region)
    Some(Avc420Stream { regions, h264_data: data[meta_size..].to_vec() })
}


fn parse_avc444(data: &[u8]) -> Option<(Avc420Stream, u8)> {
    if data.len() < 4 { return None; }
    let cb_field = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let lc = ((cb_field >> 30) & 0x03) as u8;
    let cb_stream1 = (cb_field & 0x3FFF_FFFF) as usize;
    let rest = &data[4..];
    match lc {
        0 => {
            if cb_stream1 > rest.len() { return None; }
            let s = parse_avc420(&rest[..cb_stream1])?;
            Some((s, lc))
        }
        1 => {
            // Clip to cb_stream1 bytes if the field is valid (same as grdp).
            // Passing extra trailing bytes to the H264 decoder can corrupt output.
            let stream_data = if cb_stream1 > 0 && cb_stream1 <= rest.len() {
                &rest[..cb_stream1]
            } else {
                rest
            };
            let s = parse_avc420(stream_data)?;
            Some((s, lc))
        }
        2 => {
            // Auxiliary only — no main stream
            Some((Avc420Stream { regions: vec![], h264_data: vec![] }, lc))
        }
        _ => None,
    }
}

// ── Utility helpers ────────────────────────────────────────────────────────────

fn make_bitmap(x: i32, y: i32, w: i32, h: i32, data: Vec<u8>) -> Bitmap {
    Bitmap {
        dest_left:     x,
        dest_top:      y,
        dest_right:    x + w - 1,
        dest_bottom:   y + h - 1,
        width:         w,
        height:        h,
        bits_per_pixel: 32,
        data,
    }
}

/// Crop a BGRA pixel buffer to (ew × eh) from a (fw × fh) frame.
fn crop_bgra(pixels: &[u8], fw: i32, fh: i32, ew: i32, eh: i32) -> Vec<u8> {
    if fw == ew && fh == eh {
        return pixels.to_vec();
    }
    let mut out = vec![0u8; (ew * eh * 4) as usize];
    let h = eh.min(fh);
    let w = ew.min(fw);
    for row in 0..h {
        let src_off = (row * fw * 4) as usize;
        let dst_off = (row * ew * 4) as usize;
        let n = (w * 4) as usize;
        if src_off + n <= pixels.len() && dst_off + n <= out.len() {
            out[dst_off..dst_off + n].copy_from_slice(&pixels[src_off..src_off + n]);
        }
    }
    out
}

fn extract_region(data: &[u8], stride: i32, x: i32, y: i32, w: i32, h: i32) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 4) as usize];
    for row in 0..h {
        let src_off = ((y + row) * stride * 4 + x * 4) as usize;
        let dst_off = (row * w * 4) as usize;
        let n = (w * 4) as usize;
        if src_off + n <= data.len() && dst_off + n <= out.len() {
            out[dst_off..dst_off + n].copy_from_slice(&data[src_off..src_off + n]);
        }
    }
    out
}

// ── PDU builders ──────────────────────────────────────────────────────────────

/// Build RDPGFX_CAPS_ADVERTISE_PDU (client→server).
/// Advertises multiple capability sets so the server can pick the highest it supports.
pub fn build_caps_advertise() -> Vec<u8> {
    let mut caps = Vec::new();

    // capsSetCount
    caps.extend_from_slice(&11u16.to_le_bytes());

    let push_cap = |caps: &mut Vec<u8>, version: u32, flags: u32| {
        caps.extend_from_slice(&version.to_le_bytes());
        caps.extend_from_slice(&4u32.to_le_bytes()); // capsDataLength
        caps.extend_from_slice(&flags.to_le_bytes());
    };
    let push_cap16 = |caps: &mut Vec<u8>, version: u32| {
        caps.extend_from_slice(&version.to_le_bytes());
        caps.extend_from_slice(&16u32.to_le_bytes());
        caps.extend_from_slice(&[0u8; 16]);
    };

    push_cap(&mut caps, CAP_VERSION_8,   CAP_FLAG_THIN_CLIENT);
    push_cap(&mut caps, CAP_VERSION_81,  CAP_FLAG_SMALL_CACHE | CAP_FLAG_AVC420_ENABLED);
    push_cap(&mut caps, CAP_VERSION_10,  CAP_FLAG_SMALL_CACHE);
    push_cap16(&mut caps, CAP_VERSION_101);
    push_cap(&mut caps, CAP_VERSION_102, CAP_FLAG_SMALL_CACHE);
    push_cap(&mut caps, CAP_VERSION_103, 0);
    push_cap(&mut caps, CAP_VERSION_104, CAP_FLAG_SMALL_CACHE);
    push_cap(&mut caps, CAP_VERSION_105, CAP_FLAG_SMALL_CACHE);
    push_cap(&mut caps, CAP_VERSION_106, CAP_FLAG_SMALL_CACHE);
    push_cap(&mut caps, 0x000A0601,      CAP_FLAG_SMALL_CACHE);
    push_cap(&mut caps, CAP_VERSION_107, CAP_FLAG_SMALL_CACHE);

    let pdu_len = (GFX_HEADER_SIZE + caps.len()) as u32;
    let mut pdu = Vec::with_capacity(pdu_len as usize);
    pdu.extend_from_slice(&CMDID_CAPS_ADVERTISE.to_le_bytes());
    pdu.extend_from_slice(&0u16.to_le_bytes()); // flags
    pdu.extend_from_slice(&pdu_len.to_le_bytes());
    pdu.extend_from_slice(&caps);
    pdu
}

/// Build RDPGFX_FRAME_ACKNOWLEDGE_PDU (client→server).
/// queue_depth: number of frames still queued in client decode pipeline.
/// total_decoded: cumulative number of frames decoded since connection start.
fn build_frame_ack(frame_id: u32, queue_depth: u32, total_decoded: u32) -> Vec<u8> {
    let mut pdu = vec![0u8; 20];
    pdu[0..2].copy_from_slice(&CMDID_FRAME_ACKNOWLEDGE.to_le_bytes());
    // pdu[2..4] = flags = 0
    pdu[4..8].copy_from_slice(&20u32.to_le_bytes());   // pduLength
    pdu[8..12].copy_from_slice(&queue_depth.to_le_bytes());
    pdu[12..16].copy_from_slice(&frame_id.to_le_bytes());
    pdu[16..20].copy_from_slice(&total_decoded.to_le_bytes()); // totalFramesDecoded
    pdu
}

/// Build RDPGFX_CACHE_IMPORT_REPLY_PDU (client→server): no cached entries.
fn build_cache_import_reply() -> Vec<u8> {
    let payload = 0u16.to_le_bytes(); // importedEntriesCount = 0
    let pdu_len = (GFX_HEADER_SIZE + payload.len()) as u32;
    let mut pdu = Vec::with_capacity(pdu_len as usize);
    pdu.extend_from_slice(&CMDID_CACHE_IMPORT_REPLY.to_le_bytes());
    pdu.extend_from_slice(&0u16.to_le_bytes());
    pdu.extend_from_slice(&pdu_len.to_le_bytes());
    pdu.extend_from_slice(&payload);
    pdu
}
