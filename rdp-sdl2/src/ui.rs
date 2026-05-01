use sdl2::pixels::PixelFormatEnum;
use sdl2::render::{Canvas, Texture, TextureCreator};
use sdl2::video::{Window, WindowContext};
use std::error::Error;

use rdp_core::bitmap::Bitmap;

// Fields are declared in drop order (first declared → first dropped).
// Texture must be destroyed before the renderer (canvas), so it is declared first.
pub struct RdpUI {
    texture: Texture,                                // dropped 1st: SDL_DestroyTexture
    texture_creator: TextureCreator<WindowContext>,  // dropped 2nd
    canvas: Canvas<Window>,                          // dropped 3rd: SDL_DestroyRenderer
    /// CPU-side RGBA back-buffer: compositing target for incremental bitmap tiles.
    back_buf: Vec<u8>,
    width: u16,
    height: u16,
}

impl RdpUI {
    pub fn new(sdl_context: &sdl2::Sdl, width: u16, height: u16, title: &str) -> Result<Self, Box<dyn Error>> {
        let video_subsystem = sdl_context.video()?;
        let window = video_subsystem
            .window(title, width as u32, height as u32)
            .position_centered()
            .build()?;

        let canvas = window.into_canvas().build()?;
        let texture_creator = canvas.texture_creator();
        // Create one streaming texture and reuse it every frame.
        let texture = texture_creator.create_texture_streaming(
            PixelFormatEnum::RGBA32,
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

    /// Composite bitmap tiles into the back-buffer then present the full frame.
    pub fn update_screen(&mut self, bitmaps: &[Bitmap]) -> Result<(), Box<dyn Error>> {
        for bitmap in bitmaps {
            self.blit_bitmap_to_buf(bitmap);
        }
        self.present()
    }

    /// Re-present the current back-buffer without modifying it.
    /// Call this on SDL Exposed / Restored events so the window redraws itself.
    pub fn repaint(&mut self) -> Result<(), Box<dyn Error>> {
        self.present()
    }

    /// Upload the back-buffer to the persistent streaming texture and flip.
    fn present(&mut self) -> Result<(), Box<dyn Error>> {
        let row_bytes = self.width as usize * 4;
        self.texture.update(None, &self.back_buf, row_bytes)?;
        self.canvas.copy(&self.texture, None, None)?;
        self.canvas.present();
        Ok(())
    }

    /// Blit one bitmap tile (already RGBA) into the CPU back-buffer at its destination.
    fn blit_bitmap_to_buf(&mut self, bitmap: &Bitmap) {
        let rgba = bitmap.to_rgba();
        let bw = bitmap.width as usize;   // source row stride
        let bh = bitmap.height as usize;
        // Clamp to dest rect: bitmap Width may be padded wider than
        // DestRight-DestLeft+1 (RDP bitmaps on Linux), or conversely
        // DestRight-DestLeft+1 may exceed Width (surface-bits on Windows).
        let clip_w = bw.min((bitmap.dest_right - bitmap.dest_left + 1).max(0) as usize);
        let clip_h = bh.min((bitmap.dest_bottom - bitmap.dest_top + 1).max(0) as usize);
        let dest_x = bitmap.dest_left.max(0) as usize;
        let dest_y = bitmap.dest_top.max(0) as usize;
        let scr_w = self.width as usize;
        let scr_h = self.height as usize;

        for row in 0..clip_h {
            let dy = dest_y + row;
            if dy >= scr_h { break; }
            let src_start = row * bw * 4;  // use stride (bw), not clip_w
            let dst_start = (dy * scr_w + dest_x) * 4;
            let copy_pixels = clip_w.min(scr_w.saturating_sub(dest_x));
            let src_end = src_start + copy_pixels * 4;
            let dst_end = dst_start + copy_pixels * 4;
            if src_end <= rgba.len() && dst_end <= self.back_buf.len() {
                self.back_buf[dst_start..dst_end].copy_from_slice(&rgba[src_start..src_end]);
            }
        }
    }

    pub fn get_window_size(&self) -> (u16, u16) {
        (self.width, self.height)
    }
}
