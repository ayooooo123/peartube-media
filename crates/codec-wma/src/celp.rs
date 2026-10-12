// Ported from FFmpeg (commit 2da55bf): libavcodec/celp_filters.c
// (ff_celp_lp_synthesis_filterf, ff_celp_lp_zero_synthesis_filterf),
// libavcodec/acelp_filters.c (ff_acelp_interpolatef,
// ff_acelp_apply_order_2_transfer_function, ff_tilt_compensation),
// libavcodec/acelp_vectors.c / acelp_vectors.h (AMRFixed,
// ff_set_fixed_vector, ff_weighted_vector_sumf), libavcodec/lsp.c
// (lsp2polyf, ff_acelp_lspd2lpc), libavcodec/sinewin_tablegen.h
// (ff_sine_window_init) and libavutil/float_scalarproduct.c
// (ff_scalarproduct_float_c).
// GNU Lesser General Public License 2.1 or later.

//! The CELP/ACELP float helpers WMA Voice calls. As in FFmpeg's arm64
//! builds (clang, `-ffp-contract=on`), each `a*b ± c` inside one C
//! expression is a single fused multiply-add (`mul_add`), except in a
//! `sum += a[i] * b[i]` loop clang vectorizes (see [`unfused_terms`]).

/// `AMRFixed`: a sparse fixed-codebook vector.
#[derive(Clone, Copy, Debug, Default)]
pub struct AmrFixed {
    pub n: usize,
    pub x: [i32; 10],
    pub y: [f32; 10],
    pub no_repeat_mask: i32,
    pub pitch_lag: i32,
    pub pitch_fac: f32,
}

/// How many leading terms of a `sum += a[i] * b[i]` loop of `len` terms
/// FFmpeg 2da55bf's arm64 build (clang, -O3) vectorizes: it multiplies
/// them four at a time and adds the products one by one in C order, so
/// each product is rounded before its addition. The remaining terms, and
/// all of them when `len < 4`, are fused multiply-adds. Read from
/// `ff_scalarproduct_float_c` and `ff_celp_lp_zero_synthesis_filterf` in
/// FFmpeg's objects.
fn unfused_terms(len: usize) -> usize {
    if len >= 4 { len & !3 } else { 0 }
}

/// `ff_scalarproduct_float_c`.
pub fn scalarproduct_float(v1: &[f32], v2: &[f32], len: usize) -> f32 {
    let split = unfused_terms(len);
    let mut p = 0f32;
    for i in 0..split {
        p += v1[i] * v2[i];
    }
    for i in split..len {
        p = v1[i].mul_add(v2[i], p);
    }
    p
}

