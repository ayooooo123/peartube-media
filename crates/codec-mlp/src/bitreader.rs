// Ported from FFmpeg libavcodec/get_bits.h semantics and libavcodec/mlp_parse.c
// (commit 2da55bf). Licensed under LGPL-2.1-or-later.

//! Big-endian MSB-first bit reader, the subset `get_bits.h` provides that
//! the MLP decoder uses. Bounds-checked: reads past the end return 0 bits
//! (FFmpeg guarantees buffers are padded; we cannot pad untrusted input, so
//! overreads degrade to zeros instead of panicking).

pub struct BitReader<'a> {
    buf: &'a [u8],
    /// Bit position from the start of `buf`.
    pos: usize,
    /// Buffer length in bits.
    len_bits: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            len_bits: buf.len() * 8,
        }
    }

    #[inline]
    pub fn bits_read(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn bits_left(&self) -> usize {
        self.len_bits.saturating_sub(self.pos)
    }

    /// `get_bits`: read up to 25 bits MSB-first; 0 when past the end.
    #[inline]
    pub fn get_bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let n = n as usize;
        if self.pos + n > self.len_bits {
            // Consume what is left; FFmpeg reads into padding. We return the
            // available bits zero-extended so the decoder errors out on the
            // resulting structure instead of panicking.
            let avail = self.len_bits - self.pos;
            self.pos = self.len_bits;
            return self.read_bits_at(self.len_bits - avail, avail);
        }
        let v = self.read_bits_at(self.pos, n);
        self.pos += n;
        v
    }

    /// `show_bits`: like `get_bits` without consuming.
    #[inline]
    pub fn show_bits(&self, n: u32) -> u32 {
        let n = n as usize;
        if n == 0 {
            return 0;
        }
        if self.pos + n > self.len_bits {
            let avail = self.len_bits - self.pos;
            return self.read_bits_at(self.pos, avail);
        }
        self.read_bits_at(self.pos, n)
    }

    /// `skip_bits`.
    #[inline]
    pub fn skip(&mut self, n: u32) {
        self.pos = (self.pos + n as usize).min(self.len_bits);
    }

    /// `skip_bits(&gb, (-get_bits_count(&gb)) & 15)`: align to 16 bits.
    #[inline]
    pub fn align_16(&mut self) {
        self.pos = (self.pos + 15) & !15;
        if self.pos > self.len_bits {
            self.pos = self.len_bits;
        }
    }

    /// `get_sbits`: sign-extend the `n`-bit value.
    #[inline]
    pub fn get_sbits(&mut self, n: u32) -> i32 {
        let v = self.get_bits(n);
        if n == 0 {
            return 0;
        }
        // Sign-extend from bit n-1.
        let sign = 1i32 << (n - 1);
        ((v as i32) & ((sign << 1) - 1)) - (((v as i32) & sign) << 1)
    }

    #[inline]
    fn read_bits_at(&self, start: usize, n: usize) -> u32 {
        let mut v: u32 = 0;
        let mut pos = start;
        let mut remaining = n;
        while remaining > 0 {
            let byte = pos / 8;
            let bit = pos % 8;
            let take = remaining.min(8 - bit);
            let chunk = if byte < self.buf.len() {
                (self.buf[byte] >> (8 - bit - take)) as u32 & ((1u32 << take) - 1)
            } else {
                0
            };
            v = (v << take) | chunk;
            pos += take;
            remaining -= take;
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first() {
        let br = BitReader::new(&[0b1011_0010, 0b0100_0000]);
        let mut br = br;
        assert_eq!(br.get_bits(4), 0b1011);
        assert_eq!(br.get_bits(4), 0b0010);
        assert_eq!(br.get_bits(2), 0b01);
    }

    #[test]
    fn sign_extension() {
        let mut br = BitReader::new(&[0b1111_1111, 0b0111_1111]);
        assert_eq!(br.get_sbits(4), -1);
        assert_eq!(br.get_sbits(4), -1);
        assert_eq!(br.get_sbits(4), 7);
    }

    #[test]
    fn overreads_return_zero() {
        let mut br = BitReader::new(&[0xff]);
        br.skip(8);
        assert_eq!(br.get_bits(8), 0);
        assert_eq!(br.get_sbits(8), 0);
    }
}
