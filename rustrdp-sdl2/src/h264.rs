/// H.264 decoder backed by the [`ffmpeg-next`] crate.
///
/// This keeps rdp-core codec-agnostic while using FFmpeg's H.264 decoder via
/// Rust bindings. On macOS, VideoToolbox hardware acceleration is used for
/// zero-latency decode. Otherwise, FFmpeg SW decode is used.
/// Frames are converted to BGRA so they can be blitted directly
/// into the RDP surface buffers.
use ffmpeg::codec;
use ffmpeg::codec::threading;
use ffmpeg::ffi;
use ffmpeg::software::scaling::{context::Context as ScalingContext, flag::Flags as ScalingFlags};
#[cfg(target_arch = "aarch64")]
use ffmpeg::util::color::Range as ColorRange;
use ffmpeg::util::format::pixel::Pixel;
use ffmpeg::util::frame::video::Video as VideoFrame;
use ffmpeg_next as ffmpeg;
use rustrdp_core::avc::AvcDecoder;
use std::sync::OnceLock;

/// After this many consecutive receive_frame EAGAINs (VT not producing output),
/// ask the server for an IDR keyframe while continuing to feed the decoder.
/// Do not flush here: Windows/RDPGFX servers may keep sending only P-frames,
/// and flushing would discard the references needed to decode them.
///
/// VideoToolbox typically needs 4-6 input frames before producing its first
/// output frame (pipeline fill latency).  A threshold of 10 prevents false
/// stall detection during normal VT startup or after an IDR.
const EAGAIN_FLUSH_THRESHOLD: usize = 10;

/// After this many consecutive EAGAINs, VideoToolbox is considered permanently
/// stalled and the decoder is switched to FFmpeg software decode.  At ~8 VT
/// packets per second this is roughly 7–8 seconds, matching grdp's
/// `avcHWReadyFreezeThreshold` (7 s).  The `hw_stalled` flag is set so that
/// the caller (RdpgfxHandler) can prime the new SW decoder with the cached IDR.
const STALL_SW_FALLBACK_THRESHOLD: usize = 60;

/// Maximum number of non-IDR packets to drop while waiting for a keyframe
/// after a real decoder reset/flush.  Matches grdp's keyframeWaitLimit.
const KEYFRAME_WAIT_LIMIT: usize = 900;

/// Silent FFmpeg log callback — suppresses all output to stderr.
///
/// FFmpeg's internal codec open/reinit (triggered by SPS changes during
/// `avcodec_send_packet`) can reset the global `av_log_level` back to the
/// default (AV_LOG_WARNING), bypassing `av_log_set_level` calls made after
/// decoder creation.  Installing a no-op callback is the only reliable way
/// to silence those spurious warnings (e.g. "number of reference frames
/// exceeds max") for non-conformant RDP server streams.
unsafe extern "C" fn silent_log_callback(
    _avcl: *mut std::ffi::c_void,
    _level: std::ffi::c_int,
    _fmt: *const std::ffi::c_char,
    _vl: ffi::va_list,
) {
}

/// `get_format` callback for VideoToolbox HW acceleration.
/// Iterates the list of offered pixel formats and returns
/// `AV_PIX_FMT_VIDEOTOOLBOX` if present, otherwise the first offered format.
unsafe extern "C" fn get_hw_format_vt(
    _ctx: *mut ffi::AVCodecContext,
    pix_fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    let mut p = pix_fmts;
    while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        if *p == ffi::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX {
            return ffi::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX;
        }
        p = p.add(1);
    }
    *pix_fmts
}

fn init_ffmpeg() -> Result<(), ffmpeg::Error> {
    static INIT: OnceLock<Result<(), ffmpeg::Error>> = OnceLock::new();
    INIT.get_or_init(|| {
        let result = ffmpeg::init();
        // Install a no-op log callback so FFmpeg never writes to stderr.
        // set_level(Error) alone is insufficient: avcodec_open2 and codec
        // reinit paths inside avcodec_send_packet can reset the global
        // av_log_level back to AV_LOG_WARNING, letting warnings like
        // "number of reference frames exceeds max" leak through.
        unsafe { ffi::av_log_set_callback(Some(silent_log_callback)) };
        result
    })
    .clone()
}

