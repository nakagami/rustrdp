use crate::core::io::*;

pub const INPUT_EVENT_SYNC: u16 = 0x0000;
pub const INPUT_EVENT_SCANCODE: u16 = 0x0004;
pub const INPUT_EVENT_UNICODE: u16 = 0x0005;
pub const INPUT_EVENT_MOUSE: u16 = 0x8001;
pub const INPUT_EVENT_MOUSEX: u16 = 0x8002;

pub const KBDFLAGS_EXTENDED: u16 = 0x0100;
pub const KBDFLAGS_KEYUP: u16 = 0x8000;

pub const PTRFLAGS_BUTTON1: u16 = 0x1000;
pub const PTRFLAGS_BUTTON2: u16 = 0x2000;
pub const PTRFLAGS_BUTTON3: u16 = 0x4000;
pub const PTRFLAGS_DOWN: u16 = 0x8000;
pub const PTRFLAGS_MOVE: u16 = 0x0800;
pub const PTRFLAGS_WHEEL: u16 = 0x0200;
pub const PTRFLAGS_WHEEL_NEGATIVE: u16 = 0x0100;

pub fn build_keyboard_event(flags: u16, scancode: u8) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u16_le(&mut buf, 0);
    write_u16_le(&mut buf, INPUT_EVENT_SCANCODE);
    write_u16_le(&mut buf, flags);
    write_u16_le(&mut buf, scancode as u16);
    buf
}

pub fn build_unicode_keyboard_event(flags: u16, code_point: u16) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u16_le(&mut buf, 0);
    write_u16_le(&mut buf, INPUT_EVENT_UNICODE);
    write_u16_le(&mut buf, flags);
    write_u16_le(&mut buf, code_point);
    buf
}

pub fn build_mouse_event(flags: u16, x: u16, y: u16) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u16_le(&mut buf, 0);
    write_u16_le(&mut buf, INPUT_EVENT_MOUSE);
    write_u16_le(&mut buf, flags);
    write_u16_le(&mut buf, x);
    write_u16_le(&mut buf, y);
    buf
}

pub fn build_sync_event(flags: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u16_le(&mut buf, 0);
    write_u16_le(&mut buf, INPUT_EVENT_SYNC);
    write_u32_le(&mut buf, flags);
    buf
}

pub fn wrap_input_pdu(events: &[Vec<u8>]) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u16_le(&mut buf, events.len() as u16);
    write_u16_le(&mut buf, 0);
    for ev in events {
        buf.extend_from_slice(ev);
    }
    buf
}
