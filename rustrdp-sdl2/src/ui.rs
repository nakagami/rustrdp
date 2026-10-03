use sdl2::pixels::PixelFormatEnum;
use sdl2::rect::Rect;
use sdl2::render::{Canvas, Texture, TextureCreator};
use sdl2::video::{Window, WindowContext};
use std::error::Error;
use std::os::raw::c_int;

use rustrdp_core::bitmap::Bitmap;

extern "C" {
    fn SDL_UpdateNVTexture(
        texture: *mut sdl2::sys::SDL_Texture,
        rect: *const sdl2::sys::SDL_Rect,
        Yplane: *const u8,
        Ypitch: c_int,
        UVplane: *const u8,
        UVpitch: c_int,
    ) -> c_int;
}

fn create_and_init_nv12_texture(
    canvas_raw: *mut sdl2::sys::SDL_Renderer,
    width: u32,
    height: u32,
) -> Result<*mut sdl2::sys::SDL_Texture, String> {
    let tex = unsafe {
        sdl2::sys::SDL_CreateTexture(
            canvas_raw,
            0x3231_564E_u32, // SDL_PIXELFORMAT_NV12
            sdl2::sys::SDL_TextureAccess::SDL_TEXTUREACCESS_STREAMING as c_int,
            width as c_int,
            height as c_int,
        )
    };
    if tex.is_null() {
        let err_msg = unsafe {
            let s = sdl2::sys::SDL_GetError();
            if s.is_null() {
                "SDL_CreateTexture NV12 failed".to_string()
            } else {
                std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned()
            }
        };
        return Err(err_msg);
    }

    // Initialize NV12 texture with black (Y=0, UV=128) to prevent green flash
    // before the first video frame arrives.
    let w = width as usize;
    let h = height as usize;
    let ph = (h + 1) / 2;
    let y_buf = vec![0u8; w * h];
    let uv_buf = vec![128u8; w * ph];
    let ret = unsafe {
        SDL_UpdateNVTexture(
            tex,
            std::ptr::null(),
            y_buf.as_ptr(),
            w as c_int,
            uv_buf.as_ptr(),
            w as c_int,
        )
    };
    if ret < 0 {
        log::warn!("Failed to initialize NV12 texture with black: ret={}", ret);
    }

    Ok(tex)
}

// Fields are declared in drop order (first declared → first dropped).
// nv12_tex must drop before canvas (SDL_DestroyTexture before SDL_DestroyRenderer).
pub struct RdpUI {
    /// Raw NV12 texture for direct hardware-decoded frame overlay.
    nv12_tex: *mut sdl2::sys::SDL_Texture,
    nv12_tex_w: u32,
    nv12_tex_h: u32,
    nv12_ready: bool,
    texture: Texture,                               // dropped after nv12_tex
    texture_creator: TextureCreator<WindowContext>,
    canvas: Canvas<Window>,                         // dropped last: SDL_DestroyRenderer
    /// CPU-side BGRA back-buffer: compositing target for incremental bitmap tiles.
    back_buf: Vec<u8>,
    width: u16,
    height: u16,
}

impl Drop for RdpUI {
    fn drop(&mut self) {
        if !self.nv12_tex.is_null() {
            unsafe { sdl2::sys::SDL_DestroyTexture(self.nv12_tex); }
            self.nv12_tex = std::ptr::null_mut();
        }
    }
}

