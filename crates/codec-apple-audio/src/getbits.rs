// Bit readers.
//
// Ported from FFmpeg libavcodec/get_bits.h and unary.h (commit 2da55bf),
// LGPL-2.1-or-later: the checked reader, big-endian (the default) and
// little-endian (`BITSTREAM_READER_LE`).

//! FFmpeg's checked `GetBitContext` (`CONFIG_SAFE_BITSTREAM_READER`):
//! [`GetBits`] takes bits from the most significant end of each byte, as
//! ALAC reads its packets; [`GetBitsLe`] from the least significant end,
//! as QDM2 and QDMC do. Reading past the end returns the zero bits of
//! FFmpeg's input padding, and the position stops 8 bits past the end, so
//! `bits_left` can fall to -8 as it does there.

pub struct GetBits<'a> {
    data: &'a [u8],
    /// Bits read (`index`), at most `len * 8 + 8` (`size_in_bits_plus8`).
    index: usize,
}

impl<'a> GetBits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, index: 0 }
    }

    fn size_in_bits(&self) -> usize {
        self.data.len() * 8
    }

    fn advance(&mut self, n: usize) {
        self.index = (self.index + n).min(self.size_in_bits() + 8);
    }

    /// `get_bits_left`
    pub fn bits_left(&self) -> i64 {
        self.size_in_bits() as i64 - self.index as i64
    }

    /// `show_bits` / `show_bits_long`: the next `n` (0..=32) bits.
    pub fn show(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte = self.index >> 3;
        let mut acc = 0u64;
        for i in 0..5 {
            acc = (acc << 8) | u64::from(self.data.get(byte + i).copied().unwrap_or(0));
        }
        let shift = 40 - (self.index & 7) - n as usize;
        ((acc >> shift) & ((1u64 << n) - 1)) as u32
    }

    /// `get_bits` / `get_bits_long`: the next `n` (0..=32) bits.
    pub fn get(&mut self, n: u32) -> u32 {
        let v = self.show(n);
        self.advance(n as usize);
        v
    }

    /// `get_bits1`
    pub fn get1(&mut self) -> u32 {
        self.get(1)
    }

    /// `get_sbits` / `get_sbits_long`: `n` (1..=32) bits, sign-extended.
    pub fn get_signed(&mut self, n: u32) -> i32 {
        let v = self.get(n);
        let shift = 32 - n;
        ((v << shift) as i32) >> shift
    }

    /// `skip_bits`
    pub fn skip(&mut self, n: u32) {
        self.advance(n as usize);
    }

    /// `get_unary(gb, 0, len)`: ones up to the first zero (read) or `len`.
    pub fn unary0(&mut self, len: u32) -> u32 {
        let mut i = 0;
        while i < len && self.get1() != 0 {
            i += 1;
        }
        i
    }
}

pub struct GetBitsLe<'a> {
    data: &'a [u8],
    /// Bits read (`index`), at most `len * 8 + 8` (`size_in_bits_plus8`).
    index: usize,
}

impl<'a> GetBitsLe<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, index: 0 }
    }

    /// The bytes being read (FFmpeg's `gb->buffer`).
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    fn size_in_bits(&self) -> usize {
        self.data.len() * 8
    }

    /// `get_bits_left`
    pub fn bits_left(&self) -> i64 {
        self.size_in_bits() as i64 - self.index as i64
    }

    /// `get_bits_count`
    pub fn bits_count(&self) -> usize {
        self.index
    }

    /// `show_bits` / `show_bits_long`: the next `n` (0..=32) bits, the
    /// first read in the lowest bit.
    pub fn show(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte = self.index >> 3;
        let mut acc = 0u64;
        for i in (0..5).rev() {
            acc = (acc << 8) | u64::from(self.data.get(byte + i).copied().unwrap_or(0));
        }
        ((acc >> (self.index & 7)) & ((1u64 << n) - 1)) as u32
    }

    /// `get_bits` / `get_bits_long` / `get_bitsz`
    pub fn get(&mut self, n: u32) -> u32 {
        let v = self.show(n);
        self.skip(n as usize);
        v
    }

    /// `get_bits1`
    pub fn get1(&mut self) -> u32 {
        self.get(1)
    }

    /// `skip_bits` / `skip_bits_long`
    pub fn skip(&mut self, n: usize) {
        self.index = (self.index + n).min(self.size_in_bits() + 8);
    }
}

#[cfg(test)]
mod tests {
    use super::{GetBits, GetBitsLe};

    #[test]
    fn reads_msb_first_and_pads_with_zeros() {
        let mut gb = GetBits::new(&[0b1010_0000, 0xff]);
        assert_eq!(gb.get(3), 0b101);
        assert_eq!(gb.show(13), 0b0_0000_1111_1111);
        assert_eq!(gb.get(13), 0b0_0000_1111_1111);
        assert_eq!(gb.bits_left(), 0);
        // Past the end: zero bits, and the position stops 8 bits on.
        assert_eq!(gb.get(32), 0);
        assert_eq!(gb.bits_left(), -8);
    }

    #[test]
    fn signed_and_unary() {
        let mut gb = GetBits::new(&[0xff, 0xfe, 0b1101_0000]);
        assert_eq!(gb.get_signed(16), -2);
        assert_eq!(gb.unary0(9), 2);
        assert_eq!(gb.bits_left(), 24 - 19);
        let mut ones = GetBits::new(&[0xff, 0xff]);
        assert_eq!(ones.unary0(9), 9);
        assert_eq!(ones.bits_left(), 16 - 9);
    }

    #[test]
    fn reads_lsb_first_and_pads_with_zeros() {
        let mut gb = GetBitsLe::new(&[0b1010_0101, 0x01, 0x80]);
        assert_eq!(gb.get(1), 1);
        assert_eq!(gb.get(2), 0b10);
        assert_eq!(gb.show(14), 0b00_0000_0011_0100);
        assert_eq!(gb.get(14), 0b00_0000_0011_0100);
        assert_eq!(gb.get(7), 0b100_0000);
        assert_eq!(gb.bits_left(), 0);
        assert_eq!(gb.get(32), 0);
        assert_eq!(gb.bits_left(), -8);
        // A 32-bit read assembles the bytes little-endian.
        assert_eq!(GetBitsLe::new(b"QMC\x01").get(32), u32::from_le_bytes(*b"QMC\x01"));
    }
}
