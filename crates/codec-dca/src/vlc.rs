// Ported from FFmpeg libavcodec/vlc.c (ff_vlc_init_from_lengths / build_table)
// and libavcodec/bitstream_template.h (read_vlc) (commit 2da55bf).
// Licensed under LGPL-2.1-or-later.

//! Huffman VLC decoding tables built from code lengths, matching FFmpeg's
//! `ff_vlc_init_from_lengths` with `VLC_INIT_STATIC_OVERLONG` (codes are
//! assigned in table order, MSB-first, left-to-right) and the `dca_get_vlc`
//! / `parse_vlc` read paths of the DCA decoders. Big- and little-endian
//! output table variants (`VLC_INIT_LE` for the LBR books) are supported.
//!
//! Reads are bounds-checked: malformed input yields FFmpeg's invalid-code
//! value instead of a panic.

use crate::bitreader::BitReader;
use crate::bitreader_le::LeBitReader;

pub const VLC_INVALID: i32 = -1;

/// One built VLC table: a primary lookup of `2^bits` entries plus auxiliary
/// tables for overlong prefixes. Entry layout mirrors FFmpeg's `VLCElem`
/// (`sym`, `len`; `len < 0` = an auxiliary subtable of `-len` bits whose
/// entry pointer rides in `sym`).
#[derive(Clone)]
pub struct Vlc {
    /// `nb_bits` — the primary lookup width.
    pub bits: u32,
    /// Primary table of `1 << bits` entries: `(symbol, len)`.
    table: Vec<(i32, i8)>,
    /// Auxiliary tables (referenced by index from primary entries).
    aux: Vec<Vec<(i32, i8)>>,
}

struct Code {
    bits: u32,
    code: u32,
    symbol: i32,
}

/// Build a table from `(symbol, code_length)` pairs in code-length order,
/// as `ff_dca_vlc_src_tables` is laid out for each book. `offset` is added
/// to each symbol (`entry_offset` in FFmpeg); `le` selects the LBR books'
/// `VLC_INIT_LE` table layout.
pub fn init(src: &[[u8; 2]], nb_bits: u32, entry_offset: i32, le: bool) -> Vlc {
    // Assign canonical codes in list order (vlc.c ff_vlc_init_from_lengths).
    let mut codes: Vec<Code> = Vec::new();
    let mut code: u64 = 0;
    for entry in src {
        let len = entry[1] as u32;
        if len == 0 {
            continue;
        }
        debug_assert!(len <= 32);
        codes.push(Code {
            bits: len,
            code: code as u32,
            symbol: entry[0] as i32 + entry_offset,
        });
        code += 1u64 << (32 - len);
    }
    build(nb_bits, codes, le)
}

fn build(nb_bits: u32, codes: Vec<Code>, le: bool) -> Vlc {
    let mut aux: Vec<Vec<(i32, i8)>> = Vec::new();
    let table_size = 1usize << nb_bits;
    let mut table = vec![(0i32, 0i8); table_size];

    let mut i = 0usize;
    while i < codes.len() {
        let n = codes[i].bits;
        let code = codes[i].code;
        let symbol = codes[i].symbol;
        if n == 0 {
            i += 1;
            continue;
        }
        if n <= nb_bits {
            // No need for another table.
            let (mut j, inc) = if le {
                (u32::reverse_bits(code) as usize, 1usize << n)
            } else {
                ((code >> (32 - nb_bits)) as usize, 1usize)
            };
            for _ in 0..(1usize << (nb_bits - n)) {
                table[j] = (symbol, n as i8);
                j += inc;
            }
            i += 1;
        } else {
            // Fill an auxiliary table recursively (vlc.c build_table).
            let prefix = if le {
                (u32::reverse_bits(code) & ((1u32 << nb_bits) - 1)) as usize
            } else {
                (code >> (32 - nb_bits)) as usize
            };
            // Collect the run of codes sharing this prefix.
            let mut group: Vec<Code> = Vec::new();
            let mut k = i;
            while k < codes.len() {
                let c = codes[k].code;
                let p = if le {
                    u32::reverse_bits(c) & ((1u32 << nb_bits) - 1)
                } else {
                    c >> (32 - nb_bits)
                } as usize;
                if p != prefix || codes[k].bits <= nb_bits {
                    break;
                }
                group.push(Code {
                    bits: codes[k].bits - nb_bits,
                    code: codes[k].code << nb_bits,
                    symbol: codes[k].symbol,
                });
                k += 1;
            }
            let sub_bits = group.iter().map(|g| g.bits).max().unwrap_or(1).min(nb_bits);
            let sub = build(sub_bits, std::mem::take(&mut group), le);
            debug_assert!(
                sub.aux.is_empty(),
                "DCA books need at most one aux level"
            );
            let sub_index = aux.len() as i32;
            aux.push(sub.table);
            table[prefix] = (sub_index, -(sub_bits as i8));
            i = k;
        }
    }

    for (sym, len) in table.iter_mut() {
        if *len == 0 {
            *sym = VLC_INVALID;
        }
    }

    Vlc {
        bits: nb_bits,
        table,
        aux,
    }
}

