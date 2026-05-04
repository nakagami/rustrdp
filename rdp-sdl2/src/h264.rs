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
use ffmpeg::util::color::Range as ColorRange;
use ffmpeg::util::format::pixel::Pixel;
use ffmpeg::util::frame::video::Video as VideoFrame;
use ffmpeg_next as ffmpeg;
use rdp_core::avc::AvcDecoder;
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

/// Maximum number of non-IDR packets to drop while waiting for a keyframe
/// after a real decoder reset/flush.  Matches grdp's keyframeWaitLimit.
const KEYFRAME_WAIT_LIMIT: usize = 900;

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
    INIT.get_or_init(ffmpeg::init).clone()
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
                found = true;
                // Don't break — log all leading NAL types in debug mode.
                #[cfg(not(debug_assertions))]
                return true;
            }
        }
        i += 1;
    }
    found
}

#[inline]
fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[inline]
fn bt601_bgra(y_raw: u8, u_raw: u8, v_raw: u8, full_range: bool) -> [u8; 4] {
    let y = y_raw as i32;
    let u = u_raw as i32 - 128;
    let v = v_raw as i32 - 128;
    let (r, g, b) = if full_range {
        (
            (256 * y + 359 * v + 128) >> 8,
            (256 * y - 88 * u - 183 * v + 128) >> 8,
            (256 * y + 454 * u + 128) >> 8,
        )
    } else {
        let c = y - 16;
        (
            (298 * c + 409 * v + 128) >> 8,
            (298 * c - 100 * u - 208 * v + 128) >> 8,
            (298 * c + 516 * u + 128) >> 8,
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
                    let px = bt601_bgra(y_row[col], u_row[col / 2], v_row[col / 2], full_range);
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

            if !regions.is_empty() {
                // Region-aware path: only convert pixels within dirty rects.
                // For small dirty regions (e.g. 272x32 scroll update on a 1920x1088
                // frame) this avoids converting ~99.5% of the pixels.
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
                            let px = bt601_bgra(
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
                for row in 0..height {
                    let y_row = &y[row * y_stride..];
                    let uv_row = &uv[(row / 2) * uv_stride..];
                    let dst_row = &mut out[row * width * 4..(row + 1) * width * 4];
                    for col in 0..width {
                        let uv_off = (col / 2) * 2;
                        let px = bt601_bgra(y_row[col], uv_row[uv_off], uv_row[uv_off + 1], full_range);
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

#[cfg(not(target_arch = "aarch64"))]
fn frame_to_bgra_direct(_decoded: &VideoFrame, _regions: &[(u16, u16, u16, u16)]) -> Option<Vec<u8>> {
    None
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
            // Limit the decoded picture buffer (DPB) to 1 reference frame so the
            // decoder outputs each frame immediately instead of accumulating up to
            // max_dec_frame_buffering (often 8) frames from the server's SPS.
            // RDP H.264 streams use sequential P-frames that reference only the
            // immediately preceding frame, so capping at 1 is safe and eliminates
            // the multi-frame visual latency introduced by a large DPB.
            (*ctx).refs = 1;

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
            }
            Err(e) => {
                log::warn!("[h264] failed to recreate ffmpeg decoder: {}", e);
            }
        }
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
        // In debug builds, skip the pure-Rust pixel loop and always use ffmpeg's
        // swscale (C library with SIMD).  The Rust loop is ~10x slower in debug
        // mode (no optimization, bounds checks per pixel) and causes choppy video.
        // In release builds the Rust path is used for region-aware partial conversion.
        #[cfg(not(debug_assertions))]
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
                    return latest;
                }
                Err(ffmpeg::Error::Eof) => return latest,
                Err(e) => {
                    log::warn!("[h264] receive_frame error: {}", e);
                    self.recreate_decoder();
                    return None;
                }
            }
        }
    }

    fn take_decoded_frame(&mut self) -> Option<(Vec<u8>, u32, u32)> {
        let sw = self.take_decoded_frame_raw()?;
        let width = sw.width();
        let height = sw.height();
        match self.frame_to_bgra(&sw) {
            Ok(pixels) => Some((pixels, width, height)),
            Err(e) => {
                log::warn!("[h264] failed to convert frame to BGRA: {}", e);
                self.recreate_decoder();
                None
            }
        }
    }

    fn take_decoded_frame_nv12(&mut self) -> Option<rdp_core::avc::NV12Frame> {
        let sw = self.take_decoded_frame_raw()?;
        let width = sw.width();
        let height = sw.height();
        let (y, uv, y_stride, uv_stride) = extract_nv12_planes(&sw)?;
        Some(rdp_core::avc::NV12Frame {
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
            // needs_flush is true → IDR/SPS present; clear the wait.
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
        if self.use_hw {
            self.consecutive_eagain += 1;
            if self.consecutive_eagain >= EAGAIN_FLUSH_THRESHOLD && !self.request_keyframe {
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

    fn decode_nv12(&mut self, data: &[u8]) -> Option<rdp_core::avc::NV12Frame> {
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
            if self.consecutive_eagain >= EAGAIN_FLUSH_THRESHOLD && !self.request_keyframe {
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
}

#[cfg(test)]
mod tests {
    use super::H264Decoder;
    use rdp_core::avc::AvcDecoder;

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
