/// RDPGFX (MS-RDPEGFX) protocol handler.
///
/// Receives raw RDPGFX payload bytes (already ZGFX-decompressed),
/// dispatches PDU commands, and returns decoded bitmap tiles.
use crate::bitmap::Bitmap;
use crate::avc::NV12Frame;
use crate::protocol::zgfx::ZgfxContext;
use super::clearcodec::ClearCodecContext;
use super::planar::decode_planar;
use super::rfx::RfxDecoder;
use super::rfx_progressive::RfxProgressiveDecoder;
use std::collections::HashMap;
use std::collections::VecDeque;

// ── RDPGFX command IDs ────────────────────────────────────────────────────────
const CMDID_WIRE_TO_SURFACE_1: u16 = 0x0001;
const CMDID_WIRE_TO_SURFACE_2: u16 = 0x0002;
const CMDID_SOLID_FILL: u16 = 0x0004;
const CMDID_SURFACE_TO_SURFACE: u16 = 0x0005;
const CMDID_SURFACE_TO_CACHE: u16 = 0x0006;
const CMDID_CACHE_TO_SURFACE: u16 = 0x0007;
const CMDID_EVICT_CACHE_ENTRY: u16 = 0x0008;
const CMDID_CREATE_SURFACE: u16 = 0x0009;
const CMDID_DELETE_SURFACE: u16 = 0x000A;
const CMDID_START_FRAME: u16 = 0x000B;
const CMDID_END_FRAME: u16 = 0x000C;
const CMDID_FRAME_ACKNOWLEDGE: u16 = 0x000D;
const CMDID_RESET_GRAPHICS: u16 = 0x000E;
const CMDID_MAP_SURFACE_TO_OUTPUT: u16 = 0x000F;
const CMDID_CACHE_IMPORT_OFFER: u16 = 0x0010;
const CMDID_CACHE_IMPORT_REPLY: u16 = 0x0011;
const CMDID_CAPS_ADVERTISE: u16 = 0x0012;
const CMDID_CAPS_CONFIRM: u16 = 0x0013;
const CMDID_MAP_SURFACE_TO_SCALED_OUTPUT: u16 = 0x0015;
const CMDID_MAP_SURFACE_TO_SCALED_WINDOW: u16 = 0x0016;
const CMDID_MAP_SURFACE_TO_SCALED_OUTPUT_V2: u16 = 0x0017;
const CMDID_MAP_SURFACE_TO_WINDOW: u16 = 0x0018;

// ── Codec IDs ─────────────────────────────────────────────────────────────────
const CODEC_UNCOMPRESSED: u16 = 0x0000;
const CODEC_CAVIDEO: u16 = 0x0003;
const CODEC_PLANAR: u16 = 0x0004;
const CODEC_CLEARCODEC: u16 = 0x0008;
const CODEC_PROGRESSIVE: u16 = 0x0009;
const CODEC_AVC420: u16 = 0x000B;
const CODEC_AVC444: u16 = 0x000E;
const CODEC_AVC444V2: u16 = 0x000F;

// ── Capability versions ───────────────────────────────────────────────────────
const CAP_VERSION_8: u32 = 0x00080004;
const CAP_VERSION_81: u32 = 0x00080105;
const CAP_VERSION_10: u32 = 0x000A0002;
const CAP_VERSION_101: u32 = 0x000A0100;
const CAP_VERSION_102: u32 = 0x000A0200;
const CAP_VERSION_103: u32 = 0x000A0301;
const CAP_VERSION_104: u32 = 0x000A0400;
const CAP_VERSION_105: u32 = 0x000A0502;
const CAP_VERSION_106: u32 = 0x000A0600;
const CAP_VERSION_107: u32 = 0x000A0701;

#[allow(dead_code)]
const CAP_FLAG_THIN_CLIENT: u32 = 0x00000001;
#[allow(dead_code)]
const CAP_FLAG_SMALL_CACHE: u32 = 0x00000002;
#[allow(dead_code)]
const CAP_FLAG_AVC420_ENABLED: u32 = 0x00000010;
#[allow(dead_code)]
const CAP_FLAG_AVC_DISABLED: u32 = 0x00000020;

const GFX_HEADER_SIZE: usize = 8;

/// Raw H.264 NAL packet with screen position, emitted when no AVC decoder is
/// configured (e.g. in the WASM frontend which uses WebCodecs instead).
pub struct H264NalEvent {
    pub dest_left: i32,
    pub dest_top: i32,
    pub is_key: bool,
    pub data: Vec<u8>,
}

struct Surface {
    width: u16,
    height: u16,
    data: Vec<u8>, // BGRA pixels
    output_x: u32,
    output_y: u32,
    mapped: bool,
}

struct CacheEntry {
    data: Vec<u8>,
    width: i32,
    height: i32,
}

/// Cached luma (Y) and half-res chroma (U/V) planes from the most recently
/// decoded AVC444 main stream.  Used to combine with the auxiliary chroma
/// stream when LC=2 frames arrive (AVC444v2, [MS-RDPEGFX 3.3.8.3.3]).
struct Avc444YCache {
    y: Vec<u8>,
    y_stride: usize,
    /// Cb (U) plane from stream1, half-res, stride = (w+1)/2.
    u: Vec<u8>,
    u_stride: usize,
    /// Cr (V) plane from stream1, half-res, stride = (w+1)/2.
    v: Vec<u8>,
    v_stride: usize,
    width: u32,
    height: u32,
    full_range: bool,
}

pub struct RdpgfxHandler {
    surfaces: HashMap<u16, Surface>,
    cache: HashMap<u16, CacheEntry>,
    zgfx: ZgfxContext,
    frames_decoded: u32,
    last_reset_size: Option<(u16, u16)>,
    /// Set when the AVC decoder is waiting for a keyframe (IDR) after failures.
    /// Signals to the caller that a force-refresh (suppress→allow) should be sent.
    needs_force_refresh: bool,
    /// FIFO queue of dirty regions, one entry pushed per H.264 packet sent to the
    /// decoder.  Because FFmpeg's frame-threading pipeline buffers N frames before
    /// outputting (EAGAIN), we must pair each decoded frame with the dirty regions
    /// of the *original* packet — not the *current* one.  Pushing before sending
    /// and popping on each successful decode keeps content and regions in sync.
    pending_avc_regions_queue: VecDeque<Vec<AvcRect>>,
    /// Injected AVC decoder.  `None` when no H.264 support is configured
    /// (e.g. rdp-wasm, CLI tools that don't need video decode).
    avc_dec: Option<Box<dyn crate::avc::AvcDecoder>>,
    disable_avc444: bool,
    /// Secondary AVC decoder for the AVC444 auxiliary chroma stream.
    /// When present, LC=0 stream2 data is fed here (priming), and LC=2 frames
    /// are decoded here and combined with the cached luma plane.
    avc_dec2: Option<Box<dyn crate::avc::AvcDecoder>>,
    /// Cached luma plane from the most recent lc=0/1 AVC444 decode.
    /// Used by the LC=2 chroma-upgrade combine path.
    avc444_y_cache: Option<Avc444YCache>,
    /// Last stream1 IDR NAL data seen.  Kept so that when VideoToolbox stalls
    /// and the decoder falls back to FFmpeg SW decode, the new decoder can be
    /// primed immediately without waiting for the server to send a fresh IDR.
    last_stream1_idr: Vec<u8>,
    /// Number of HW→SW soft resets performed since the last RESET_GRAPHICS.
    soft_reset_count: usize,
    /// True once the primary AVC decoder has been switched to SW fallback mode.
    using_sw_fallback: bool,
    clear_ctx: ClearCodecContext,
    rfx_dec: RfxDecoder,
    rfx_prog_dec: RfxProgressiveDecoder,
}

impl RdpgfxHandler {
    /// Create a handler without an AVC decoder.
    pub fn new() -> Self {
        Self::with_avc(None)
    }

    /// Create a handler with an optional AVC decoder plug-in.
    ///
    /// Pass `Some(decoder)` when H.264/AVC decode support is desired.
    pub fn with_avc(avc_dec: Option<Box<dyn crate::avc::AvcDecoder>>) -> Self {
        Self::with_avc_pair(avc_dec, None)
    }

    /// Create a handler with a primary and optional secondary AVC decoder.
    ///
    /// `avc_dec` handles the main H.264 stream.  `avc_dec2` handles the
    /// AVC444 auxiliary chroma stream for LC=2 chroma-upgrade decoding.
    /// When `avc_dec2` is `Some`, the handler also caches the luma (Y) plane
    /// from each LC=0/1 decode so that LC=2 combine is possible.
    pub fn with_avc_pair(
        avc_dec: Option<Box<dyn crate::avc::AvcDecoder>>,
        avc_dec2: Option<Box<dyn crate::avc::AvcDecoder>>,
    ) -> Self {
        Self::with_avc_pair_and_options(avc_dec, avc_dec2, false)
    }

    /// Create a handler with optional AVC444 capability advertisement disabled.
    pub fn with_avc_pair_and_options(
        avc_dec: Option<Box<dyn crate::avc::AvcDecoder>>,
        avc_dec2: Option<Box<dyn crate::avc::AvcDecoder>>,
        disable_avc444: bool,
    ) -> Self {
        if avc_dec.is_some() {
            if avc_dec2.is_some() {
                log::debug!("[rdpgfx] AVC decoder configured (primary + auxiliary for LC=2)");
            } else {
                log::debug!("[rdpgfx] AVC decoder configured");
            }
        } else {
            log::debug!("[rdpgfx] no AVC decoder — H.264 frames will be skipped");
        }
        RdpgfxHandler {
            surfaces: HashMap::new(),
            cache: HashMap::new(),
            zgfx: ZgfxContext::new(),
            frames_decoded: 0,
            last_reset_size: None,
            needs_force_refresh: false,
            pending_avc_regions_queue: VecDeque::new(),
            avc_dec,
            disable_avc444,
            avc_dec2,
            avc444_y_cache: None,
            last_stream1_idr: Vec::new(),
            soft_reset_count: 0,
            using_sw_fallback: false,
            clear_ctx: ClearCodecContext::new(),
            rfx_dec: RfxDecoder::new(),
            rfx_prog_dec: RfxProgressiveDecoder::new(),
        }
    }

    /// Process a raw RDPGFX payload (ZGFX-compressed).
    /// Returns (decoded bitmaps, nv12 frames, raw H264 NAL events, outgoing PDUs to send back via DVC, needs_force_refresh).
    /// `needs_force_refresh` is true when the H264 decoder wants an IDR keyframe;
    /// the caller should send a SuppressOutput (suppress→allow) PDU to request one.
    pub fn process(
        &mut self,
        data: &[u8],
    ) -> (Vec<Bitmap>, Vec<NV12Frame>, Vec<H264NalEvent>, Vec<Vec<u8>>, bool, Option<(u16, u16)>) {
        // ZGFX decompress
        let decompressed = self.zgfx.decompress(data);
        if decompressed.is_empty() {
            return (vec![], vec![], vec![], vec![], false, None);
        }
        self.last_reset_size = None;
        let (bitmaps, nv12_frames, h264_nals, responses) = self.dispatch_pdus(&decompressed);
        let force_refresh = self.needs_force_refresh;
        self.needs_force_refresh = false;
        let reset_size = self.last_reset_size.take();
        (bitmaps, nv12_frames, h264_nals, responses, force_refresh, reset_size)
    }

