// MPEG-4 ALS decoder.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/alsdec.c, with
// libavutil/softfloat_ieee754.h for floating-point streams, and decode.c's
// calling pattern (a packet decoded until its bytes are taken).
// Copyright (c) 2009 Thilo Borgmann <thilo.borgmann _at_ mail.de>
// (alsdec.c), (c) 2016 Umair Khan (softfloat_ieee754.h);
// LGPL-2.1-or-later (see LICENSE).

use std::collections::VecDeque;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

use crate::bgmc::{self, Luts};
use crate::bits::{Bits, av_ceil_log2, av_log2};
use crate::config::{Config, read_specific_config};
use crate::mlz::Mlz;

/// `parcor_rice_table`: (offset, Rice parameter) per coefficient.
const PARCOR_RICE_TABLE: [[(i32, u32); 20]; 3] = [
    [(-52, 4), (-29, 5), (-31, 4), (19, 4), (-16, 4), (12, 3), (-7, 3), (9, 3), (-5, 3), (6, 3),
     (-4, 3), (3, 3), (-3, 2), (3, 2), (-2, 2), (3, 2), (-1, 2), (2, 2), (-1, 2), (2, 2)],
    [(-58, 3), (-42, 4), (-46, 4), (37, 5), (-36, 4), (29, 4), (-29, 4), (25, 4), (-23, 4), (20, 4),
     (-17, 4), (16, 4), (-12, 4), (12, 3), (-10, 4), (7, 3), (-4, 4), (3, 3), (-1, 3), (1, 3)],
    [(-59, 3), (-45, 5), (-50, 4), (38, 4), (-39, 4), (32, 4), (-30, 4), (25, 3), (-23, 3), (20, 3),
     (-20, 3), (16, 3), (-13, 3), (10, 3), (-7, 3), (3, 3), (0, 3), (-1, 3), (2, 3), (-1, 2)],
];

/// `parcor_scaled_values`: `(32 + ((i * (i + 1)) << 7) - (1 << 20)) / 32`.
fn parcor_scaled_value(i: usize) -> i32 {
    let i = i as i32;
    (32 + ((i * (i + 1)) << 7) - (1 << 20)) / 32
}

/// `ltp_gain_values`
const LTP_GAIN_VALUES: [[i32; 4]; 4] = [[0, 8, 16, 24], [32, 40, 48, 56], [64, 70, 76, 82], [88, 92, 96, 100]];

/// `mcc_weightings`
const MCC_WEIGHTINGS: [i32; 32] = [
    204, 192, 179, 166, 153, 140, 128, 115, 102, 89, 76, 64, 51, 38, 25, 12, 0, -12, -25, -38, -51, -64, -76, -89,
    -102, -115, -128, -140, -153, -166, -179, -192,
];

/// `tail_code`
const TAIL_CODE: [[u32; 6]; 16] = [
    [74, 44, 25, 13, 7, 3],
    [68, 42, 24, 13, 7, 3],
    [58, 39, 23, 13, 7, 3],
    [126, 70, 37, 19, 10, 5],
    [132, 70, 37, 20, 10, 5],
    [124, 70, 38, 20, 10, 5],
    [120, 69, 37, 20, 11, 5],
    [116, 67, 37, 20, 11, 5],
    [108, 66, 36, 20, 10, 5],
    [102, 62, 36, 20, 10, 5],
    [88, 58, 34, 19, 10, 5],
    [162, 89, 49, 25, 13, 7],
    [156, 87, 49, 26, 14, 7],
    [150, 86, 47, 26, 14, 7],
    [142, 84, 47, 26, 14, 7],
    [131, 79, 46, 26, 14, 7],
];

const RA_FLAG_FRAMES: u32 = 1;

fn invalid(what: &str) -> Error {
    Error::invalid(format!("als: {what}"))
}

/// `decode_rice`
fn decode_rice(gb: &mut Bits, k: u32) -> i32 {
    let max = gb.left() - i64::from(k);
    let mut q = gb.unary0(max);
    let r = if k > 0 { gb.bit() != 0 } else { q & 1 == 0 };
    if k > 1 {
        q <<= k - 1;
        q = q.wrapping_add(gb.get_long(k - 1));
    } else if k == 0 {
        q >>= 1;
    }
    if r { q as i32 } else { !q as i32 }
}

/// `(MUL64(a, b) + (1 << 19)) >> 20`
#[inline]
fn mul_round20(a: i32, b: i32) -> i64 {
    (i64::from(a) * i64::from(b)).wrapping_add(1 << 19) >> 20
}

/// `parcor_to_lpc`
fn parcor_to_lpc(k: usize, par: &[i32], cof: &mut [i32]) {
    let mut i = 0isize;
    let mut j = k as isize - 1;
    while i < j {
        let tmp1 = mul_round20(par[k], cof[j as usize]) as u32;
        cof[j as usize] = (i64::from(cof[j as usize]) + mul_round20(par[k], cof[i as usize])) as i32;
        cof[i as usize] = (cof[i as usize] as u32).wrapping_add(tmp1) as i32;
        i += 1;
        j -= 1;
    }
    if i == j {
        cof[i as usize] = (i64::from(cof[i as usize]) + mul_round20(par[k], cof[j as usize])) as i32;
    }
    cof[k] = par[k];
}

/// `parse_bs_info`: the depth of each block of the partition.
fn parse_bs_info(bs_info: u32, n: u32, div: u32, out: &mut Vec<u32>) {
    if n < 31 && (bs_info << n) & 0x4000_0000 != 0 {
        parse_bs_info(bs_info, 2 * n + 1, div + 1, out);
        parse_bs_info(bs_info, 2 * n + 2, div + 1, out);
    } else {
        out.push(div);
    }
}

/// `SoftFloat_IEEE754`
#[derive(Clone, Copy, PartialEq, Eq)]
struct SoftFloat {
    sign: i32,
    mant: u64,
    exp: i32,
}

