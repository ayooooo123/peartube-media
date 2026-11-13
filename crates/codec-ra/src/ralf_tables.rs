//! Compact LZSS-packed RALF Huffman table definitions.
//! Ported from FFmpeg libavcodec/ralfdata.h (commit 2da55bf).
//! License: LGPL-2.1-or-later.

#![forbid(unsafe_code)]

pub const FILTERPARAM_ELEMENTS: usize = 643;
pub const BIAS_ELEMENTS: usize = 255;
pub const CODING_MODE_ELEMENTS: usize = 140;
pub const FILTER_COEFFS_ELEMENTS: usize = 43;
pub const SHORT_CODES_ELEMENTS: usize = 169;
pub const LONG_CODES_ELEMENTS: usize = 441;

const RAW_LEN: usize =
    324 * 3 + 128 * 3 + 72 * 3 + 24 * 11 * 10 * 3 + 88 * 15 * 3 + 224 * 125 * 3;
static PACKED: &[u8] = include_bytes!("ralf_tables_packed.bin");

pub struct RalfRawTables {
    pub filter_param: [[u8; 324]; 3],
    pub bias: [[u8; 128]; 3],
    pub coding_mode: [[u8; 72]; 3],
    pub filter_coeffs: [[[[u8; 24]; 11]; 10]; 3],
    pub short_codes: [[[u8; 88]; 15]; 3],
    pub long_codes: [[[u8; 224]; 125]; 3],
}

// Unpacks the nibble-run RALF codebooks once when RALF VLC sets initialize, then drops cleanly.
pub fn unpack_ralf_tables() -> Box<RalfRawTables> {
    let mut raw = Vec::with_capacity(RAW_LEN);
    let mut i = 0usize;
    while raw.len() < RAW_LEN && i < PACKED.len() {
        let flags = PACKED[i];
        i += 1;
        for bit in 0..8 {
            if raw.len() >= RAW_LEN || i >= PACKED.len() {
                break;
            }
            if (flags & (1 << bit)) != 0 {
                let enc = (u16::from(PACKED[i]) << 8) | u16::from(PACKED[i + 1]);
                i += 2;
                let off = usize::from(enc >> 4) + 1;
                let len = usize::from(enc & 0x0f) + 3;
                let start = raw.len() - off;
                for k in 0..len {
                    let b = raw[start + k];
                    raw.push(b);
                }
            } else {
                raw.push(PACKED[i]);
                i += 1;
            }
        }
    }
    debug_assert_eq!(raw.len(), RAW_LEN);
    let mut pos = 0usize;
    let mut take = |dst: &mut [u8]| {
        dst.copy_from_slice(&raw[pos..pos + dst.len()]);
        pos += dst.len();
    };
    let mut out = Box::new(RalfRawTables {
        filter_param: [[0; 324]; 3],
        bias: [[0; 128]; 3],
        coding_mode: [[0; 72]; 3],
        filter_coeffs: [[[[0; 24]; 11]; 10]; 3],
        short_codes: [[[0; 88]; 15]; 3],
        long_codes: [[[0; 224]; 125]; 3],
    });
    for row in &mut out.filter_param {
        take(row);
    }
    for row in &mut out.bias {
        take(row);
    }
    for row in &mut out.coding_mode {
        take(row);
    }
    for m in &mut out.filter_coeffs {
        for j in m {
            for k in j {
                take(k);
            }
        }
    }
    for m in &mut out.short_codes {
        for j in m {
            take(j);
        }
    }
    for m in &mut out.long_codes {
        for j in m {
            take(j);
        }
    }
    debug_assert_eq!(pos, RAW_LEN);
    out
}
