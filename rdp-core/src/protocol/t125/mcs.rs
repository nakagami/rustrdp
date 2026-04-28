use crate::error::RdpError;
use crate::protocol::Transport;
use crate::protocol::x224::X224;
use crate::core::io::*;
use super::ber;
use super::gcc::ServerData;

const MCS_CONNECT_INITIAL: u8 = 0x65;
const MCS_CONNECT_RESPONSE: u8 = 0x66;
const MCS_ERECT_DOMAIN_REQUEST: u8 = 1;
const MCS_DISCONNECT_PROVIDER_ULTIMATUM: u8 = 8;
const MCS_ATTACH_USER_REQUEST: u8 = 10;
const MCS_ATTACH_USER_CONFIRM: u8 = 11;
const MCS_CHANNEL_JOIN_REQUEST: u8 = 14;
const MCS_CHANNEL_JOIN_CONFIRM: u8 = 15;
const MCS_SEND_DATA_REQUEST: u8 = 25;
const MCS_SEND_DATA_INDICATION: u8 = 26;

pub struct McsClient<T: Transport> {
    pub x224: X224<T>,
    pub user_channel: u16,
    pub io_channel: u16,
    pub channels: Vec<u16>,
}

fn encode_domain_params(max_channels: i32, max_users: i32, max_tokens: i32, num_priorities: i32, min_throughput: i32, max_height: i32, max_pdu_size: i32, proto_ver: i32) -> Vec<u8> {
    let mut seq = Vec::new();
    seq.extend_from_slice(&ber::encode_integer(max_channels));
    seq.extend_from_slice(&ber::encode_integer(max_users));
    seq.extend_from_slice(&ber::encode_integer(max_tokens));
    seq.extend_from_slice(&ber::encode_integer(num_priorities));
    seq.extend_from_slice(&ber::encode_integer(min_throughput));
    seq.extend_from_slice(&ber::encode_integer(max_height));
    seq.extend_from_slice(&ber::encode_integer(max_pdu_size));
    seq.extend_from_slice(&ber::encode_integer(proto_ver));
    ber::encode_sequence(&seq)
}

impl<T: Transport> McsClient<T> {
    pub fn new(x224: X224<T>) -> Self {
        McsClient {
            x224,
            user_channel: 0,
            io_channel: 1003,
            channels: Vec::new(),
        }
    }

    pub async fn connect(&mut self, gcc_data: &[u8]) -> Result<(), RdpError> {
        let calling_domain = ber::encode_octet_string(&[0x01]);
        let called_domain = ber::encode_octet_string(&[0x01]);
        let upward_flag = ber::encode_bool(true);

        let target_params = encode_domain_params(34, 2, 0, 1, 0, 1, 65535, 2);
        let min_params = encode_domain_params(1, 1, 1, 1, 0, 1, 1056, 2);
        let max_params = encode_domain_params(65535, 64535, 0, 1, 0, 1, 65535, 2);
        let user_data = ber::encode_octet_string(gcc_data);

        let mut inner = Vec::new();
        inner.extend_from_slice(&calling_domain);
        inner.extend_from_slice(&called_domain);
        inner.extend_from_slice(&upward_flag);
        inner.extend_from_slice(&target_params);
        inner.extend_from_slice(&min_params);
        inner.extend_from_slice(&max_params);
        inner.extend_from_slice(&user_data);

        let mut buf = Vec::new();
        buf.push(0x7F);
        buf.push(MCS_CONNECT_INITIAL);
        buf.extend_from_slice(&ber::encode_length(inner.len()));
        buf.extend_from_slice(&inner);

        self.x224.send(&buf).await
    }

    pub async fn recv_connect_response(&mut self) -> Result<ServerData, RdpError> {
        let (_, data) = self.x224.recv().await?;
        let mut pos = 0;

        if data.len() < 2 || data[0] != 0x7F || data[1] != MCS_CONNECT_RESPONSE {
            return Err(RdpError::Protocol("Expected MCS Connect Response".into()));
        }
        pos += 2;
        let _len = ber::decode_length(&data, &mut pos)?;

        let result = ber::decode_integer(&data, &mut pos)?;
        if result != 0 {
            return Err(RdpError::Protocol(format!("MCS Connect Response failed: {}", result)));
        }

        let _connect_id = ber::decode_integer(&data, &mut pos)?;

        if pos >= data.len() || data[pos] != 0x30 {
            return Err(RdpError::Protocol("Expected domain params SEQUENCE".into()));
        }
        pos += 1;
        let dp_len = ber::decode_length(&data, &mut pos)?;
        pos += dp_len;

        let user_data = ber::decode_octet_string(&data, &mut pos)?;

        let gcc_resp = parse_gcc_conference_response(&user_data)?;
        self.io_channel = gcc_resp.io_channel;
        self.channels = gcc_resp.channels.clone();

        Ok(gcc_resp)
    }

    pub async fn erect_domain(&mut self) -> Result<(), RdpError> {
        let mut buf = Vec::new();
        buf.push((MCS_ERECT_DOMAIN_REQUEST << 2) as u8);
        write_u8(&mut buf, 0x01);
        write_u8(&mut buf, 0x00);
        write_u8(&mut buf, 0x01);
        write_u8(&mut buf, 0x00);
        self.x224.send(&buf).await
    }

    pub async fn attach_user(&mut self) -> Result<(), RdpError> {
        let buf = vec![(MCS_ATTACH_USER_REQUEST << 2) as u8];
        self.x224.send(&buf).await
    }