/// `ff_celp_lp_synthesis_filterf` (the unrolled path FFmpeg compiles):
/// `out[o + n] = in[n] - Σ filter_coeffs[i-1] · out[o + n - i]`, with the
/// filter memory in `out[o - filter_length..o]`. `filter_length` is even
/// and at least 4.
pub fn celp_lp_synthesis_filterf(
    out: &mut [f32],
    o: usize,
    filter_coeffs: &[f32],
    input: &[f32],
    buffer_length: usize,
    filter_length: usize,
) {
    let a = filter_coeffs[0];
    let mut b = filter_coeffs[1];
    let mut c = filter_coeffs[2];
    b = (-filter_coeffs[0]).mul_add(filter_coeffs[0], b);
    c = (-filter_coeffs[1]).mul_add(filter_coeffs[0], c);
    c = (-filter_coeffs[0]).mul_add(b, c);

    let mut old_out0 = out[o - 4];
    let mut old_out1 = out[o - 3];
    let mut old_out2 = out[o - 2];
    let mut old_out3 = out[o - 1];
    let mut n = 0;
    while n + 4 <= buffer_length {
        let p = o + n;
        let mut out0 = input[n];
        let mut out1 = input[n + 1];
        let mut out2 = input[n + 2];
        let mut out3 = input[n + 3];

        out0 = (-filter_coeffs[2]).mul_add(old_out1, out0);
        out1 = (-filter_coeffs[2]).mul_add(old_out2, out1);
        out2 = (-filter_coeffs[2]).mul_add(old_out3, out2);

        out0 = (-filter_coeffs[1]).mul_add(old_out2, out0);
        out1 = (-filter_coeffs[1]).mul_add(old_out3, out1);

        out0 = (-filter_coeffs[0]).mul_add(old_out3, out0);

        let mut val = filter_coeffs[3];

        out0 = (-val).mul_add(old_out0, out0);
        out1 = (-val).mul_add(old_out1, out1);
        out2 = (-val).mul_add(old_out2, out2);
        out3 = (-val).mul_add(old_out3, out3);

        let mut i = 5;
        while i < filter_length {
            old_out3 = out[p - i];
            val = filter_coeffs[i - 1];

            out0 = (-val).mul_add(old_out3, out0);
            out1 = (-val).mul_add(old_out0, out1);
            out2 = (-val).mul_add(old_out1, out2);
            out3 = (-val).mul_add(old_out2, out3);

            old_out2 = out[p - i - 1];

            val = filter_coeffs[i];

            out0 = (-val).mul_add(old_out2, out0);
            out1 = (-val).mul_add(old_out3, out1);
            out2 = (-val).mul_add(old_out0, out2);
            out3 = (-val).mul_add(old_out1, out3);

            core::mem::swap(&mut old_out0, &mut old_out2);
            old_out1 = old_out3;
            i += 2;
        }

        let tmp0 = out0;
        let tmp1 = out1;
        let tmp2 = out2;

        out3 = (-a).mul_add(tmp2, out3);
        out2 = (-a).mul_add(tmp1, out2);
        out1 = (-a).mul_add(tmp0, out1);

        out3 = (-b).mul_add(tmp1, out3);
        out2 = (-b).mul_add(tmp0, out2);

        out3 = (-c).mul_add(tmp0, out3);

        out[p] = out0;
        out[p + 1] = out1;
        out[p + 2] = out2;
        out[p + 3] = out3;

        old_out0 = out0;
        old_out1 = out1;
        old_out2 = out2;
        old_out3 = out3;

        n += 4;
    }

    while n < buffer_length {
        let mut v = input[n];
        for i in 1..=filter_length {
            v = (-filter_coeffs[i - 1]).mul_add(out[o + n - i], v);
        }
        out[o + n] = v;
        n += 1;
    }
}

/// `ff_celp_lp_zero_synthesis_filterf`: `out[o + n] = in[i0 + n] +
/// Σ filter_coeffs[i-1] · in[i0 + n - i]`. FFmpeg's build vectorizes the
/// sum over `i` when `out` overlaps neither `in` nor the coefficients, as
/// in every WMA Voice call: the first [`unfused_terms`] products are
/// rounded and added in order, the rest fused.
pub fn celp_lp_zero_synthesis_filterf(
    out: &mut [f32],
    o: usize,
    filter_coeffs: &[f32],
    input: &[f32],
    i0: usize,
    buffer_length: usize,
    filter_length: usize,
) {
    let split = unfused_terms(filter_length);
    for n in 0..buffer_length {
        let mut v = input[i0 + n];
        for i in 1..=split {
            v += filter_coeffs[i - 1] * input[i0 + n - i];
        }
        for i in split + 1..=filter_length {
            v = filter_coeffs[i - 1].mul_add(input[i0 + n - i], v);
        }
        out[o + n] = v;
    }
}

/// `ff_acelp_interpolatef` on one buffer: `buf[out_off + n]` from
/// `buf[in_off + n ± i]`. Later outputs may read earlier ones.
#[allow(clippy::too_many_arguments)]
pub fn acelp_interpolatef(
    buf: &mut [f32],
    out_off: usize,
    in_off: usize,
    filter_coeffs: &[f32],
    precision: usize,
    frac_pos: usize,
    filter_length: usize,
    length: usize,
) {
    for n in 0..length {
        let mut idx = 0;
        let mut v = 0f32;
        let mut i = 0;
        while i < filter_length {
            v = buf[in_off + n + i].mul_add(filter_coeffs[idx + frac_pos], v);
            idx += precision;
            i += 1;
            v = buf[in_off + n - i].mul_add(filter_coeffs[idx - frac_pos], v);
        }
        buf[out_off + n] = v;
    }
}

