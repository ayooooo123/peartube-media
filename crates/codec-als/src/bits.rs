// FFmpeg's checked big-endian bit reader, as alsdec.c, bgmc.c, mlz.c and
// mpeg4audio.c read with it.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/get_bits.h (the
// CONFIG_SAFE_BITSTREAM_READER path: the index stops 8 bits past the end;
// skip_bits_long also goes backwards) and unary.h's get_unary.
// Copyright (c) 2004 Michael Niedermayer <michaelni@gmx.at> (get_bits.h);
// LGPL-2.1-or-later (see LICENSE).

/// `MIN_CACHE_BITS`
const MIN_CACHE_BITS: u32 = 25;

/// `av_log2`
#[inline]
pub(crate) fn av_log2(v: u32) -> u32 {
    if v == 0 { 0 } else { 31 - v.leading_zeros() }
}

/// `av_ceil_log2` for `x >= 1`.
#[inline]
pub(crate) fn av_ceil_log2(x: u32) -> u32 {
    av_log2(x.wrapping_sub(1) << 1)
}

pub(crate) struct Bits<'a> {
    data: &'a [u8],
    index: usize,
    size_in_bits: usize,
    size_plus8: usize,
}

impl<'a> Bits<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        let size_in_bits = data.len() * 8;
        Self { data, index: 0, size_in_bits, size_plus8: size_in_bits + 8 }
    }

    /// 32 bits from the index on; zeros past the data (FFmpeg's padding).
    #[inline]
    fn cache(&self) -> u32 {
        let p = self.index >> 3;
        let b = |i: usize| u32::from(self.data.get(p + i).copied().unwrap_or(0));
        (b(0) << 24 | b(1) << 16 | b(2) << 8 | b(3)) << (self.index & 7)
    }

    #[inline]
    fn advance(&mut self, n: usize) {
        self.index = self.size_plus8.min(self.index + n);
    }

    /// `get_bits_count`
    pub(crate) fn count(&self) -> usize {
        self.index
    }

    /// `get_bits_left`
    pub(crate) fn left(&self) -> i64 {
        self.size_in_bits as i64 - self.index as i64
    }

    /// `show_bits`, `n` from 1 to 25.
    #[inline]
    pub(crate) fn show(&self, n: u32) -> u32 {
        self.cache() >> (32 - n)
    }

    /// `get_bits`, `n` from 1 to 25.
    #[inline]
    pub(crate) fn get(&mut self, n: u32) -> u32 {
        let v = self.show(n);
        self.advance(n as usize);
        v
    }

    /// `get_bits1`
    #[inline]
    pub(crate) fn bit(&mut self) -> u32 {
        let v = self.cache() >> 31;
        if self.index < self.size_plus8 {
            self.index += 1;
        }
        v
    }

    /// `get_bits_long`, `n` from 0 to 32.
    pub(crate) fn get_long(&mut self, n: u32) -> u32 {
        match n {
            0 => 0,
            1..=MIN_CACHE_BITS => self.get(n),
            _ => {
                let hi = self.get(16) << (n - 16);
                hi | self.get(n - 16)
            }
        }
    }

    /// `get_sbits_long`, `n` from 0 to 32.
    pub(crate) fn sget_long(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        let v = self.get_long(n);
        ((v << (32 - n)) as i32) >> (32 - n)
    }

    /// `skip_bits_long`: either way, within the data and its 8 bits.
    pub(crate) fn skip(&mut self, n: i64) {
        let index = self.index as i64;
        self.index = (index + n.clamp(-index, self.size_plus8 as i64 - index)) as usize;
    }

    /// `align_get_bits`
    pub(crate) fn align(&mut self) {
        let n = self.index.wrapping_neg() & 7;
        if n > 0 {
            self.advance(n);
        }
    }

    /// `get_unary(gb, 0, len)`: ones until a zero (read too), at most `len`.
    pub(crate) fn unary0(&mut self, len: i64) -> u32 {
        let mut i = 0i64;
        while i < len && self.bit() != 0 {
            i += 1;
        }
        i as u32
    }
}
