//! The integer mixer, ported from libopenmpt 0.8.9 `soundlib/Fastmix.cpp`,
//! `IntMixer.h`, `MixerInterface.h`, `MixerLoops.cpp`, `Resampler.h`,
//! `WindowedFIR.cpp` and the sinc table generation in `Tables.cpp`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

#![allow(dead_code)]

use std::sync::LazyLock;

use crate::channel::{CurrentSample, ModChannel};
use crate::defs::pb::*;
use crate::defs::*;
use crate::player::Player;
use crate::sample::{SampleData, PRE_FRAMES};
use crate::tables::FAST_SINC_TABLE;

const SINC_PHASES_BITS: u32 = 12;
const SINC_PHASES: usize = 1 << SINC_PHASES_BITS;
const SINC_WIDTH: usize = 8;
const SINC_MASK: u32 = (SINC_PHASES - 1) as u32;
const SINC_QUANTSHIFT: u32 = 15;

const WFIR_QUANTBITS: i32 = 15;
const WFIR_QUANTSCALE: f64 = (1 << WFIR_QUANTBITS) as f64;
const WFIR_16BITSHIFT: i32 = WFIR_QUANTBITS;
const WFIR_FRACBITS: i32 = 12;
const WFIR_LUTLEN: usize = (1 << (WFIR_FRACBITS + 1)) + 1;
const WFIR_LOG2WIDTH: i32 = 3;
const WFIR_WIDTH: usize = 1 << WFIR_LOG2WIDTH;
const WFIR_FRACSHIFT: i32 = 16 - (WFIR_FRACBITS + 1 + WFIR_LOG2WIDTH);
const WFIR_FRACMASK: u32 = (((1u32 << (17 - WFIR_FRACSHIFT)) - 1) & !(WFIR_WIDTH as u32 - 1)) as u32;
const WFIR_FRACHALVE: u32 = 1 << (16 - (WFIR_FRACBITS + 2));
const WFIR_KAISER4T: u8 = 7;

pub struct Resampler {
    pub kaiser: Vec<i16>,
    pub down13: Vec<i16>,
    pub down2: Vec<i16>,
    pub wfir: Vec<i16>,
}

fn izero(y: f64) -> f64 {
    let mut s = 1.0f64;
    let mut ds = 1.0f64;
    let mut d = 0.0f64;
    loop {
        d += 2.0;
        ds = ds * (y * y) / (d * d);
        s += ds;
        if ds <= 1e-7 * s {
            break;
        }
    }
    s
}

