// Ported from FFmpeg (commit 2da55bf): libavutil/float_scalarproduct.c
// (ff_scalarproduct_float_c).
// GNU Lesser General Public License 2.1 or later.

//! Sums of products as FFmpeg 2da55bf's arm64 build computes them.
//!
//! Clang (-O3, `-ffp-contract=on`) vectorizes a `sum += a[i] * b[i]` loop:
//! it multiplies four terms at a time and adds the products one by one in
//! C order, so each product is rounded before its addition. Only the terms
//! after the vectorized ones are fused multiply-adds. The Homebrew FFmpeg on
//! PATH does not vectorize these loops and fuses every term, so a port that
//! fuses every term matches that build, not the pinned one.

/// How many leading terms of a vectorized `sum += a[i] * b[i]` loop of
/// `len` terms have their products rounded: `len & !3` once `len >= 4`.
/// The rest are fused.
pub(crate) fn unfused_terms(len: usize) -> usize {
    if len >= 4 { len & !3 } else { 0 }
}

/// `ff_scalarproduct_float_c` over the first `len` terms.
pub(crate) fn scalarproduct_float(v1: &[f32], v2: &[f32], len: usize) -> f32 {
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
