use sdl2::keyboard::Keycode;

const KBDFLAGS_EXTENDED: u16 = 0x0100;

pub struct InputHandler {
    swap_alt_meta: bool,
}

impl InputHandler {
    pub fn new(swap_alt_meta: bool) -> Self {
        InputHandler { swap_alt_meta }
    }

    /// Returns `(flags, scancode)` for the given keycode, or `None` if unmapped.
    /// `flags` contains `KBDFLAGS_EXTENDED` for extended keys (right-side or navigation keys).
    pub fn handle_keyboard_event(&self, keycode: Keycode, _pressed: bool) -> Option<(u16, u8)> {
        let (flags, scancode) = match keycode {
            Keycode::Escape => (0, 0x01),
            Keycode::Num1 => (0, 0x02),
            Keycode::Num2 => (0, 0x03),
            Keycode::Num3 => (0, 0x04),
            Keycode::Num4 => (0, 0x05),
            Keycode::Num5 => (0, 0x06),
            Keycode::Num6 => (0, 0x07),
            Keycode::Num7 => (0, 0x08),
            Keycode::Num8 => (0, 0x09),
            Keycode::Num9 => (0, 0x0A),
            Keycode::Num0 => (0, 0x0B),
            Keycode::Minus => (0, 0x0C),
            Keycode::Equals => (0, 0x0D),
            Keycode::Backspace => (0, 0x0E),
            Keycode::Tab => (0, 0x0F),
            Keycode::Q => (0, 0x10),
            Keycode::W => (0, 0x11),
            Keycode::E => (0, 0x12),
            Keycode::R => (0, 0x13),
            Keycode::T => (0, 0x14),
            Keycode::Y => (0, 0x15),
            Keycode::U => (0, 0x16),
            Keycode::I => (0, 0x17),
            Keycode::O => (0, 0x18),
            Keycode::P => (0, 0x19),
            Keycode::LeftBracket => (0, 0x1A),
            Keycode::RightBracket => (0, 0x1B),
            Keycode::Return => (0, 0x1C),
            Keycode::LCtrl => (0, 0x1D),
            Keycode::A => (0, 0x1E),
            Keycode::S => (0, 0x1F),
            Keycode::D => (0, 0x20),
            Keycode::F => (0, 0x21),
            Keycode::G => (0, 0x22),
            Keycode::H => (0, 0x23),
            Keycode::J => (0, 0x24),
            Keycode::K => (0, 0x25),
            Keycode::L => (0, 0x26),
            Keycode::Semicolon => (0, 0x27),
            Keycode::Quote => (0, 0x28),
            Keycode::Backquote => (0, 0x29),
            Keycode::LShift => (0, 0x2A),
            Keycode::Backslash => (0, 0x2B),
            Keycode::Z => (0, 0x2C),
            Keycode::X => (0, 0x2D),
            Keycode::C => (0, 0x2E),
            Keycode::V => (0, 0x2F),
            Keycode::B => (0, 0x30),
            Keycode::N => (0, 0x31),
            Keycode::M => (0, 0x32),
            Keycode::Comma => (0, 0x33),
            Keycode::Period => (0, 0x34),
            Keycode::Slash => (0, 0x35),
            Keycode::RShift => (0, 0x36),
            Keycode::PrintScreen => (0, 0x37),
            Keycode::LAlt => {
                if self.swap_alt_meta {
                    (KBDFLAGS_EXTENDED, 0x5B) // → LGui (MetaLeft)
                } else {
                    (0, 0x38)
                }
            }
            Keycode::RAlt => {
                if self.swap_alt_meta {
                    (KBDFLAGS_EXTENDED, 0x5C) // → RGui (MetaRight)
                } else {
                    (KBDFLAGS_EXTENDED, 0x38)
                }
            }
            Keycode::LGui => {
                if self.swap_alt_meta {
                    (0, 0x38) // → LAlt
                } else {
                    (KBDFLAGS_EXTENDED, 0x5B)
                }
            }
            Keycode::RGui => {
                if self.swap_alt_meta {
                    (KBDFLAGS_EXTENDED, 0x38) // → RAlt
                } else {
                    (KBDFLAGS_EXTENDED, 0x5C)
                }
            }
            Keycode::Space => (0, 0x39),
            Keycode::CapsLock => (0, 0x3A),
            Keycode::F1 => (0, 0x3B),
            Keycode::F2 => (0, 0x3C),
            Keycode::F3 => (0, 0x3D),
            Keycode::F4 => (0, 0x3E),
            Keycode::F5 => (0, 0x3F),
            Keycode::F6 => (0, 0x40),
            Keycode::F7 => (0, 0x41),
            Keycode::F8 => (0, 0x42),
            Keycode::F9 => (0, 0x43),
            Keycode::F10 => (0, 0x44),
            Keycode::NumLockClear => (0, 0x45),
            Keycode::ScrollLock => (0, 0x46),
            _ => return None,
        };

        Some((flags, scancode))
    }

    pub fn handle_mouse_event_motion(&self, x: i32, y: i32) -> (u16, u16) {
        (x as u16, y as u16)
    }

    pub fn handle_mouse_button(
        &self,
        button: sdl2::mouse::MouseButton,
        down: bool,
    ) -> Option<(u8, bool)> {
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
