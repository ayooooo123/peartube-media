// Ported from FFmpeg libavcodec/vlc.c (ff_vlc_init_from_lengths,
// build_table) and libavcodec/get_bits.h (get_vlc2) at commit 2da55bf.
// Licensed under GNU Lesser General Public License 2.1 or later.
//
//! VLC build + decode for VP3's Huffman tables.
//!
//! VP3's VLCs are built from code-length tables in `tables.rs` (the same
//! way FFmpeg's `ff_vlc_init_from_lengths` builds them from
//! `vp3_bias[..]`, `motion_vector_vlc_table`, ...): codes are assigned
//! in canonical order — one extra bit per entry, entries whose length
//! is negative consume no code. Decoding walks the same lookup tables
//! FFmpeg builds: one first-level table of 2^nb_bits entries, entries
//! whose code is longer than nb_bits pointing at a second-level table
//! indexed by the next `subtable_bits` bits.

use crate::bitread::Gb;
use oxideav_core::Error;

/// One lookup-table entry: a symbol plus the code length behind it.
/// `len < 0` means "descend into the subtable whose offset is `sym`"
/// (FFmpeg's convention: the length field stores `-subtable_bits` and
/// the symbol field the subtable's index).
#[derive(Clone, Copy, Debug, Default)]
struct VlcElem {
    sym: i32,
    len: i32,
}

/// A built VLC: first-level table plus, for long codes, second-level
/// tables appended after it.
pub struct Vlc {
    table: Vec<VlcElem>,
    nb_bits: u32,
}

impl Vlc {
    /// Build a VLC from a code-length table, the symbol being the entry
    /// index plus `offset` (FFmpeg's `ff_vlc_init_from_lengths` with
    /// `symbols == NULL`). Entries of length 0 get no code.
    pub fn from_lengths(lens: &[u8], nb_bits: u32, offset: i32) -> Result<Vlc, Error> {
        let mut codes: Vec<Code> = Vec::new();
        let mut code: u64 = 0;
        let len_max = (3 * nb_bits).min(32);
        for (i, &len) in lens.iter().enumerate() {
            if len == 0 {
                continue;
            }
            let len = u32::from(len);
            if len > len_max || code & ((1u64 << (32 - len)) - 1) != 0 {
                return Err(Error::invalid("vp3: invalid VLC length"));
            }
            codes.push(Code {
                code: ((code >> (32 - len)) as u32) << (32 - len),
                bits: len,
                symbol: i as i32 + offset,
            });
            code += 1u64 << (32 - len);
            if code > 1u64 << 32 {
                return Err(Error::invalid("vp3: overdetermined VLC tree"));
            }
        }
        Self::build(codes, nb_bits)
    }

    /// Build a VLC from `{symbol, length}` byte pairs (`vp3_bias`,
    /// `vp4_bias`, `motion_vector_vlc_table` and `vp4_mv_vlc` are shaped
    /// this way), with `offset` added to each symbol. Entries with
    /// `len == 0` are skipped.
    pub fn from_pairs(pairs: &[[u8; 2]], nb_bits: u32, offset: i32) -> Result<Vlc, Error> {
        let mut codes: Vec<Code> = Vec::new();
        let mut code: u64 = 0;
        let len_max = (3 * nb_bits).min(32);
        for &p in pairs {
            let (symbol, len) = (i32::from(p[0]), i32::from(p[1]));
            if len == 0 {
                continue;
            }
            if len > len_max as i32 || code & ((1u64 << (32 - len)) - 1) != 0 {
                return Err(Error::invalid("vp3: invalid VLC length"));
            }
            codes.push(Code {
                code: ((code >> (32 - len)) as u32) << (32 - len),
                bits: len as u32,
                symbol: symbol + offset,
            });
            code += 1u64 << (32 - len);
            if code > 1u64 << 32 {
                return Err(Error::invalid("vp3: overdetermined VLC tree"));
            }
        }
        Self::build(codes, nb_bits)
    }

    /// Build a VLC from explicit `{code, length}` byte pairs, the symbol
    /// being the entry index (FFmpeg's `ff_vlc_init_tables` without a
    /// symbol table; `vp4_block_pattern_vlc` is shaped this way).
    pub fn from_codes(pairs: &[[u8; 2]], nb_bits: u32) -> Result<Vlc, Error> {
        let mut codes: Vec<Code> = Vec::with_capacity(pairs.len());
        for (i, &[code, len]) in pairs.iter().enumerate() {
            let len = u32::from(len);
            if len == 0 {
                continue;
            }
            if len > 3 * nb_bits || len > 32 || u64::from(code) >= 1u64 << len {
                return Err(Error::invalid("vp3: invalid VLC code"));
            }
            codes.push(Code {
                code: u32::from(code) << (32 - len),
                bits: len,
                symbol: i as i32,
            });
        }
        Self::build(codes, nb_bits)
    }

