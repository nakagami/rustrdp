pub mod caps;
pub mod input;

use crate::core::io::*;
use crate::error::RdpError;

pub const PDUTYPE_DEMANDACTIVEPDU: u16 = 0x1;
pub const PDUTYPE_CONFIRMACTIVEPDU: u16 = 0x3;
pub const PDUTYPE_DEACTIVATEALLPDU: u16 = 0x6;
pub const PDUTYPE_DATAPDU: u16 = 0x7;

pub const PDUTYPE2_UPDATE: u8 = 0x02;
pub const PDUTYPE2_CONTROL: u8 = 0x14;
pub const PDUTYPE2_SYNCHRONIZE: u8 = 0x1F;
pub const PDUTYPE2_INPUT: u8 = 0x1C;
pub const PDUTYPE2_FONTMAP: u8 = 0x28;
pub const PDUTYPE2_FONTLIST: u8 = 0x27;
pub const PDUTYPE2_SUPPRESS_OUTPUT: u8 = 0x23;
pub const PDUTYPE2_SHUTDOWN_REQUEST: u8 = 0x24;

pub const UPDATETYPE_BITMAP: u16 = 0x0001;

pub struct ShareControlHeader {
    pub total_length: u16,
    pub pdu_type: u16,
    pub pdu_source: u16,
}

pub struct ShareDataHeader {
    pub share_id: u32,
    pub stream_id: u8,
    pub uncompressed_length: u16,
    pub pdu_type2: u8,
    pub general_compression_type: u8,
    pub general_compression_flags: u16,
}

impl ShareControlHeader {
    pub fn parse(data: &[u8], pos: &mut usize) -> Result<Self, RdpError> {
        if *pos + 6 > data.len() {
            return Err(RdpError::Protocol("ShareControlHeader too short".into()));
        }
        Ok(ShareControlHeader {
            total_length: read_u16_le(data, pos),
            pdu_type: read_u16_le(data, pos) & 0x0F,
            pdu_source: read_u16_le(data, pos),
        })
    }

    pub fn build(pdu_type: u16, pdu_source: u16, payload_len: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        write_u16_le(&mut buf, (payload_len + 6) as u16);
        write_u16_le(&mut buf, pdu_type | 0x10);
        write_u16_le(&mut buf, pdu_source);
        buf
    }
}

impl ShareDataHeader {
    pub fn parse(data: &[u8], pos: &mut usize) -> Result<Self, RdpError> {
        if *pos + 12 > data.len() {
            return Err(RdpError::Protocol("ShareDataHeader too short".into()));
        }
        let share_id = read_u32_le(data, pos);
        let _pad = read_u8(data, pos);
        let stream_id = read_u8(data, pos);
        let uncompressed_length = read_u16_le(data, pos);
        let pdu_type2 = read_u8(data, pos);
        let general_compression_type = read_u8(data, pos);
        let general_compression_flags = read_u16_le(data, pos);
        Ok(ShareDataHeader {
            share_id,
            stream_id,
            uncompressed_length,
            pdu_type2,
            general_compression_type,
            general_compression_flags,
        })
    }

    pub fn build(share_id: u32, pdu_type2: u8, payload_len: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        write_u32_le(&mut buf, share_id);
        write_u8(&mut buf, 0);
        write_u8(&mut buf, 1);
        write_u16_le(&mut buf, (payload_len + 4) as u16);
        write_u8(&mut buf, pdu_type2);
        write_u8(&mut buf, 0);
        write_u16_le(&mut buf, 0);
        buf
    }
}
