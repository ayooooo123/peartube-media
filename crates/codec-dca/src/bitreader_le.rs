// Ported from FFmpeg libavcodec/bitstream_template.h (LE reader) and
// libavcodec/get_bits.h `get_bits_le` (commit 2da55bf). Licensed under
// LGPL-2.1-or-later.

//! Little-endian bit reader, the BITSTREAM_READER_LE variant. Reads bits
//! LSB-first within 64-bit words pulled little-endian from the buffer —
//! what FFmpeg's DTS-LBR decoder (and its LBR VLCs) need. Overreads return
//! zero bits instead of panicking (untrusted input cannot be padded).

pub struct LeBitReader<'a> {
    buf: &'a [u8],
    /// Absolute bit position (FFmpeg's `tell`).
    pos: usize,
    len_bits: usize,
}

impl<'a> LeBitReader<'a> {
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
    pub fn bits_left(&self) -> i32 {
        (self.len_bits.saturating_sub(self.pos)) as i32
    }

    /// `bits_read` (LE): read `n` bits LSB-first; 0 once past the end.
    #[inline]
    pub fn get_bits(&mut self, n: u32) -> u32 {
        let n = n as usize;
        if n == 0 {
            return 0;
        }
        if self.pos + n > self.len_bits {
            self.pos = self.len_bits;
            return 0;
        }
        let v = self.read_at(self.pos, n);
        self.pos += n;
        v
    }

    /// `get_bits_le` up to 32 bits, same as `get_bits` here.
    #[inline]
    pub fn get_bits_long(&mut self, n: u32) -> u32 {
        if n <= 32 {
            self.get_bits(n)
        } else {
            let lo = self.get_bits(32);
            let hi = self.get_bits(n - 32);
            // LE read of >32 bits assembles low word first.
            ((hi as u64) << 32).wrapping_add(lo as u64) as u32
        }
    }

    /// `get_sbits` (LE reader).
    #[inline]
    pub fn get_sbits(&mut self, n: u32) -> i32 {
        let v = self.get_bits(n);
        if n == 0 || n >= 32 {
            return v as i32;
        }
        let sign = 1i32 << (n - 1);
        ((v as i32) & ((sign << 1) - 1)) - (((v as i32) & sign) << 1)
    }

    /// `show_bits` (LE).
    #[inline]
    pub fn show_bits(&self, n: u32) -> u32 {
        let n = n as usize;
        if n == 0 || self.pos + n > self.len_bits {
            return 0;
        }
        self.read_at(self.pos, n)
    }

    /// `skip_bits`.
    #[inline]
    pub fn skip(&mut self, n: u32) {
        self.pos = (self.pos + n as usize).min(self.len_bits);
    }

    /// `get_bits_count`-based seek used by the LBR parser.
    #[inline]
    pub fn seek_bits(&mut self, p: usize) -> bool {
        if p < self.pos || p > self.len_bits {
            return false;
        }
        self.pos = p;
        true
    }

    #[inline]
    fn read_at(&self, start: usize, n: usize) -> u32 {
        // LSB-first within little-endian words.
        let mut v: u32 = 0;
        let mut pos = start;
        for i in 0..n {
            let byte = pos / 8;
            let bit = pos % 8;
            let b = if byte < self.buf.len() {
                (self.buf[byte] >> bit) & 1
            } else {
                0
            };
            v |= (b as u32) << i;
            pos += 1;
        }
        v
    }
}
