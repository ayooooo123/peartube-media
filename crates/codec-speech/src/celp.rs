// CELP and ACELP routines the speech decoders share.
//
// Ported from FFmpeg (commit 2da55bf), LGPL-2.1-or-later:
// libavutil/float_scalarproduct.c (ff_scalarproduct_float_c),
// libavcodec/celp_filters.c, acelp_filters.c, acelp_vectors.c,
// acelp_pitch_delay.c and lsp.c, and libavutil/ffmath.h (ff_exp10).

//! The float routines follow FFmpeg's C step by step. FFmpeg's arm64
//! builds use clang's default `-ffp-contract=on`: within one expression it
//! may fuse `a * b + c` into one multiply-add, the left operand's product
//! first where both operands are products (`a * b - c * d` is
//! `fma(a, b, -(c * d))`), and `x -= a * b` is `fma(-a, b, x)`. Whether it
//! does is the compiler's choice: a reduction loop it vectorizes keeps its
//! additions in order and so cannot fuse them. Each routine here does what
//! FFmpeg's compiled code does (read from the arm64 objects), `mul_add`
//! where that fuses; and arithmetic C does in double (an unsuffixed
//! literal or a double function in the expression) is done in double.

pub const PITCH_DELAY_MIN: i32 = 20;
pub const PITCH_DELAY_MAX: i32 = 143;
pub const MAX_LP_HALF_ORDER: usize = 10;
const M_LOG2_10: f64 = 3.321_928_094_887_362_347_87;

/// How many terms of a `sum += a[i] * b[i]` loop of `len` terms clang
/// vectorizes: the products of the first `len & !3` (once `len` is at
/// least 4) are rounded and then added in order; the rest are fused.
fn unfused_terms(len: usize) -> usize {
    if len >= 4 { len & !3 } else { 0 }
}

/// `ff_scalarproduct_float_c`
pub fn dot(a: &[f32], b: &[f32], len: usize) -> f32 {
    let split = unfused_terms(len);
    let mut p = 0.0f32;
    for i in 0..split {
        p += a[i] * b[i];
    }
    for i in split..len {
        p = a[i].mul_add(b[i], p);
    }
    p
}

/// `ff_exp10`
pub fn exp10(x: f64) -> f64 {
    (M_LOG2_10 * x).exp2()
}

/// `ff_celp_circ_addf` with `out` separate from `in`:
/// `out[k] = in[k] + fac * lagged[(k - lag) mod n]`.
pub fn circ_addf(out: &mut [f32], inp: &[f32], lagged: &[f32], lag: usize, fac: f32, n: usize) {
    for k in 0..n {
        let l = if k < lag { lagged[n + k - lag] } else { lagged[k - lag] };
        out[k] = fac.mul_add(l, inp[k]);
    }
}

/// `ff_celp_circ_addf(out, out, lagged, ...)`.
pub fn circ_addf_in_place(out: &mut [f32], lagged: &[f32], lag: usize, fac: f32, n: usize) {
    for k in 0..n {
        let l = if k < lag { lagged[n + k - lag] } else { lagged[k - lag] };
        out[k] = fac.mul_add(l, out[k]);
    }
}

