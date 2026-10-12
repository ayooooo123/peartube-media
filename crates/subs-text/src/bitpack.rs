//! LSB-first bit reader (Vorbis/libkate bitpack semantics).
//!
//! Kate packs every bit-level field with a Vorbis-style bitpacker: the
//! first bit of a value lands in the least-significant bit of the first
//! byte. All bounds are checked; a read past the end returns
//! `Error::InvalidData` instead of panicking.

use oxideav_core::{Error, Result};

pub struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    pub fn bits_read(&self) -> usize {
        self.bit
    }

    /// True when a read of `n` bits would run past the buffer.
    pub fn would_overread(&self, n: usize) -> bool {
        self.bit + n > self.data.len() * 8
    }

    /// Read `n` bits (n <= 32), LSB-first.
    pub fn read(&mut self, n: usize) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        if n > 32 || self.would_overread(n) {
            return Err(Error::invalid("kate bitstream overread"));
        }
        let mut v: u32 = 0;
        for i in 0..n {
            let byte = self.data[self.bit / 8];
            if (byte >> (self.bit % 8)) & 1 == 1 {
                v |= 1 << i;
            }
            self.bit += 1;
        }
        Ok(v)
    }

    /// Read one bit.
    pub fn read_bit(&mut self) -> Result<bool> {
        Ok(self.read(1)? != 0)
    }

    /// libkate's `kate_read32`: a 32-bit little-endian value.
    pub fn read_u32(&mut self) -> Result<u32> {
        Ok(self.read(32)?)
    }

    /// libkate's `kate_read64`: two 32-bit halves, low first.
    pub fn read_u64(&mut self) -> Result<u64> {
        let lo = self.read_u32()? as u64;
        let hi = self.read_u32()? as u64;
        Ok(lo | (hi << 32))
    }

    /// libkate's `kate_read32v`: a variable-length signed integer.
    /// `0..=14` direct on 4 bits; else 15 + sign + 5-bit (bitlen-1) +
    /// the absolute value.
    pub fn read_32v(&mut self) -> Result<i32> {
        let small = self.read(4)?;
        if small != 15 {
            return Ok(small as i32);
        }
        let sign = self.read_bit()?;
        let bits = self.read(5)? + 1;
        if bits >= 32 {
            return Err(Error::invalid("kate 32v bit count out of range"));
        }
        let v = self.read(bits as usize)? as i32;
        Ok(if sign { -v } else { v })
    }

    /// libkate's `kate_warp` loop: read a 32v bit count; skip that many
    /// bits; repeat until a zero count. Returns the total number of
    /// skipped payload bits.
    pub fn skip_warp(&mut self) -> Result<usize> {
        let mut skipped = 0usize;
        loop {
            let bits = self.read_32v()?;
            if bits < 0 {
                return Err(Error::invalid("kate warp size negative"));
            }
            if bits == 0 {
                return Ok(skipped);
            }
            if self.would_overread(bits as usize) {
                return Err(Error::invalid("kate warp overread"));
            }
            self.bit += bits as usize;
            skipped += bits as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lsb_first() {
        // 0b10000001 = byte with bits 0 and 7 set: reads 1,0,0,0,0,0,0,1
        let mut r = BitReader::new(&[0x81]);
        assert_eq!(r.read_bit().unwrap(), true);
        for _ in 0..6 {
            assert_eq!(r.read_bit().unwrap(), false);
        }
        assert_eq!(r.read_bit().unwrap(), true);
        assert!(r.would_overread(1));
    }

    #[test]
    fn reads_multi_byte_value_lsb_first() {
        // 0x01 0x02: bit stream 1,0...0, 0,1,0... → first 9 bits = 0x102
        let mut r = BitReader::new(&[0x01, 0x01]);
        let v = r.read(9).unwrap();
        assert_eq!(v, 0x101); // bit0 (from 0x01 bit0) + bit8 (from 0x01 bit0)
    }

    #[test]
    fn reads_32v_small_and_large() {
        let mut r = BitReader::new(&[0x07]);
        assert_eq!(r.read_32v().unwrap(), 7);
        let mut r2 = BitReader::new(&[0x0f, 0x04]);
        // 1111 0 00000 1 -> 1
        assert_eq!(r2.read_32v().unwrap(), 1);
    }

    #[test]
    fn overread_is_error_not_panic() {
        let mut r = BitReader::new(&[0x00]);
        assert!(r.read(9).is_err());
    }
}
