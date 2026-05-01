use sdl2::pixels::PixelFormatEnum;
use sdl2::rect::Rect;
use sdl2::render::Canvas;
use sdl2::video::Window;
use std::error::Error;

use rdp_core::bitmap::Bitmap;

pub struct RdpUI {
    canvas: Canvas<Window>,
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

        Ok(RdpUI {
            canvas,
            width,
            height,
        })
    }

    pub fn update_screen(&mut self, bitmaps: &[Bitmap]) -> Result<(), Box<dyn Error>> {
        self.canvas.clear();

        for bitmap in bitmaps {
            self.render_bitmap(bitmap)?;
        }

        self.canvas.present();
        Ok(())
    }

    fn render_bitmap(&mut self, bitmap: &Bitmap) -> Result<(), Box<dyn Error>> {
        let texture_creator = self.canvas.texture_creator();
        
        // Convert to RGBA
        let rgba_data = bitmap.to_rgba();
        
        let mut rgba_data_mut = rgba_data.clone();
        
        let surface = sdl2::surface::Surface::from_data(
            &mut rgba_data_mut,
            bitmap.width as u32,
            bitmap.height as u32,
            bitmap.width as u32 * 4,
            PixelFormatEnum::ABGR8888,
        )?;

        let texture = texture_creator.create_texture_from_surface(&surface)?;

        let rect = Rect::new(
            bitmap.dest_left,
            bitmap.dest_top,
            bitmap.width as u32,
            bitmap.height as u32,
        );

        self.canvas.copy(&texture, None, rect)?;
        Ok(())
    }

    pub fn get_window_size(&self) -> (u16, u16) {
        (self.width, self.height)
    }
}