    /// Called when the DVC channel was just created.
    /// Returns CAPS_ADVERTISE PDU(s) to send.
    pub fn on_channel_created(&mut self) -> Vec<Vec<u8>> {
        vec![build_caps_advertise_with_options(self.disable_avc444)]
    }

    /// Called when the server sent a large/full-screen raw Bitmap Update in
    /// response to a SuppressOutput force-refresh PDU while the VideoToolbox
    /// pipeline was elevated (depth > 0).  Flushes the H.264 decoder pipeline
    /// and the region FIFO so stale frames do not overwrite the refreshed pixels.
    /// The decoder_flushed flag returned from the next decode() call will be
    /// true, causing the FIFO to be re-synced when AVC resumes with an IDR.
    pub fn signal_screen_refreshed(&mut self) {
        if let Some(dec) = &mut self.avc_dec {
            dec.signal_screen_refreshed();
            // If decoder_flushed is now set, clear the FIFO immediately
            // (no need to wait for the next decode() call).
            if dec.take_decoder_flushed() {
                log::debug!(
                    "[rdpgfx] signal_screen_refreshed: cleared FIFO (had {} entries)",
                    self.pending_avc_regions_queue.len()
                );
                self.pending_avc_regions_queue.clear();
                self.needs_force_refresh = false;
            }
        }
    }

    // ── PDU dispatcher ─────────────────────────────────────────────────────────

    fn dispatch_pdus(&mut self, data: &[u8]) -> (Vec<Bitmap>, Vec<NV12Frame>, Vec<H264NalEvent>, Vec<Vec<u8>>) {
        let mut bitmaps = Vec::new();
        let mut nv12_frames = Vec::new();
        let mut h264_nals = Vec::new();
        let mut outgoing = Vec::new();
        let mut offset = 0;

        while offset + GFX_HEADER_SIZE <= data.len() {
            let cmd_id = u16::from_le_bytes([data[offset], data[offset + 1]]);
            let _flags = u16::from_le_bytes([data[offset + 2], data[offset + 3]]);
            let pdu_len = u32::from_le_bytes([
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]) as usize;
            if pdu_len < GFX_HEADER_SIZE || offset + pdu_len > data.len() {
                log::warn!("[rdpgfx] bad pduLength {} at offset {}", pdu_len, offset);
                break;
            }
            let body = &data[offset + GFX_HEADER_SIZE..offset + pdu_len];
            self.dispatch_one(cmd_id, body, &mut bitmaps, &mut nv12_frames, &mut h264_nals, &mut outgoing);
            offset += pdu_len;
        }

        (bitmaps, nv12_frames, h264_nals, outgoing)
    }