    /// FFmpeg's `build_table`: first-level table of 2^nb_bits entries;
    /// codes longer than nb_bits share a second-level table per
    /// nb_bits prefix. VP3's longest codes are 15 bits against an
    /// 11-bit first level, so depth 2 always suffices.
    fn build(mut codes: Vec<Code>, nb_bits: u32) -> Result<Vlc, Error> {
        codes.sort_by_key(|c| c.code);
        let table_size = 1usize << nb_bits;
        let mut table = vec![VlcElem::default(); table_size];
        let mut subtables: Vec<VlcElem> = Vec::new();

        let mut i = 0;
        while i < codes.len() {
            let n = codes[i].bits;
            let code = codes[i].code;
            let symbol = codes[i].symbol;
            if n <= nb_bits {
                let j = (code >> (32 - nb_bits)) as usize;
                let nb = 1usize << (nb_bits - n);
                for k in 0..nb {
                    let entry = &mut table[j + k];
                    if (entry.len != 0 || entry.sym != 0)
                        && (entry.len != n as i32 || entry.sym != symbol)
                    {
                        return Err(Error::invalid("vp3: incorrect VLC codes"));
                    }
                    entry.len = n as i32;
                    entry.sym = symbol;
                }
                i += 1;
            } else {
                // Gather every code sharing this nb_bits prefix into a
                // subtable sized by the longest remaining length.
                let prefix = code >> (32 - nb_bits);
                let mut sub_bits = n - nb_bits;
                let mut k = i + 1;
                while k < codes.len() {
                    if codes[k].bits <= nb_bits
                        || codes[k].code >> (32 - nb_bits) != prefix
                    {
                        break;
                    }
                    sub_bits = sub_bits.max(codes[k].bits - nb_bits);
                    k += 1;
                }
                let sub_bits = sub_bits.min(nb_bits);
                let sub_index = table_size + subtables.len();
                subtables.resize(subtables.len() + (1usize << sub_bits), VlcElem::default());
                let span = &mut codes[i..k];
                for c in span.iter_mut() {
                    c.code <<= nb_bits;
                    c.bits -= nb_bits;
                }
                fill_subtable(
                    &mut subtables[sub_index - table_size..],
                    span,
                    sub_bits,
                )?;
                table[prefix as usize].len = -(sub_bits as i32);
                table[prefix as usize].sym = sub_index as i32;
                i = k;
            }
        }
        Ok(Vlc {
            table: [table, subtables].concat(),
            nb_bits,
        })
    }

    /// Decode one symbol (FFmpeg's `get_vlc2` with `max_depth = 2`):
    /// peek nb_bits, index the first-level table, descend into a
    /// second-level table when the entry says so, then consume what is
    /// left of the code. Reads past the end show zero bits like FFmpeg's
    /// padded buffers. An unassigned code is an error where FFmpeg
    /// returns -1; every VP3 table is complete, so none occurs.
    pub fn get(&self, gb: &mut Gb<'_>) -> Result<i32, Error> {
        let idx = gb.show_bits(self.nb_bits) as usize;
        let e = self.table[idx];
        if e.len < 0 {
            let sub_bits = -e.len as u32;
            let sub_index = e.sym as usize;
            gb.skip_bits(self.nb_bits);
            let idx2 = gb.show_bits(sub_bits) as usize;
            let e2 = self.table[sub_index + idx2];
            if e2.len <= 0 {
                return Err(Error::invalid("vp3: invalid VLC code"));
            }
            gb.skip_bits(e2.len as u32);
            return Ok(e2.sym);
        }
        if e.len == 0 {
            return Err(Error::invalid("vp3: invalid VLC code"));
        }
        gb.skip_bits(e.len as u32);
        Ok(e.sym)
    }
}

#[derive(Clone, Copy, Debug)]
struct Code {
    /// Code value, left-aligned in 32 bits (FFmpeg's `VLCcode.code`).
    code: u32,
    /// Code length in bits.
    bits: u32,
    /// Symbol stored for this code.
    symbol: i32,
}

