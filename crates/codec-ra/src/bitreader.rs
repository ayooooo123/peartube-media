//! Bitstream reader utilities for MSB-first and LSB-first bitstreams.
//! Ported and adapted from FFmpeg (libavcodec/get_bits.h, golomb.h).
//! Commit: 2da55bf.
//! License: LGPL-2.1-or-later.

#![forbid(unsafe_code)]

/// MSB-first (Big-Endian bit order) bit reader.
#[derive(Clone, Debug)]
pub struct BitReaderBe<'a> {
    data: &'a [u8],
    bit_pos: usize,
    total_bits: usize,
}

impl<'a> BitReaderBe<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            bit_pos: 0,
            total_bits: data.len() * 8,
        }
    }

    pub fn with_bits(data: &'a [u8], bits: usize) -> Self {
        let max_bits = data.len() * 8;
        Self {
            data,
            bit_pos: 0,
            total_bits: bits.min(max_bits),
        }
    }

    #[inline]
    pub fn bits_left(&self) -> usize {
        self.total_bits.saturating_sub(self.bit_pos)
    }

    #[inline]
    pub fn bit_position(&self) -> usize {
        self.bit_pos
    }

    #[inline]
    pub fn set_bit_position(&mut self, pos: usize) {
        self.bit_pos = pos.min(self.total_bits);
    }

    #[inline]
    pub fn read_bit(&mut self) -> Option<u32> {
        if self.bit_pos >= self.total_bits {
            return None;
        }
        let byte_idx = self.bit_pos / 8;
        let bit_idx = 7 - (self.bit_pos % 8);
        self.bit_pos += 1;
        let byte = self.data.get(byte_idx)?;
        Some(((byte >> bit_idx) & 1) as u32)
    }

    pub fn read_bits(&mut self, n: usize) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if n > 32 || self.bit_pos + n > self.total_bits {
            return None;
        }
        let mut res = 0u32;
        for _ in 0..n {
            let byte_idx = self.bit_pos / 8;
            let bit_idx = 7 - (self.bit_pos % 8);
            self.bit_pos += 1;
            let byte = self.data.get(byte_idx)?;
            let bit = ((byte >> bit_idx) & 1) as u32;
            res = (res << 1) | bit;
        }
        Some(res)
    }

    pub fn read_unary(&mut self, stop: u32, max: usize) -> Option<usize> {
        let mut count = 0;
        while count < max {
            let bit = self.read_bit()?;
            if bit == stop {
                break;
            }
            count += 1;
        }
        Some(count)
    }

    pub fn read_ue_golomb(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.read_bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            Some(0)
        } else {
            let rem = self.read_bits(zeros)?;
            Some((1 << zeros) - 1 + rem)
        }
    }
}

/// LSB-first (Little-Endian bit order) bit reader.
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

    pub fn with_bits(data: &'a [u8], bits: usize) -> Self {
        let max_bits = data.len() * 8;
        Self {
            data,
            bit_pos: 0,
            total_bits: bits.min(max_bits),
        }
    }

    #[inline]
    pub fn bits_left(&self) -> usize {
        self.total_bits.saturating_sub(self.bit_pos)
    }

    #[inline]
    pub fn bit_position(&self) -> usize {
        self.bit_pos
    }

    #[inline]
    pub fn read_bit(&mut self) -> Option<u32> {
        if self.bit_pos >= self.total_bits {
            return None;
        }
        let byte_idx = self.bit_pos / 8;
        let bit_idx = self.bit_pos % 8;
        self.bit_pos += 1;
        let byte = self.data.get(byte_idx)?;
        Some(((byte >> bit_idx) & 1) as u32)
    }

    pub fn read_bits(&mut self, n: usize) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if n > 32 || self.bit_pos + n > self.total_bits {
            return None;
        }
        let mut res = 0u32;
        for i in 0..n {
            let byte_idx = self.bit_pos / 8;
            let bit_idx = self.bit_pos % 8;
            self.bit_pos += 1;
            let byte = self.data.get(byte_idx)?;
            let bit = ((byte >> bit_idx) & 1) as u32;
            res |= bit << i;
        }
        Some(res)
    }
}
