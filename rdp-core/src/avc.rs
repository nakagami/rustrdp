/// Plug-in trait for H.264/AVC decoding.
///
/// rdp-core uses this interface to decode AVC frames without depending on any
/// codec library.  The concrete implementation lives in rdp-sdl2 (or any
/// other frontend that wants H.264 support).  Frontends that do not need
/// H.264 simply pass `None` when calling `RdpSession::login`.
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

    /// Returns `true` when the decoder wants the server to send an IDR keyframe.
    /// Implementations may still keep feeding P-frames while requesting one.
    fn needs_keyframe(&self) -> bool;

    /// Returns `true` (and clears the flag) if the last decoded frame must be
    /// blitted to the full surface, ignoring AVC dirty regions.
    fn take_full_blit(&mut self) -> bool;

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
}
