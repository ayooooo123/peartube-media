// Ported from FFmpeg libavcodec/wavpack.c, libavcodec/dsd.c, libswresample/dsd2pcm.c (commit 2da55bf)
// Copyright (c) 2006, 2011 Konstantin Shishkov
// Copyright (c) 2014 Peter Ross
// Copyright (c) 2009, 2011 Sebastian Gesemann
// Copyright (c) 2020 David Bryant
// License: LGPL-2.1-or-later

#![forbid(unsafe_code)]

use std::sync::LazyLock;
use oxideav_core::{Error, Result};

pub const DSD_HTAPS: usize = 48;
pub const DSD_CTABLES: usize = 6;
pub const DSD_FIFOSIZE: usize = 16;
pub const DSD_FIFOMASK: usize = 15;

pub const PTABLE_BITS: usize = 8;
pub const PTABLE_BINS: usize = 1 << PTABLE_BITS;
pub const PTABLE_MASK: usize = PTABLE_BINS - 1;

pub const UP: i32 = 0x010000fe;
pub const DOWN: i32 = 0x00010000;
pub const DECAY: usize = 8;

pub const PRECISION: usize = 20;
pub const VALUE_ONE: i32 = 1 << PRECISION;
pub const PRECISION_USE: usize = 12;

pub const RATE_S: u8 = 20;

pub const MAX_HISTORY_BITS: usize = 5;
pub const MAX_HISTORY_BINS: usize = 1 << MAX_HISTORY_BITS;
pub const MAX_BIN_BYTES: usize = 1280;

pub const HTAPS: [f64; 48] = [
    0.09950731974056658,
    0.09562845727714668,
    0.08819647126516944,
    0.07782552527068175,
    0.06534876523171299,
    0.05172629311427257,
    0.0379429484910187,
    0.02490921351762261,
    0.0133774746265897,
    0.003883043418804416,
    -0.003284703416210726,
    -0.008080250212687497,
    -0.01067241812471033,
    -0.01139427235000863,
    -0.0106813877974587,
    -0.009007905078766049,
    -0.006828859761015335,
    -0.004535184322001496,
    -0.002425035959059578,
    -0.0006922187080790708,
    0.0005700762133516592,
    0.001353838005269448,
    0.001713709169690937,
    0.001742046839472948,
    0.001545601648013235,
    0.001226696225277855,
    0.0008704322683580222,
    0.0005381636200535649,
    0.000266446345425276,
    7.002968738383528e-05,
    -5.279407053811266e-05,
    -0.0001140625650874684,
    -0.0001304796361231895,
    -0.0001189970287491285,
    -9.396247155265073e-05,
    -6.577634378272832e-05,
    -4.07492895872535e-05,
    -2.17407957554587e-05,
    -9.163058931391722e-06,
    -2.017460145032201e-06,
    1.249721855219005e-06,
    2.166655190537392e-06,
    1.930520892991082e-06,
    1.319400334374195e-06,
    7.410039764949091e-07,
    3.423230509967409e-07,
    1.244182214744588e-07,
    3.130441005359396e-08,
];

static CTABLES: LazyLock<[[f64; 256]; 6]> = LazyLock::new(|| {
    let mut tables = [[0.0f64; 256]; 6];
    for e in 0..256 {
        let mut acc = [0.0f64; 6];
        for m in 0..8 {
            let sign = if ((e >> (7 - m)) & 1) != 0 { 1.0f64 } else { -1.0f64 };
            for t in 0..6 {
                acc[t] += sign * HTAPS[t * 8 + m];
            }
        }
        for t in 0..6 {
            tables[5 - t][e] = acc[t];
        }
    }
    tables
});

pub fn get_ctables() -> &'static [[f64; 256]; 6] {
    &CTABLES
}

#[derive(Clone, Debug)]
pub struct DsdContext {
    pub buf: [u8; DSD_FIFOSIZE],
    pub pos: usize,
}

impl Default for DsdContext {
    fn default() -> Self {
        Self {
            buf: [0x69; DSD_FIFOSIZE],
            pos: 0,
        }
    }
}

impl DsdContext {
    pub fn reset(&mut self) {
        self.buf = [0x69; DSD_FIFOSIZE];
        self.pos = 0;
    }
}

