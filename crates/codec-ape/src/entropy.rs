// Ported from FFmpeg libavcodec/apedec.c (commit 2da55bf).
//
// Copyright (c) 2007 Benjamin Zores <ben@geexbox.org>
//   based upon libdemac from Dave Chapman.
// Copyright (c) FFmpeg developers
//
// This file is part of FFmpeg.
// Licensed under the GNU Lesser General Public License 2.1 or later.

/// Fixed probabilities for symbols in Monkey Audio version 3.97.
pub static COUNTS_3970: [u16; 22] = [
    0, 14824, 28224, 39348, 47855, 53994, 58171, 60926,
    62682, 63786, 64463, 64878, 65126, 65276, 65365, 65419,
    65450, 65469, 65480, 65487, 65491, 65493,
];

/// Probability ranges for symbols in Monkey Audio version 3.97.
pub static COUNTS_DIFF_3970: [u16; 21] = [
    14824, 13400, 11124, 8507, 6139, 4177, 2755, 1756,
    1104, 677, 415, 248, 150, 89, 54, 31,
    19, 11, 7, 4, 2,
];

/// Fixed probabilities for symbols in Monkey Audio version 3.98.
pub static COUNTS_3980: [u16; 22] = [
    0, 19578, 36160, 48417, 56323, 60899, 63265, 64435,
    64971, 65232, 65351, 65416, 65447, 65466, 65476, 65482,
    65485, 65488, 65490, 65491, 65492, 65493,
];

/// Probability ranges for symbols in Monkey Audio version 3.98.
pub static COUNTS_DIFF_3980: [u16; 21] = [
    19578, 16582, 12257, 7906, 4576, 2366, 1170, 536,
    261, 119, 65, 31, 19, 10, 6, 3,
    3, 2, 1, 1, 1,
];

pub const MODEL_ELEMENTS: usize = 64;

pub const CODE_BITS: u32 = 32;
pub const TOP_VALUE: u32 = 1 << (CODE_BITS - 1); // 0x8000_0000
pub const EXTRA_BITS: u32 = (CODE_BITS - 2) % 8 + 1; // 7
pub const BOTTOM_VALUE: u32 = TOP_VALUE >> 8; // 0x0080_0000

/// Big-endian MSB-first bit reader over a byte slice.
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    #[inline]
    pub fn bits_left(&self) -> usize {
        let total_bits = self.data.len().saturating_mul(8);
        if self.pos < total_bits {
            total_bits - self.pos
        } else {
            0
        }
    }

    #[inline]
    pub fn skip_bits(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }

    #[inline]
    pub fn get_bits1(&mut self) -> u32 {
        let byte_idx = self.pos / 8;
        let bit_idx = 7 - (self.pos % 8);
        self.pos += 1;
        if byte_idx < self.data.len() {
            ((self.data[byte_idx] >> bit_idx) & 1) as u32
        } else {
            0
        }
    }

    #[inline]
    pub fn get_bits(&mut self, n: usize) -> u32 {
        if n == 0 {
            return 0;
        }
        let mut val = 0u32;
        for _ in 0..n {
            val = (val << 1) | self.get_bits1();
        }
        val
    }

    #[inline]
    pub fn get_unary(&mut self, stop: u32, len: usize) -> u32 {
        let mut i = 0u32;
        while (i as usize) < len && self.bits_left() > 0 {
            let bit = self.get_bits1();
            if bit == stop {
                break;
            }
            i += 1;
        }
        i
    }
}

/// APE range coder used for version >= 3.90.
#[derive(Clone, Copy, Default)]
pub struct APERangecoder {
    pub low: u32,
    pub range: u32,
    pub help: u32,
    pub buffer: u32,
}

impl APERangecoder {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn start_decoding(&mut self, ptr: &mut usize, data: &[u8]) {
        if *ptr < data.len() {
            self.buffer = data[*ptr] as u32;
            *ptr += 1;
        } else {
            self.buffer = 0;
        }
        self.low = self.buffer >> (8 - EXTRA_BITS);
        self.range = 1 << EXTRA_BITS;
    }

    #[inline]
    pub fn dec_normalize(&mut self, ptr: &mut usize, data: &[u8], error: &mut bool) {
        while self.range <= BOTTOM_VALUE {
            self.buffer <<= 8;
            if *ptr < data.len() {
                self.buffer += data[*ptr] as u32;
                *ptr += 1;
            } else {
                *error = true;
            }
            self.low = (self.low << 8) | ((self.buffer >> 1) & 0xFF);
            self.range <<= 8;
        }
    }

    #[inline]
    pub fn decode_culfreq(&mut self, tot_f: u32, ptr: &mut usize, data: &[u8], error: &mut bool) -> u32 {
        self.dec_normalize(ptr, data, error);
        if tot_f == 0 {
            *error = true;
            return 0;
        }
        self.help = self.range / tot_f;
        if self.help == 0 {
            *error = true;
            return 0;
        }
        self.low / self.help
    }

