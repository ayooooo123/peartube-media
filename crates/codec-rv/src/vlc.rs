//! VLC decoding tables with FFmpeg's exact layout and construction rules.
//!
//! Ported from FFmpeg libavcodec/vlc.c (`build_table`, `ff_vlc_init_sparse`,
//! `ff_vlc_init_from_lengths`) at commit 2da55bf; LGPL-2.1-or-later.
//!
//! A table is a flat `Vec<VlcElem>`: the root level has `1 << bits` entries;
//! an entry with `len < 0` points (`sym` = offset) to a subtable of
//! `-len` bits. Missing codes have `len == 0, sym == -1`. Decoding is
//! [`crate::bits::BitReader::get_vlc2`].

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VlcElem {
    pub sym: i16,
    pub len: i16,
}

#[derive(Clone, Debug, Default)]
pub struct Vlc {
    /// Root-level index width (`vlc->bits`).
    pub bits: u32,
    pub table: Vec<VlcElem>,
}

#[derive(Clone, Copy, Debug)]
struct VlcCode {
    bits: u8,
    symbol: i16,
    /// Codeword with the first bit to be read in the MSB.
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
    /// `ff_vlc_init_sparse` with big-endian codes. `entries` holds
    /// `(length, code, symbol)` per input code; length-0 entries are skipped.
    pub fn init_sparse(nb_bits: u32, entries: &[(u32, u32, i16)]) -> Result<Vlc, &'static str> {
        let mut buf: Vec<VlcCode> = Vec::with_capacity(entries.len());
        let mut push = |len: u32, code: u32, symbol: i16| -> Result<(), &'static str> {
            if len > 3 * nb_bits || len > 32 {
                return Err("vlc: too long code");
            }
            if (code as u64) >= (1u64 << len) {
                return Err("vlc: invalid code");
            }
            let code = if len == 0 { 0 } else { code << (32 - len) };
            buf.push(VlcCode { bits: len as u8, symbol, code });
            Ok(())
        };
        for &(len, code, sym) in entries {
            if len > nb_bits {
                push(len, code, sym)?;
            }
        }
        // AV_QSORT by code >> 1; the codes of a prefix-free set are distinct.
        buf.sort_unstable_by_key(|c| c.code >> 1);
        let mut short = Vec::new();
        for &(len, code, sym) in entries {
            if len != 0 && len <= nb_bits {
                short.push((len, code, sym));
            }
        }
        let mut push = |len: u32, code: u32, symbol: i16| -> Result<(), &'static str> {
            if (code as u64) >= (1u64 << len) {
                return Err("vlc: invalid code");
            }
            buf.push(VlcCode { bits: len as u8, symbol, code: code << (32 - len) });
            Ok(())
        };
        for (len, code, sym) in short {
            push(len, code, sym)?;
        }
        let mut table = Vec::new();
        build_table(&mut table, nb_bits, &mut buf)?;
        Ok(Vlc { bits: nb_bits, table })
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitReader;

    #[test]
    fn sparse_three_level_decode() {
        // Codes: 0 (1 bit) -> 7, 10 (2) -> 8, 110 (3) -> 9, 1110000000001 (13) -> 10,
        // 1111 (4) -> 11; a 5-bit root puts the 13-bit code three levels deep.
        let entries = [(1, 0b0, 7), (2, 0b10, 8), (3, 0b110, 9), (13, 0b1110000000001, 10), (4, 0b1111, 11)];
        let vlc = Vlc::init_sparse(5, &entries).unwrap();
        // stream: 1110000000001 110 0 1111 10
        let bits = "1110000000001110011111000000000";
        let mut data = vec![0u8; 8];
        for (i, c) in bits.chars().enumerate() {
            if c == '1' {
                data[i / 8] |= 0x80 >> (i % 8);
            }
        }
        let mut gb = BitReader::new(&data, data.len());
        let got: Vec<i32> = (0..5).map(|_| gb.get_vlc2(&vlc.table, 5, 3)).collect();
        assert_eq!(got, vec![10, 9, 7, 11, 8]);
    }

    #[test]
    fn from_lengths_assigns_in_order() {
        let vlc = Vlc::init_from_lengths(3, &[1, 2, 3, 3], &[5, 6, 7, 8], 0).unwrap();
        // codes: 0, 10, 110, 111
        let data = [0b1101_1110u8, 0b0000_0000, 0, 0, 0, 0];
        let mut gb = BitReader::new(&data, data.len());
        let got: Vec<i32> = (0..4).map(|_| gb.get_vlc2(&vlc.table, 3, 1)).collect();
        assert_eq!(got, vec![7, 8, 6, 5]);
    }
}
