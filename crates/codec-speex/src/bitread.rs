// Ported from FFmpeg libavcodec/get_bits.h (the checked GetBitContext
// reader) at commit 2da55bf.
// Licensed under GNU Lesser General Public License 2.1 or later.

//! MSB-first bit reader with FFmpeg's checked-reader behaviour: reads past
//! the end return zero bits (FFmpeg's zeroed input padding), and the
//! position stops 8 bits past the end, so `bits_left` and `count` match
//! FFmpeg's `get_bits_left` and `get_bits_count` on truncated input.

pub struct Gb<'a> {
    data: &'a [u8],
    index: usize,
    /// FFmpeg's `size_in_bits_plus8`.
    limit: usize,
}

impl<'a> Gb<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, index: 0, limit: data.len() * 8 + 8 }
    }

    /// FFmpeg's `get_bits_left`.
    pub fn bits_left(&self) -> i64 {
        self.data.len() as i64 * 8 - self.index as i64
    }

    /// FFmpeg's `get_bits_count`.
    pub fn count(&self) -> usize {
        self.index
    }

    fn bit(&self, i: usize) -> u32 {
        match self.data.get(i >> 3) {
            Some(&b) => u32::from((b >> (7 - (i & 7))) & 1),
            None => 0,
        }
    }

    /// The next `n` bits (1..=32) without consuming them.
    pub fn show_bits(&self, n: u32) -> u32 {
        let mut v = 0u32;
        for k in 0..n as usize {
            v = (v << 1) | self.bit(self.index + k);
        }
        v
    }

    pub fn show_bits1(&self) -> u32 {
        self.show_bits(1)
    }

    /// FFmpeg's `skip_bits_long` for `n >= 0`.
    pub fn skip_bits(&mut self, n: usize) {
        self.index = (self.index + n).min(self.limit);
    }

    /// `n` bits (1..=32).
    pub fn get_bits(&mut self, n: u32) -> u32 {
        let v = self.show_bits(n);
        self.skip_bits(n as usize);
        v
    }

    /// FFmpeg's `get_bitsz`: 0 for `n == 0`.
    pub fn get_bitsz(&mut self, n: u32) -> u32 {
        if n == 0 { 0 } else { self.get_bits(n) }
    }

    pub fn get_bits1(&mut self) -> u32 {
        self.get_bits(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads past the end give zeros and stop 8 bits past the end, as
    /// FFmpeg's checked reader does; the byte count the decoder returns
    /// depends on that position.
    #[test]
    fn position_stops_eight_bits_past_the_end() {
        let gb = &mut Gb::new(&[0b1011_0110]);
        assert_eq!(gb.get_bits(3), 0b101);
        assert_eq!(gb.get_bits(7), 0b101_1000);
        assert_eq!(gb.bits_left(), -2);
        gb.skip_bits(100);
        assert_eq!(gb.count(), 16);
        assert_eq!(gb.bits_left(), -8);
        assert_eq!(gb.get_bits1(), 0);
        assert_eq!(gb.count(), 16);
    }
}
