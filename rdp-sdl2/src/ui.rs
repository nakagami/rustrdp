use sdl2::pixels::PixelFormatEnum;
use sdl2::rect::Rect;
use sdl2::render::{Canvas, Texture, TextureCreator};
use sdl2::video::{Window, WindowContext};
use std::error::Error;

use rdp_core::bitmap::Bitmap;

// Fields are declared in drop order (first declared → first dropped).
// Texture must be destroyed before the renderer (canvas), so it is declared first.
pub struct RdpUI {
    texture: Texture,                               // dropped 1st: SDL_DestroyTexture
    texture_creator: TextureCreator<WindowContext>, // dropped 2nd
    canvas: Canvas<Window>,                         // dropped 3rd: SDL_DestroyRenderer
    /// CPU-side BGRA back-buffer: compositing target for incremental bitmap tiles.
    back_buf: Vec<u8>,
    width: u16,
    height: u16,
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

        let canvas = match window.into_canvas().accelerated().build() {
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
        };
        let texture_creator = canvas.texture_creator();
        // ARGB8888 = BGRA in memory on little-endian (macOS/Linux/Windows x86).
        // AVC frames from VideoToolbox are already BGRA — no per-pixel swap needed.
        let texture = texture_creator.create_texture_streaming(
            PixelFormatEnum::ARGB8888,
            width as u32,
            height as u32,
        )?;
        let back_buf = vec![0u8; width as usize * height as usize * 4];

        Ok(RdpUI {
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
            self.canvas.copy(&self.texture, None, None)?;
            self.canvas.present();
        }
        Ok(())
    }

    /// Re-present the current back-buffer without modifying it.
    /// Call this on SDL Exposed / Restored events so the window redraws itself.
    pub fn repaint(&mut self) -> Result<(), Box<dyn Error>> {
        self.present_full()
    }

    pub fn resize(&mut self, width: u16, height: u16) -> Result<(), Box<dyn Error>> {
        if self.width == width && self.height == height {
            return Ok(());
        }
        self.canvas
            .window_mut()
            .set_size(width as u32, height as u32)?;
        self.texture = self.texture_creator.create_texture_streaming(
            PixelFormatEnum::ARGB8888,
            width as u32,
            height as u32,
        )?;
        self.back_buf = vec![0u8; width as usize * height as usize * 4];
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Upload the full back-buffer to the persistent streaming texture and flip.
    fn present_full(&mut self) -> Result<(), Box<dyn Error>> {
        let row_bytes = self.width as usize * 4;
        self.texture.update(None, &self.back_buf, row_bytes)?;
        self.canvas.copy(&self.texture, None, None)?;
        self.canvas.present();
        Ok(())
    }

    /// Blit one bitmap tile into the CPU back-buffer, then upload that rect to the SDL texture.
    ///
    /// Texture format is ARGB8888 (= BGRA byte order on little-endian).
    /// 32-bpp bitmap data from RDPGFX/AVC is already BGRA, so bpp=32 uses a direct
    /// `copy_from_slice` per row (pure memcpy — fast even in debug builds).
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
        let back_pitch = scr_w * 4;

        // Write pixels into the CPU back-buffer (BGRA format).
        for row in 0..clip_h {
            let src_start = ((src_y + row) * bw + src_x) * src_bpp;
            let back_start = ((dest_y + row) * scr_w + dest_x) * 4;
            let back_row = &mut self.back_buf[back_start..back_start + clip_w * 4];
            match bitmap.bits_per_pixel {
                32 => {
                    let src_end = src_start + clip_w * 4;
                    if src_end > src_data.len() {
                        return Ok(false);
                    }
                    // Source is BGRA; back-buffer is BGRA: direct memcpy, no per-pixel work.
                    back_row.copy_from_slice(&src_data[src_start..src_end]);
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
                        // Source is BGR; BGRA back-buffer: add A=255, no swap.
                        back_row[d] = src_row[s];
                        back_row[d + 1] = src_row[s + 1];
                        back_row[d + 2] = src_row[s + 2];
                        back_row[d + 3] = 255;
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
                        // Expand to BGRA.
                        back_row[d] = (b5 << 3) | (b5 >> 2);
                        back_row[d + 1] = (g6 << 2) | (g6 >> 4);
                        back_row[d + 2] = (r5 << 3) | (r5 >> 2);
                        back_row[d + 3] = 255;
                    }
                }
                _ => {}
            }
        }

        // Upload the updated rect from back_buf to the SDL texture via SDL's C memcpy path.
        let back_start = (dest_y * scr_w + dest_x) * 4;
        self.texture
            .update(Some(rect), &self.back_buf[back_start..], back_pitch)?;
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
