// The inverse MDCT of FFmpeg's `av_tx` (`AV_TX_FLOAT_MDCT`, inverse;
// libavutil/tx_template.c, FFmpeg commit 2da55bf): `ff_tx_mdct_naive_inv`'s
// definition, computed through an n/2-point complex FFT in double
// precision, and `ff_tx_mdct_inv_full`'s symmetric extension.
// Copyright (c) the FFmpeg developers; LGPL-2.1-or-later (see LICENSE).

/// Largest transform: ATRAC3's 256 coefficients.
const MAX_N: usize = 256;

/// The inverse MDCT of `n` coefficients (a power of two, 16..=256) with
/// av_tx's `scale`.
pub(crate) struct Imdct {
    n: usize,
    scale: f64,
    pre: Vec<(f64, f64)>,
    post: Vec<(f64, f64)>,
    fft_twiddle: Vec<(f64, f64)>,
    bitrev: Vec<usize>,
}

impl Imdct {
    pub(crate) fn new(n: usize, scale: f64) -> Self {
        assert!(n.is_power_of_two() && (16..=MAX_N).contains(&n));
        let m = n / 2;
        let pi = std::f64::consts::PI;
        let rot = |a: f64| (a.cos(), a.sin());
        let pre = (0..m)
            .map(|j| rot(-pi * (j as f64 + 0.25) / n as f64))
            .collect();
        let post = (0..m).map(|k| rot(-pi * k as f64 / n as f64)).collect();
        let fft_twiddle = (0..m / 2)
            .map(|k| rot(-2.0 * pi * k as f64 / m as f64))
            .collect();
        let bits = m.trailing_zeros();
        let bitrev = (0..m)
            .map(|i| i.reverse_bits() >> (usize::BITS - bits))
            .collect();
        Self {
            n,
            scale,
            pre,
            post,
            fft_twiddle,
            bitrev,
        }
    }

    /// av_tx's inverse MDCT: `n` samples from `n` coefficients. That is
    /// the DCT-IV of the input in reverse order, times `scale`.
    pub(crate) fn half(&self, out: &mut [f32], input: &[f32]) {
        let n = self.n;
        let m = n / 2;
        let mut z = [(0f64, 0f64); MAX_N / 2];
        let z = &mut z[..m];
        for j in 0..m {
            let re = f64::from(input[2 * j]);
            let im = f64::from(input[n - 1 - 2 * j]);
            let (c, s) = self.pre[j];
            z[self.bitrev[j]] = (re * c - im * s, re * s + im * c);
        }
        let mut size = 2;
        while size <= m {
            let half = size / 2;
            let step = m / size;
            for start in (0..m).step_by(size) {
                for k in 0..half {
                    let (wr, wi) = self.fft_twiddle[k * step];
                    let (ar, ai) = z[start + k];
                    let (br, bi) = z[start + k + half];
                    let tr = br * wr - bi * wi;
                    let ti = br * wi + bi * wr;
                    z[start + k] = (ar + tr, ai + ti);
                    z[start + k + half] = (ar - tr, ai - ti);
                }
            }
            size *= 2;
        }
        for k in 0..m {
            let (zr, zi) = z[k];
            let (c, s) = self.post[k];
            let ur = zr * c - zi * s;
            let ui = zr * s + zi * c;
            out[n - 1 - 2 * k] = (ur * self.scale) as f32;
            out[2 * k] = (-ui * self.scale) as f32;
        }
    }

    /// `AV_TX_FULL_IMDCT`: `2n` samples, the half transform in the middle
    /// and its symmetric extension on both sides.
    pub(crate) fn full(&self, out: &mut [f32], input: &[f32]) {
        let n = self.n;
        self.half(&mut out[n / 2..n / 2 + n], input);
        for i in 0..n / 2 {
            out[i] = -out[n - 1 - i];
            out[2 * n - 1 - i] = out[n + i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ff_tx_mdct_naive_inv`: the definition the fast transform meets.
    fn naive(input: &[f32], scale: f64) -> Vec<f64> {
        let len2 = input.len();
        let len = len2 / 2;
        let phase = std::f64::consts::PI / (4.0 * len2 as f64);
        let mut out = vec![0f64; len2];
        for i in 0..len {
            let i_d = phase * (4 * len - 2 * i - 1) as f64;
            let i_u = phase * (3 * len2 + 2 * i + 1) as f64;
            let (mut d, mut u) = (0f64, 0f64);
            for (j, &x) in input.iter().enumerate() {
                let a = (2 * j + 1) as f64;
                d += (a * i_d).cos() * f64::from(x);
                u += (a * i_u).cos() * f64::from(x);
            }
            out[i] = d * scale;
            out[i + len] = -u * scale;
        }
        out
    }

    #[test]
    fn matches_av_tx_definition_for_every_size_and_scale() {
        let mut seed = 0x1234_5678u32;
        for (n, scale) in [
            (16, 32.0 / 32768.0),
            (32, -1.0 / 32768.0),
            (128, -1.0),
            (256, 1.0 / 32768.0),
        ] {
            let input: Vec<f32> = (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed as i32) as f32 / 65536.0
                })
                .collect();
            let reference = naive(&input, scale);
            let tx = Imdct::new(n, scale);
            let mut half = vec![0f32; n];
            tx.half(&mut half, &input);
            let peak = reference.iter().fold(0f64, |m, v| m.max(v.abs()));
            for (k, (&f, &r)) in half.iter().zip(&reference).enumerate() {
                assert!(
                    (f64::from(f) - r).abs() <= peak * 1e-6,
                    "n {n} sample {k}: {f} vs {r}"
                );
            }
            let mut full = vec![0f32; 2 * n];
            tx.full(&mut full, &input);
            assert_eq!(&full[n / 2..n / 2 + n], &half[..]);
            for i in 0..n / 2 {
                assert_eq!(full[i], -half[n / 2 - 1 - i]);
                assert_eq!(full[2 * n - 1 - i], half[n / 2 + i]);
            }
        }
    }
}
