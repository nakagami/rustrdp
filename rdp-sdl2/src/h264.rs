/// H.264 decoder via FFmpeg libavcodec.
///
/// This module lives in rdp-sdl2 so that rdp-core remains free of any
/// codec library dependency.  It implements the `rdp_core::avc::AvcDecoder`
/// trait so it can be passed to `RdpSession::login` as a plug-in.

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
        pub fn rdp_h264_signal_screen_refreshed(d: *mut RdpH264Dec);
    }
}

pub struct H264Decoder {
    ptr: *mut ffi::RdpH264Dec,
}

impl H264Decoder {
    /// Try to create a new H.264 decoder.
    ///
    /// Returns `None` if FFmpeg could not initialise (missing codec, no hw
    /// support, etc.).
    pub fn new() -> Option<Self> {
        let ptr = unsafe { ffi::rdp_h264_new() };
        if ptr.is_null() {
            None
        } else {
            Some(H264Decoder { ptr })
        }
    }

    /// Create a boxed `AvcDecoder` trait object, ready to pass to
    /// `RdpSession::login`.  Returns `None` if the decoder could not be
    /// initialised.
    pub fn new_boxed() -> Option<Box<dyn rdp_core::avc::AvcDecoder>> {
        Self::new().map(|d| Box::new(d) as Box<dyn rdp_core::avc::AvcDecoder>)
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::rdp_h264_free(self.ptr) };
        }
    }
}

// SAFETY: The underlying FFmpeg decoder context is not shared across threads,
// and H264Decoder is the sole owner.
unsafe impl Send for H264Decoder {}

impl rdp_core::avc::AvcDecoder for H264Decoder {
    /// Decode one H.264 NAL packet.  Returns `(bgra_pixels, width, height)`
    /// when a frame is available, or `None` when the decoder needs more data.
    fn decode(&mut self, data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        if self.ptr.is_null() {
            return None;
        }
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

    fn needs_keyframe(&self) -> bool {
        if self.ptr.is_null() {
            return false;
        }
        unsafe { ffi::rdp_h264_needs_keyframe(self.ptr) != 0 }
    }

    fn take_full_blit(&mut self) -> bool {
        if self.ptr.is_null() {
            return false;
        }
        unsafe { ffi::rdp_h264_take_full_blit(self.ptr) != 0 }
    }

    fn take_decoder_flushed(&mut self) -> bool {
        if self.ptr.is_null() {
            return false;
        }
        unsafe { ffi::rdp_h264_take_decoder_flushed(self.ptr) != 0 }
    }

    fn take_drain_happened(&mut self) -> bool {
        if self.ptr.is_null() {
            return false;
        }
        unsafe { ffi::rdp_h264_take_drain_happened(self.ptr) != 0 }
    }

    fn signal_screen_refreshed(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        unsafe { ffi::rdp_h264_signal_screen_refreshed(self.ptr) }
    }

    /// Reset the decoder by freeing and re-creating the FFmpeg context.
    /// Called after RDPGFX RESET_GRAPHICS to discard stale pipeline frames.
    fn reset(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::rdp_h264_free(self.ptr) };
        }
        self.ptr = unsafe { ffi::rdp_h264_new() };
        if self.ptr.is_null() {
            log::warn!("[h264] reset: rdp_h264_new() returned null — H.264 decode disabled");
        }
    }
}
