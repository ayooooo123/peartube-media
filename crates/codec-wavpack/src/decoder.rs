// Ported from FFmpeg libavcodec/wavpack.c, libavcodec/wavpack.h (commit 2da55bf)
// Copyright (c) 2006, 2011 Konstantin Shishkov
// Copyright (c) 2020 David Bryant
// License: LGPL-2.1-or-later

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::sync::LazyLock;
use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result,
    SampleFormat,
};

use crate::bitreader::BitReaderLe;
use crate::common::*;
use crate::dsd::{
    dsd2pcm_translate, wv_unpack_dsd_copy, wv_unpack_dsd_fast, wv_unpack_dsd_high, DsdContext,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct Decorr {
    pub delta: i32,
    pub value: i32,
    pub weight_a: i32,
    pub weight_b: i32,
    pub samples_a: [i32; MAX_TERM],
    pub samples_b: [i32; MAX_TERM],
    pub sum_a: i32,
    pub sum_b: i32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WvChannel {
    pub median: [i32; 3],
    pub slow_level: i32,
    pub error_limit: i32,
    pub bitrate_acc: u32,
    pub bitrate_delta: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modulation {
    Pcm,
    Dsd,
}

#[derive(Clone, Debug, Default)]
pub struct WavpackFrameContext {
    pub frame_flags: u32,
    pub stereo: bool,
    pub stereo_in: bool,
    pub joint: bool,
    pub crc: u32,
    pub got_extra_bits: bool,
    pub crc_extra_bits: u32,
    pub extra_bits_data: Vec<u8>,
    pub samples: usize,
    pub terms: usize,
    pub decorr: [Decorr; MAX_TERMS],
    pub zero: bool,
    pub one: bool,
    pub zeroes: i32,
    pub extra_bits: usize,
    pub and: u32,
    pub or: u32,
    pub shift: usize,
    pub post_shift: usize,
    pub hybrid: bool,
    pub hybrid_bitrate: bool,
    pub hybrid_maxclip: i32,
    pub hybrid_minclip: i32,
    pub float_flag: u8,
    pub float_shift: usize,
    pub float_max_exp: i32,
    pub ch: [WvChannel; 2],
    pub pcm_data: Vec<u8>,
    pub dsd_data: Vec<u8>,
    pub dsd_mode: u8,
    pub rate_x: u32,
    pub custom_sample_rate: u32,
    pub custom_chan: u16,
    pub custom_chmask: u64,
}

#[inline(always)]
fn level_decay(a: i32) -> i32 {
    (a + 0x80) >> 8
}

#[inline(always)]
fn get_med(median: &[i32; 3], n: usize) -> i32 {
    (median[n] >> 4) + 1
}

#[inline(always)]
fn dec_med(median: &mut [i32; 3], n: usize) {
    let denom = 128 >> n;
    let num = (median[n] as u32).wrapping_add(128 >> n).wrapping_sub(2) as i32;
    let div = num / denom;
    median[n] = median[n].wrapping_sub((div as u32).wrapping_mul(2) as i32);
}

#[inline(always)]
fn inc_med(median: &mut [i32; 3], n: usize) {
    let denom = 128 >> n;
    let num = (median[n] as u32).wrapping_add(128 >> n) as i32;
    let div = num / denom;
    median[n] = median[n].wrapping_add((div as u32).wrapping_mul(5) as i32);
}

#[inline(always)]
fn update_weight_clip(weight: &mut i32, delta: i32, samples: i32, input: i32) {
    if samples != 0 && input != 0 {
        if (samples ^ input) < 0 {
            *weight -= delta;
            if *weight < -1024 {
                *weight = -1024;
            }
        } else {
            *weight += delta;
            if *weight > 1024 {
                *weight = 1024;
            }
        }
    }
}

fn update_error_limit(ctx: &mut WavpackFrameContext) -> Result<()> {
    let num_ch = if ctx.stereo_in { 2 } else { 1 };
    let mut br = [0i32; 2];
    let mut sl = [0i32; 2];

    for i in 0..num_ch {
        if ctx.ch[i].bitrate_acc > u32::MAX - ctx.ch[i].bitrate_delta {
            return Err(Error::invalid("bitrate_acc overflow"));
        }
        ctx.ch[i].bitrate_acc += ctx.ch[i].bitrate_delta;
        br[i] = (ctx.ch[i].bitrate_acc >> 16) as i32;
        sl[i] = level_decay(ctx.ch[i].slow_level);
    }

    if ctx.stereo_in && ctx.hybrid_bitrate {
        let balance = (sl[1] - sl[0] + br[1] + 1) >> 1;
        if balance > br[0] {
            br[1] = br[0] * 2;
            br[0] = 0;
        } else if -balance > br[0] {
            br[0] *= 2;
            br[1] = 0;
        } else {
            br[1] = br[0] + balance;
            br[0] = br[0] - balance;
        }
    }

    for i in 0..num_ch {
        if ctx.hybrid_bitrate {
            if sl[i] - br[i] > -0x100 {
                ctx.ch[i].error_limit = wp_exp2((sl[i] - br[i] + 0x100) as i16);
            } else {
                ctx.ch[i].error_limit = 0;
            }
        } else {
            ctx.ch[i].error_limit = wp_exp2(br[i] as i16);
        }
    }

    Ok(())
}

fn wv_get_value(
    ctx: &mut WavpackFrameContext,
    gb: &mut BitReaderLe,
    channel: usize,
    last: &mut bool,
) -> i32 {
    *last = false;

    if (ctx.ch[0].median[0] as u32) < 2 && (ctx.ch[1].median[0] as u32) < 2 && !ctx.zero && !ctx.one {
        if ctx.zeroes > 0 {
            ctx.zeroes -= 1;
            if ctx.zeroes > 0 {
                ctx.ch[channel].slow_level -= level_decay(ctx.ch[channel].slow_level);
                return 0;
            }
        } else {
            let mut t = gb.read_unary_0_33() as i32;
            if t >= 2 {
                if t >= 32 || gb.bits_left_signed() < (t - 1) as isize {
                    *last = true;
                    return 0;
                }
                t = (gb.read_bits((t - 1) as usize) as i32) | (1 << (t - 1));
            } else if gb.bits_left_signed() < 0 {
                *last = true;
                return 0;
            }
            ctx.zeroes = t;
            if ctx.zeroes > 0 {
                ctx.ch[0].median = [0; 3];
                ctx.ch[1].median = [0; 3];
                ctx.ch[channel].slow_level -= level_decay(ctx.ch[channel].slow_level);
                return 0;
            }
        }
    }

    let mut t: i32;
    if ctx.zero {
        t = 0;
        ctx.zero = false;
    } else {
        t = gb.read_unary_0_33() as i32;
        if gb.bits_left_signed() < 0 {
            *last = true;
            return 0;
        }
        if t == 16 {
            let t2 = gb.read_unary_0_33() as i32;
            if t2 < 2 {
                if gb.bits_left_signed() < 0 {
                    *last = true;
                    return 0;
                }
                t = t.wrapping_add(t2);
            } else {
                if t2 >= 32 || gb.bits_left_signed() < (t2 - 1) as isize {
                    *last = true;
                    return 0;
                }
                let bits = gb.read_bits((t2 - 1) as usize) as i32;
                t = t.wrapping_add(bits | (1 << (t2 - 1)));
            }
        }

        if ctx.one {
            ctx.one = (t & 1) != 0;
            t = (t >> 1) + 1;
        } else {
            ctx.one = (t & 1) != 0;
            t >>= 1;
        }
        ctx.zero = !ctx.one;
    }

    if ctx.hybrid && channel == 0 {
        if update_error_limit(ctx).is_err() {
            *last = true;
            return 0;
        }
    }

    let mut base: i32;
    let mut add: i32;

    let med0 = get_med(&ctx.ch[channel].median, 0);
    let med1 = get_med(&ctx.ch[channel].median, 1);
    let med2 = get_med(&ctx.ch[channel].median, 2);

    if t == 0 {
        base = 0;
        add = med0 - 1;
        dec_med(&mut ctx.ch[channel].median, 0);
    } else if t == 1 {
        base = med0;
        add = med1 - 1;
        inc_med(&mut ctx.ch[channel].median, 0);
        dec_med(&mut ctx.ch[channel].median, 1);
    } else if t == 2 {
        base = med0 + med1;
        add = med2 - 1;
        inc_med(&mut ctx.ch[channel].median, 0);
        inc_med(&mut ctx.ch[channel].median, 1);
        dec_med(&mut ctx.ch[channel].median, 2);
    } else {
        base = med0
            .wrapping_add(med1)
            .wrapping_add(med2.wrapping_mul(t.wrapping_sub(2)));
        add = med2 - 1;
        inc_med(&mut ctx.ch[channel].median, 0);
        inc_med(&mut ctx.ch[channel].median, 1);
        inc_med(&mut ctx.ch[channel].median, 2);
    }

    let error_limit = ctx.ch[channel].error_limit;
    let ret: i32;
    if error_limit == 0 {
        let tail = gb.get_tail(add as u32) as i32;
        ret = base.wrapping_add(tail);
        if gb.bits_left_signed() <= 0 {
            *last = true;
            return 0;
        }
    } else {
        let mut mid = (((base as u32).wrapping_mul(2)).wrapping_add(add as u32).wrapping_add(1) >> 1) as i32;
        while add > error_limit {
            if gb.bits_left_signed() <= 0 {
                *last = true;
                return 0;
            }
            if gb.read_bit() == 1 {
                add = add.wrapping_sub(mid.wrapping_sub(base));
                base = mid;
            } else {
                add = mid.wrapping_sub(base).wrapping_sub(1);
            }
            mid = (((base as u32).wrapping_mul(2)).wrapping_add(add as u32).wrapping_add(1) >> 1) as i32;
        }
        ret = mid;
    }

    let sign = gb.read_bit() != 0;
    if ctx.hybrid_bitrate {
        ctx.ch[channel].slow_level += wp_log2(ret as u32) - level_decay(ctx.ch[channel].slow_level);
    }

    if sign {
        !ret
    } else {
        ret
    }
}

#[inline]
fn wv_get_value_integer(
    s: &WavpackFrameContext,
    extra_gb: &mut Option<BitReaderLe>,
    crc: &mut u32,
    mut sample: u32,
) -> i32 {
    if s.extra_bits > 0 {
        sample = sample.wrapping_mul(1 << s.extra_bits);
        if s.got_extra_bits {
            if let Some(gb) = &mut *extra_gb {
                if gb.bits_left() >= s.extra_bits {
                    let bits = gb.read_bits(s.extra_bits);
                    sample |= bits;
                    *crc = crc
                        .wrapping_mul(9)
                        .wrapping_add((sample & 0xffff).wrapping_mul(3))
                        .wrapping_add(sample >> 16);
                }
            }
        }
    }

    let mut bit = (sample & s.and) | s.or;
    bit = (sample.wrapping_add(bit) << s.shift).wrapping_sub(bit);

    if s.hybrid {
        let clamped = (bit as i32).clamp(s.hybrid_minclip, s.hybrid_maxclip);
        bit = clamped as u32;
    }

    (bit << s.post_shift) as i32
}

#[inline]
fn wv_get_value_float(
    s: &WavpackFrameContext,
    extra_gb: &mut Option<BitReaderLe>,
    crc: &mut u32,
    mut sample: i32,
) -> f32 {
    let mut exp = s.float_max_exp;
    let sign: u32;

    if s.got_extra_bits {
        const MAX_BITS: usize = 1 + 23 + 8 + 1;
        let left_bits = extra_gb.as_ref().map_or(0, |gb| gb.bits_left());
        if left_bits + 8 * 64 < MAX_BITS {
            return 0.0;
        }
    }

    let mut s_val: u32;
    if sample != 0 {
        sample = sample.wrapping_mul(1 << s.float_shift);
        let is_neg = sample < 0;
        sign = if is_neg { 1 } else { 0 };
        s_val = if is_neg {
            (sample as u32).wrapping_neg()
        } else {
            sample as u32
        };

        if s_val >= 0x1000000 {
            if s.got_extra_bits && extra_gb.as_mut().map_or(0, |gb| gb.read_bit()) == 1 {
                s_val = extra_gb.as_mut().map_or(0, |gb| gb.read_bits(23));
            } else {
                s_val = 0;
            }
            exp = 255;
        } else if exp != 0 {
            let mut shift = 23 - (31 - (s_val | 1).leading_zeros() as i32);
            exp = s.float_max_exp;
            if exp <= shift {
                exp -= 1;
                shift = exp;
            }
            exp -= shift;

            if shift > 0 {
                s_val <<= shift;
                if (s.float_flag & WV_FLT_SHIFT_ONES) != 0
                    || (s.got_extra_bits
                        && (s.float_flag & WV_FLT_SHIFT_SAME) != 0
                        && extra_gb.as_mut().map_or(0, |gb| gb.read_bit()) == 1)
                {
                    s_val |= (1 << shift) - 1;
                } else if s.got_extra_bits && (s.float_flag & WV_FLT_SHIFT_SENT) != 0 {
                    let bits = extra_gb.as_mut().map_or(0, |gb| gb.read_bits(shift as usize));
                    s_val |= bits;
                }
            }
        } else {
            exp = s.float_max_exp;
        }
        s_val &= 0x7fffff;
    } else {
        let mut zero_sign = 0;
        exp = 0;
        s_val = 0;
        if s.got_extra_bits && (s.float_flag & WV_FLT_ZERO_SENT) != 0 {
            if extra_gb.as_mut().map_or(0, |gb| gb.read_bit()) == 1 {
                s_val = extra_gb.as_mut().map_or(0, |gb| gb.read_bits(23));
                if s.float_max_exp >= 25 {
                    exp = extra_gb.as_mut().map_or(0, |gb| gb.read_bits(8)) as i32;
                }
                zero_sign = extra_gb.as_mut().map_or(0, |gb| gb.read_bit());
            } else if (s.float_flag & WV_FLT_ZERO_SIGN) != 0 {
                zero_sign = extra_gb.as_mut().map_or(0, |gb| gb.read_bit());
            }
        }
        sign = zero_sign;
    }

    *crc = crc
        .wrapping_mul(27)
        .wrapping_add(s_val.wrapping_mul(9))
        .wrapping_add((exp as u32).wrapping_mul(3))
        .wrapping_add(sign);

    let bits = (sign << 31) | ((exp as u32) << 23) | s_val;
    f32::from_bits(bits)
}

fn wv_unpack_stereo(
    s: &mut WavpackFrameContext,
    gb: &mut BitReaderLe,
    mut extra_gb: Option<BitReaderLe>,
    dst_l: &mut [u8],
    dst_r: &mut [u8],
    sample_fmt: SampleFormat,
) -> Result<()> {
    let mut last = false;
    let mut pos = 0usize;
    let mut crc = 0xffff_ffffu32;
    let mut crc_extra_bits = 0xffff_ffffu32;
    let mut count = 0usize;

    s.one = false;
    s.zero = false;
    s.zeroes = 0;

    let is_s16 = sample_fmt == SampleFormat::S16P;
    let is_flt = sample_fmt == SampleFormat::F32P;
    let is_s32 = sample_fmt == SampleFormat::S32P;

    while !last && count < s.samples {
        let mut l = wv_get_value(s, gb, 0, &mut last);
        if last {
            break;
        }
        let mut r = wv_get_value(s, gb, 1, &mut last);
        if last {
            break;
        }

        for i in 0..s.terms {
            let decorr = &mut s.decorr[i];
            let t = decorr.value;
            let (a, b, j);
            if t > 0 {
                if t > 8 {
                    if (t & 1) != 0 {
                        a = (2u32).wrapping_mul(decorr.samples_a[0] as u32).wrapping_sub(decorr.samples_a[1] as u32) as i32;
                        b = (2u32).wrapping_mul(decorr.samples_b[0] as u32).wrapping_sub(decorr.samples_b[1] as u32) as i32;
                    } else {
                        a = ((3u32).wrapping_mul(decorr.samples_a[0] as u32).wrapping_sub(decorr.samples_a[1] as u32) as i32) >> 1;
                        b = ((3u32).wrapping_mul(decorr.samples_b[0] as u32).wrapping_sub(decorr.samples_b[1] as u32) as i32) >> 1;
                    }
                    decorr.samples_a[1] = decorr.samples_a[0];
                    decorr.samples_b[1] = decorr.samples_b[0];
                    j = 0;
                } else {
                    a = decorr.samples_a[pos];
                    b = decorr.samples_b[pos];
                    j = (pos + t as usize) & 7;
                }

                let l2 = if !is_s16 {
                    l.wrapping_add(((decorr.weight_a as i64 * a as i64 + 512) >> 10) as i32)
                } else {
                    l.wrapping_add((decorr.weight_a as u32).wrapping_mul(a as u32).wrapping_add(512) as i32 >> 10)
                };
                let r2 = if !is_s16 {
                    r.wrapping_add(((decorr.weight_b as i64 * b as i64 + 512) >> 10) as i32)
                } else {
                    r.wrapping_add((decorr.weight_b as u32).wrapping_mul(b as u32).wrapping_add(512) as i32 >> 10)
                };

                if a != 0 && l != 0 {
                    let s_sign = if (l ^ a) < 0 { 1 } else { -1 };
                    decorr.weight_a -= s_sign * decorr.delta;
                }
                if b != 0 && r != 0 {
                    let s_sign = if (r ^ b) < 0 { 1 } else { -1 };
                    decorr.weight_b -= s_sign * decorr.delta;
                }

                l = l2;
                r = r2;
                decorr.samples_a[j] = l;
                decorr.samples_b[j] = r;
            } else if t == -1 {
                let l2 = if !is_s16 {
                    l.wrapping_add(((decorr.weight_a as i64 * decorr.samples_a[0] as i64 + 512) >> 10) as i32)
                } else {
                    l.wrapping_add((decorr.weight_a as u32).wrapping_mul(decorr.samples_a[0] as u32).wrapping_add(512) as i32 >> 10)
                };
                update_weight_clip(&mut decorr.weight_a, decorr.delta, decorr.samples_a[0], l);
                l = l2;
                let r2 = if !is_s16 {
                    r.wrapping_add(((decorr.weight_b as i64 * l2 as i64 + 512) >> 10) as i32)
                } else {
                    r.wrapping_add((decorr.weight_b as u32).wrapping_mul(l2 as u32).wrapping_add(512) as i32 >> 10)
                };
                update_weight_clip(&mut decorr.weight_b, decorr.delta, l2, r);
                r = r2;
                decorr.samples_a[0] = r;
            } else {
                let r2 = if !is_s16 {
                    r.wrapping_add(((decorr.weight_b as i64 * decorr.samples_b[0] as i64 + 512) >> 10) as i32)
                } else {
                    r.wrapping_add((decorr.weight_b as u32).wrapping_mul(decorr.samples_b[0] as u32).wrapping_add(512) as i32 >> 10)
                };
                update_weight_clip(&mut decorr.weight_b, decorr.delta, decorr.samples_b[0], r);
                r = r2;

                let r_for_l = if t == -3 {
                    let old_a = decorr.samples_a[0];
                    decorr.samples_a[0] = r;
                    old_a
                } else {
                    r2
                };

                let l2 = if !is_s16 {
                    l.wrapping_add(((decorr.weight_a as i64 * r_for_l as i64 + 512) >> 10) as i32)
                } else {
                    l.wrapping_add((decorr.weight_a as u32).wrapping_mul(r_for_l as u32).wrapping_add(512) as i32 >> 10)
                };
                update_weight_clip(&mut decorr.weight_a, decorr.delta, r_for_l, l);
                l = l2;
                decorr.samples_b[0] = l;
            }
        }

        if is_s16 {
            if (l as i64).abs() + (r as i64).abs() > (1 << 19) {
                return Err(Error::invalid("sample too large"));
            }
        }

        pos = (pos + 1) & 7;
        if s.joint {
            r = r.wrapping_sub(l >> 1);
            l = l.wrapping_add(r);
        }
        crc = crc.wrapping_mul(3).wrapping_add(l as u32).wrapping_mul(3).wrapping_add(r as u32);

        if is_flt {
            let fl = wv_get_value_float(s, &mut extra_gb, &mut crc_extra_bits, l);
            let fr = wv_get_value_float(s, &mut extra_gb, &mut crc_extra_bits, r);
            let off = count * 4;
            if off + 4 <= dst_l.len() && off + 4 <= dst_r.len() {
                dst_l[off..off + 4].copy_from_slice(&fl.to_le_bytes());
                dst_r[off..off + 4].copy_from_slice(&fr.to_le_bytes());
            }
        } else if is_s32 {
            let sl = wv_get_value_integer(s, &mut extra_gb, &mut crc_extra_bits, l as u32);
            let sr = wv_get_value_integer(s, &mut extra_gb, &mut crc_extra_bits, r as u32);
            let off = count * 4;
            if off + 4 <= dst_l.len() && off + 4 <= dst_r.len() {
                dst_l[off..off + 4].copy_from_slice(&sl.to_le_bytes());
                dst_r[off..off + 4].copy_from_slice(&sr.to_le_bytes());
            }
        } else {
            let sl = wv_get_value_integer(s, &mut extra_gb, &mut crc_extra_bits, l as u32) as i16;
            let sr = wv_get_value_integer(s, &mut extra_gb, &mut crc_extra_bits, r as u32) as i16;
            let off = count * 2;
            if off + 2 <= dst_l.len() && off + 2 <= dst_r.len() {
                dst_l[off..off + 2].copy_from_slice(&sl.to_le_bytes());
                dst_r[off..off + 2].copy_from_slice(&sr.to_le_bytes());
            }
        }

        count += 1;
    }

    if count < s.samples {
        let bpp = sample_fmt.bytes_per_sample();
        let off = count * bpp;
        if off < dst_l.len() {
            dst_l[off..].fill(0);
        }
        if off < dst_r.len() {
            dst_r[off..].fill(0);
        }
    }

    Ok(())
}

fn wv_unpack_mono(
    s: &mut WavpackFrameContext,
    gb: &mut BitReaderLe,
    mut extra_gb: Option<BitReaderLe>,
    dst: &mut [u8],
    sample_fmt: SampleFormat,
) -> Result<()> {
    let mut last = false;
    let mut pos = 0usize;
    let mut crc = 0xffff_ffffu32;
    let mut crc_extra_bits = 0xffff_ffffu32;
    let mut count = 0usize;

    s.one = false;
    s.zero = false;
    s.zeroes = 0;

    let is_s16 = sample_fmt == SampleFormat::S16P;
    let is_flt = sample_fmt == SampleFormat::F32P;
    let is_s32 = sample_fmt == SampleFormat::S32P;

    while !last && count < s.samples {
        let mut t_val = wv_get_value(s, gb, 0, &mut last);
        if last {
            break;
        }
        let mut s_val = 0i32;

        for i in 0..s.terms {
            let decorr = &mut s.decorr[i];
            let t = decorr.value;
            let (a, j);
            if t > 8 {
                if (t & 1) != 0 {
                    a = (2u32).wrapping_mul(decorr.samples_a[0] as u32).wrapping_sub(decorr.samples_a[1] as u32) as i32;
                } else {
                    a = ((3u32).wrapping_mul(decorr.samples_a[0] as u32).wrapping_sub(decorr.samples_a[1] as u32) as i32) >> 1;
                }
                decorr.samples_a[1] = decorr.samples_a[0];
                j = 0;
            } else {
                a = decorr.samples_a[pos];
                j = ((pos as i32).wrapping_add(t) as usize) & 7;
            }

            if !is_s16 {
                s_val = t_val.wrapping_add(((decorr.weight_a as i64 * a as i64 + 512) >> 10) as i32);
            } else {
                s_val = t_val.wrapping_add((decorr.weight_a as u32).wrapping_mul(a as u32).wrapping_add(512) as i32 >> 10);
            }

            if a != 0 && t_val != 0 {
                let s_sign = if (t_val ^ a) < 0 { 1 } else { -1 };
                decorr.weight_a -= s_sign * decorr.delta;
            }

            t_val = s_val;
            decorr.samples_a[j] = s_val;
        }

        pos = (pos + 1) & 7;
        crc = crc.wrapping_mul(3).wrapping_add(s_val as u32);

        if is_flt {
            let f = wv_get_value_float(s, &mut extra_gb, &mut crc_extra_bits, s_val);
            let off = count * 4;
            if off + 4 <= dst.len() {
                dst[off..off + 4].copy_from_slice(&f.to_le_bytes());
            }
        } else if is_s32 {
            let s32 = wv_get_value_integer(s, &mut extra_gb, &mut crc_extra_bits, s_val as u32);
            let off = count * 4;
            if off + 4 <= dst.len() {
                dst[off..off + 4].copy_from_slice(&s32.to_le_bytes());
            }
        } else {
            let s16 = wv_get_value_integer(s, &mut extra_gb, &mut crc_extra_bits, s_val as u32) as i16;
            let off = count * 2;
            if off + 2 <= dst.len() {
                dst[off..off + 2].copy_from_slice(&s16.to_le_bytes());
            }
        }

        count += 1;
    }

    if count < s.samples {
        let bpp = sample_fmt.bytes_per_sample();
        let off = count * bpp;
        if off < dst.len() {
            dst[off..].fill(0);
        }
    }

    Ok(())
}

fn parse_block_payload(
    s: &mut WavpackFrameContext,
    buf: &[u8],
    expected_samples: usize,
) -> Result<()> {
    if buf.len() < 12 {
        return Err(Error::invalid("block payload too short"));
    }

    s.decorr = [Decorr::default(); MAX_TERMS];
    s.ch = [WvChannel::default(); 2];
    s.extra_bits = 0;
    s.and = 0;
    s.or = 0;
    s.shift = 0;
    s.got_extra_bits = false;
    s.pcm_data.clear();
    s.dsd_data.clear();
    s.extra_bits_data.clear();

    s.samples = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
    if s.samples != expected_samples {
        return Err(Error::invalid("mismatching number of samples in block"));
    }
    s.frame_flags = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    s.crc = u32::from_le_bytes(buf[8..12].try_into().unwrap());

    let orig_bpp = (((s.frame_flags & 0x03) + 1) * 8) as usize;
    let bpp = if (s.frame_flags & WV_DSD_DATA) != 0 || (s.frame_flags & WV_FLOAT_DATA) != 0 {
        4
    } else if (s.frame_flags & 0x03) <= 1 {
        2
    } else {
        4
    };

    s.stereo = (s.frame_flags & WV_MONO) == 0;
    s.stereo_in = if (s.frame_flags & WV_FALSE_STEREO) != 0 {
        false
    } else {
        s.stereo
    };
    s.joint = (s.frame_flags & WV_JOINT_STEREO) != 0;
    s.hybrid = (s.frame_flags & WV_HYBRID_MODE) != 0;
    s.hybrid_bitrate = (s.frame_flags & WV_HYBRID_BITRATE) != 0;

    let pshift = (bpp * 8) as i32 - orig_bpp as i32 + ((s.frame_flags as i32 >> 13) & 0x1f);
    if !(0..=31).contains(&pshift) {
        return Err(Error::invalid("invalid post shift"));
    }
    s.post_shift = pshift as usize;

    s.hybrid_maxclip = ((1i64 << (orig_bpp - 1)) - 1) as i32;
    s.hybrid_minclip = (-1i64 << (orig_bpp - 1)) as i32;

    let mut got_terms = false;
    let mut got_weights = false;
    let mut got_samples = false;
    let mut got_entropy = false;
    let mut got_hybrid = false;
    let mut got_float = false;
    let mut got_pcm = false;
    let mut got_dsd = false;

    let mut cursor = 12;
    while cursor < buf.len() {
        let id_byte = buf[cursor];
        cursor += 1;
        if cursor >= buf.len() {
            break;
        }
        let mut size = buf[cursor] as usize;
        cursor += 1;
        if (id_byte & WP_IDF_LONG) != 0 {
            if cursor + 2 > buf.len() {
                break;
            }
            size |= (u16::from_le_bytes(buf[cursor..cursor + 2].try_into().unwrap()) as usize) << 8;
            cursor += 2;
        }
        let raw_size = size as i32;
        let ssize = (raw_size << 1) as usize;
        let size = (raw_size << 1) - if (id_byte & WP_IDF_ODD) != 0 { 1 } else { 0 };
        if size < 0 || cursor + ssize > buf.len() {
            break;
        }
        let size = size as usize;

        let sub_data = &buf[cursor..cursor + size];
        let sub_id = id_byte & WP_IDF_MASK;

        match sub_id {
            WP_ID_DECTERMS => {
                if size > MAX_TERMS {
                    s.terms = 0;
                    cursor += ssize;
                    continue;
                }
                s.terms = size;
                for i in 0..s.terms {
                    let val = sub_data[i];
                    s.decorr[s.terms - i - 1].value = (val as i32 & 0x1F) - 5;
                    s.decorr[s.terms - i - 1].delta = (val as i32) >> 5;
                }
                got_terms = true;
            }
            WP_ID_DECWEIGHTS => {
                if !got_terms {
                    cursor += ssize;
                    continue;
                }
                let weights = size >> (if s.stereo_in { 1 } else { 0 });
                if weights > MAX_TERMS || weights > s.terms {
                    cursor += ssize;
                    continue;
                }
                let mut c = 0;
                for i in 0..weights {
                    let t = sub_data[c] as i8 as i32;
                    c += 1;
                    s.decorr[s.terms - i - 1].weight_a = t * 8;
                    if s.decorr[s.terms - i - 1].weight_a > 0 {
                        s.decorr[s.terms - i - 1].weight_a +=
                            (s.decorr[s.terms - i - 1].weight_a + 64) >> 7;
                    }
                    if s.stereo_in {
                        let t = sub_data[c] as i8 as i32;
                        c += 1;
                        s.decorr[s.terms - i - 1].weight_b = t * 8;
                        if s.decorr[s.terms - i - 1].weight_b > 0 {
                            s.decorr[s.terms - i - 1].weight_b +=
                                (s.decorr[s.terms - i - 1].weight_b + 64) >> 7;
                        }
                    }
                }
                got_weights = true;
            }
            WP_ID_DECSAMPLES => {
                if !got_terms {
                    cursor += ssize;
                    continue;
                }
                let mut c = 0;
                for i in (0..s.terms).rev() {
                    if c >= size {
                        break;
                    }
                    let val = s.decorr[i].value;
                    if val > 8 {
                        if c + 4 > size {
                            break;
                        }
                        s.decorr[i].samples_a[0] = wp_exp2(i16::from_le_bytes([sub_data[c], sub_data[c + 1]]));
                        s.decorr[i].samples_a[1] = wp_exp2(i16::from_le_bytes([sub_data[c + 2], sub_data[c + 3]]));
                        c += 4;
                        if s.stereo_in {
                            if c + 4 > size {
                                break;
                            }
                            s.decorr[i].samples_b[0] = wp_exp2(i16::from_le_bytes([sub_data[c], sub_data[c + 1]]));
                            s.decorr[i].samples_b[1] = wp_exp2(i16::from_le_bytes([sub_data[c + 2], sub_data[c + 3]]));
                            c += 4;
                        }
                    } else if val < 0 {
                        if c + 4 > size {
                            break;
                        }
                        s.decorr[i].samples_a[0] = wp_exp2(i16::from_le_bytes([sub_data[c], sub_data[c + 1]]));
                        s.decorr[i].samples_b[0] = wp_exp2(i16::from_le_bytes([sub_data[c + 2], sub_data[c + 3]]));
                        c += 4;
                    } else {
                        for j in 0..val as usize {
                            if c + 2 > size {
                                break;
                            }
                            s.decorr[i].samples_a[j] = wp_exp2(i16::from_le_bytes([sub_data[c], sub_data[c + 1]]));
                            c += 2;
                            if s.stereo_in {
                                if c + 2 > size {
                                    break;
                                }
                                s.decorr[i].samples_b[j] = wp_exp2(i16::from_le_bytes([sub_data[c], sub_data[c + 1]]));
                                c += 2;
                            }
                        }
                    }
                }
                got_samples = true;
            }
            WP_ID_ENTROPY => {
                let expected = 6 * (if s.stereo_in { 2 } else { 1 });
                if size != expected {
                    cursor += ssize;
                    continue;
                }
                let mut c = 0;
                let ch_count = if s.stereo_in { 2 } else { 1 };
                for j in 0..ch_count {
                    for m in 0..3 {
                        s.ch[j].median[m] = wp_exp2(i16::from_le_bytes([sub_data[c], sub_data[c + 1]]));
                        c += 2;
                    }
                }
                got_entropy = true;
            }
            WP_ID_HYBRID => {
                let mut c = 0;
                let mut rem_size = size;
                let ch_count = if s.stereo_in { 2 } else { 1 };
                if s.hybrid_bitrate {
                    for i in 0..ch_count {
                        if rem_size < 2 {
                            break;
                        }
                        s.ch[i].slow_level = wp_exp2(i16::from_le_bytes([sub_data[c], sub_data[c + 1]]));
                        c += 2;
                        rem_size -= 2;
                    }
                }
                for i in 0..ch_count {
                    if rem_size < 2 {
                        break;
                    }
                    s.ch[i].bitrate_acc = (u16::from_le_bytes([sub_data[c], sub_data[c + 1]]) as u32) << 16;
                    c += 2;
                    rem_size -= 2;
                }
                if rem_size > 0 {
                    for i in 0..ch_count {
                        if rem_size >= 2 {
                            s.ch[i].bitrate_delta = wp_exp2(i16::from_le_bytes([sub_data[c], sub_data[c + 1]])) as u32;
                            c += 2;
                            rem_size -= 2;
                        }
                    }
                } else {
                    for i in 0..ch_count {
                        s.ch[i].bitrate_delta = 0;
                    }
                }
                got_hybrid = true;
            }
            WP_ID_INT32INFO => {
                if size != 4 {
                    cursor += ssize;
                    continue;
                }
                let val0 = sub_data[0] as usize;
                if val0 > 30 {
                    cursor += ssize;
                    continue;
                }
                s.extra_bits = val0;
                if sub_data[1] != 0 {
                    s.shift = sub_data[1] as usize;
                }
                if sub_data[2] != 0 {
                    s.and = 1;
                    s.or = 1;
                    s.shift = sub_data[2] as usize;
                }
                if sub_data[3] != 0 {
                    s.and = 1;
                    s.shift = sub_data[3] as usize;
                }
                if s.shift > 31 {
                    s.and = 0;
                    s.or = 0;
                    s.shift = 0;
                    cursor += ssize;
                    continue;
                }
                if s.hybrid && bpp == 4 && s.post_shift < 8 && s.shift > 8 {
                    s.post_shift += 8;
                    s.shift -= 8;
                    s.hybrid_maxclip >>= 8;
                    s.hybrid_minclip >>= 8;
                }
            }
            WP_ID_FLOATINFO => {
                if size != 4 {
                    cursor += ssize;
                    continue;
                }
                s.float_flag = sub_data[0];
                let fshift = sub_data[1] as usize;
                s.float_max_exp = sub_data[2] as i32;
                if fshift > 31 {
                    s.float_shift = 0;
                    cursor += ssize;
                    continue;
                }
                s.float_shift = fshift;
                got_float = true;
            }
            WP_ID_DATA => {
                s.pcm_data = sub_data.to_vec();
                got_pcm = true;
            }
            WP_ID_DSD_DATA => {
                if size < 2 {
                    cursor += ssize;
                    continue;
                }
                let rate_x = sub_data[0];
                if rate_x > 30 {
                    return Err(Error::invalid("invalid rate_x"));
                }
                s.rate_x = 1 << rate_x;
                let dsd_mode = sub_data[1];
                if dsd_mode != 0 && dsd_mode != 1 && dsd_mode != 3 {
                    return Err(Error::invalid("invalid DSD mode"));
                }
                s.dsd_mode = dsd_mode;
                s.dsd_data = sub_data[2..].to_vec();
                got_dsd = true;
            }
            WP_ID_EXTRABITS => {
                if size > 4 {
                    s.crc_extra_bits = u32::from_le_bytes(sub_data[0..4].try_into().unwrap());
                    s.extra_bits_data = sub_data[4..].to_vec();
                    s.got_extra_bits = true;
                }
            }
            WP_ID_CHANINFO => {
                if size > 1 {
                    let mut chan = sub_data[0] as u16;
                    let mut chmask = 0u64;
                    match size - 2 {
                        0 => chmask = sub_data[1] as u64,
                        1 => chmask = u16::from_le_bytes([sub_data[1], sub_data[2]]) as u64,
                        2 => chmask = (sub_data[1] as u64) | ((sub_data[2] as u64) << 8) | ((sub_data[3] as u64) << 16),
                        3 => chmask = u32::from_le_bytes(sub_data[1..5].try_into().unwrap()) as u64,
                        4 => {
                            chan |= ((sub_data[2] & 0xF) as u16) << 8;
                            chan += 1;
                            chmask = (sub_data[3] as u64) | ((sub_data[4] as u64) << 8) | ((sub_data[5] as u64) << 16);
                        }
                        5 => {
                            chan |= ((sub_data[2] & 0xF) as u16) << 8;
                            chan += 1;
                            chmask = u32::from_le_bytes(sub_data[3..7].try_into().unwrap()) as u64;
                        }
                        _ => {}
                    }
                    s.custom_chan = chan;
                    s.custom_chmask = chmask;
                }
            }
            WP_ID_SAMPLE_RATE => {
                if size != 3 {
                    return Err(Error::invalid("invalid custom sample rate"));
                }
                s.custom_sample_rate = (sub_data[0] as u32) | ((sub_data[1] as u32) << 8) | ((sub_data[2] as u32) << 16);
            }
            _ => {}
        }

        cursor += ssize;
    }

    if got_pcm {
        if !got_terms || !got_weights || !got_samples || !got_entropy {
            return Err(Error::invalid("missing PCM decorrelation blocks"));
        }
        if s.hybrid && !got_hybrid {
            return Err(Error::invalid("hybrid config not found"));
        }
        if (s.frame_flags & WV_FLOAT_DATA) != 0 && !got_float {
            return Err(Error::invalid("float info not found"));
        }
    } else if !got_dsd {
        return Err(Error::invalid("no packed samples in block"));
    }

    if s.got_extra_bits && (s.frame_flags & WV_FLOAT_DATA) == 0 {
        let wanted = (s.samples * s.extra_bits) << (if s.stereo_in { 1 } else { 0 });
        if s.extra_bits_data.len() * 8 < wanted {
            s.got_extra_bits = false;
        }
    }

    Ok(())
}

pub struct WavpackDecoder {
    output_format: Option<AudioFormat>,
    dsd_contexts: Vec<DsdContext>,
    dsd_channels: usize,
    dsd_rate: u32,
    last_dsd_chmask: u64,
    last_modulation: Option<Modulation>,
    pending_frames: VecDeque<Frame>,
}

impl WavpackDecoder {
    pub fn new(_params: &CodecParameters) -> Result<Self> {
        Ok(Self {
            output_format: None,
            dsd_contexts: Vec::new(),
            dsd_channels: 0,
            dsd_rate: 0,
            last_dsd_chmask: 0,
            last_modulation: None,
            pending_frames: VecDeque::new(),
        })
    }

    pub fn decode_packet(&mut self, packet: &Packet) -> Result<()> {
        let buf = &packet.data;
        if buf.len() <= WV_HEADER_SIZE {
            return Err(Error::invalid("packet too short for wavpack header"));
        }

        let samples = u32::from_le_bytes(buf[20..24].try_into().unwrap()) as usize;
        let frame_flags = u32::from_le_bytes(buf[24..28].try_into().unwrap());
        if samples == 0 || samples > WV_MAX_SAMPLES as usize {
            return Err(Error::invalid("invalid sample count"));
        }

        let is_dsd = (frame_flags & WV_DSD_DATA) != 0;
        let is_float = (frame_flags & WV_FLOAT_DATA) != 0;
        let sample_fmt = if is_dsd || is_float {
            SampleFormat::F32P
        } else if (frame_flags & 0x03) <= 1 {
            SampleFormat::S16P
        } else {
            SampleFormat::S32P
        };

        let multiblock = (frame_flags & WV_SINGLE_BLOCK) != WV_SINGLE_BLOCK;

        let mut block_contexts = Vec::new();
        let mut total_channels = 0usize;
        let mut sample_rate = 0u32;

        let mut cur_buf = &buf[..];
        while cur_buf.len() > WV_HEADER_SIZE {
            let blocksize = u32::from_le_bytes(cur_buf[4..8].try_into().unwrap()) as usize;
            if blocksize < 24 || blocksize > WV_BLOCK_LIMIT as usize {
                return Err(Error::invalid("invalid blocksize in packet"));
            }
            let frame_size = blocksize - 12;
            cur_buf = &cur_buf[20..];
            if frame_size > cur_buf.len() {
                return Err(Error::invalid("block size exceeds packet data"));
            }
            let block_payload = &cur_buf[..frame_size];
            cur_buf = &cur_buf[frame_size..];

            let mut fctx = WavpackFrameContext::default();
            parse_block_payload(&mut fctx, block_payload, samples)?;

            let block_channels = if fctx.stereo { 2 } else { 1 };
            total_channels += block_channels;

            if total_channels > WV_MAX_CHANNELS {
                return Err(Error::invalid("too many channels in packet"));
            }

            if block_contexts.is_empty() {
                let sr_idx = ((fctx.frame_flags >> 23) & 0xF) as usize;
                let rate = if sr_idx == 0xF {
                    if fctx.custom_sample_rate == 0 {
                        return Err(Error::invalid("custom sample rate missing"));
                    }
                    fctx.custom_sample_rate
                } else {
                    WV_RATES[sr_idx] as u32
                };
                let rate_x = if fctx.rate_x > 0 { fctx.rate_x } else { 1 };
                let rate_prod = (rate as u64) * (rate_x as u64);
                if rate_prod > i32::MAX as u64 {
                    return Err(Error::invalid("sample rate too high"));
                }
                sample_rate = rate_prod as u32;
            }

            block_contexts.push(fctx);
        }

        if block_contexts.is_empty() {
            return Err(Error::invalid("no blocks found in packet"));
        }

        if multiblock {
            if block_contexts[0].custom_chmask != 0 {
                let mask_ch = block_contexts[0].custom_chmask.count_ones() as usize;
                if block_contexts[0].custom_chan != 0 && mask_ch != block_contexts[0].custom_chan as usize {
                    return Err(Error::invalid("channel mask does not match channel count"));
                }
                total_channels = mask_ch;
            } else if block_contexts[0].custom_chan > 0 {
                total_channels = block_contexts[0].custom_chan as usize;
            }
        }
        if total_channels == 0 || total_channels > WV_MAX_CHANNELS {
            return Err(Error::invalid("invalid channel count"));
        }

        let out_fmt = AudioFormat {
            sample_format: sample_fmt,
            sample_rate,
            channels: total_channels as u16,
        };
        self.output_format = Some(out_fmt);

        let bytes_per_sample = sample_fmt.bytes_per_sample();
        let mut planes: Vec<Vec<u8>> = vec![vec![0u8; samples * bytes_per_sample]; total_channels];

        if is_dsd {
            let chmask = if multiblock { block_contexts[0].custom_chmask } else { 0 };
            let reset_dsd = self.last_modulation == Some(Modulation::Pcm)
                || self.dsd_channels != total_channels
                || self.dsd_rate != sample_rate
                || self.last_dsd_chmask != chmask;

            if reset_dsd {
                self.dsd_contexts = vec![DsdContext::default(); total_channels];
                self.dsd_channels = total_channels;
                self.dsd_rate = sample_rate;
                self.last_dsd_chmask = chmask;
            }
            self.last_modulation = Some(Modulation::Dsd);

            let mut dsd_scratch = vec![0x69u8; samples * total_channels];
            let mut ch_off = 0usize;

            for fctx in &block_contexts {
                let num_block_channels = if fctx.stereo { 2 } else { 1 };
                if ch_off + num_block_channels > total_channels {
                    return Err(Error::invalid("too many channels coded in a packet"));
                }
                let off_l = ch_off;
                let off_r = if fctx.stereo { Some(ch_off + 1) } else { None };

                match fctx.dsd_mode {
                    3 => {
                        wv_unpack_dsd_high(
                            &fctx.dsd_data,
                            samples,
                            fctx.stereo_in,
                            &mut dsd_scratch,
                            off_l,
                            off_r,
                            total_channels,
                            fctx.crc,
                        )?;
                    }
                    1 => {
                        wv_unpack_dsd_fast(
                            &fctx.dsd_data,
                            samples,
                            fctx.stereo_in,
                            &mut dsd_scratch,
                            off_l,
                            off_r,
                            total_channels,
                            fctx.crc,
                        )?;
                    }
                    _ => {
                        wv_unpack_dsd_copy(
                            &fctx.dsd_data,
                            samples,
                            fctx.stereo_in,
                            &mut dsd_scratch,
                            off_l,
                            off_r,
                            total_channels,
                            fctx.crc,
                        )?;
                    }
                }

                if fctx.stereo && !fctx.stereo_in {
                    if ch_off + 1 >= total_channels {
                        return Err(Error::invalid("false stereo exceeds channel count"));
                    }
                    for s_i in 0..samples {
                        dsd_scratch[(ch_off + 1) + s_i * total_channels] =
                            dsd_scratch[ch_off + s_i * total_channels];
                    }
                }

                ch_off += num_block_channels;
            }

            if ch_off != total_channels {
                return Err(Error::invalid("not enough channels coded in a packet"));
            }

            for ch in 0..total_channels {
                let mut float_samples = vec![0.0f32; samples];
                dsd2pcm_translate(
                    &mut self.dsd_contexts[ch],
                    &dsd_scratch[ch..],
                    total_channels,
                    samples,
                    &mut float_samples,
                );
                for (s_i, &val) in float_samples.iter().enumerate() {
                    let b = val.to_le_bytes();
                    planes[ch][s_i * 4..s_i * 4 + 4].copy_from_slice(&b);
                }
            }
        } else {
            if self.last_modulation == Some(Modulation::Dsd) {
                self.dsd_contexts.clear();
                self.dsd_channels = 0;
            }
            self.last_modulation = Some(Modulation::Pcm);

            let mut ch_off = 0usize;
            for mut fctx in block_contexts {
                let num_block_channels = if fctx.stereo { 2 } else { 1 };
                if ch_off + num_block_channels > total_channels {
                    return Err(Error::invalid("too many channels coded in a packet"));
                }
                let pcm_data = std::mem::take(&mut fctx.pcm_data);
                let extra_bits_data = std::mem::take(&mut fctx.extra_bits_data);
                let mut gb = BitReaderLe::new(&pcm_data);
                let extra_gb = if fctx.got_extra_bits {
                    Some(BitReaderLe::new(&extra_bits_data))
                } else {
                    None
                };

                if fctx.stereo_in {
                    let (left_plane, rest) = planes.split_at_mut(ch_off + 1);
                    let dst_l = &mut left_plane[ch_off];
                    let dst_r = &mut rest[0];
                    wv_unpack_stereo(&mut fctx, &mut gb, extra_gb, dst_l, dst_r, sample_fmt)?;
                } else {
                    let dst = &mut planes[ch_off];
                    wv_unpack_mono(&mut fctx, &mut gb, extra_gb, dst, sample_fmt)?;
                    if fctx.stereo {
                        let (left, right) = planes.split_at_mut(ch_off + 1);
                        right[0].copy_from_slice(&left[ch_off]);
                    }
                }

                ch_off += num_block_channels;
            }

            if ch_off != total_channels {
                return Err(Error::invalid("not enough channels coded in a packet"));
            }
        }

        let audio_frame = AudioFrame {
            samples: samples as u32,
            pts: packet.pts,
            data: planes,
        };

        self.pending_frames.push_back(Frame::Audio(audio_frame));

        Ok(())
    }
}

impl Decoder for WavpackDecoder {
    fn codec_id(&self) -> &CodecId {
        static ID: LazyLock<CodecId> = LazyLock::new(|| CodecId::new("wavpack"));
        &ID
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.decode_packet(packet)
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.pending_frames.pop_front().ok_or(Error::NeedMore)
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        self.output_format
    }

    fn flush(&mut self) -> Result<()> {
        self.pending_frames.clear();
        self.last_modulation = None;
        for ctx in &mut self.dsd_contexts {
            ctx.reset();
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.flush()
    }
}

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(WavpackDecoder::new(params)?))
}