const FLOAT_0: SoftFloat = SoftFloat { sign: 0, mant: 0, exp: -126 };
const FLOAT_1: SoftFloat = SoftFloat { sign: 0, mant: 0, exp: 0 };

impl SoftFloat {
    /// `av_normalize_sf_ieee754`
    fn normalize(mut self) -> Self {
        while self.mant >= 0x100_0000 {
            self.exp = self.exp.wrapping_add(1);
            self.mant >>= 1;
        }
        self.mant &= 0x7F_FFFF;
        self
    }

    /// `av_int2sf_ieee754`
    fn from_int(n: i64, e: i32) -> Self {
        let (sign, n) = if n < 0 { (1, n.wrapping_neg()) } else { (0, n) };
        SoftFloat { sign, mant: (n as u64) << 23, exp: e }.normalize()
    }

    /// `av_bits2sf_ieee754`
    fn from_bits(n: u32) -> Self {
        SoftFloat { sign: (n >> 31) as i32, mant: u64::from(n & 0x7F_FFFF), exp: i32::from(((n & 0x7F80_0000) >> 23) as u8 as i8) }
    }

    /// `av_div_sf_ieee754`
    fn div(self, b: Self) -> Self {
        let a = self.normalize();
        let b = b.normalize();
        let mant = (((a.mant | 0x80_0000) << 23) / (b.mant | 0x80_0000)) as i32;
        SoftFloat { sign: a.sign ^ b.sign, mant: mant as i64 as u64, exp: a.exp.wrapping_sub(b.exp) }.normalize()
    }

    /// `av_cmp_sf_ieee754`
    fn same(self, b: Self) -> bool {
        let (a, b) = (self.normalize(), b.normalize());
        a.sign == b.sign && a.mant == b.mant && a.exp == b.exp
    }
}

/// alsdec.c's `multiply`, with its rounding (and its sign bit) as it is.
fn multiply(a: SoftFloat, b: SoftFloat) -> SoftFloat {
    let sign = a.sign ^ b.sign;
    let mut temp = a.mant.wrapping_mul(b.mant);
    let mut mask: u64 = 1 << 47;
    if temp == 0 {
        return FLOAT_0;
    }
    let mut bit_count: i32 = 48;
    while temp & mask == 0 && mask != 0 {
        bit_count -= 1;
        mask >>= 1;
    }
    let cutoff = bit_count - 24;
    if cutoff > 0 {
        let low = temp as u32;
        let last_2_bits = (low >> (cutoff - 1)) & 0x3;
        let rest = u64::from(low) & ((1u64 << (cutoff - 1)) - 1);
        if last_2_bits == 0x3 || (last_2_bits == 0x1 && rest != 0) {
            temp = temp.wrapping_add(1u64 << cutoff);
        }
    }
    let mut mantissa = if cutoff >= 0 { (temp >> cutoff) as u32 } else { (temp << -cutoff) as u32 };
    if mantissa & 0x0100_0000 != 0 {
        bit_count += 1;
        mantissa >>= 1;
    }
    let mut ret: u32 = if sign == 0 { 0x8000_0000 } else { 0 };
    let e = a.exp.wrapping_add(b.exp).wrapping_add(bit_count).wrapping_sub(47).clamp(-126, 127);
    ret |= ((e as u32) << 23) & 0x7F80_0000;
    ret |= mantissa;
    SoftFloat::from_bits(ret)
}

/// Per prediction buffer (`num_buffers`: one, or one per channel for
/// multichannel coding).
#[derive(Clone)]
struct Buffer {
    const_block: bool,
    shift_lsbs: u32,
    opt_order: u32,
    store_prev_samples: bool,
    use_ltp: bool,
    ltp_lag: i32,
    ltp_gain: [i32; 5],
    quant_cof: Vec<i32>,
    lpc_cof: Vec<i32>,
}

/// `ALSChannelData`
#[derive(Clone, Copy, Default)]
struct ChannelData {
    stop_flag: bool,
    master_channel: usize,
    time_diff_flag: bool,
    time_diff_sign: bool,
    time_diff_index: i64,
    weighting: [i32; 6],
}

/// `ALSBlockData`: positions are indices into the raw sample buffer.
#[derive(Clone, Copy)]
struct Block {
    block_length: usize,
    ra_block: bool,
    js_blocks: bool,
    buf: usize,
    raw: usize,
    raw_other: Option<usize>,
}

/// The floating-point state.
struct FloatState {
    acf: Vec<SoftFloat>,
    last_acf_mantissa: Vec<u32>,
    shift_value: Vec<u32>,
    last_shift_value: Vec<u32>,
    raw_mantissa: Vec<Vec<u32>>,
    larray: Vec<u8>,
    nbits: Vec<u32>,
    mlz: Mlz,
}

/// The `mp4als` decoder (`ALSDecContext`).
pub struct AlsDecoder {
    codec_id: CodecId,
    c: Config,
    bits_per_raw_sample: u32,
    s_max: u32,
    ltp_lag_length: u32,
    cur_frame_length: usize,
    frame_id: u32,
    js_switch: bool,
    num_blocks: usize,
    highest_decoded_channel: i64,
    buffers: Vec<Buffer>,
    lpc_cof_reversed: Vec<i32>,
    chan_data: Vec<ChannelData>,
    reverted: Vec<bool>,
    prev_raw_samples: Vec<i32>,
    /// All channels' samples: per channel `max_order` carried over from the
    /// previous frame, then the frame.
    raw: Vec<i32>,
    channel_size: usize,
    luts: Luts,
    float: Option<FloatState>,
    ready: VecDeque<Frame>,
}

