// Ported from FFmpeg libavcodec/get_bits.h, unary.h and libavutil/common.h
// (commit 2da55bf), LGPL-2.1-or-later.
// Copyright (c) 2004 Michael Niedermayer, Fabrice Bellard, Konstantin Shishkov.

use crate::mpc8_data::{MPC8_CNK, MPC8_CNK_LEN, MPC8_CNK_LOST};
use crate::vlc::VlcElem;


#[derive(Clone)]
pub struct BitReader<'a> {
    buf: &'a [u8],
    index: u32,
    size_in_bits: u32,
    size_in_bits_plus8: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(buf: &'a [u8], size: usize) -> Self {
        let size = size.min((i32::MAX as usize - 1024) / 8);
        let size_in_bits = (size * 8) as u32;
        Self { buf, index: 0, size_in_bits, size_in_bits_plus8: size_in_bits + 8 }
    }

    #[inline]
    fn byte(&self, i: usize) -> u32 {
        self.buf.get(i).copied().unwrap_or(0) as u32
    }

    #[inline]
    pub fn cache32(&self) -> u32 {
        let p = (self.index >> 3) as usize;
        let v = (self.byte(p) << 24) | (self.byte(p + 1) << 16) | (self.byte(p + 2) << 8) | self.byte(p + 3);
        v << (self.index & 7)
    }

    #[inline]
    pub fn show_bits(&self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        debug_assert!(n <= 25);
        self.cache32() >> (32 - n)
    }

    #[inline]
    pub fn skip_bits(&mut self, n: u32) {
        self.index = self.size_in_bits_plus8.min(self.index.wrapping_add(n));
    }

    #[inline]
    pub fn skip_bits_signed(&mut self, n: i32) {
        self.index = self.size_in_bits_plus8.min(self.index.wrapping_add(n as u32));
    }

    #[inline]
    pub fn get_bits(&mut self, n: u32) -> u32 {
        let v = self.show_bits(n);
        self.skip_bits(n);
        v
    }

    #[inline]
    pub fn get_bits1(&mut self) -> u32 {
        let b = ((self.byte((self.index >> 3) as usize) << (self.index & 7)) & 0xFF) >> 7;
        if self.index < self.size_in_bits_plus8 {
            self.index += 1;
        }
        b
    }

    pub fn get_bits_long(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        if n <= 25 {
            return self.get_bits(n);
        }
        let p = (self.index >> 3) as usize;
        let mut v: u64 = 0;
        for k in 0..8 {
            v = (v << 8) | self.byte(p + k) as u64;
        }
        let v = ((v << (self.index & 7)) >> 32) as u32;
        self.skip_bits(n);
        v >> (32 - n)
    }

    #[inline]
    pub fn bits_count(&self) -> u32 {
        self.index
    }

    #[inline]
    pub fn bits_left(&self) -> i32 {
        self.size_in_bits as i32 - self.index as i32
    }
    #[inline]
    pub fn size_in_bits(&self) -> u32 {
        self.size_in_bits
    }


    #[inline]
    pub fn get_unary(&mut self, stop: u32, max_len: u32) -> u32 {
        let mut i = 0;
        while i < max_len && self.get_bits1() != stop {
            i += 1;
        }
        i
    }

    #[inline]
    pub fn get_vlc2(&mut self, table: &[VlcElem], bits: u32, max_depth: u32) -> i32 {
        let idx = self.show_bits(bits) as usize;
        let e = table.get(idx).copied().unwrap_or(VlcElem { sym: -1, len: 0 });
        let mut code = e.sym as i32;
        let mut n = e.len as i32;
        if max_depth > 1 && n < 0 {
            self.skip_bits(bits);
            let nb_bits = (-n) as u32;
            let sub_idx = (self.show_bits(nb_bits) as i32 + code) as usize;
            let e = table.get(sub_idx).copied().unwrap_or(VlcElem { sym: -1, len: 0 });
            code = e.sym as i32;
            n = e.len as i32;
            if max_depth > 2 && n < 0 {
                self.skip_bits(nb_bits);
                let nb_bits = (-n) as u32;
                let sub_idx = (self.show_bits(nb_bits) as i32 + code) as usize;
                let e = table.get(sub_idx).copied().unwrap_or(VlcElem { sym: -1, len: 0 });
                code = e.sym as i32;
                n = e.len as i32;
            }
        }
        self.skip_bits_signed(n);
        code
    }

    pub fn mpc8_dec_base(&mut self, k: usize, n: usize) -> u32 {
        if k == 0 || k > 16 || n == 0 || n > 33 {
            return 0;
        }
        let raw_len = MPC8_CNK_LEN[k - 1][n - 1];
        if raw_len == 0 {
            return 0;
        }
        let len = (raw_len - 1) as u32;
        let mut code = if len > 0 { self.get_bits_long(len) } else { 0 };
        let lost = MPC8_CNK_LOST[k - 1][n - 1];
        if code >= lost {
            code = ((code << 1) | self.get_bits1()) - lost;
        }
        code
    }

    pub fn mpc8_dec_enum(&mut self, mut k: usize, mut n: usize) -> u32 {
        let mut bits = 0u32;
        let mut code = self.mpc8_dec_base(k, n);
        while k > 0 && n > 0 && k <= 16 && n <= 32 {
            n -= 1;
            let c = MPC8_CNK[k - 1][n];
            if code >= c {
                bits |= 1u32 << n;
                code -= c;
                k -= 1;
            }
        }
        bits
    }

    pub fn mpc8_get_mod_golomb(&mut self, m: usize) -> u32 {
        if m >= 33 || MPC8_CNK_LEN[0][m] < 1 {
            return 0;
        }
        self.mpc8_dec_base(1, m + 1)
    }

    pub fn mpc8_get_mask(&mut self, size: usize, t: usize) -> u32 {
        let mut mask = 0u32;
        if t != 0 && t != size {
            mask = self.mpc8_dec_enum(t.min(size.saturating_sub(t)), size);
        }
        if (t << 1) > size {
            mask = !mask;
        }
        mask
    }

    pub fn gb_get_v(&mut self) -> u64 {
        let mut v = 0u64;
        let mut bits = 0;
        while self.get_bits1() != 0 && bits < 64 - 7 {
            v <<= 7;
            v |= self.get_bits(7) as u64;
            bits += 7;
        }
        v <<= 7;
        v |= self.get_bits(7) as u64;
        v
    }
}

