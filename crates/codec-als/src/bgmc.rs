// Block Gilbert-Moore decoding.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/bgmc.c and bgmc.h, in
// FFmpeg's 32-bit unsigned arithmetic.
// Copyright (c) 2010 Thilo Borgmann; LGPL-2.1-or-later (see LICENSE).

use crate::bgmc_tables::cf_table;
use crate::bits::Bits;

const FREQ_BITS: u32 = 14;
const VALUE_BITS: u32 = 18;
const TOP_VALUE: u32 = (1 << VALUE_BITS) - 1;
const FIRST_QTR: u32 = TOP_VALUE / 4 + 1;
const HALF: u32 = 2 * FIRST_QTR;
const THIRD_QTR: u32 = 3 * FIRST_QTR;
const LUT_BITS: u32 = FREQ_BITS - 8;
const LUT_SIZE: usize = 1 << LUT_BITS;

/// The lookup tables, one set of 16 per `delta` (FFmpeg keeps four and
/// refills them; the entries depend on `delta` only, so keeping all six is
/// the same).
pub(crate) struct Luts {
    tables: [Option<Box<[u8; 16 * LUT_SIZE]>>; 6],
}

impl Luts {
    pub(crate) fn new() -> Self {
        Self { tables: Default::default() }
    }

    /// `bgmc_lut_getp` / `bgmc_lut_fillp`
    fn get(&mut self, delta: u32) -> &[u8; 16 * LUT_SIZE] {
        self.tables[delta as usize].get_or_insert_with(|| {
            let mut lut = Box::new([0u8; 16 * LUT_SIZE]);
            for sx in 0..16 {
                let cf = cf_table(sx);
                for i in 0..LUT_SIZE {
                    let target = ((i as u32) + 1) << (FREQ_BITS - LUT_BITS);
                    let mut symbol = 1u32 << delta;
                    while u32::from(cf[symbol as usize]) > target {
                        symbol += 1 << delta;
                    }
                    lut[sx * LUT_SIZE + i] = (symbol >> delta) as u8;
                }
            }
            lut
        })
    }
}

/// The coder's state.
pub(crate) struct State {
    high: u32,
    low: u32,
    value: u32,
}

/// `ff_bgmc_decode_init`
pub(crate) fn decode_init(gb: &mut Bits) -> Option<State> {
    if gb.left() < i64::from(VALUE_BITS) {
        return None;
    }
    Some(State { high: TOP_VALUE, low: 0, value: gb.get(VALUE_BITS) })
}

/// `ff_bgmc_decode_end`
pub(crate) fn decode_end(gb: &mut Bits) {
    gb.skip(-i64::from(VALUE_BITS - 2));
}

/// `ff_bgmc_decode`: `dst.len()` symbols. `delta` is 0 to 5, `sx` 0 to 15.
pub(crate) fn decode(gb: &mut Bits, dst: &mut [i32], delta: u32, sx: usize, st: &mut State, luts: &mut Luts) {
    let lut = &luts.get(delta)[sx * LUT_SIZE..(sx + 1) * LUT_SIZE];
    let cf = cf_table(sx);
    let (mut high, mut low, mut value) = (st.high, st.low, st.value);
    for d in dst.iter_mut() {
        let range = high.wrapping_sub(low).wrapping_add(1);
        let target = (value.wrapping_sub(low).wrapping_add(1) << FREQ_BITS).wrapping_sub(1) / range.max(1);
        // A valid stream keeps value within [low, high], so target stays
        // below 1 << FREQ_BITS; a damaged one gets the last entry.
        let hint = lut.get((target >> (FREQ_BITS - LUT_BITS)) as usize).copied().unwrap_or(lut[LUT_SIZE - 1]);
        let mut symbol = u32::from(hint) << delta;
        while cf.get(symbol as usize).is_some_and(|&c| u32::from(c) > target) {
            symbol += 1 << delta;
        }
        let symbol = (symbol >> delta).wrapping_sub(1);
        let at = |s: u32| u32::from(cf.get((s << delta) as usize).copied().unwrap_or(0));
        high = low.wrapping_add(range.wrapping_mul(at(symbol)).wrapping_sub(1 << FREQ_BITS) >> FREQ_BITS);
        low = low.wrapping_add(range.wrapping_mul(at(symbol.wrapping_add(1))) >> FREQ_BITS);
        // A valid stream renormalises at most VALUE_BITS times per symbol;
        // damaged state (low past the top) can cycle forever, where FFmpeg
        // would hang, so the loop stops after 64.
        for _ in 0..64 {
            if high >= HALF {
                if low >= HALF {
                    value = value.wrapping_sub(HALF);
                    low = low.wrapping_sub(HALF);
                    high = high.wrapping_sub(HALF);
                } else if low >= FIRST_QTR && high < THIRD_QTR {
                    value = value.wrapping_sub(FIRST_QTR);
                    low -= FIRST_QTR;
                    high -= FIRST_QTR;
                } else {
                    break;
                }
            }
            low = low.wrapping_mul(2);
            high = high.wrapping_mul(2).wrapping_add(1);
            value = value.wrapping_mul(2).wrapping_add(gb.bit());
        }
        *d = symbol as i32;
    }
    *st = State { high, low, value };
}