impl AlsDecoder {
    /// `decode_init`
    pub fn new(params: &CodecParameters) -> Result<Self> {
        if params.extradata.is_empty() {
            return Err(invalid("missing required ALS extradata"));
        }
        let c = read_specific_config(&params.extradata)?;
        let channels = c.channels;
        let bits_per_raw_sample = if c.floating { 32 } else { (c.resolution + 1) * 8 };
        let num_buffers = if c.mc_coding { channels } else { 1 };
        let max_order = c.max_order as usize;
        let channel_size = c.frame_length as usize + max_order;
        let buffer = Buffer {
            const_block: false,
            shift_lsbs: 0,
            opt_order: 0,
            store_prev_samples: false,
            use_ltp: false,
            ltp_lag: 0,
            ltp_gain: [0; 5],
            quant_cof: vec![0; max_order.max(1)],
            lpc_cof: vec![0; max_order.max(1)],
        };
        let float = c.floating.then(|| FloatState {
            acf: vec![FLOAT_1; channels],
            last_acf_mantissa: vec![0; channels],
            shift_value: vec![0; channels],
            last_shift_value: vec![0; channels],
            raw_mantissa: vec![vec![0; c.frame_length as usize]; channels],
            larray: vec![0; c.frame_length as usize * 4],
            nbits: vec![0; c.frame_length as usize],
            mlz: Mlz::new(),
        });
        Ok(Self {
            codec_id: params.codec_id.clone(),
            bits_per_raw_sample,
            s_max: if c.resolution > 1 { 31 } else { 15 },
            ltp_lag_length: 8 + u32::from(c.sample_rate >= 96000) + u32::from(c.sample_rate >= 192_000),
            cur_frame_length: c.frame_length as usize,
            frame_id: 0,
            js_switch: false,
            num_blocks: 0,
            highest_decoded_channel: -1,
            buffers: vec![buffer; num_buffers],
            lpc_cof_reversed: vec![0; max_order.max(1)],
            chan_data: if c.mc_coding { vec![ChannelData::default(); channels * channels] } else { Vec::new() },
            reverted: vec![false; channels],
            prev_raw_samples: vec![0; max_order],
            raw: vec![0; channels * channel_size],
            channel_size,
            luts: Luts::new(),
            float,
            ready: VecDeque::new(),
            c,
        })
    }

    /// Where channel `ch`'s frame starts in `raw`.
    fn base(&self, ch: usize) -> usize {
        self.c.max_order as usize + ch * self.channel_size
    }

    fn sample_format(&self) -> SampleFormat {
        if self.c.floating {
            SampleFormat::F32
        } else if self.bits_per_raw_sample <= 16 {
            SampleFormat::S16
        } else {
            SampleFormat::S32
        }
    }

    /// `get_block_sizes`: the block lengths of a channel (or of all, with
    /// multichannel coding).
    fn get_block_sizes(&mut self, gb: &mut Bits, bs_info: &mut u32) -> Vec<usize> {
        if self.c.block_switching > 0 {
            let len = 1u32 << (self.c.block_switching + 2);
            *bs_info = gb.get_long(len) << (32 - len);
        }
        let mut depths = Vec::with_capacity(32);
        parse_bs_info(*bs_info, 0, 0, &mut depths);
        let frame_length = self.c.frame_length as usize;
        let mut div: Vec<usize> = depths.iter().map(|&d| frame_length >> d).collect();
        self.num_blocks = div.len();
        if self.cur_frame_length != frame_length {
            let mut remaining = self.cur_frame_length;
            for b in 0..div.len() {
                if remaining <= div[b] {
                    div[b] = remaining;
                    self.num_blocks = b + 1;
                    break;
                }
                remaining -= div[b];
            }
        }
        div.truncate(self.num_blocks);
        div
    }

    /// `read_const_block_data`
    fn read_const_block(&mut self, gb: &mut Bits, bd: &mut Block) -> Result<()> {
        if bd.block_length == 0 {
            return Err(invalid("empty block"));
        }
        self.raw[bd.raw] = 0;
        let constant = gb.bit() != 0;
        bd.js_blocks = gb.bit() != 0;
        gb.skip(5);
        if constant {
            let bits = if self.c.floating { 24 } else { self.bits_per_raw_sample };
            self.raw[bd.raw] = gb.sget_long(bits);
        }
        self.buffers[bd.buf].const_block = true;
        Ok(())
    }

