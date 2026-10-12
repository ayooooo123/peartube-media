// Ported from FFmpeg (commit 2da55bf): libavcodec/dvdec.c (dv_init_static)
// with libavcodec/vlc.c (ff_vlc_init_from_lengths, build_table).
// License: LGPL-2.1-or-later

//! `dv_rl_vlc`: FFmpeg's run-level table for DV AC coefficients, the sign
//! bit folded in. Codes up to TEX_VLC_BITS long are looked up directly;
//! longer ones through a subtable whose entry holds the subtable's offset
//! (in `level`) and the negated count of extra bits to read (in `len8`).

use std::sync::LazyLock;

use crate::tables::{DV_VLC_LEN, DV_VLC_LEVEL, DV_VLC_RUN};

pub const TEX_VLC_BITS: u32 = 10;
/// FF_ARRAY_ELEMS(dv_rl_vlc).
pub const TABLE_SIZE: usize = 1664;

/// RL_VLC_ELEM.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RlVlc {
    pub level: i16,
    pub len8: i8,
    pub run: u8,
}

/// VLCcode.
#[derive(Clone, Copy)]
struct Code {
    bits: i32,
    code: u32,
    symbol: i16,
}

/// VLCElem: (sym, len).
#[derive(Clone, Copy, Default)]
struct Elem {
    sym: i16,
    len: i16,
}

/// build_table: fills a table of `1 << table_nb_bits` entries at the end of
/// `table` from `codes` (sorted by code) and returns its index.
fn build_table(table: &mut Vec<Elem>, table_nb_bits: u32, codes: &mut [Code]) -> usize {
    let table_size = 1usize << table_nb_bits;
    let table_index = table.len();
    table.resize(table_index + table_size, Elem::default());
    let nb = table_nb_bits as i32;
    let mut i = 0;
    while i < codes.len() {
        let Code { bits: n, code, symbol } = codes[i];
        if n <= nb {
            let j = (code >> (32 - table_nb_bits)) as usize;
            for e in &mut table[table_index + j..table_index + j + (1usize << (nb - n))] {
                *e = Elem { sym: symbol, len: n as i16 };
            }
        } else {
            let code_prefix = code >> (32 - table_nb_bits);
            let mut subtable_bits = n - nb;
            codes[i].bits = n - nb;
            codes[i].code = code << table_nb_bits;
            let mut k = i + 1;
            while k < codes.len() {
                let n = codes[k].bits - nb;
                if n <= 0 || codes[k].code >> (32 - table_nb_bits) != code_prefix {
                    break;
                }
                codes[k].bits = n;
                codes[k].code <<= table_nb_bits;
                subtable_bits = subtable_bits.max(n);
                k += 1;
            }
            let subtable_bits = subtable_bits.min(nb);
            let j = table_index + code_prefix as usize;
            table[j].len = -subtable_bits as i16;
            let index = build_table(table, subtable_bits as u32, &mut codes[i..k]);
            table[j].sym = index as i16;
            i = k - 1;
        }
        i += 1;
    }
    for e in &mut table[table_index..table_index + table_size] {
        if e.len == 0 {
            e.sym = -1;
        }
    }
    table_index
}

/// dv_init_static.
fn build() -> Vec<RlVlc> {
    // (len8, run, level): each nonzero level twice, with its sign bit.
    let mut tmp: Vec<(i8, u8, i16)> = Vec::with_capacity(2 * DV_VLC_LEN.len());
    for i in 0..DV_VLC_LEN.len() {
        let (len, run, level) = (DV_VLC_LEN[i] as i8, DV_VLC_RUN[i], i16::from(DV_VLC_LEVEL[i]));
        if level != 0 {
            tmp.push((len + 1, run, level));
            tmp.push((len + 1, run, -level));
        } else {
            tmp.push((len, run, 0));
        }
    }
    // ff_vlc_init_from_lengths: codes assigned in order of the lengths.
    let mut code = 0u64;
    let mut codes: Vec<Code> = Vec::with_capacity(tmp.len());
    for (i, &(len, _, _)) in tmp.iter().enumerate() {
        let len = i32::from(len);
        codes.push(Code { bits: len, code: code as u32, symbol: i as i16 });
        code += 1u64 << (32 - len);
    }
    let mut table = Vec::with_capacity(TABLE_SIZE);
    build_table(&mut table, TEX_VLC_BITS, &mut codes);
    table
        .iter()
        .map(|e| {
            if e.len < 0 {
                // more bits needed
                RlVlc { len8: e.len as i8, level: e.sym, run: 0 }
            } else {
                let (_, run, level) = usize::try_from(e.sym).ok().and_then(|s| tmp.get(s)).copied().unwrap_or((0, 0, 0));
                RlVlc { len8: e.len as i8, level, run: run.wrapping_add(1) }
            }
        })
        .collect()
}

/// The table, built once.
pub static DV_RL_VLC: LazyLock<Vec<RlVlc>> = LazyLock::new(build);

#[cfg(test)]
mod tests {
    use super::*;

    /// FFmpeg asserts the table it builds is 1664 entries with no unused
    /// code (`dv_vlc.table_size == 1664`; partial codes rely on it).
    #[test]
    fn the_code_is_complete_in_1664_entries() {
        assert_eq!(DV_RL_VLC.len(), TABLE_SIZE);
        assert!(DV_RL_VLC.iter().all(|e| e.len8 != 0));
    }
}