/// `ff_celp_lp_synthesis_filterf` (its C path, four samples at a time):
/// the all-pole filter `out[n] = in[n] - sum(c[i - 1] * out[n - i])`,
/// where `out[n]` is `buf[at + n]` and the `order` samples before `at` are
/// the filter's memory. `order` is even and at least 4.
pub fn lp_synthesis_filterf(buf: &mut [f32], at: usize, c: &[f32], inp: &[f32], len: usize, order: usize) {
    debug_assert!(order % 2 == 0 && order >= 4 && at >= order);
    let a = c[0];
    let mut b = c[1];
    let mut cc = c[2];
    b = (-c[0]).mul_add(c[0], b);
    cc = (-c[1]).mul_add(c[0], cc);
    cc = (-c[0]).mul_add(b, cc);

    let mut old0 = buf[at - 4];
    let mut old1 = buf[at - 3];
    let mut old2 = buf[at - 2];
    let mut old3 = buf[at - 1];
    let mut n = 0;
    while n + 4 <= len {
        let o = at + n;
        let mut out0 = inp[n];
        let mut out1 = inp[n + 1];
        let mut out2 = inp[n + 2];
        let mut out3 = inp[n + 3];

        out0 = (-c[2]).mul_add(old1, out0);
        out1 = (-c[2]).mul_add(old2, out1);
        out2 = (-c[2]).mul_add(old3, out2);

        out0 = (-c[1]).mul_add(old2, out0);
        out1 = (-c[1]).mul_add(old3, out1);

        out0 = (-c[0]).mul_add(old3, out0);

        let mut val = c[3];
        out0 = (-val).mul_add(old0, out0);
        out1 = (-val).mul_add(old1, out1);
        out2 = (-val).mul_add(old2, out2);
        out3 = (-val).mul_add(old3, out3);

        let mut i = 5;
        while i < order {
            old3 = buf[o - i];
            val = c[i - 1];
            out0 = (-val).mul_add(old3, out0);
            out1 = (-val).mul_add(old0, out1);
            out2 = (-val).mul_add(old1, out2);
            out3 = (-val).mul_add(old2, out3);

            old2 = buf[o - i - 1];
            val = c[i];
            out0 = (-val).mul_add(old2, out0);
            out1 = (-val).mul_add(old3, out1);
            out2 = (-val).mul_add(old0, out2);
            out3 = (-val).mul_add(old1, out3);

            std::mem::swap(&mut old0, &mut old2);
            old1 = old3;
            i += 2;
        }

        let (tmp0, tmp1, tmp2) = (out0, out1, out2);
        out3 = (-a).mul_add(tmp2, out3);
        out2 = (-a).mul_add(tmp1, out2);
        out1 = (-a).mul_add(tmp0, out1);

        out3 = (-b).mul_add(tmp1, out3);
        out2 = (-b).mul_add(tmp0, out2);

        out3 = (-cc).mul_add(tmp0, out3);

        buf[o] = out0;
        buf[o + 1] = out1;
        buf[o + 2] = out2;
        buf[o + 3] = out3;
        (old0, old1, old2, old3) = (out0, out1, out2, out3);
        n += 4;
    }
    while n < len {
        let o = at + n;
        buf[o] = inp[n];
        for i in 1..=order {
            buf[o] = (-c[i - 1]).mul_add(buf[o - i], buf[o]);
        }
        n += 1;
    }
}

/// `ff_celp_lp_zero_synthesis_filterf`: `out[n] = in[n] + sum(c[i - 1] *
/// in[n - i])`, where `in[n]` is `inp[at + n]`.
pub fn lp_zero_synthesis_filterf(out: &mut [f32], c: &[f32], inp: &[f32], at: usize, len: usize, order: usize) {
    let split = unfused_terms(order);
    for n in 0..len {
        let mut v = inp[at + n];
        for i in 1..=split {
            v += c[i - 1] * inp[at + n - i];
        }
        for i in split + 1..=order {
            v = c[i - 1].mul_add(inp[at + n - i], v);
        }
        out[n] = v;
    }
}

/// `ff_acelp_interpolatef` within one buffer: `out[n]` is `buf[out_at +
/// n]` and `in[n]` is `buf[in_at + n]`; output written earlier in the loop
/// is input later, as in C.
#[allow(clippy::too_many_arguments)]
pub fn interpolatef(
    buf: &mut [f32],
    out_at: usize,
    in_at: usize,
    c: &[f32],
    precision: usize,
    frac_pos: usize,
    filter_length: usize,
    len: usize,
) {
    for n in 0..len {
        let mut idx = 0;
        let mut v = 0.0f32;
        let mut i = 0;
        while i < filter_length {
            v = buf[in_at + n + i].mul_add(c[idx + frac_pos], v);
            idx += precision;
            i += 1;
            v = buf[in_at + n - i].mul_add(c[idx - frac_pos], v);
        }
        buf[out_at + n] = v;
    }
}