    /// `read_var_block_data`
    fn read_var_block(&mut self, gb: &mut Bits, bd: &mut Block) -> Result<()> {
        let c = &self.c;
        let buf = &mut self.buffers[bd.buf];
        buf.const_block = false;
        buf.opt_order = 1;
        bd.js_blocks = gb.bit() != 0;
        let mut opt_order = buf.opt_order;

        let log2_sub_blocks = if !c.bgmc && !c.sb_part {
            0
        } else if c.bgmc && c.sb_part {
            gb.get(2)
        } else {
            2 * gb.bit()
        };
        let sub_blocks = 1usize << log2_sub_blocks;
        if bd.block_length & (sub_blocks - 1) != 0 || bd.block_length == 0 {
            return Err(invalid("block length is not evenly divisible by the number of subblocks"));
        }
        let sb_length = bd.block_length >> log2_sub_blocks;

        let mut s = [0u32; 8];
        let mut sx = [0u32; 8];
        if c.bgmc {
            s[0] = gb.get(8 + u32::from(c.resolution > 1));
            for k in 1..sub_blocks {
                s[k] = s[k - 1].wrapping_add(decode_rice(gb, 2) as u32);
            }
            for k in 0..sub_blocks {
                sx[k] = s[k] & 0x0F;
                s[k] >>= 4;
            }
        } else {
            s[0] = gb.get(4 + u32::from(c.resolution > 1));
            for k in 1..sub_blocks {
                s[k] = s[k - 1].wrapping_add(decode_rice(gb, 0) as u32);
            }
        }
        if s[1..sub_blocks].iter().any(|&v| v > 32) {
            return Err(invalid("k invalid for rice code"));
        }
        if gb.bit() != 0 {
            buf.shift_lsbs = gb.get(4) + 1;
        }
        buf.store_prev_samples = (bd.js_blocks && bd.raw_other.is_some()) || buf.shift_lsbs != 0;

        if !c.rlslms {
            if c.adapt_order && c.max_order > 0 {
                let x = ((bd.block_length >> 3) as i64 - 1).clamp(2, i64::from(c.max_order) + 1) as u32;
                buf.opt_order = gb.get_long(av_ceil_log2(x));
                if buf.opt_order > c.max_order {
                    buf.opt_order = c.max_order;
                    return Err(invalid("predictor order too large"));
                }
            } else {
                buf.opt_order = c.max_order;
            }
            opt_order = buf.opt_order;
            let n = opt_order as usize;
            if n > 0 {
                let q = &mut buf.quant_cof;
                let add_base: i32;
                if c.coef_table == 3 {
                    add_base = 0x7F;
                    q[0] = 32 * parcor_scaled_value(gb.get(7) as usize);
                    if n > 1 {
                        q[1] = -32 * parcor_scaled_value(gb.get(7) as usize);
                    }
                    for k in 2..n {
                        q[k] = gb.get(7) as i32;
                    }
                } else {
                    add_base = 1;
                    let table = &PARCOR_RICE_TABLE[c.coef_table as usize];
                    let mut k = 0;
                    while k < n.min(20) {
                        let (offset, rice) = table[k];
                        q[k] = decode_rice(gb, rice).wrapping_add(offset);
                        if !(-64..=63).contains(&q[k]) {
                            return Err(invalid("quant_cof is out of range"));
                        }
                        k += 1;
                    }
                    while k < n.min(127) {
                        q[k] = decode_rice(gb, 2).wrapping_add((k & 1) as i32);
                        k += 1;
                    }
                    while k < n {
                        q[k] = decode_rice(gb, 1);
                        k += 1;
                    }
                    q[0] = 32 * parcor_scaled_value((q[0] + 64) as usize);
                    if n > 1 {
                        q[1] = -32 * parcor_scaled_value((q[1] + 64) as usize);
                    }
                }
                for v in q.iter_mut().take(n).skip(2) {
                    *v = (*v as u32).wrapping_mul(1 << 14).wrapping_add((add_base << 13) as u32) as i32;
                }
            }
        }

        if c.long_term_prediction {
            buf.use_ltp = gb.bit() != 0;
            if buf.use_ltp {
                buf.ltp_gain[0] = decode_rice(gb, 1).wrapping_mul(8);
                buf.ltp_gain[1] = decode_rice(gb, 2).wrapping_mul(8);
                let r = gb.unary0(4) as usize;
                let col = gb.get(2) as usize;
                if r >= 4 {
                    return Err(invalid("r overflow"));
                }
                buf.ltp_gain[2] = LTP_GAIN_VALUES[r][col];
                buf.ltp_gain[3] = decode_rice(gb, 2).wrapping_mul(8);
                buf.ltp_gain[4] = decode_rice(gb, 1).wrapping_mul(8);
                buf.ltp_lag = gb.get(self.ltp_lag_length) as i32 + (opt_order as i32 + 1).max(4);
            }
        }

        let mut start = 0usize;
        if bd.ra_block {
            start = (opt_order as usize).min(3);
            if sb_length <= start {
                return Err(Error::unsupported("als: sub block length smaller or equal start"));
            }
            if opt_order > 0 {
                self.raw[bd.raw] = decode_rice(gb, self.bits_per_raw_sample - 4);
            }
            if opt_order > 1 {
                self.raw[bd.raw + 1] = decode_rice(gb, (s[0] + 3).min(self.s_max));
            }
            if opt_order > 2 {
                self.raw[bd.raw + 2] = decode_rice(gb, (s[0] + 1).min(self.s_max));
            }
        }

        if self.c.bgmc {
            let mut delta = [0u32; 8];
            let mut k = [0u32; 8];
            let b = ((av_ceil_log2(bd.block_length as u32) as i32 - 3) >> 1).clamp(0, 5) as u32;
            let Some(mut st) = bgmc::decode_init(gb) else { return Err(invalid("BGMC data past the frame")) };
            let mut at = bd.raw + start;
            for sb in 0..sub_blocks {
                let sb_len = sb_length - if sb == 0 { start } else { 0 };
                k[sb] = s[sb].saturating_sub(b);
                delta[sb] = (5 + k[sb]).wrapping_sub(s[sb]);
                if k[sb] >= 32 {
                    return Err(invalid("invalid BGMC parameter"));
                }
                bgmc::decode(gb, &mut self.raw[at..at + sb_len], delta[sb], sx[sb] as usize, &mut st, &mut self.luts);
                at += sb_len;
            }
            bgmc::decode_end(gb);
            let mut at = bd.raw + start;
            let mut first = start;
            for sb in 0..sub_blocks {
                let tail = TAIL_CODE[sx[sb] as usize][delta[sb] as usize];
                let (cur_k, cur_s) = (k[sb], s[sb]);
                for _ in first..sb_length {
                    let mut res = self.raw[at];
                    if res as u32 == tail {
                        let max_msb = (2 + u32::from(sx[sb] > 2) + u32::from(sx[sb] > 10)) << (5 - delta[sb]);
                        res = decode_rice(gb, cur_s);
                        res = if res >= 0 {
                            (res as u32).wrapping_add(max_msb.wrapping_shl(cur_k)) as i32
                        } else {
                            (res as u32).wrapping_sub((max_msb - 1).wrapping_shl(cur_k)) as i32
                        };
                    } else {
                        if res as u32 > tail {
                            res -= 1;
                        }
                        if res & 1 != 0 {
                            res = res.wrapping_neg();
                        }
                        res >>= 1;
                        if cur_k > 0 {
                            res = (res as u32).wrapping_mul(1u32 << cur_k) as i32;
                            res |= gb.get_long(cur_k) as i32;
                        }
                    }
                    self.raw[at] = res;
                    at += 1;
                }
                first = 0;
            }
        } else {
            let mut at = bd.raw + start;
            let mut first = start;
            for sb in 0..sub_blocks {
                for _ in first..sb_length {
                    self.raw[at] = decode_rice(gb, s[sb]);
                    at += 1;
                }
                first = 0;
            }
        }
        Ok(())
    }

