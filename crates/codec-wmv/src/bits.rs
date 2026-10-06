//! MSB-first bit reader mirroring FFmpeg's `GetBitContext` semantics for the
//! WMV/VC-1 family: reads may overshoot the end (FFmpeg pads with zeros past
//! the end via `AV_INPUT_BUFFER_PADDING_SIZE`), so reads past the end return 0
//! bits and track overread instead of erroring. Decoders check
//! [`BitReader::overread`] at frame boundaries like FFmpeg checks
//! `get_bits_left < 0`.

use oxideav_core::{Error, Result};

pub struct BitReader<'a> {
    data: &'a [u8],
    /// Next byte to load.
    pos: usize,
    /// Bit position within the stream (consumed bits).
    bit: i64,
    /// One 64-bit cache; high `valid` bits are meaningful, MSB-first.
    cache: u64,
    valid: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            bit: 0,
            cache: 0,
            valid: 0,
        }
    }

    #[inline]
    fn fill(&mut self) {
        while self.valid <= 56 {
            let b = if self.pos < self.data.len() {
                self.data[self.pos]
            } else {
                0
            };
            self.pos += 1;
            self.cache |= (b as u64) << (56 - self.valid);
            self.valid += 8;
        }
    }

    /// Bits consumed so far.
    #[inline]
    pub fn position(&self) -> i64 {
        self.bit
    }

    /// Bits remaining in the underlying buffer (may go negative on overread).
    #[inline]
    pub fn bits_left(&self) -> i64 {
        self.data.len() as i64 * 8 - self.bit
    }

    /// Whether reads have passed the end of the buffer.
    #[inline]
    pub fn overread(&self) -> bool {
        self.bit > self.data.len() as i64 * 8
    }

    /// True when fewer than `n` real bits remain.
    #[inline]
    pub fn short(&self, n: i64) -> bool {
        self.bits_left() < n
    }

    /// Read `n` bits (n <= 32) as unsigned, zero-padded past the end.
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

    /// Read `n` bits signed (sign-extended from bit n-1).
    #[inline]
    pub fn read_signed(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        let v = self.read(n) as i32;
        v << (32 - n) >> (32 - n)
    }

    #[inline]
    pub fn read_bit(&mut self) -> u32 {
        self.read(1)
    }

    /// Peek `n` bits (n <= 32) without consuming.
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

    /// Skip `n` bits (tracks position past the end like FFmpeg).
    #[inline]
    pub fn skip(&mut self, n: u32) {
        let mut left = n;
        while left > 0 {
            let chunk = left.min(32);
            let _ = self.read(chunk);
            left -= chunk;
        }
    }

    /// Read a unary code of leading zeros terminated by a 1, capped at
    /// `max` (returns `max` when the cap is reached without the terminator,
    /// matching FFmpeg's `get_unary(gb, stop, len)` with stop==0).
    pub fn read_unary(&mut self, max: u32) -> u32 {
        let mut n = 0;
        loop {
            if self.read_bit() == 1 {
                return n;
            }
            n += 1;
            if n >= max {
                return max;
            }
        }
    }

    /// FFmpeg's `decode012`: 0 -> 0, 10 -> 1, 11 -> 2.
    #[inline]
    pub fn decode012(&mut self) -> Result<u32> {
        if self.read_bit() == 0 {
            return Ok(0);
        }
        Ok(1 + self.read_bit())
    }

    /// FFmpeg's `decode210`: 1 -> 0, 01 -> 1, 00 -> 2.
    #[inline]
    pub fn decode210(&mut self) -> u32 {
        if self.read_bit() != 0 {
            return 0;
        }
        2 - self.read_bit()
    }

    /// FFmpeg's `get_ue_golomb`-free signed VLC used by VC-1 MVDATA with
    /// `k_x`-bit extension: `(value << 1 | sign)` semantics handled by the
    /// caller; this helper reads `n` bits and applies `-` when the top bit
    /// of the pair is set.
    #[inline]
    pub fn check(&self, n: i64) -> Result<()> {
        if self.bits_left() < n {
            return Err(Error::InvalidData(format!(
                "codec-wmv: bitstream overread ({n} bits needed, {} left)",
                self.bits_left()
            )));
        }
        Ok(())
    }
}
