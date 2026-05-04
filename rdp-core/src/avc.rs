/// Plug-in trait for H.264/AVC decoding.
///
/// rdp-core uses this interface to decode AVC frames without depending on any
/// codec library.  The concrete implementation lives in rdp-sdl2 (or any
/// other frontend that wants H.264 support).  Frontends that do not need
/// H.264 simply pass `None` when calling `RdpSession::login`.
pub trait AvcDecoder {
    /// Decode one H.264 NAL packet.
    ///
    /// Returns `(bgra_pixels, width, height)` when a frame is available,
    /// or `None` when the decoder is still buffering.
    fn decode(&mut self, data: &[u8]) -> Option<(Vec<u8>, u32, u32)>;

    /// Returns `true` while the decoder is waiting for an IDR keyframe.
    /// This happens after P-frame decode failures or codec reset.
    fn needs_keyframe(&self) -> bool;

    /// Returns `true` (and clears the flag) if the last decoded frame must be
    /// blitted to the full surface, ignoring AVC dirty regions.
    fn take_full_blit(&mut self) -> bool;

    /// Returns `true` (and clears the flag) if the decoder was flushed/reset
    /// during the last operation.  Callers use this to clear the dirty-region
    /// FIFO so stale region metadata is discarded.
    fn take_decoder_flushed(&mut self) -> bool;

    /// Returns `true` (and clears the flag) if the last `decode()` call
    /// produced at least one output frame.
    fn take_drain_happened(&mut self) -> bool;

    /// Called when the server sent a large/full-screen raw Bitmap Update in
    /// response to a SuppressOutput force-refresh PDU.  Flushes stale frames
    /// from the pipeline.
    fn signal_screen_refreshed(&mut self);

    /// Reset the decoder to a clean state.  Called after RDPGFX RESET_GRAPHICS
    /// to discard buffered frames from the previous surface configuration.
    fn reset(&mut self);
}
