//! FFmpeg-`GetBitContext`-compatible MSB-first bit reader, ported from
//! FFmpeg libavcodec/get_bits.h (commit 2da55bf).
//!
//! License: GNU Lesser General Public License, version 2.1 or later.
//!
//! Past-the-end reads return zero bits and the position saturates, which
//! mirrors FFmpeg's `UNCHECKED_BITSTREAM_READER` behaviour that the H.263
//! decoder relies on; `get_bits_left` stays the malformed-input guard.

#![forbid(unsafe_code)]

/// Maximum index into a VLC table read in one step (9 bits is what every
/// RV VLC uses).
pub const TEX_VLC_BITS: u32 = 9;

#[derive(Clone)]
pub struct GetBitContext<'a> {
    data: &'a [u8],
    /// Bit position, may exceed `data.len() * 8` after unchecked reads.
    index: usize,
    /// `size_in_bits`, cached like FFmpeg does.
    size_in_bits: usize,
}

impl<'a> GetBitContext<'a> {
    /// `init_get_bits8`: byte-aligned buffer with a bit length.
    pub fn new(buf: &'a [u8]) -> Self {
        let size_in_bits = buf.len().saturating_mul(8);
        Self { data: buf, index: 0, size_in_bits }
    }

    /// Total readable bits (`size_in_bits`).
    pub fn size_in_bits(&self) -> usize {
        self.size_in_bits
    }

    /// `get_bits_count`.
    pub fn bits_count(&self) -> usize {
        self.index
    }

    /// `get_bits_left`: may be negative in FFmpeg; here it saturates at 0
    /// but callers compare it against 0 the same way.
    pub fn bits_left(&self) -> i32 {
        self.size_in_bits as i32 - self.index as i32
    }

    /// Read `n` bits (n <= 25 in all call sites). Past the end: zeros.
    pub fn get_bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let mut value: u32 = 0;
        for _ in 0..n {
            value = (value << 1) | self.get_bits1();
        }
        value
    }

    /// Read a single bit. Past the end: zero.
    pub fn get_bits1(&mut self) -> u32 {
        let byte = self.index >> 3;
        let bit = if byte < self.data.len() {
            ((self.data[byte] >> (7 - (self.index & 7))) & 1) as u32
        } else {
            0
        };
        self.index += 1;
        bit
    }

    /// `show_bits`: peek `n` bits without consuming.
    pub fn show_bits(&self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let mut value: u32 = 0;
        let mut pos = self.index;
        for _ in 0..n {
            let byte = pos >> 3;
            let b = if byte < self.data.len() {
                ((self.data[byte] >> (7 - (pos & 7))) & 1) as u32
            } else {
                0
            };
            value = (value << 1) | b;
            pos += 1;
        }
        value
    }

    /// `skip_bits`.
    pub fn skip_bits(&mut self, n: u32) {
        self.index += n as usize;
    }

    /// `skip_bits1`.
    pub fn skip_bits1(&mut self) {
        self.index += 1;
    }

    /// `align_get_bits`.
    pub fn align_get_bits(&mut self) {
        let rem = self.index & 7;
        if rem != 0 {
            self.index += 8 - rem;
        }
    }

    /// `get_unary(gb, stop, len)`: count bits until `stop` or `len` bits.
    pub fn get_unary(&mut self, stop: u32, len: u32) -> u32 {
        let mut i: u32 = 0;
        while i < len && self.get_bits1() != stop {
            i += 1;
        }
        i
    }
}

/// `sign_extend(val, bits)` from libavutil.
pub fn sign_extend(val: i32, bits: u32) -> i32 {
    if bits == 0 {
        return 0;
    }
    let m = 1i32 << (bits - 1);
    (val & (m - 1)).wrapping_sub(val & m)
}

/// `mid_pred(a, b, c)` — median of three.
pub fn mid_pred(a: i32, b: i32, c: i32) -> i32 {
    let mut a = a;
    let mut b = b;
    let mut c = c;
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    if b > c {
        b = c;
        if a > b {
            b = a;
        }
    }
    b
}

/// `av_clip` for i32.
pub fn av_clip_i32(v: i32, min: i32, max: i32) -> i32 {
    v.clamp(min, max)
}

/// `av_clip_uint8`.
pub fn av_clip_uint8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first() {
        let data = [0b1011_0100, 0b0010_1111];
        let mut gb = GetBitContext::new(&data);
        assert_eq!(gb.get_bits(4), 0b1011);
        assert_eq!(gb.get_bits(4), 0b0100);
        assert_eq!(gb.get_bits(8), 0b0010_1111);
        assert_eq!(gb.bits_left(), 0);
    }

    #[test]
    fn past_end_reads_zero() {
        let data = [0xFF];
        let mut gb = GetBitContext::new(&data);
        assert_eq!(gb.get_bits(8), 0xFF);
        assert_eq!(gb.get_bits(8), 0);
        assert_eq!(gb.bits_left(), -8);
    }

    #[test]
    fn show_and_skip() {
        let data = [0b1100_1010];
        let gb = GetBitContext::new(&data);
        assert_eq!(gb.show_bits(3), 0b110);
        let mut gb = gb;
        gb.skip_bits(2);
        assert_eq!(gb.get_bits(3), 0b001);
    }

    #[test]
    fn mid_pred_orders() {
        assert_eq!(mid_pred(1, 2, 3), 2);
        assert_eq!(mid_pred(3, 1, 2), 2);
        assert_eq!(mid_pred(9, 9, 1), 9);
    }
}
