pub fn read_u8(data: &[u8], pos: &mut usize) -> u8 {
    let v = data.get(*pos).copied().unwrap_or(0);
    *pos += 1;
    v
}

pub fn read_u16_le(data: &[u8], pos: &mut usize) -> u16 {
    let v = data
        .get(*pos..*pos + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0);
    *pos += 2;
    v
}

pub fn read_u32_le(data: &[u8], pos: &mut usize) -> u32 {
    let v = data
        .get(*pos..*pos + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .unwrap_or(0);
    *pos += 4;
    v
}

pub fn read_u16_be(data: &[u8], pos: &mut usize) -> u16 {
    let v = data
        .get(*pos..*pos + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .unwrap_or(0);
    *pos += 2;
    v
}

pub fn read_u32_be(data: &[u8], pos: &mut usize) -> u32 {
    let v = data
        .get(*pos..*pos + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .unwrap_or(0);
    *pos += 4;
    v
}

pub fn read_bytes(data: &[u8], pos: &mut usize, n: usize) -> Vec<u8> {
    let v = data.get(*pos..*pos + n).unwrap_or(&[]).to_vec();
    *pos += n;
    v
}

pub fn write_u8(buf: &mut Vec<u8>, v: u8) {
    buf.push(v);
}

pub fn write_u16_le(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn write_u32_le(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn write_u16_be(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_be_bytes());
}

pub fn write_u32_be(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

pub fn write_bytes(buf: &mut Vec<u8>, data: &[u8]) {
    buf.extend_from_slice(data);
}
