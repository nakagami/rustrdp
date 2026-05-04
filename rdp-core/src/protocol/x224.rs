use crate::error::RdpError;
use crate::protocol::tpkt::Tpkt;
use crate::protocol::Transport;

const TPDU_CONNECTION_REQUEST: u8 = 0xE0;
const TPDU_CONNECTION_CONFIRM: u8 = 0xD0;

const TYPE_RDP_NEG_REQ: u8 = 0x01;
const TYPE_RDP_NEG_RSP: u8 = 0x02;
const TYPE_RDP_NEG_FAILURE: u8 = 0x03;

pub const PROTOCOL_RDP: u32 = 0x00000000;
pub const PROTOCOL_SSL: u32 = 0x00000001;
pub const PROTOCOL_HYBRID: u32 = 0x00000002;

pub struct X224<T: Transport> {
    tpkt: Tpkt<T>,
    pub selected_protocol: u32,
}

impl<T: Transport> X224<T> {
    pub fn new(tpkt: Tpkt<T>) -> Self {
        X224 {
            tpkt,
            selected_protocol: PROTOCOL_RDP,
        }
    }

    pub async fn connect(&mut self, username: &str) -> Result<(), RdpError> {
        let cookie = format!("Cookie: mstshash={}\r\n", username);
        let cookie_bytes = cookie.as_bytes();
        let neg_req = [TYPE_RDP_NEG_REQ, 0x00, 0x08, 0x00, 0x03, 0x00, 0x00, 0x00];

        // LI = bytes after LI field to end of TPDU header
        //    = code(1) + dst-ref(2) + src-ref(2) + class(1) + variable = 6 + var_len
        let var_len = cookie_bytes.len() + neg_req.len();
        let li = (6 + var_len) as u8;

        let mut buf = Vec::new();
        buf.push(li);
        buf.push(TPDU_CONNECTION_REQUEST);
        buf.extend_from_slice(&[0x00, 0x00]); // DST-REF
        buf.extend_from_slice(&[0x00, 0x00]); // SRC-REF
        buf.push(0x00); // class
        buf.extend_from_slice(cookie_bytes);
        buf.extend_from_slice(&neg_req);

        if log::log_enabled!(log::Level::Trace) {
            log::trace!(
                "[X224] send CR  li=0x{:02x} len={} hex={}",
                li,
                buf.len(),
                hex_dump(&buf)
            );
        }

        self.tpkt.send(&buf).await
    }

    pub async fn recv_confirm(&mut self) -> Result<u32, RdpError> {
        let (_, data) = self.tpkt.recv().await?;

        if log::log_enabled!(log::Level::Trace) {
            log::trace!("[X224] recv CC  len={} hex={}", data.len(), hex_dump(&data));
        }

        if data.len() < 7 {
            return Err(RdpError::Protocol("X224 confirm too short".into()));
        }
        let tpdu_code = data[1];
        if tpdu_code != TPDU_CONNECTION_CONFIRM {
            return Err(RdpError::Protocol(format!(
                "Expected CC, got {:02x}",
                tpdu_code
            )));
        }
        let li = data[0] as usize;
        if li >= 7 && data.len() >= 15 {
            let neg_type = data[7];
            if neg_type == TYPE_RDP_NEG_RSP {
                // data[8]=flags, data[9..11]=length, data[11..15]=selectedProtocol
                let proto = u32::from_le_bytes([data[11], data[12], data[13], data[14]]);
                self.selected_protocol = proto;
                log::debug!("[X224] selectedProtocol=0x{:08x}", proto);
                return Ok(proto);
            } else if neg_type == TYPE_RDP_NEG_FAILURE {
                let code = if data.len() >= 15 {
                    u32::from_le_bytes([data[11], data[12], data[13], data[14]])
                } else {
                    0
                };
                return Err(RdpError::Protocol(format!(
                    "RDP negotiation failure code=0x{:08x}",
                    code
                )));
            }
        }
        Ok(PROTOCOL_RDP)
    }

    pub async fn send(&mut self, data: &[u8]) -> Result<(), RdpError> {
        let mut buf = vec![0x02, 0xF0, 0x80];
        buf.extend_from_slice(data);

        if log::log_enabled!(log::Level::Trace) {
            log::trace!("[X224] send MCS len={} hex={}", buf.len(), hex_dump(&buf));
        }

        self.tpkt.send(&buf).await
    }

    pub async fn recv(&mut self) -> Result<(bool, Vec<u8>), RdpError> {
        let (is_fp, data) = self.tpkt.recv().await?;

        if log::log_enabled!(log::Level::Trace) {
            if is_fp {
                log::trace!("[X224] recv FastPath len={}", data.len());
            } else {
                log::trace!(
                    "[X224] recv MCS  len={} hex={}",
                    data.len(),
                    hex_dump(&data)
                );
            }
        }

        if is_fp {
            return Ok((true, data));
        }
        if data.len() < 3 {
            return Err(RdpError::Protocol("X224 data too short".into()));
        }
        Ok((false, data[3..].to_vec()))
    }

    pub fn selected_protocol(&self) -> u32 {
        self.selected_protocol
    }

    pub fn into_tpkt(self) -> Tpkt<T> {
        self.tpkt
    }

    pub fn tpkt_mut(&mut self) -> &mut Tpkt<T> {
        &mut self.tpkt
    }
}

fn hex_dump(data: &[u8]) -> String {
    let limit = data.len().min(64);
    let hex: Vec<String> = data[..limit].iter().map(|b| format!("{:02x}", b)).collect();
    if data.len() > limit {
        format!("{}...({}bytes)", hex.join(" "), data.len())
    } else {
        hex.join(" ")
    }
}