    pub async fn recv_attach_user_confirm(&mut self) -> Result<(), RdpError> {
        let (_, data) = self.x224.recv().await?;
        if data.is_empty() || (data[0] >> 2) != MCS_ATTACH_USER_CONFIRM {
            return Err(RdpError::Protocol("Expected Attach User Confirm".into()));
        }
        // Layout: [type=1B] [result=1B] [initiator=2B BE]
        if data.len() >= 4 {
            self.user_channel = u16::from_be_bytes([data[2], data[3]]) + 1001;
        }
        Ok(())
    }

    pub async fn channel_join(&mut self, channel_id: u16) -> Result<(), RdpError> {
        let mut buf = Vec::new();
        buf.push((MCS_CHANNEL_JOIN_REQUEST << 2) as u8);
        write_u16_be(&mut buf, self.user_channel - 1001);
        write_u16_be(&mut buf, channel_id);
        self.x224.send(&buf).await
    }

    pub async fn recv_channel_join_confirm(&mut self) -> Result<(), RdpError> {
        let (_, data) = self.x224.recv().await?;
        if data.is_empty() || (data[0] >> 2) != MCS_CHANNEL_JOIN_CONFIRM {
            return Err(RdpError::Protocol("Expected Channel Join Confirm".into()));
        }
        Ok(())
    }

    pub async fn send_data(&mut self, channel_id: u16, data: &[u8]) -> Result<(), RdpError> {
        let mut buf = Vec::new();
        buf.push((MCS_SEND_DATA_REQUEST << 2) as u8);
        write_u16_be(&mut buf, self.user_channel - 1001);
        write_u16_be(&mut buf, channel_id);
        buf.push(0x70);
        // PER length: 1-byte for < 128, 2-byte (0x8000 | len) for >= 128
        if data.len() < 128 {
            buf.push(data.len() as u8);
        } else {
            let data_len = data.len() as u16 | 0x8000;
            write_u16_be(&mut buf, data_len);
        }
        buf.extend_from_slice(data);
        self.x224.send(&buf).await
    }

    pub async fn recv_data(&mut self) -> Result<(u16, Vec<u8>), RdpError> {
        let (is_fp, data) = self.x224.recv().await?;
        if is_fp {
            return Ok((0xFFFF, data));
        }
        if data.is_empty() {
            return Err(RdpError::Protocol("MCS recv_data: empty".into()));
        }
        let pdu_type = data[0] >> 2;
        if pdu_type == MCS_DISCONNECT_PROVIDER_ULTIMATUM {
            return Err(RdpError::Closed);
        }
        if pdu_type != MCS_SEND_DATA_INDICATION {
            return Err(RdpError::Protocol(format!("MCS recv_data: unexpected type {}", pdu_type)));
        }
        if data.len() < 7 {
            return Err(RdpError::Protocol("MCS recv_data: too short".into()));
        }
        let channel_id = u16::from_be_bytes([data[3], data[4]]);
        // PER length field at byte 6: if high bit set, two-byte length (skip byte 7 too)
        let payload_start = if data[6] & 0x80 != 0 { 8 } else { 7 };
        if payload_start > data.len() {
            return Err(RdpError::Protocol("MCS recv_data: payload_start out of range".into()));
        }
        let payload = data[payload_start..].to_vec();
        Ok((channel_id, payload))
    }
}

fn parse_gcc_conference_response(data: &[u8]) -> Result<ServerData, RdpError> {
    let mut server_data = ServerData {
        io_channel: 1003,
        encryption_method: 0,
        server_random: Vec::new(),
        server_certificate: Vec::new(),
        channels: Vec::new(),
        msg_channel: None,
    };

    let mut pos = 0;
    while pos + 4 <= data.len() {
        let block_type = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if block_type == 0x0C01 || block_type == 0x0C02 || block_type == 0x0C03 || block_type == 0x0C04 {
            break;
        }
        pos += 1;
    }

    while pos + 4 <= data.len() {
        let block_type = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let block_len = u16::from_le_bytes([data[pos + 2], data[pos + 3]]) as usize;
        if block_len < 4 || pos + block_len > data.len() {
            break;
        }
        let block = &data[pos + 4..pos + block_len];
        match block_type {
            0x0C01 => {}
            0x0C02 => {
                if block.len() >= 8 {
                    let mut p = 0;
                    server_data.encryption_method = read_u32_le(block, &mut p);
                    let _level = read_u32_le(block, &mut p);
                    if server_data.encryption_method != 0 && p + 8 <= block.len() {
                        let random_len = read_u32_le(block, &mut p) as usize;
                        let cert_len = read_u32_le(block, &mut p) as usize;
                        if p + random_len <= block.len() {
                            server_data.server_random = block[p..p + random_len].to_vec();
                            p += random_len;
                        }
                        if cert_len > 0 && p + cert_len <= block.len() {
                            server_data.server_certificate = block[p..p + cert_len].to_vec();
                        }
                    }
                }
            }
            0x0C03 => {
                // TS_UD_SC_NET: MCSChannelId(2) + pad(2) + channelIdArray (remaining / 2 each)
                if block.len() >= 4 {
                    let mut p = 0;
                    server_data.io_channel = read_u16_le(block, &mut p);
                    let _pad = read_u16_le(block, &mut p);
                    while p + 2 <= block.len() {
                        server_data.channels.push(read_u16_le(block, &mut p));
                    }
                }
            }
            0x0C04 => {
                // SC_MCS_MSGCHANNEL: server-assigned message channel id
                if block.len() >= 2 {
                    let ch = u16::from_le_bytes([block[0], block[1]]);
                    server_data.msg_channel = Some(ch);
                }
            }
            _ => {}
        }
        pos += block_len;
    }

    Ok(server_data)
}
