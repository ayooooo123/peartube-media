// Ported from FFmpeg (commit 2da55bf): libavcodec/get_bits.h
// GNU Lesser General Public License 2.1 or later

use oxideav_core::{Error, Result};

/// An MSB-first bit reader matching FFmpeg's GetBitContext semantics.
#[derive(Clone, Debug)]
pub struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
    total_bits: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            bit_pos: 0,
            total_bits: data.len().saturating_mul(8),
        }
    }

    pub fn with_bit_len(data: &'a [u8], bit_len: usize) -> Self {
        let max_bits = data.len().saturating_mul(8);
        Self {
            data,
            bit_pos: 0,
            total_bits: bit_len.min(max_bits),
        }
    }

    #[inline]
    pub fn bits_count(&self) -> usize {
        self.bit_pos
    }

    #[inline]
    pub fn bits_left(&self) -> usize {
        self.total_bits.saturating_sub(self.bit_pos)
    }

    #[inline]
    pub fn is_eof(&self) -> bool {
        self.bit_pos >= self.total_bits
    }

    #[inline]
    pub fn skip_bits(&mut self, n: usize) -> Result<()> {
        if n > self.bits_left() {
            self.bit_pos = self.total_bits;
            return Err(Error::invalid("bitstream overread"));
        }
        self.bit_pos += n;
        Ok(())
    }

    #[inline]
    pub fn seek_bits(&mut self, pos: usize) -> Result<()> {
        if pos > self.total_bits {
            self.bit_pos = self.total_bits;
            return Err(Error::invalid("bitstream seek beyond end"));
        }
        self.bit_pos = pos;
        Ok(())
    }

    #[inline]
    pub fn align_to_byte(&mut self) {
        let rem = self.bit_pos % 8;
        if rem != 0 {
            let skip = 8 - rem;
            self.bit_pos = (self.bit_pos + skip).min(self.total_bits);
        }
    }

    #[inline]
    pub fn show_bits(&self, n: usize) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        if n > 32 {
            return Err(Error::invalid("cannot show more than 32 bits"));
        }
        if n > self.bits_left() {
            return Err(Error::invalid("bitstream overread"));
        }

        let byte_idx = self.bit_pos / 8;
        let bit_offset = self.bit_pos % 8;

        let mut val = 0u64;
        let bytes_to_read = ((bit_offset + n + 7) / 8).min(self.data.len().saturating_sub(byte_idx));
        for i in 0..bytes_to_read {
            val = (val << 8) | (self.data[byte_idx + i] as u64);
        }

        let total_bits_read = bytes_to_read * 8;
        let shift = total_bits_read - bit_offset - n;
        Ok(((val >> shift) & ((1u64 << n) - 1)) as u32)
    }

    #[inline]
    pub fn get_bits(&mut self, n: usize) -> Result<u32> {
        let val = self.show_bits(n)?;
        self.bit_pos += n;
        Ok(val)
    }

    #[inline]
    pub fn get_bits1(&mut self) -> Result<u32> {
        self.get_bits(1)
    }

    #[inline]
    pub fn get_bool(&mut self) -> Result<bool> {
        Ok(self.get_bits1()? != 0)
    }

    #[inline]
    pub fn get_sbits(&mut self, n: usize) -> Result<i32> {
        if n == 0 {
            return Ok(0);
        }
        let unsigned = self.get_bits(n)?;
        let sign_bit = 1u32 << (n - 1);
        if unsigned & sign_bit != 0 {
            Ok((unsigned as i32) - (1i32 << n))
        } else {
            Ok(unsigned as i32)
        }
    }

    /// Read up to 64 bits.
    pub fn get_bits64(&mut self, n: usize) -> Result<u64> {
        if n <= 32 {
            self.get_bits(n).map(|v| v as u64)
        } else if n <= 64 {
            let high = self.get_bits(n - 32)? as u64;
            let low = self.get_bits(32)? as u64;
            Ok((high << 32) | low)
        } else {
            Err(Error::invalid("cannot read more than 64 bits"))
        }
    }
}
