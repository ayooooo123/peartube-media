// Ported from FFmpeg (commit 2da55bf): libavutil/tx_template.c
// (ff_tx_mdct_inv, ff_tx_mdct_inv_full, ff_tx_mdct_gen_exp), libavutil/
// float_dsp.c (vector_fmul_window) and libavcodec/sinewin_tablegen.h
// (ff_sine_window_init).
// GNU Lesser General Public License 2.1 or later.
//
//! Inverse MDCT (FFmpeg's `AV_TX_FLOAT_MDCT`, inverse direction) and the
//! windowing helpers the MDCT decoders share.
//!
//! The pre-rotation, the N/2-point complex inverse FFT and the post-rotation
//! follow `ff_tx_mdct_inv` in natural (unpermuted) order. The FFT is a plain
//! radix-2 decimation-in-time transform, so float rounding differs from
//! FFmpeg's split-radix by an ulp or so; the decoded audio stays well above
//! the 90 dB SNR the references demand.

use core::f64::consts::PI;

/// An inverse MDCT of `n` coefficients (FFmpeg's transform `len`), with
/// FFmpeg's `scale` folded into the pre/post rotations.
pub struct Imdct {
    n: usize,
    /// `ff_tx_mdct_gen_exp`: n/2 rotations `(cos α, sin α)·sqrt(|scale|)`.
    exp: Vec<[f32; 2]>,
    /// `e^{+2πi j/(n/2)}` for j < n/4: the inverse FFT twiddles.
    twiddle: Vec<[f32; 2]>,
    /// Bit-reversal permutation of the n/2-point FFT input.
    bitrev: Vec<u32>,
    z: Vec<[f32; 2]>,
}

impl Imdct {
    /// `n` is a power of two, at least 4.
    pub fn new(n: usize, scale: f64) -> Self {
        assert!(n >= 4 && n.is_power_of_two(), "IMDCT length {n}");
        let len2 = n >> 1;
        let theta = if scale < 0.0 { len2 as f64 } else { 0.0 } + 1.0 / 8.0;
        let s = scale.abs().sqrt();
        let exp = (0..len2)
            .map(|i| {
                let alpha = core::f64::consts::FRAC_PI_2 * (i as f64 + theta) / len2 as f64;
                [(alpha.cos() * s) as f32, (alpha.sin() * s) as f32]
            })
            .collect();
        let twiddle = (0..(len2 / 2).max(1))
            .map(|j| {
                let a = 2.0 * PI * j as f64 / len2 as f64;
                [a.cos() as f32, a.sin() as f32]
            })
            .collect();
        let bits = len2.trailing_zeros();
        let bitrev = (0..len2 as u32)
            .map(|i| if bits == 0 { 0 } else { i.reverse_bits() >> (32 - bits) })
            .collect();
        Self { n, exp, twiddle, bitrev, z: vec![[0.0; 2]; len2] }
    }

    /// Number of input coefficients.
    pub fn len(&self) -> usize {
        self.n
    }

    /// Half inverse MDCT (`ff_tx_mdct_inv`): `src[..n]` coefficients to the
    /// `n` middle samples `dst[..n]` of the 2n-sample window.
    pub fn imdct_half(&mut self, dst: &mut [f32], src: &[f32]) {
        let n = self.n;
        let len2 = n >> 1;
        let len4 = n >> 2;
        let src = &src[..n];
        let dst = &mut dst[..n];

        // pre-rotation, written in bit-reversed order for the FFT
        for m in 0..len2 {
            let (are, aim) = (src[n - 1 - 2 * m], src[2 * m]);
            let [bre, bim] = self.exp[m];
            self.z[self.bitrev[m] as usize] = [are * bre - aim * bim, are * bim + aim * bre];
        }

        // unnormalized inverse FFT
        let mut size = 2;
        while size <= len2 {
            let half = size >> 1;
            let step = len2 / size;
            for start in (0..len2).step_by(size) {
                for k in 0..half {
                    let [wr, wi] = self.twiddle[k * step];
                    let [ar, ai] = self.z[start + k];
                    let [br, bi] = self.z[start + k + half];
                    let (tr, ti) = (br * wr - bi * wi, br * wi + bi * wr);
                    self.z[start + k] = [ar + tr, ai + ti];
                    self.z[start + k + half] = [ar - tr, ai - ti];
                }
            }
            size <<= 1;
        }

        // post-rotation
        for i in 0..len4 {
            let i0 = len4 + i;
            let i1 = len4 - i - 1;
            let (s1re, s1im) = (self.z[i1][1], self.z[i1][0]);
            let (s0re, s0im) = (self.z[i0][1], self.z[i0][0]);
            let [e1re, e1im] = self.exp[i1];
            let [e0re, e0im] = self.exp[i0];
            dst[2 * i1] = s1re * e1im - s1im * e1re;
            dst[2 * i0 + 1] = s1re * e1re + s1im * e1im;
            dst[2 * i0] = s0re * e0im - s0im * e0re;
            dst[2 * i1 + 1] = s0re * e0re + s0im * e0im;
        }
    }