/// Returns true if the H.264 bitstream contains a NAL unit that requires
/// flushing the decoder's DPB before submission:
///   - NAL type 5: IDR slice (full-screen refresh, resets reference frames)
///   - NAL type 7: SPS (encoding-parameter change may increase DPB size)
///
/// Both cases cause FFmpeg to reinitialise its internal decoder state.  If we
/// don't flush first, the new frames pile up in the DPB alongside buffered
/// old frames, causing a growing region-FIFO offset that maps each decoded
/// frame to the wrong dirty rect.
fn packet_needs_decoder_flush(data: &[u8]) -> bool {
    #[allow(unused_mut)] // mut is needed only in debug builds (cfg(debug_assertions) branch)
    let mut found = false;
    let mut i = 0usize;
    while i + 4 <= data.len() {
        if data[i] == 0x00 && data[i + 1] == 0x00 {
            let nal_byte = if data[i + 2] == 0x01 && i + 3 < data.len() {
                i += 3;
                data[i]
            } else if data[i + 2] == 0x00 && data[i + 3] == 0x01 && i + 4 < data.len() {
                i += 4;
                data[i]
            } else {
                i += 1;
                continue;
            };
            let nal_type = nal_byte & 0x1F;
            if nal_type <= 12 {
                log::trace!("[h264] packet NAL type={} at offset {}", nal_type, i);
            }
            // IDR (5) or SPS (7) — both require a DPB flush.
            if nal_type == 5 || nal_type == 7 {
                // Don't break — log all leading NAL types in debug mode.
                #[cfg(not(debug_assertions))]
                return true;
                #[cfg(debug_assertions)]
                {
                    found = true;
                }
            }
        }
        i += 1;
    }
    found
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn bt709_bgra(y_raw: u8, u_raw: u8, v_raw: u8, full_range: bool) -> [u8; 4] {
    let y = y_raw as i32;
    let u = u_raw as i32 - 128;
    let v = v_raw as i32 - 128;
    // BT.709 coefficients (correct for HD content ≥ 720p).
    let (r, g, b) = if full_range {
        (
            (256 * y + 403 * v + 128) >> 8,
            (256 * y - 48 * u - 120 * v + 128) >> 8,
            (256 * y + 475 * u + 128) >> 8,
        )
    } else {
        let c = y - 16;
        (
            (298 * c + 459 * v + 128) >> 8,
            (298 * c - 55 * u - 136 * v + 128) >> 8,
            (298 * c + 541 * u + 128) >> 8,
        )
    };
    [clamp_u8(b), clamp_u8(g), clamp_u8(r), 255]
}

#[cfg(target_arch = "aarch64")]
fn frame_to_bgra_direct(decoded: &VideoFrame, regions: &[(u16, u16, u16, u16)]) -> Option<Vec<u8>> {
    let width = decoded.width() as usize;
    let height = decoded.height() as usize;
    let full_range =
        decoded.format() == Pixel::YUVJ420P || decoded.color_range() == ColorRange::JPEG;
    let mut out = vec![0u8; width * height * 4];

    match decoded.format() {
        Pixel::YUV420P | Pixel::YUVJ420P => {
            let y = decoded.data(0);
            let u = decoded.data(1);
            let v = decoded.data(2);
            let y_stride = decoded.stride(0);
            let u_stride = decoded.stride(1);
            let v_stride = decoded.stride(2);

            for row in 0..height {
                let y_row = &y[row * y_stride..];
                let u_row = &u[(row / 2) * u_stride..];
                let v_row = &v[(row / 2) * v_stride..];
                let dst_row = &mut out[row * width * 4..(row + 1) * width * 4];
                for col in 0..width {
                    let px = bt709_bgra(y_row[col], u_row[col / 2], v_row[col / 2], full_range);
                    let off = col * 4;
                    dst_row[off..off + 4].copy_from_slice(&px);
                }
            }
            Some(out)
        }
        Pixel::NV12 => {
            let y = decoded.data(0);
            let uv = decoded.data(1);
            let y_stride = decoded.stride(0);
            let uv_stride = decoded.stride(1);

            // On ARM64, swscale's non-accelerated NV12→BGRA path ignores
            // sws_setColorspaceDetails, producing a green cast (same issue
            // as for YUV420P on ARM64, documented in grdp h264_ffmpeg.go).
            // Always use the Rust BT.709 path here — never fall through to
            // swscale for NV12 on ARM64, even for full-frame blits.
            let frame_area = width * height;
            let region_area: usize = regions
                .iter()
                .map(|&(l, t, r, b)| {
                    (r as usize).saturating_sub(l as usize)
                        * (b as usize).saturating_sub(t as usize)
                })
                .sum();
            let use_region_path =
                !regions.is_empty() && region_area.saturating_mul(100) < frame_area * 60;

            if use_region_path {
                // Region-aware path: only convert pixels within dirty rects.
                for &(left, top, right, bottom) in regions {
                    let row_start = top as usize;
                    let row_end = (bottom as usize).min(height);
                    let col_start = left as usize;
                    let col_end = (right as usize).min(width);
                    for row in row_start..row_end {
                        let y_row = &y[row * y_stride..];
                        let uv_row = &uv[(row / 2) * uv_stride..];
                        let dst_row = &mut out[row * width * 4..];
                        for col in col_start..col_end {
                            let uv_off = (col / 2) * 2;
                            let px = bt709_bgra(
                                y_row[col],
                                uv_row[uv_off],
                                uv_row[uv_off + 1],
                                full_range,
                            );
                            let off = col * 4;
                            dst_row[off..off + 4].copy_from_slice(&px);
                        }
                    }
                }
            } else {
                // Full-frame path: convert every pixel.
                for row in 0..height {
                    let y_row = &y[row * y_stride..];
                    let uv_row = &uv[(row / 2) * uv_stride..];
                    let dst_row = &mut out[row * width * 4..];
                    for col in 0..width {
                        let uv_off = (col / 2) * 2;
                        let px = bt709_bgra(
                            y_row[col],
                            uv_row[uv_off],
                            uv_row[uv_off + 1],
                            full_range,
                        );
                        let off = col * 4;
                        dst_row[off..off + 4].copy_from_slice(&px);
                    }
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// Sample the centre pixel of a decoded frame, returning (Y, U, V).
/// Works for YUV420P / YUVJ420P (planar) and NV12 (semi-planar).
#[allow(dead_code)]
fn sample_yuv_center(frame: &VideoFrame) -> (u8, u8, u8) {
    let cx = frame.width() as usize / 2;
    let cy = frame.height() as usize / 2;
    match frame.format() {
        Pixel::YUV420P | Pixel::YUVJ420P => {
            let y = frame.data(0).get(cy * frame.stride(0) + cx).copied().unwrap_or(0);
            let u = frame.data(1).get((cy / 2) * frame.stride(1) + cx / 2).copied().unwrap_or(128);
            let v = frame.data(2).get((cy / 2) * frame.stride(2) + cx / 2).copied().unwrap_or(128);
            (y, u, v)
        }
        Pixel::NV12 => {
            let y = frame.data(0).get(cy * frame.stride(0) + cx).copied().unwrap_or(0);
            let uvx = (cx / 2) * 2;
            let uvy = cy / 2;
            let u = frame.data(1).get(uvy * frame.stride(1) + uvx).copied().unwrap_or(128);
            let v = frame.data(1).get(uvy * frame.stride(1) + uvx + 1).copied().unwrap_or(128);
            (y, u, v)
        }
        _ => (0, 128, 128),
    }
}

pub struct H264Decoder {
    decoder: ffmpeg::codec::decoder::Video,
    /// True when VideoToolbox hardware acceleration is active.
    use_hw: bool,
    scaler: Option<ScalingContext>,
    scaler_src_format: Pixel,
    scaler_width: u32,
    scaler_height: u32,
    /// Dirty regions for the next frame (set via set_region_hint).
    /// When non-empty, NV12→BGRA conversion is limited to these rects.
    region_hint: Vec<(u16, u16, u16, u16)>,
    /// True immediately after `reset()`, until the flag is consumed by
    /// `take_decoder_flushed()`.
    decoder_flushed: bool,
    /// True immediately after `reset()`, until consumed by `take_full_blit()`.
    /// Causes the next frame to be blitted to the entire surface rather than
    /// just the dirty regions.
    full_blit: bool,
    /// Number of frames drained by the last `decode()` call. Consumed by
    /// `take_drain_count()`.
    drain_count: usize,
    needs_keyframe: bool,
    /// Non-IDR packets dropped while waiting for an IDR after a decoder flush.
    /// When this reaches KEYFRAME_WAIT_LIMIT the wait is abandoned so the
    /// screen doesn't freeze indefinitely (see KEYFRAME_WAIT_LIMIT comment).
    keyframe_wait_count: usize,
    /// True when VT is silent and we want the core to send force-refresh PDUs,
    /// but the decoder has not been flushed and must keep receiving P-frames.
    request_keyframe: bool,
    /// Consecutive decode() calls that produced no frame (both early_frame and
    /// post_frame returned None).  When this exceeds EAGAIN_FLUSH_THRESHOLD for
    /// the HW (VT) path, the decoder is flushed and a keyframe is requested so
    /// the display recovers from a VT backpressure stall.
    consecutive_eagain: usize,
    /// Set when consecutive_eagain reaches STALL_SW_FALLBACK_THRESHOLD: the
    /// VideoToolbox session has stalled irrecoverably and the decoder has been
    /// recreated in software (FFmpeg) mode.  Callers should prime the new SW
    /// decoder with the last cached IDR and request a force-refresh.
    hw_stalled: bool,
    /// True after decoder creation / flush.  When set, decoded frames are
    /// sampled at the centre pixel and dropped if the chroma is zero (U=0 AND
    /// V=0).  VideoToolbox sometimes returns an uninitialised (all-zero)
    /// IOSurface as the first frame after decoder init or
    /// avcodec_flush_buffers.  BT.601 limited-range conversion of
    /// (Y=0, U=0, V=0) produces BGRA(0,135,0,255) — a bright green frame.
    /// Valid H.264 chroma always centres on 128, so U=0 && V=0 at the
    /// centre is an unambiguous signal of an uninitialised buffer.
    /// Cleared once the first frame with valid chroma arrives.
    /// Matches grdp's hwNeedsZeroCheck / needZeroCheck logic.
    hw_needs_zero_check: bool,
}

impl H264Decoder {
    pub fn new() -> Result<Self, ffmpeg::Error> {
        init_ffmpeg()?;
        let (decoder, use_hw) = Self::create_decoder()?;
        Ok(H264Decoder {
            decoder,
            use_hw,
            scaler: None,
            scaler_src_format: Pixel::None,
            scaler_width: 0,
            scaler_height: 0,
            region_hint: Vec::new(),
            decoder_flushed: false,
            full_blit: false,
            drain_count: 0,
            needs_keyframe: true,
            keyframe_wait_count: 0,
            request_keyframe: false,
            consecutive_eagain: 0,
            hw_stalled: false,
            hw_needs_zero_check: true,
        })
    }

    /// Convenience constructor that wraps the decoder in the trait-object box
    /// expected by `RdpSession::login`.  Returns `None` if initialisation
    /// failed (logged at WARN level).
    pub fn new_boxed() -> Option<Box<dyn AvcDecoder>> {
        match Self::new() {
            Ok(d) => Some(Box::new(d) as Box<dyn AvcDecoder>),
            Err(e) => {
                log::warn!("[h264] failed to create ffmpeg decoder: {}", e);
                None
            }
        }
    }

    fn create_decoder() -> Result<(ffmpeg::codec::decoder::Video, bool), ffmpeg::Error> {
        let codec = codec::decoder::find(codec::Id::H264).ok_or(ffmpeg::Error::DecoderNotFound)?;
        let mut context = codec::context::Context::new_with_codec(codec);
        context.set_threading(threading::Config::count(1));
        let mut use_hw = false;
        unsafe {
            let ctx = context.as_mut_ptr();
            (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).flags2 |= ffi::AV_CODEC_FLAG2_FAST as i32;

            // Attempt VideoToolbox hardware acceleration (macOS).  VT decodes
            // with zero output-buffering, eliminating the ~400ms DPB delay seen
            // with FFmpeg SW decode for RDP streams where the server signals
            // max_dec_frame_buffering=4 in the SPS.
            let mut hw_device_ctx: *mut ffi::AVBufferRef = std::ptr::null_mut();
            let ret = ffi::av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            );
            if ret >= 0 && !hw_device_ctx.is_null() {
                (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device_ctx);
                (*ctx).get_format = Some(get_hw_format_vt);
                ffi::av_buffer_unref(&mut hw_device_ctx);
                use_hw = true;
                eprintln!("[h264] VideoToolbox hardware acceleration enabled");
            } else {
                // SW decode: limit the decoded picture buffer (DPB) to 1 reference
                // frame so the decoder outputs each frame immediately instead of
                // accumulating up to max_dec_frame_buffering (often 8) frames from
                // the server's SPS.  RDP H.264 streams use sequential P-frames that
                // reference only the immediately preceding frame, so capping at 1 is
                // safe.  VT has its own zero-latency output and does not need this.
                (*ctx).refs = 1;
                eprintln!("[h264] VideoToolbox not available (ret={}), using SW decode", ret);
            }
        }
        let decoder = context.decoder().open_as(codec)?.video()?;
        Ok((decoder, use_hw))
    }

    /// Recreate the internal decoder, setting the flushed/full-blit flags so
    /// the next frame is blitted unconditionally and the region FIFO is reset.
    fn recreate_decoder(&mut self) {
        match Self::create_decoder() {
            Ok((d, hw)) => {
                self.decoder = d;
                self.use_hw = hw;
                self.scaler = None;
                self.scaler_src_format = Pixel::None;
                self.scaler_width = 0;
                self.scaler_height = 0;
                self.decoder_flushed = true;
                self.full_blit = true;
                self.needs_keyframe = true;
                self.keyframe_wait_count = 0;
                self.request_keyframe = false;
                self.consecutive_eagain = 0;
                self.hw_stalled = false;
                self.hw_needs_zero_check = hw; // VT needs zero-check; SW does not
            }
            Err(e) => {
                log::warn!("[h264] failed to recreate ffmpeg decoder: {}", e);
            }
        }
    }

    /// Recreate the decoder in software-only mode (no VideoToolbox).
    /// Called after a VT stall exceeds STALL_SW_FALLBACK_THRESHOLD.
    /// Sets `hw_stalled = true` so the caller can prime the new decoder
    /// with the last cached IDR and request a force-refresh from the server.
    fn recreate_as_sw(&mut self) {
        match Self::create_sw_decoder() {
            Ok(d) => {
                log::debug!("[h264] VT permanently stalled — switching to FFmpeg SW decode");
                self.decoder = d;
                self.use_hw = false;
                self.scaler = None;
                self.scaler_src_format = Pixel::None;
                self.scaler_width = 0;
                self.scaler_height = 0;
                self.decoder_flushed = true;
                self.full_blit = true;
                self.needs_keyframe = true;
                self.keyframe_wait_count = 0;
                self.request_keyframe = false;
                self.consecutive_eagain = 0;
                self.hw_stalled = true;
                self.hw_needs_zero_check = false; // SW decode does not produce zero IOSurfaces
            }
            Err(e) => {
                log::warn!("[h264] failed to create SW fallback decoder: {}", e);
            }
        }
    }

    /// Create a software-only FFmpeg H.264 decoder (no VideoToolbox).
    fn create_sw_decoder() -> Result<ffmpeg::codec::decoder::Video, ffmpeg::Error> {
        let codec = codec::decoder::find(codec::Id::H264).ok_or(ffmpeg::Error::DecoderNotFound)?;
        let mut context = codec::context::Context::new_with_codec(codec);
        context.set_threading(threading::Config::count(1));
        unsafe {
            let ctx = context.as_mut_ptr();
            (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).flags2 |= ffi::AV_CODEC_FLAG2_FAST as i32;
            // Limit DPB to 1 reference frame for low-latency SW decode.
            (*ctx).refs = 1;
        }
        let decoder = context.decoder().open_as(codec)?.video()?;
        Ok(decoder)
    }

    fn ensure_scaler(&mut self, decoded: &VideoFrame) -> Result<(), ffmpeg::Error> {
        let src_format = decoded.format();
        let width = decoded.width();
        let height = decoded.height();
        let needs_new = self.scaler.is_none()
            || self.scaler_src_format != src_format
            || self.scaler_width != width
            || self.scaler_height != height;
        if needs_new {
            self.scaler = Some(ScalingContext::get(
                src_format,
                width,
                height,
                Pixel::BGRA,
                width,
                height,
                ScalingFlags::FAST_BILINEAR,
            )?);
            self.scaler_src_format = src_format;
            self.scaler_width = width;
            self.scaler_height = height;
        }
        Ok(())
    }

    fn frame_to_bgra(&mut self, decoded: &VideoFrame) -> Result<Vec<u8>, ffmpeg::Error> {
        // On ARM64, swscale's non-accelerated YUV→BGRA paths ignore
        // sws_setColorspaceDetails, producing a green cast for both YUV420P and
        // NV12 (documented in grdp h264_ffmpeg.go).  Always use the hand-written
        // BT.709 Rust path on ARM64, regardless of debug/release build mode.
        // On x86_64, swscale is both correct (respects colorspace) and
        // SIMD-accelerated, so we continue to use it there.
        #[cfg(target_arch = "aarch64")]
        if let Some(out) = frame_to_bgra_direct(decoded, &self.region_hint) {
            return Ok(out);
        }
        self.ensure_scaler(decoded)?;
        let width = decoded.width() as usize;
        let height = decoded.height() as usize;
        let row_bytes = width * 4;
        let mut out = vec![0u8; row_bytes * height];
        // Write directly into `out` via sws_scale, bypassing the intermediate
        // stride-padded VideoFrame allocation and the subsequent row-copy loop.
        // This saves ~8 MB allocation + copy per frame at 1080p.
        let ret = unsafe {
            let scaler = self.scaler.as_mut().unwrap().as_mut_ptr();
            let src = decoded.as_ptr();
            let dst_ptrs = [out.as_mut_ptr()];
            let dst_strides = [row_bytes as i32];
            ffi::sws_scale(
                scaler,
                (*src).data.as_ptr() as *const *const u8,
                (*src).linesize.as_ptr(),
                0,
                height as i32,
                dst_ptrs.as_ptr() as *const *mut u8,
                dst_strides.as_ptr(),
            )
        };
        if ret < 0 {
            return Err(ffmpeg::Error::from(ret));
        }
        Ok(out)
    }

    /// Drain all available decoded frames from the decoder.
    /// Performs av_hwframe_transfer_data for VideoToolbox frames (GPU→CPU NV12).
    /// Returns the latest CPU-side sw_frame, or None if nothing is ready.
    fn take_decoded_frame_raw(&mut self) -> Option<VideoFrame> {
        let mut latest: Option<VideoFrame> = None;
        loop {
            let mut decoded = VideoFrame::empty();
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    let sw = if self.use_hw && decoded.format() == Pixel::VIDEOTOOLBOX {
                        log::trace!(
                            "[h264] VT frame received ({}x{}), transferring to CPU",
                            decoded.width(),
                            decoded.height()
                        );
                        let mut sw_frame = VideoFrame::empty();
                        unsafe {
                            (*sw_frame.as_mut_ptr()).format =
                                ffi::AVPixelFormat::AV_PIX_FMT_NONE as i32;
                            let ret = ffi::av_hwframe_transfer_data(
                                sw_frame.as_mut_ptr(),
                                decoded.as_ptr(),
                                0,
                            );
                            if ret < 0 {
                                log::warn!(
                                    "[h264] av_hwframe_transfer_data failed: {}",
                                    ret
                                );
                                self.recreate_decoder();
                                return latest;
                            }
                        }
                        sw_frame
                    } else {
                        if self.use_hw {
                            log::debug!(
                                "[h264] unexpected SW frame format={:?} (use_hw=true)",
                                decoded.format()
                            );
                        }
                        decoded
                    };
                    self.drain_count += 1;
                    latest = Some(sw);
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::ffi::EAGAIN => {
                    break;
                }
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => {
                    log::warn!("[h264] receive_frame error: {}", e);
                    self.recreate_decoder();
                    return None;
                }
            }
        }

        // Zero-UV detection: VideoToolbox sometimes returns an uninitialised
        // (all-zero) IOSurface as the first frame after decoder init or flush.
        // BT.601 limited-range conversion of (Y=0, U=0, V=0) produces
        // BGRA(0,135,0,255) — a bright green frame.  Valid H.264 chroma always
        // centres on 128, so U=0 && V=0 simultaneously is unambiguous corruption.
        // Drop the frame and keep hw_needs_zero_check set so we continue checking
        // until a frame with valid chroma arrives.  Matches grdp's hwNeedsZeroCheck.
        if self.hw_needs_zero_check {
            if let Some(ref frame) = latest {
                let (_y, u, v) = sample_yuv_center(frame);
                if u == 0 && v == 0 {
                    log::debug!(
                        "[h264] dropped zero-UV VT frame (uninitialised IOSurface, Y={} U={} V={})",
                        _y, u, v
                    );
                    return None; // keep hw_needs_zero_check=true for next frame
                }
                // Valid chroma: clear the check flag.
                self.hw_needs_zero_check = false;
                log::debug!(
                    "[h264] first valid VT frame (Y={} U={} V={}), zero-check cleared",
                    _y, u, v
                );
            }
        }

        latest
    }

    fn bgra_or_reset(&mut self, frame: &VideoFrame) -> Option<Vec<u8>> {
        match self.frame_to_bgra(frame) {
            Ok(pixels) => Some(pixels),
            Err(e) => {
                log::warn!("[h264] failed to convert frame to BGRA: {}", e);
                self.recreate_decoder();
                None
            }
        }
    }

    fn take_decoded_frame(&mut self) -> Option<(Vec<u8>, u32, u32)> {
        let sw = self.take_decoded_frame_raw()?;
        let width = sw.width();
        let height = sw.height();
        let pixels = self.bgra_or_reset(&sw)?;
        Some((pixels, width, height))
    }

    fn take_decoded_frame_nv12(&mut self) -> Option<rustrdp_core::avc::NV12Frame> {
        let sw = self.take_decoded_frame_raw()?;
        let width = sw.width();
        let height = sw.height();
        let (y, uv, y_stride, uv_stride) = extract_nv12_planes(&sw)?;
        Some(rustrdp_core::avc::NV12Frame {
            y,
            uv,
            width,
            height,
            y_stride,
            uv_stride,
            screen_x: 0,
            screen_y: 0,
        })
    }

    fn take_decoded_frame_with_i420(
        &mut self,
    ) -> Option<(Vec<u8>, u32, u32, rustrdp_core::avc::I420Frame)> {
        let sw = self.take_decoded_frame_raw()?;
        let width = sw.width();
        let height = sw.height();
        let i420 = extract_i420_from_frame(&sw)?;
        let pixels = self.bgra_or_reset(&sw)?;
        Some((pixels, width, height, i420))
    }
}

fn extract_nv12_planes(frame: &VideoFrame) -> Option<(Vec<u8>, Vec<u8>, usize, usize)> {
    let height = frame.height() as usize;
    let y_stride = frame.stride(0);
    let uv_stride = frame.stride(1);
    if y_stride == 0 || uv_stride == 0 || height == 0 {
        return None;
    }
    let y_data = frame.data(0);
    let uv_data = frame.data(1);
    let y_size = y_stride * height;
    let uv_size = uv_stride * (height / 2);
    if y_data.len() < y_size || uv_data.len() < uv_size {
        log::warn!(
            "[h264] NV12 frame data too small: y={}/{} uv={}/{}",
            y_data.len(), y_size, uv_data.len(), uv_size
        );
        return None;
    }
    Some((y_data[..y_size].to_vec(), uv_data[..uv_size].to_vec(), y_stride, uv_stride))
}

/// Extract I420 (planar YUV) from a decoded video frame.
///
/// Handles:
///   - YUV420P / YUVJ420P: data(0)=Y, data(1)=U, data(2)=V
///   - NV12 (VideoToolbox): data(0)=Y, data(1)=interleaved UV → deinterleave
///
/// Returns (I420Frame) or None on format mismatch.
fn extract_i420_from_frame(frame: &VideoFrame) -> Option<rustrdp_core::avc::I420Frame> {
    use ffmpeg::format::pixel::Pixel;
    let w = frame.width() as usize;
    let h = frame.height() as usize;
    if w == 0 || h == 0 {
        return None;
    }

    match frame.format() {
        Pixel::YUV420P | Pixel::YUVJ420P => {
            let full_range = frame.format() == Pixel::YUVJ420P;
            let y_stride = frame.stride(0);
            let u_stride = frame.stride(1);
            let v_stride = frame.stride(2);
            if y_stride == 0 || u_stride == 0 || v_stride == 0 {
                return None;
            }
            let y_data = frame.data(0);
            let u_data = frame.data(1);
            let v_data = frame.data(2);
            let y_size = y_stride * h;
            let u_size = u_stride * (h / 2);
            let v_size = v_stride * (h / 2);
            if y_data.len() < y_size || u_data.len() < u_size || v_data.len() < v_size {
                return None;
            }
            Some(rustrdp_core::avc::I420Frame {
                y: y_data[..y_size].to_vec(),
                u: u_data[..u_size].to_vec(),
                v: v_data[..v_size].to_vec(),
                y_stride,
                u_stride,
                v_stride,
                width: w as u32,
                height: h as u32,
                full_range,
            })
        }
        Pixel::NV12 => {
            // Deinterleave UV plane.
            let y_stride = frame.stride(0);
            let uv_stride = frame.stride(1);
            if y_stride == 0 || uv_stride == 0 {
                return None;
            }
            let y_data = frame.data(0);
            let uv_data = frame.data(1);
            let y_size = y_stride * h;
            let uv_size = uv_stride * (h / 2);
            if y_data.len() < y_size || uv_data.len() < uv_size {
                return None;
            }
            let uv_bytes = &uv_data[..uv_size];
            let u_stride = (w / 2 + 15) & !15; // 16-byte aligned
            let v_stride = u_stride;
            let mut u = vec![0u8; u_stride * (h / 2)];
            let mut v = vec![0u8; v_stride * (h / 2)];
            for row in 0..(h / 2) {
                let src_row = &uv_bytes[row * uv_stride..];
                let u_row = &mut u[row * u_stride..row * u_stride + w / 2];
                let v_row = &mut v[row * v_stride..row * v_stride + w / 2];
                for col in 0..(w / 2) {
                    u_row[col] = src_row.get(col * 2).copied().unwrap_or(128);
                    v_row[col] = src_row.get(col * 2 + 1).copied().unwrap_or(128);
                }
            }
            Some(rustrdp_core::avc::I420Frame {
                y: y_data[..y_size].to_vec(),
                u,
                v,
                y_stride,
                u_stride,
                v_stride,
                width: w as u32,
                height: h as u32,
                full_range: false,
            })
        }
        _ => None,
    }
}

impl AvcDecoder for H264Decoder {
    /// Decode one H.264 NAL packet.
    ///
    /// Returns `Some((bgra_pixels, width, height))` when a full frame is
    /// available, or `None` when the decoder needs more data.
    fn decode(&mut self, data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        self.drain_count = 0;
        let needs_flush = packet_needs_decoder_flush(data);

        // On IDR or SPS: flush the decoder's DPB before sending the packet.
        // The DPB holds buffered P-frames; if we let them drain lazily, the
        // new keyframe's decoded output gets paired with stale FIFO entries —
        // blitting the new content into a tiny dirty rect from several packets
        // ago and leaving the rest of the screen frozen.
        //
        // avcodec_flush_buffers() discards buffered frames immediately.
        // The decoder_flushed flag tells rdpgfx to clear and reset the region
        // FIFO, so the keyframe starts with a clean pairing.
        // Reset the consecutive-EAGAIN counter whenever we have a keyframe
        // packet (the decoder is either flushed here or will be on the next IDR
        // boundary, so a stale backpressure count is no longer meaningful).
        if needs_flush {
            self.consecutive_eagain = 0;
            self.keyframe_wait_count = 0;
            self.request_keyframe = false;
            self.decoder.flush(); // avcodec_flush_buffers
            self.decoder_flushed = true;
            log::debug!("[h264] DPB flush triggered before IDR/SPS packet");
        }

        // While waiting for an IDR after a decoder flush, drop non-keyframe
        // packets early — before send_packet and the consecutive_eagain counter.
        // Without this early exit, consecutive_eagain would keep incrementing
        // for every dropped P-frame and re-trigger a flush every 3 packets,
        // creating an infinite flush loop that permanently freezes the display.
        //
        // After KEYFRAME_WAIT_LIMIT drops we give up and attempt error-concealment
        // decode, matching grdp's keyframeWaitLimit behaviour: the server may
        // not send an IDR in the RDPGFX pipeline in response to SuppressOutput.
        if self.needs_keyframe {
            if !needs_flush {
                self.keyframe_wait_count += 1;
                if self.keyframe_wait_count < KEYFRAME_WAIT_LIMIT {
                    return None;
                }
                log::debug!(
                    "[h264] no IDR after {} packets; proceeding without keyframe (error concealment)",
                    KEYFRAME_WAIT_LIMIT
                );
            }
            // needs_flush → IDR/SPS present, or wait limit reached: clear the wait.
            self.needs_keyframe = false;
            self.keyframe_wait_count = 0;
        }

        // For VideoToolbox (async): drain any frames that VT completed since the
        // previous decode() call before pushing the new packet.  FFmpeg's VT
        // wrapper submits packets to VT asynchronously; receive_frame() right
        // after send_packet() often returns EAGAIN even though the frame will be
        // ready milliseconds later.  By draining *before* send_packet we surface
        // those completed frames without waiting an extra packet cycle, reducing
        // visible DPB latency from ~4 frames to ~1 frame.
        //
        // IMPORTANT: clear region_hint before capturing early_frame so the
        // NV12→BGRA conversion covers the full frame.  early_frame is from the
        // *previous* packet — its dirty regions are unknown (cleared after the
        // previous decode() call).  Using the current packet's (smaller) regions
        // here would leave most of the decoded frame as uninitialised zeroes,
        // causing visual corruption.  Restore the hint afterwards so post_frame
        // still benefits from the region optimisation.
        let early_frame = if self.use_hw && !needs_flush {
            let saved_hint = std::mem::take(&mut self.region_hint);
            let ef = self.take_decoded_frame();
            self.region_hint = saved_hint;
            ef
        } else {
            None
        };

        let packet = ffmpeg::Packet::copy(data);

        // AVERROR(EAGAIN) from send_packet means the packet was NOT accepted —
        // the decoder has buffered output that must be consumed first.
        // For VideoToolbox (HW): flush the decoder and wait for a keyframe,
        // matching grdp's behaviour (av_hwframe_transfer_data cannot unblock a
        // stalled VT session; a full flush + IDR is the only safe recovery).
        // For SW decode: drain the output buffer, then retry once.
        match self.decoder.send_packet(&packet) {
            Ok(()) => {}
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::ffi::EAGAIN => {
                if self.use_hw {
                    log::debug!("[h264] VT send_packet EAGAIN — flushing decoder, waiting for IDR");
                    self.decoder.flush();
                    self.decoder_flushed = true;
                    self.needs_keyframe = true;
                    self.request_keyframe = false;
                    self.keyframe_wait_count = 0;
                    self.consecutive_eagain = 0;
                    self.drain_count = 0; // early_frame is pre-flush, discard it
                    return None;
                }
                log::debug!("[h264] SW send_packet EAGAIN — draining and retrying");
                self.take_decoded_frame(); // drain whatever is buffered
                // Retry the packet now that the output buffer is free.
                if let Err(e) = self.decoder.send_packet(&packet) {
                    log::warn!("[h264] send_packet retry failed: {}", e);
                    self.recreate_decoder();
                    return None;
                }
            }
            Err(e) => {
                log::warn!("[h264] send_packet error: {}", e);
                self.recreate_decoder();
                return None;
            }
        }
        let post_frame = self.take_decoded_frame();
        self.region_hint.clear(); // hint consumed — clear for next packet

        // Prefer the frame decoded from the new packet (post_frame); fall back to
        // the earlier async frame if the new packet didn't produce output yet.
        // When falling back to early_frame, signal rdpgfx to blit the entire
        // surface: early_frame is from a previous packet and its correct dirty
        // regions are unknown — the current packet's regions would only update
        // a subset of the screen, leaving the rest stale.
        let frame = post_frame.or_else(|| {
            if early_frame.is_some() {
                self.full_blit = true;
            }
            early_frame
        });

        if let Some(frame) = frame {
            self.consecutive_eagain = 0; // frame produced — VT is keeping up
            self.request_keyframe = false;
            return Some(frame);
        }

        // Both early_frame and post_frame returned nothing — VT is stalled.
        // After EAGAIN_FLUSH_THRESHOLD consecutive occurrences on the HW path,
        // request an IDR so the server sends a fresh reference frame.  Keep
        // feeding P-frames: if the server ignores the request and never sends an
        // IDR, VT can still recover with the references it already has.
        // After STALL_SW_FALLBACK_THRESHOLD consecutive EAGAINs, VT is considered
        // permanently stalled; switch to FFmpeg SW decode (matching grdp's
        // HW-stall recovery path).
        if self.use_hw {
            self.consecutive_eagain += 1;
            if self.consecutive_eagain >= STALL_SW_FALLBACK_THRESHOLD {
                log::debug!(
                    "[h264] VT stalled ({} consecutive EAGAINs) — switching to SW fallback",
                    self.consecutive_eagain
                );
                self.recreate_as_sw();
            } else if self.consecutive_eagain >= EAGAIN_FLUSH_THRESHOLD && !self.request_keyframe {
                log::debug!(
                    "[h264] VT stalled ({} consecutive EAGAINs) — requesting IDR without flush",
                    self.consecutive_eagain
                );
                self.request_keyframe = true;
            }
        }

        None
    }

    /// Returns `true` while the decoder requires an IDR keyframe before it can
    /// produce output.  This is set at startup and after a decoder flush caused
    /// by a critical VT error (send_packet EAGAIN on the HW path).
    ///
    /// NOTE: `request_keyframe` (soft hint after consecutive EAGAINs) is
    /// intentionally NOT included here.  Including it caused an infinite
    /// force-refresh loop: every IDR flushed the VT pipeline, which took ~10
    /// frames to warm up (triggering request_keyframe again), which sent
    /// another force refresh 2 seconds later, repeating indefinitely.
    fn needs_keyframe(&self) -> bool {
        self.needs_keyframe
    }

    /// Returns `true` (and clears the flag) when a soft IDR request should be
    /// sent to the server.  Set after `EAGAIN_FLUSH_THRESHOLD` consecutive
    /// receive_frame EAGAINs.  Unlike `needs_keyframe`, P-frames continue to
    /// be accepted — only a force-refresh PDU is sent, no decoder flush.
    fn take_request_keyframe(&mut self) -> bool {
        let v = self.request_keyframe;
        self.request_keyframe = false;
        v
    }

    fn take_full_blit(&mut self) -> bool {
        let v = self.full_blit;
        self.full_blit = false;
        v
    }

    fn take_decoder_flushed(&mut self) -> bool {
        let v = self.decoder_flushed;
        self.decoder_flushed = false;
        v
    }

    fn take_drain_count(&mut self) -> usize {
        let v = self.drain_count;
        self.drain_count = 0;
        v
    }

    fn take_hw_stalled(&mut self) -> bool {
        let v = self.hw_stalled;
        self.hw_stalled = false;
        v
    }

    fn set_region_hint(&mut self, regions: &[(u16, u16, u16, u16)]) {
        self.region_hint.clear();
        self.region_hint.extend_from_slice(regions);
    }

    fn uses_region_fifo(&self) -> bool {
        // VT (hw) is zero-latency: every send_packet → receive_frame is immediate,
        // so the current packet's regions always match the decoded frame.
        // The FIFO is only needed for SW decode where the DPB buffers N frames.
        !self.use_hw
    }

    fn signal_screen_refreshed(&mut self) {
        // grdpsdl2 hit a similar freeze mode when the decoder got reset too
        // aggressively. Without the old helper's pipeline-elevated heuristic,
        // flushing here can strand the decoder waiting for a fresh IDR after
        // ordinary full-screen bitmap refreshes. Preserve decoder state and
        // let explicit decode errors / RESET_GRAPHICS drive recovery.
        self.drain_count = 0;
    }

    /// Reset the decoder to a clean state after RDPGFX RESET_GRAPHICS.
    fn reset(&mut self) {
        self.recreate_decoder();
    }

    fn decode_nv12(&mut self, data: &[u8]) -> Option<rustrdp_core::avc::NV12Frame> {
        self.drain_count = 0;
        let needs_flush = packet_needs_decoder_flush(data);

        if needs_flush {
            self.consecutive_eagain = 0;
            self.keyframe_wait_count = 0;
            self.request_keyframe = false;
            self.decoder.flush();
            self.decoder_flushed = true;
            log::debug!("[h264] DPB flush triggered before IDR/SPS packet");
        }

        if self.needs_keyframe && !needs_flush {
            self.keyframe_wait_count += 1;
            if self.keyframe_wait_count < KEYFRAME_WAIT_LIMIT {
                return None;
            }
            log::debug!(
                "[h264] no IDR after {} packets; proceeding without keyframe (error concealment)",
                KEYFRAME_WAIT_LIMIT
            );
            self.needs_keyframe = false;
            self.keyframe_wait_count = 0;
        }
        if self.needs_keyframe {
            self.needs_keyframe = false;
            self.keyframe_wait_count = 0;
        }

        let early_frame = if self.use_hw && !needs_flush {
            let saved_hint = std::mem::take(&mut self.region_hint);
            let ef = self.take_decoded_frame_nv12();
            self.region_hint = saved_hint;
            ef
        } else {
            None
        };

        let packet = ffmpeg::Packet::copy(data);

        match self.decoder.send_packet(&packet) {
            Ok(()) => {}
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::ffi::EAGAIN => {
                if self.use_hw {
                    log::debug!("[h264] VT send_packet EAGAIN — flushing decoder, waiting for IDR");
                    self.decoder.flush();
                    self.decoder_flushed = true;
                    self.needs_keyframe = true;
                    self.request_keyframe = false;
                    self.keyframe_wait_count = 0;
                    self.consecutive_eagain = 0;
                    self.drain_count = 0;
                    return None;
                }
                log::debug!("[h264] SW send_packet EAGAIN — draining and retrying");
                let _ = self.take_decoded_frame_raw();
                if let Err(e) = self.decoder.send_packet(&packet) {
                    log::warn!("[h264] send_packet retry failed: {}", e);
                    self.recreate_decoder();
                    return None;
                }
            }
            Err(e) => {
                log::warn!("[h264] send_packet error: {}", e);
                self.recreate_decoder();
                return None;
            }
        }
        let post_frame = self.take_decoded_frame_nv12();
        self.region_hint.clear();

        let frame = post_frame.or_else(|| {
            if early_frame.is_some() {
                self.full_blit = true;
            }
            early_frame
        });

        if let Some(frame) = frame {
            self.consecutive_eagain = 0;
            self.request_keyframe = false;
            return Some(frame);
        }

        if self.use_hw {
            self.consecutive_eagain += 1;
            if self.consecutive_eagain >= STALL_SW_FALLBACK_THRESHOLD {
                log::debug!(
                    "[h264] decode_nv12: VT stalled ({} consecutive EAGAINs) — switching to SW fallback",
                    self.consecutive_eagain
                );
                self.recreate_as_sw();
            } else if self.consecutive_eagain >= EAGAIN_FLUSH_THRESHOLD && !self.request_keyframe {
                log::debug!(
                    "[h264] VT stalled ({} consecutive EAGAINs) — requesting IDR without flush",
                    self.consecutive_eagain
                );
                self.request_keyframe = true;
            }
        }

        None
    }

    fn supports_nv12(&self) -> bool {
        self.use_hw
    }

    /// Decode one H.264 NAL packet, returning BGRA pixels and I420 planar data.
    ///
    /// Used for AVC444 LC=2 chroma-upgrade support: the caller caches the I420
    /// Y plane from lc=0/1 main-stream decodes and later combines it with the
    /// auxiliary stream's chroma planes.
    ///
    /// Shares the same flush, early-drain and EAGAIN logic as `decode()`.
    fn decode_i420(
        &mut self,
        data: &[u8],
    ) -> Option<(Vec<u8>, u32, u32, rustrdp_core::avc::I420Frame)> {
        self.drain_count = 0;
        let needs_flush = packet_needs_decoder_flush(data);

        if needs_flush {
            self.consecutive_eagain = 0;
            self.keyframe_wait_count = 0;
            self.request_keyframe = false;
            self.decoder.flush();
            self.decoder_flushed = true;
            log::debug!("[h264] decode_i420: DPB flush triggered before IDR/SPS packet");
        }

        if self.needs_keyframe && !needs_flush {
            self.keyframe_wait_count += 1;
            if self.keyframe_wait_count < KEYFRAME_WAIT_LIMIT {
                return None;
            }
            log::debug!(
                "[h264] decode_i420: no IDR after {} packets; proceeding without keyframe",
                KEYFRAME_WAIT_LIMIT
            );
            self.needs_keyframe = false;
            self.keyframe_wait_count = 0;
        }
        if self.needs_keyframe {
            self.needs_keyframe = false;
            self.keyframe_wait_count = 0;
        }

        // VT pre-drain: capture any async frame completed since last decode.
        let early_result: Option<(Vec<u8>, u32, u32, rustrdp_core::avc::I420Frame)> =
            if self.use_hw && !needs_flush {
                let saved_hint = std::mem::take(&mut self.region_hint);
                let ef = self.take_decoded_frame_with_i420();
                self.region_hint = saved_hint;
                ef
            } else {
                None
            };

        let packet = ffmpeg::Packet::copy(data);

        match self.decoder.send_packet(&packet) {
            Ok(()) => {}
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::ffi::EAGAIN => {
                if self.use_hw {
                    log::debug!("[h264] decode_i420: VT send_packet EAGAIN — flushing, waiting for IDR");
                    self.decoder.flush();
                    self.decoder_flushed = true;
                    self.needs_keyframe = true;
                    self.request_keyframe = false;
                    self.keyframe_wait_count = 0;
                    self.consecutive_eagain = 0;
                    self.drain_count = 0;
                    return None;
                }
                log::debug!("[h264] decode_i420: SW send_packet EAGAIN — draining and retrying");
                let _ = self.take_decoded_frame_raw();
                if let Err(e) = self.decoder.send_packet(&packet) {
                    log::warn!("[h264] decode_i420: send_packet retry failed: {}", e);
                    self.recreate_decoder();
                    return None;
                }
            }
            Err(e) => {
                log::warn!("[h264] decode_i420: send_packet error: {}", e);
                self.recreate_decoder();
                return None;
            }
        }

        let post_result = self.take_decoded_frame_with_i420();
        self.region_hint.clear();

        let result = post_result.or_else(|| {
            if early_result.is_some() {
                self.full_blit = true;
            }
            early_result
        });

        if let Some(r) = result {
            self.consecutive_eagain = 0;
            self.request_keyframe = false;
            return Some(r);
        }

        if self.use_hw {
            self.consecutive_eagain += 1;
            if self.consecutive_eagain >= STALL_SW_FALLBACK_THRESHOLD {
                log::debug!(
                    "[h264] decode_i420: VT stalled ({} consecutive EAGAINs) — switching to SW fallback",
                    self.consecutive_eagain
                );
                self.recreate_as_sw();
            } else if self.consecutive_eagain >= EAGAIN_FLUSH_THRESHOLD && !self.request_keyframe {
                log::debug!(
                    "[h264] decode_i420: VT stalled ({} consecutive EAGAINs) — requesting IDR",
                    self.consecutive_eagain
                );
                self.request_keyframe = true;
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::H264Decoder;
    use rustrdp_core::avc::AvcDecoder;

    #[test]
    fn signal_screen_refreshed_keeps_decoder_state() {
        let mut dec = H264Decoder::new().expect("ffmpeg decoder");

        dec.signal_screen_refreshed();

        assert!(!dec.take_decoder_flushed());
        assert!(!dec.take_full_blit());
        assert_eq!(dec.take_drain_count(), 0);
        assert!(dec.needs_keyframe());
    }

    #[test]
    fn reset_still_requests_full_blit() {
        let mut dec = H264Decoder::new().expect("ffmpeg decoder");

        dec.reset();

        assert!(dec.take_decoder_flushed());
        assert!(dec.take_full_blit());
    }
}
