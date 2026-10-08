// FFmpeg's cached little-endian bit reader, as TAK reads with it.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/bitstream_template.h with
// BITSTREAM_TEMPLATE_LE (CACHED_BITSTREAM_READER and BITSTREAM_READER_LE,
// as tak.c, takdec.c and tak_parser.c define them), get_bits.h's names
// for it and unary.h's get_unary.
// Copyright (c) 2016 Alexandra Hájková (bitstream_template.h);
// LGPL-2.1-or-later (see LICENSE).

/// A reader over `data`. Bytes past `data` are zeros, as FFmpeg's padding;
/// once the refill pointer passes the end, reads return what the cache
/// holds plus zeros and the position stops advancing, as FFmpeg's does.
pub(crate) struct Bits<'a> {
    data: &'a [u8],
    ptr: usize,
    end: usize,
    size_in_bits: usize,
    bits: u64,
    bits_valid: u32,
}

impl<'a> Bits<'a> {
    /// `bits_init8`
    pub(crate) fn new(data: &'a [u8]) -> Self {
        let size_in_bits = data.len() * 8;
        let mut b = Self { data, ptr: 0, end: data.len(), size_in_bits, bits: 0, bits_valid: 0 };
        b.refill_64();
        b
    }

    fn byte(&self, i: usize) -> u64 {
        u64::from(self.data.get(i).copied().unwrap_or(0))
    }

    fn rl64(&self, at: usize) -> u64 {
        (0..8).fold(0, |v, i| v | self.byte(at + i) << (8 * i))
    }

    fn rl32(&self, at: usize) -> u64 {
        (0..4).fold(0, |v, i| v | self.byte(at + i) << (8 * i))
    }

    /// `bits_priv_refill_64`
    fn refill_64(&mut self) -> bool {
        if self.ptr >= self.end {
            return false;
        }
        self.bits = self.rl64(self.ptr);
        self.ptr += 8;
        self.bits_valid = 64;
        true
    }

    /// `bits_priv_refill_32`
    fn refill_32(&mut self) -> bool {
        if self.ptr >= self.end {
            return false;
        }
        self.bits |= self.rl32(self.ptr) << self.bits_valid;
        self.ptr += 4;
        self.bits_valid += 32;
        true
    }

    /// `bits_priv_val_get`, `n` from 1 to 63.
    #[inline]
    fn val_get(&mut self, n: u32) -> u64 {
        let v = self.bits & (u64::MAX >> (64 - n));
        self.bits >>= n;
        self.bits_valid = self.bits_valid.wrapping_sub(n);
        v
    }

    /// `get_bits_count` (`bits_tell`): computed in 64 bits and returned as
    /// an `int`, so a cache count that wrapped past the end (a skip there)
    /// reads as a negative count, as in FFmpeg.
    pub(crate) fn tell(&self) -> isize {
        (self.ptr as i64 * 8 - i64::from(self.bits_valid)) as i32 as isize
    }

    /// `get_bits_left` (`bits_left`), with the same `int` result.
    pub(crate) fn left(&self) -> isize {
        (self.size_in_bits as i64 - self.ptr as i64 * 8 + i64::from(self.bits_valid)) as i32 as isize
    }

    /// `get_bits1` (`bits_read_bit`)
    pub(crate) fn bit(&mut self) -> u32 {
        if self.bits_valid == 0 && !self.refill_64() {
            return 0;
        }
        self.val_get(1) as u32
    }

    /// `get_bits` (`bits_read_nz`), `n` from 1 to 32.
    pub(crate) fn get(&mut self, n: u32) -> u32 {
        if n > self.bits_valid && !self.refill_32() {
            self.bits_valid = n;
        }
        self.val_get(n) as u32
    }

    /// `get_bits_long` (`bits_read`), `n` from 0 to 32.
    pub(crate) fn get_long(&mut self, n: u32) -> u32 {
        if n == 0 { 0 } else { self.get(n) }
    }

    /// `get_sbits` (`bits_read_signed_nz`), `n` from 1 to 32.
    pub(crate) fn sget(&mut self, n: u32) -> i32 {
        let v = self.get(n);
        ((v << (32 - n)) as i32) >> (32 - n)
    }

    /// `get_bits64` (`bits_read_63`), `n` from 1 to 63.
    pub(crate) fn get63(&mut self, mut n: u32) -> u64 {
        let mut ret = 0u64;
        let mut left = 0;
        if n > self.bits_valid {
            left = self.bits_valid;
            n -= left;
            if left > 0 {
                ret = self.val_get(left);
            }
            if !self.refill_64() {
                self.bits_valid = n;
            }
        }
        self.val_get(n) << left | ret
    }

    /// `skip_bits` (`bits_skip`)
    pub(crate) fn skip(&mut self, n: u32) {
        if n < self.bits_valid {
            self.bits >>= n;
            self.bits_valid -= n;
        } else {
            let mut n = n - self.bits_valid;
            self.bits = 0;
            self.bits_valid = 0;
            if n >= 64 {
                let skip = n / 8;
                n -= skip * 8;
                self.ptr += skip as usize;
            }
            self.refill_64();
            if n > 0 {
                self.bits >>= n;
                self.bits_valid = self.bits_valid.wrapping_sub(n);
            }
        }
    }

    /// `align_get_bits` (`bits_align`)
    pub(crate) fn align(&mut self) {
        let n = (self.tell().wrapping_neg() & 7) as u32;
        if n > 0 {
            self.skip(n);
        }
    }

    /// `get_unary(gb, 1, len)`: zeros until a one (read too), at most `len`.
    pub(crate) fn unary1(&mut self, len: u32) -> u32 {
        let mut i = 0;
        while i < len && self.bit() != 1 {
            i += 1;
        }
        i
    }
}
