//! Multi-level VLC lookup tables with FFmpeg's layout and decode semantics.
//!
//! Ported from FFmpeg commit 2da55bf `libavcodec/vlc.c` (`build_table`,
//! `ff_vlc_init_sparse`, `ff_vlc_init_from_lengths`, `ff_rl_init_vlc`) and
//! the `GET_VLC` / `GET_RL_VLC` readers in `get_bits.h`. LGPL-2.1-or-later.
//!
//! Tables are built exactly like FFmpeg's so that invalid codes behave the
//! same way: an invalid code yields symbol `-1` (or the RL "illegal" marker)
//! after consuming the bits of every table level already walked.

use crate::bits::BitReader;

#[derive(Clone, Copy, Default)]
pub struct VlcElem {
    /// Symbol, or subtable offset when `len < 0`, or -1 when invalid.
    pub sym: i32,
    /// Code length (> 0), -(subtable bits) (< 0), or 0 for an invalid code.
    pub len: i8,
}

pub struct Vlc {
    pub table: Vec<VlcElem>,
    pub bits: u32,
}

#[derive(Clone, Copy)]
struct Code {
    bits: u8,
    /// Left-aligned in 32 bits.
    code: u32,
    sym: i32,
}

impl Vlc {
    /// `ff_vlc_init_sparse`: `codes[i] = (code, length)`; entries with
    /// length 0 are absent. Symbols default to the entry index.
    pub fn new(nb_bits: u32, codes: &[(u32, u8)], syms: Option<&[i32]>) -> Vlc {
        let mut list: Vec<Code> = Vec::with_capacity(codes.len());
        for (i, &(code, len)) in codes.iter().enumerate() {
            if len == 0 {
                continue;
            }
            debug_assert!(len <= 32 && (len == 32 || (code as u64) < (1u64 << len)));
            let sym = syms.map(|s| s[i]).unwrap_or(i as i32);
            list.push(Code { bits: len, code: ((code as u64) << (32 - len as u32)) as u32, sym });
        }
        list.sort_by_key(|c| c.code);
        Self::build_all(nb_bits, &mut list)
    }

    /// `ff_vlc_init_from_lengths`: codes are assigned in table order from
    /// the lengths (a negative length skips code space without a symbol).
    pub fn from_lengths(nb_bits: u32, lens: &[i8], syms: Option<&[i32]>, offset: i32) -> Vlc {
        let mut list: Vec<Code> = Vec::with_capacity(lens.len());
        let mut code: u64 = 0;
        for (i, &l) in lens.iter().enumerate() {
            let len;
            if l > 0 {
                len = l as u32;
                let sym = syms.map(|s| s[i]).unwrap_or(i as i32) + offset;
                list.push(Code { bits: l as u8, code: code as u32, sym });
            } else if l < 0 {
                len = (-l) as u32;
            } else {
                continue;
            }
            code += 1u64 << (32 - len);
        }
        Self::build_all(nb_bits, &mut list)
    }

    fn build_all(nb_bits: u32, codes: &mut [Code]) -> Vlc {
        let mut vlc = Vlc { table: Vec::new(), bits: nb_bits };
        vlc.build(nb_bits, codes);
        vlc
    }

    /// `build_table`; returns the index of the new (sub)table.
    fn build(&mut self, table_nb_bits: u32, codes: &mut [Code]) -> usize {
        let table_size = 1usize << table_nb_bits;
        let table_index = self.table.len();
        self.table.resize(table_index + table_size, VlcElem { sym: 0, len: 0 });
        let mut i = 0;
        while i < codes.len() {
            let n = codes[i].bits as u32;
            let code = codes[i].code;
            let symbol = codes[i].sym;
            if n <= table_nb_bits {
                let j = (code >> (32 - table_nb_bits)) as usize;
                let nb = 1usize << (table_nb_bits - n);
                for k in 0..nb {
                    let e = &mut self.table[table_index + j + k];
                    e.len = n as i8;
                    e.sym = symbol;
                }
                i += 1;
            } else {
                let n = n - table_nb_bits;
                let code_prefix = code >> (32 - table_nb_bits);
                let mut subtable_bits = n;
                codes[i].bits = n as u8;
                codes[i].code = code << table_nb_bits;
                let mut k = i + 1;
                while k < codes.len() {
                    let nk = codes[k].bits as i32 - table_nb_bits as i32;
                    if nk <= 0 {
                        break;
                    }
                    let ck = codes[k].code;
                    if ck >> (32 - table_nb_bits) != code_prefix {
                        break;
                    }
                    codes[k].bits = nk as u8;
                    codes[k].code = ck << table_nb_bits;
                    subtable_bits = subtable_bits.max(nk as u32);
                    k += 1;
                }
                let subtable_bits = subtable_bits.min(table_nb_bits);
                let j = code_prefix as usize;
                self.table[table_index + j].len = -(subtable_bits as i8);
                let index = self.build(subtable_bits, &mut codes[i..k]);
                self.table[table_index + j].sym = index as i32;
                i = k;
            }
        }
        for e in &mut self.table[table_index..table_index + table_size] {
            if e.len == 0 {
                e.sym = -1;
            }
        }
        table_index
    }