#[inline]
pub fn sign_extend(val: i32, bits: u32) -> i32 {
    let shift = 32 - bits;
    (val << shift) >> shift
}

pub fn bswap_buf(dst: &mut [u8], src: &[u8]) {
    let words = (src.len() / 4).min(dst.len() / 4);
    for i in 0..words {
        let chunk = &src[i * 4..i * 4 + 4];
        let swapped = [chunk[3], chunk[2], chunk[1], chunk[0]];
        dst[i * 4..i * 4 + 4].copy_from_slice(&swapped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mpc8_dec_base_n33() {
        // n == 33 corresponds to maxbands == 31: mpc8_get_mod_golomb(32) -> mpc8_dec_base(1, 33)
        // MPC8_CNK_LEN[0][32] is 6.
        let data = [0b1010_1010, 0b1111_0000];
        let mut gb = BitReader::new(&data, data.len());
        let code = gb.mpc8_dec_base(1, 33);
        // With the fix, bits are consumed from the stream.
        assert!(gb.bits_count() > 0, "bits must be consumed for n == 33");
        assert!(code > 0, "code must be decoded for n == 33");

        let mut gb2 = BitReader::new(&data, data.len());
        let golomb = gb2.mpc8_get_mod_golomb(32);
        assert_eq!(code, golomb);
    }
}
