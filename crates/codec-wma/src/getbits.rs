// Ported from FFmpeg (commit 2da55bf): libavcodec/get_bits.h (the checked
// GetBitContext reader) and libavcodec/put_bits.h / bitstream.c
// (put_bits, ff_copy_bits, flush_put_bits).
// GNU Lesser General Public License 2.1 or later.

//! A bit reader with FFmpeg's checked `GetBitContext` semantics: reads never
//! fail, bits past the declared size come from the backing memory (zero past
//! its end, like FFmpeg's zeroed input padding), and the position saturates
//! at `size_in_bits + 8`. Decoders whose control flow depends on overreads
//! (the WMA Pro bit reservoir) need exactly this to follow FFmpeg.

use crate::vlc::VlcTable;

#[derive(Clone, Debug)]
pub struct GetBits<'a> {
    buf: &'a [u8],
    index: usize,
    size_in_bits: usize,
    size_in_bits_plus8: usize,
}

/// The reader's position without its buffer, for readers that must persist
/// across calls while their buffer is owned elsewhere.
#[derive(Clone, Copy, Debug, Default)]
pub struct GetBitsState {
    index: usize,
    size_in_bits: usize,
}

impl GetBitsState {
    /// `get_bits_count` of the saved reader.
    pub fn bits_count(&self) -> usize {
        self.index
    }
}

impl<'a> GetBits<'a> {
    /// `init_get_bits(gb, buf, size_in_bits)`; `buf` is all the memory the
    /// reader may touch (it can extend past `size_in_bits`).
    pub fn new(buf: &'a [u8], size_in_bits: usize) -> Self {
        Self { buf, index: 0, size_in_bits, size_in_bits_plus8: size_in_bits + 8 }
    }

    pub fn with_state(buf: &'a [u8], state: GetBitsState) -> Self {
        let mut gb = Self::new(buf, state.size_in_bits);
        gb.index = state.index;
        gb
    }

    pub fn state(&self) -> GetBitsState {
        GetBitsState { index: self.index, size_in_bits: self.size_in_bits }
    }

    /// Backing memory, as FFmpeg's `gb->buffer`.
    pub fn buffer(&self) -> &'a [u8] {
        self.buf
    }

    #[inline]
    pub fn bits_count(&self) -> usize {
        self.index
    }

    #[inline]
    pub fn bits_left(&self) -> i64 {
        self.size_in_bits as i64 - self.index as i64
    }

    /// The 32 bits at the current position, MSB first.
    #[inline]
    fn peek32(&self) -> u32 {
        let byte = self.index >> 3;
        let mut w = 0u64;
        for k in 0..5 {
            w = (w << 8) | *self.buf.get(byte + k).unwrap_or(&0) as u64;
        }
        (w >> (8 - (self.index & 7))) as u32
    }

    #[inline]
    pub fn show_bits(&self, n: u32) -> u32 {
        if n == 0 {
            0
        } else {
            self.peek32() >> (32 - n)
        }
    }

    #[inline]
    pub fn skip_bits(&mut self, n: u32) {
        self.index = (self.index + n as usize).min(self.size_in_bits_plus8);
    }

    #[inline]
    pub fn get_bits(&mut self, n: u32) -> u32 {
        let v = self.show_bits(n);
        self.skip_bits(n);
        v
    }

    /// `get_bitsz`: like `get_bits` but `n == 0` is allowed.
    #[inline]
    pub fn get_bitsz(&mut self, n: u32) -> u32 {
        self.get_bits(n)
    }

    #[inline]
    pub fn get_bits1(&mut self) -> u32 {
        self.get_bits(1)
    }

    #[inline]
    pub fn get_sbits(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        let v = self.get_bits(n);
        ((v << (32 - n)) as i32) >> (32 - n)
    }

    /// `skip_bits_long`: the position moves by `n` (possibly negative),
    /// clipped to `[0, size_in_bits + 8]`.
    pub fn skip_bits_long(&mut self, n: i64) {
        let target = (self.index as i64 + n).clamp(0, self.size_in_bits_plus8 as i64);
        self.index = target as usize;
    }

    pub fn align(&mut self) {
        let n = (8 - (self.index & 7)) & 7;
        self.skip_bits(n as u32);
    }

    /// `get_vlc2`: an invalid code yields -1 and consumes nothing.
    #[inline]
    pub fn get_vlc(&mut self, vlc: &VlcTable) -> i32 {
        let (sym, len) = vlc.decode_peek(self.peek32());
        self.skip_bits(len);
        sym
    }
}

/// A bit writer over a caller-owned buffer, as FFmpeg's `PutBitContext`
/// writing into a fixed array: bytes past the written bits keep whatever
/// they held, and `flush` zero-fills the rest of the last partial byte.
#[derive(Clone, Copy, Debug, Default)]
pub struct PutBits {
    count: usize,
}

impl PutBits {
    /// `init_put_bits`.
    pub fn reset(&mut self) {
        self.count = 0;
    }

    /// `put_bits_count`.
    pub fn count(&self) -> usize {
        self.count
    }

    fn put_bit(&mut self, buf: &mut [u8], bit: u32) {
        let byte = self.count >> 3;
        let mask = 0x80u8 >> (self.count & 7);
        if bit != 0 {
            buf[byte] |= mask;
        } else {
            buf[byte] &= !mask;
        }
        self.count += 1;
    }

    /// `put_bits(pb, n, value)`.
    pub fn put_bits(&mut self, buf: &mut [u8], n: u32, value: u32) {
        for k in (0..n).rev() {
            self.put_bit(buf, (value >> k) & 1);
        }
    }

    /// `ff_copy_bits`: `length` bits of `src`, starting at its first bit.
    /// Bits past the end of `src` read as zero (input padding).
    pub fn copy_bits(&mut self, buf: &mut [u8], src: &[u8], length: usize) {
        let mut k = 0;
        if self.count & 7 == 0 {
            while k + 8 <= length {
                buf[self.count >> 3] = *src.get(k >> 3).unwrap_or(&0);
                self.count += 8;
                k += 8;
            }
        }
        while k < length {
            let b = *src.get(k >> 3).unwrap_or(&0);
            self.put_bit(buf, ((b >> (7 - (k & 7))) & 1) as u32);
            k += 1;
        }
    }

    /// `flush_put_bits` on a copy of the context: zero the unused low bits
    /// of the last partial byte.
    pub fn flush(&self, buf: &mut [u8]) {
        if self.count & 7 != 0 {
            buf[self.count >> 3] &= !(0xFFu8 >> (self.count & 7));
        }
    }
}
