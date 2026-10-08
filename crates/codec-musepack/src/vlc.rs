// Ported from FFmpeg libavcodec/vlc.c (commit 2da55bf), LGPL-2.1-or-later.
// Copyright (c) 2003-2023 FFmpeg developers.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VlcElem {
    pub sym: i16,
    pub len: i16,
}

#[derive(Clone, Debug, Default)]
pub struct Vlc {
    pub bits: u32,
    pub table: Vec<VlcElem>,
}

#[derive(Clone, Copy, Debug)]
struct VlcCode {
    bits: u8,
    symbol: i16,
    code: u32,
}

fn build_table(table: &mut Vec<VlcElem>, table_nb_bits: u32, codes: &mut [VlcCode]) -> Result<usize, &'static str> {
    if table_nb_bits > 30 {
        return Err("vlc: table too wide");
    }
    let table_size = 1usize << table_nb_bits;
    let table_index = table.len();
    table.resize(table_index + table_size, VlcElem::default());

    let nb_codes = codes.len();
    let mut i = 0;
    while i < nb_codes {
        let n = codes[i].bits as u32;
        let code = codes[i].code;
        let symbol = codes[i].symbol;
        if n <= table_nb_bits {
            let mut j = (code >> (32 - table_nb_bits)) as usize;
            let nb = 1usize << (table_nb_bits - n);
            for _ in 0..nb {
                let e = &mut table[table_index + j];
                if (e.len != 0 || e.sym != 0) && (e.len != n as i16 || e.sym != symbol) {
                    return Err("vlc: incorrect codes");
                }
                e.len = n as i16;
                e.sym = symbol;
                j += 1;
            }
        } else {
            let n = n - table_nb_bits;
            let code_prefix = code >> (32 - table_nb_bits);
            let mut subtable_bits = n;
            codes[i].bits = n as u8;
            codes[i].code = code << table_nb_bits;
            let mut k = i + 1;
            while k < nb_codes {
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
            table[table_index + j].len = -(subtable_bits as i16);
            let index = build_table(table, subtable_bits, &mut codes[i..k])?;
            let index = i16::try_from(index).map_err(|_| "vlc: strange codes")?;
            table[table_index + j].sym = index;
            i = k - 1;
        }
        i += 1;
    }
    for e in &mut table[table_index..table_index + table_size] {
        if e.len == 0 {
            e.sym = -1;
        }
    }
    Ok(table_index)
}

impl Vlc {
    /// `ff_vlc_init_from_lengths`: codes are assigned in table order from
    /// the lengths; a negative length reserves a code without an entry.
    pub fn init_from_lengths(nb_bits: u32, lens: &[i8], symbols: &[i16], offset: i16) -> Result<Vlc, &'static str> {
        let len_max = 32.min(3 * nb_bits);
        let mut buf: Vec<VlcCode> = Vec::with_capacity(lens.len());
        let mut code: u64 = 0;
        for (i, &l) in lens.iter().enumerate() {
            let len: u32 = if l > 0 {
                buf.push(VlcCode { bits: l as u8, symbol: symbols[i].wrapping_add(offset), code: code as u32 });
                l as u32
            } else if l < 0 {
                (-(l as i32)) as u32
            } else {
                continue;
            };
            if len > len_max || code & ((1u64 << (32 - len)) - 1) != 0 {
                return Err("vlc: invalid length");
            }
            code += 1u64 << (32 - len);
            if code > u32::MAX as u64 + 1 {
                return Err("vlc: overdetermined tree");
            }
        }
        let mut table = Vec::new();
        build_table(&mut table, nb_bits, &mut buf)?;
        Ok(Vlc { bits: nb_bits, table })
    }

    /// `build_vlc` from `mpc8.c`: codes_counts gives count of codes of length 1..16.
    /// Lengths are sorted descending (16 down to 1).
    pub fn build_vlc_counts(codes_counts: &[u8; 16], symbols: &[u8], offset: i16) -> Result<Vlc, &'static str> {
        let mut len = [0i8; 256];
        let mut num = 0;
        for i in (1..=16).rev() {
            let cnt = codes_counts[i - 1] as usize;
            for _ in 0..cnt {
                if num >= len.len() {
                    return Err("vlc: too many codes");
                }
                len[num] = i as i8;
                num += 1;
            }
        }
        let nb_bits = if num > 0 { (len[0] as u32).min(9) } else { 0 };
        let syms: Vec<i16> = symbols[..num].iter().map(|&s| s as i16).collect();
        Self::init_from_lengths(nb_bits, &len[..num], &syms, offset)
    }
}