/// Peek the primary table at `bits` width over a big-endian reader.
#[inline]
fn lookup<'t>(vlc: &'t Vlc, primary: u32) -> (i32, i32) {
    let idx = primary as usize;
    if idx >= vlc.table.len() {
        return (VLC_INVALID, 0);
    }
    let (sym, len) = vlc.table[idx];
    (sym, len as i32)
}

impl Vlc {
    /// `get_vlc2(gb, vlc->table, bits, max_depth)` on a big-endian reader.
    pub fn get(&self, gb: &mut BitReader, max_depth: u32) -> i32 {
        let (code0, n0) = lookup(self, gb.show_bits(self.bits));
        let (mut code, mut n) = (code0, n0);
        if max_depth > 1 && n < 0 {
            gb.skip(self.bits);
            // Enter auxiliary table: index rides in `code`.
            let nb_bits = -n as u32;
            let sub = &self.aux[code as usize];
            let idx2 = sub_idx(sub, gb.show_bits(nb_bits));
            code = sub[idx2].0;
            n = sub[idx2].1 as i32;
            if max_depth > 2 && n < 0 {
                gb.skip(nb_bits);
                let nb_bits2 = -n as u32;
                let sub2 = &self.aux[code as usize];
                let idx3 = sub_idx(sub2, gb.show_bits(nb_bits2));
                code = sub2[idx3].0;
                let _ = sub2[idx3].1; // terminal: length unused after final lookup
                gb.skip(nb_bits2.max(0) as u32);
                return code;
            }
            gb.skip(nb_bits.max(0) as u32);
            return code;
        }
        gb.skip(n.max(0) as u32);
        code
    }

    /// `read_vlc` (LE reader) — for the LBR books.
    pub fn get_le(&self, gb: &mut LeBitReader, max_depth: u32) -> i32 {
        let mut code: i32;
        let mut n: i32;
        let (c0, l0) = le_lookup(self, gb.show_bits(self.bits));
        code = c0;
        n = l0;
        if max_depth > 1 && n < 0 {
            gb.skip(self.bits);
            let nb_bits = -n as u32;
            let sub = &self.aux[code as usize];
            let idx2 = sub_idx(sub, gb.show_bits(nb_bits));
            code = sub[idx2].0;
            n = sub[idx2].1 as i32;
            if max_depth > 2 && n < 0 {
                gb.skip(nb_bits);
                let nb_bits2 = -n as u32;
                let sub2 = &self.aux[code as usize];
                let idx3 = sub_idx(sub2, gb.show_bits(nb_bits2));
                code = sub2[idx3].0;
                let _ = sub2[idx3].1; // terminal: length unused after final lookup
                gb.skip(nb_bits2.max(0) as u32);
                return code;
            }
            gb.skip(nb_bits.max(0) as u32);
            return code;
        }
        gb.skip(n.max(0) as u32);
        code
    }
}

#[inline]
fn le_lookup(vlc: &Vlc, primary: u32) -> (i32, i32) {
    let idx = primary as usize;
    if idx >= vlc.table.len() {
        return (VLC_INVALID, 0);
    }
    let (sym, len) = vlc.table[idx];
    (sym, len as i32)
}

#[inline]
fn sub_idx(sub: &[(i32, i8)], idx: u32) -> usize {
    (idx as usize).min(sub.len().saturating_sub(1))
}

/// Alias mirroring FFmpeg's `DCA_INIT_VLC` naming for the core books.
pub fn build_src(src: &[[u8; 2]], nb_bits: u32, entry_offset: i32, le: bool) -> Vlc {
    init(src, nb_bits, entry_offset, le)
}

impl std::fmt::Debug for Vlc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vlc").field("bits", &self.bits).finish()
    }
}
