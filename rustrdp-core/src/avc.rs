/// Plug-in trait for H.264/AVC decoding.
///
/// rdp-core uses this interface to decode AVC frames without depending on any
/// codec library.  The concrete implementation lives in rdp-sdl2 (or any
/// other frontend that wants H.264 support).  Frontends that do not need
/// H.264 simply pass `None` when calling `RdpSession::login`.

/// A decoded H.264 frame in planar I420 (YUV420P) format.
///
/// Used for AVC444 LC=2 chroma-upgrade combining: the main stream's Y plane
/// is cached and later combined with the auxiliary stream's chroma planes to
/// produce a full-colour BGRA frame without a second full-surface decode.
pub struct I420Frame {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub y_stride: usize,
    pub u_stride: usize,
    pub v_stride: usize,
    pub width: u32,
    pub height: u32,
    /// True when the source used full-range (JPEG/PC) YUV [0-255].
    /// False for limited (video) range [16-235].
    pub full_range: bool,
}

/// A raw NV12 video frame produced by the hardware decoder, with screen coordinates for direct SDL overlay rendering.
pub struct NV12Frame {
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub y_stride: usize,
    pub uv_stride: usize,
    pub screen_x: i32,
    pub screen_y: i32,
}

pub trait AvcDecoder {
    /// Hint to the decoder about the dirty regions that will be blitted from the
    /// next decoded frame, as `(left, top, right, bottom)` tuples in frame pixels.
    /// Implementations may use this to skip converting pixels outside the dirty
    /// area.  The hint is consumed after one `decode()` call.  The default
    /// implementation is a no-op.
    fn set_region_hint(&mut self, _regions: &[(u16, u16, u16, u16)]) {}

    /// Decode one H.264 NAL packet.
    ///
    /// Returns `(bgra_pixels, width, height)` when a frame is available,
    /// or `None` when the decoder is still buffering.
    fn decode(&mut self, data: &[u8]) -> Option<(Vec<u8>, u32, u32)>;

    /// Decode one H.264 NAL packet, returning BGRA pixels and I420 planar data.
    ///
    /// Returns `Some((bgra_pixels, width, height, i420))` when a full frame is
    /// available, or `None` when the decoder is still buffering.
    ///
    /// The I420 frame is used for AVC444 LC=2 chroma-upgrade combines: the
    /// caller caches the Y plane from lc=0/1 decodes and combines it with the
    /// auxiliary stream's I420 planes on lc=2 frames.
    ///
    /// Default implementation returns `None` (unsupported).
    fn decode_i420(&mut self, data: &[u8]) -> Option<(Vec<u8>, u32, u32, I420Frame)> {
        let _ = data;
        None
    }

    /// Returns `true` when the decoder wants the server to send an IDR keyframe.
    /// Implementations may still keep feeding P-frames while requesting one.
    fn needs_keyframe(&self) -> bool;

    /// Returns `true` (and clears the flag) if the last decoded frame must be
    /// blitted to the full surface, ignoring AVC dirty regions.
    fn take_full_blit(&mut self) -> bool;

    /// Returns `true` (and clears the flag) when the decoder wants the server
    /// to send an IDR keyframe as a soft hint — the decoder continues accepting
    /// P-frames while waiting.  Set after `EAGAIN_FLUSH_THRESHOLD` consecutive
    /// receive_frame EAGAINs.  Unlike `needs_keyframe()`, this does NOT drop
    /// non-IDR packets; it only triggers a force-refresh PDU.
    fn take_request_keyframe(&mut self) -> bool {
        false
    }

    /// Returns `true` (and clears the flag) if the decoder was flushed/reset
    /// during the last operation.  Callers use this to clear the dirty-region
    /// FIFO so stale region metadata is discarded.
    fn take_decoder_flushed(&mut self) -> bool;

    /// Returns the number of frames drained from the decoder by the last
    /// `decode()` call and clears the counter.
    fn take_drain_count(&mut self) -> usize;

    /// Returns true when AVC dirty-region metadata must be paired with decoded
    /// frames through a packet FIFO because the decoder can emit delayed frames
    /// from older packets. Decoders that stay aligned with the current packet's
    /// region metadata should return false.
    fn uses_region_fifo(&self) -> bool {
        true
    }

    /// Called when the server sent a large/full-screen raw Bitmap Update in
    /// response to a SuppressOutput force-refresh PDU.  Flushes stale frames
    /// from the pipeline.
    fn signal_screen_refreshed(&mut self);

    /// Reset the decoder to a clean state.  Called after RDPGFX RESET_GRAPHICS
    /// to discard buffered frames from the previous surface configuration.
    fn reset(&mut self);

    /// Decode one H.264 NAL packet, returning a raw NV12 frame.
    /// Only valid when `supports_nv12()` returns true.
    /// Default implementation returns None.
    fn decode_nv12(&mut self, _data: &[u8]) -> Option<NV12Frame> {
        None
    }

    /// Returns true when this decoder supports direct NV12 output.
    /// NV12 output bypasses swscale and surface compositing.
    fn supports_nv12(&self) -> bool {
        false
    }

    /// Returns `true` (and clears the flag) when the HW decoder (VideoToolbox)
    /// has stalled irrecoverably and has been switched to software decoding
    /// internally.  The caller (RdpgfxHandler) should prime the new SW decoder
    /// with the last cached IDR and request a force-refresh from the server.
    fn take_hw_stalled(&mut self) -> bool {
        false
    }
}
