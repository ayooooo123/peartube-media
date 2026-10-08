// Ported from FFmpeg libavcodec/get_bits.h (commit 2da55bf)
// Copyright (c) 2004 Michael Niedermayer <michaelni@gmx.at>
// License: LGPL-2.1-or-later

#![forbid(unsafe_code)]

#[derive(Clone, Debug)]
pub struct BitReaderLe<'a> {
    data: &'a [u8],
    bit_pos: usize,
    total_bits: usize,
}

impl<'a> BitReaderLe<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            bit_pos: 0,
            total_bits: data.len() * 8,
        }
    }

    #[inline]
    pub fn bits_left(&self) -> usize {
        self.total_bits.saturating_sub(self.bit_pos)
    }

    #[inline]
    pub fn bits_left_signed(&self) -> isize {
        self.total_bits as isize - self.bit_pos as isize
    }

    #[inline]
    pub fn read_bit(&mut self) -> u32 {
        let bit = if self.bit_pos < self.total_bits {
            let byte_idx = self.bit_pos / 8;
            let bit_idx = self.bit_pos % 8;
            ((self.data[byte_idx] >> bit_idx) & 1) as u32
        } else {
            0
        };
        self.bit_pos += 1;
        bit
    }

    #[inline]
    pub fn read_bits(&mut self, n: usize) -> u32 {
        if n == 0 {
            return 0;
        }
        let bit_idx = self.bit_pos % 8;
        let byte_idx = self.bit_pos / 8;
        let mut chunk = 0u64;
        if byte_idx < self.data.len() {
            let end = (byte_idx + 8).min(self.data.len());
            for (j, &b) in self.data[byte_idx..end].iter().enumerate() {
                chunk |= (b as u64) << (j * 8);
            }
        }
        let mask = if n >= 32 {
            0xFFFF_FFFFu64
        } else {
            (1u64 << n) - 1
        };
        let res = ((chunk >> bit_idx) & mask) as u32;
        self.bit_pos += n;
        res
    }

    #[inline]
    pub fn read_unary_0_33(&mut self) -> usize {
        let mut count = 0;
        while count < 33 {
            if self.read_bit() == 1 {
                count += 1;
            } else {
                break;
            }
        }
        count
    }

    #[inline]
    pub fn get_tail(&mut self, k: u32) -> u32 {
        if k < 1 {
            return 0;
        }
        let p = (31 - k.leading_zeros()) as usize;
        let e = (1i64 << (p + 1)) - k as i64 - 1;
        let mut res = self.read_bits(p);
        if res as i64 >= e {
            let b = self.read_bit();
            res = res.wrapping_mul(2).wrapping_sub(e as u32).wrapping_add(b);
        }
        res
    }
}
