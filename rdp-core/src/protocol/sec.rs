use crate::core::io::*;

pub const SEC_ENCRYPT: u32 = 0x0008;
pub const SEC_RESET_SEQNO: u32 = 0x0400;
pub const SEC_IGNORE_SEQNO: u32 = 0x0800;
pub const SEC_INFO_PKT: u32 = 0x0040;
pub const SEC_LICENSE_PKT: u32 = 0x0080;

pub struct SecurityLayer {
    pub encryption_method: u32,
    pub encryption_level: u32,
}

impl SecurityLayer {
    pub fn new() -> Self {
        SecurityLayer {
            encryption_method: 0,
            encryption_level: 0,
        }
    }

    pub fn build_security_header(&self, flags: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        write_u32_le(&mut buf, flags);
        buf
    }
}

impl Default for SecurityLayer {
    fn default() -> Self {
        Self::new()
    }
}
