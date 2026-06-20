use crate::core::io::*;

pub fn write_length(buf: &mut Vec<u8>, length: u16) {
    if length > 0x7F {
        write_u16_be(buf, length | 0x8000);
    } else {
        write_u8(buf, length as u8);
    }
}

pub fn write_choice(buf: &mut Vec<u8>, choice: u8) {
    write_u8(buf, choice);
}

pub fn write_selection(buf: &mut Vec<u8>, selection: u8) {
    write_u8(buf, selection);
}

pub fn write_number_of_sets(buf: &mut Vec<u8>, n: u8) {
    write_u8(buf, n);
}

pub fn write_padding(buf: &mut Vec<u8>, n: usize) {
    for _ in 0..n {
        write_u8(buf, 0);
    }
}

pub fn write_octet_string(buf: &mut Vec<u8>, data: &[u8], min: usize) {
    let len = data.len();
    if len >= min {
        write_length(buf, (len - min) as u16);
    } else {
        write_length(buf, 0);
    }
    write_bytes(buf, data);
}

pub fn write_enumerated(buf: &mut Vec<u8>, e: u8) {
    write_u8(buf, e);
}

pub fn write_integer_16(buf: &mut Vec<u8>, v: u16, min: u16) {
    write_u16_be(buf, v - min);
}

pub fn write_integer(buf: &mut Vec<u8>, v: u32) {
    write_u8(buf, 1);
    write_u8(buf, v as u8);
}

pub fn read_length(data: &[u8], pos: &mut usize) -> usize {
    let b = data[*pos] as usize;
    *pos += 1;
    if b & 0x80 != 0 {
        let lo = data[*pos] as usize;
        *pos += 1;
        ((b & 0x7F) << 8) | lo
    } else {
        b
    }
}

pub fn read_integer(data: &[u8], pos: &mut usize) -> u32 {
    let len = data[*pos] as usize;
    *pos += 1;
    let mut v = 0u32;
    for _ in 0..len {
        v = (v << 8) | data[*pos] as u32;
        *pos += 1;
    }
    v
}

pub fn read_integer_16(data: &[u8], pos: &mut usize, min: u16) -> u16 {
    let v = u16::from_be_bytes([data[*pos], data[*pos + 1]]);
    *pos += 2;
    v + min
}

pub fn read_enumerated(data: &[u8], pos: &mut usize) -> u8 {
    let v = data[*pos];
    *pos += 1;
    v
}

pub fn read_octet_string(data: &[u8], pos: &mut usize, min: usize) -> Vec<u8> {
    let len = read_length(data, pos) + min;
    let v = data[*pos..*pos + len].to_vec();
    *pos += len;
    v
}
