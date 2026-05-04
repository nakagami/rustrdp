/// ZGFX (RDP8 Bulk Compression) decompressor.
/// Ported from /Users/nakagami/grdp/plugin/rdpgfx/zgfx.go

const HISTORY_SIZE: usize = 2_500_000;

// Token types
const TOKEN_LITERAL: u8 = 0;
const TOKEN_MATCH: u8 = 1;

#[derive(Copy, Clone, Default)]
struct TokenLutEntry {
    prefix_len: u8,
    value_bits: u8,
    token_type: u8,
    value_base: u32,
}

struct ZgfxTokenDef {
    prefix_len: u8,
    prefix_code: u16,
    value_bits: u8,
    token_type: u8,
    value_base: u32,
}

static TOKEN_TABLE: &[ZgfxTokenDef] = &[
    ZgfxTokenDef {
        prefix_len: 1,
        prefix_code: 0,
        value_bits: 8,
        token_type: TOKEN_LITERAL,
        value_base: 0,
    },
    ZgfxTokenDef {
        prefix_len: 5,
        prefix_code: 17,
        value_bits: 5,
        token_type: TOKEN_MATCH,
        value_base: 0,
    },
    ZgfxTokenDef {
        prefix_len: 5,
        prefix_code: 18,
        value_bits: 7,
        token_type: TOKEN_MATCH,
        value_base: 32,
    },
    ZgfxTokenDef {
        prefix_len: 5,
        prefix_code: 19,
        value_bits: 9,
        token_type: TOKEN_MATCH,
        value_base: 160,
    },
    ZgfxTokenDef {
        prefix_len: 5,
        prefix_code: 20,
        value_bits: 10,
        token_type: TOKEN_MATCH,
        value_base: 672,
    },
    ZgfxTokenDef {
        prefix_len: 5,
        prefix_code: 21,
        value_bits: 12,
        token_type: TOKEN_MATCH,
        value_base: 1696,
    },
    ZgfxTokenDef {
        prefix_len: 5,
        prefix_code: 24,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x00,
    },
    ZgfxTokenDef {
        prefix_len: 5,
        prefix_code: 25,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x01,
    },
    ZgfxTokenDef {
        prefix_len: 6,
        prefix_code: 44,
        value_bits: 14,
        token_type: TOKEN_MATCH,
        value_base: 5792,
    },
    ZgfxTokenDef {
        prefix_len: 6,
        prefix_code: 45,
        value_bits: 15,
        token_type: TOKEN_MATCH,
        value_base: 22176,
    },
    ZgfxTokenDef {
        prefix_len: 6,
        prefix_code: 52,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x02,
    },
    ZgfxTokenDef {
        prefix_len: 6,
        prefix_code: 53,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x03,
    },
    ZgfxTokenDef {
        prefix_len: 6,
        prefix_code: 54,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0xFF,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 92,
        value_bits: 18,
        token_type: TOKEN_MATCH,
        value_base: 54944,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 93,
        value_bits: 20,
        token_type: TOKEN_MATCH,
        value_base: 317088,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 110,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x04,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 111,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x05,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 112,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x06,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 113,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x07,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 114,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x08,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 115,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x09,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 116,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x0A,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 117,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x0B,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 118,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x3A,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 119,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x3B,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 120,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x3C,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 121,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x3D,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 122,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x3E,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 123,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x3F,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 124,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x40,
    },
    ZgfxTokenDef {
        prefix_len: 7,
        prefix_code: 125,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x80,
    },
    ZgfxTokenDef {
        prefix_len: 8,
        prefix_code: 188,
        value_bits: 20,
        token_type: TOKEN_MATCH,
        value_base: 1365664,
    },
    ZgfxTokenDef {
        prefix_len: 8,
        prefix_code: 189,
        value_bits: 21,
        token_type: TOKEN_MATCH,
        value_base: 2414240,
    },
    ZgfxTokenDef {
        prefix_len: 8,
        prefix_code: 252,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x0C,
    },
    ZgfxTokenDef {
        prefix_len: 8,
        prefix_code: 253,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x38,
    },
    ZgfxTokenDef {
        prefix_len: 8,
        prefix_code: 254,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x39,
    },
    ZgfxTokenDef {
        prefix_len: 8,
        prefix_code: 255,
        value_bits: 0,
        token_type: TOKEN_LITERAL,
        value_base: 0x66,
    },
    ZgfxTokenDef {
        prefix_len: 9,
        prefix_code: 380,
        value_bits: 22,
        token_type: TOKEN_MATCH,
        value_base: 4511392,
    },
    ZgfxTokenDef {
        prefix_len: 9,
        prefix_code: 381,
        value_bits: 23,
        token_type: TOKEN_MATCH,
        value_base: 8705696,
    },
    ZgfxTokenDef {
        prefix_len: 9,
        prefix_code: 382,
        value_bits: 24,
        token_type: TOKEN_MATCH,
        value_base: 17094304,
    },
];

