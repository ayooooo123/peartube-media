// Port of the checked `GetBitContext` reader (libavcodec/get_bits.h, FFmpeg
// commit 2da55bf) the ATRAC decoders use.
// Copyright (c) the FFmpeg developers; LGPL-2.1-or-later (see LICENSE).

/// MSB-first reader over the first `size_bits` bits of a buffer. As in
/// FFmpeg's safe reader the position saturates 8 bits past the end, and
/// bits beyond the buffer read as zero (FFmpeg reads its zeroed padding).
pub(crate) struct BitReader<'a> {
    buf: &'a [u8],
    index: usize,
    size_bits: usize,
}

impl<'a> BitReader<'a> {
    /// `init_get_bits(gb, buf, bit_size)`.
    pub(crate) fn new(buf: &'a [u8], size_bits: usize) -> Self {
        Self {
            buf,
            index: 0,
            size_bits,
        }
    }

    /// `init_get_bits8(gb, buf, byte_size)`.
    pub(crate) fn from_bytes(buf: &'a [u8]) -> Self {
        Self::new(buf, buf.len() * 8)
    }

    fn byte(&self, i: usize) -> u64 {
        u64::from(self.buf.get(i).copied().unwrap_or(0))
    }

    /// The next `n` (0..=32) bits without consuming them.
    pub(crate) fn show(&self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let first = self.index >> 3;
        let mut window = 0u64;
        for k in 0..5 {
            window = (window << 8) | self.byte(first + k);
        }
        let shift = 40 - (self.index & 7) as u32 - n;
        ((window >> shift) & ((1u64 << n) - 1)) as u32
    }

    pub(crate) fn skip(&mut self, n: usize) {
        self.index = (self.index + n).min(self.size_bits + 8);
    }

    /// `get_bits` / `get_bits_long`: `n` in 0..=32.
    pub(crate) fn get(&mut self, n: u32) -> u32 {
        let v = self.show(n);
        self.skip(n as usize);
        v
    }

    pub(crate) fn get1(&mut self) -> u32 {
        self.get(1)
    }

    /// `get_bits` as a signed value for arithmetic.
    pub(crate) fn geti(&mut self, n: u32) -> i32 {
        self.get(n) as i32
    }

    /// `get_sbits`: `n` in 1..=32, two's complement.
    pub(crate) fn get_s(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        let v = self.get(n);
        ((v << (32 - n)) as i32) >> (32 - n)
    }

    /// `get_bits_left`.
    pub(crate) fn left(&self) -> isize {
        self.size_bits as isize - self.index as isize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first_signed_and_zero_past_the_end() {
        let data = [0b1011_0001u8, 0xFF];
        let mut br = BitReader::from_bytes(&data);
        assert_eq!(br.get(3), 0b101);
        assert_eq!(br.get_s(3), -4); // 0b100
        assert_eq!(br.get(2), 0b01);
        assert_eq!(br.get(8), 0xFF);
        assert_eq!(br.left(), 0);
        assert_eq!(br.get(16), 0);
        assert_eq!(br.left(), -8); // saturates 8 bits past the end
    }
}