impl RdpUI {
    pub fn new(
        sdl_context: &sdl2::Sdl,
        width: u16,
        height: u16,
        title: &str,
    ) -> Result<Self, Box<dyn Error>> {
        let video_subsystem = sdl_context.video()?;
        let window = video_subsystem
            .window(title, width as u32, height as u32)
            .position_centered()
            .build()?;

        // Prefer an accelerated renderer with VSync so that canvas.present()
        // waits for the display vblank.  This caps rendering to the display
        // refresh rate, eliminates tearing, and mirrors grdpsdl2's preference
        // for sdl.RENDERER_ACCELERATED|sdl.RENDERER_PRESENTVSYNC.
        let canvas = match window.into_canvas().accelerated().present_vsync().build() {
            Ok(canvas) => canvas,
            Err(err) => {
                log::warn!(
                    "vsync renderer unavailable, trying without vsync: {}",
                    err
                );
                // into_canvas() consumed window; build a new one for the fallback.
                let window = video_subsystem
                    .window(title, width as u32, height as u32)
                    .position_centered()
                    .build()?;
                match window.into_canvas().accelerated().build() {
                    Ok(canvas) => canvas,
                    Err(err) => {
                        log::warn!(
                            "hardware renderer unavailable, falling back to software: {}",
                            err
                        );
                        video_subsystem
                            .window(title, width as u32, height as u32)
                            .position_centered()
                            .build()?
                            .into_canvas()
                            .software()
                            .build()?
                    }
                }
            }
        };
        let texture_creator = canvas.texture_creator();
        // ARGB8888 = BGRA in memory on little-endian (macOS/Linux/Windows x86).
        // AVC frames from VideoToolbox are already BGRA — no per-pixel swap needed.
        let mut texture = texture_creator.create_texture_streaming(
            PixelFormatEnum::ARGB8888,
            width as u32,
            height as u32,
        )?;
        texture.set_blend_mode(sdl2::render::BlendMode::Blend);
        let back_buf = vec![0u8; width as usize * height as usize * 4];
        texture.update(None, &back_buf, width as usize * 4)?;

        // Set BT.709 YUV→RGB conversion for HD H.264 frames (≥720p).
        // SDL2 defaults to BT.601 which maps the chroma differently, producing
        // a green-tinted or washed-out image.  Matches grdpsdl2's
        // sdl.SetYUVConversionMode(sdl.YUV_CONVERSION_BT709).
        unsafe {
            sdl2::sys::SDL_SetYUVConversionMode(
                sdl2::sys::SDL_YUV_CONVERSION_MODE::SDL_YUV_CONVERSION_BT709,
            );
        }

        let (nv12_tex, nv12_tex_w, nv12_tex_h) = match create_and_init_nv12_texture(canvas.raw(), width as u32, height as u32) {
            Ok(tex) => (tex, width as u32, height as u32),
            Err(e) => {
                log::warn!("NV12 texture unavailable, will use BGRA fallback: {}", e);
                (std::ptr::null_mut(), 0, 0)
            }
        };

        Ok(RdpUI {
            nv12_tex,
            nv12_tex_w,
            nv12_tex_h,
            nv12_ready: false,
            texture,
            texture_creator,
            canvas,
            back_buf,
            width,
            height,
        })
    }

    /// Composite bitmap tiles into the back-buffer, update dirty texture rects, then present.
    pub fn update_screen(&mut self, bitmaps: &[Bitmap]) -> Result<(), Box<dyn Error>> {
        let mut dirty = false;
        for bitmap in bitmaps {
            dirty |= self.blit_bitmap_to_texture(bitmap)?;
        }
        if log::log_enabled!(log::Level::Trace) {
            self.log_back_buffer_sample(bitmaps);
        }
        if dirty {
            self.present_composed()?;
        }
        Ok(())
    }

    /// Re-present the current back-buffer without modifying it.
    /// Call this on SDL Exposed / Restored events so the window redraws itself.
    pub fn repaint(&mut self) -> Result<(), Box<dyn Error>> {
        let row_bytes = self.width as usize * 4;
        self.texture.update(None, &self.back_buf, row_bytes)?;
        self.present_composed()
    }

    pub fn resize(&mut self, width: u16, height: u16) -> Result<(), Box<dyn Error>> {
        if self.width == width && self.height == height {
            return Ok(());
        }
        self.canvas
            .window_mut()
            .set_size(width as u32, height as u32)?;
        let mut texture = self.texture_creator.create_texture_streaming(
            PixelFormatEnum::ARGB8888,
            width as u32,
            height as u32,
        )?;
        texture.set_blend_mode(sdl2::render::BlendMode::Blend);
        self.back_buf = vec![0u8; width as usize * height as usize * 4];
        texture.update(None, &self.back_buf, width as usize * 4)?;
        self.texture = texture;
        self.width = width;
        self.height = height;
        // Recreate NV12 texture at the new dimensions
        if !self.nv12_tex.is_null() {
            unsafe { sdl2::sys::SDL_DestroyTexture(self.nv12_tex); }
            self.nv12_tex = std::ptr::null_mut();
            self.nv12_tex_w = 0;
            self.nv12_tex_h = 0;
            self.nv12_ready = false;
        }
        if let Ok(tex) = create_and_init_nv12_texture(self.canvas.raw(), width as u32, height as u32) {
            self.nv12_tex = tex;
            self.nv12_tex_w = width as u32;
            self.nv12_tex_h = height as u32;
        }
        Ok(())
    }