fn build_lut() -> Box<[TokenLutEntry; 512]> {
    let mut lut = Box::new([TokenLutEntry::default(); 512]);
    for t in TOKEN_TABLE {
        let shift = 9u32 - t.prefix_len as u32;
        let base = (t.prefix_code as u32) << shift;
        let span = 1u32 << shift;
        for j in 0..span {
            let idx = (base | j) as usize;
            if idx < 512 && lut[idx].prefix_len == 0 {
                lut[idx] = TokenLutEntry {
                    prefix_len: t.prefix_len,
                    value_bits: t.value_bits,
                    token_type: t.token_type,
                    value_base: t.value_base,
                };
            }
        }
    }
    lut
}

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8, // bits remaining in current byte (8..1)
    bits_remaining: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        if data.len() < 2 {
            return BitReader {
                data: &[],
                byte_pos: 0,
                bit_pos: 8,
                bits_remaining: 0,
            };
        }
        let padding_bits = data[data.len() - 1] as u32;
        let total_bits = (data.len() as u32 - 1) * 8;
        let bits_remaining = if padding_bits > total_bits {
            0
        } else {
            total_bits - padding_bits
        };
        BitReader {
            data: &data[..data.len() - 1],
            byte_pos: 0,
            bit_pos: 8,
            bits_remaining,
        }
    }

    fn has_bits_remaining(&self) -> bool {
        self.bits_remaining > 0
    }

    /// Peek up to 9 bits MSB-first. Returns (val, avail) where avail is clamped to bits_remaining.
    fn peek9(&self) -> (u32, u8) {
        if self.byte_pos >= self.data.len() {
            return (0, 0);
        }
        let mask = (1u32 << self.bit_pos) - 1;
        let mut bits = self.data[self.byte_pos] as u32 & mask;
        let mut avail = self.bit_pos;
        let mut idx = self.byte_pos + 1;
        while avail < 9 && idx < self.data.len() {
            bits = (bits << 8) | self.data[idx] as u32;
            avail += 8;
            idx += 1;
        }
        let val = if avail >= 9 {
            let v = (bits >> (avail as u32 - 9)) & 0x1FF;
            avail = 9;
            v
        } else if avail > 0 {
            bits << (9 - avail)
        } else {
            0
        };
        if self.bits_remaining < avail as u32 {
            avail = self.bits_remaining as u8;
        }
        (val, avail)
    }

    fn consume_bits(&mut self, n: u8) {
        let n = (n as u32).min(self.bits_remaining) as u8;
        self.bits_remaining -= n as u32;
        let mut n = n;
        while n >= self.bit_pos {
            n -= self.bit_pos;
            self.byte_pos += 1;
            self.bit_pos = 8;
        }
        self.bit_pos -= n;
        if self.bit_pos == 0 {
            self.byte_pos += 1;
            self.bit_pos = 8;
        }
    }

    fn get_bit(&mut self) -> u32 {
        if self.byte_pos >= self.data.len() {
            return 0;
        }
        self.bit_pos -= 1;
        let bit = ((self.data[self.byte_pos] >> self.bit_pos) & 1) as u32;
        if self.bit_pos == 0 {
            self.byte_pos += 1;
            self.bit_pos = 8;
        }
        bit
    }

    fn get_bits(&mut self, n: u8) -> u32 {
        if n == 0 {
            return 0;
        }
        // Fast path: enough bits in current byte
        if n <= self.bit_pos && self.byte_pos < self.data.len() {
            self.bit_pos -= n;
            let v = (self.data[self.byte_pos] as u32 >> self.bit_pos) & ((1u32 << n) - 1);
            if self.bit_pos == 0 {
                self.byte_pos += 1;
                self.bit_pos = 8;
            }
            return v;
        }
        let mut result = 0u32;
        for _ in 0..n {
            result = (result << 1) | self.get_bit();
        }
        result
    }
}