    #[inline]
    pub fn decode_culshift(&mut self, shift: u32, ptr: &mut usize, data: &[u8], error: &mut bool) -> u32 {
        self.dec_normalize(ptr, data, error);
        self.help = self.range >> shift;
        if self.help == 0 {
            *error = true;
            return 0;
        }
        self.low / self.help
    }

    #[inline]
    pub fn decode_update(&mut self, sy_f: u32, lt_f: u32) {
        self.low = self.low.wrapping_sub(self.help.wrapping_mul(lt_f));
        self.range = self.help.wrapping_mul(sy_f);
    }

    #[inline]
    pub fn decode_bits(&mut self, n: u32, ptr: &mut usize, data: &[u8], error: &mut bool) -> u32 {
        let sym = self.decode_culshift(n, ptr, data, error);
        self.decode_update(1, sym);
        sym
    }

    #[inline]
    pub fn get_symbol(
        &mut self,
        counts: &[u16; 22],
        counts_diff: &[u16; 21],
        ptr: &mut usize,
        data: &[u8],
        error: &mut bool,
    ) -> i32 {
        let cf = self.decode_culshift(16, ptr, data, error);
        if cf > 65492 {
            let symbol = (cf as i32).wrapping_sub(65535).wrapping_add(63);
            self.decode_update(1, cf);
            if cf > 65535 {
                *error = true;
            }
            return symbol;
        }
        let mut symbol = 0usize;
        while symbol + 1 < counts.len() && counts[symbol + 1] as u32 <= cf {
            symbol += 1;
        }
        if symbol < counts_diff.len() {
            self.decode_update(counts_diff[symbol] as u32, counts[symbol] as u32);
        } else {
            *error = true;
        }
        symbol as i32
    }
}

/// Rice code state for one channel.
#[derive(Clone, Copy, Default)]
pub struct APERice {
    pub k: u32,
    pub ksum: u32,
}

#[inline]
pub fn update_rice(rice: &mut APERice, x: u32) {
    let lim = if rice.k != 0 { 1 << (rice.k + 4) } else { 0 };
    rice.ksum = rice.ksum.wrapping_add((x + 1) / 2).wrapping_sub((rice.ksum + 16) >> 5);

    if rice.ksum < lim {
        rice.k = rice.k.saturating_sub(1);
    } else if rice.ksum >= (1 << (rice.k + 5)) && rice.k < 24 {
        rice.k += 1;
    }
}

#[inline]
pub fn get_rice_ook(gb: &mut BitReader, k: u32) -> u32 {
    let mut x = gb.get_unary(1, gb.bits_left());
    if k != 0 {
        x = (x << k) | gb.get_bits(k as usize);
    }
    x
}

#[inline]
pub fn get_k(ksum: u32) -> u32 {
    if ksum == 0 {
        0
    } else {
        31 - ksum.leading_zeros() + 1
    }
}

pub fn decode_array_0000(
    gb: &mut BitReader,
    out: &mut [i32],
    rice: &mut APERice,
    blockstodecode: usize,
    error: &mut bool,
) {
    rice.ksum = 0;
    let limit5 = blockstodecode.min(5);
    for i in 0..limit5 {
        out[i] = get_rice_ook(gb, 10) as i32;
        rice.ksum = rice.ksum.wrapping_add(out[i] as u32);
    }
    if blockstodecode <= 5 {
        for i in 0..blockstodecode {
            let val = out[i] as u32;
            out[i] = (((val >> 1) ^ ((val & 1).wrapping_sub(1))) as i32).wrapping_add(1);
        }
        return;
    }

    rice.k = get_k(rice.ksum / 10);
    if rice.k >= 24 {
        return;
    }
    let limit64 = blockstodecode.min(64);
    for i in limit5..limit64 {
        out[i] = get_rice_ook(gb, rice.k) as i32;
        rice.ksum = rice.ksum.wrapping_add(out[i] as u32);
        rice.k = get_k(rice.ksum / ((i as u32 + 1) * 2));
        if rice.k >= 24 {
            return;
        }
    }
    if blockstodecode <= 64 {
        for i in 0..blockstodecode {
            let val = out[i] as u32;
            out[i] = (((val >> 1) ^ ((val & 1).wrapping_sub(1))) as i32).wrapping_add(1);
        }
        return;
    }

    rice.k = get_k(rice.ksum >> 7);
    let mut ksummax = 1u32 << (rice.k + 7);
    let mut ksummin = if rice.k != 0 { 1u32 << (rice.k + 6) } else { 0 };
    for i in limit64..blockstodecode {
        if gb.bits_left() < 1 {
            *error = true;
            return;
        }
        out[i] = get_rice_ook(gb, rice.k) as i32;
        rice.ksum = rice.ksum.wrapping_add(out[i] as u32).wrapping_sub(out[i - 64] as u32);
        while rice.ksum < ksummin {
            rice.k = rice.k.saturating_sub(1);
            ksummin = if rice.k != 0 { ksummin >> 1 } else { 0 };
            ksummax >>= 1;
        }
        while rice.ksum >= ksummax {
            rice.k += 1;
            if rice.k > 24 {
                return;
            }
            ksummax <<= 1;
            ksummin = if ksummin != 0 { ksummin << 1 } else { 128 };
        }
    }

    for i in 0..blockstodecode {
        let val = out[i] as u32;
        out[i] = (((val >> 1) ^ ((val & 1).wrapping_sub(1))) as i32).wrapping_add(1);
    }
}

