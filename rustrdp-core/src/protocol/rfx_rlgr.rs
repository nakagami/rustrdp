/// RLGR1/RLGR3 (Run-Length Golomb-Rice) decoder for RFX codec.
/// Reference: MS-RDPRFX 3.1.8.1.7.3

const RLGR_LSGR: usize = 3;
const RLGR_KP_MAX: u32 = 80;
const RLGR_UPGR: u32 = 4;
const RLGR_DNGR: u32 = 6;
const RLGR_UQGR: u32 = 3;
const RLGR_DQGR: u32 = 3;

pub struct RlgrBitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    acc: u64,
    bits_in_acc: usize,
}

impl<'a> RlgrBitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        RlgrBitReader {
            data,
            byte_pos: 0,
            acc: 0,
            bits_in_acc: 0,
        }
    }

    #[inline(always)]
    pub fn remaining(&self) -> usize {
        (self.data.len() - self.byte_pos) * 8 + self.bits_in_acc
    }

    #[inline(always)]
    fn fill(&mut self, need: usize) {
        while self.bits_in_acc < need && self.byte_pos < self.data.len() {
            self.acc |= (self.data[self.byte_pos] as u64) << (56 - self.bits_in_acc);
            self.byte_pos += 1;
            self.bits_in_acc += 8;
        }
    }

    #[inline(always)]
    pub fn read_bits(&mut self, n: usize) -> u32 {
        if self.bits_in_acc < n {
            self.fill(n);
            if self.bits_in_acc < n {
                self.bits_in_acc = 0;
                return 0;
            }
        }
        let val = (self.acc >> (64 - n)) as u32;
        self.acc <<= n;
        self.bits_in_acc -= n;
        val
    }

    #[inline(always)]
    pub fn count_leading_zeros(&mut self) -> u32 {
        let mut count = 0u32;
        loop {
            if self.bits_in_acc < 56 && self.byte_pos < self.data.len() {
                self.fill(56);
            }
            if self.bits_in_acc == 0 {
                return count;
            }
            let lz = self.acc.leading_zeros() as usize;
            if lz >= self.bits_in_acc {
                count += self.bits_in_acc as u32;
                self.acc = 0;
                self.bits_in_acc = 0;
                continue;
            }
            count += lz as u32;
            let consume = lz + 1;
            self.acc <<= consume;
            self.bits_in_acc -= consume;
            return count;
        }
    }

    #[inline(always)]
    pub fn count_leading_ones(&mut self) -> u32 {
        let mut count = 0u32;
        loop {
            if self.bits_in_acc < 56 && self.byte_pos < self.data.len() {
                self.fill(56);
            }
            if self.bits_in_acc == 0 {
                return count;
            }
            let lo = (!self.acc).leading_zeros() as usize;
            if lo >= self.bits_in_acc {
                count += self.bits_in_acc as u32;
                self.acc = 0;
                self.bits_in_acc = 0;
                continue;
            }
            count += lo as u32;
            let consume = lo + 1;
            self.acc <<= consume;
            self.bits_in_acc -= consume;
            return count;
        }
    }
}

pub fn rlgr1_decode(data: &[u8], output: &mut [i16]) {
    output.fill(0);
    let mut br = RlgrBitReader::new(data);
    let mut cnt = 0usize;
    let output_size = output.len();

    let mut k = 1u32;
    let mut kp = 1u32 << RLGR_LSGR;
    let mut kr = 1u32;
    let mut krp = 1u32 << RLGR_LSGR;

    while br.remaining() > 0 && cnt < output_size {
        if k > 0 {
            let vk = br.count_leading_zeros();
            let mut run = 0u32;
            for _ in 0..vk {
                run += 1 << k;
                kp += RLGR_UPGR;
                if kp > RLGR_KP_MAX {
                    kp = RLGR_KP_MAX;
                }
                k = kp >> RLGR_LSGR;
            }

            if k > 0 {
                run += br.read_bits(k as usize);
            }

            let sign = br.read_bits(1);
            let vk2 = br.count_leading_ones();

            let mut code = 0u32;
            if kr > 0 {
                code = br.read_bits(kr as usize);
            }
            code |= vk2 << kr;

            if vk2 == 0 {
                krp = krp.saturating_sub(2);
                kr = krp >> RLGR_LSGR;
            } else if vk2 != 1 {
                krp += vk2;
                if krp > RLGR_KP_MAX {
                    krp = RLGR_KP_MAX;
                }
                kr = krp >> RLGR_LSGR;
            }

            kp = kp.saturating_sub(RLGR_DNGR);
            k = kp >> RLGR_LSGR;

            let mut mag = (code + 1) as i16;
            if sign != 0 {
                mag = -mag;
            }

            let run_end = (cnt + run as usize).min(output_size);
            cnt = run_end;
            if cnt < output_size {
                output[cnt] = mag;
                cnt += 1;
            }
        } else {
            let vk = br.count_leading_ones();
            let mut code = 0u32;
            if kr > 0 {
                code = br.read_bits(kr as usize);
            }
            code |= vk << kr;

            if vk == 0 {
                krp = krp.saturating_sub(2);
                kr = krp >> RLGR_LSGR;
            } else if vk != 1 {
                krp += vk;
                if krp > RLGR_KP_MAX {
                    krp = RLGR_KP_MAX;
                }
                kr = krp >> RLGR_LSGR;
            }

            if code == 0 {
                kp += RLGR_UQGR;
                if kp > RLGR_KP_MAX {
                    kp = RLGR_KP_MAX;
                }
                k = kp >> RLGR_LSGR;
                if cnt < output_size {
                    cnt += 1;
                }
            } else {
                kp = kp.saturating_sub(RLGR_DQGR);
                k = kp >> RLGR_LSGR;

                let mag = if (code & 1) != 0 {
                    -(((code + 1) >> 1) as i16)
                } else {
                    (code >> 1) as i16
                };
                if cnt < output_size {
                    output[cnt] = mag;
                    cnt += 1;
                }
            }
        }
    }
}