    /// `decode_var_block_data`
    fn decode_var_block(&mut self, bd: &Block) {
        let max_order = self.c.max_order as usize;
        let block_length = bd.block_length;
        let buf = &mut self.buffers[bd.buf];
        let opt_order = buf.opt_order as usize;
        let raw = &mut self.raw;
        let at = bd.raw;

        if buf.use_ltp {
            let lag = buf.ltp_lag as i64;
            let mut ltp_smp = (lag - 2).max(0);
            while (ltp_smp as usize) < block_length {
                let center = ltp_smp - lag;
                let begin = (center - 2).max(0);
                let end = center + 3;
                let mut tab = (5 - (end - begin)) as usize;
                let mut y: i64 = 1 << 6;
                for base in begin..end {
                    y = y.wrapping_add(i64::from(buf.ltp_gain[tab]).wrapping_mul(i64::from(raw[at + base as usize])));
                    tab += 1;
                }
                let i = at + ltp_smp as usize;
                raw[i] = (i64::from(raw[i]) + (y >> 7)) as i32;
                ltp_smp += 1;
            }
        }

        let mut smp = 0usize;
        if bd.ra_block {
            while smp < opt_order.min(block_length) {
                let mut y: i64 = 1 << 19;
                for sb in 0..smp {
                    y = y.wrapping_add(i64::from(buf.lpc_cof[sb]).wrapping_mul(i64::from(raw[at + smp - (sb + 1)])));
                }
                raw[at + smp] = (i64::from(raw[at + smp]) - (y >> 20)) as i32;
                parcor_to_lpc(smp, &buf.quant_cof, &mut buf.lpc_cof);
                smp += 1;
            }
        } else {
            for k in 0..opt_order {
                parcor_to_lpc(k, &buf.quant_cof, &mut buf.lpc_cof);
            }
            if buf.store_prev_samples {
                self.prev_raw_samples.copy_from_slice(&raw[at - max_order..at]);
            }
            if bd.js_blocks {
                if let Some(other) = bd.raw_other {
                    // D = R - L, the channel pair's lower channel being left.
                    let (left, right) = if other > at { (at, other) } else { (other, at) };
                    for sb in 1..=max_order {
                        raw[at - sb] = raw[right - sb].wrapping_sub(raw[left - sb]);
                    }
                }
            }
            if buf.shift_lsbs > 0 {
                for sb in 1..=max_order {
                    raw[at - sb] >>= buf.shift_lsbs;
                }
            }
        }

        for sb in 0..opt_order {
            self.lpc_cof_reversed[sb] = buf.lpc_cof[opt_order - 1 - sb];
        }
        for i in at + smp..at + block_length {
            let mut y: i64 = 1 << 19;
            for (j, &cof) in self.lpc_cof_reversed[..opt_order].iter().enumerate() {
                y = y.wrapping_add(i64::from(cof).wrapping_mul(i64::from(raw[i - opt_order + j])));
            }
            raw[i] = (i64::from(raw[i]) - (y >> 20)) as i32;
        }

        if buf.store_prev_samples {
            raw[at - max_order..at].copy_from_slice(&self.prev_raw_samples);
        }
    }

    /// `read_block`
    fn read_block(&mut self, gb: &mut Bits, bd: &mut Block) -> Result<()> {
        self.buffers[bd.buf].shift_lsbs = 0;
        if gb.left() < 7 {
            return Err(invalid("block past the frame"));
        }
        let result = if gb.bit() != 0 { self.read_var_block(gb, bd) } else { self.read_const_block(gb, bd) };
        if !self.c.mc_coding || self.js_switch {
            gb.align();
        }
        result
    }

    /// `decode_block`
    fn decode_block(&mut self, bd: &Block) {
        let buf = &self.buffers[bd.buf];
        if buf.const_block {
            let v = self.raw[bd.raw];
            self.raw[bd.raw + 1..bd.raw + bd.block_length].fill(v);
        } else {
            self.decode_var_block(bd);
        }
        let shift = self.buffers[bd.buf].shift_lsbs;
        if shift > 0 {
            for v in &mut self.raw[bd.raw..bd.raw + bd.block_length] {
                *v = (*v as u32).wrapping_shl(shift) as i32;
            }
        }
    }

    /// `read_decode_block`
    fn read_decode_block(&mut self, gb: &mut Bits, bd: &mut Block) -> Result<()> {
        self.read_block(gb, bd)?;
        self.decode_block(bd);
        Ok(())
    }

    /// `zero_remaining`
    fn zero_remaining(&mut self, b: usize, div: &[usize], at: usize) {
        let count: usize = div[b..].iter().sum();
        self.raw[at..at + count].fill(0);
    }

    /// `decode_blocks_ind`
    fn decode_blocks_ind(&mut self, gb: &mut Bits, ra_frame: bool, c: usize, div: &[usize]) -> Result<()> {
        let mut bd = Block { block_length: 0, ra_block: ra_frame, js_blocks: false, buf: 0, raw: self.base(c), raw_other: None };
        for (b, &len) in div.iter().enumerate() {
            bd.block_length = len;
            if let Err(e) = self.read_decode_block(gb, &mut bd) {
                self.zero_remaining(b, div, bd.raw);
                return Err(e);
            }
            bd.raw += len;
            bd.ra_block = false;
        }
        Ok(())
    }