pub fn dsd2pcm_translate(
    ctx: &mut DsdContext,
    src: &[u8],
    src_stride: usize,
    samples: usize,
    dst: &mut [f32],
) {
    let ctables = get_ctables();
    let mut pos = ctx.pos;
    let mut buf = ctx.buf;
    let mut src_idx = 0;

    for out_sample in dst.iter_mut().take(samples) {
        if src_idx >= src.len() {
            break;
        }
        buf[pos] = src[src_idx];
        src_idx += src_stride;

        let p_idx = (pos.wrapping_sub(DSD_CTABLES)) & DSD_FIFOMASK;
        buf[p_idx] = buf[p_idx].reverse_bits();

        let mut sum = 0.0f64;
        for i in 0..DSD_CTABLES {
            let a = buf[(pos.wrapping_sub(i)) & DSD_FIFOMASK] as usize;
            let b = buf[(pos.wrapping_sub(DSD_CTABLES * 2 - 1).wrapping_add(i)) & DSD_FIFOMASK]
                as usize;
            sum += ctables[i][a] + ctables[i][b];
        }

        *out_sample = sum as f32;
        pos = (pos + 1) & DSD_FIFOMASK;
    }

    ctx.pos = pos;
    ctx.buf = buf;
}

#[inline(always)]
fn dsd_byte_ready(low: u32, high: u32) -> bool {
    ((low ^ high) & 0xff000000) == 0
}

pub fn init_ptable(table: &mut [i32; 256], rate_i: u8, rate_s: u8) {
    let mut value = 0x808000i32;
    let mut rate = (rate_i as i32) << 8;

    let c_init = (rate + 128) >> 8;
    for _ in 0..c_init {
        value += (DOWN - value) >> DECAY;
    }

    for i in 0..128 {
        table[i] = value;
        table[255 - i] = 0x100ffff - value;

        if value > 0x010000 {
            rate += (rate * (rate_s as i32) + 128) >> 8;
            let c_loop = (rate + 64) >> 7;
            for _ in 0..c_loop {
                value += (DOWN - value) >> DECAY;
            }
        }
    }
}

pub fn wv_dsd_silence(dst: &mut [u8], offset: usize, samples: usize, stride: usize) {
    for i in 0..samples {
        if let Some(slot) = dst.get_mut(offset + i * stride) {
            *slot = 0x69;
        }
    }
}

#[derive(Clone, Copy, Default)]
struct DsdFilters {
    value: i32,
    fltr0: i32,
    fltr1: i32,
    fltr2: i32,
    fltr3: i32,
    fltr4: i32,
    fltr5: i32,
    fltr6: i32,
    factor: i32,
    byte: u32,
}