/// `ff_acelp_apply_order_2_transfer_function` in place.
pub fn apply_order_2_transfer_function(buf: &mut [f32], zeros: &[f32; 2], poles: &[f32; 2], gain: f32, mem: &mut [f32; 2]) {
    for x in buf.iter_mut() {
        let tmp = (-poles[1]).mul_add(mem[1], gain.mul_add(*x, -(poles[0] * mem[0])));
        *x = zeros[1].mul_add(mem[1], zeros[0].mul_add(mem[0], tmp));
        mem[1] = mem[0];
        mem[0] = tmp;
    }
}

/// `ff_tilt_compensation`
pub fn tilt_compensation(mem: &mut f32, tilt: f32, samples: &mut [f32]) {
    let size = samples.len();
    let new_mem = samples[size - 1];
    for i in (1..size).rev() {
        samples[i] = (-tilt).mul_add(samples[i - 1], samples[i]);
    }
    samples[0] = (-tilt).mul_add(*mem, samples[0]);
    *mem = new_mem;
}

/// `ff_weighted_vector_sumf`; `out` may hold `in_a` (pass a copy).
pub fn weighted_vector_sumf(out: &mut [f32], in_a: &[f32], in_b: &[f32], wa: f32, wb: f32, len: usize) {
    for i in 0..len {
        out[i] = wa.mul_add(in_a[i], wb * in_b[i]);
    }
}

/// `ff_weighted_vector_sumf` with `out` as `in_a`.
pub fn weighted_vector_sumf_in_place(out: &mut [f32], in_b: &[f32], wa: f32, wb: f32, len: usize) {
    for i in 0..len {
        out[i] = wa.mul_add(out[i], wb * in_b[i]);
    }
}

/// `ff_adaptive_gain_control` in place.
pub fn adaptive_gain_control(buf: &mut [f32], speech_energ: f32, alpha: f32, gain_mem: &mut f32) {
    let postfilter_energ = dot(buf, buf, buf.len());
    let mut gain_scale_factor = 1.0f32;
    if postfilter_energ != 0.0 {
        gain_scale_factor = f64::from(speech_energ / postfilter_energ).sqrt() as f32;
    }
    gain_scale_factor = (f64::from(gain_scale_factor) * (1.0 - f64::from(alpha))) as f32;
    let mut mem = *gain_mem;
    for x in buf.iter_mut() {
        mem = alpha.mul_add(mem, gain_scale_factor);
        *x *= mem;
    }
    *gain_mem = mem;
}

/// `ff_adaptive_gain_control` with separate input and output.
pub fn adaptive_gain_control_from(out: &mut [f32], inp: &[f32], speech_energ: f32, alpha: f32, gain_mem: &mut f32) {
    out.copy_from_slice(inp);
    adaptive_gain_control(out, speech_energ, alpha, gain_mem);
}

/// `ff_scale_vector_to_given_sum_of_squares` in place.
pub fn scale_vector_to_given_sum_of_squares(buf: &mut [f32], sum_of_squares: f32) {
    let mut scalefactor = dot(buf, buf, buf.len());
    if scalefactor != 0.0 {
        scalefactor = f64::from(sum_of_squares / scalefactor).sqrt() as f32;
    }
    for x in buf.iter_mut() {
        *x *= scalefactor;
    }
}

/// `AMRFixed`: the algebraic codebook vector as pulses.
#[derive(Clone, Copy, Default)]
pub struct AmrFixed {
    pub n: usize,
    pub x: [i32; 10],
    pub y: [f32; 10],
    pub no_repeat_mask: u32,
    pub pitch_lag: i32,
    pub pitch_fac: f32,
}

/// `ff_set_fixed_vector`. FFmpeg asserts every pulse lies inside the
/// vector; the decoders only produce such pulses, and one that does not
/// is left out here.
pub fn set_fixed_vector(out: &mut [f32], f: &AmrFixed, scale: f32, size: usize) {
    for i in 0..f.n {
        let mut x = f.x[i];
        let repeats = (f.no_repeat_mask >> i) & 1 == 0;
        let mut y = f.y[i] * scale;
        if f.pitch_lag > 0 {
            if x < 0 || x as usize >= size {
                continue;
            }
            loop {
                out[x as usize] += y;
                y *= f.pitch_fac;
                x += f.pitch_lag;
                if !(x < size as i32 && repeats) {
                    break;
                }
            }
        }
    }
}