    /// `decode_blocks`: channels `c` and `c + 1` together.
    fn decode_blocks(&mut self, gb: &mut Bits, ra_frame: bool, c: usize, div: &[usize]) -> Result<()> {
        let max_order = self.c.max_order as usize;
        let frame_length = self.c.frame_length as usize;
        let mut bd0 = Block { block_length: 0, ra_block: ra_frame, js_blocks: false, buf: 0, raw: 0, raw_other: None };
        let mut bd1 = bd0;
        let mut offset = 0;
        for (b, &len) in div.iter().enumerate() {
            bd0.block_length = len;
            bd1.block_length = len;
            bd0.raw = self.base(c) + offset;
            bd1.raw = self.base(c + 1) + offset;
            bd0.raw_other = Some(bd1.raw);
            bd1.raw_other = Some(bd0.raw);
            let result = self.read_decode_block(gb, &mut bd0).and_then(|()| self.read_decode_block(gb, &mut bd1));
            if let Err(e) = result {
                self.zero_remaining(b, div, bd0.raw);
                self.zero_remaining(b, div, bd1.raw);
                return Err(e);
            }
            // Joint stereo: the difference channel back to its own.
            if bd0.js_blocks {
                for s in 0..len {
                    self.raw[bd0.raw + s] = self.raw[bd1.raw + s].wrapping_sub(self.raw[bd0.raw + s]);
                }
            } else if bd1.js_blocks {
                for s in 0..len {
                    self.raw[bd1.raw + s] = self.raw[bd1.raw + s].wrapping_add(self.raw[bd0.raw + s]);
                }
            }
            offset += len;
            bd0.ra_block = false;
            bd1.ra_block = false;
        }
        let base = self.base(c);
        self.raw.copy_within(base - max_order + frame_length..base + frame_length, base - max_order);
        Ok(())
    }

    /// `read_channel_data`
    fn read_channel_data(&mut self, gb: &mut Bits, c: usize) -> Result<()> {
        let channels = self.c.channels;
        let bits = av_ceil_log2(channels as u32);
        let weighting = |gb: &mut Bits, k: u32, off: i32| {
            MCC_WEIGHTINGS[decode_rice(gb, k).wrapping_add(off).clamp(0, 31) as usize]
        };
        let mut entries = 0;
        while entries < channels {
            let cd = &mut self.chan_data[c * channels + entries];
            cd.stop_flag = gb.bit() != 0;
            if cd.stop_flag {
                break;
            }
            cd.master_channel = gb.get_long(bits) as usize;
            if cd.master_channel >= channels {
                return Err(invalid("invalid master channel"));
            }
            if cd.master_channel != c {
                cd.time_diff_flag = gb.bit() != 0;
                cd.weighting[0] = weighting(gb, 1, 16);
                cd.weighting[1] = weighting(gb, 2, 14);
                cd.weighting[2] = weighting(gb, 1, 16);
                if cd.time_diff_flag {
                    cd.weighting[3] = weighting(gb, 1, 16);
                    cd.weighting[4] = weighting(gb, 1, 16);
                    cd.weighting[5] = weighting(gb, 1, 16);
                    cd.time_diff_sign = gb.bit() != 0;
                    cd.time_diff_index = i64::from(gb.get_long(self.ltp_lag_length - 3)) + 3;
                }
            }
            entries += 1;
        }
        if entries == channels {
            return Err(invalid("damaged channel data"));
        }
        gb.align();
        Ok(())
    }

    /// `revert_channel_correlation`
    fn revert_channel_correlation(&mut self, block_length: usize, offset: usize, c: usize) -> Result<()> {
        let channels = self.c.channels;
        if self.reverted[c] {
            return Ok(());
        }
        self.reverted[c] = true;
        let mut dep = 0;
        while dep < channels && !self.chan_data[c * channels + dep].stop_flag {
            let master = self.chan_data[c * channels + dep].master_channel;
            // FFmpeg ignores what the masters' reversion returns.
            let _ = self.revert_channel_correlation(block_length, offset, master);
            dep += 1;
        }
        if dep == channels {
            return Err(invalid("invalid channel correlation"));
        }
        let total = (channels * self.channel_size) as i64;
        let dst = self.base(c) + offset;
        let mut dep = 0;
        while !self.chan_data[c * channels + dep].stop_flag {
            let cd = self.chan_data[c * channels + dep];
            dep += 1;
            if cd.master_channel == c {
                continue;
            }
            let master = (self.base(cd.master_channel) + offset) as i64;
            let mut begin: i64 = 1;
            let mut end: i64 = block_length as i64 - 1;
            let w = cd.weighting.map(i64::from);
            let m = |raw: &[i32], i: i64| i64::from(raw[(master + i) as usize]);
            if cd.time_diff_flag {
                let mut t = cd.time_diff_index;
                if cd.time_diff_sign {
                    t = -t;
                    if begin < t {
                        return Err(invalid("begin smaller than time diff index"));
                    }
                    begin -= t;
                } else {
                    if end < t {
                        return Err(invalid("end smaller than time diff index"));
                    }
                    end -= t;
                }
                if (begin - 1).min(begin - 1 + t) < -master || (end + 1).max(end + 1 + t) > total - master {
                    return Err(invalid("sample range outside the raw buffer"));
                }
                for smp in begin..end {
                    let y = (1i64 << 6)
                        .wrapping_add(w[0].wrapping_mul(m(&self.raw, smp - 1)))
                        .wrapping_add(w[1].wrapping_mul(m(&self.raw, smp)))
                        .wrapping_add(w[2].wrapping_mul(m(&self.raw, smp + 1)))
                        .wrapping_add(w[3].wrapping_mul(m(&self.raw, smp - 1 + t)))
                        .wrapping_add(w[4].wrapping_mul(m(&self.raw, smp + t)))
                        .wrapping_add(w[5].wrapping_mul(m(&self.raw, smp + 1 + t)));
                    let i = dst + smp as usize;
                    self.raw[i] = (i64::from(self.raw[i]) + (y >> 7)) as i32;
                }
            } else {
                if begin - 1 < -master || end + 1 > total - master {
                    return Err(invalid("sample range outside the raw buffer"));
                }
                for smp in begin..end {
                    let y = (1i64 << 6)
                        .wrapping_add(w[0].wrapping_mul(m(&self.raw, smp - 1)))
                        .wrapping_add(w[1].wrapping_mul(m(&self.raw, smp)))
                        .wrapping_add(w[2].wrapping_mul(m(&self.raw, smp + 1)));
                    let i = dst + smp as usize;
                    self.raw[i] = (i64::from(self.raw[i]) + (y >> 7)) as i32;
                }
            }
        }
        Ok(())
    }

