// Ported from FFmpeg libavcodec/get_bits.h semantics (commit 2da55bf).
// Licensed under LGPL-2.1-or-later.

//! Big-endian MSB-first bit reader, the subset of `get_bits.h` the DCA
//! decoders use. Bounds-checked: reads past the end return 0 bits (FFmpeg
//! pads its buffers; we cannot pad untrusted input, so overreads degrade
//! to zeros instead of panicking).

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

    /// `get_bits_count`.
    #[inline]
    pub fn bits_read(&self) -> usize {
        self.pos
    }

    /// `get_bits_left`.
    #[inline]
    pub fn bits_left(&self) -> i32 {
        (self.len_bits.saturating_sub(self.pos)) as i32
    }

    #[inline]
    pub fn len_bits(&self) -> usize {
        self.len_bits
    }

    /// `get_bits`: read up to 25 bits MSB-first; 0 when past the end.
    #[inline]
    pub fn get_bits(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 25);
        let n = n as usize;
        if n == 0 {
            return 0;
        }
        if self.pos + n > self.len_bits {
            self.pos = self.len_bits;
            return 0;
        }
        let v = self.read_bits_at(self.pos, n);
        self.pos += n;
        v
    }

    /// `get_bits_long`: up to 32 bits. FFmpeg assembles the high 16 bits
    /// first, then the low `n - 16`.
    #[inline]
    pub fn get_bits_long(&mut self, n: u32) -> u32 {
        if n <= 25 {
            self.get_bits(n)
        } else {
            let hi = self.get_bits(16) as u32;
            let lo = self.get_bits(n - 16) as u32;
            (hi << (n - 16)) | lo
        }
    }

    /// `get_bits64`: up to 64 bits (high word first).
    #[inline]
    pub fn get_bits64(&mut self, n: u32) -> u64 {
        if n <= 32 {
            u64::from(self.get_bits_long(n))
        } else {
            let hi = u64::from(self.get_bits_long(32));
            let lo = u64::from(self.get_bits_long(n - 32));
            (hi << 32) | lo
        }
    }

    /// `show_bits`: like `get_bits` without consuming.
    #[inline]
    pub fn show_bits(&self, n: u32) -> u32 {
        let n = n as usize;
        if n == 0 || self.pos + n > self.len_bits {
            return 0;
        }
        self.read_bits_at(self.pos, n)
    }

    /// `skip_bits`.
    #[inline]
    pub fn skip(&mut self, n: u32) {
        self.pos = (self.pos + n as usize).min(self.len_bits);
    }

    /// `skip_bits_long` (may move backwards).
    #[inline]
    pub fn skip_long(&mut self, n: i32) {
        let pos = self.pos as i64 + i64::from(n);
        self.pos = pos.clamp(0, self.len_bits as i64) as usize;
    }

    /// `get_sbits`: sign-extend the `n`-bit value.
    #[inline]
    pub fn get_sbits(&mut self, n: u32) -> i32 {
        let v = self.get_bits(n);
        if n == 0 || n >= 32 {
            return v as i32;
        }
        let sign = 1i32 << (n - 1);
        ((v as i32) & ((sign << 1) - 1)) - (((v as i32) & sign) << 1)
    }

    /// `get_sbits_long`: sign-extend up to 32 bits.
    #[inline]
    pub fn get_sbits_long(&mut self, n: u32) -> i32 {
        self.get_bits_long(n) as i32
    }

    /// `ff_dca_seek_bits`: fail when seeking backwards or past the buffer.
    /// Returns `false` when the target is unreachable (the C callers log
    /// and return `AVERROR_INVALIDDATA`).
    #[inline]
    pub fn seek_bits(&mut self, p: usize) -> bool {
        if p < self.pos || p > self.len_bits {
            return false;
        }
        self.pos = p;
        true
    }

    /// `align_get_bits`.
    #[inline]
    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
        if self.pos > self.len_bits {
            self.pos = self.len_bits;
        }
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