fn saturate_round_i16(x: f64) -> i16 {
    let r = x.round();
    r.clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

fn getsinc(beta: f64, mut cutoff: f64) -> Vec<i16> {
    if cutoff >= 0.999 {
        cutoff = 0.999;
    }
    let izero_beta = izero(beta);
    let k_pi = 4.0 * 1.0f64.atan() * cutoff;
    let mut out = Vec::with_capacity(8 * SINC_PHASES);
    for isrc in 0..8 * SINC_PHASES as i32 {
        let mut ix = 7 - (isrc & 7);
        ix = (ix * SINC_PHASES as i32) + (isrc >> 3);
        let fsinc = if ix == 4 * SINC_PHASES as i32 {
            1.0
        } else {
            let x = (ix - 4 * SINC_PHASES as i32) as f64 * (1.0 / SINC_PHASES as f64);
            let x_pi = x * k_pi;
            x_pi.sin() * izero(beta * (1.0 - x * x * (1.0 / 16.0)).sqrt()) / (izero_beta * x_pi)
        };
        let coeff = fsinc * cutoff;
        out.push(saturate_round_i16(coeff * (1 << SINC_QUANTSHIFT) as f64));
    }
    out
}

fn wfir_coef(cnr: i32, ofs: f64, cut: f64, width: i32, ty: u8) -> f64 {
    let epsilon = 1e-8;
    let width_m1 = (width - 1) as f64;
    let width_m1_half = 0.5 * width_m1;
    let pos_u = cnr as f64 - ofs;
    let idl = (2.0 * core::f64::consts::PI) / width_m1;
    let mut pos = pos_u - width_m1_half;
    let (wc, si);
    if pos.abs() < epsilon {
        wc = 1.0;
        si = cut;
    } else {
        wc = match ty {
            WFIR_KAISER4T => {
                0.40243 - 0.49804 * (idl * pos_u).cos() + 0.09831 * (2.0 * idl * pos_u).cos() - 0.00122 * (3.0 * idl * pos_u).cos()
            }
            _ => 1.0,
        };
        pos *= core::f64::consts::PI;
        si = (cut * pos).sin() / pos;
    }
    wc * si
}

fn wfir_table(cutoff: f64, ty: u8) -> Vec<i16> {
    let pcllen = (1 << WFIR_FRACBITS) as f64;
    let norm = 1.0 / (2.0 * pcllen);
    let mut lut = vec![0i16; WFIR_LUTLEN * WFIR_WIDTH];
    for pcl in 0..WFIR_LUTLEN {
        let mut gain = 0.0;
        let mut coefs = [0.0f64; WFIR_WIDTH];
        let ofs = (pcl as f64 - pcllen) * norm;
        let idx = pcl << WFIR_LOG2WIDTH;
        for (cc, c) in coefs.iter_mut().enumerate() {
            *c = wfir_coef(cc as i32, ofs, cutoff, WFIR_WIDTH as i32, ty);
            gain += *c;
        }
        gain = 1.0 / gain;
        for cc in 0..WFIR_WIDTH {
            let coef = (0.5 + WFIR_QUANTSCALE * coefs[cc] * gain).floor();
            lut[idx + cc] = if coef < -WFIR_QUANTSCALE {
                -WFIR_QUANTSCALE as i32 as i16
            } else if coef > WFIR_QUANTSCALE {
                WFIR_QUANTSCALE as i32 as i16
            } else {
                coef as i32 as i16
            };
        }
    }
    lut
}

/// The resampler tables (`CResampler`), built once.
pub static RESAMPLER: LazyLock<Resampler> = LazyLock::new(|| Resampler {
    kaiser: getsinc(9.6377, 0.97),
    down13: getsinc(8.5, 0.5),
    down2: getsinc(7.0, 0.425),
    wfir: wfir_table(0.97, WFIR_KAISER4T),
});

/// Sample point types the kernels read.
trait SampleIn: Copy {
    fn conv(self) -> i32;
}
impl SampleIn for i8 {
    #[inline(always)]
    fn conv(self) -> i32 {
        (self as i32) * 256
    }
}
impl SampleIn for i16 {
    #[inline(always)]
    fn conv(self) -> i32 {
        self as i32
    }
}

const IP_NEAREST: u8 = 0;
const IP_LINEAR: u8 = 1;
const IP_CUBIC: u8 = 2;
const IP_KAISER: u8 = 3;
const IP_FIR: u8 = 4;

fn interp_index(mode: u8) -> u8 {
    match mode {
        SRCMODE_NEAREST => IP_NEAREST,
        SRCMODE_LINEAR => IP_LINEAR,
        SRCMODE_CUBIC => IP_CUBIC,
        SRCMODE_SINC8LP => IP_KAISER,
        SRCMODE_SINC8 => IP_FIR,
        _ => IP_NEAREST,
    }
}

const PREAMP: i32 = 256;
const FILTER_CLIP_MIN: i32 = i16::MIN as i32 * 2 * PREAMP;
const FILTER_CLIP_MAX: i32 = i16::MAX as i32 * 2 * PREAMP;

/// `SampleLoop<Traits, Interpolation, Filter, Mix>`: mixes `n` frames of
/// `data` (elements; `base` is the element index of sampling point 0) into
/// `out` (interleaved stereo).
#[inline(always)]
fn sample_loop<T: SampleIn, const NCH: usize, const IP: u8, const FILTER: bool, const RAMP: bool>(
    chn: &mut ModChannel,
    data: &[T],
    base: isize,
    rs: &Resampler,
    out: &mut [i32],
    n: usize,
) {
    let mut pos = chn.position;
    let inc = chn.increment;
    if IP == IP_NEAREST {
        pos += SamplePosition::ratio(1, 2);
    }
    let sinc: &[i16] = if IP == IP_KAISER {
        if inc > SamplePosition(0x1_3000_0000) || inc < SamplePosition(-0x1_3000_0000) {
            if inc > SamplePosition(0x1_8000_0000) || inc < SamplePosition(-0x1_8000_0000) { &rs.down2 } else { &rs.down13 }
        } else {
            &rs.kaiser
        }
    } else {
        &[]
    };
    let mut fy = chn.n_filter_y;
    let (a0, b0, b1, hp) = (chn.n_filter_a0 as i64, chn.n_filter_b0 as i64, chn.n_filter_b1 as i64, chn.n_filter_hp);
    let mut lramp = chn.ramp_left_vol;
    let mut rramp = chn.ramp_right_vol;
    let (lstep, rstep) = (chn.left_ramp, chn.right_ramp);
    let (lvol, rvol) = (chn.left_vol, chn.right_vol);
    let at = |i: isize| -> i32 { data[i as usize].conv() };
    for frame in 0..n {
        let p = base + pos.int() as isize * NCH as isize;
        let fract = pos.fract();
        let mut s = [0i32; 2];
        for c in 0..NCH {
            let i = p + c as isize;
            let nc = NCH as isize;
            s[c] = match IP {
                IP_NEAREST => at(i),
                IP_LINEAR => {
                    let f = (fract >> 18) as i32;
                    let src = at(i);
                    let dst = at(i + nc);
                    src.wrapping_add(f.wrapping_mul(dst.wrapping_sub(src)) / 16384)
                }
                IP_CUBIC => {
                    let o = ((fract >> 22) & 0x3FC) as usize;
                    let l = &FAST_SINC_TABLE[o..o + 4];
                    (l[0] as i32)
                        .wrapping_mul(at(i - nc))
                        .wrapping_add((l[1] as i32).wrapping_mul(at(i)))
                        .wrapping_add((l[2] as i32).wrapping_mul(at(i + nc)))
                        .wrapping_add((l[3] as i32).wrapping_mul(at(i + 2 * nc)))
                        / 16384
                }
                IP_KAISER => {
                    let o = (((fract >> (32 - SINC_PHASES_BITS)) & SINC_MASK) as usize) * SINC_WIDTH;
                    let l = &sinc[o..o + 8];
                    let mut acc = 0i32;
                    for k in 0..8 {
                        acc = acc.wrapping_add((l[k] as i32).wrapping_mul(at(i + (k as isize - 3) * nc)));
                    }
                    acc / (1 << SINC_QUANTSHIFT)
                }
                _ => {
                    let o = ((((fract >> 16) + WFIR_FRACHALVE) >> WFIR_FRACSHIFT) & WFIR_FRACMASK) as usize;
                    let l = &rs.wfir[o..o + 8];
                    let mut v1 = 0i32;
                    for k in 0..4 {
                        v1 = v1.wrapping_add((l[k] as i32).wrapping_mul(at(i + (k as isize - 3) * nc)));
                    }
                    let mut v2 = 0i32;
                    for k in 4..8 {
                        v2 = v2.wrapping_add((l[k] as i32).wrapping_mul(at(i + (k as isize - 3) * nc)));
                    }
                    ((v1 / 2).wrapping_add(v2 / 2)) / (1 << (WFIR_16BITSHIFT - 1))
                }
            };
        }
        if FILTER {
            for c in 0..NCH {
                let input = s[c].wrapping_mul(PREAMP);
                let v = (input as i64 * a0
                    + fy[c][0].clamp(FILTER_CLIP_MIN, FILTER_CLIP_MAX) as i64 * b0
                    + fy[c][1].clamp(FILTER_CLIP_MIN, FILTER_CLIP_MAX) as i64 * b1
                    + (1i64 << (MIXING_FILTER_PRECISION - 1)))
                    >> MIXING_FILTER_PRECISION;
                let v = v as i32;
                fy[c][1] = fy[c][0];
                fy[c][0] = v.wrapping_sub(input & hp);
                s[c] = v / PREAMP;
            }
        }
        let (sl, sr) = if NCH == 2 { (s[0], s[1]) } else { (s[0], s[0]) };
        let o = &mut out[frame * 2..frame * 2 + 2];
        if RAMP {
            lramp = lramp.wrapping_add(lstep);
            rramp = rramp.wrapping_add(rstep);
            o[0] = o[0].wrapping_add(sl.wrapping_mul(lramp >> VOLUMERAMPPRECISION));
            o[1] = o[1].wrapping_add(sr.wrapping_mul(rramp >> VOLUMERAMPPRECISION));
        } else {
            o[0] = o[0].wrapping_add(sl.wrapping_mul(lvol));
            o[1] = o[1].wrapping_add(sr.wrapping_mul(rvol));
        }
        pos += inc;
    }
    if IP == IP_NEAREST {
        pos -= SamplePosition::ratio(1, 2);
    }
    chn.position = pos;
    if FILTER {
        chn.n_filter_y = fy;
    }
    if RAMP {
        chn.ramp_left_vol = lramp;
        chn.left_vol = lramp >> VOLUMERAMPPRECISION;
        chn.ramp_right_vol = rramp;
        chn.right_vol = rramp >> VOLUMERAMPPRECISION;
    }
}

fn dispatch<T: SampleIn, const NCH: usize>(
    chn: &mut ModChannel,
    data: &[T],
    base: isize,
    rs: &Resampler,
    out: &mut [i32],
    n: usize,
    ip: u8,
    filter: bool,
    ramp: bool,
) {
    macro_rules! go {
        ($ip:expr) => {
            match (filter, ramp) {
                (false, false) => sample_loop::<T, NCH, { $ip }, false, false>(chn, data, base, rs, out, n),
                (false, true) => sample_loop::<T, NCH, { $ip }, false, true>(chn, data, base, rs, out, n),
                (true, false) => sample_loop::<T, NCH, { $ip }, true, false>(chn, data, base, rs, out, n),
                (true, true) => sample_loop::<T, NCH, { $ip }, true, true>(chn, data, base, rs, out, n),
            }
        };
    }
    match ip {
        IP_NEAREST => go!(IP_NEAREST),
        IP_LINEAR => go!(IP_LINEAR),
        IP_CUBIC => go!(IP_CUBIC),
        IP_KAISER => go!(IP_KAISER),
        _ => go!(IP_FIR),
    }
}

/// `StereoFill`: fills with the decaying click-removal offsets.
fn stereo_fill(buf: &mut [i32], rofs: &mut i32, lofs: &mut i32) {
    if *rofs == 0 && *lofs == 0 {
        buf.fill(0);
        return;
    }
    for f in buf.chunks_exact_mut(2) {
        let xr = (rofs.wrapping_add((rofs.wrapping_neg() >> 31) & 0xFF)) >> 8;
        let xl = (lofs.wrapping_add((lofs.wrapping_neg() >> 31) & 0xFF)) >> 8;
        *rofs -= xr;
        *lofs -= xl;
        f[0] = *rofs;
        f[1] = *lofs;
    }
}

/// `EndChannelOfs`.
fn end_channel_ofs(chn: &mut ModChannel, buf: &mut [i32]) {
    let mut rofs = chn.n_r_ofs;
    let mut lofs = chn.n_l_ofs;
    if rofs == 0 && lofs == 0 {
        return;
    }
    for f in buf.chunks_exact_mut(2) {
        let xr = (rofs.wrapping_add((rofs.wrapping_neg() >> 31) & 0xFF)) >> 8;
        let xl = (lofs.wrapping_add((lofs.wrapping_neg() >> 31) & 0xFF)) >> 8;
        rofs -= xr;
        lofs -= xl;
        f[0] = f[0].wrapping_add(rofs);
        f[1] = f[1].wrapping_add(lofs);
    }
    chn.n_r_ofs = rofs;
    chn.n_l_ofs = lofs;
}

/// `MixLoopState`.
struct MixLoopState {
    sample: Option<SampleIndex>,
    lookahead: Option<isize>,
    lookahead_start: SmpLength,
    max_samples: u32,
    it_ping_pong_diff: u32,
    precise_ping_pong_loops: bool,
}

fn distance_to_buffer_length(from: SamplePosition, to: SamplePosition, inc: SamplePosition) -> u32 {
    if from < to {
        if inc.0 == 0 {
            return 1;
        }
        ((to.0 - from.0 - 1) / inc.0) as u32 + 1
    } else {
        1
    }
}

impl MixLoopState {
    fn new(m: &crate::sndfile::Module, chn: &ModChannel) -> Self {
        let mut s = MixLoopState {
            sample: None,
            lookahead: None,
            lookahead_start: 0,
            max_samples: 0,
            it_ping_pong_diff: if m.behaviour(kITPingPongMode) { 1 } else { 0 },
            precise_ping_pong_loops: !m.behaviour(kImprecisePingPongLoops),
        };
        if chn.p_current_sample.is_none() {
            return s;
        }
        s.update_lookahead_pointers(m, chn);
        let mut inc = chn.increment;
        if inc.is_negative() {
            inc.negate();
        }
        s.max_samples = 16384u32 / inc.uint().wrapping_add(1);
        if s.max_samples < 2 {
            s.max_samples = 2;
        }
        s
    }

    fn update_lookahead_pointers(&mut self, m: &crate::sndfile::Module, chn: &ModChannel) {
        self.sample = chn.p_current_sample.map(|c| c.sample);
        self.lookahead = None;
        let Some(si) = self.sample else {
            return;
        };
        let lb = INTERPOLATION_LOOKAHEAD_BUFFER_SIZE;
        self.lookahead_start =
            if chn.n_loop_end < lb { chn.n_loop_start } else { chn.n_loop_start.max(chn.n_loop_end - lb) };
        if chn.has(CHN_LOOP) {
            let smp = &m.samples[si as usize];
            let in_sustain = chn.in_sustain_loop(m) && chn.n_loop_start == smp.n_sustain_start && chn.n_loop_end == smp.n_sustain_end;
            if in_sustain || (chn.n_loop_start == smp.n_loop_start && chn.n_loop_end == smp.n_loop_end) {
                let mut off = (3 * lb) as isize + smp.n_length as isize - chn.n_loop_end as isize;
                if in_sustain {
                    off += (4 * lb) as isize;
                }
                self.lookahead = Some(off);
            }
        }
    }

    fn sample_pointer(&self) -> Option<CurrentSample> {
        self.sample.map(|s| CurrentSample { sample: s, offset: 0 })
    }

    /// `GetSampleCount`.
    fn get_sample_count(&self, chn: &mut ModChannel, mut n_samples: u32) -> u32 {
        let n_loop_start: i32 = if chn.has(CHN_LOOP) { chn.n_loop_start as i32 } else { 0 };
        let mut n_inc = chn.increment;
        if n_samples == 0 || n_inc.is_zero() || chn.n_length == 0 || self.sample.is_none() {
            return 0;
        }
        chn.p_current_sample = self.sample_pointer();
        if chn.position.int() < n_loop_start {
            if n_inc.is_negative() {
                chn.position = SamplePosition::new(n_loop_start.wrapping_add(n_loop_start), 0) - chn.position;
                if chn.position.int() < n_loop_start
                    || chn.position.uint() >= (n_loop_start as u32).wrapping_add(chn.n_length) / 2
                {
                    chn.position.set(n_loop_start, 0);
                }
                if chn.has(CHN_PINGPONGLOOP) {
                    chn.reset_flag(CHN_PINGPONGFLAG);
                    n_inc.negate();
                    chn.increment = n_inc;
                } else {
                    chn.position.set_int(chn.n_length.wrapping_sub(1) as i32);
                }
                if !chn.has(CHN_LOOP) || chn.position.uint() >= chn.n_length {
                    chn.position.set(chn.n_length as i32, 0);
                    return 0;
                }
            } else if chn.position.int() < 0 {
                chn.position.set_int(0);
            }
        } else if chn.position.uint() >= chn.n_length {
            if !chn.has(CHN_LOOP) {
                return 0;
            }
            if chn.has(CHN_PINGPONGLOOP) {
                if n_inc.is_positive() {
                    n_inc.negate();
                    chn.increment = n_inc;
                }
                chn.set(CHN_PINGPONGFLAG);
                if self.precise_ping_pong_loops {
                    let overshoot = chn.position - SamplePosition::new(chn.n_length as i32, 0);
                    let loop_length = chn.n_loop_end.wrapping_sub(chn.n_loop_start).wrapping_sub(self.it_ping_pong_diff);
                    if overshoot.uint() < loop_length {
                        chn.position = SamplePosition::new(chn.n_length.wrapping_sub(self.it_ping_pong_diff) as i32, 0) - overshoot;
                    } else {
                        chn.position = SamplePosition::new(chn.n_loop_start as i32, 0);
                    }
                } else {
                    let inv = chn.position.inverted_fract();
                    chn.position = SamplePosition::new(
                        (chn.n_length as i32)
                            .wrapping_sub(chn.position.int().wrapping_sub(chn.n_length as i32))
                            .wrapping_sub(inv.int()),
                        inv.fract(),
                    );
                    if chn.position.uint() <= chn.n_loop_start || chn.position.uint() >= chn.n_length {
                        chn.position.set_int(chn.n_length.wrapping_sub(chn.n_length.min(self.it_ping_pong_diff + 1)) as i32);
                    }
                }
            } else {
                if n_inc.is_negative() {
                    n_inc.negate();
                    chn.increment = n_inc;
                }
                chn.position += SamplePosition::new(n_loop_start.wrapping_sub(chn.n_length as i32), 0);
                chn.set(CHN_WRAPPED_LOOP);
            }
        }

        let n_pos = chn.position;
        let n_pos_int = n_pos.uint();
        if n_pos.int() < n_loop_start {
            if n_pos.is_negative() || n_inc.is_negative() {
                return 0;
            }
        } else {
            if n_pos_int > chn.n_length {
                return 0;
            }
            if n_pos_int == chn.n_length && n_inc.is_positive() {
                return 0;
            }
        }
        let mut n_smp_count = n_samples;
        let mut n_inv = n_inc;
        if n_inc.is_negative() {
            n_inv.negate();
        }
        n_samples = n_samples.min(self.max_samples);
        let inc_samples = n_inc.mul((n_samples - 1) as i64);
        let n_pos_dest = (n_pos + inc_samples).int();
        let lb = INTERPOLATION_LOOKAHEAD_BUFFER_SIZE;
        let is_at_loop_start = n_pos_int >= chn.n_loop_start && n_pos_int < chn.n_loop_start.wrapping_add(lb);
        if !is_at_loop_start {
            chn.reset_flag(CHN_WRAPPED_LOOP);
        }
        let mut check_dest = true;
        if let Some(la) = self.lookahead {
            let si = self.sample.unwrap();
            if n_pos_int >= self.lookahead_start {
                if n_inc.is_negative() {
                    n_smp_count = distance_to_buffer_length(SamplePosition::new(self.lookahead_start as i32, 0), n_pos, n_inv);
                    chn.p_current_sample = Some(CurrentSample { sample: si, offset: la });
                } else if n_pos_int <= chn.n_loop_end {
                    n_smp_count = distance_to_buffer_length(n_pos, SamplePosition::new(chn.n_loop_end as i32, 0), n_inv);
                    chn.p_current_sample = Some(CurrentSample { sample: si, offset: la });
                } else {
                    n_smp_count = distance_to_buffer_length(n_pos, SamplePosition::new(chn.n_length as i32, 0), n_inv);
                }
                check_dest = false;
            } else if chn.has(CHN_WRAPPED_LOOP) && is_at_loop_start {
                n_smp_count = distance_to_buffer_length(n_pos, SamplePosition::new(n_loop_start + lb as i32, 0), n_inv);
                chn.p_current_sample = Some(CurrentSample { sample: si, offset: la + (chn.n_loop_end as isize - n_loop_start as isize) });
                check_dest = false;
            } else if n_inc.is_positive() && (n_pos_dest as u32) >= self.lookahead_start && n_smp_count > 1 {
                n_smp_count = distance_to_buffer_length(n_pos, SamplePosition::new(self.lookahead_start as i32, 0), n_inv);
                check_dest = false;
            }
        }
        if check_dest {
            if n_inc.is_negative() {
                if n_pos_dest < n_loop_start {
                    n_smp_count = distance_to_buffer_length(SamplePosition::new(n_loop_start, 0), n_pos, n_inv);
                }
            } else if n_pos_dest >= chn.n_length as i32 {
                n_smp_count = distance_to_buffer_length(n_pos, SamplePosition::new(chn.n_length as i32, 0), n_inv);
            }
        }
        n_smp_count.clamp(1, n_samples)
    }
}

impl Player {
    /// `CreateStereoMix`. With `dry`, nothing is mixed but every channel
    /// advances as if it were (seeking).
    pub fn create_stereo_mix(&mut self, count: usize, dry: bool) {
        if count == 0 {
            return;
        }
        let (mut r, mut l) = (self.dry_r_ofs, self.dry_l_ofs);
        stereo_fill(&mut self.mix_buffer[..count * 2], &mut r, &mut l);
        self.dry_r_ofs = r;
        self.dry_l_ofs = l;
        let mut mixed = 0usize;
        for i in 0..self.n_mix_channels {
            let ci = self.ps.chn_mix[i] as usize;
            let do_mix = !dry && mixed < self.settings.max_mix_channels;
            if self.mix_channel(count, ci, do_mix) {
                mixed += 1;
            }
        }
        self.n_mix_stat = self.n_mix_stat.max(mixed);
    }

    /// `MixChannel`.
    fn mix_channel(&mut self, count: usize, ci: usize, do_mix: bool) -> bool {
        let chn_has = {
            let c = &self.ps.chn[ci];
            c.p_current_sample.is_some() || c.n_l_ofs != 0 || c.n_r_ofs != 0
        };
        if !chn_has {
            return false;
        }
        let rs: &Resampler = &RESAMPLER;
        let sample_swap_ok = self.m.behaviour(kMODSampleSwap);
        let one_shot = self.m.behaviour(kMODOneShotLoops);
        let num_samples = self.m.num_samples;
        let m = &self.m;
        let chn = &mut self.ps.chn[ci];
        let mut pbuf = 0usize;
        if chn.is_paused {
            end_channel_ofs(chn, &mut self.mix_buffer[..count * 2]);
            self.dry_r_ofs = self.dry_r_ofs.wrapping_add(chn.n_r_ofs);
            self.dry_l_ofs = self.dry_l_ofs.wrapping_add(chn.n_l_ofs);
            chn.n_r_ofs = 0;
            chn.n_l_ofs = 0;
            return false;
        }
        let mut st = MixLoopState::new(m, chn);
        let mut add_to_mix = false;
        let mut nsamples = count as i32;
        loop {
            let mut nramp = nsamples as u32;
            if chn.n_ramp_length > 0 && nramp > chn.n_ramp_length {
                nramp = chn.n_ramp_length;
            }
            let n_smp_count = st.get_sample_count(chn, nramp);
            if n_smp_count == 0 {
                chn.p_current_sample = None;
                chn.n_length = 0;
                chn.position = SamplePosition(0);
                chn.n_ramp_length = 0;
                end_channel_ofs(chn, &mut self.mix_buffer[pbuf..count * 2]);
                self.dry_r_ofs = self.dry_r_ofs.wrapping_add(chn.n_r_ofs);
                self.dry_l_ofs = self.dry_l_ofs.wrapping_add(chn.n_l_ofs);
                chn.n_r_ofs = 0;
                chn.n_l_ofs = 0;
                chn.reset_flag(CHN_PINGPONGFLAG);
                break;
            }
            let n = n_smp_count as usize;
            if !do_mix || (chn.n_ramp_length == 0 && (chn.left_vol | chn.right_vol) == 0) {
                chn.position += chn.increment.mul(n as i64);
                chn.n_r_ofs = 0;
                chn.n_l_ofs = 0;
                pbuf += n * 2;
                add_to_mix = false;
            } else {
                let end = pbuf + n * 2;
                let out = &mut self.mix_buffer[pbuf..end];
                chn.n_r_ofs = out[n * 2 - 2].wrapping_neg();
                chn.n_l_ofs = out[n * 2 - 1].wrapping_neg();
                mix_samples(m, rs, chn, out, n);
                chn.n_r_ofs = chn.n_r_ofs.wrapping_add(out[n * 2 - 2]);
                chn.n_l_ofs = chn.n_l_ofs.wrapping_add(out[n * 2 - 1]);
                pbuf = end;
                add_to_mix = true;
            }
            nsamples -= n as i32;
            if chn.n_ramp_length != 0 {
                if chn.n_ramp_length <= n_smp_count {
                    chn.n_ramp_length = 0;
                    chn.left_vol = chn.new_left_vol;
                    chn.right_vol = chn.new_right_vol;
                    chn.right_ramp = 0;
                    chn.left_ramp = 0;
                    if chn.has(CHN_NOTEFADE) && chn.n_fade_out_vol == 0 {
                        chn.n_length = 0;
                        chn.p_current_sample = None;
                    }
                } else {
                    chn.n_ramp_length -= n_smp_count;
                }
            }
            let past_loop_end = chn.position.uint() >= chn.n_loop_end && chn.has(CHN_LOOP);
            let past_sample_end = chn.position.uint() >= chn.n_length && !chn.has(CHN_LOOP) && chn.n_length != 0 && chn.n_master_chn == 0;
            let do_swap = sample_swap_ok
                && chn.swap_sample_index != 0
                && chn.swap_sample_index <= num_samples
                && chn.p_mod_sample != Some(chn.swap_sample_index);
            if (past_loop_end || past_sample_end) && do_swap {
                let si = chn.swap_sample_index;
                let smp = &m.samples[si as usize];
                chn.p_mod_sample = Some(si);
                chn.p_current_sample = if smp.has_sample_data() { Some(CurrentSample { sample: si, offset: 0 }) } else { None };
                chn.dw_flags = (chn.dw_flags & CHN_CHANNELFLAGS) | smp.u_flags;
                if smp.u_flags & CHN_LOOP != 0 {
                    chn.n_length = smp.n_loop_end;
                } else if !one_shot {
                    chn.n_length = smp.n_length;
                } else {
                    chn.n_length = 0;
                }
                chn.n_loop_start = smp.n_loop_start;
                chn.n_loop_end = smp.n_loop_end;
                chn.position.set_int(chn.n_loop_start as i32);
                chn.swap_sample_index = 0;
                st.update_lookahead_pointers(m, chn);
                if chn.p_current_sample.is_none() {
                    break;
                }
            } else if past_loop_end && !do_swap && one_shot && chn.n_loop_start == 0 {
                chn.position.set_int(0);
                let le = chn.p_mod_sample.map_or(0, |s| m.samples[s as usize].n_loop_end);
                chn.n_loop_end = le;
                chn.n_length = le;
            }
            if nsamples <= 0 {
                break;
            }
        }
        chn.p_current_sample = st.sample_pointer();
        add_to_mix
    }
}

/// Picks the kernel for the channel and mixes `n` frames into `out`.
fn mix_samples(m: &crate::sndfile::Module, rs: &Resampler, chn: &mut ModChannel, out: &mut [i32], n: usize) {
    let Some(cs) = chn.p_current_sample else {
        return;
    };
    let smp = &m.samples[cs.sample as usize];
    let nch = smp.num_channels() as isize;
    let ip = interp_index(chn.resampling_mode);
    let filter = chn.has(CHN_FILTER);
    let ramp = chn.n_ramp_length != 0;
    // Bounds of the sampling points this run reads (taps reach 3 back and
    // 4 ahead); malformed state renders nothing rather than reading
    // outside the buffer.
    let first = chn.position.int() as i64;
    let last = (chn.position + chn.increment.mul(n as i64 - 1)).int() as i64;
    let lo = first.min(last) - 4;
    let hi = first.max(last) + 5;
    let base_frame = PRE_FRAMES as i64 + cs.offset as i64;
    let len_frames = match &smp.data {
        SampleData::I8(d) => d.len() as i64 / nch as i64,
        SampleData::I16(d) => d.len() as i64 / nch as i64,
        SampleData::None => 0,
    };
    if base_frame + lo < 0 || base_frame + hi > len_frames {
        chn.position += chn.increment.mul(n as i64);
        return;
    }
    let base = (base_frame as isize) * nch;
    match (&smp.data, nch) {
        (SampleData::I8(d), 1) => dispatch::<i8, 1>(chn, d, base, rs, out, n, ip, filter, ramp),
        (SampleData::I8(d), _) => dispatch::<i8, 2>(chn, d, base, rs, out, n, ip, filter, ramp),
        (SampleData::I16(d), 1) => dispatch::<i16, 1>(chn, d, base, rs, out, n, ip, filter, ramp),
        (SampleData::I16(d), _) => dispatch::<i16, 2>(chn, d, base, rs, out, n, ip, filter, ramp),
        (SampleData::None, _) => chn.position += chn.increment.mul(n as i64),
    }
}
