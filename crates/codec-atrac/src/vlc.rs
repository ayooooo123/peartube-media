// Port of `ff_vlc_init_from_lengths` and single-level `get_vlc2`
// (libavcodec/vlc.c, get_bits.h, FFmpeg commit 2da55bf).
// Copyright (c) the FFmpeg developers; LGPL-2.1-or-later (see LICENSE).

use crate::bits::BitReader;

/// A VLC lookup table of `1 << bits` entries, as FFmpeg builds it when no
/// code is longer than the table: each entry holds a symbol and its code
/// length; entries no code covers hold symbol -1 and length 0.
pub(crate) struct Vlc {
    bits: u32,
    table: Vec<(i32, u8)>,
}

impl Vlc {
    /// `ff_vlc_init_from_lengths(vlc, bits, n, lens, ..., symbols, ...,
    /// offset)`: codes are assigned in the given order, each the previous
    /// one plus `2^(32 - len)` in a left-aligned 32-bit code space. Every
    /// length must be at most `bits`.
    pub(crate) fn from_lengths(bits: u32, lens: &[u8], symbols: &[i32]) -> Self {
        debug_assert_eq!(lens.len(), symbols.len());
        let mut table = vec![(-1i32, 0u8); 1usize << bits];
        let mut code: u64 = 0;
        for (&len, &sym) in lens.iter().zip(symbols) {
            if len == 0 {
                continue;
            }
            debug_assert!(u32::from(len) <= bits);
            let first = (code >> (32 - bits)) as usize;
            let count = 1usize << (bits - u32::from(len));
            for entry in &mut table[first..first + count] {
                *entry = (sym, len);
            }
            code += 1u64 << (32 - u32::from(len));
        }
        debug_assert!(code <= 1u64 << 32, "overdetermined VLC");
        Self { bits, table }
    }

    /// `get_vlc2(gb, table, bits, 1)`: the symbol, or -1 (consuming
    /// nothing) for a bit pattern no code starts.
    pub(crate) fn get(&self, br: &mut BitReader) -> i32 {
        let (sym, len) = self.table[br.show(self.bits) as usize];
        br.skip(usize::from(len));
        sym
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_assigned_in_order_and_gaps_read_as_minus_one() {
        // lengths 1, 2, 3: codes 0, 10, 110; 111 is unused.
        let vlc = Vlc::from_lengths(3, &[1, 2, 3], &[7, 8, 9]);
        // codes 0, 10, 110, then 111 (unused), then zero padding
        let data = [0b0101_1011u8, 0b1000_0000];
        let mut br = BitReader::from_bytes(&data);
        assert_eq!(vlc.get(&mut br), 7);
        assert_eq!(vlc.get(&mut br), 8);
        assert_eq!(vlc.get(&mut br), 9);
        assert_eq!(vlc.get(&mut br), -1);
        assert_eq!(br.left(), 16 - 6);
    }
}