    /// Full inverse MDCT (`ff_tx_mdct_inv_full`, `AV_TX_FULL_IMDCT`):
    /// `src[..n]` coefficients to `dst[..2n]`.
    pub fn imdct_full(&mut self, dst: &mut [f32], src: &[f32]) {
        let n = self.n;
        let len2 = n;
        let len4 = n >> 1;
        self.imdct_half(&mut dst[len4..len4 + n], src);
        for i in 0..len4 {
            dst[i] = -dst[len2 - i - 1];
            dst[2 * n - i - 1] = dst[len2 + i];
        }
    }
}

/// `ff_sine_window_init`: `w[i] = sinf((i + 0.5) * (M_PI / (2.0 * n)))`.
pub fn sine_window(n: usize) -> Vec<f32> {
    (0..n).map(|i| (((i as f64 + 0.5) * (PI / (2.0 * n as f64))) as f32).sin()).collect()
}

/// `vector_fmul_window` (float_dsp.c) over one buffer: `buf[..2*len]` is
/// both `src0` (first half) and the destination, `src1` is `buf[len..]`,
/// exactly as FFmpeg's in-place call `vector_fmul_window(start, start,
/// start + len, win, len)`.
pub fn vector_fmul_window_inplace(buf: &mut [f32], win: &[f32], len: usize) {
    for k in 0..len {
        // FFmpeg: i = k - len, j = len - 1 - k, dst/src0/win offset by len.
        let j = len - 1 - k;
        let s0 = buf[k];
        let s1 = buf[len + j];
        let wi = win[k];
        let wj = win[len + j];
        buf[k] = s0 * wj - s1 * wi;
        buf[len + j] = s0 * wi + s1 * wj;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact-arithmetic model of `ff_tx_mdct_inv` (validated against
    /// libavutil's av_tx output): dst[i] = -IMDCT(src)[i + n/2] · scale,
    /// IMDCT(src)[t] = Σ_k src[k]·cos(π/n·(t + 1/2 + n/2)·(k + 1/2)).
    fn half_direct(src: &[f32], scale: f64) -> Vec<f64> {
        let n = src.len();
        (0..n)
            .map(|i| {
                let t = (i + n / 2) as f64;
                let sum: f64 = src
                    .iter()
                    .enumerate()
                    .map(|(k, &x)| x as f64 * (PI / n as f64 * (t + 0.5 + n as f64 / 2.0) * (k as f64 + 0.5)).cos())
                    .sum();
                -sum * scale
            })
            .collect()
    }

    #[test]
    fn imdct_half_matches_direct() {
        for log in 2..=11 {
            let n = 1usize << log;
            let mut seed = 12345u32;
            let src: Vec<f32> = (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    ((seed >> 8) as f32 / 16777216.0 - 0.5) * 2.0
                })
                .collect();
            for scale in [1.0, 1.0 / 32768.0] {
                let reference = half_direct(&src, scale);
                let mut imdct = Imdct::new(n, scale);
                let mut half = vec![0f32; n];
                imdct.imdct_half(&mut half, &src);
                let peak = reference.iter().fold(0f64, |m, v| m.max(v.abs()));
                for i in 0..n {
                    assert!((half[i] as f64 - reference[i]).abs() <= 1e-5 * peak, "n={n} i={i}");
                }
                let mut full = vec![0f32; 2 * n];
                imdct.imdct_full(&mut full, &src);
                assert_eq!(&full[n / 2..n / 2 + n], &half[..]);
                for i in 0..n / 2 {
                    assert_eq!(full[i], -full[n - i - 1]);
                    assert_eq!(full[2 * n - i - 1], full[n + i]);
                }
            }
        }
    }

    /// First outputs of libavutil's av_tx (n = 8, scale 1, inverse, half),
    /// for the same LCG input, printed with `%a`.
    #[test]
    fn imdct_half_matches_av_tx() {
        let mut seed = 12345u32;
        let src: Vec<f32> = (0..8)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                ((seed >> 8) as f32 / 16777216.0 - 0.5) * 2.0
            })
            .collect();
        let expect = [
            1.2695541381835938f32,
            0.7252858877182007,
            -0.8268116116523743,
            1.2198761701583862,
            0.19520221650600433,
            -1.957970142364502,
            -1.6236138343811035,
            -1.4331984519958496,
        ];
        let mut imdct = Imdct::new(8, 1.0);
        let mut out = [0f32; 8];
        imdct.imdct_half(&mut out, &src);
        for i in 0..8 {
            assert!((out[i] - expect[i]).abs() < 1e-6, "i={i} {} vs {}", out[i], expect[i]);
        }
    }
}