pub struct ZgfxContext {
    history: Vec<u8>,
    history_idx: usize,
    lut: Box<[TokenLutEntry; 512]>,
}

impl ZgfxContext {
    pub fn new() -> Self {
        ZgfxContext {
            history: vec![0u8; HISTORY_SIZE],
            history_idx: 0,
            lut: build_lut(),
        }
    }

    pub fn history_write(&mut self, data: &[u8]) {
        let n = data.len();
        if n == 0 {
            return;
        }
        if n >= HISTORY_SIZE {
            let start = n - HISTORY_SIZE;
            self.history.copy_from_slice(&data[start..]);
            self.history_idx = 0;
            return;
        }
        let end = self.history_idx + n;
        if end <= HISTORY_SIZE {
            self.history[self.history_idx..end].copy_from_slice(data);
            self.history_idx = end;
            if self.history_idx == HISTORY_SIZE {
                self.history_idx = 0;
            }
        } else {
            let first = HISTORY_SIZE - self.history_idx;
            self.history[self.history_idx..].copy_from_slice(&data[..first]);
            self.history[..n - first].copy_from_slice(&data[first..]);
            self.history_idx = n - first;
        }
    }

    fn output_literal(&mut self, b: u8, out: &mut Vec<u8>) {
        self.history[self.history_idx] = b;
        self.history_idx += 1;
        if self.history_idx == HISTORY_SIZE {
            self.history_idx = 0;
        }
        out.push(b);
    }

    fn output_match(&mut self, distance: usize, count: usize, out: &mut Vec<u8>) {
        if distance == 0 || count == 0 {
            return;
        }
        let base = out.len();
        out.resize(base + count, 0);

        let src_idx = if self.history_idx >= distance {
            self.history_idx - distance
        } else {
            HISTORY_SIZE + self.history_idx - distance
        };

        if count <= distance {
            let end = src_idx + count;
            if end <= HISTORY_SIZE {
                out[base..base + count].copy_from_slice(&self.history[src_idx..end]);
            } else {
                let first = HISTORY_SIZE - src_idx;
                out[base..base + first].copy_from_slice(&self.history[src_idx..]);
                out[base + first..base + count].copy_from_slice(&self.history[..count - first]);
            }
        } else {
            let end = src_idx + distance;
            if end <= HISTORY_SIZE {
                out[base..base + distance].copy_from_slice(&self.history[src_idx..end]);
            } else {
                let first = HISTORY_SIZE - src_idx;
                out[base..base + first].copy_from_slice(&self.history[src_idx..]);
                out[base + first..base + distance]
                    .copy_from_slice(&self.history[..distance - first]);
            }
            for i in distance..count {
                out[base + i] = out[base + i - distance];
            }
        }

        self.history_write(&out[base..base + count].to_vec());
    }

    fn decode_match_count(&self, br: &mut BitReader) -> usize {
        let bit = br.get_bit();
        br.bits_remaining = br.bits_remaining.saturating_sub(1);
        if bit == 0 {
            return 3;
        }

        let mut count: usize = 4;
        let mut extra: u8 = 2;

        let mut bit = br.get_bit();
        br.bits_remaining = br.bits_remaining.saturating_sub(1);
        while bit == 1 {
            count <<= 1;
            extra += 1;
            bit = br.get_bit();
            br.bits_remaining = br.bits_remaining.saturating_sub(1);
        }

        if br.bits_remaining < extra as u32 {
            return count;
        }
        count += br.get_bits(extra) as usize;
        br.bits_remaining = br.bits_remaining.saturating_sub(extra as u32);
        count
    }

