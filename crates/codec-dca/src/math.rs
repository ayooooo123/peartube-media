// Ported from FFmpeg libavcodec/dcamath.h, libavcodec/dcaenc.h
// (quantize_value), libavutil/softfloat.h, libavcodec/dcaadpcm.h/c
// (ff_dcaadpcm_predict) and libavcodec/dca_core.c
// (ff_dca_core_dequantize) (commit 2da55bf). Licensed under LGPL-2.1-or-later.

//! DCA fixed-point arithmetic: the `norm__`/`mul__`/`clip23` helpers from
//! `dcamath.h`, the `softfloat` type FFmpeg's DCA encoder header defines
//! (used by the ADPCM path of the core decoder), and the ADPCM prediction
//! kernel `ff_dcaadpcm_predict`.

// ───────────────────────── dcamath.h ─────────────────────────

#[inline]
pub fn norm__(a: i64, bits: u32) -> i32 {
    if bits > 0 {
        ((a + (1i64 << (bits - 1))) >> bits) as i32
    } else {
        a as i32
    }
}

#[inline]
pub fn mul__(a: i32, b: i32, bits: u32) -> i32 {
    norm__(i64::from(a) * i64::from(b), bits)
}

#[inline]
pub fn norm13(a: i64) -> i32 {
    norm__(a, 13)
}
#[inline]
pub fn norm16(a: i64) -> i32 {
    norm__(a, 16)
}
#[inline]
pub fn norm20(a: i64) -> i32 {
    norm__(a, 20)
}
#[inline]
pub fn norm21(a: i64) -> i32 {
    norm__(a, 21)
}
#[inline]
pub fn norm23(a: i64) -> i32 {
    norm__(a, 23)
}

#[inline]
pub fn mul15(a: i32, b: i32) -> i32 {
    mul__(a, b, 15)
}
#[inline]
pub fn mul16(a: i32, b: i32) -> i32 {
    mul__(a, b, 16)
}
#[inline]
pub fn mul17(a: i32, b: i32) -> i32 {
    mul__(a, b, 17)
}
#[inline]
pub fn mul22(a: i32, b: i32) -> i32 {
    mul__(a, b, 22)
}
#[inline]
pub fn mul23(a: i32, b: i32) -> i32 {
    mul__(a, b, 23)
}
#[inline]
pub fn mul31(a: i32, b: i32) -> i32 {
    mul__(a, b, 31)
}
#[inline]
pub fn mul32(a: i32, b: i32) -> i32 {
    mul__(a, b, 32)
}

/// `av_clip_intp2(a, 23)`.
#[inline]
pub fn clip23(a: i32) -> i32 {
    const MIN: i32 = -(1 << 23);
    const MAX: i32 = (1 << 23) - 1;
    if a < MIN {
        MIN
    } else if a > MAX {
        MAX
    } else {
        a
    }
}

/// `av_clip_intp2(a, 22)`.
#[inline]
pub fn clip22(a: i32) -> i32 {
    const MIN: i32 = -(1 << 22);
    const MAX: i32 = (1 << 22) - 1;
    if a < MIN {
        MIN
    } else if a > MAX {
        MAX
    } else {
        a
    }
}

// ───────────────────────── softfloat (dcaenc.h) ─────────────────────────

/// FFmpeg's DCA encoder `softfloat`: value ≈ `m * 2^-e`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SoftFloat {
    pub m: i32,
    pub e: i32,
}

/// `quantize_value` from `dcaenc.h`.
#[inline]
pub fn quantize_value(value: i32, quant: SoftFloat) -> i32 {
    let offset = 1i32 << (quant.e - 1);
    let value = mul32(value, quant.m).wrapping_add(offset);
    value >> quant.e
}

// ───────────────────────── dca_core.c dequantize ─────────────────────────

/// `ff_dca_core_dequantize`: scale samples by `step_size * scale`,
/// limiting the scale factor resolution to 22 bits.
pub fn core_dequantize(output: &mut [i32], input: &[i32], step_size: i32, scale: i32, residual: bool) {
    // Account for quantizer step size
    let mut step_scale: i64 = i64::from(step_size) * i64::from(scale);
    let mut shift: u32 = 0;

    // Limit scale factor resolution to 22 bits.
    // av_log2(v) + 1 == 64 - leading_zeros(v) for v > 0.
    if step_scale > (1 << 23) {
        shift = 64 - (step_scale >> 23).leading_zeros();
        step_scale >>= shift;
    }

    // Scale the samples
    if residual {
        // The C clips the DELTA and adds it to the (unclipped, wrapping)
        // running sum: output[n] += clip23(norm__(...)). The sum itself is
        // never clipped — later stages (XLL residual reconstruction,
        // filter bank inputs) consume the full range.
        for (o, &i) in output.iter_mut().zip(input.iter()) {
            *o = o.wrapping_add(clip23(norm__(i as i64 * step_scale, 22 - shift)));
        }
    } else {
        for (o, &i) in output.iter_mut().zip(input.iter()) {
            *o = clip23(norm__(i as i64 * step_scale, 22 - shift));
        }
    }
}

// ───────────────────────── dcaadpcm ─────────────────────────

/// `ff_dcaadpcm_predict` (dcaadpcm.h). `input` must have at least
/// `DCA_ADPCM_COEFFS` (4) entries; the prediction looks back 4 samples.
#[inline]
pub fn dcaadpcm_predict(pred_vq_index: usize, input: &[i32]) -> i32 {
    let coeff = &crate::data::FF_DCA_ADPCM_VB[pred_vq_index];
    let mut pred: i64 = 0;
    for i in 0..crate::data::DCA_ADPCM_COEFFS {
        pred += i64::from(input[crate::data::DCA_ADPCM_COEFFS - 1 - i]) * i64::from(coeff[i]);
    }
    clip23(norm13(pred))
}
