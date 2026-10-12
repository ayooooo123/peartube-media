// Variable-length code tables.
//
// Ported from FFmpeg libavcodec/vlc.c (ff_vlc_init_from_lengths) and
// get_bits.h (get_vlc2) (commit 2da55bf), LGPL-2.1-or-later.

//! A VLC as `ff_vlc_init_from_lengths` builds it: codes are handed out in
//! the order the lengths are listed, each the next free code of its length
//! (`code += 1 << (32 - len)`), and decoded as `get_vlc2` on a
//! little-endian reader decodes them (`VLC_INIT_LE`): the code's first bit
//! is the first bit read.

use crate::getbits::GetBitsLe;

pub struct Vlc {
    /// Per code length: (code, symbol), sorted by code.
    by_len: Vec<Vec<(u32, i32)>>,
    max_len: u32,
}

impl Vlc {
    /// `ff_vlc_init_from_lengths(lens, symbols, offset)`: `(symbol, len)`
    /// pairs in code order; a negative length takes its codes out of the
    /// tree without a symbol. `None` for a table FFmpeg rejects.
    pub fn from_lengths(entries: impl IntoIterator<Item = (i32, i32)>, offset: i32) -> Option<Self> {
        let mut by_len: Vec<Vec<(u32, i32)>> = vec![Vec::new(); 33];
        let mut code: u64 = 0;
        let mut max_len = 0;
        for (symbol, len) in entries {
            let bits = len.unsigned_abs();
            if len == 0 {
                continue;
            }
            if bits > 32 || code & ((1u64 << (32 - bits)) - 1) != 0 {
                return None;
            }
            if len > 0 {
                by_len[bits as usize].push(((code >> (32 - bits)) as u32, symbol + offset));
                max_len = max_len.max(bits);
            }
            code += 1u64 << (32 - bits);
            if code > 1 << 32 {
                return None;
            }
        }
        for codes in &mut by_len {
            codes.sort_unstable();
        }
        Some(Self { by_len, max_len })
    }

    /// `get_vlc2`: the next symbol, its code consumed; `None` (FFmpeg's
    /// -1) when the next bits are no code, nothing consumed.
    pub fn read(&self, gb: &mut GetBitsLe) -> Option<i32> {
        let bits = gb.show(self.max_len);
        let mut code = 0u32;
        for len in 1..=self.max_len {
            code = (code << 1) | ((bits >> (len - 1)) & 1);
            let codes = &self.by_len[len as usize];
            if let Ok(i) = codes.binary_search_by_key(&code, |&(c, _)| c) {
                gb.skip(len as usize);
                return Some(codes[i].1);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_follow_the_listed_order() {
        // Listed 1 (1 bit), 0 (2 bits), 2 and 3 (3 bits): codes 0, 10,
        // 110 and 111.
        let vlc = Vlc::from_lengths([(1, 1), (0, 2), (2, 3), (3, 3)], 0).unwrap();
        let read = |byte: u8| vlc.read(&mut GetBitsLe::new(&[byte]));
        assert_eq!(read(0b0), Some(1));
        assert_eq!(read(0b01), Some(0));
        assert_eq!(read(0b011), Some(2));
        assert_eq!(read(0b111), Some(3));
        // A code of the wrong alignment is rejected, as FFmpeg rejects it.
        assert!(Vlc::from_lengths([(0, 2), (1, 1)], 0).is_none());
    }
}