    fn decompress_raw(&mut self, data: &[u8]) -> Vec<u8> {
        if data.len() < 2 {
            return vec![];
        }
        let mut br = BitReader::new(data);
        let mut out = Vec::with_capacity(data.len() * 3);

        while br.has_bits_remaining() {
            let (val, avail) = br.peek9();
            if avail == 0 {
                break;
            }

            let e = self.lut[val as usize];
            if e.prefix_len == 0 || avail < e.prefix_len {
                break;
            }

            br.consume_bits(e.prefix_len);

            if e.token_type == TOKEN_LITERAL {
                if br.bits_remaining < e.value_bits as u32 {
                    break;
                }
                let value = e.value_base + br.get_bits(e.value_bits);
                br.bits_remaining = br.bits_remaining.saturating_sub(e.value_bits as u32);
                self.output_literal(value as u8, &mut out);
            } else {
                if br.bits_remaining < e.value_bits as u32 {
                    break;
                }
                let distance = (e.value_base + br.get_bits(e.value_bits)) as usize;
                br.bits_remaining = br.bits_remaining.saturating_sub(e.value_bits as u32);

                if distance != 0 {
                    let count = self.decode_match_count(&mut br);
                    self.output_match(distance, count, &mut out);
                } else {
                    // Raw unencoded block
                    if br.bits_remaining < 15 {
                        break;
                    }
                    let raw_count = br.get_bits(15) as usize;
                    br.bits_remaining = br.bits_remaining.saturating_sub(15);
                    // Byte-align
                    if br.bit_pos < 8 {
                        br.bits_remaining = br.bits_remaining.saturating_sub(br.bit_pos as u32);
                        br.byte_pos += 1;
                        br.bit_pos = 8;
                    }
                    if br.byte_pos + raw_count > br.data.len() {
                        break;
                    }
                    if (raw_count as u32) * 8 > br.bits_remaining {
                        break;
                    }
                    let raw = br.data[br.byte_pos..br.byte_pos + raw_count].to_vec();
                    br.byte_pos += raw_count;
                    br.bits_remaining = br.bits_remaining.saturating_sub(raw_count as u32 * 8);
                    self.history_write(&raw);
                    out.extend_from_slice(&raw);
                }
            }
        }

        out
    }

    fn decompress_segment(&mut self, seg: &[u8]) -> Vec<u8> {
        if seg.is_empty() {
            return vec![];
        }
        let header = seg[0];
        let payload = &seg[1..];
        if header & 0x20 != 0 {
            self.decompress_raw(payload)
        } else {
            self.history_write(payload);
            payload.to_vec()
        }
    }

    fn decompress_multipart(&mut self, data: &[u8]) -> Vec<u8> {
        if data.len() < 6 {
            return vec![];
        }
        let seg_count = u16::from_le_bytes([data[0], data[1]]) as usize;
        let _uncomp_size = u32::from_le_bytes([data[2], data[3], data[4], data[5]]);
        let mut offset = 6;
        let mut result = Vec::new();
        for _ in 0..seg_count {
            if offset + 4 > data.len() {
                break;
            }
            let seg_size = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;
            if offset + seg_size > data.len() {
                break;
            }
            let decompressed = self.decompress_segment(&data[offset..offset + seg_size]);
            offset += seg_size;
            result.extend_from_slice(&decompressed);
        }
        result
    }

    /// Decompress a full ZGFX payload (including the descriptor byte).
    /// Returns decompressed bytes.
    pub fn decompress(&mut self, data: &[u8]) -> Vec<u8> {
        if data.is_empty() {
            return vec![];
        }
        match data[0] {
            0xE0 => {
                if data.len() < 2 {
                    return vec![];
                }
                self.decompress_segment(&data[1..])
            }
            0xE1 => {
                if data.len() < 7 {
                    return vec![];
                }
                self.decompress_multipart(&data[1..])
            }
            _ => {
                log::warn!(
                    "[zgfx] unknown descriptor 0x{:02X}, passing through",
                    data[0]
                );
                data.to_vec()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_caps_confirm_decompress() {
        let input = [
            0xE0u8, 0x24, 0x09, 0xE3, 0x18, 0x0A, 0x44, 0x8C, 0xF1, 0xE9, 0x8D, 0xD1, 0x43, 0x4C,
            0x63, 0x00, 0x05,
        ];
        let mut ctx = ZgfxContext::new();
        let result = ctx.decompress(&input);
        assert_eq!(result[0], 0x13);
        assert_eq!(result[1], 0x00);
    }
}
