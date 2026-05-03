use sdl2::keyboard::Keycode;

pub struct InputHandler {
    swap_alt_meta: bool,
}

impl InputHandler {
    pub fn new(swap_alt_meta: bool) -> Self {
        InputHandler { swap_alt_meta }
    }

    pub fn handle_keyboard_event(&self, keycode: Keycode, pressed: bool) -> Option<(u8, bool)> {
        let rdp_keycode = match keycode {
            Keycode::Escape => 0x01,
            Keycode::Num1 => 0x02,
            Keycode::Num2 => 0x03,
            Keycode::Num3 => 0x04,
            Keycode::Num4 => 0x05,
            Keycode::Num5 => 0x06,
            Keycode::Num6 => 0x07,
            Keycode::Num7 => 0x08,
            Keycode::Num8 => 0x09,
            Keycode::Num9 => 0x0A,
            Keycode::Num0 => 0x0B,
            Keycode::Minus => 0x0C,
            Keycode::Equals => 0x0D,
            Keycode::Backspace => 0x0E,
            Keycode::Tab => 0x0F,
            Keycode::Q => 0x10,
            Keycode::W => 0x11,
            Keycode::E => 0x12,
            Keycode::R => 0x13,
            Keycode::T => 0x14,
            Keycode::Y => 0x15,
            Keycode::U => 0x16,
            Keycode::I => 0x17,
            Keycode::O => 0x18,
            Keycode::P => 0x19,
            Keycode::LeftBracket => 0x1A,
            Keycode::RightBracket => 0x1B,
            Keycode::Return => 0x1C,
            Keycode::LCtrl => 0x1D,
            Keycode::A => 0x1E,
            Keycode::S => 0x1F,
            Keycode::D => 0x20,
            Keycode::F => 0x21,
            Keycode::G => 0x22,
            Keycode::H => 0x23,
            Keycode::J => 0x24,
            Keycode::K => 0x25,
            Keycode::L => 0x26,
            Keycode::Semicolon => 0x27,
            Keycode::Quote => 0x28,
            Keycode::Backquote => 0x29,
            Keycode::LShift => 0x2A,
            Keycode::Backslash => 0x2B,
            Keycode::Z => 0x2C,
            Keycode::X => 0x2D,
            Keycode::C => 0x2E,
            Keycode::V => 0x2F,
            Keycode::B => 0x30,
            Keycode::N => 0x31,
            Keycode::M => 0x32,
            Keycode::Comma => 0x33,
            Keycode::Period => 0x34,
            Keycode::Slash => 0x35,
            Keycode::RShift => 0x36,
            Keycode::PrintScreen => 0x37,
            Keycode::LAlt | Keycode::RAlt => 0x38,
            Keycode::Space => 0x39,
            Keycode::CapsLock => 0x3A,
            Keycode::F1 => 0x3B,
            Keycode::F2 => 0x3C,
            Keycode::F3 => 0x3D,
            Keycode::F4 => 0x3E,
            Keycode::F5 => 0x3F,
            Keycode::F6 => 0x40,
            Keycode::F7 => 0x41,
            Keycode::F8 => 0x42,
            Keycode::F9 => 0x43,
            Keycode::F10 => 0x44,
            Keycode::NumLockClear => 0x45,
            Keycode::ScrollLock => 0x46,
            _ => return None,
        };

        Some((rdp_keycode, pressed))
    }

    pub fn handle_mouse_event_motion(&self, x: i32, y: i32) -> (u16, u16) {
        (x as u16, y as u16)
    }

    pub fn handle_mouse_button(&self, button: sdl2::mouse::MouseButton, down: bool) -> Option<(u8, bool)> {
        let btn = match button {
            sdl2::mouse::MouseButton::Left => 1,
            sdl2::mouse::MouseButton::Right => 2,
            sdl2::mouse::MouseButton::Middle => 3,
            _ => return None,
        };
        Some((btn, down))
    }

    pub fn handle_mouse_wheel(&self, x: i32, y: i32) -> Option<i16> {
        if x == 0 && y != 0 {
            Some((y * 10) as i16)
        } else {
            None
        }
    }
}
