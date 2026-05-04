use crate::error::RdpError;

const TAG_BOOLEAN: u8 = 0x01;
const TAG_INTEGER: u8 = 0x02;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_ENUMERATED: u8 = 0x0A;
const TAG_SEQUENCE: u8 = 0x30;

pub fn encode_length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else if len < 0x100 {
        vec![0x81, len as u8]
    } else {
        vec![0x82, (len >> 8) as u8, (len & 0xFF) as u8]
    }
}

pub fn encode_tag_len(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut buf = vec![tag];
    buf.extend_from_slice(&encode_length(content.len()));
    buf.extend_from_slice(content);
    buf
}

pub fn encode_bool(v: bool) -> Vec<u8> {
    encode_tag_len(TAG_BOOLEAN, &[if v { 0xFF } else { 0x00 }])
}

pub fn encode_integer(v: i32) -> Vec<u8> {
    let bytes = if v == 0 {
        vec![0x00]
    } else if v > 0 && v < 0x80 {
        vec![v as u8]
    } else if v > 0 && v < 0x8000 {
        vec![(v >> 8) as u8, (v & 0xFF) as u8]
    } else if v > 0 && v < 0x800000 {
        vec![(v >> 16) as u8, (v >> 8) as u8, (v & 0xFF) as u8]
    } else {
        vec![
            (v >> 24) as u8,
            (v >> 16) as u8,
            (v >> 8) as u8,
            (v & 0xFF) as u8,
        ]
    };
    encode_tag_len(TAG_INTEGER, &bytes)
}

pub fn encode_octet_string(data: &[u8]) -> Vec<u8> {
    encode_tag_len(TAG_OCTET_STRING, data)
}

pub fn encode_sequence(content: &[u8]) -> Vec<u8> {
    encode_tag_len(TAG_SEQUENCE, content)
}

pub fn encode_constructed(tag: u8, content: &[u8]) -> Vec<u8> {
    encode_tag_len(tag, content)
}

pub fn decode_length(data: &[u8], pos: &mut usize) -> Result<usize, RdpError> {
    if *pos >= data.len() {
        return Err(RdpError::Protocol("BER: unexpected end of data".into()));
    }
    let b = data[*pos];
    *pos += 1;
    if b & 0x80 == 0 {
        Ok(b as usize)
    } else {
        let n = (b & 0x7F) as usize;
        if *pos + n > data.len() {
            return Err(RdpError::Protocol("BER: length overflow".into()));
        }
        let mut len = 0usize;
        for _ in 0..n {
            len = (len << 8) | data[*pos] as usize;
            *pos += 1;
        }
        Ok(len)
    }
}

pub fn decode_bool(data: &[u8], pos: &mut usize) -> Result<bool, RdpError> {
    if data[*pos] != TAG_BOOLEAN {
        return Err(RdpError::Protocol("BER: expected BOOLEAN tag".into()));
    }
    *pos += 1;
    let len = decode_length(data, pos)?;
    if len < 1 {
        return Err(RdpError::Protocol("BER: BOOLEAN length 0".into()));
    }
    let v = data[*pos] != 0;
    *pos += len;
    Ok(v)
}

pub fn decode_integer(data: &[u8], pos: &mut usize) -> Result<i32, RdpError> {
    // Accept both INTEGER (0x02) and ENUMERATED (0x0A) — both encode the same way
    if data[*pos] != TAG_INTEGER && data[*pos] != TAG_ENUMERATED {
        return Err(RdpError::Protocol(format!(
            "BER: expected INTERGER tag, got {:02x}",
            data[*pos]
        )));
    }
    *pos += 1;
    let len = decode_length(data, pos)?;
    let mut v = 0i32;
    for i in 0..len {
        v = (v << 8) | data[*pos + i] as i32;
    }
    *pos += len;
    Ok(v)
}

pub fn decode_octet_string(data: &[u8], pos: &mut usize) -> Result<Vec<u8>, RdpError> {
    if data[*pos] != TAG_OCTET_STRING {
        return Err(RdpError::Protocol(format!(
            "BER: expected OCTET STRING tag, got {:02x}",
            data[*pos]
        )));
    }
    *pos += 1;
    let len = decode_length(data, pos)?;
    let v = data[*pos..*pos + len].to_vec();
    *pos += len;
    Ok(v)
}
