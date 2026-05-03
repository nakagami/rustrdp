/// H.264 decoder via FFmpeg libavcodec (optional "h264" feature).
///
/// Without the feature, `H264Decoder` is a zero-size stub whose `decode()`
/// always returns `None`.

#[cfg(feature = "h264")]
mod ffi {
    use std::os::raw::{c_int, c_uchar};

    pub enum RdpH264Dec {}

    extern "C" {
        pub fn rdp_h264_new() -> *mut RdpH264Dec;
        pub fn rdp_h264_free(d: *mut RdpH264Dec);
        pub fn rdp_h264_decode(
            d: *mut RdpH264Dec,
            data: *const c_uchar,
            len: c_int,
            width: *mut c_int,
            height: *mut c_int,
        ) -> *mut c_uchar;
        pub fn rdp_h264_free_buf(buf: *mut c_uchar);
        pub fn rdp_h264_needs_keyframe(d: *mut RdpH264Dec) -> c_int;
        pub fn rdp_h264_take_full_blit(d: *mut RdpH264Dec) -> c_int;
        pub fn rdp_h264_take_decoder_flushed(d: *mut RdpH264Dec) -> c_int;
        pub fn rdp_h264_take_drain_happened(d: *mut RdpH264Dec) -> c_int;
    }
}

#[cfg(feature = "h264")]
pub struct H264Decoder {
    ptr: *mut ffi::RdpH264Dec,
}

#[cfg(feature = "h264")]
impl H264Decoder {
    pub fn new() -> Option<Self> {
        let ptr = unsafe { ffi::rdp_h264_new() };
        if ptr.is_null() {
            None
        } else {
            Some(H264Decoder { ptr })
        }
    }

    /// Decode one H.264 NAL packet.  Returns `(bgra_pixels, width, height)` when
    /// a frame is available, or `None` when the decoder needs more data (EAGAIN).
    pub fn decode(&mut self, data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        let mut w: std::os::raw::c_int = 0;
        let mut h: std::os::raw::c_int = 0;
        let bgra = unsafe {
            ffi::rdp_h264_decode(
                self.ptr,
                data.as_ptr(),
                data.len() as std::os::raw::c_int,
                &mut w,
                &mut h,
            )
        };
        if bgra.is_null() || w <= 0 || h <= 0 {
            return None;
        }
        let size = (w as usize) * (h as usize) * 4;
        // SAFETY: pointer was malloc'd by the C side; we take ownership here.
        let pixels = unsafe { std::slice::from_raw_parts(bgra, size).to_vec() };
        unsafe { ffi::rdp_h264_free_buf(bgra) };
        Some((pixels, w as u32, h as u32))
    }
    /// Returns true if the decoder is currently waiting for an IDR keyframe.
    /// This happens after P-frame decode failures or codec reset.
    pub fn needs_keyframe(&self) -> bool {
        unsafe { ffi::rdp_h264_needs_keyframe(self.ptr) != 0 }
    }

    /// Returns true (and clears the flag) if the last decoded frame must be
    /// blitted to the full surface, ignoring AVC dirty regions.
    /// This happens when a pipeline mismatch is detected (EAGAIN → drain).
    pub fn take_full_blit(&mut self) -> bool {
        unsafe { ffi::rdp_h264_take_full_blit(self.ptr) != 0 }
    }

    /// Returns true (and clears the flag) if `avcodec_flush_buffers` was called
    /// during the last `decode()` call.  The Rust caller uses this to discard
    /// its region FIFO so that stale entries are not paired with new frames.
    pub fn take_decoder_flushed(&mut self) -> bool {
        unsafe { ffi::rdp_h264_take_decoder_flushed(self.ptr) != 0 }
    }

    /// Returns true (and clears the flag) if the last `decode()` call drained
    /// at least one frame from the decoder pipeline (drain_count >= 1), even if
    /// that frame was suppressed (dark-frame suppression) and not returned to
    /// the caller.  The FIFO must be popped whenever a frame was drained,
    /// regardless of whether it was visible, to keep region–frame alignment.
    pub fn take_drain_happened(&mut self) -> bool {
        unsafe { ffi::rdp_h264_take_drain_happened(self.ptr) != 0 }
    }
}

#[cfg(feature = "h264")]
impl Drop for H264Decoder {
    fn drop(&mut self) {
        unsafe { ffi::rdp_h264_free(self.ptr) };
    }
}

// SAFETY: The underlying FFmpeg decoder context is not shared across threads,
// and H264Decoder is the sole owner.
#[cfg(feature = "h264")]
unsafe impl Send for H264Decoder {}

// ------- stub when feature is disabled ----------------------------------------

#[cfg(not(feature = "h264"))]
pub struct H264Decoder;

#[cfg(not(feature = "h264"))]
impl H264Decoder {
    pub fn new() -> Option<Self> { None }
    pub fn decode(&mut self, _data: &[u8]) -> Option<(Vec<u8>, u32, u32)> { None }
}