    /// Clear the entire BGRA overlay back-buffer and GPU texture to transparent.
    pub fn clear_overlay(&mut self) -> Result<(), Box<dyn Error>> {
        self.back_buf.fill(0);
        let row_bytes = self.width as usize * 4;
        self.texture.update(None, &self.back_buf, row_bytes)?;
        Ok(())
    }

    /// Composite bitmap tiles into the back-buffer and update dirty texture
    /// rects, but do **not** present. Call `present_composed` separately.
    pub fn compose_bitmaps(&mut self, bitmaps: &[Bitmap]) -> Result<bool, Box<dyn Error>> {
        let mut dirty = false;
        for bitmap in bitmaps {
            dirty |= self.blit_bitmap_to_texture(bitmap)?;
        }
        if log::log_enabled!(log::Level::Trace) {
            self.log_back_buffer_sample(bitmaps);
        }
        Ok(dirty)
    }

    /// Upload a raw NV12 frame to the GPU texture but do **not** present.
    /// Call `present_composed` once after draining all pending frames.
    pub fn upload_nv12_frame(&mut self, frame: &rustrdp_core::avc::NV12Frame) -> Result<(), Box<dyn Error>> {
        let scr_w = self.width as i32;
        let scr_h = self.height as i32;

        if self.nv12_tex.is_null() || self.nv12_tex_w != self.width as u32 || self.nv12_tex_h != self.height as u32 {
            if !self.nv12_tex.is_null() {
                unsafe { sdl2::sys::SDL_DestroyTexture(self.nv12_tex); }
                self.nv12_tex = std::ptr::null_mut();
            }
            if let Ok(tex) = create_and_init_nv12_texture(self.canvas.raw(), self.width as u32, self.height as u32) {
                self.nv12_tex = tex;
                self.nv12_tex_w = self.width as u32;
                self.nv12_tex_h = self.height as u32;
            } else {
                return Err("Failed to create NV12 texture".into());
            }
        }

        let dst_w = (frame.width as i32).min(scr_w - frame.screen_x).max(0);
        let dst_h = (frame.height as i32).min(scr_h - frame.screen_y).max(0);

        if dst_w > 0 && dst_h > 0 {
            let update_rect = sdl2::sys::SDL_Rect {
                x: frame.screen_x as c_int,
                y: frame.screen_y as c_int,
                w: dst_w as c_int,
                h: dst_h as c_int,
            };

            let ret = unsafe {
                SDL_UpdateNVTexture(
                    self.nv12_tex,
                    &update_rect,
                    frame.y.as_ptr(),
                    frame.y_stride as c_int,
                    frame.uv.as_ptr(),
                    frame.uv_stride as c_int,
                )
            };
            if ret < 0 {
                let err_msg = unsafe {
                    let s = sdl2::sys::SDL_GetError();
                    if s.is_null() {
                        format!("SDL_UpdateNVTexture failed: {}", ret)
                    } else {
                        std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned()
                    }
                };
                return Err(err_msg.into());
            }
            self.nv12_ready = true;
        }
        Ok(())
    }

    /// Compose the NV12 background video with the BGRA overlay and present.
    ///
    /// Background layer: NV12 hardware-decoded video frame.
    /// Foreground overlay: BGRA texture (with SDL_BLENDMODE_BLEND) for UI, mouse, and bitmaps.
    pub fn present_composed(&mut self) -> Result<(), Box<dyn Error>> {
        self.canvas.set_draw_color(sdl2::pixels::Color::RGB(0, 0, 0));
        self.canvas.clear();

        if self.nv12_ready && !self.nv12_tex.is_null() {
            unsafe {
                sdl2::sys::SDL_RenderCopy(
                    self.canvas.raw(),
                    self.nv12_tex,
                    std::ptr::null(),
                    std::ptr::null(),
                );
            }
        }
        self.canvas.copy(&self.texture, None, None)?;
        self.canvas.present();
        Ok(())
    }

