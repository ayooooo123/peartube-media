// Ported from FFmpeg (commit 2da55bf): libavcodec/golomb.h
// (get_interleaved_ue_golomb, get_interleaved_se_golomb) and
// libavcodec/get_bits.h (skip_1stop_8data_bits).
// License: LGPL-2.1-or-later

//! SVQ3 bit reading: MSB-first bits, the interleaved Exp-Golomb codes and
//! `skip_1stop_8data_bits`. Every read past the end returns `None`
//! (surfaced as invalid data) instead of reading out of bounds: input
//! comes from untrusted peers.

/// MSB-first bit reader over a borrowed byte slice. Inclusive of the
/// source: all reads are bounds-checked.
pub struct GetBits<'a> {
    buf: &'a [u8],
    /// Next bit position to read from (bit 0 = MSB of `buf[0]`).
    index: usize,
    /// `buf.len() * 8`; the hard limit.
    size: usize,
}

impl<'a> GetBits<'a> {
    /// A reader over `buf`, or `None` for an empty slice (FFmpeg's
    /// `init_get_bits` rejects those too).
    pub fn new(buf: &'a [u8]) -> Option<Self> {
        if buf.is_empty() {
            return None;
        }
        let size = buf.len() * 8;
        Some(Self { buf, index: 0, size })
    }

    /// A reader over `buf` limited to `bit_size` bits.
    pub fn with_size(buf: &'a [u8], bit_size: usize) -> Option<Self> {
        if buf.is_empty() || bit_size > buf.len() * 8 {
            return None;
        }
        Some(Self { buf, index: 0, size: bit_size })
    }

    /// Bits still unread.
    pub fn bits_left(&self) -> usize {
        self.size.saturating_sub(self.index)
    }

    /// Current bit position.
    pub fn position(&self) -> usize {
        self.index
    }

    /// Read `n` bits (n <= 25), MSB-first; `None` past the end.
    pub fn get_bits(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 25, "get_bits supports up to 25 bits");
        if n == 0 {
            return Some(0);
        }
        if self.bits_left() < n as usize {
            self.index = self.size;
            return None;
        }
        let mut acc = 0u32;
        let mut remaining = n as usize;
        let mut pos = self.index;
        while remaining > 0 {
            let byte = self.buf[pos >> 3] as u32;
            let off = pos & 7;
            let take = (8 - off).min(remaining);
            let mask = (1u32 << take) - 1;
            let shift = 8 - off - take;
            acc = (acc << take) | ((byte >> shift) & mask);
            pos += take;
            remaining -= take;
        }
        self.index += n as usize;
        Some(acc)
    }

    /// Read one bit.
    pub fn get_bit(&mut self) -> Option<u32> {
        self.get_bits(1)
    }

    /// Skip `n` bits; `None` past the end (no partial skip).
    pub fn skip(&mut self, n: usize) -> Option<()> {
        if self.bits_left() < n {
            self.index = self.size;
            return None;
        }
        self.index += n;
        Some(())
    }

    /// Show the next `n` bits without consuming; `None` if fewer than
    /// `n` bits remain.
    pub fn show_bits(&self, n: usize) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if self.bits_left() < n {
            return None;
        }
        let mut acc = 0u32;
        let mut remaining = n;
        let mut pos = self.index;
        while remaining > 0 {
            let byte = self.buf[pos >> 3] as u32;
            let off = pos & 7;
            let take = (8 - off).min(remaining);
            let mask = (1u32 << take) - 1;
            let shift = 8 - off - take;
            acc = (acc << take) | ((byte >> shift) & mask);
            pos += take;
            remaining -= take;
        }
        Some(acc)
    }

    /// The whole buffer.
    pub fn data(&self) -> &'a [u8] {
        self.buf
    }

    /// Byte offset of the current bit position (rounds down).
    pub fn byte_index(&self) -> usize {
        self.index >> 3
    }
}

/// get_interleaved_ue_golomb: from 1, each `0` marker bit appends the
/// data bit after it, a `1` marker ends the code; the value less one.
/// FFmpeg stops growing the value at 2^27 (it reads the code a byte at a
/// time); a code still running there is corrupt, so this returns None.
pub fn get_interleaved_ue_golomb(gb: &mut GetBits) -> Option<u32> {
    let mut value = 1u32;
    while gb.get_bit()? == 0 {
        value = (value << 1) | gb.get_bit()?;
        if value >= 0x800_0000 {
            return None;
        }
    }
    Some(value - 1)
}

/// get_interleaved_se_golomb: the interleaved code's value v as v/2 when
/// even, -(v-1)/2 when odd.
pub fn get_interleaved_se_golomb(gb: &mut GetBits) -> Option<i32> {
    let v = i64::from(get_interleaved_ue_golomb(gb)?) + 1;
    Some((if v & 1 == 0 { v / 2 } else { -(v - 1) / 2 }) as i32)
}

/// FFmpeg's `skip_1stop_8data_bits`: while the next bit is 1, skip 8
/// bits. `None` when the stream ends mid-structure.
pub fn skip_1stop_8data_bits(gb: &mut GetBits) -> Option<()> {
    if gb.bits_left() == 0 {
        return None;
    }
    while gb.get_bit()? == 1 {
        gb.skip(8)?;
        if gb.bits_left() == 0 {
            return None;
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_ue_known_codewords() {
        // From FFmpeg's interleaved code: 1=0b1, 3=0b001, 2=0b011.
        let mut gb = GetBits::new(&[0b1000_0000]).unwrap();
        assert_eq!(get_interleaved_ue_golomb(&mut gb), Some(0));
        let mut gb = GetBits::new(&[0b0010_0000]).unwrap();
        assert_eq!(get_interleaved_ue_golomb(&mut gb), Some(1));
        let mut gb = GetBits::new(&[0b0110_0000]).unwrap();
        assert_eq!(get_interleaved_ue_golomb(&mut gb), Some(2));
    }

    #[test]
    fn interleaved_se_zigzag() {
        // ue 1 -> +1, ue 2 -> -1, ue 3 -> +2, ue 4 -> -2.
        for (bits, v) in [(0b0010_0000u8, 1i32), (0b0110_0000, -1), (0b0000_1000, 2), (0b0001_1000, -2)] {
            let data = [bits];
            let mut gb = GetBits::new(&data).unwrap();
            assert_eq!(get_interleaved_se_golomb(&mut gb), Some(v));
        }
    }

    #[test]
    fn reads_end_cleanly() {
        let mut gb = GetBits::new(&[0b1010_1010, 0b0101_0101]).unwrap();
        assert_eq!(gb.get_bits(16), Some(0b1010_1010_0101_0101));
        assert_eq!(gb.get_bits(1), None);
    }

    #[test]
    fn skip_1stop_walks_extension_bytes() {
        // 1 <8 bits> 0  = stop after one extension byte.
        let mut gb = GetBits::new(&[0b1000_0000, 0b0011_1111]).unwrap();
        assert!(skip_1stop_8data_bits(&mut gb).is_some());
        // Truncated: stop bit itself missing.
        let mut gb = GetBits::new(&[0b1111_1111]).unwrap();
        assert!(skip_1stop_8data_bits(&mut gb).is_none());
    }
}
