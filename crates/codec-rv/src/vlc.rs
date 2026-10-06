//! VLC decoding tables, ported from FFmpeg libavcodec/vlc.c (commit 2da55bf).
//! License: GNU Lesser General Public License, version 2.1 or later.

#![forbid(unsafe_code)]

use crate::bitread::GetBitContext;

#[derive(Clone, Copy, Default, Debug)]
pub struct VlcElem {
    pub sym: i16,
    pub len: i16,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct RlVlcElem {
    pub level: i16,
    pub len8: i8,
    pub run: u8,
}

#[derive(Clone, Debug, Default)]
pub struct Vlc {
    pub bits: u32,
    pub table: Vec<VlcElem>,
}

#[derive(Clone, Copy, Debug)]
pub struct VlcCode {
    pub bits: u8,
    pub symbol: i32,
    pub code: u32, // MSB-aligned 32-bit
}

impl Vlc {
    pub fn new() -> Self {
        Self { bits: 0, table: Vec::new() }
    }

    fn build_table(
        vlc: &mut Vlc,
        table_nb_bits: u32,
        codes: &mut [VlcCode],
    ) -> Result<usize, String> {
        let table_size = 1usize << table_nb_bits;
        let table_index = vlc.table.len();
        vlc.table.resize(table_index + table_size, VlcElem::default());

        let mut i = 0usize;
        while i < codes.len() {
            let n = codes[i].bits as u32;
            let code = codes[i].code;
            let symbol = codes[i].symbol;

            if n <= table_nb_bits {
                let j0 = (code >> (32 - table_nb_bits)) as usize;
                let nb = 1usize << (table_nb_bits - n);
                for k in 0..nb {
                    let j = table_index + j0 + k;
                    if (vlc.table[j].len != 0 || vlc.table[j].sym != 0)
                        && (vlc.table[j].len != n as i16 || vlc.table[j].sym != symbol as i16)
                    {
                        return Err("incorrect codes".to_string());
                    }
                    vlc.table[j].len = n as i16;
                    vlc.table[j].sym = symbol as i16;
                }
                i += 1;
            } else {
                let n_rem = n - table_nb_bits;
                let code_prefix = code >> (32 - table_nb_bits);
                let mut subtable_bits = n_rem;
                codes[i].bits = n_rem as u8;
                codes[i].code = code << table_nb_bits;

                let mut k = i + 1;
                while k < codes.len() {
                    let n2 = codes[k].bits as u32;
                    let code2 = codes[k].code;
                    if n2 <= table_nb_bits || (code2 >> (32 - table_nb_bits)) != code_prefix {
                        break;
                    }
                    let rem = n2 - table_nb_bits;
                    codes[k].bits = rem as u8;
                    codes[k].code = code2 << table_nb_bits;
                    subtable_bits = subtable_bits.max(rem);
                    k += 1;
                }
                let subtable_bits = subtable_bits.min(table_nb_bits);
                let j = table_index + code_prefix as usize;
                vlc.table[j].len = -(subtable_bits as i16);
                let sub_index = Self::build_table(vlc, subtable_bits, &mut codes[i..k])?;
                vlc.table[j].sym = sub_index as i16;
                i = k;
            }
        }
        Ok(table_index)
    }
}

pub fn vlc_init_from_lengths(
    nb_bits: u32,
    lens: &[i32],
    symbols: Option<&[u16]>,
) -> Result<Vlc, String> {
    let len_max = 3 * nb_bits.min(32);
    let mut codes = Vec::new();
    let mut code: u64 = 0;
    for (i, &l0) in lens.iter().enumerate() {
        let len = if l0 > 0 {
            l0 as u32
        } else if l0 < 0 {
            (-l0) as u32
        } else {
            continue;
        };
        let sym = symbols.map(|s| s[i] as i32).unwrap_or(i as i32);
        if l0 > 0 {
            codes.push(VlcCode {
                bits: len as u8,
                symbol: sym,
                code: code as u32,
            });
        }
        if len > len_max || (code & ((1u64 << (32 - len)) - 1)) != 0 {
            return Err(format!("Invalid VLC (length {len})"));
        }
        code += 1u64 << (32 - len);
        if code > u32::MAX as u64 + 1 {
            return Err("Overdetermined VLC tree".to_string());
        }
    }
    finish_vlc(nb_bits, codes)
}

pub fn vlc_init_sparse(
    nb_bits: u32,
    raw_codes: Vec<(u32, u32, i32)>, // (code, bits, symbol)
) -> Result<Vlc, String> {
    let mut codes = Vec::with_capacity(raw_codes.len());
    for (raw_code, bits, symbol) in raw_codes {
        if bits == 0 {
            continue;
        }
        if bits > 3 * nb_bits || bits > 32 {
            return Err("Too long VLC".to_string());
        }
        if raw_code >= (1u64 << bits) as u32 {
            return Err("Invalid code".to_string());
        }
        let code = raw_code << (32 - bits);
        codes.push(VlcCode {
            bits: bits as u8,
            symbol,
            code,
        });
    }
    finish_vlc(nb_bits, codes)
}
fn finish_vlc(nb_bits: u32, codes: Vec<VlcCode>) -> Result<Vlc, String> {
    let mut long_codes: Vec<VlcCode> = codes.iter().filter(|c| c.bits as u32 > nb_bits).copied().collect();
    long_codes.sort_by_key(|c| c.code >> 1);
    let short_codes: Vec<VlcCode> = codes.iter().filter(|c| (c.bits as u32) <= nb_bits && c.bits > 0).copied().collect();
    let mut all_codes = long_codes;
    all_codes.extend(short_codes);

    let mut vlc = Vlc::new();
    vlc.bits = nb_bits;
    Vlc::build_table(&mut vlc, nb_bits, &mut all_codes)?;
    for e in vlc.table.iter_mut() {
        if e.len == 0 {
            e.sym = -1;
        }
    }
    Ok(vlc)
}

pub fn get_vlc2(gb: &mut GetBitContext, vlc: &Vlc) -> i32 {
    let n = vlc.bits;
    let index = gb.show_bits(n) as usize;
    if index >= vlc.table.len() {
        return -1;
    }
    let mut elem = vlc.table[index];
    if elem.len > 0 {
        gb.skip_bits(elem.len as u32);
        return elem.sym as i32;
    }
    if elem.len < 0 {
        gb.skip_bits(n);
        let sub_bits = (-elem.len) as u32;
        let sub_index = (elem.sym as usize) + gb.show_bits(sub_bits) as usize;
        if sub_index < vlc.table.len() {
            elem = vlc.table[sub_index];
            if elem.len > 0 {
                gb.skip_bits(elem.len as u32);
                return elem.sym as i32;
            }
        }
    }
    -1
}

pub struct RlTable {
    pub n: usize,
    pub last: usize,
    pub table_vlc: Vec<(u32, u8)>,
    pub table_run: Vec<i8>,
    pub table_level: Vec<i8>,
}

pub struct RlVlc {
    pub tables: Vec<Vec<RlVlcElem>>,
    pub bits: u32,
}

impl RlTable {
    pub fn build_rl_vlc(&self) -> Result<RlVlc, String> {
        let mut codes = Vec::with_capacity(self.table_vlc.len());
        for (i, &(code, bits)) in self.table_vlc.iter().enumerate() {
            codes.push((code, bits as u32, i as i32));
        }
        let base = vlc_init_sparse(9, codes)?;
        let static_size = base.table.len();
        let mut tables = Vec::with_capacity(32);
        for q in (0..32).rev() {
            let (qmul, qadd) = if q == 0 { (1, 0) } else { (q * 2, ((q - 1) | 1) as i32) };
            let mut out = vec![RlVlcElem::default(); static_size];
            for i in 0..static_size {
                let idx = base.table[i].sym;
                let len = base.table[i].len;
                let (level, mut run, len8): (i16, u8, i8);
                if len == 0 {
                    run = 66;
                    level = 64;
                    len8 = 0;
                } else if len < 0 {
                    run = 0;
                    level = idx;
                    len8 = len as i8;
                } else {
                    len8 = len as i8;
                    if idx == self.n as i16 {
                        run = 66;
                        level = 0;
                    } else {
                        let idx = idx as usize;
                        run = (self.table_run[idx] + 1) as u8;
                        level = (self.table_level[idx] as i32 * qmul + qadd) as i16;
                        if idx >= self.last {
                            run += 192;
                        }
                    }
                }
                out[i] = RlVlcElem { level, len8, run };
            }
            tables.push(out);
        }
        tables.reverse();
        Ok(RlVlc { tables, bits: 9 })
    }
}

pub fn get_rl_vlc(gb: &mut GetBitContext, rl: &RlVlc) -> (i32, u32) {
    let index = gb.show_bits(rl.bits) as usize;
    let mut elem = rl.tables[0][index];
    if elem.len8 < 0 {
        gb.skip_bits(rl.bits);
        let nb_bits = (-elem.len8) as u32;
        let sub_idx = gb.show_bits(nb_bits) as usize + elem.level as usize;
        if sub_idx < rl.tables[0].len() {
            elem = rl.tables[0][sub_idx];
        }
    }
    if elem.len8 > 0 {
        gb.skip_bits(elem.len8 as u32);
        (elem.level as i32, elem.run as u32)
    } else {
        (0, 66)
    }
}
