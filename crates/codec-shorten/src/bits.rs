// FFmpeg's checked big-endian bit reader and the Shorten Rice codes.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/get_bits.h (the
// CONFIG_SAFE_BITSTREAM_READER path: the index stops 8 bits past the end)
// and libavcodec/golomb.h (get_ur_golomb_jpegls, its uncached path, and
// get_ur_golomb_shorten / get_sr_golomb_shorten).
// Copyright (c) 2004 Michael Niedermayer <michaelni@gmx.at> (get_bits.h);
// Copyright (c) 2003 Michael Niedermayer, (c) 2004 Alex Beregszaszi
// (golomb.h); LGPL-2.1-or-later (see LICENSE).

/// `MIN_CACHE_BITS`
const MIN_CACHE_BITS: u32 = 25;

/// `av_log2`: the index of the highest set bit, 0 for 0.
#[inline]
pub(crate) fn av_log2(v: u32) -> i32 {
    if v == 0 { 0 } else { 31 - v.leading_zeros() as i32 }
}

/// A reader over `data`, `size_in_bits` long. Bytes of `data` past the
/// size are read as FFmpeg reads them from its buffer (the bitstream buffer
/// past the valid bytes, or a probe buffer's tail); past `data`, zeros.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    index: usize,
    size_in_bits: usize,
    size_plus8: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8], size_bytes: usize) -> Self {
        let size_in_bits = size_bytes * 8;
        Self { data, index: 0, size_in_bits, size_plus8: size_in_bits + 8 }
    }

    /// `UPDATE_CACHE_BE`: 32 bits from the index on.
    #[inline]
    fn cache(&self) -> u32 {
        let p = self.index >> 3;
        let b = |i: usize| u32::from(self.data.get(p + i).copied().unwrap_or(0));
        (b(0) << 24 | b(1) << 16 | b(2) << 8 | b(3)) << (self.index & 7)
    }

    /// `SKIP_COUNTER`: the index never passes 8 bits beyond the end.
    #[inline]
    fn skip(&mut self, n: usize) {
        self.index = self.size_plus8.min(self.index + n);
    }

    /// `skip_bits`
    pub(crate) fn skip_bits(&mut self, n: usize) {
        self.skip(n);
    }

    /// `get_bits`, `n` from 1 to 25.
    #[inline]
    pub(crate) fn get_bits(&mut self, n: u32) -> u32 {
        let v = self.cache() >> (32 - n);
        self.skip(n as usize);
        v
    }

    /// `get_bits_long`, `n` up to 32.
    pub(crate) fn get_bits_long(&mut self, n: u32) -> u32 {
        if n <= MIN_CACHE_BITS {
            self.get_bits(n)
        } else {
            let hi = self.get_bits(16) << (n - 16);
            hi | self.get_bits(n - 16)
        }
    }

    /// `get_bits_count`
    pub(crate) fn count(&self) -> usize {
        self.index
    }

    /// `get_bits_left`
    pub(crate) fn left(&self) -> isize {
        self.size_in_bits as isize - self.index as isize
    }

    /// `get_ur_golomb_jpegls(gb, k, INT_MAX, 0)`, uncached: a run of zeros,
    /// a one, then `k` bits; -1 when the zeros run past the end.
    pub(crate) fn ur_golomb_jpegls(&mut self, k: u32) -> i32 {
        let buf = self.cache();
        let log = av_log2(buf);
        let ki = k as i32;
        if log - ki >= 32 - MIN_CACHE_BITS as i32 {
            let v = (buf >> (log - ki)).wrapping_add(30u32.wrapping_sub(log as u32) << k);
            self.skip((32 + ki - log) as usize);
            return v as i32;
        }
        let mut i: i32 = 0;
        let mut cache = buf;
        while cache >> (32 - MIN_CACHE_BITS) == 0 {
            if self.size_in_bits <= self.index {
                return -1;
            }
            self.skip(MIN_CACHE_BITS as usize);
            cache = self.cache();
            i += MIN_CACHE_BITS as i32;
        }
        while cache >> 31 == 0 {
            cache <<= 1;
            self.skip(1);
            i += 1;
        }
        self.skip(1);
        cache = self.cache();
        let v = if k == 0 {
            0
        } else if k > MIN_CACHE_BITS - 1 {
            let hi = (cache >> 16) << (k - 16);
            self.skip(16);
            cache = self.cache();
            let lo = cache >> (32 - (k - 16));
            self.skip((k - 16) as usize);
            hi | lo
        } else {
            let v = cache >> (32 - k);
            self.skip(k as usize);
            v
        };
        v.wrapping_add((i as u32) << k) as i32
    }

    /// `get_ur_golomb_shorten`
    #[inline]
    pub(crate) fn ur(&mut self, k: u32) -> u32 {
        self.ur_golomb_jpegls(k) as u32
    }

    /// `get_sr_golomb_shorten`
    #[inline]
    pub(crate) fn sr(&mut self, k: u32) -> i32 {
        let u = self.ur_golomb_jpegls(k + 1);
        (u >> 1) ^ (u & 1).wrapping_neg()
    }
}