pub fn wv_unpack_dsd_high(
    data: &[u8],
    samples: usize,
    stereo: bool,
    dst: &mut [u8],
    offset_l: usize,
    offset_r: Option<usize>,
    stride: usize,
    expected_crc: u32,
) -> Result<()> {
    if data.len() < if stereo { 20 } else { 13 } {
        return Err(Error::invalid("insufficient DSD high data"));
    }

    let mut cursor = 0;
    let rate_i = data[cursor];
    cursor += 1;
    let rate_s = data[cursor];
    cursor += 1;

    if rate_s != RATE_S {
        return Err(Error::invalid("invalid rate_s"));
    }

    let mut ptable = [0i32; 256];
    init_ptable(&mut ptable, rate_i, rate_s);

    let num_channels = if stereo { 2 } else { 1 };
    let mut filters = [DsdFilters::default(); 2];

    for channel in 0..num_channels {
        if cursor + 7 > data.len() {
            return Err(Error::invalid("DSD filter truncated"));
        }
        filters[channel].fltr1 = (data[cursor] as i32) << (PRECISION - 8);
        filters[channel].fltr2 = (data[cursor + 1] as i32) << (PRECISION - 8);
        filters[channel].fltr3 = (data[cursor + 2] as i32) << (PRECISION - 8);
        filters[channel].fltr4 = (data[cursor + 3] as i32) << (PRECISION - 8);
        filters[channel].fltr5 = (data[cursor + 4] as i32) << (PRECISION - 8);
        filters[channel].fltr6 = 0;
        let factor = (data[cursor + 5] as i32) | ((data[cursor + 6] as i32) << 8);
        filters[channel].factor = ((factor as u32) << 16) as i32 >> 16;
        cursor += 7;
    }

    if cursor + 4 > data.len() {
        return Err(Error::invalid("DSD value truncated"));
    }
    let mut value = u32::from_be_bytes(data[cursor..cursor + 4].try_into().unwrap());
    cursor += 4;

    let mut high = 0xffff_ffffu32;
    let mut low = 0u32;
    let mut checksum = 0xffff_ffffu32;

    for s in 0..samples {
        let mut bitcount = 8;
        filters[0].value = filters[0].fltr1 - filters[0].fltr5
            + ((filters[0].fltr6.wrapping_mul(filters[0].factor)) >> 2);
        if stereo {
            filters[1].value = filters[1].fltr1 - filters[1].fltr5
                + ((filters[1].fltr6.wrapping_mul(filters[1].factor)) >> 2);
        }

        while bitcount > 0 {
            bitcount -= 1;
            // Channel 0
            {
                let p_idx = ((filters[0].value >> (PRECISION - PRECISION_USE)) as usize) & PTABLE_MASK;
                let pp_val = ptable[p_idx];
                let split = low.wrapping_add(((high - low) >> 8).wrapping_mul((pp_val >> 16) as u32));

                if value <= split {
                    high = split;
                    ptable[p_idx] += (UP - pp_val) >> DECAY;
                    filters[0].fltr0 = -1;
                } else {
                    low = split.wrapping_add(1);
                    ptable[p_idx] += (DOWN - pp_val) >> DECAY;
                    filters[0].fltr0 = 0;
                }

                while dsd_byte_ready(high, low) {
                    if cursor >= data.len() {
                        return Err(Error::invalid("DSD byte underflow"));
                    }
                    value = (value << 8) | (data[cursor] as u32);
                    cursor += 1;
                    high = (high << 8) | 0xff;
                    low <<= 8;
                }

                filters[0].value = filters[0].value.wrapping_add(filters[0].fltr6 * 8);
                filters[0].byte = (filters[0].byte << 1) | ((filters[0].fltr0 & 1) as u32);
                let term1 = ((filters[0].value ^ filters[0].fltr0) >> 31) | 1;
                let term2 = (filters[0].value ^ (filters[0].value - (filters[0].fltr6 * 16))) >> 31;
                filters[0].factor = filters[0].factor.wrapping_add(term1 & term2);

                filters[0].fltr1 += ((filters[0].fltr0 & VALUE_ONE) - filters[0].fltr1) >> 6;
                filters[0].fltr2 += ((filters[0].fltr0 & VALUE_ONE) - filters[0].fltr2) >> 4;
                filters[0].fltr3 += (filters[0].fltr2 - filters[0].fltr3) >> 4;
                filters[0].fltr4 += (filters[0].fltr3 - filters[0].fltr4) >> 4;
                filters[0].value = (filters[0].fltr4 - filters[0].fltr5) >> 4;
                filters[0].fltr5 += filters[0].value;
                filters[0].fltr6 += (filters[0].value - filters[0].fltr6) >> 3;
                filters[0].value = filters[0].fltr1 - filters[0].fltr5
                    + ((filters[0].fltr6.wrapping_mul(filters[0].factor)) >> 2);
            }

            if !stereo {
                continue;
            }

            // Channel 1
            {
                let p_idx = ((filters[1].value >> (PRECISION - PRECISION_USE)) as usize) & PTABLE_MASK;
                let pp_val = ptable[p_idx];
                let split = low.wrapping_add(((high - low) >> 8).wrapping_mul((pp_val >> 16) as u32));

                if value <= split {
                    high = split;
                    ptable[p_idx] += (UP - pp_val) >> DECAY;
                    filters[1].fltr0 = -1;
                } else {
                    low = split.wrapping_add(1);
                    ptable[p_idx] += (DOWN - pp_val) >> DECAY;
                    filters[1].fltr0 = 0;
                }

                while dsd_byte_ready(high, low) {
                    if cursor >= data.len() {
                        return Err(Error::invalid("DSD byte underflow"));
                    }
                    value = (value << 8) | (data[cursor] as u32);
                    cursor += 1;
                    high = (high << 8) | 0xff;
                    low <<= 8;
                }

                filters[1].value = filters[1].value.wrapping_add(filters[1].fltr6 * 8);
                filters[1].byte = (filters[1].byte << 1) | ((filters[1].fltr0 & 1) as u32);
                let term1 = ((filters[1].value ^ filters[1].fltr0) >> 31) | 1;
                let term2 = (filters[1].value ^ (filters[1].value - (filters[1].fltr6 * 16))) >> 31;
                filters[1].factor = filters[1].factor.wrapping_add(term1 & term2);

                filters[1].fltr1 += ((filters[1].fltr0 & VALUE_ONE) - filters[1].fltr1) >> 6;
                filters[1].fltr2 += ((filters[1].fltr0 & VALUE_ONE) - filters[1].fltr2) >> 4;
                filters[1].fltr3 += (filters[1].fltr2 - filters[1].fltr3) >> 4;
                filters[1].fltr4 += (filters[1].fltr3 - filters[1].fltr4) >> 4;
                filters[1].value = (filters[1].fltr4 - filters[1].fltr5) >> 4;
                filters[1].fltr5 += filters[1].value;
                filters[1].fltr6 += (filters[1].value - filters[1].fltr6) >> 3;
                filters[1].value = filters[1].fltr1 - filters[1].fltr5
                    + ((filters[1].fltr6.wrapping_mul(filters[1].factor)) >> 2);
            }
        }

        let byte_l = (filters[0].byte & 0xff) as u8;
        if let Some(slot) = dst.get_mut(offset_l + s * stride) {
            *slot = byte_l;
        }
        checksum = checksum.wrapping_add((checksum << 1).wrapping_add(byte_l as u32));
        filters[0].factor -= (filters[0].factor + 512) >> 10;

        if stereo {
            let byte_r = (filters[1].byte & 0xff) as u8;
            if let Some(off_r) = offset_r {
                if let Some(slot) = dst.get_mut(off_r + s * stride) {
                    *slot = byte_r;
                }
            }
            checksum = checksum.wrapping_add((checksum << 1).wrapping_add(byte_r as u32));
            filters[1].factor -= (filters[1].factor + 512) >> 10;
        }
    }

    if checksum != expected_crc {
        wv_dsd_silence(dst, offset_l, samples, stride);
        if let Some(off_r) = offset_r {
            wv_dsd_silence(dst, off_r, samples, stride);
        }
    }

    Ok(())
}