    fn dispatch_one(
        &mut self,
        cmd_id: u16,
        data: &[u8],
        bitmaps: &mut Vec<Bitmap>,
        nv12_frames: &mut Vec<NV12Frame>,
        h264_nals: &mut Vec<H264NalEvent>,
        outgoing: &mut Vec<Vec<u8>>,
    ) {
        log::trace!("[rdpgfx] cmd 0x{:04X} len={}", cmd_id, data.len());
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
            CMDID_MAP_SURFACE_TO_SCALED_OUTPUT | CMDID_MAP_SURFACE_TO_SCALED_OUTPUT_V2 => {
                self.on_map_surface_to_scaled_output(data);
            }
            CMDID_MAP_SURFACE_TO_WINDOW | CMDID_MAP_SURFACE_TO_SCALED_WINDOW => {
                self.on_map_surface_to_window(data);
            }
            CMDID_START_FRAME => {
                log::debug!("[rdpgfx] START_FRAME");
            }
            CMDID_END_FRAME => {
                if let Some(ack) = self.on_end_frame(data) {
                    log::debug!("[rdpgfx] END_FRAME → ack sent");
                    outgoing.push(ack);
                }
            }
            CMDID_WIRE_TO_SURFACE_1 => {
                self.on_wire_to_surface_1(data, bitmaps, nv12_frames, h264_nals);
            }
            CMDID_WIRE_TO_SURFACE_2 => {
                self.on_wire_to_surface_2(data, bitmaps, nv12_frames, h264_nals);
            }
            CMDID_SOLID_FILL => {
                self.on_solid_fill(data, bitmaps);
            }
            CMDID_SURFACE_TO_SURFACE => {
                self.on_surface_to_surface(data, bitmaps);
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
                version,
                flags
            );
        }
    }

    fn on_create_surface(&mut self, data: &[u8]) {
        if data.len() < 7 {
            return;
        }
        let id = u16::from_le_bytes([data[0], data[1]]);
        let width = u16::from_le_bytes([data[2], data[3]]);
        let height = u16::from_le_bytes([data[4], data[5]]);
        log::debug!("[rdpgfx] CREATE_SURFACE id={} w={} h={}", id, width, height);
        self.surfaces.insert(
            id,
            Surface {
                width,
                height,
                data: vec![0u8; width as usize * height as usize * 4],
                output_x: 0,
                output_y: 0,
                mapped: false,
            },
        );
    }

    fn on_delete_surface(&mut self, data: &[u8]) {
        if data.len() < 2 {
            return;
        }
        let id = u16::from_le_bytes([data[0], data[1]]);
        log::debug!("[rdpgfx] DELETE_SURFACE id={}", id);
        self.surfaces.remove(&id);
    }

    fn on_map_surface_to_output(&mut self, data: &[u8]) {
        if data.len() < 12 {
            return;
        }
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
        if data.len() < 20 {
            return;
        }
        let id = u16::from_le_bytes([data[0], data[1]]);
        let ox = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let oy = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        log::debug!(
            "[rdpgfx] MAP_SURFACE_SCALED id={} ox={} oy={} target={}x{}",
            id,
            ox,
            oy,
            u32::from_le_bytes([data[12], data[13], data[14], data[15]]),
            u32::from_le_bytes([data[16], data[17], data[18], data[19]])
        );
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.output_x = ox;
            s.output_y = oy;
            s.mapped = true;
        }
    }

    fn on_map_surface_to_window(&mut self, data: &[u8]) {
        if data.len() < 2 {
            return;
        }
        let id = u16::from_le_bytes([data[0], data[1]]);
        log::debug!("[rdpgfx] MAP_SURFACE_TO_WINDOW id={} ignored", id);
    }

    fn on_end_frame(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < 4 {
            return None;
        }
        let frame_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        self.frames_decoded += 1;
        // queue_depth=0: no pending frames in decode queue.
        // totalFramesDecoded: cumulative count of decoded frames.
        Some(build_frame_ack(frame_id, 0, self.frames_decoded))
    }

    fn on_reset_graphics(&mut self, data: &[u8]) {
        if data.len() < 8 {
            return;
        }
        let w = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let h = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        log::debug!("[rdpgfx] RESET_GRAPHICS {}x{}", w, h);
        if let (Ok(w16), Ok(h16)) = (u16::try_from(w), u16::try_from(h)) {
            self.last_reset_size = Some((w16, h16));
        }
        self.surfaces.clear();
        self.frames_decoded = 0;
        self.last_stream1_idr.clear();
        self.soft_reset_count = 0;
        self.using_sw_fallback = false;
        self.clear_ctx.reset_cache();
        self.rfx_prog_dec.reset();
        // Reset both AVC decoders so stale pipeline frames do not bleed
        // into the new surface configuration.
        if let Some(dec) = &mut self.avc_dec {
            dec.reset();
            log::debug!("[rdpgfx] RESET_GRAPHICS: primary AVC decoder reset");
        }
        if let Some(dec2) = &mut self.avc_dec2 {
            dec2.reset();
            log::debug!("[rdpgfx] RESET_GRAPHICS: auxiliary AVC decoder reset");
        }
        self.avc444_y_cache = None;
    }

    fn on_wire_to_surface_1(&mut self, data: &[u8], bitmaps: &mut Vec<Bitmap>, nv12_frames: &mut Vec<NV12Frame>, h264_nals: &mut Vec<H264NalEvent>) {
        // MS-RDPEGFX §2.2.2.1: surfaceId(2)+codecId(2)+pixelFormat(1)+destRect(8)+bitmapDataLength(4) = 17 bytes
        if data.len() < 17 {
            return;
        }
        let surf_id = u16::from_le_bytes([data[0], data[1]]);
        let codec_id = u16::from_le_bytes([data[2], data[3]]);
        let _pix_fmt = data[4];
        let dest_left = u16::from_le_bytes([data[5], data[6]]);
        let dest_top = u16::from_le_bytes([data[7], data[8]]);
        let dest_right = u16::from_le_bytes([data[9], data[10]]);
        let dest_bottom = u16::from_le_bytes([data[11], data[12]]);
        let bmp_len = u32::from_le_bytes([data[13], data[14], data[15], data[16]]) as usize;
        if data.len() < 17 + bmp_len {
            return;
        }
        let bmp_data = &data[17..17 + bmp_len];

        let w = dest_right.saturating_sub(dest_left) as i32;
        let h = dest_bottom.saturating_sub(dest_top) as i32;
        if w <= 0 || h <= 0 {
            return;
        }

        log::debug!(
            "[rdpgfx] WTS1: surf={} codec=0x{:04X} {}x{} at ({},{}) data_len={}",
            surf_id,
            codec_id,
            w,
            h,
            dest_left,
            dest_top,
            bmp_len
        );

        let (mapped, output_x, output_y) = match self.surfaces.get(&surf_id) {
            Some(s) => (s.mapped, s.output_x, s.output_y),
            None => {
                log::debug!("[rdpgfx] WTS1: surface {} not found", surf_id);
                return;
            }
        };

        let abs_x = output_x as i32 + dest_left as i32;
        let abs_y = output_y as i32 + dest_top as i32;
        log::debug!(
            "[rdpgfx] WTS1: surf={} mapped={} codec=0x{:04X} abs=({},{}) eff={}x{}",
            surf_id, mapped, codec_id, abs_x, abs_y, w, h
        );

        match codec_id {
            CODEC_UNCOMPRESSED => {
                let expected = w as usize * h as usize * 4;
                if bmp_data.len() < expected {
                    return;
                }
                let pixels = bmp_data[..expected].to_vec();
                self.blit_to_surface(surf_id, dest_left as i32, dest_top as i32, w, h, &pixels);
                if mapped {
                    log::debug!(
                        "[rdpgfx] emit WTS1 uncompressed surf={} rect=({},{} {}x{}) abs=({},{})",
                        surf_id, dest_left, dest_top, w, h, abs_x, abs_y
                    );
                    bitmaps.push(make_bitmap(abs_x, abs_y, w, h, pixels));
                }
            }
            CODEC_PLANAR => {
                let pixels = decode_planar(bmp_data, w as usize, h as usize);
                self.blit_to_surface(surf_id, dest_left as i32, dest_top as i32, w, h, &pixels);
                if mapped {
                    bitmaps.push(make_bitmap(abs_x, abs_y, w, h, pixels));
                }
            }
            CODEC_CLEARCODEC => {
                let mut pixels = vec![0u8; w as usize * h as usize * 4];
                if self.clear_ctx.decode(
                    bmp_data,
                    w as usize,
                    h as usize,
                    &mut pixels,
                    0,
                    0,
                    w as usize,
                    h as usize,
                ) {
                    self.blit_to_surface(surf_id, dest_left as i32, dest_top as i32, w, h, &pixels);
                    if mapped {
                        bitmaps.push(make_bitmap(abs_x, abs_y, w, h, pixels));
                    }
                }
            }
            CODEC_CAVIDEO => {
                if let Some(surf) = self.surfaces.get_mut(&surf_id) {
                    let sw = surf.width as usize;
                    let sh = surf.height as usize;
                    let rects = self.rfx_dec.decode(
                        bmp_data,
                        dest_left as usize,
                        dest_top as usize,
                        &mut surf.data,
                        sw,
                        sh,
                    );
                    emit_tile_rects(surf, &rects, bitmaps);
                }
            }
            CODEC_PROGRESSIVE => {
                if let Some(surf) = self.surfaces.get_mut(&surf_id) {
                    let sw = surf.width as usize;
                    let sh = surf.height as usize;
                    let rects = self.rfx_prog_dec.decode(
                        bmp_data,
                        dest_left as usize,
                        dest_top as usize,
                        &mut surf.data,
                        sw,
                        sh,
                    );
                    emit_tile_rects(surf, &rects, bitmaps);
                }
            }
            CODEC_AVC420 => {
                if self.avc_supports_nv12() {
                    if let Some(mut nv12) = self.decode_avc420_nv12(bmp_data) {
                        nv12.screen_x = abs_x;
                        nv12.screen_y = abs_y;
                        nv12_frames.push(nv12);
                    }
                } else if let Some((pixels, fw, fh, regions, force_regions)) = self.decode_avc420(bmp_data) {
                    let (fw, fh) = (fw as i32, fh as i32);
                    let (ew, eh) = (fw.min(w), fh.min(h));
                    let mut discard = Vec::new();
                    let out = if mapped { bitmaps } else { &mut discard };
                    blit_avc_frame(
                        surf_id,
                        &mut self.surfaces,
                        out,
                        &pixels,
                        fw,
                        fh,
                        ew,
                        eh,
                        dest_left as i32,
                        dest_top as i32,
                        abs_x,
                        abs_y,
                        force_regions,
                        &regions,
                    );
                } else if self.avc_dec.is_none() && mapped {
                    if let Some(stream) = parse_avc420(bmp_data) {
                        h264_nals.push(H264NalEvent {
                            dest_left: abs_x,
                            dest_top: abs_y,
                            is_key: is_h264_keyframe(&stream.h264_data),
                            data: stream.h264_data,
                        });
                    }
                }
            }
            CODEC_AVC444 => {
                if self.avc_supports_nv12() {
                    if let Some(mut nv12) = self.decode_avc444_nv12(bmp_data) {
                        nv12.screen_x = abs_x;
                        nv12.screen_y = abs_y;
                        nv12_frames.push(nv12);
                    }
                } else if let Some((pixels, fw, fh, regions, force_regions)) = self.decode_avc444(bmp_data) {
                    let (fw, fh) = (fw as i32, fh as i32);
                    let (ew, eh) = (fw.min(w), fh.min(h));
                    log::debug!("[rdpgfx] AVC444 decoded {}x{} → blit {}x{} regions={} at ({},{}) abs=({},{}) mapped={}",
                        fw, fh, ew, eh, regions.len(), dest_left, dest_top, abs_x, abs_y, mapped);
                    let mut discard = Vec::new();
                    let out = if mapped { bitmaps } else { &mut discard };
                    blit_avc_frame(
                        surf_id,
                        &mut self.surfaces,
                        out,
                        &pixels,
                        fw,
                        fh,
                        ew,
                        eh,
                        dest_left as i32,
                        dest_top as i32,
                        abs_x,
                        abs_y,
                        force_regions,
                        &regions,
                    );
                } else if self.avc_dec.is_none() && mapped {
                    if let Some(parsed) = parse_avc444(bmp_data) {
                        if let Some(stream) = parsed.stream1 {
                            h264_nals.push(H264NalEvent {
                                dest_left: abs_x,
                                dest_top: abs_y,
                                is_key: is_h264_keyframe(&stream.h264_data),
                                data: stream.h264_data,
                            });
                        }
                    }
                }
            }
            CODEC_AVC444V2 => {
                // AVC444v2 stream1 carries packed B2/B3 chroma data per the B2-B9
                // algorithm (MS-RDPEGFX 3.3.8.3.3).  Displaying the NV12 output
                // directly would show stream1's packed chroma as raw YUV420, producing
                // incorrect colours.  Always use the BGRA/combine path so that the
                // proper chroma reconstruction is performed.
                if let Some((pixels, fw, fh, regions, force_regions)) = self.decode_avc444(bmp_data) {
                    let (fw, fh) = (fw as i32, fh as i32);
                    let (ew, eh) = (fw.min(w), fh.min(h));
                    log::debug!("[rdpgfx] AVC444v2 decoded {}x{} → blit {}x{} regions={} at ({},{}) abs=({},{}) mapped={}",
                        fw, fh, ew, eh, regions.len(), dest_left, dest_top, abs_x, abs_y, mapped);
                    let mut discard = Vec::new();
                    let out = if mapped { bitmaps } else { &mut discard };
                    blit_avc_frame(
                        surf_id,
                        &mut self.surfaces,
                        out,
                        &pixels,
                        fw,
                        fh,
                        ew,
                        eh,
                        dest_left as i32,
                        dest_top as i32,
                        abs_x,
                        abs_y,
                        force_regions,
                        &regions,
                    );
                } else if self.avc_dec.is_none() && mapped {
                    if let Some(parsed) = parse_avc444(bmp_data) {
                        if let Some(stream) = parsed.stream1 {
                            h264_nals.push(H264NalEvent {
                                dest_left: abs_x,
                                dest_top: abs_y,
                                is_key: is_h264_keyframe(&stream.h264_data),
                                data: stream.h264_data,
                            });
                        }
                    }
                }
            }
            _ => {
                log::debug!(
                    "[rdpgfx] WTS1: unsupported codec 0x{:04X} surf={} {}x{}",
                    codec_id,
                    surf_id,
                    w,
                    h
                );
            }
        }
    }

    fn on_wire_to_surface_2(&mut self, data: &[u8], bitmaps: &mut Vec<Bitmap>, nv12_frames: &mut Vec<NV12Frame>, h264_nals: &mut Vec<H264NalEvent>) {
        // MS-RDPEGFX §2.2.2.2: surfaceId(2)+codecId(2)+codecCtxId(4)+pixelFormat(1)+bitmapDataLength(4) = 13 bytes
        if data.len() < 13 {
            return;
        }
        let surf_id = u16::from_le_bytes([data[0], data[1]]);
        let codec_id = u16::from_le_bytes([data[2], data[3]]);
        let _ctx_id = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let _pix_fmt = data[8];
        let bmp_len = u32::from_le_bytes([data[9], data[10], data[11], data[12]]) as usize;
        if data.len() < 13 + bmp_len {
            return;
        }
        let bmp_data = &data[13..13 + bmp_len];

        let (sw, sh, mapped, output_x, output_y) = match self.surfaces.get(&surf_id) {
            Some(s) => (
                s.width as i32,
                s.height as i32,
                s.mapped,
                s.output_x,
                s.output_y,
            ),
            None => {
                log::debug!("[rdpgfx] WTS2: surface {} not found", surf_id);
                return;
            }
        };

        let w = sw;
        let h = sh;
        let abs_x = output_x as i32;
        let abs_y = output_y as i32;

        log::debug!(
            "[rdpgfx] WTS2: surf={} codec=0x{:04X} {}x{} mapped={} abs=({},{})",
            surf_id, codec_id, w, h, mapped, abs_x, abs_y
        );

        match codec_id {
            CODEC_UNCOMPRESSED => {
                let expected = w as usize * h as usize * 4;
                if bmp_data.len() < expected {
                    return;
                }
                let pixels = bmp_data[..expected].to_vec();
                self.blit_to_surface(surf_id, 0, 0, w, h, &pixels);
                if mapped {
                    log::debug!(
                        "[rdpgfx] emit WTS2 uncompressed surf={} rect=(0,0 {}x{}) abs=({},{})",
                        surf_id, w, h, abs_x, abs_y
                    );
                    bitmaps.push(make_bitmap(abs_x, abs_y, w, h, pixels));
                }
            }
            CODEC_PLANAR => {
                let pixels = decode_planar(bmp_data, w as usize, h as usize);
                self.blit_to_surface(surf_id, 0, 0, w, h, &pixels);
                if mapped {
                    bitmaps.push(make_bitmap(abs_x, abs_y, w, h, pixels));
                }
            }
            CODEC_CLEARCODEC => {
                let mut pixels = vec![0u8; w as usize * h as usize * 4];
                if self.clear_ctx.decode(
                    bmp_data,
                    w as usize,
                    h as usize,
                    &mut pixels,
                    0,
                    0,
                    w as usize,
                    h as usize,
                ) {
                    self.blit_to_surface(surf_id, 0, 0, w, h, &pixels);
                    if mapped {
                        bitmaps.push(make_bitmap(abs_x, abs_y, w, h, pixels));
                    }
                }
            }
            CODEC_CAVIDEO => {
                if let Some(surf) = self.surfaces.get_mut(&surf_id) {
                    let sw = surf.width as usize;
                    let sh = surf.height as usize;
                    let rects = self.rfx_dec.decode(bmp_data, 0, 0, &mut surf.data, sw, sh);
                    emit_tile_rects(surf, &rects, bitmaps);
                }
            }
            CODEC_PROGRESSIVE => {
                if let Some(surf) = self.surfaces.get_mut(&surf_id) {
                    let sw = surf.width as usize;
                    let sh = surf.height as usize;
                    let rects = self.rfx_prog_dec.decode(bmp_data, 0, 0, &mut surf.data, sw, sh);
                    emit_tile_rects(surf, &rects, bitmaps);
                }
            }
            CODEC_AVC420 => {
                if self.avc_supports_nv12() {
                    if let Some(mut nv12) = self.decode_avc420_nv12(bmp_data) {
                        nv12.screen_x = abs_x;
                        nv12.screen_y = abs_y;
                        nv12_frames.push(nv12);
                    }
                } else if let Some((pixels, fw, fh, regions, force_regions)) = self.decode_avc420(bmp_data) {
                    let (fw, fh) = (fw as i32, fh as i32);
                    let (ew, eh) = (fw.min(w), fh.min(h));
                    log::debug!(
                        "[rdpgfx] WTS2 AVC420 decoded {}x{} → blit {}x{} mapped={}",
                        fw, fh, ew, eh, mapped
                    );
                    let mut discard = Vec::new();
                    let out = if mapped { bitmaps } else { &mut discard };
                    blit_avc_frame(
                        surf_id,
                        &mut self.surfaces,
                        out,
                        &pixels,
                        fw,
                        fh,
                        ew,
                        eh,
                        0,
                        0,
                        abs_x,
                        abs_y,
                        force_regions,
                        &regions,
                    );
                } else if self.avc_dec.is_none() && mapped {
                    if let Some(stream) = parse_avc420(bmp_data) {
                        h264_nals.push(H264NalEvent {
                            dest_left: abs_x,
                            dest_top: abs_y,
                            is_key: is_h264_keyframe(&stream.h264_data),
                            data: stream.h264_data,
                        });
                    }
                }
            }
            CODEC_AVC444 => {
                if self.avc_supports_nv12() {
                    if let Some(mut nv12) = self.decode_avc444_nv12(bmp_data) {
                        nv12.screen_x = abs_x;
                        nv12.screen_y = abs_y;
                        nv12_frames.push(nv12);
                    }
                } else if let Some((pixels, fw, fh, regions, force_regions)) = self.decode_avc444(bmp_data) {
                    let (fw, fh) = (fw as i32, fh as i32);
                    let (ew, eh) = (fw.min(w), fh.min(h));
                    log::debug!("[rdpgfx] WTS2 AVC444 decoded {}x{} → blit {}x{} regions={} abs=({},{}) mapped={}",
                        fw, fh, ew, eh, regions.len(), abs_x, abs_y, mapped);
                    let mut discard = Vec::new();
                    let out = if mapped { bitmaps } else { &mut discard };
                    blit_avc_frame(
                        surf_id,
                        &mut self.surfaces,
                        out,
                        &pixels,
                        fw,
                        fh,
                        ew,
                        eh,
                        0,
                        0,
                        abs_x,
                        abs_y,
                        force_regions,
                        &regions,
                    );
                } else if self.avc_dec.is_none() && mapped {
                    if let Some(parsed) = parse_avc444(bmp_data) {
                        if let Some(stream) = parsed.stream1 {
                            h264_nals.push(H264NalEvent {
                                dest_left: abs_x,
                                dest_top: abs_y,
                                is_key: is_h264_keyframe(&stream.h264_data),
                                data: stream.h264_data,
                            });
                        }
                    }
                }
            }
            CODEC_AVC444V2 => {
                // AVC444v2 always uses the BGRA/combine path — same reason as WTS1.
                if let Some((pixels, fw, fh, regions, force_regions)) = self.decode_avc444(bmp_data) {
                    let (fw, fh) = (fw as i32, fh as i32);
                    let (ew, eh) = (fw.min(w), fh.min(h));
                    log::debug!("[rdpgfx] WTS2 AVC444v2 decoded {}x{} → blit {}x{} regions={} abs=({},{}) mapped={}",
                        fw, fh, ew, eh, regions.len(), abs_x, abs_y, mapped);
                    let mut discard = Vec::new();
                    let out = if mapped { bitmaps } else { &mut discard };
                    blit_avc_frame(
                        surf_id,
                        &mut self.surfaces,
                        out,
                        &pixels,
                        fw,
                        fh,
                        ew,
                        eh,
                        0,
                        0,
                        abs_x,
                        abs_y,
                        force_regions,
                        &regions,
                    );
                } else if self.avc_dec.is_none() && mapped {
                    if let Some(parsed) = parse_avc444(bmp_data) {
                        if let Some(stream) = parsed.stream1 {
                            h264_nals.push(H264NalEvent {
                                dest_left: abs_x,
                                dest_top: abs_y,
                                is_key: is_h264_keyframe(&stream.h264_data),
                                data: stream.h264_data,
                            });
                        }
                    }
                }
            }
            _ => {
                log::debug!(
                    "[rdpgfx] WTS2: unsupported codec 0x{:04X} surf={} {}x{}",
                    codec_id,
                    surf_id,
                    w,
                    h
                );
            }
        }
    }

    fn on_solid_fill(&mut self, data: &[u8], bitmaps: &mut Vec<Bitmap>) {
        if data.len() < 8 {
            return;
        }
        let surf_id = u16::from_le_bytes([data[0], data[1]]);
        let b = data[2];
        let g = data[3];
        let r = data[4]; // _xa = data[5]
        let fill_count = u16::from_le_bytes([data[6], data[7]]) as usize;

        let pixel = [b, g, r, 0xFF];
        let mut off = 8;
        for _ in 0..fill_count {
            if off + 8 > data.len() {
                break;
            }
            let left = u16::from_le_bytes([data[off], data[off + 1]]) as i32;
            let top = u16::from_le_bytes([data[off + 2], data[off + 3]]) as i32;
            let right = u16::from_le_bytes([data[off + 4], data[off + 5]]) as i32;
            let bottom = u16::from_le_bytes([data[off + 6], data[off + 7]]) as i32;
            off += 8;
            let w = (right - left).max(0);
            let h = (bottom - top).max(0);
            if w == 0 || h == 0 {
                continue;
            }

            let mut fill_data = vec![0u8; (w * h * 4) as usize];
            for i in 0..(w * h) as usize {
                fill_data[i * 4..i * 4 + 4].copy_from_slice(&pixel);
            }

            self.blit_to_surface(surf_id, left, top, w, h, &fill_data);

            if let Some(s) = self.surfaces.get(&surf_id) {
                if s.mapped {
                    let ax = s.output_x as i32 + left;
                    let ay = s.output_y as i32 + top;
                    log::debug!(
                        "[rdpgfx] SOLID_FILL surf={} abs=({},{}) {}x{} rgb=({},{},{})",
                        surf_id,
                        ax,
                        ay,
                        w,
                        h,
                        r,
                        g,
                        b
                    );
                    bitmaps.push(make_bitmap(ax, ay, w, h, fill_data));
                }
            }
        }
    }

    fn on_surface_to_cache(&mut self, data: &[u8]) {
        if data.len() < 20 {
            return;
        }
        let surf_id = u16::from_le_bytes([data[0], data[1]]);
        // data[2..10] = cacheKey (unused)
        let cache_slot = u16::from_le_bytes([data[10], data[11]]);
        let left = u16::from_le_bytes([data[12], data[13]]) as i32;
        let top = u16::from_le_bytes([data[14], data[15]]) as i32;
        let right = u16::from_le_bytes([data[16], data[17]]) as i32;
        let bottom = u16::from_le_bytes([data[18], data[19]]) as i32;
        let w = (right - left).max(0);
        let h = (bottom - top).max(0);
        if w == 0 || h == 0 {
            return;
        }

        let region = match self.surfaces.get(&surf_id) {
            Some(s) => extract_region(&s.data, s.width as i32, left, top, w, h),
            None => return,
        };
        self.cache.insert(
            cache_slot,
            CacheEntry {
                data: region,
                width: w,
                height: h,
            },
        );
    }

    fn on_surface_to_surface(&mut self, data: &[u8], bitmaps: &mut Vec<Bitmap>) {
        if data.len() < 14 {
            return;
        }
        let src_id = u16::from_le_bytes([data[0], data[1]]);
        let dst_id = u16::from_le_bytes([data[2], data[3]]);
        let left = u16::from_le_bytes([data[4], data[5]]) as i32;
        let top = u16::from_le_bytes([data[6], data[7]]) as i32;
        let right = u16::from_le_bytes([data[8], data[9]]) as i32;
        let bottom = u16::from_le_bytes([data[10], data[11]]) as i32;
        let dest_count = u16::from_le_bytes([data[12], data[13]]) as usize;
        if data.len() < 14 + dest_count * 4 {
            return;
        }

        let (region, w, h) = match self.surfaces.get(&src_id) {
            Some(src) => {
                let left = left.clamp(0, src.width as i32);
                let top = top.clamp(0, src.height as i32);
                let right = right.clamp(left, src.width as i32);
                let bottom = bottom.clamp(top, src.height as i32);
                let w = right - left;
                let h = bottom - top;
                if w <= 0 || h <= 0 {
                    return;
                }
                (
                    extract_region(&src.data, src.width as i32, left, top, w, h),
                    w,
                    h,
                )
            }
            None => return,
        };

        let mut off = 14;
        for _ in 0..dest_count {
            let dx = u16::from_le_bytes([data[off], data[off + 1]]) as i32;
            let dy = u16::from_le_bytes([data[off + 2], data[off + 3]]) as i32;
            off += 4;

            self.blit_to_surface(dst_id, dx, dy, w, h, &region);
            if let Some(dst) = self.surfaces.get(&dst_id) {
                if dst.mapped {
                    let ax = dst.output_x as i32 + dx;
                    let ay = dst.output_y as i32 + dy;
                    bitmaps.push(make_bitmap(ax, ay, w, h, region.clone()));
                }
            }
        }
    }

    fn on_cache_to_surface(&mut self, data: &[u8], bitmaps: &mut Vec<Bitmap>) {
        if data.len() < 6 {
            return;
        }
        let cache_slot = u16::from_le_bytes([data[0], data[1]]);
        let surf_id = u16::from_le_bytes([data[2], data[3]]);
        let dest_count = u16::from_le_bytes([data[4], data[5]]) as usize;

        let (ce_data, ce_w, ce_h) = match self.cache.get(&cache_slot) {
            Some(ce) => (ce.data.clone(), ce.width, ce.height),
            None => return,
        };

        let mut off = 6;
        for _ in 0..dest_count {
            if off + 4 > data.len() {
                break;
            }
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
        if data.len() < 2 {
            return;
        }
        let slot = u16::from_le_bytes([data[0], data[1]]);
        self.cache.remove(&slot);
    }

    // ── H.264 / AVC decode helpers ─────────────────────────────────────────────

    fn decode_avc420(&mut self, data: &[u8]) -> Option<(Vec<u8>, u32, u32, Vec<AvcRect>, bool)> {
        let stream = parse_avc420(data)?;
        let regions = stream.regions;
        let uses_region_fifo = self
            .avc_dec
            .as_ref()
            .map(|dec| dec.uses_region_fifo())
            .unwrap_or(false);

        if uses_region_fifo {
            self.pending_avc_regions_queue.push_back(regions.clone());
        }

        if let Some(ref mut dec) = self.avc_dec {
            let hints: Vec<(u16, u16, u16, u16)> = regions
                .iter()
                .map(|r| (r.left, r.top, r.right, r.bottom))
                .collect();
            dec.set_region_hint(&hints);
        }

        let result = self.decode_h264(&stream.h264_data);

        let (flushed, drain_count) = if let Some(ref mut dec) = self.avc_dec {
            (dec.take_decoder_flushed(), dec.take_drain_count())
        } else {
            (false, 0)
        };

        if flushed {
            let queue_len_before = self.pending_avc_regions_queue.len();
            self.pending_avc_regions_queue.clear();
            if uses_region_fifo {
                self.pending_avc_regions_queue.push_back(regions.clone());
            }
            log::debug!(
                "[rdpgfx] decode_avc420: decoder flushed during decode — cleared FIFO (had {} entries), re-pushed current regions",
                queue_len_before
            );
        }

        let is_mismatch = self.avc_dec_take_full_blit();

        let effective_regions = if drain_count > 0 {
            if uses_region_fifo {
                let mut popped = None;
                for _ in 0..drain_count {
                    popped = self.pending_avc_regions_queue.pop_front();
                }
                popped.unwrap_or_else(|| regions.clone())
            } else {
                regions.clone()
            }
        } else {
            regions.clone()
        };

        if result.is_none() {
            return None;
        }

        result.map(|(pixels, w, h)| (pixels, w, h, effective_regions, is_mismatch))
    }

    fn decode_avc444(&mut self, data: &[u8]) -> Option<(Vec<u8>, u32, u32, Vec<AvcRect>, bool)> {
        let parsed = parse_avc444(data)?;
        let lc = parsed.lc;

        // LC=2: Chroma-upgrade frame — combine cached luma with aux chroma planes.
        if lc == 2 {
            return self.decode_avc444_lc2(parsed.stream2?);
        }

        // LC=0 or LC=1: main YUV420 stream decode.
        let stream = parsed.stream1?;
        log::trace!(
            "[rdpgfx] decode_avc444: lc={} h264_len={}",
            lc,
            stream.h264_data.len()
        );
        let regions = stream.regions;
        let uses_region_fifo = self
            .avc_dec
            .as_ref()
            .map(|dec| dec.uses_region_fifo())
            .unwrap_or(false);

        // Some decoders can emit delayed frames that belong to older packets,
        // so their dirty regions must be tracked through a FIFO. Others stay
        // aligned with the current packet, so applying a FIFO causes obvious
        // region mismatches (stale rects on fresh frames).
        if uses_region_fifo {
            self.pending_avc_regions_queue.push_back(regions.clone());
        }

        // Give the decoder a region hint so it can skip converting pixels outside
        // the dirty area. For VT (uses_region_fifo=false) the current packet's
        // regions always match the decoded frame, so the hint is always valid.
        if let Some(ref mut dec) = self.avc_dec {
            let hints: Vec<(u16, u16, u16, u16)> = regions
                .iter()
                .map(|r| (r.left, r.top, r.right, r.bottom))
                .collect();
            dec.set_region_hint(&hints);
        }

        // When the secondary decoder is present, use decode_i420 to also capture
        // the luma plane for future LC=2 combines; otherwise use normal decode.
        let result: Option<(Vec<u8>, u32, u32)> = if self.avc_dec2.is_some() {
            if let Some(ref mut dec) = self.avc_dec {
                // Cache IDR for SW fallback priming.
                if is_h264_keyframe(&stream.h264_data) {
                    self.last_stream1_idr.clear();
                    self.last_stream1_idr.extend_from_slice(&stream.h264_data);
                }
                let r = dec.decode_i420(&stream.h264_data);
                if dec.needs_keyframe() || dec.take_request_keyframe() {
                    log::debug!("[rdpgfx] AVC decoder needs IDR — scheduling force refresh");
                    self.needs_force_refresh = true;
                }
                if dec.take_hw_stalled() {
                    self.on_hw_stall();
                }
                if let Some((bgra, w, h, i420)) = r {
                    // Cache luma and chroma planes for LC=2 combine.
                    self.avc444_y_cache = Some(Avc444YCache {
                        y: i420.y,
                        y_stride: i420.y_stride,
                        u: i420.u,
                        u_stride: i420.u_stride,
                        v: i420.v,
                        v_stride: i420.v_stride,
                        width: i420.width,
                        height: i420.height,
                        full_range: i420.full_range,
                    });
                    Some((bgra, w, h))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            self.decode_h264(&stream.h264_data)
        };

        // Prime the auxiliary decoder with LC=0 stream2 data so that subsequent
        // LC=2 P-frames have a reference IDR in their context.
        if lc == 0 {
            if let Some(ref s2) = parsed.stream2 {
                if !s2.h264_data.is_empty() {
                    self.prime_aux_decoder(&s2.h264_data);
                }
            }
        }

        // If the decoder flushed its internal buffers during this decode call
        // (avcodec_flush_buffers was called), all previously buffered frames were
        // discarded.  Clear the FIFO so the stale entries (from packets whose frames
        // were flushed away) are not used.  Then re-push the current packet's regions
        // so they are available when the packet's frame eventually drains.
        let (flushed, drain_count) = if let Some(ref mut dec) = self.avc_dec {
            (dec.take_decoder_flushed(), dec.take_drain_count())
        } else {
            (false, 0)
        };

        if flushed {
            let queue_len_before = self.pending_avc_regions_queue.len();
            self.pending_avc_regions_queue.clear();
            if uses_region_fifo {
                self.pending_avc_regions_queue.push_back(regions.clone());
            }
            log::debug!(
                "[rdpgfx] decode_avc444: decoder flushed during decode — cleared FIFO (had {} entries), re-pushed current regions",
                queue_len_before
            );
        }

        let is_mismatch = self.avc_dec_take_full_blit();

        // Pop the FIFO once per drained frame — the returned frame corresponds to
        // the *last* drained frame from this decode() call. If we only pop once
        // after draining multiple frames, the queue stays offset and later packet
        // regions get paired with stale decoded output.
        let effective_regions = if drain_count > 0 {
            if uses_region_fifo {
                let mut popped = None;
                for _ in 0..drain_count {
                    popped = self.pending_avc_regions_queue.pop_front();
                }
                log::trace!(
                    "[rdpgfx] decode_avc444: drain_happened — drained={} fifo_depth_after={} used_regions={:?} result={}",
                    drain_count,
                    self.pending_avc_regions_queue.len(),
                    popped.as_ref().map(|r| r.len()),
                    if result.is_some() { "frame" } else { "suppressed" }
                );
                popped.unwrap_or_else(|| regions.clone())
            } else {
                log::trace!(
                    "[rdpgfx] decode_avc444: drain_happened — drained={} current packet regions={} result={}",
                    drain_count,
                    regions.len(),
                    if result.is_some() {
                        "frame"
                    } else {
                        "suppressed"
                    }
                );
                regions.clone()
            }
        } else {
            log::trace!(
                "[rdpgfx] decode_avc444: EAGAIN (lc={} h264_len={} regions={} fifo_depth={} mismatch_flag={})",
                lc,
                stream.h264_data.len(),
                regions.len(),
                if uses_region_fifo { self.pending_avc_regions_queue.len() } else { 0 },
                is_mismatch
            );
            regions.clone()
        };

        // No frame decoded (EAGAIN or dark-frame suppression with no result):
        // skip the blit; the display shows the previous frame.
        if result.is_none() {
            return None;
        }

        result.map(|(pixels, w, h)| (pixels, w, h, effective_regions, is_mismatch))
    }

    /// Prime the auxiliary decoder with the LC=0 stream2 IDR data so future
    /// LC=2 P-frames have a valid reference frame in the decoder's context.
    fn prime_aux_decoder(&mut self, h264_data: &[u8]) {
        if let Some(ref mut dec2) = self.avc_dec2 {
            let _ = dec2.decode(h264_data);
            log::trace!("[rdpgfx] primed aux decoder with LC=0 stream2 IDR ({} bytes)", h264_data.len());
        }
    }

    /// Decode an AVC444 LC=2 chroma-upgrade frame.
    ///
    /// Implements the AVC444v2 chroma reconstruction from [MS-RDPEGFX 3.3.8.3.3].
    /// Stream2 carries chroma values for positions not covered by stream1's 4:2:0
    /// quantiser, split across three Bx areas of the auxiliary I420 frame:
    ///
    ///   B4/B5 — aux Y plane: odd-column Cb/Cr at all rows
    ///   B6/B7 — aux U plane: even-column (col%4==0) Cb/Cr at odd rows
    ///   B8/B9 — aux V plane: even-column (col%4==2) Cb/Cr at odd rows
    ///
    /// Even-column, even-row positions use stream1's cached half-res U/V planes.
    fn decode_avc444_lc2(
        &mut self,
        aux_stream: Avc420Stream,
    ) -> Option<(Vec<u8>, u32, u32, Vec<AvcRect>, bool)> {
        log::trace!(
            "[rdpgfx] decode_avc444_lc2: h264_len={} regions={}",
            aux_stream.h264_data.len(),
            aux_stream.regions.len()
        );
        if self.avc_dec2.is_none() {
            log::trace!("[rdpgfx] decode_avc444_lc2: no aux decoder, skipping");
            return None;
        }
        if self.avc444_y_cache.is_none() {
            log::trace!("[rdpgfx] decode_avc444_lc2: no Y cache yet, skipping");
            return None;
        }

        let regions = aux_stream.regions;
        let i420_opt = if let Some(ref mut dec2) = self.avc_dec2 {
            let r = dec2.decode_i420(&aux_stream.h264_data);
            if dec2.needs_keyframe() || dec2.take_request_keyframe() {
                log::debug!("[rdpgfx] aux AVC decoder needs IDR — will wait for next LC=0");
            }
            r
        } else {
            None
        };

        let (_bgra_aux, _aw, _ah, i420) = i420_opt?;

        // Detect uninitialised or corrupted auxiliary chroma.  Windows Server
        // initialises stream2 IDR with Cb≈0/Cr≈0; combining zero chroma with
        // any luma produces BGRA(0,135,0,255) — a bright green frame.
        // Near-saturation (≥235) indicates DPB mismatch / aux decoder
        // corruption and produces a pink/magenta overlay instead.  Skip the
        // combine in both cases and wait for valid chroma data.
        let stream2_is_idr = is_h264_keyframe(&aux_stream.h264_data);
        if is_aux_chroma_blank(&i420) {
            log::debug!("[rdpgfx] AVC444 LC=2 skipped (stream2 chroma invalid: near-zero or near-saturated)");
            if !stream2_is_idr {
                // P-frame with corrupt chroma: the aux decoder's DPB has
                // diverged from the server's reference.  Reset it now so the
                // corruption does not cascade into subsequent LC=2 frames.
                // Recovery happens automatically on the next stream2 IDR
                // delivered in an LC=0 packet.
                log::debug!("[rdpgfx] aux decoder reset after P-frame blank chroma (DPB cascade prevention)");
                self.avc_dec2 = None;
            }
            return None;
        }

        let cache = self.avc444_y_cache.as_ref()?;

        let w = cache.width;
        let h = cache.height;
        log::trace!(
            "[rdpgfx] decode_avc444_lc2: combine {}x{} full_range={}",
            w,
            h,
            cache.full_range
        );

        let bgra = combine_avc444v2_bgra(
            &cache.y,
            cache.y_stride,
            &cache.u,
            cache.u_stride,
            &cache.v,
            cache.v_stride,
            &i420,
            w,
            h,
            cache.full_range,
        );

        Some((bgra, w, h, regions, false))
    }

    #[allow(dead_code)]
    fn avc_dec_take_full_blit(&mut self) -> bool {
        if let Some(ref mut dec) = self.avc_dec {
            return dec.take_full_blit();
        }
        false
    }

    fn avc_supports_nv12(&self) -> bool {
        self.avc_dec.as_ref().map(|d| d.supports_nv12()).unwrap_or(false)
    }

    fn decode_h264_nv12(&mut self, h264_data: &[u8]) -> Option<NV12Frame> {
        if is_h264_keyframe(h264_data) {
            self.last_stream1_idr.clear();
            self.last_stream1_idr.extend_from_slice(h264_data);
        }

        if let Some(ref mut dec) = self.avc_dec {
            let result = dec.decode_nv12(h264_data);
            let _ = dec.take_decoder_flushed();
            let _ = dec.take_drain_count();
            let _ = dec.take_full_blit();
            if dec.needs_keyframe() || dec.take_request_keyframe() {
                log::debug!("[rdpgfx] AVC decoder needs IDR — scheduling force refresh");
                self.needs_force_refresh = true;
            }
            if dec.take_hw_stalled() {
                self.on_hw_stall();
            }
            return result;
        }
        None
    }

    fn decode_avc420_nv12(&mut self, data: &[u8]) -> Option<NV12Frame> {
        let stream = parse_avc420(data)?;
        if let Some(ref mut dec) = self.avc_dec {
            let hints: Vec<(u16, u16, u16, u16)> = stream.regions.iter()
                .map(|r| (r.left, r.top, r.right, r.bottom))
                .collect();
            dec.set_region_hint(&hints);
        }
        self.decode_h264_nv12(&stream.h264_data)
    }

    fn decode_avc444_nv12(&mut self, data: &[u8]) -> Option<NV12Frame> {
        let parsed = parse_avc444(data)?;
        let stream = parsed.stream1?;
        if let Some(ref mut dec) = self.avc_dec {
            let hints: Vec<(u16, u16, u16, u16)> = stream.regions.iter()
                .map(|r| (r.left, r.top, r.right, r.bottom))
                .collect();
            dec.set_region_hint(&hints);
        }
        self.decode_h264_nv12(&stream.h264_data)
    }

    /// Called when the primary AVC decoder has switched from VideoToolbox (HW)
    /// to FFmpeg software decode after a permanent stall.
    ///
    /// Primes the new SW decoder with the last cached stream1 IDR so it can
    /// decode subsequent P-frames immediately without waiting for the server to
    /// send a fresh IDR (AVC444 servers typically only send an IDR at session
    /// start).  Also requests a force-refresh so the server sends a new IDR to
    /// clean up any transient artifacts from the stale prime.
    ///
    /// Matches grdp's `maybeNotifyDecoderBroken` HWStall soft-reset path
    /// (plugin/rdpgfx/avc.go).
    fn on_hw_stall(&mut self) {
        self.soft_reset_count += 1;
        self.using_sw_fallback = true;
        log::debug!(
            "[rdpgfx] HW→SW fallback (soft_reset={}): priming SW decoder with cached IDR",
            self.soft_reset_count
        );
        // Clear the region FIFO: the decoder state was reset; stale FIFO
        // entries would pair the primed IDR frame with wrong dirty rects.
        self.pending_avc_regions_queue.clear();
        // Prime the SW decoder with the last IDR so it can decode P-frames
        // without waiting for a server-sent keyframe.
        let idr = self.last_stream1_idr.clone();
        if !idr.is_empty() {
            if let Some(ref mut dec) = self.avc_dec {
                log::debug!(
                    "[rdpgfx] priming SW fallback decoder with cached IDR ({} bytes)",
                    idr.len()
                );
                let _ = dec.decode(&idr);
            }
        } else {
            log::debug!("[rdpgfx] no cached IDR to prime SW decoder — waiting for server IDR");
        }
        // Request a fresh IDR from the server to heal any artifacts from the stale prime.
        self.needs_force_refresh = true;
    }

    fn decode_h264(&mut self, h264_data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        // Cache IDR packets so the SW fallback decoder can be primed immediately
        // after a VideoToolbox stall without waiting for a fresh server IDR.
        if is_h264_keyframe(h264_data) {
            self.last_stream1_idr.clear();
            self.last_stream1_idr.extend_from_slice(h264_data);
        }

        if let Some(ref mut dec) = self.avc_dec {
            let result = dec.decode(h264_data);
            // Check unconditionally: needs_keyframe() may stay true while
            // VideoToolbox is silent. Checking here ensures force-refresh PDUs
            // are retried by the caller's rate limiter until decoding resumes.
            // Also check take_request_keyframe() for soft stalls (10 consecutive
            // EAGAINs without a send_packet failure): VT keeps accepting P-frames
            // but stops producing output.  A force-refresh triggers an IDR so
            // VT can resync.  The 2-second rate limiter in client.rs prevents flooding.
            if dec.needs_keyframe() || dec.take_request_keyframe() {
                log::debug!("[rdpgfx] AVC decoder needs IDR — scheduling force refresh");
                self.needs_force_refresh = true;
            }
            if dec.take_hw_stalled() {
                self.on_hw_stall();
            }
            log::trace!(
                "[rdpgfx] AVC decode {} bytes → {}",
                h264_data.len(),
                if result.is_some() { "frame" } else { "none" }
            );
            return result;
        }
        log::debug!("[rdpgfx] H.264 data received but no AVC decoder configured");
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
            if dy < 0 || dy >= s.height as i32 {
                continue;
            }
            let src_off = (row * w * 4) as usize;
            let dst_off = (dy * stride + x * 4) as usize;
            let n = (w * 4) as usize;
            if src_off + n <= src.len() && dst_off + n <= s.data.len() {
                s.data[dst_off..dst_off + n].copy_from_slice(&src[src_off..src_off + n]);
            }
        }
    }
}

fn should_use_avc_regions(regions: &[AvcRect], frame_w: i32, frame_h: i32) -> bool {
    if frame_w <= 0 || frame_h <= 0 {
        return false;
    }
    let total = frame_w * frame_h;
    if total == 0 {
        return false;
    }
    // Sum region areas.  If the total dirty area reaches 60% of the frame,
    // fall back to a single full-frame blit (matching grdp's 60% threshold).
    let mut sum = 0i32;
    for rc in regions {
        if rc.right <= rc.left || rc.bottom <= rc.top {
            continue;
        }
        let w = (rc.right - rc.left) as i32;
        let h = (rc.bottom - rc.top) as i32;
        sum = sum.saturating_add(w.saturating_mul(h));
        if sum.saturating_mul(100) >= total.saturating_mul(60) {
            return false;
        }
    }
    sum > 0
}

fn avc_regions_area(regions: &[AvcRect]) -> i32 {
    let mut sum = 0i32;
    for rc in regions {
        if rc.right <= rc.left || rc.bottom <= rc.top {
            continue;
        }
        let w = (rc.right - rc.left) as i32;
        let h = (rc.bottom - rc.top) as i32;
        sum = sum.saturating_add(w.saturating_mul(h));
    }
    sum
}

fn avc_regions_bounds(regions: &[AvcRect]) -> Option<(i32, i32, i32, i32)> {
    let mut left = i32::MAX;
    let mut top = i32::MAX;
    let mut right = i32::MIN;
    let mut bottom = i32::MIN;
    let mut seen = false;

    for rc in regions {
        if rc.right <= rc.left || rc.bottom <= rc.top {
            continue;
        }
        seen = true;
        left = left.min(rc.left as i32);
        top = top.min(rc.top as i32);
        right = right.max(rc.right as i32);
        bottom = bottom.max(rc.bottom as i32);
    }

    if seen {
        Some((left, top, right, bottom))
    } else {
        None
    }
}

/// Blit a decoded AVC frame into the surface and emit bitmap updates.
/// For small AVC dirty regions, only copy those regions. Full-frame blits can
/// overwrite unchanged desktop areas when the decoder output only contains the
/// updated macroblocks for a window transition.
///
/// Blit a decoded AVC frame to the surface.
/// Stale-frame suppression is handled entirely in C (`stale_frames_remaining`):
/// the decoder returns NULL for stale frames so this function is only called
/// with live content.
#[allow(clippy::too_many_arguments)]
fn blit_avc_frame(
    surf_id: u16,
    surfaces: &mut std::collections::HashMap<u16, Surface>,
    bitmaps: &mut Vec<Bitmap>,
    pixels: &[u8],
    fw: i32,
    fh: i32, // decoded frame dimensions (may include macroblock padding)
    ew: i32,
    eh: i32, // effective (clipped to dest rect) dimensions
    dx: i32,
    dy: i32, // destination offset on surface
    ax: i32,
    ay: i32, // absolute screen position
    force_regions: bool,
    regions: &[AvcRect],
) {
    let use_regions = should_use_avc_regions(regions, ew, eh);
    let region_area = avc_regions_area(regions);
    let frame_area = ew.saturating_mul(eh);
    let coverage_pct = if frame_area > 0 { region_area * 100 / frame_area } else { 0 };
    log::debug!(
        "[rdpgfx] blit_avc_frame surf={} frame={}x{} eff={}x{} abs=({},{}) regions={} region_area={} frame_area={} coverage={}% use_regions={} force_regions={} bounds={:?}",
        surf_id, fw, fh, ew, eh, ax, ay,
        regions.len(), region_area, frame_area, coverage_pct,
        use_regions, force_regions,
        avc_regions_bounds(regions)
    );

    if force_regions && !regions.is_empty() {
        // NOTE: force_regions=true means the decoder's full_blit flag was set
        // (decoded frame came from an older packet whose dirty regions are unknown).
        // Ideally this path should do a FULL blit, but currently forces region blit.
        // This can leave areas outside the current packet's regions unpainted (black).
        log::debug!(
            "[rdpgfx] force_regions=true → region blit (full_blit flag was set by decoder; may cause black regions if current packet regions don't cover full surface)"
        );
        blit_avc_regions(
            surf_id, surfaces, bitmaps, pixels, fw, fh, ew, eh, dx, dy, ax, ay, regions,
        );
        return;
    }

    if use_regions {
        log::debug!(
            "[rdpgfx] use_regions=true → region blit (coverage {}% < 60%)",
            coverage_pct
        );
        blit_avc_regions(
            surf_id, surfaces, bitmaps, pixels, fw, fh, ew, eh, dx, dy, ax, ay, regions,
        );
        return;
    }

    let cropped = crop_bgra(pixels, fw, fh, ew, eh);
    if let Some(s) = surfaces.get_mut(&surf_id) {
        let stride = s.width as i32 * 4;
        for row in 0..eh {
            let sy = dy + row;
            if sy < 0 || sy >= s.height as i32 {
                continue;
            }
            let src_off = (row * ew * 4) as usize;
            let dst_off = (sy * stride + dx * 4) as usize;
            let n = (ew * 4) as usize;
            if src_off + n <= cropped.len() && dst_off + n <= s.data.len() {
                s.data[dst_off..dst_off + n].copy_from_slice(&cropped[src_off..src_off + n]);
            }
        }
    }
    log::trace!(
        "[rdpgfx] emit AVC full surf={} surface=({},{} {}x{}) abs=({},{}) frame={}x{} regions={} region_area={} frame_area={} bounds={:?}",
        surf_id,
        dx,
        dy,
        ew,
        eh,
        ax,
        ay,
        fw,
        fh,
        regions.len(),
        avc_regions_area(regions),
        ew.saturating_mul(eh),
        avc_regions_bounds(regions)
    );
    bitmaps.push(make_bitmap(ax, ay, ew, eh, cropped));
}

#[allow(clippy::too_many_arguments)]
fn blit_avc_regions(
    surf_id: u16,
    surfaces: &mut std::collections::HashMap<u16, Surface>,
    bitmaps: &mut Vec<Bitmap>,
    pixels: &[u8],
    fw: i32,
    fh: i32,
    ew: i32,
    eh: i32,
    dx: i32,
    dy: i32,
    ax: i32,
    ay: i32,
    regions: &[AvcRect],
) {
    let frame_stride = fw * 4;
    let Some(s) = surfaces.get_mut(&surf_id) else {
        return;
    };
    let surf_stride = s.width as i32 * 4;

    // Log how much of the surface the regions cover
    let surf_area = (s.width as i32).saturating_mul(s.height as i32);
    let reg_area = avc_regions_area(regions);
    let reg_cov_pct = if surf_area > 0 { reg_area * 100 / surf_area } else { 0 };
    log::debug!(
        "[rdpgfx] blit_avc_regions surf={} surface={}x{} frame={}x{} eff={}x{} abs=({},{}) regions={} region_area={} surf_area={} surf_coverage={}% bounds={:?}",
        surf_id, s.width, s.height, fw, fh, ew, eh, ax, ay,
        regions.len(), reg_area, surf_area, reg_cov_pct,
        avc_regions_bounds(regions)
    );

    for rc in regions {
        if rc.right <= rc.left || rc.bottom <= rc.top {
            continue;
        }
        let rx = rc.left as i32;
        let ry = rc.top as i32;
        if rx >= ew || ry >= eh || rx >= fw || ry >= fh {
            continue;
        }
        let mut rw = (rc.right - rc.left) as i32;
        let mut rh = (rc.bottom - rc.top) as i32;
        rw = rw.min(ew - rx).min(fw - rx);
        rh = rh.min(eh - ry).min(fh - ry);
        rw = rw.min(s.width as i32 - dx - rx);
        rh = rh.min(s.height as i32 - dy - ry);
        if rw <= 0 || rh <= 0 {
            continue;
        }

        let row_bytes = (rw * 4) as usize;
        let mut region = vec![0u8; (rw * rh * 4) as usize];
        for row in 0..rh {
            let src_off = ((ry + row) * frame_stride + rx * 4) as usize;
            if src_off + row_bytes > pixels.len() {
                break;
            }

            let dst_region_off = (row * rw * 4) as usize;
            region[dst_region_off..dst_region_off + row_bytes]
                .copy_from_slice(&pixels[src_off..src_off + row_bytes]);

            let sy = dy + ry + row;
            let sx = dx + rx;
            if sy < 0 || sy >= s.height as i32 || sx < 0 || sx >= s.width as i32 {
                continue;
            }
            let dst_off = (sy * surf_stride + sx * 4) as usize;
            if dst_off + row_bytes <= s.data.len() {
                s.data[dst_off..dst_off + row_bytes]
                    .copy_from_slice(&pixels[src_off..src_off + row_bytes]);
            }
        }

        if log::log_enabled!(log::Level::Trace) {
            // Sample the first pixel of this region from the decoded frame (BGRA).
            let sample_off = ((ry * frame_stride) + rx * 4) as usize;
            let (fb, fg, fr) = if sample_off + 2 < pixels.len() {
                (
                    pixels[sample_off],
                    pixels[sample_off + 1],
                    pixels[sample_off + 2],
                )
            } else {
                (0, 0, 0)
            };
            // Sample the centre pixel too.
            let cx = rx + rw / 2;
            let cy = ry + rh / 2;
            let csample_off = ((cy * frame_stride) + cx * 4) as usize;
            let (cb, cg, cr) = if csample_off + 2 < pixels.len() {
                (
                    pixels[csample_off],
                    pixels[csample_off + 1],
                    pixels[csample_off + 2],
                )
            } else {
                (0, 0, 0)
            };
            log::trace!(
                "[rdpgfx] emit AVC region surf={} surface=({},{} {}x{}) abs=({},{}) frame_px0=RGB({},{},{}) frame_ctr=RGB({},{},{})",
                surf_id,
                dx + rx, dy + ry, rw, rh,
                ax + rx, ay + ry,
                fr, fg, fb,
                cr, cg, cb,
            );
        }
        bitmaps.push(make_bitmap(ax + rx, ay + ry, rw, rh, region));
    }
}

#[derive(Clone)]
struct AvcRect {
    left: u16,
    top: u16,
    right: u16,
    bottom: u16,
}

struct Avc420Stream {
    regions: Vec<AvcRect>,
    h264_data: Vec<u8>,
}

fn parse_avc420(data: &[u8]) -> Option<Avc420Stream> {
    if data.len() < 4 {
        log::warn!("[rdpgfx] parse_avc420: data too short ({})", data.len());
        return None;
    }
    let num_regions = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if num_regions > 65536 {
        log::warn!(
            "[rdpgfx] parse_avc420: num_regions={} too large",
            num_regions
        );
        return None;
    }
    // 4 header + 8 bytes rect + 2 bytes quant per region
    let meta_size = 4 + num_regions * 10;
    if meta_size > data.len() {
        log::warn!(
            "[rdpgfx] parse_avc420: meta_size={} > data.len()={}",
            meta_size,
            data.len()
        );
        return None;
    }
    log::debug!(
        "[rdpgfx] parse_avc420: num_regions={} meta_size={} h264_data_len={}",
        num_regions,
        meta_size,
        data.len() - meta_size
    );

    let mut regions = Vec::with_capacity(num_regions);
    let mut off = 4usize;
    // Region rects come first (8 bytes each), then quant/quality (2 bytes each)
    for _ in 0..num_regions {
        let left = u16::from_le_bytes([data[off], data[off + 1]]);
        let top = u16::from_le_bytes([data[off + 2], data[off + 3]]);
        let right = u16::from_le_bytes([data[off + 4], data[off + 5]]);
        let bottom = u16::from_le_bytes([data[off + 6], data[off + 7]]);
        regions.push(AvcRect {
            left,
            top,
            right,
            bottom,
        });
        off += 8;
    }
    // skip quant/quality bytes (2 per region)
    Some(Avc420Stream {
        regions,
        h264_data: data[meta_size..].to_vec(),
    })
}

/// Result of parsing an AVC444/AVC444v2 packet.
struct ParsedAvc444 {
    /// Main YUV420 H.264 stream (lc=0 or lc=1).  `None` when lc=2.
    stream1: Option<Avc420Stream>,
    /// Auxiliary chroma stream (lc=0 or lc=2).  `None` when lc=1.
    stream2: Option<Avc420Stream>,
    lc: u8,
}

fn parse_avc444(data: &[u8]) -> Option<ParsedAvc444> {
    if data.len() < 4 {
        eprintln!("[rdpgfx] parse_avc444: data too short ({})", data.len());
        return None;
    }
    let cb_field = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let lc = ((cb_field >> 30) & 0x03) as u8;
    let cb_stream1 = (cb_field & 0x3FFF_FFFF) as usize;
    let rest = &data[4..];
    log::trace!(
        "[rdpgfx] parse_avc444: data={} lc={} cb_stream1={} rest={}",
        data.len(),
        lc,
        cb_stream1,
        rest.len()
    );
    match lc {
        0 => {
            // lc=0: Both streams present; stream1 = H264 YUV420 base layer, stream2 = aux chroma.
            if cb_stream1 > rest.len() {
                eprintln!(
                    "[rdpgfx] parse_avc444: lc=0 cb_stream1={} > rest={} → None",
                    cb_stream1,
                    rest.len()
                );
                return None;
            }
            let s1 = parse_avc420(&rest[..cb_stream1])?;
            let aux_data = &rest[cb_stream1..];
            log::trace!(
                "[rdpgfx] parse_avc444: lc=0 stream1={} stream2={} regions={} h264_len={} bounds={:?}",
                cb_stream1,
                aux_data.len(),
                s1.regions.len(),
                s1.h264_data.len(),
                avc_regions_bounds(&s1.regions)
            );
            // Parse stream2 if present (may be empty when no aux data)
            let s2 = if !aux_data.is_empty() { parse_avc420(aux_data) } else { None };
            Some(ParsedAvc444 { stream1: Some(s1), stream2: s2, lc })
        }
        1 => {
            // lc=1: Main stream only.  cb_stream1==0 means "all of rest" (grdp behaviour).
            let stream_data = if cb_stream1 == 0 || cb_stream1 > rest.len() {
                rest
            } else {
                &rest[..cb_stream1]
            };
            let s = parse_avc420(stream_data)?;
            log::trace!(
                "[rdpgfx] parse_avc444: lc=1 stream={} regions={} h264_len={} bounds={:?}",
                stream_data.len(),
                s.regions.len(),
                s.h264_data.len(),
                avc_regions_bounds(&s.regions)
            );
            Some(ParsedAvc444 { stream1: Some(s), stream2: None, lc })
        }
        2 => {
            // lc=2: Auxiliary chroma-upgrade frame only (P-frame in aux stream).
            // stream2 Y plane = Cb (full W×H), stream2 U plane = Cr (W/2×H/2).
            // Combine with cached Y plane from the last lc=0/1 main decode.
            let aux_data = if cb_stream1 == 0 || cb_stream1 > rest.len() {
                rest
            } else {
                &rest[..cb_stream1]
            };
            log::trace!(
                "[rdpgfx] parse_avc444: lc=2 aux_len={} cb_stream1={}",
                aux_data.len(),
                cb_stream1
            );
            let s2 = parse_avc420(aux_data)?;
            Some(ParsedAvc444 { stream1: None, stream2: Some(s2), lc })
        }
        _ => {
            eprintln!("[rdpgfx] parse_avc444: unknown lc={} → None", lc);
            None
        }
    }
}

// ── Utility helpers ────────────────────────────────────────────────────────────

/// Returns `true` when the auxiliary I420 frame's Y plane contains uninitialised
/// or corrupted chroma data.
///
/// In AVC444v2 the Y plane of the auxiliary stream2 carries Cb (left half) and
/// Cr (right half) rather than luma.  Windows Server initialises stream2 IDR
/// frames with Cb≈0/Cr≈0, and only refreshes regions that change.  Combining
/// zero chroma with any luma produces BGRA(0,135,0,255) — a bright green frame.
/// Near-saturation (≥235) indicates DPB mismatch in the aux decoder and
/// produces a pink/magenta overlay instead.
///
/// The check samples 6 positions spread across each half of the Y plane and
/// considers the frame blank when a majority of those positions fall outside
/// the valid chroma range [20, 235).
fn is_aux_chroma_blank(i420: &crate::avc::I420Frame) -> bool {
    let w = i420.width as usize;
    let h = i420.height as usize;
    if w < 16 || h < 4 || i420.y.is_empty() {
        return false;
    }
    let stride = i420.y_stride;
    let half_w = w / 2;
    const LO: u8 = 20;  // below this: near-zero (uninitialised)
    const HI: u8 = 235; // at or above this: near-saturated (corrupted DPB)
    let (mut near_zero, mut near_sat, mut total) = (0usize, 0usize, 0usize);
    for i in 0..6usize {
        let row = (i + 1) * h / 7;
        let col = (i + 1) * half_w / 7;
        if row >= h || col >= half_w {
            continue;
        }
        if let Some(&v) = i420.y.get(row * stride + col) {
            total += 1;
            if v < LO { near_zero += 1; }
            else if v >= HI { near_sat += 1; }
        }
        if half_w + col < w {
            if let Some(&v2) = i420.y.get(row * stride + half_w + col) {
                total += 1;
                if v2 < LO { near_zero += 1; }
                else if v2 >= HI { near_sat += 1; }
            }
        }
    }
    if total == 0 {
        return false;
    }
    near_zero * 2 > total || near_sat * 2 > total
}

/// Combine an AVC444 luma plane (from the main LC=0/1 stream) with the chroma
/// planes from the auxiliary LC=2 stream into a BGRA pixel buffer.
///
/// BT.709 conversion (limited range unless `full_range` is true):
///   Limited: c = y-16; r=(298c+459v+128)>>8, g=(298c-55u-136v+128)>>8, b=(298c+541u+128)>>8
///   Full:    r=(256y+403v+128)>>8, g=(256y-48u-120v+128)>>8, b=(256y+475u+128)>>8
///
/// Output is BGRA with alpha=255.
/// Implements the AVC444v2 chroma reconstruction defined in
/// [MS-RDPEGFX 3.3.8.3.3] ("YUV420p Stream Combination for YUV444v2 mode").
///
/// Stream2 encodes the chroma positions that stream1's 4:2:0 quantiser discards,
/// split across three Bx areas of the auxiliary I420 frame:
///
///   B4/B5 — aux Y plane, each row:
///     bytes [0,   w/2)  = Cb at all odd-x columns  (col=2k+1, any row)
///     bytes [w/2, w)    = Cr at all odd-x columns
///
///   B6/B7 — aux U plane, each half-height row j:
///     bytes [0,    w/4) = Cb at col=4k,   odd row (2j+1)
///     bytes [w/4,  w/2) = Cr at col=4k,   odd row
///
///   B8/B9 — aux V plane, each half-height row j:
///     bytes [0,    w/4) = Cb at col=4k+2, odd row (2j+1)
///     bytes [w/4,  w/2) = Cr at col=4k+2, odd row
///
/// Even-column, even-row positions use stream1's cached half-res U/V planes (B2/B3).
fn combine_avc444v2_bgra(
    y: &[u8],
    y_stride: usize,
    cached_u: &[u8],  // Cb from stream1, half-res
    cached_u_stride: usize,
    cached_v: &[u8],  // Cr from stream1, half-res
    cached_v_stride: usize,
    aux: &crate::avc::I420Frame, // decoded stream2
    width: u32,
    height: u32,
    full_range: bool,
) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut out = vec![0u8; w * h * 4];

    let half_w = w / 2;
    let quarter_w = w / 4;

    for row in 0..h {
        let y_row = &y[row * y_stride..];
        let aux_y_row = &aux.y[row * aux.y_stride..];
        let uv_row = row >> 1;

        for col in 0..w {
            let y_val = y_row[col];
            let (cb, cr): (u8, u8) = if col & 1 == 1 {
                // Odd column: B4/B5 from aux Y plane.
                let k = col >> 1;
                (aux_y_row[k], aux_y_row[half_w + k])
            } else if row & 1 == 0 {
                // Even column, even row: B2/B3 from stream1 cached chroma.
                let uv_col = col >> 1;
                (
                    cached_u.get(uv_row * cached_u_stride + uv_col).copied().unwrap_or(128),
                    cached_v.get(uv_row * cached_v_stride + uv_col).copied().unwrap_or(128),
                )
            } else {
                // Even column, odd row.
                let k = col >> 2;
                if col & 2 == 0 {
                    // col % 4 == 0: B6/B7 from aux U plane.
                    (
                        aux.u.get(uv_row * aux.u_stride + k).copied().unwrap_or(128),
                        aux.u.get(uv_row * aux.u_stride + quarter_w + k).copied().unwrap_or(128),
                    )
                } else {
                    // col % 4 == 2: B8/B9 from aux V plane.
                    (
                        aux.v.get(uv_row * aux.v_stride + k).copied().unwrap_or(128),
                        aux.v.get(uv_row * aux.v_stride + quarter_w + k).copied().unwrap_or(128),
                    )
                }
            };

            let u = cb as i32 - 128;
            let v = cr as i32 - 128;
            let y_i = y_val as i32;

            // BT.709 coefficients (correct for HD content ≥ 720p).
            let (r, g, b) = if full_range {
                let r = (256 * y_i + 403 * v + 128) >> 8;
                let g = (256 * y_i - 48 * u - 120 * v + 128) >> 8;
                let b = (256 * y_i + 475 * u + 128) >> 8;
                (r, g, b)
            } else {
                let c = y_i - 16;
                let r = (298 * c + 459 * v + 128) >> 8;
                let g = (298 * c - 55 * u - 136 * v + 128) >> 8;
                let b = (298 * c + 541 * u + 128) >> 8;
                (r, g, b)
            };

            let dst = (row * w + col) * 4;
            out[dst]     = b.clamp(0, 255) as u8;
            out[dst + 1] = g.clamp(0, 255) as u8;
            out[dst + 2] = r.clamp(0, 255) as u8;
            out[dst + 3] = 255;
        }
    }
    out
}