    /// Save current canvas content to a BMP file.
    pub fn save_screenshot(&mut self, path: &str) -> Result<(), Box<dyn Error>> {
        self.canvas.set_draw_color(sdl2::pixels::Color::RGB(0, 0, 0));
        self.canvas.clear();

        if self.nv12_ready && !self.nv12_tex.is_null() {
            unsafe {
                sdl2::sys::SDL_RenderCopy(
                    self.canvas.raw(),
                    self.nv12_tex,
                    std::ptr::null(),
                    std::ptr::null(),
                );
            }
        }
        self.canvas.copy(&self.texture, None, None)?;
        let pixels = self.canvas.read_pixels(None, PixelFormatEnum::RGB24)?;
        self.canvas.present();

        let surface = sdl2::surface::Surface::from_data(
            pixels.as_slice().to_vec().leak(),
            self.width as u32,
            self.height as u32,
            self.width as u32 * 3,
            PixelFormatEnum::RGB24,
        )?;
        surface.save_bmp(path)?;
        log::info!("Screenshot saved to {}", path);
        Ok(())
    }


    /// Blit one bitmap tile into the CPU back-buffer, then upload that rect to the SDL texture.
    ///
    /// Texture format is ARGB8888 (= BGRA byte order on little-endian).
    /// All bitmaps have alpha forced to 0xFF (opaque) so that overlay blending
    /// does not let underlying YUV or garbage shine through as green/black artifacts.
    fn blit_bitmap_to_texture(&mut self, bitmap: &Bitmap) -> Result<bool, Box<dyn Error>> {
        let bw = bitmap.width as usize; // source row stride
        let bh = bitmap.height as usize;
        if bw == 0 || bh == 0 {
            return Ok(false);
        }
        let rect_w = (bitmap.dest_right - bitmap.dest_left + 1).max(0) as usize;
        let rect_h = (bitmap.dest_bottom - bitmap.dest_top + 1).max(0) as usize;
        if rect_w == 0 || rect_h == 0 {
            return Ok(false);
        }
        let src_x = if bitmap.dest_left < 0 {
            (-bitmap.dest_left) as usize
        } else {
            0
        };
        let src_y = if bitmap.dest_top < 0 {
            (-bitmap.dest_top) as usize
        } else {
            0
        };
        let dest_x = bitmap.dest_left.max(0) as usize;
        let dest_y = bitmap.dest_top.max(0) as usize;
        let scr_w = self.width as usize;
        let scr_h = self.height as usize;
        if src_x >= bw || src_y >= bh || dest_x >= scr_w || dest_y >= scr_h {
            return Ok(false);
        }
        let clip_w = bw
            .saturating_sub(src_x)
            .min(rect_w.saturating_sub(src_x))
            .min(scr_w.saturating_sub(dest_x));
        let clip_h = bh
            .saturating_sub(src_y)
            .min(rect_h.saturating_sub(src_y))
            .min(scr_h.saturating_sub(dest_y));
        if clip_w == 0 || clip_h == 0 {
            return Ok(false);
        }

        let rect = Rect::new(dest_x as i32, dest_y as i32, clip_w as u32, clip_h as u32);
        let src_bpp = match bitmap.bits_per_pixel {
            32 => 4usize,
            24 => 3usize,
            16 => 2usize,
            _ => return Ok(false),
        };
        let src_data = &bitmap.data;

        let mut patch_buf = vec![0u8; clip_w * clip_h * 4];

        // Write pixels into the CPU back-buffer (BGRA format) and temporary patch buffer.
        for row in 0..clip_h {
            let src_start = ((src_y + row) * bw + src_x) * src_bpp;
            let back_start = ((dest_y + row) * scr_w + dest_x) * 4;
            let patch_start = row * clip_w * 4;
            let back_row = &mut self.back_buf[back_start..back_start + clip_w * 4];
            let patch_row = &mut patch_buf[patch_start..patch_start + clip_w * 4];
            match bitmap.bits_per_pixel {
                32 => {
                    let src_end = src_start + clip_w * 4;
                    if src_end > src_data.len() {
                        return Ok(false);
                    }
                    let src_row = &src_data[src_start..src_end];
                    for col in 0..clip_w {
                        let s = col * 4;
                        let d = col * 4;
                        let b = src_row[s];
                        let g = src_row[s + 1];
                        let r = src_row[s + 2];
                        back_row[d] = b;
                        back_row[d + 1] = g;
                        back_row[d + 2] = r;
                        back_row[d + 3] = 255;
                        patch_row[d] = b;
                        patch_row[d + 1] = g;
                        patch_row[d + 2] = r;
                        patch_row[d + 3] = 255;
                    }
                }
                24 => {
                    let src_end = src_start + clip_w * 3;
                    if src_end > src_data.len() {
                        return Ok(false);
                    }
                    let src_row = &src_data[src_start..src_end];
                    for col in 0..clip_w {
                        let s = col * 3;
                        let d = col * 4;
                        let b = src_row[s];
                        let g = src_row[s + 1];
                        let r = src_row[s + 2];
                        back_row[d] = b;
                        back_row[d + 1] = g;
                        back_row[d + 2] = r;
                        back_row[d + 3] = 255;
                        patch_row[d] = b;
                        patch_row[d + 1] = g;
                        patch_row[d + 2] = r;
                        patch_row[d + 3] = 255;
                    }
                }
                16 => {
                    let src_end = src_start + clip_w * 2;
                    if src_end > src_data.len() {
                        return Ok(false);
                    }
                    let src_row = &src_data[src_start..src_end];
                    for col in 0..clip_w {
                        let s = col * 2;
                        let d = col * 4;
                        let v = u16::from_le_bytes([src_row[s], src_row[s + 1]]);
                        let r5 = ((v >> 11) & 0x1f) as u8;
                        let g6 = ((v >> 5) & 0x3f) as u8;
                        let b5 = (v & 0x1f) as u8;
                        let b = (b5 << 3) | (b5 >> 2);
                        let g = (g6 << 2) | (g6 >> 4);
                        let r = (r5 << 3) | (r5 >> 2);
                        back_row[d] = b;
                        back_row[d + 1] = g;
                        back_row[d + 2] = r;
                        back_row[d + 3] = 255;
                        patch_row[d] = b;
                        patch_row[d + 1] = g;
                        patch_row[d + 2] = r;
                        patch_row[d + 3] = 255;
                    }
                }
                _ => {}
            }
        }

        // Upload the updated rect to the SDL texture
        self.texture
            .update(Some(rect), &patch_buf, clip_w * 4)?;
        Ok(true)
    }

