use sdl2::pixels::PixelFormatEnum;
use sdl2::render::{Canvas, Texture, TextureCreator};
use sdl2::video::{Window, WindowContext};
use std::error::Error;

use rdp_core::bitmap::Bitmap;

pub struct RdpUI {
    canvas: Canvas<Window>,
    texture_creator: TextureCreator<WindowContext>,
    /// Persistent RGBA back-buffer: compositing target for incremental bitmap tiles.
    /// Uploading the whole buffer on each update avoids canvas.clear() which would
    /// wipe previously rendered tiles.
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
        let back_buf = vec![0u8; width as usize * height as usize * 4];

        Ok(RdpUI {
            canvas,
            texture_creator,
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

    /// Upload the back-buffer to the GPU and flip.
    fn present(&mut self) -> Result<(), Box<dyn Error>> {
        let mut tex: Texture = self.texture_creator.create_texture_streaming(
            PixelFormatEnum::ABGR8888,
            self.width as u32,
            self.height as u32,
        )?;
        let row_bytes = self.width as usize * 4;
        tex.update(None, &self.back_buf, row_bytes)?;
        self.canvas.copy(&tex, None, None)?;
        self.canvas.present();
        Ok(())
    }

    /// Blit one bitmap tile (already RGBA) into the CPU back-buffer at its destination.
    fn blit_bitmap_to_buf(&mut self, bitmap: &Bitmap) {
        let rgba = bitmap.to_rgba();
        let bw = bitmap.width as usize;
        let bh = bitmap.height as usize;
        let dest_x = bitmap.dest_left.max(0) as usize;
        let dest_y = bitmap.dest_top.max(0) as usize;
        let scr_w = self.width as usize;
        let scr_h = self.height as usize;

        for row in 0..bh {
            let dy = dest_y + row;
            if dy >= scr_h { break; }
            let src_start = row * bw * 4;
            let dst_start = (dy * scr_w + dest_x) * 4;
            let copy_pixels = bw.min(scr_w.saturating_sub(dest_x));
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