/// `ff_acelp_apply_order_2_transfer_function`, in place.
pub fn acelp_apply_order_2_transfer_function(
    samples: &mut [f32],
    zero_coeffs: [f32; 2],
    pole_coeffs: [f32; 2],
    gain: f32,
    mem: &mut [f32; 2],
    n: usize,
) {
    for s in samples[..n].iter_mut() {
        let tmp = (-pole_coeffs[1]).mul_add(mem[1], gain.mul_add(*s, -(pole_coeffs[0] * mem[0])));
        *s = zero_coeffs[1].mul_add(mem[1], zero_coeffs[0].mul_add(mem[0], tmp));
        mem[1] = mem[0];
        mem[0] = tmp;
    }
}

/// `ff_tilt_compensation`.
pub fn tilt_compensation(mem: &mut f32, tilt: f32, samples: &mut [f32], size: usize) {
    let new_tilt_mem = samples[size - 1];
    for i in (1..size).rev() {
        samples[i] = (-tilt).mul_add(samples[i - 1], samples[i]);
    }
    samples[0] = (-tilt).mul_add(*mem, samples[0]);
    *mem = new_tilt_mem;
}

/// `ff_set_fixed_vector`. Pulses at or past `size` are dropped (FFmpeg
/// asserts they cannot occur).
pub fn set_fixed_vector(out: &mut [f32], fixed: &AmrFixed, scale: f32, size: usize) {
    for i in 0..fixed.n {
        let mut x = fixed.x[i];
        let repeats = (fixed.no_repeat_mask >> i) & 1 == 0;
        let mut y = fixed.y[i] * scale;

        if fixed.pitch_lag > 0 {
            if x < 0 || x as usize >= size {
                continue;
            }
            loop {
                out[x as usize] += y;
                y *= fixed.pitch_fac;
                x += fixed.pitch_lag;
                if !((x as usize) < size && repeats) {
                    break;
                }
            }
        }
    }
}

/// `ff_weighted_vector_sumf` with `out == in_a`.
pub fn weighted_vector_sumf_inplace(out: &mut [f32], in_b: &[f32], weight_coeff_a: f32, weight_coeff_b: f32, length: usize) {
    for (o, b) in out[..length].iter_mut().zip(&in_b[..length]) {
        *o = weight_coeff_a.mul_add(*o, weight_coeff_b * b);
    }
}

/// `lsp2polyf` (lsp.c); `lsp` holds the interleaved LSPs, every second one
/// used.
fn lsp2polyf(lsp: &[f64], f: &mut [f64], lp_half_order: usize) {
    f[0] = 1.0;
    f[1] = -2.0 * lsp[0];
    for i in 2..=lp_half_order {
        let val = -2.0 * lsp[2 * i - 2];
        f[i] = val.mul_add(f[i - 1], 2.0 * f[i - 2]);
        for j in (2..i).rev() {
            f[j] += f[j - 1].mul_add(val, f[j - 2]);
        }
        f[1] += val;
    }
}

/// `ff_acelp_lspd2lpc`: LSPs (cosine domain) to `2·lp_half_order` LPCs.
pub fn acelp_lspd2lpc(lsp: &[f64], lpc: &mut [f32], lp_half_order: usize) {
    let mut pa = [0f64; 11];
    let mut qa = [0f64; 11];
    lsp2polyf(lsp, &mut pa, lp_half_order);
    lsp2polyf(&lsp[1..], &mut qa, lp_half_order);
    let lpc2 = (lp_half_order << 1) - 1;
    for k in (0..lp_half_order).rev() {
        let paf = pa[k + 1] + pa[k];
        let qaf = qa[k + 1] - qa[k];
        lpc[k] = (0.5 * (paf + qaf)) as f32;
        lpc[lpc2 - k] = (0.5 * (paf - qaf)) as f32;
    }
}

/// `ff_sine_window_init`.
pub fn sine_window_init(window: &mut [f32], n: usize) {
    for (i, w) in window[..n].iter_mut().enumerate() {
        *w = (((i as f64 + 0.5) * (core::f64::consts::PI / (2.0 * n as f64))) as f32).sin();
    }
}