/// `ff_clear_fixed_vector`
pub fn clear_fixed_vector(out: &mut [f32], f: &AmrFixed, size: usize) {
    for i in 0..f.n {
        let mut x = f.x[i];
        let repeats = (f.no_repeat_mask >> i) & 1 == 0;
        if f.pitch_lag > 0 {
            if x < 0 || x as usize >= size {
                continue;
            }
            loop {
                out[x as usize] = 0.0;
                x += f.pitch_lag;
                if !(x < size as i32 && repeats) {
                    break;
                }
            }
        }
    }
}

/// `ff_decode_10_pulses_35bits`
pub fn decode_10_pulses_35bits(
    fixed_index: &[u16],
    f: &mut AmrFixed,
    gray_decode: &[u8; 8],
    half_pulse_count: usize,
    bits: u32,
) {
    let mask = (1u16 << bits) - 1;
    f.no_repeat_mask = 0;
    f.n = 2 * half_pulse_count;
    for i in 0..half_pulse_count {
        let pos1 = i32::from(gray_decode[usize::from(fixed_index[2 * i + 1] & mask)]) + i as i32;
        let pos2 = i32::from(gray_decode[usize::from(fixed_index[2 * i] & mask)]) + i as i32;
        let sign: f32 = if fixed_index[2 * i + 1] & (1 << bits) != 0 { -1.0 } else { 1.0 };
        f.x[2 * i + 1] = pos1;
        f.x[2 * i] = pos2;
        f.y[2 * i + 1] = sign;
        f.y[2 * i] = if pos2 < pos1 { -sign } else { sign };
    }
}

/// `ff_decode_pitch_lag`: (integer lag, fractional lag).
pub fn decode_pitch_lag(
    mut pitch_index: i32,
    prev_lag_int: i32,
    subframe: usize,
    third_as_first: bool,
    resolution: u32,
) -> (i32, i32) {
    if subframe == 0 || (subframe == 2 && third_as_first) {
        if pitch_index < 197 {
            pitch_index += 59;
        } else {
            pitch_index = 3 * pitch_index - 335;
        }
    } else if resolution == 4 {
        let search_range_min = (prev_lag_int - 5).clamp(PITCH_DELAY_MIN, PITCH_DELAY_MAX - 9);
        if pitch_index < 4 {
            pitch_index = 3 * (pitch_index + search_range_min) + 1;
        } else if pitch_index < 12 {
            pitch_index += 3 * search_range_min + 7;
        } else {
            pitch_index = 3 * (pitch_index + search_range_min - 6) + 1;
        }
    } else {
        pitch_index -= 1;
        if resolution == 5 {
            pitch_index += 3 * (prev_lag_int - 10).clamp(PITCH_DELAY_MIN, PITCH_DELAY_MAX - 19);
        } else {
            pitch_index += 3 * (prev_lag_int - 5).clamp(PITCH_DELAY_MIN, PITCH_DELAY_MAX - 9);
        }
    }
    let lag_int = (pitch_index * 10923) >> 15;
    (lag_int, pitch_index - 3 * lag_int - 1)
}

/// `ff_amr_set_fixed_gain`: the fixed gain, and the prediction error
/// history moved on by one subframe.
pub fn amr_set_fixed_gain(
    fixed_gain_factor: f32,
    fixed_mean_energy: f32,
    prediction_error: &mut [f32; 4],
    energy_mean: f32,
    pred_table: &[f32; 4],
) -> f32 {
    let predicted = dot(pred_table, prediction_error, 4) + energy_mean;
    let mean = if fixed_mean_energy != 0.0 { fixed_mean_energy } else { 1.0 };
    let val = (f64::from(fixed_gain_factor) * exp10(0.05 * f64::from(predicted)) / f64::from(mean.sqrt())) as f32;
    prediction_error.copy_within(1..4, 0);
    prediction_error[3] = (20.0 * f64::from(fixed_gain_factor.log10())) as f32;
    val
}

