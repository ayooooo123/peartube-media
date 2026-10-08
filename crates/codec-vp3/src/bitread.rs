// Ported from FFmpeg libavcodec/get_bits.h (GetBitContext semantics) at
// commit 2da55bf. Licensed under GNU Lesser General Public License 2.1
// or later.
//
//! FFmpeg-semantics bit reader.
//!
//! VP3's unpack_* loops terminate on `get_bits_left(gb) > 0` and rely on
//! reads past the end yielding zero bits (FFmpeg's cache zero-fills at
//! the end of the buffer, and `skip_bits` may move the index past it).
//! Reproducing those exact semantics keeps malformed and truncated
//! packets on FFmpeg's code paths instead of inventing new error paths
//! that could decode differently.

/// MSB-first bit reader over a borrowed slice with FFmpeg's
/// `GetBitContext` boundary behaviour: `show_bits`/`get_bits` return
/// zero bits past the end, and `skip_bits`/`get_bits` may leave the
/// index beyond the buffer (`bits_left` goes negative; the decode loops
/// check it and bail).
pub struct Gb<'a> {
    data: &'a [u8],
    pub(crate) bit_index: usize,
}

impl<'a> Gb<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, bit_index: 0 }
    }

    /// Bits not yet consumed; negative once the index passed the end.
    pub fn bits_left(&self) -> i64 {
        self.data.len() as i64 * 8 - self.bit_index as i64
    }

    /// Single bit at absolute index `i`; 0 past the end.
    #[inline]
    fn bit(&self, i: usize) -> u32 {
        let byte = i >> 3;
        if byte >= self.data.len() {
            0
        } else {
            ((self.data[byte] >> (7 - (i & 7))) & 1) as u32
        }
    }

    /// Peek the next `n` bits (n <= 32), MSB first, zero-padded past
    /// the end. The index does not move.
    #[inline]
    pub fn show_bits(&self, n: u32) -> u32 {
        let n = n as usize;
        let start = self.bit_index;
        let end = start + n;
        if end <= self.data.len() * 8 {
            // Whole bytes covering [start, end), packed MSB-first into
            // a 64-bit window, then shifted so the wanted bits sit at
            // the top.
            let first = start >> 3;
            let last = (end - 1) >> 3;
            let mut acc: u64 = 0;
            for &b in &self.data[first..=last] {
                acc = (acc << 8) | b as u64;
            }
            let total = (last - first + 1) * 8;
            let drop_head = (start & 7) as u32;
            // acc << (64 - total) puts the covered stream at the top of
            // the 64-bit window; the wanted bits then sit at bits
            // 63 - drop_head .. 63 - drop_head - n + 1, so shift them
            // down to 0..n-1 and mask.
            let shift = 64 - drop_head - n as u32;
            let mask = (1u64 << n) - 1;
            (((acc << (64 - total as u32)) >> shift) & mask) as u32
        } else {
            let mut v = 0u32;
            for k in 0..n {
                v = (v << 1) | self.bit(start + k);
            }
            v
        }
    }

    /// Consume `n` bits without returning them (may pass the end).
    #[inline]
    pub fn skip_bits(&mut self, n: u32) {
        self.bit_index += n as usize;
    }

    /// Read `n` bits (n <= 32), consuming them.
    #[inline]
    pub fn get_bits(&mut self, n: u32) -> u32 {
        let v = self.show_bits(n);
        self.skip_bits(n);
        v
    }

    /// Read one bit, consuming it.
    #[inline]
    pub fn get_bits1(&mut self) -> u32 {
        let v = self.bit(self.bit_index);
        self.bit_index += 1;
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first() {
        let gb = &mut Gb::new(&[0b1011_0101, 0b0110_0010]);
        assert_eq!(gb.get_bits(3), 0b101);
        assert_eq!(gb.get_bits(7), 0b101_0101);
        // The last 6 bits; nothing is left after them.
        assert_eq!(gb.get_bits(6), 0b10_0010);
        assert_eq!(gb.bits_left(), 0);
    }

    #[test]
    fn show_does_not_consume() {
        let gb = &mut Gb::new(&[0b1111_0000]);
        assert_eq!(gb.show_bits(4), 0b1111);
        assert_eq!(gb.show_bits(4), 0b1111);
        assert_eq!(gb.bits_left(), 8);
        gb.skip_bits(2);
        assert_eq!(gb.show_bits(4), 0b1100);
    }

    #[test]
    fn past_end_is_zero() {
        let gb = &mut Gb::new(&[0b1000_0000]);
        gb.skip_bits(8);
        assert_eq!(gb.bits_left(), 0);
        assert_eq!(gb.show_bits(11), 0);
        gb.skip_bits(11);
        assert_eq!(gb.bits_left(), -11);
        assert_eq!(gb.get_bits1(), 0);
    }

    #[test]
    fn wide_show_spans_bytes() {
        let gb = &mut Gb::new(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(gb.show_bits(32), 0xDEAD_BEEF);
        assert_eq!(gb.show_bits(20), 0xD_EADB);
    }
}