    /// `read_diff_float_data`
    fn read_diff_float_data(&mut self, gb: &mut Bits, ra_frame: bool) -> Result<()> {
        let channels = self.c.channels;
        let frame_length = self.cur_frame_length;
        let bases: Vec<usize> = (0..channels).map(|c| self.base(c)).collect();
        let Some(fs) = self.float.as_mut() else { return Ok(()) };
        let scale = SoftFloat::from_int(1, 23);
        gb.skip(32);
        let use_acf = gb.bit() != 0;
        if ra_frame {
            fs.last_acf_mantissa.fill(0);
            fs.last_shift_value.fill(0);
            fs.mlz.flush();
        }
        if (channels * 8) as i64 > gb.left() {
            return Err(invalid("float data past the frame"));
        }
        for c in 0..channels {
            let raw = &mut self.raw[bases[c]..bases[c] + frame_length];
            if use_acf {
                let m = if gb.bit() != 0 {
                    let m = gb.get(23);
                    fs.last_acf_mantissa[c] = m;
                    m
                } else {
                    fs.last_acf_mantissa[c]
                };
                fs.acf[c] = SoftFloat::from_bits(m);
            } else {
                fs.acf[c] = FLOAT_1;
            }
            let highest_byte = gb.get(2);
            let part_a = gb.bit() != 0;
            let shift_amp = gb.bit() != 0;
            if shift_amp {
                fs.shift_value[c] = gb.get(8);
                fs.last_shift_value[c] = fs.shift_value[c];
            } else {
                fs.shift_value[c] = fs.last_shift_value[c];
            }
            let mantissa = &mut fs.raw_mantissa[c];
            if part_a {
                if gb.bit() == 0 {
                    for i in 0..frame_length {
                        if raw[i] == 0 {
                            mantissa[i] = gb.get_long(32);
                        }
                    }
                } else {
                    let nchars = 4 * raw.iter().filter(|&&v| v == 0).count();
                    let got = fs.mlz.decompress(gb, &mut fs.larray[..nchars]);
                    if got != nchars {
                        return Err(invalid("MLZ decompression error"));
                    }
                    let mut j = 0;
                    for i in 0..frame_length {
                        if raw[i] == 0 {
                            mantissa[i] = u32::from_be_bytes([fs.larray[j], fs.larray[j + 1], fs.larray[j + 2], fs.larray[j + 3]]);
                            j += 4;
                        }
                    }
                }
            }
            if highest_byte != 0 {
                let unit = fs.acf[c].same(FLOAT_1);
                for i in 0..frame_length {
                    if raw[i] != 0 {
                        let n = if unit {
                            let nbit = av_log2(raw[i].unsigned_abs());
                            if nbit > 23 {
                                return Err(invalid("float sample too large"));
                            }
                            23 - nbit
                        } else {
                            23
                        };
                        fs.nbits[i] = n.min(highest_byte * 8);
                    }
                }
                if gb.bit() == 0 {
                    for i in 0..frame_length {
                        if raw[i] != 0 {
                            mantissa[i] = gb.get_long(fs.nbits[i]);
                        }
                    }
                } else {
                    let nchars: usize = (0..frame_length)
                        .filter(|&i| raw[i] != 0)
                        .map(|i| fs.nbits[i].div_ceil(8) as usize)
                        .sum();
                    let got = fs.mlz.decompress(gb, &mut fs.larray[..nchars]);
                    if got != nchars {
                        return Err(invalid("MLZ decompression error"));
                    }
                    let mut j = 0;
                    for i in 0..frame_length {
                        if raw[i] != 0 {
                            let aligned = 8 * fs.nbits[i].div_ceil(8);
                            let mut acc: u64 = 0;
                            for _ in 0..aligned / 8 {
                                acc = (acc << 8) + u64::from(fs.larray[j]);
                                j += 1;
                            }
                            mantissa[i] = (acc >> (aligned - fs.nbits[i])) as u32;
                        }
                    }
                }
            }
            for i in 0..frame_length {
                if raw[i] != 0 {
                    let mut pcm = SoftFloat::from_int(i64::from(raw[i]), 0).div(scale);
                    if !fs.acf[c].same(FLOAT_1) {
                        pcm = multiply(fs.acf[c], pcm);
                    }
                    let sign = pcm.sign as u32;
                    let mut e = pcm.exp as u32;
                    let mut m = ((pcm.mant | 0x80_0000) as u32).wrapping_add(mantissa[i]);
                    while m >= 0x100_0000 {
                        e = e.wrapping_add(1);
                        m >>= 1;
                    }
                    if m != 0 {
                        e = e.wrapping_add(fs.shift_value[c].wrapping_sub(127));
                    }
                    m &= 0x7F_FFFF;
                    raw[i] = ((sign << 31) | (e.wrapping_add(127) << 23) | m) as i32;
                } else {
                    raw[i] = mantissa[i] as i32;
                }
            }
            gb.align();
        }
        Ok(())
    }

