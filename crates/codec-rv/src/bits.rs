//! MSB-first bit reader with FFmpeg's checked `GetBitContext` semantics,
//! interleaved exp-Golomb codes and small integer helpers.
//!
//! Ported from FFmpeg libavcodec/get_bits.h, golomb.h and libavutil/common.h
//! (commit 2da55bf); LGPL-2.1-or-later.
//!
//! The reader is given every byte from the start of its data to the end of
//! the packet but only `size_in_bits` of them count as "its" bits, exactly
//! like FFmpeg, whose caches read past `size_in_bits` into the rest of the
//! packet and then into zeroed padding. The position saturates at
//! `size_in_bits + 8`, as FFmpeg's checked reader does.

use crate::golomb_tables::{
    INTERLEAVED_DIRAC_GOLOMB_VLC_CODE, INTERLEAVED_GOLOMB_VLC_LEN, INTERLEAVED_SE_GOLOMB_VLC_CODE,
    INTERLEAVED_UE_GOLOMB_VLC_CODE,
};

/// FFmpeg's `INVALID_VLC`.
pub const INVALID_VLC: i32 = i32::MIN;

#[derive(Clone)]
pub struct BitReader<'a> {
    buf: &'a [u8],
    index: u32,
    size_in_bits: u32,
    size_in_bits_plus8: u32,
}

impl<'a> BitReader<'a> {
    /// `init_get_bits8(buf, size)`: the reader owns `size` bytes of `buf`;
    /// bytes of `buf` past `size` are what FFmpeg's cache would see there.
    pub fn new(buf: &'a [u8], size: usize) -> Self {
        // FFmpeg rejects bit sizes near INT_MAX; packets are far smaller.
        let size = size.min((i32::MAX as usize - 1024) / 8);
        let size_in_bits = (size * 8) as u32;
        Self { buf, index: 0, size_in_bits, size_in_bits_plus8: size_in_bits + 8 }
    }

    #[inline]
    fn byte(&self, i: usize) -> u32 {
        self.buf.get(i).copied().unwrap_or(0) as u32
    }

    /// `UPDATE_CACHE` + `GET_CACHE`: 32 bits from the byte holding the next
    /// bit, shifted so the next bit is the MSB (low bits zero-filled).
    #[inline]
    pub fn cache32(&self) -> u32 {
        let p = (self.index >> 3) as usize;
        let v = (self.byte(p) << 24) | (self.byte(p + 1) << 16) | (self.byte(p + 2) << 8) | self.byte(p + 3);
        v << (self.index & 7)
    }

    /// `show_bits(n)`, 1 <= n <= 25.
    #[inline]
    pub fn show_bits(&self, n: u32) -> u32 {
        debug_assert!((1..=25).contains(&n));
        self.cache32() >> (32 - n)
    }

    /// `skip_bits(n)` (checked: saturates at `size_in_bits + 8`).
    #[inline]
    pub fn skip_bits(&mut self, n: u32) {
        self.index = self.size_in_bits_plus8.min(self.index.wrapping_add(n));
    }

    /// `SKIP_BITS` with a possibly negative count, as `GET_VLC` performs it.
    #[inline]
    fn skip_bits_signed(&mut self, n: i32) {
        self.index = self.size_in_bits_plus8.min(self.index.wrapping_add(n as u32));
    }

    /// `get_bits(n)`, 1 <= n <= 25.
    #[inline]
    pub fn get_bits(&mut self, n: u32) -> u32 {
        let v = self.show_bits(n);
        self.skip_bits(n);
        v
    }

    /// `get_bits1`.
    #[inline]
    pub fn get_bits1(&mut self) -> u32 {
        let b = ((self.byte((self.index >> 3) as usize) << (self.index & 7)) & 0xFF) >> 7;
        if self.index < self.size_in_bits_plus8 {
            self.index += 1;
        }
        b
    }

    /// `get_bits_long(n)`, 0 <= n <= 32 (64-bit cache variant).
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

    /// `get_sbits(n)`, 1 <= n <= 25.
    pub fn get_sbits(&mut self, n: u32) -> i32 {
        let v = (self.cache32() as i32) >> (32 - n);
        self.skip_bits(n);
        v
    }

    /// `get_bits_count`.
    #[inline]
    pub fn bits_count(&self) -> u32 {
        self.index
    }

    /// `get_bits_left`.
    #[inline]
    pub fn bits_left(&self) -> i32 {
        self.size_in_bits as i32 - self.index as i32
    }

    /// `get_bits_bytesize(gb, 1)`... in bits: `size_in_bits`.
    #[inline]
    pub fn size_in_bits(&self) -> u32 {
        self.size_in_bits
    }

    /// `align_get_bits`.
    pub fn align(&mut self) {
        let n = (8 - (self.index & 7)) & 7;
        if n != 0 {
            self.skip_bits(n);
        }
    }