    /// `get_vlc2`: decode one symbol; -1 on an invalid code.
    #[inline]
    pub fn get(&self, br: &mut BitReader) -> i32 {
        let mut nb = self.bits;
        let mut e = self.table[br.peek(nb) as usize];
        while e.len < 0 {
            br.skip(nb);
            nb = (-e.len) as u32;
            e = self.table[e.sym as usize + br.peek(nb) as usize];
        }
        br.skip(e.len as u32);
        e.sym
    }
}

/// One entry of an RL-VLC table (`RL_VLC_ELEM`).
#[derive(Clone, Copy, Default)]
pub struct RlVlcElem {
    pub level: i16,
    pub len: i8,
    pub run: u8,
}

/// `RLTable` with its per-qscale RL-VLC tables (`ff_rl_init` +
/// `ff_rl_init_vlc`).
pub struct RlTable {
    /// `max_level[last][run]`.
    pub max_level: [[i8; 65]; 2],
    /// `max_run[last][level]`.
    pub max_run: [[i8; 128]; 2],
    pub vlc: Vlc,
    /// `rl_vlc[q]` for q = 0..32 (same layout as `vlc.table`).
    pub rl_vlc: Vec<Vec<RlVlcElem>>,
}

impl RlTable {
    pub fn new(n: usize, last: usize, table_vlc: &[[u16; 2]], table_run: &[i8], table_level: &[i8]) -> RlTable {
        let mut max_level = [[0i8; 65]; 2];
        let mut max_run = [[0i8; 128]; 2];
        for l in 0..2 {
            let (start, end) = if l == 0 { (0, last) } else { (last, n) };
            for i in start..end {
                let run = table_run[i] as usize;
                let level = table_level[i] as usize;
                if table_level[i] > max_level[l][run] {
                    max_level[l][run] = table_level[i];
                }
                if table_run[i] > max_run[l][level] {
                    max_run[l][level] = table_run[i];
                }
            }
        }
        let codes: Vec<(u32, u8)> = table_vlc.iter().map(|e| (e[0] as u32, e[1] as u8)).collect();
        let vlc = Vlc::new(9, &codes[..n + 1], None);
        let mut rl_vlc = Vec::with_capacity(32);
        for q in 0..32i32 {
            let (qmul, qadd) = if q == 0 { (1, 0) } else { (q * 2, (q - 1) | 1) };
            let mut t = Vec::with_capacity(vlc.table.len());
            for e in &vlc.table {
                let (level, run);
                if e.len == 0 {
                    run = 66;
                    level = 64;
                } else if e.len < 0 {
                    run = 0;
                    level = e.sym;
                } else if e.sym as usize == n {
                    run = 66;
                    level = 0;
                } else {
                    let idx = e.sym as usize;
                    let mut r = table_run[idx] as i32 + 1;
                    if idx >= last {
                        r += 192;
                    }
                    run = r;
                    level = table_level[idx] as i32 * qmul + qadd;
                }
                t.push(RlVlcElem { level: level as i16, len: e.len, run: run as u8 });
            }
            rl_vlc.push(t);
        }
        RlTable { max_level, max_run, vlc, rl_vlc }
    }

    /// `GET_RL_VLC` with `rl_vlc[q]`: returns (level, run, consumed len>=0).
    #[inline]
    pub fn get_rl(&self, q: usize, br: &mut BitReader) -> (i32, i32) {
        let table = &self.rl_vlc[q];
        let mut nb = self.vlc.bits;
        let mut e = table[br.peek(nb) as usize];
        while e.len < 0 {
            br.skip(nb);
            nb = (-e.len) as u32;
            e = table[e.level as usize + br.peek(nb) as usize];
        }
        br.skip(e.len as u32);
        (e.level as i32, e.run as i32)
    }
}