/// `ff_set_min_dist_lsf`
pub fn set_min_dist_lsf(lsf: &mut [f32], min_spacing: f64, size: usize) {
    let mut prev = 0.0f32;
    for v in &mut lsf[..size] {
        let floor = f64::from(prev) + min_spacing;
        if f64::from(*v) <= floor || v.is_nan() {
            *v = floor as f32;
        }
        prev = *v;
    }
}

/// `ff_acelp_lsf2lspd`: `lsp[i] = cos(2 pi lsf[i])`.
pub fn lsf2lspd(lsp: &mut [f64], lsf: &[f32], order: usize) {
    for i in 0..order {
        lsp[i] = (2.0 * std::f64::consts::PI * f64::from(lsf[i])).cos();
    }
}

/// `lsp2polyf` on `lsp[start]`, `lsp[start + 2]`, ...
fn lsp2polyf(lsp: &[f64], start: usize, f: &mut [f64], half_order: usize) {
    f[0] = 1.0;
    f[1] = -2.0 * lsp[start];
    for i in 2..=half_order {
        let val = -2.0 * lsp[start + 2 * i - 2];
        f[i] = val.mul_add(f[i - 1], 2.0 * f[i - 2]);
        for j in (2..i).rev() {
            f[j] += f[j - 1].mul_add(val, f[j - 2]);
        }
        f[1] += val;
    }
}

/// `ff_acelp_lspd2lpc`
pub fn lspd2lpc(lsp: &[f64], lpc: &mut [f32], half_order: usize) {
    let mut pa = [0.0f64; MAX_LP_HALF_ORDER + 1];
    let mut qa = [0.0f64; MAX_LP_HALF_ORDER + 1];
    lsp2polyf(lsp, 0, &mut pa, half_order);
    lsp2polyf(lsp, 1, &mut qa, half_order);
    for k in (0..half_order).rev() {
        let paf = pa[k + 1] + pa[k];
        let qaf = qa[k + 1] - qa[k];
        lpc[k] = (0.5 * (paf + qaf)) as f32;
        lpc[2 * half_order - 1 - k] = (0.5 * (paf - qaf)) as f32;
    }
}

/// `ff_amrwb_lsp2lpc`
pub fn amrwb_lsp2lpc(lsp: &[f64], lp: &mut [f32], order: usize) {
    let half = order >> 1;
    // qa[i] is buf[i + 1]; qa[-1] is 0.
    let mut buf = [0.0f64; MAX_LP_HALF_ORDER + 1];
    let mut pa = [0.0f64; MAX_LP_HALF_ORDER + 1];
    lsp2polyf(lsp, 0, &mut pa, half);
    lsp2polyf(lsp, 1, &mut buf[1..], half - 1);
    let last = lsp[order - 1];
    let mut j = order - 1;
    for i in 1..half {
        let paf = pa[i] * (1.0 + last);
        let qaf = (buf[i + 1] - buf[i - 1]) * (1.0 - last);
        lp[i - 1] = ((paf + qaf) * 0.5) as f32;
        lp[j - 1] = ((paf - qaf) * 0.5) as f32;
        j -= 1;
    }
    lp[half - 1] = ((1.0 + last) * pa[half] * 0.5) as f32;
    lp[order - 1] = last as f32;
}

/// `ff_amr_bit_reorder`: the frame's 16-bit fields from the bitstream,
/// as an order table (field size, field byte offset, then the stream
/// position of each bit, most significant first; zero ends it) lays them
/// out. Bits past the data read as zero.
pub fn amr_bit_reorder<T: Copy + Into<u32>>(out: &mut [u16], data: &[u8], order: &[T]) {
    out.fill(0);
    let mut k = 0;
    while let Some(&size) = order.get(k) {
        let size: u32 = size.into();
        if size == 0 {
            break;
        }
        let offset: u32 = order[k + 1].into();
        let mut field = 0u32;
        for &bit in &order[k + 2..k + 2 + size as usize] {
            let bit: u32 = bit.into();
            let byte = data.get((bit >> 3) as usize).copied().unwrap_or(0);
            field = (field << 1) | u32::from((byte >> (bit & 7)) & 1);
        }
        if let Some(slot) = out.get_mut((offset >> 1) as usize) {
            *slot = field as u16;
        }
        k += 2 + size as usize;
    }
}