    fn log_back_buffer_sample(&self, bitmaps: &[Bitmap]) {
        if bitmaps.is_empty() || self.back_buf.is_empty() {
            return;
        }
        let mut sum = 0u64;
        let mut count = 0u64;
        let step = ((self.width as usize * self.height as usize) / 4096).max(1);
        for px in (0..self.width as usize * self.height as usize).step_by(step) {
            let off = px * 4;
            if off + 2 >= self.back_buf.len() {
                break;
            }
            let r = self.back_buf[off] as u64;
            let g = self.back_buf[off + 1] as u64;
            let b = self.back_buf[off + 2] as u64;
            sum += (r * 299 + g * 587 + b * 114) / 1000;
            count += 1;
        }
        if count > 0 {
            // Also sample a few fixed positions to detect spatial non-uniformity.
            let scr_w = self.width as usize;
            let pixel_rgba = |px: usize, py: usize| -> (u8, u8, u8) {
                let off = (py * scr_w + px) * 4;
                if off + 2 < self.back_buf.len() {
                    (
                        self.back_buf[off],
                        self.back_buf[off + 1],
                        self.back_buf[off + 2],
                    )
                } else {
                    (0, 0, 0)
                }
            };
            let (r0, g0, b0) = pixel_rgba(0, 0);
            let (r1, g1, b1) = pixel_rgba(512.min(scr_w - 1), 128.min(self.height as usize - 1));
            let (r2, g2, b2) = pixel_rgba(scr_w / 2, self.height as usize / 2);
            log::trace!(
                "[rdp-sdl2] backbuf avg_luma={} samples={} size={}x{} px(0,0)=RGB({},{},{}) px(512,128)=RGB({},{},{}) px(mid)=RGB({},{},{})",
                sum / count,
                count,
                self.width,
                self.height,
                r0, g0, b0,
                r1, g1, b1,
                r2, g2, b2,
            );
        }
    }

    pub fn get_window_size(&self) -> (u16, u16) {
        (self.width, self.height)
    }
}