pub fn ape_decode_value_3860(
    fileversion: i32,
    gb: &mut BitReader,
    rice: &mut APERice,
    error: &mut bool,
) -> i32 {
    let mut overflow = gb.get_unary(1, gb.bits_left());
    if fileversion > 3880 {
        while overflow >= 16 {
            overflow -= 16;
            rice.k += 4;
        }
    }
    let x: u32 = if rice.k == 0 {
        overflow
    } else if rice.k <= 24 {
        (overflow << rice.k) + gb.get_bits(rice.k as usize)
    } else {
        *error = true;
        return 0;
    };
    rice.ksum = rice.ksum.wrapping_add(x).wrapping_sub((rice.ksum + 8) >> 4);
    if rice.ksum < (if rice.k != 0 { 1 << (rice.k + 4) } else { 0 }) {
        rice.k = rice.k.saturating_sub(1);
    } else if rice.ksum >= (1 << (rice.k + 5)) && rice.k < 24 {
        rice.k += 1;
    }
    (((x >> 1) ^ ((x & 1).wrapping_sub(1))) as i32).wrapping_add(1)
}

pub fn ape_decode_value_3900(
    fileversion: i32,
    rc: &mut APERangecoder,
    ptr: &mut usize,
    data: &[u8],
    rice: &mut APERice,
    error: &mut bool,
) -> i32 {
    let mut overflow = rc.get_symbol(&COUNTS_3970, &COUNTS_DIFF_3970, ptr, data, error) as u32;
    let tmpk: u32;
    if overflow == 63 {
        tmpk = rc.decode_bits(5, ptr, data, error);
        overflow = 0;
    } else {
        tmpk = if rice.k < 1 { 0 } else { rice.k - 1 };
    }

    let mut x: u32;
    if tmpk <= 16 || fileversion < 3910 {
        if tmpk > 23 {
            *error = true;
            return 0;
        }
        x = rc.decode_bits(tmpk, ptr, data, error);
    } else if tmpk <= 31 {
        x = rc.decode_bits(16, ptr, data, error);
        let hi = rc.decode_bits(tmpk - 16, ptr, data, error);
        x |= hi << 16;
    } else {
        *error = true;
        return 0;
    }
    x = x.wrapping_add(overflow << tmpk);
    update_rice(rice, x);
    (((x >> 1) ^ ((x & 1).wrapping_sub(1))) as i32).wrapping_add(1)
}

pub fn ape_decode_value_3990(
    rc: &mut APERangecoder,
    ptr: &mut usize,
    data: &[u8],
    rice: &mut APERice,
    error: &mut bool,
) -> i32 {
    let pivot = (rice.ksum >> 5).max(1);
    let mut overflow = rc.get_symbol(&COUNTS_3980, &COUNTS_DIFF_3980, ptr, data, error) as u32;
    if overflow == 63 {
        let hi = rc.decode_bits(16, ptr, data, error);
        let lo = rc.decode_bits(16, ptr, data, error);
        overflow = (hi << 16) | lo;
    }

    let base: u32;
    if pivot < 0x10000 {
        base = rc.decode_culfreq(pivot, ptr, data, error);
        rc.decode_update(1, base);
    } else {
        let mut base_hi = pivot;
        let mut bbits = 0;
        while base_hi & !0xFFFF != 0 {
            base_hi >>= 1;
            bbits += 1;
        }
        let base_hi_dec = rc.decode_culfreq(base_hi + 1, ptr, data, error);
        rc.decode_update(1, base_hi_dec);
        let base_lo = rc.decode_culfreq(1 << bbits, ptr, data, error);
        rc.decode_update(1, base_lo);
        base = (base_hi_dec << bbits) + base_lo;
    }

    let x = base.wrapping_add(overflow.wrapping_mul(pivot));
    update_rice(rice, x);
    (((x >> 1) ^ ((x & 1).wrapping_sub(1))) as i32).wrapping_add(1)
}