/// Fill one second-level table from its codes (the recursive case of
/// FFmpeg's `build_table`, flattened to depth 2).
fn fill_subtable(table: &mut [VlcElem], codes: &[Code], nb_bits: u32) -> Result<(), Error> {
    for c in codes {
        if c.bits > nb_bits {
            return Err(Error::invalid("vp3: VLC code too long for subtable"));
        }
        let j = (c.code >> (32 - nb_bits)) as usize;
        let nb = 1usize << (nb_bits - c.bits);
        for k in 0..nb {
            let entry = &mut table[j + k];
            if (entry.len != 0 || entry.sym != 0)
                && (entry.len != c.bits as i32 || entry.sym != c.symbol)
            {
                return Err(Error::invalid("vp3: incorrect VLC codes"));
            }
            entry.len = c.bits as i32;
            entry.sym = c.symbol;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete 2-bit table: 00->0, 01->1, 10->2, 11->3.
    #[test]
    fn fixed_codes_roundtrip() {
        let vlc = Vlc::from_lengths(&[2, 2, 2, 2], 2, 0).unwrap();
        let cases: [(&[u8], i32); 4] =
            [(&[0b0000_0000], 0), (&[0b0100_0000], 1), (&[0b1000_0000], 2), (&[0b1100_0000], 3)];
        for (buf, want) in cases {
            let gb = &mut Gb::new(buf);
            assert_eq!(vlc.get(gb).unwrap(), want);
        }
    }

    /// Mixed lengths from a canonical length table; symbol = index.
    #[test]
    fn canonical_lengths() {
        // lengths 1, 2, 3, 3 -> codes 0, 10, 110, 111.
        let vlc = Vlc::from_lengths(&[1, 2, 3, 3], 3, 0).unwrap();
        let cases: [(&[u8], u32, i32); 4] = [
            (&[0b0_0000000], 1, 0),
            (&[0b10_000000], 2, 1),
            (&[0b110_00000], 3, 2),
            (&[0b111_00000], 3, 3),
        ];
        for (buf, consumed, sym) in cases {
            let gb = &mut Gb::new(buf);
            assert_eq!(vlc.get(gb).unwrap(), sym);
            assert_eq!(gb.bit_index, consumed as usize);
        }
    }

    /// from_pairs with an offset (motion_vector_vlc uses offset -31).
    #[test]
    fn pairs_with_offset() {
        let pairs = [[0u8, 1], [1, 2], [2, 3], [3, 3]];
        let vlc = Vlc::from_pairs(&pairs, 3, -31).unwrap();
        let gb = &mut Gb::new(&[0b0_0000000]);
        assert_eq!(vlc.get(gb).unwrap(), -31);
    }

    /// Packs a string of '0'/'1' MSB first.
    fn bits(s: &str) -> Vec<u8> {
        let mut out = vec![0u8; s.len().div_ceil(8)];
        for (i, c) in s.bytes().enumerate() {
            if c == b'1' {
                out[i / 8] |= 0x80 >> (i % 8);
            }
        }
        out
    }

    /// Codes longer than the first level go through a second-level
    /// table; codes of different lengths under one prefix each consume
    /// their own length (vp3_bias tables have 12- and 13-bit codes under
    /// one 11-bit prefix).
    #[test]
    fn long_codes_consume_their_own_length() {
        // Canonical codes: 11-bit 0, 1, 2; 12-bit 0b000000000110;
        // 13-bit 0b0000000001110 and 0b0000000001111. The last three
        // share the 11-bit prefix 3.
        let vlc = Vlc::from_lengths(&[11, 11, 11, 12, 13, 13], 11, 0).unwrap();
        let buf = bits(&["000000000110", "0000000001110", "0000000001111", "00000000010"].concat());
        let gb = &mut Gb::new(&buf);
        for (sym, end) in [(3, 12), (4, 25), (5, 38), (2, 49)] {
            assert_eq!(vlc.get(gb).unwrap(), sym);
            assert_eq!(gb.bit_index, end);
        }
    }

    /// An empty buffer zero-fills like FFmpeg's cache: the first
    /// symbol decodes; the callers' `bits_left` guards catch the
    /// overrun (no panic, no infinite loop).
    #[test]
    fn zero_fill_matches_ffmpeg() {
        let vlc = Vlc::from_lengths(&[1, 2, 3, 3], 3, 0).unwrap();
        let gb = &mut Gb::new(&[]);
        assert_eq!(vlc.get(gb).unwrap(), 0);
        assert_eq!(gb.bits_left(), -1);
    }
}
