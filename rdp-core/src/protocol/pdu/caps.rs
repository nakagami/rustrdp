use crate::core::io::*;

pub const CAPSTYPE_GENERAL: u16 = 0x0001;
pub const CAPSTYPE_BITMAP: u16 = 0x0002;
pub const CAPSTYPE_ORDER: u16 = 0x0003;
pub const CAPSTYPE_BITMAPCACHE: u16 = 0x0004;
pub const CAPSTYPE_CONTROL: u16 = 0x0005;
pub const CAPSTYPE_ACTIVATION: u16 = 0x0007;
pub const CAPSTYPE_POINTER: u16 = 0x0008;
pub const CAPSTYPE_SHARE: u16 = 0x0009;
pub const CAPSTYPE_COLORS: u16 = 0x000A;
pub const CAPSTYPE_INPUT: u16 = 0x000D;
pub const CAPSTYPE_FONT: u16 = 0x000E;
pub const CAPSTYPE_BRUSH: u16 = 0x000F;
pub const CAPSTYPE_GLYPHCACHE: u16 = 0x0010;
pub const CAPSTYPE_OFFSCREENCACHE: u16 = 0x0011;
pub const CAPSTYPE_BITMAPCACHE_HOSTSUPPORT: u16 = 0x0012;
pub const CAPSTYPE_VIRTUAL_CHANNEL: u16 = 0x0014;
pub const CAPSTYPE_SOUND: u16 = 0x0017;

pub fn build_general_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 1);
    write_u16_le(&mut inner, 3);
    write_u16_le(&mut inner, 0x200);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0x04DC);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    build_capability(CAPSTYPE_GENERAL, &inner)
}

pub fn build_bitmap_capability(width: u16, height: u16) -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 24);
    write_u16_le(&mut inner, 1);
    write_u16_le(&mut inner, 1);
    write_u16_le(&mut inner, 1);
    write_u16_le(&mut inner, width);
    write_u16_le(&mut inner, height);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 1);
    write_u16_le(&mut inner, 0);
    write_u8(&mut inner, 0);
    write_u8(&mut inner, 0);
    write_u16_le(&mut inner, 1);
    write_u16_le(&mut inner, 0);
    build_capability(CAPSTYPE_BITMAP, &inner)
}

pub fn build_order_capability() -> Vec<u8> {
    let mut inner = vec![0u8; 20];
    write_u32_le(&mut inner, 0);
    write_u16_le(&mut inner, 1);
    write_u16_le(&mut inner, 20);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 1);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0x22);
    inner.extend_from_slice(&[0u8; 32]);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    write_u32_le(&mut inner, 0);
    write_u32_le(&mut inner, 480 * 480);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    build_capability(CAPSTYPE_ORDER, &inner)
}

pub fn build_input_capability(kbd_layout: u32) -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 0x0001 | 0x0004);
    write_u16_le(&mut inner, 0);
    write_u32_le(&mut inner, kbd_layout);
    write_u32_le(&mut inner, 4);
    write_u32_le(&mut inner, 0);
    write_u32_le(&mut inner, 12);
    inner.extend_from_slice(&[0u8; 64]);
    build_capability(CAPSTYPE_INPUT, &inner)
}

pub fn build_virtual_channel_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u32_le(&mut inner, 0);
    write_u32_le(&mut inner, 1600 * 100);
    build_capability(CAPSTYPE_VIRTUAL_CHANNEL, &inner)
}

pub fn build_pointer_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    build_capability(CAPSTYPE_POINTER, &inner)
}

pub fn build_sound_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    build_capability(CAPSTYPE_SOUND, &inner)
}

pub fn build_font_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u16_le(&mut inner, 0x0003);
    write_u16_le(&mut inner, 0);
    build_capability(CAPSTYPE_FONT, &inner)
}

pub fn build_brush_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u32_le(&mut inner, 0);
    build_capability(CAPSTYPE_BRUSH, &inner)
}

pub fn build_glyph_capability() -> Vec<u8> {
    let mut inner = vec![0u8; 40];
    write_u32_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    build_capability(CAPSTYPE_GLYPHCACHE, &inner)
}

pub fn build_offscreen_capability() -> Vec<u8> {
    let mut inner = Vec::new();
    write_u32_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    write_u16_le(&mut inner, 0);
    build_capability(CAPSTYPE_OFFSCREENCACHE, &inner)
}

pub fn build_all_capabilities(width: u16, height: u16, kbd_layout: u32) -> Vec<u8> {
    let mut caps = Vec::new();
    caps.extend_from_slice(&build_general_capability());
    caps.extend_from_slice(&build_bitmap_capability(width, height));
    caps.extend_from_slice(&build_order_capability());
    caps.extend_from_slice(&build_pointer_capability());
    caps.extend_from_slice(&build_input_capability(kbd_layout));
    caps.extend_from_slice(&build_virtual_channel_capability());
    caps.extend_from_slice(&build_sound_capability());
    caps.extend_from_slice(&build_font_capability());
    caps.extend_from_slice(&build_brush_capability());
    caps.extend_from_slice(&build_glyph_capability());
    caps.extend_from_slice(&build_offscreen_capability());
    caps
}

fn build_capability(cap_type: u16, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u16_le(&mut buf, cap_type);
    write_u16_le(&mut buf, (data.len() + 4) as u16);
    buf.extend_from_slice(data);
    buf
}
