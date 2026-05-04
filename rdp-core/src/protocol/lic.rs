use crate::error::RdpError;

const ERROR_ALERT: u8 = 0xFF;
const NEW_LICENSE: u8 = 0x03;
const UPGRADE_LICENSE: u8 = 0x04;

pub const STATUS_VALID_CLIENT: u32 = 0x00000007;

pub fn parse_license_pdu(data: &[u8]) -> Result<bool, RdpError> {
    if data.len() < 2 {
        return Err(RdpError::Protocol("License PDU too short".into()));
    }
    let msg_type = data[0];
    match msg_type {
        ERROR_ALERT => {
            if data.len() >= 8 {
                let error_code = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                if error_code == STATUS_VALID_CLIENT {
                    return Ok(true);
                }
            }
            Err(RdpError::Protocol(format!(
                "License error type: {:02x}",
                msg_type
            )))
        }
        NEW_LICENSE | UPGRADE_LICENSE => Ok(true),
        _ => Ok(false),
    }
}