    /// `GET_VLC` with `max_depth` levels over an FFmpeg-layout table.
    #[inline]
    pub fn get_vlc2(&mut self, table: &[crate::vlc::VlcElem], bits: u32, max_depth: u32) -> i32 {
        let e = table[self.show_bits(bits) as usize];
        let mut code = e.sym as i32;
        let mut n = e.len as i32;
        if max_depth > 1 && n < 0 {
            self.skip_bits(bits);
            let nb_bits = (-n) as u32;
            let e = table[(self.show_bits(nb_bits) as i32 + code) as usize];
            code = e.sym as i32;
            n = e.len as i32;
            if max_depth > 2 && n < 0 {
                self.skip_bits(nb_bits);
                let nb_bits = (-n) as u32;
                let e = table[(self.show_bits(nb_bits) as i32 + code) as usize];
                code = e.sym as i32;
                n = e.len as i32;
            }
        }
        self.skip_bits_signed(n);
        code
    }

    /// `get_interleaved_ue_golomb` (non-cached reader variant).
    pub fn get_interleaved_ue_golomb(&mut self) -> u32 {
        let mut buf = self.cache32();
        if buf & 0xAA80_0000 != 0 {
            let b = (buf >> 24) as usize;
            self.skip_bits(INTERLEAVED_GOLOMB_VLC_LEN[b] as u32);
            return INTERLEAVED_UE_GOLOMB_VLC_CODE[b] as u32;
        }
        let mut ret: u32 = 1;
        loop {
            let b = (buf >> 24) as usize;
            let len = INTERLEAVED_GOLOMB_VLC_LEN[b] as u32;
            self.skip_bits(len.min(8));
            if len != 9 {
                ret <<= (len - 1) >> 1;
                ret |= INTERLEAVED_DIRAC_GOLOMB_VLC_CODE[b] as u32;
                break;
            }
            ret = (ret << 4) | INTERLEAVED_DIRAC_GOLOMB_VLC_CODE[b] as u32;
            buf = self.cache32();
            if !(ret < 0x800_0000 && self.index < self.size_in_bits_plus8) {
                break;
            }
        }
        ret.wrapping_sub(1)
    }

    /// `get_interleaved_se_golomb` (non-cached reader variant).
    pub fn get_interleaved_se_golomb(&mut self) -> i32 {
        let mut buf = self.cache32();
        if buf & 0xAA80_0000 != 0 {
            let b = (buf >> 24) as usize;
            self.skip_bits(INTERLEAVED_GOLOMB_VLC_LEN[b] as u32);
            return INTERLEAVED_SE_GOLOMB_VLC_CODE[b] as i32;
        }
        self.skip_bits(8);
        buf |= 1 | (self.cache32() >> 8);
        if buf & 0xAAAA_AAAA == 0 {
            return INVALID_VLC;
        }
        let mut log: u32 = 31;
        while buf & 0x8000_0000 == 0 && log > 1 {
            buf = (buf << 2).wrapping_sub((buf << log) >> (log - 1)).wrapping_add(buf >> 30);
            log -= 1;
        }
        self.skip_bits(63 - 2 * log - 8);
        let x = ((buf << log) >> log).wrapping_sub(1) ^ (buf & 1).wrapping_neg();
        (x.wrapping_add(1) as i32) >> 1
    }
}

/// `mid_pred(a, b, c)`: median of three.
#[inline]
pub fn mid_pred(a: i32, b: i32, c: i32) -> i32 {
    let (mut a, mut b) = (a, b);
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    // a <= b
    if c > b {
        b
    } else if c > a {
        c
    } else {
        a
    }
}

/// `av_clip_uint8`.
#[inline]
pub fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// `sign_extend(val, bits)`.
#[inline]
pub fn sign_extend(val: i32, bits: u32) -> i32 {
    let shift = 32 - bits;
    (((val as u32) << shift) as i32) >> shift
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes `v` as an interleaved exp-Golomb code into a bit vector.
    fn put_ue(bits: &mut Vec<u8>, v: u32) {
        let x = v + 1;
        let nbits = 32 - x.leading_zeros();
        for i in (0..nbits - 1).rev() {
            bits.push(0);
            bits.push(((x >> i) & 1) as u8);
        }
        bits.push(1);
    }

    fn pack(bits: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; bits.len().div_ceil(8) + 8];
        for (i, &b) in bits.iter().enumerate() {
            out[i / 8] |= b << (7 - (i % 8));
        }
        out
    }

    #[test]
    fn interleaved_golomb_round_trip() {
        let values: Vec<u32> = (0..300).chain([1000, 4095, 65534, 100_000]).collect();
        let mut bits = Vec::new();
        for &v in &values {
            put_ue(&mut bits, v);
        }
        let data = pack(&bits);
        let mut gb = BitReader::new(&data, data.len());
        for &v in &values {
            assert_eq!(gb.get_interleaved_ue_golomb(), v);
        }
        let mut gb = BitReader::new(&data, data.len());
        for &v in &values {
            let se = gb.get_interleaved_se_golomb();
            if v >= 65535 {
                // FFmpeg's reader gives up on codes longer than 31 bits.
                assert_eq!(se, INVALID_VLC, "ue {v}");
                break;
            }
            let expect = if v & 1 == 1 { v.div_ceil(2) as i32 } else { -((v / 2) as i32) };
            assert_eq!(se, expect, "ue {v}");
        }
    }

    #[test]
    fn mid_pred_is_median() {
        for a in -3..3 {
            for b in -3..3 {
                for c in -3..3 {
                    let mut v = [a, b, c];
                    v.sort();
                    assert_eq!(mid_pred(a, b, c), v[1]);
                }
            }
        }
    }
}