pub fn wv_unpack_dsd_fast(
    data: &[u8],
    samples: usize,
    stereo: bool,
    dst: &mut [u8],
    offset_l: usize,
    offset_r: Option<usize>,
    stride: usize,
    expected_crc: u32,
) -> Result<()> {
    if data.is_empty() {
        return Err(Error::invalid("empty DSD fast data"));
    }

    let mut cursor = 0;
    let history_bits = data[cursor] as usize;
    cursor += 1;

    if cursor >= data.len() || history_bits > MAX_HISTORY_BITS {
        return Err(Error::invalid("invalid history_bits"));
    }

    let history_bins = 1 << history_bits;
    let max_probability = data[cursor];
    cursor += 1;

    let mut probabilities = vec![[0u8; 256]; history_bins];

    if max_probability < 0xff {
        let mut p0 = 0;
        let mut i = 0;
        while p0 < history_bins && cursor < data.len() {
            let code = data[cursor];
            cursor += 1;
            if code > max_probability {
                let mut zcount = (code - max_probability) as usize;
                while zcount > 0 && p0 < history_bins {
                    probabilities[p0][i] = 0;
                    i += 1;
                    if i == 256 {
                        i = 0;
                        p0 += 1;
                    }
                    zcount -= 1;
                }
            } else if code != 0 {
                probabilities[p0][i] = code;
                i += 1;
                if i == 256 {
                    i = 0;
                    p0 += 1;
                }
            } else {
                break;
            }
        }
        if p0 < history_bins {
            return Err(Error::invalid("probabilities truncated"));
        }
        if cursor < data.len() {
            let b = data[cursor];
            cursor += 1;
            if b != 0 {
                return Err(Error::invalid("invalid DSD probability terminator"));
            }
        }
    } else {
        let needed = history_bins * 256;
        if data.len() - cursor < needed {
            return Err(Error::invalid("probabilities buffer too short"));
        }
        for p0 in 0..history_bins {
            probabilities[p0].copy_from_slice(&data[cursor..cursor + 256]);
            cursor += 256;
        }
    }

    let mut summed_probabilities = vec![[0u32; 256]; history_bins];
    let mut value_lookup = vec![Vec::new(); history_bins];
    let mut total_summed = 0usize;

    for p0 in 0..history_bins {
        let mut sum = 0u32;
        for i in 0..256 {
            sum += probabilities[p0][i] as u32;
            summed_probabilities[p0][i] = sum;
        }
        if sum > 0 {
            total_summed += sum as usize;
            if total_summed > history_bins * MAX_BIN_BYTES {
                return Err(Error::invalid("total summed probabilities overflow"));
            }
            value_lookup[p0].reserve_exact(sum as usize);
            for i in 0..256 {
                let count = probabilities[p0][i] as usize;
                value_lookup[p0].extend(std::iter::repeat(i as u8).take(count));
            }
        }
    }

    if cursor + 4 > data.len() {
        return Err(Error::invalid("DSD fast value truncated"));
    }
    let mut value = u32::from_be_bytes(data[cursor..cursor + 4].try_into().unwrap());
    cursor += 4;

    let mut low = 0u32;
    let mut high = 0xffff_ffffu32;
    let mut checksum = 0xffff_ffffu32;

    let mut p0 = 0usize;
    let mut p1 = 0usize;
    let mut chan = 0usize;

    let total_samples = if stereo { samples * 2 } else { samples };

    for s_idx in 0..total_samples {
        let total_prob = summed_probabilities[p0][255];
        if total_prob == 0 {
            return Err(Error::invalid("zero total probability"));
        }

        let mut mult = (high - low) / total_prob;
        if mult == 0 {
            if cursor + 4 <= data.len() {
                value = u32::from_be_bytes(data[cursor..cursor + 4].try_into().unwrap());
                cursor += 4;
            }
            low = 0;
            high = 0xffff_ffff;
            mult = high / total_prob;
            if mult == 0 {
                return Err(Error::invalid("zero multiplier"));
            }
        }

        let index = ((value - low) / mult) as usize;
        if index >= total_prob as usize || index >= value_lookup[p0].len() {
            return Err(Error::invalid("DSD fast index out of range"));
        }

        let code = value_lookup[p0][index];
        if code > 0 {
            low = low.wrapping_add(summed_probabilities[p0][(code - 1) as usize].wrapping_mul(mult));
        }

        if !stereo {
            if let Some(slot) = dst.get_mut(offset_l + s_idx * stride) {
                *slot = code;
            }
        } else if chan == 1 {
            let sample_idx = s_idx / 2;
            if let Some(off_r) = offset_r {
                if let Some(slot) = dst.get_mut(off_r + sample_idx * stride) {
                    *slot = code;
                }
            }
            chan = 0;
        } else {
            let sample_idx = s_idx / 2;
            if let Some(slot) = dst.get_mut(offset_l + sample_idx * stride) {
                *slot = code;
            }
            chan = 1;
        }

        high = low.wrapping_add((probabilities[p0][code as usize] as u32).wrapping_mul(mult)).wrapping_sub(1);
        checksum = checksum.wrapping_add((checksum << 1).wrapping_add(code as u32));

        if !stereo {
            p0 = (code as usize) & (history_bins - 1);
        } else {
            p0 = p1;
            p1 = (code as usize) & (history_bins - 1);
        }

        while dsd_byte_ready(high, low) && cursor < data.len() {
            value = (value << 8) | (data[cursor] as u32);
            cursor += 1;
            high = (high << 8) | 0xff;
            low <<= 8;
        }
    }

    if checksum != expected_crc {
        wv_dsd_silence(dst, offset_l, samples, stride);
        if let Some(off_r) = offset_r {
            wv_dsd_silence(dst, off_r, samples, stride);
        }
    }

    Ok(())
}

