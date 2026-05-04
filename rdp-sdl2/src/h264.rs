/// H.264 decoder using the [`openh264`] Rust crate.
///
/// This module replaces the previous FFmpeg C-FFI implementation.  It uses
/// the `openh264` crate (which bundles Cisco's OpenH264 C++ library compiled
/// from source as part of the Rust crate build — no system FFmpeg required).
///
/// It implements `rdp_core::avc::AvcDecoder` so that it can be passed as a
/// plug-in to `RdpSession::login`.
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use rdp_core::avc::AvcDecoder;

/// H.264 decoder backed by the `openh264` crate.
pub struct H264Decoder {
    decoder: Decoder,
    /// True immediately after `reset()` or `signal_screen_refreshed()`, until
    /// the flag is consumed by `take_decoder_flushed()`.
    decoder_flushed: bool,
    /// True immediately after `reset()` or `signal_screen_refreshed()`, until
    /// consumed by `take_full_blit()`.  Causes the next frame to be blitted to
    /// the entire surface rather than just the dirty regions.
    full_blit: bool,
    /// True if the last `decode()` call produced a frame (regardless of whether
    /// it was returned).  Consumed by `take_drain_happened()`.
    drain_happened: bool,
}

impl H264Decoder {
    /// Create a new decoder.  Returns an error if OpenH264 failed to
    /// initialise (should not happen with the bundled source build).
    pub fn new() -> Result<Self, openh264::Error> {
        Ok(H264Decoder {
            decoder: Decoder::new()?,
            decoder_flushed: false,
            full_blit: false,
            drain_happened: false,
        })
    }

    /// Convenience constructor that wraps the decoder in the trait-object box
    /// expected by `RdpSession::login`.  Returns `None` if initialisation
    /// failed (logged at WARN level).
    pub fn new_boxed() -> Option<Box<dyn AvcDecoder>> {
        match Self::new() {
            Ok(d) => Some(Box::new(d) as Box<dyn AvcDecoder>),
            Err(e) => {
                log::warn!("[h264] failed to create openh264 decoder: {}", e);
                None
            }
        }
    }

    /// Recreate the internal decoder, setting the flushed/full-blit flags so
    /// the next frame is blitted unconditionally and the region FIFO is reset.
    fn recreate_decoder(&mut self) {
        match Decoder::new() {
            Ok(d) => {
                self.decoder = d;
                self.decoder_flushed = true;
                self.full_blit = true;
            }
            Err(e) => {
                log::warn!("[h264] failed to recreate openh264 decoder: {}", e);
            }
        }
    }
}

impl AvcDecoder for H264Decoder {
    /// Decode one H.264 NAL packet.
    ///
    /// Returns `Some((bgra_pixels, width, height))` when a full frame is
    /// available, or `None` when the decoder needs more data.
    fn decode(&mut self, data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        self.drain_happened = false;

        match self.decoder.decode(data) {
            Ok(Some(yuv)) => {
                let (w, h) = yuv.dimensions();
                let size = w * h * 4;
                // openh264 produces RGBA; RDP surfaces expect BGRA.
                let mut rgba = vec![0u8; size];
                yuv.write_rgba8(&mut rgba);
                // Swap R ↔ B in place to produce BGRA.
                for chunk in rgba.chunks_exact_mut(4) {
                    chunk.swap(0, 2);
                }
                self.drain_happened = true;
                Some((rgba, w as u32, h as u32))
            }
            Ok(None) => {
                // Decoder is still buffering; more NAL packets needed.
                None
            }
            Err(e) => {
                log::warn!("[h264] decode error: {}", e);
                // On decode error try to recover cleanly: recreate the decoder
                // so the next IDR frame starts a fresh stream.
                self.recreate_decoder();
                None
            }
        }
    }

    /// Returns `true` while the decoder is waiting for an IDR keyframe.
    ///
    /// openh264 handles IDR waiting internally.  We expose `false` here
    /// because OpenH264 discards non-IDR frames automatically until the first
    /// IDR arrives; no external force-refresh mechanism is needed.
    fn needs_keyframe(&self) -> bool {
        false
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

    fn take_drain_happened(&mut self) -> bool {
        let v = self.drain_happened;
        self.drain_happened = false;
        v
    }

    /// Flush the decoder pipeline and request a full-screen blit on the next
    /// frame.  Called when the server sends a raw Bitmap Update in response
    /// to a SuppressOutput force-refresh PDU.
    fn signal_screen_refreshed(&mut self) {
        self.recreate_decoder();
    }

    /// Reset the decoder to a clean state after RDPGFX RESET_GRAPHICS.
    fn reset(&mut self) {
        self.recreate_decoder();
    }
}