fn make_bitmap(x: i32, y: i32, w: i32, h: i32, data: Vec<u8>) -> Bitmap {
    Bitmap {
        dest_left: x,
        dest_top: y,
        dest_right: x + w - 1,
        dest_bottom: y + h - 1,
        width: w,
        height: h,
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

fn emit_tile_rects(
    surf: &Surface,
    rects: &[(usize, usize, usize, usize)],
    bitmaps: &mut Vec<Bitmap>,
) {
    if !surf.mapped || rects.is_empty() {
        return;
    }
    let sw = surf.width as usize;
    let sh = surf.height as usize;
    for &(rx, ry, rw, rh) in rects {
        let pixels = copy_surface_rect(&surf.data, sw, sh, rx, ry, rw, rh);
        let abs_x = surf.output_x as i32 + rx as i32;
        let abs_y = surf.output_y as i32 + ry as i32;
        bitmaps.push(make_bitmap(abs_x, abs_y, rw as i32, rh as i32, pixels));
    }
}

fn copy_surface_rect(
    data: &[u8],
    surf_w: usize,
    surf_h: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    for row in 0..h {
        let sy = y + row;
        if sy >= surf_h {
            break;
        }
        let sx = x;
        let sw_len = w.min(surf_w.saturating_sub(sx));
        let src_idx = (sy * surf_w + sx) * 4;
        let dst_idx = row * w * 4;
        let bytes = sw_len * 4;
        if src_idx + bytes <= data.len() && dst_idx + bytes <= out.len() {
            out[dst_idx..dst_idx + bytes].copy_from_slice(&data[src_idx..src_idx + bytes]);
        }
    }
    out
}

// ── PDU builders ──────────────────────────────────────────────────────────────

/// Build RDPGFX_CAPS_ADVERTISE_PDU (client→server).
/// Advertises multiple capability sets so the server can pick the highest it supports.
pub fn build_caps_advertise() -> Vec<u8> {
    build_caps_advertise_with_options(false)
}

fn build_caps_advertise_with_options(disable_avc444: bool) -> Vec<u8> {
    let mut caps = Vec::new();

    let push_cap = |caps: &mut Vec<u8>, version: u32, flags: u32| {
        caps.extend_from_slice(&version.to_le_bytes());
        caps.extend_from_slice(&4u32.to_le_bytes()); // capsDataLength
        caps.extend_from_slice(&flags.to_le_bytes());
    };

    if disable_avc444 {
        caps.extend_from_slice(&2u16.to_le_bytes()); // capsSetCount = 2
        push_cap(&mut caps, CAP_VERSION_8, CAP_FLAG_THIN_CLIENT);
        push_cap(
            &mut caps,
            CAP_VERSION_81,
            CAP_FLAG_SMALL_CACHE | CAP_FLAG_AVC420_ENABLED,
        );
    } else {
        caps.extend_from_slice(&11u16.to_le_bytes()); // capsSetCount = 11
        push_cap(&mut caps, CAP_VERSION_8, CAP_FLAG_THIN_CLIENT);
        push_cap(
            &mut caps,
            CAP_VERSION_81,
            CAP_FLAG_SMALL_CACHE | CAP_FLAG_AVC420_ENABLED,
        );
        push_cap(&mut caps, CAP_VERSION_10, CAP_FLAG_SMALL_CACHE);
        caps.extend_from_slice(&CAP_VERSION_101.to_le_bytes());
        caps.extend_from_slice(&16u32.to_le_bytes());
        caps.extend_from_slice(&[0u8; 16]);
        push_cap(&mut caps, CAP_VERSION_102, CAP_FLAG_SMALL_CACHE);
        push_cap(&mut caps, CAP_VERSION_103, 0);
        push_cap(&mut caps, CAP_VERSION_104, CAP_FLAG_SMALL_CACHE);
        push_cap(&mut caps, CAP_VERSION_105, CAP_FLAG_SMALL_CACHE);
        push_cap(&mut caps, CAP_VERSION_106, CAP_FLAG_SMALL_CACHE);
        push_cap(&mut caps, 0x000A0601, CAP_FLAG_SMALL_CACHE);
        push_cap(&mut caps, CAP_VERSION_107, CAP_FLAG_SMALL_CACHE);
    }

    let pdu_len = (GFX_HEADER_SIZE + caps.len()) as u32;
    let mut pdu = Vec::with_capacity(pdu_len as usize);
    pdu.extend_from_slice(&CMDID_CAPS_ADVERTISE.to_le_bytes());
    pdu.extend_from_slice(&0u16.to_le_bytes()); // flags
    pdu.extend_from_slice(&pdu_len.to_le_bytes());
    pdu.extend_from_slice(&caps);
    pdu
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advertised_versions(pdu: &[u8]) -> Vec<u32> {
        let count = u16::from_le_bytes([pdu[8], pdu[9]]) as usize;
        let mut offset = 10;
        let mut versions = Vec::with_capacity(count);
        for _ in 0..count {
            let version = u32::from_le_bytes([
                pdu[offset],
                pdu[offset + 1],
                pdu[offset + 2],
                pdu[offset + 3],
            ]);
            let data_len = u32::from_le_bytes([
                pdu[offset + 4],
                pdu[offset + 5],
                pdu[offset + 6],
                pdu[offset + 7],
            ]) as usize;
            versions.push(version);
            offset += 8 + data_len;
        }
        assert_eq!(offset, pdu.len());
        versions
    }

    #[test]
    fn disabling_avc444_advertises_only_avc420_capabilities() {
        let pdu = build_caps_advertise_with_options(true);
        assert_eq!(advertised_versions(&pdu), [CAP_VERSION_8, CAP_VERSION_81]);
        assert_eq!(
            u32::from_le_bytes([pdu[30], pdu[31], pdu[32], pdu[33]]),
            CAP_FLAG_SMALL_CACHE | CAP_FLAG_AVC420_ENABLED
        );
    }

    #[test]
    fn default_caps_advertisement_still_includes_avc444_capabilities() {
        let versions = advertised_versions(&build_caps_advertise());
        assert_eq!(versions.len(), 11);
        assert_eq!(versions.last(), Some(&CAP_VERSION_107));
    }
}

/// Build RDPGFX_FRAME_ACKNOWLEDGE_PDU (client→server).
/// queue_depth: number of frames still queued in client decode pipeline.
/// total_decoded: cumulative number of frames decoded since connection start.
fn build_frame_ack(frame_id: u32, queue_depth: u32, total_decoded: u32) -> Vec<u8> {
    let mut pdu = vec![0u8; 20];
    pdu[0..2].copy_from_slice(&CMDID_FRAME_ACKNOWLEDGE.to_le_bytes());
    // pdu[2..4] = flags = 0
    pdu[4..8].copy_from_slice(&20u32.to_le_bytes()); // pduLength
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

/// Returns true when the H.264 Annex-B bitstream contains an IDR (keyframe) slice.
fn is_h264_keyframe(data: &[u8]) -> bool {
    let mut i = 0;
    while i < data.len() {
        if i + 4 < data.len() && data[i] == 0 && data[i+1] == 0 && data[i+2] == 0 && data[i+3] == 1 {
            if data[i+4] & 0x1F == 5 { return true; }
            i += 4;
        } else if i + 3 < data.len() && data[i] == 0 && data[i+1] == 0 && data[i+2] == 1 {
            if data[i+3] & 0x1F == 5 { return true; }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}