pub fn wv_unpack_dsd_copy(
    data: &[u8],
    samples: usize,
    stereo: bool,
    dst: &mut [u8],
    offset_l: usize,
    offset_r: Option<usize>,
    stride: usize,
    expected_crc: u32,
) -> Result<()> {
    let needed = samples * (if stereo { 2 } else { 1 });
    if data.len() < needed {
        return Err(Error::invalid("DSD copy underflow"));
    }

    let mut cursor = 0;
    let mut checksum = 0xffff_ffffu32;

    for s in 0..samples {
        let byte_l = data[cursor];
        cursor += 1;
        if let Some(slot) = dst.get_mut(offset_l + s * stride) {
            *slot = byte_l;
        }
        checksum = checksum.wrapping_add((checksum << 1).wrapping_add(byte_l as u32));

        if stereo {
            let byte_r = data[cursor];
            cursor += 1;
            if let Some(off_r) = offset_r {
                if let Some(slot) = dst.get_mut(off_r + s * stride) {
                    *slot = byte_r;
                }
            }
            checksum = checksum.wrapping_add((checksum << 1).wrapping_add(byte_r as u32));
        }
    }

    if checksum != expected_crc {
        wv_dsd_silence(dst, offset_l, samples, stride);
        if let Some(off_r) = offset_r {
            wv_dsd_silence(dst, off_r, samples, stride);
        }
    }

    Ok(())
}
