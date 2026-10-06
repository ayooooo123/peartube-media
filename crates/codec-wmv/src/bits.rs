//! MSB-first bit reader with FFmpeg `GetBitContext` semantics: reads past
//! the end of the buffer return zero bits (FFmpeg's zeroed input padding)
//! while the position keeps advancing, so `bits_left()` goes negative
//! exactly like `get_bits_left()`.
//!
//! Ported from FFmpeg commit 2da55bf `libavcodec/get_bits.h`
//! (LGPL-2.1-or-later).

#[derive(Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Next byte to load into the cache.
    pos: usize,
    /// Bits consumed so far.
    bit: i64,
    /// MSB-first cache; the top `valid` bits are meaningful.
    cache: u64,
    valid: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, bit: 0, cache: 0, valid: 0 }
    }

    #[inline]
    fn fill(&mut self) {
        while self.valid <= 56 {
            let b = if self.pos < self.data.len() { self.data[self.pos] } else { 0 };
            self.pos += 1;
            self.cache |= (b as u64) << (56 - self.valid);
            self.valid += 8;
        }
    }

    /// Bits consumed so far (`get_bits_count`).
    #[inline]
    pub fn position(&self) -> i64 {
        self.bit
    }

    /// `get_bits_left` (negative after an overread).
    #[inline]
    pub fn bits_left(&self) -> i64 {
        self.data.len() as i64 * 8 - self.bit
    }

    /// `get_bits(n)` for n <= 32.
    #[inline]
    pub fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        if self.valid < n {
            self.fill();
        }
        let v = (self.cache >> (64 - n)) as u32;
        self.cache <<= n;
        self.valid -= n;
        self.bit += n as i64;
        v
    }

    /// `get_sbits(n)`: `n` bits sign-extended.
    #[inline]
    pub fn read_signed(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        let v = self.read(n) as i32;
        (v << (32 - n)) >> (32 - n)
    }

    #[inline]
    pub fn read_bit(&mut self) -> u32 {
        self.read(1)
    }

    /// `show_bits(n)` for n <= 32.
    #[inline]
    pub fn peek(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        if self.valid < n {
            self.fill();
        }
        (self.cache >> (64 - n)) as u32
    }

    /// `skip_bits_long`.
    #[inline]
    pub fn skip(&mut self, n: u32) {
        let mut left = n;
        while left > 0 {
            let chunk = left.min(32);
            let _ = self.read(chunk);
            left -= chunk;
        }
    }

    /// `decode012`: 0 -> 0, 10 -> 1, 11 -> 2.
    #[inline]
    pub fn decode012(&mut self) -> u32 {
        if self.read_bit() == 0 {
            0
        } else {
            1 + self.read_bit()
        }
    }
}
