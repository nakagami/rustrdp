pub struct Bitmap {
    pub dest_left: i32,
    pub dest_top: i32,
    pub dest_right: i32,
    pub dest_bottom: i32,
    pub width: i32,
    pub height: i32,
    pub bits_per_pixel: i32,
    pub data: Vec<u8>,
}

impl Bitmap {
    pub fn to_rgba(&self) -> Vec<u8> {
        let pixels = (self.width * self.height) as usize;
        let mut out = vec![0u8; pixels * 4];
        match self.bits_per_pixel {
            32 => {
                for i in 0..pixels {
                    let b = self.data[i * 4];
                    let g = self.data[i * 4 + 1];
                    let r = self.data[i * 4 + 2];
                    out[i * 4] = r;
                    out[i * 4 + 1] = g;
                    out[i * 4 + 2] = b;
                    out[i * 4 + 3] = 255;
                }
            }
            24 => {
                for i in 0..pixels {
                    let b = self.data[i * 3];
                    let g = self.data[i * 3 + 1];
                    let r = self.data[i * 3 + 2];
                    out[i * 4] = r;
                    out[i * 4 + 1] = g;
                    out[i * 4 + 2] = b;
                    out[i * 4 + 3] = 255;
                }
            }
            16 => {
                for i in 0..pixels {
                    let lo = self.data[i * 2] as u16;
                    let hi = self.data[i * 2 + 1] as u16;
                    let v = (hi << 8) | lo;
                    let r = ((v >> 11) & 0x1F) as u8;
                    let g = ((v >> 5) & 0x3F) as u8;
                    let b = (v & 0x1F) as u8;
                    out[i * 4] = (r << 3) | (r >> 2);
                    out[i * 4 + 1] = (g << 2) | (g >> 4);
                    out[i * 4 + 2] = (b << 3) | (b >> 2);
                    out[i * 4 + 3] = 255;
                }
            }
            _ => {}
        }
        out
    }
}
