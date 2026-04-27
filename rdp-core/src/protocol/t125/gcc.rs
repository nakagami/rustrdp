use crate::error::RdpError;
use crate::core::io::*;

pub struct Channel {
    pub name: String,
    pub options: u32,
}

pub struct ClientData {
    pub width: u16,
    pub height: u16,
    pub kbd_layout: u32,
    pub color_depth: u16,
    pub channels: Vec<Channel>,
    pub server_selected_protocol: u32,
}

pub struct ServerData {
    pub io_channel: u16,
    pub encryption_method: u32,
    pub server_random: Vec<u8>,
    pub server_certificate: Vec<u8>,
    pub channels: Vec<u16>,
}

impl Default for ClientData {
    fn default() -> Self {
        ClientData {
            width: 1280,
            height: 720,
            kbd_layout: 0x0409,
            color_depth: 0xca01,
            channels: vec![],
            server_selected_protocol: 0,
        }
    }
}

pub fn create_gcc_data(client_data: &ClientData) -> Vec<u8> {
    let user_data = build_user_data(client_data);

    let mut conference = Vec::new();
    conference.extend_from_slice(&[0x00, 0x08, 0x00, 0x10, 0x00, 0x01, 0xc0, 0x00]);
    let ud_len = user_data.len() + 4;
    if ud_len > 0x7F {
        conference.push(0x81);
        conference.push(ud_len as u8);
    } else {
        conference.push(ud_len as u8);
    }
    conference.extend_from_slice(&[0x44, 0x75, 0x63, 0x61]); // "Duca"
    conference.extend_from_slice(&user_data);

    let mut data = Vec::new();
    data.extend_from_slice(&[0x00, 0x05, 0x00, 0x14, 0x7c, 0x00, 0x01]);
    let total = conference.len();
    if total > 0x7F {
        data.push(0x81);
        data.push(total as u8);
    } else {
        data.push(total as u8);
    }
    data.extend_from_slice(&conference);
    data
}

fn build_user_data(client_data: &ClientData) -> Vec<u8> {
    let mut data = Vec::new();

    let core = build_core_data(client_data);
    write_u16_le(&mut data, 0xC001);
    write_u16_le(&mut data, (core.len() + 4) as u16);
    data.extend_from_slice(&core);

    let sec = build_security_data();
    write_u16_le(&mut data, 0xC002);
    write_u16_le(&mut data, (sec.len() + 4) as u16);
    data.extend_from_slice(&sec);

    if !client_data.channels.is_empty() {
        let net = build_network_data(&client_data.channels);
        write_u16_le(&mut data, 0xC003);
        write_u16_le(&mut data, (net.len() + 4) as u16);
        data.extend_from_slice(&net);
    }

    data
}

fn build_core_data(cd: &ClientData) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u32_le(&mut buf, 0x00080004);
    write_u16_le(&mut buf, cd.width);
    write_u16_le(&mut buf, cd.height);
    write_u16_le(&mut buf, 0xca01);
    write_u16_le(&mut buf, 0xAA03);
    write_u32_le(&mut buf, cd.kbd_layout);
    write_u32_le(&mut buf, 2600);
    let name = "rdpwasm";
    let mut name_utf16: Vec<u8> = name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
    name_utf16.resize(32, 0);
    buf.extend_from_slice(&name_utf16);
    write_u32_le(&mut buf, 0x04);
    write_u32_le(&mut buf, 0x00);
    write_u32_le(&mut buf, 12);
    buf.extend_from_slice(&[0u8; 64]);
    write_u16_le(&mut buf, 0xca01);
    write_u16_le(&mut buf, 1);
    write_u32_le(&mut buf, 0);
    write_u16_le(&mut buf, 24);
    write_u16_le(&mut buf, 7);
    write_u16_le(&mut buf, 1);
    buf.extend_from_slice(&[0u8; 64]);
    write_u8(&mut buf, 0);
    write_u8(&mut buf, 0);
    write_u32_le(&mut buf, cd.server_selected_protocol);
    buf
}

fn build_security_data() -> Vec<u8> {
    let mut buf = Vec::new();
    write_u32_le(&mut buf, 3);
    write_u32_le(&mut buf, 0);
    buf
}

fn build_network_data(channels: &[Channel]) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u32_le(&mut buf, channels.len() as u32);
    for ch in channels {
        let mut name_bytes = [0u8; 8];
        let name = ch.name.as_bytes();
        let n = name.len().min(8);
        name_bytes[..n].copy_from_slice(&name[..n]);
        buf.extend_from_slice(&name_bytes);
        write_u32_le(&mut buf, ch.options);
    }
    buf
}

pub fn parse_gcc_response(data: &[u8]) -> Result<ServerData, RdpError> {
    let mut server_data = ServerData {
        io_channel: 1003,
        encryption_method: 0,
        server_random: Vec::new(),
        server_certificate: Vec::new(),
        channels: Vec::new(),
    };

    let mut pos = 0;
    while pos + 4 <= data.len() {
        let block_type = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if block_type == 0x0C01 || block_type == 0x0C02 || block_type == 0x0C03 {
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
                    let _encryption_level = read_u32_le(block, &mut p);
                    if server_data.encryption_method != 0 && p + 8 <= block.len() {
                        let random_len = read_u32_le(block, &mut p) as usize;
                        let cert_len = read_u32_le(block, &mut p) as usize;
                        if p + random_len <= block.len() {
                            server_data.server_random = block[p..p + random_len].to_vec();
                            p += random_len;
                        }
                        if p + cert_len <= block.len() {
                            server_data.server_certificate = block[p..p + cert_len].to_vec();
                        }
                    }
                }
            }
            0x0C03 => {
                if block.len() >= 4 {
                    let mut p = 0;
                    server_data.io_channel = read_u16_le(block, &mut p);
                    let _pad = read_u16_le(block, &mut p);
                    while p + 2 <= block.len() {
                        server_data.channels.push(read_u16_le(block, &mut p));
                        if p + 2 <= block.len() {
                            let _pad2 = read_u16_le(block, &mut p);
                        }
                    }
                }
            }
            _ => {}
        }
        pos += block_len;
    }

    Ok(server_data)
}