pub fn rlgr3_decode(data: &[u8], output: &mut [i16]) {
    output.fill(0);
    let mut br = RlgrBitReader::new(data);
    let mut cnt = 0usize;
    let output_size = output.len();

    let mut k = 1u32;
    let mut kp = 1u32 << RLGR_LSGR;
    let mut kr = 1u32;
    let mut krp = 1u32 << RLGR_LSGR;

    while br.remaining() > 0 && cnt < output_size {
        if k > 0 {
            let vk = br.count_leading_zeros();
            let mut run = 0u32;
            for _ in 0..vk {
                run += 1 << k;
                kp += RLGR_UPGR;
                if kp > RLGR_KP_MAX {
                    kp = RLGR_KP_MAX;
                }
                k = kp >> RLGR_LSGR;
            }

            if k > 0 {
                run += br.read_bits(k as usize);
            }

            let sign = br.read_bits(1);
            let vk2 = br.count_leading_ones();

            let mut code = 0u32;
            if kr > 0 {
                code = br.read_bits(kr as usize);
            }
            code |= vk2 << kr;

            if vk2 == 0 {
                krp = krp.saturating_sub(2);
                kr = krp >> RLGR_LSGR;
            } else if vk2 != 1 {
                krp += vk2;
                if krp > RLGR_KP_MAX {
                    krp = RLGR_KP_MAX;
                }
                kr = krp >> RLGR_LSGR;
            }

            kp = kp.saturating_sub(RLGR_DNGR);
            k = kp >> RLGR_LSGR;

            let mut mag = (code + 1) as i16;
            if sign != 0 {
                mag = -mag;
            }

            let run_end = (cnt + run as usize).min(output_size);
            cnt = run_end;
            if cnt < output_size {
                output[cnt] = mag;
                cnt += 1;
            }
        } else {
            let vk = br.count_leading_ones();
            let mut code = 0u32;
            if kr > 0 {
                code = br.read_bits(kr as usize);
            }
            code |= vk << kr;

            if vk == 0 {
                krp = krp.saturating_sub(2);
                kr = krp >> RLGR_LSGR;
            } else if vk != 1 {
                krp += vk;
                if krp > RLGR_KP_MAX {
                    krp = RLGR_KP_MAX;
                }
                kr = krp >> RLGR_LSGR;
            }

            let n_idx = if code != 0 {
                32 - code.leading_zeros()
            } else {
                0
            };

            if br.remaining() < n_idx as usize {
                break;
            }

            let val1 = if n_idx > 0 {
                br.read_bits(n_idx as usize)
            } else {
                0
            };
            let val2 = code.saturating_sub(val1);

            if val1 != 0 && val2 != 0 {
                kp = kp.saturating_sub(2 * RLGR_DQGR);
                k = kp >> RLGR_LSGR;
            } else if val1 == 0 && val2 == 0 {
                kp += 2 * RLGR_UQGR;
                if kp > RLGR_KP_MAX {
                    kp = RLGR_KP_MAX;
                }
                k = kp >> RLGR_LSGR;
            }

            let mag1 = if (val1 & 1) != 0 {
                -(((val1 + 1) >> 1) as i16)
            } else {
                (val1 >> 1) as i16
            };
            if cnt < output_size {
                output[cnt] = mag1;
                cnt += 1;
            }

            let mag2 = if (val2 & 1) != 0 {
                -(((val2 + 1) >> 1) as i16)
            } else {
                (val2 >> 1) as i16
            };
            if cnt < output_size {
                output[cnt] = mag2;
                cnt += 1;
            }
        }
    }
}