    /// `read_frame_data`
    fn read_frame_data(&mut self, gb: &mut Bits, ra_frame: bool) -> Result<()> {
        let channels = self.c.channels;
        let max_order = self.c.max_order as usize;
        let frame_length = self.c.frame_length as usize;
        let mut bs_info = 0u32;
        if self.c.ra_flag == RA_FLAG_FRAMES && ra_frame {
            gb.skip(32);
        }
        if self.c.mc_coding && self.c.joint_stereo {
            self.js_switch = gb.bit() != 0;
            gb.align();
        }
        if !self.c.mc_coding || self.js_switch {
            let mut independent_bs = u32::from(!self.c.joint_stereo);
            if gb.left() < (7 * channels * self.num_blocks) as i64 {
                return Err(invalid("frame shorter than its blocks"));
            }
            let mut c = 0;
            while c < channels {
                let div = self.get_block_sizes(gb, &mut bs_info);
                if self.c.joint_stereo && self.c.block_switching > 0 && bs_info >> 31 != 0 {
                    independent_bs = 2;
                }
                if c == channels - 1 || c & 1 != 0 {
                    independent_bs = 1;
                }
                if independent_bs > 0 {
                    self.decode_blocks_ind(gb, ra_frame, c, &div)?;
                    independent_bs -= 1;
                } else {
                    self.decode_blocks(gb, ra_frame, c, &div)?;
                    c += 1;
                }
                let base = self.base(c);
                self.raw.copy_within(base - max_order + frame_length..base + frame_length, base - max_order);
                self.highest_decoded_channel = c as i64;
                c += 1;
            }
        } else {
            self.reverted.fill(false);
            let div = self.get_block_sizes(gb, &mut bs_info);
            let mut bd = Block { block_length: 0, ra_block: ra_frame, js_blocks: false, buf: 0, raw: 0, raw_other: None };
            let mut offset = 0;
            for &len in &div {
                bd.block_length = len;
                if len == 0 {
                    continue;
                }
                for c in 0..channels {
                    bd.buf = c;
                    bd.raw = self.base(c) + offset;
                    bd.raw_other = None;
                    self.read_block(gb, &mut bd)?;
                    self.read_channel_data(gb, c)?;
                }
                for c in 0..channels {
                    self.revert_channel_correlation(len, offset, c)?;
                }
                for c in 0..channels {
                    bd.buf = c;
                    bd.raw = self.base(c) + offset;
                    self.decode_block(&bd);
                    self.highest_decoded_channel = self.highest_decoded_channel.max(c as i64);
                }
                self.reverted.fill(false);
                offset += len;
                bd.ra_block = false;
            }
            for c in 0..channels {
                let base = self.base(c);
                self.raw.copy_within(base - max_order + frame_length..base + frame_length, base - max_order);
            }
        }
        if self.c.floating {
            self.read_diff_float_data(gb, ra_frame)?;
        }
        if gb.left() < 0 {
            return Err(invalid("overread"));
        }
        Ok(())
    }

    /// `decode_frame`: one frame from `data`; its samples interleaved in
    /// the output format and the bytes it took.
    fn decode_frame(&mut self, data: &[u8]) -> Result<(usize, Vec<u8>, usize)> {
        let mut gb = Bits::new(data);
        let c = &self.c;
        let ra_frame = c.ra_distance != 0 && self.frame_id % c.ra_distance == 0;
        self.cur_frame_length = if c.samples != u32::MAX {
            let left = u64::from(c.samples).wrapping_sub(u64::from(self.frame_id) * u64::from(c.frame_length));
            left.min(u64::from(c.frame_length)) as usize
        } else {
            c.frame_length as usize
        };
        self.highest_decoded_channel = -1;
        let invalid_frame = self.read_frame_data(&mut gb, ra_frame).is_err();
        if self.highest_decoded_channel == -1 {
            return Err(invalid("no channel data decoded"));
        }
        self.frame_id = self.frame_id.wrapping_add(1);

        let channels = self.c.channels;
        let n = self.cur_frame_length;
        let bases: Vec<usize> = (0..channels)
            .map(|ch| self.base(self.c.chan_pos.as_ref().map_or(ch, |p| p[ch])))
            .collect();
        let mut out = Vec::with_capacity(n * channels * 4);
        if self.bits_per_raw_sample <= 16 {
            let shift = 16 - self.bits_per_raw_sample;
            for i in 0..n {
                for &b in &bases {
                    out.extend_from_slice(&(((self.raw[b + i] as u32) << shift) as i16).to_le_bytes());
                }
            }
        } else {
            let shift = 32 - self.bits_per_raw_sample;
            for i in 0..n {
                for &b in &bases {
                    out.extend_from_slice(&((self.raw[b + i] as u32) << shift).to_le_bytes());
                }
            }
        }
        let consumed = if invalid_frame { data.len() } else { gb.count().div_ceil(8) };
        Ok((n, out, consumed))
    }
}

impl Decoder for AlsDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: self.sample_format(),
            sample_rate: self.c.sample_rate,
            channels: self.c.channels as u16,
        })
    }

    /// decode.c's loop: frames until the packet's bytes are taken. An error
    /// drops the rest of the packet.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut data: &[u8] = &packet.data;
        let mut pts = packet.pts;
        while !data.is_empty() {
            let (samples, bytes, consumed) = self.decode_frame(data)?;
            self.ready.push_back(Frame::Audio(AudioFrame { samples: samples as u32, pts: pts.take(), data: vec![bytes] }));
            if consumed == 0 || consumed >= data.len() {
                break;
            }
            data = &data[consumed..];
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.ready.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// alsdec.c's `flush`: the frame count starts again (the decoder's
    /// buffers stay).
    fn reset(&mut self) -> Result<()> {
        self.frame_id = 0;
        self.ready.clear();
        Ok(())
    }
}
