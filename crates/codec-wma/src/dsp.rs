// Ported from FFmpeg (commit 2da55bf): libavutil/tx.c, libavutil/float_dsp.c,
// libavcodec/sinewin.c, libavcodec/celp_filters.c, libavcodec/acelp_filters.c, libavcodec/lsp.c
// GNU Lesser General Public License 2.1 or later

use core::f64::consts::PI as PI_F64;

#[derive(Clone, Copy, Debug, Default)]
pub struct Complex64 {
    pub re: f64,
    pub im: f64,
}

/// In-place radix-2 decimation-in-time complex FFT.
pub fn fft_in_place(buf: &mut [Complex64]) {
    let n = buf.len();
    debug_assert!(n.is_power_of_two());

    // Bit-reversal permutation
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            buf.swap(i, j);
        }
    }

    // Butterfly stages
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let step = -PI_F64 / (half as f64);
        for k in 0..half {
            let (w_im, w_re) = (step * (k as f64)).sin_cos();
            let mut i = k;
            while i < n {
                let t = buf[i + half];
                let v_re = t.re * w_re - t.im * w_im;
                let v_im = t.re * w_im + t.im * w_re;
                let u = buf[i];
                buf[i] = Complex64 {
                    re: u.re + v_re,
                    im: u.im + v_im,
                };
                buf[i + half] = Complex64 {
                    re: u.re - v_re,
                    im: u.im - v_im,
                };
                i += len;
            }
        }
        len <<= 1;
    }
}

/// Generate sine half-window of size n:
/// w[i] = sin((i + 0.5) * pi / (2 * n)) for i in 0..n
pub fn sine_window(n: usize) -> Vec<f32> {
    let mut win = Vec::with_capacity(n);
    let scale = (PI_F64 / (2.0 * n as f64)) as f32;
    for i in 0..n {
        win.push(((i as f32 + 0.5) * scale).sin());
    }
    win
}

/// FFmpeg's vector_fmul_window:
/// dst[i]       =  src0[i] * win[len - 1 - i] - src1[len - 1 - i] * win[i]   (for i in -len..0, with len offset)
/// More simply, matching float_dsp.c:
/// For j = 0..len:
///   dst[j]       = src0[j] * win[len - 1 - j] - src1[len - 1 - j] * win[j]
///   dst[len + j] = src0[len - 1 - j] * win[len - 1 - j] + src1[j] * win[j]
pub fn vector_fmul_window(
    dst: &mut [f32],
    src0: &[f32],
    src1: &[f32],
    win: &[f32],
    len: usize,
) {
    for j in 0..len {
        let s0 = src0[j];
        let s1 = src1[len - 1 - j];
        let w_rev = win[len - 1 - j];
        let w_fwd = win[j];
        dst[j] = s0 * w_rev - s1 * w_fwd;
        dst[2 * len - 1 - j] = s0 * w_fwd + s1 * w_rev;
    }
}

/// Inverse MDCT computation matching FFmpeg's AV_TX_FLOAT_MDCT (inv = 1).
///
/// Full IMDCT of length 2N:
/// y[n] = scale * sum_{k=0}^{N-1} X[k] * cos((pi/N) * (n + 1/2 + N/2) * (k + 1/2))  for n in 0..2N
///
/// Half IMDCT outputs the middle N samples: y[N/2 .. 3N/2].
/// Full IMDCT outputs all 2N samples by unfolding around the anti-symmetric / symmetric edges.
pub struct Imdct {
    n: usize,
    twiddle_pre: Vec<Complex64>,
    twiddle_post: Vec<Complex64>,
    fft_buf: Vec<Complex64>,
}

impl Imdct {
    pub fn new(n: usize) -> Self {
        debug_assert!(n >= 4 && n.is_power_of_two());
        let m = n / 2;
        let mut twiddle_pre = Vec::with_capacity(m);
        let mut twiddle_post = Vec::with_capacity(m);

        for k in 0..m {
            // (2 * PI / (8 * N)) * (8 * k + 1) = (PI / (4 * N)) * (8 * k + 1)
            let alpha = (2.0 * PI_F64 * (8.0 * (k as f64) + 1.0)) / (8.0 * (n as f64));
            let (s, c) = alpha.sin_cos();
            twiddle_pre.push(Complex64 { re: c, im: -s });

            let beta = (2.0 * PI_F64 * (8.0 * (k as f64) + 1.0)) / (8.0 * (n as f64));
            let (s2, c2) = beta.sin_cos();
            twiddle_post.push(Complex64 { re: c2, im: -s2 });
        }

        Self {
            n,
            twiddle_pre,
            twiddle_post,
            fft_buf: vec![Complex64::default(); m],
        }
    }

    /// Fast half-length IMDCT (as in WMAPro): computes the N middle samples into `dst[0..N]`.
    pub fn imdct_half(&mut self, src: &[f32], dst: &mut [f32], scale: f32) {
        debug_assert_eq!(src.len(), self.n);
        debug_assert_eq!(dst.len(), self.n);

        let n = self.n;
        let m = n / 2;

        // Pre-twiddle
        for k in 0..m {
            let re = -src[2 * k + 1] as f64;
            let im = src[n - 1 - 2 * k] as f64;
            let tw = self.twiddle_pre[k];
            self.fft_buf[k] = Complex64 {
                re: re * tw.re - im * tw.im,
                im: re * tw.im + im * tw.re,
            };
        }

        fft_in_place(&mut self.fft_buf);

        // Post-twiddle and unpack into dst
        let scale_d = scale as f64;
        for k in 0..m {
            let z = self.fft_buf[k];
            let tw = self.twiddle_post[k];
            let z_re = (z.re * tw.re - z.im * tw.im) * scale_d;
            let z_im = (z.re * tw.im + z.im * tw.re) * scale_d;

            dst[2 * k] = z_re as f32;
            dst[n - 1 - 2 * k] = -z_im as f32;
        }

        // Apply FFmpeg half-IMDCT sign/order convention:
        // dst now contains the middle N samples
    }

    /// Full IMDCT (as in WMAv1/v2): computes 2N samples into `dst[0..2N]`.
    pub fn imdct_full(&mut self, src: &[f32], dst: &mut [f32], scale: f32) {
        debug_assert_eq!(src.len(), self.n);
        debug_assert_eq!(dst.len(), 2 * self.n);

        let n = self.n;
        let len2 = n;
        let len4 = n / 2;

        // Compute middle N samples into dst[len4 .. len4 + n]
        self.imdct_half(src, &mut dst[len4..len4 + n], scale);

        // Unfold using anti-symmetry on the left and symmetry on the right
        for i in 0..len4 {
            dst[i] = -dst[len2 - i - 1];
            dst[2 * n - i - 1] = dst[len2 + i];
        }
    }
}

/// Direct reference half-IMDCT for verifying transform correctness.
pub fn imdct_half_direct(src: &[f32], dst: &mut [f32], scale: f32) {
    let n = src.len();
    debug_assert_eq!(dst.len(), n);
    for i in 0..n {
        let mut sum = 0.0f64;
        let time_idx = i + n / 2;
        for k in 0..n {
            let angle = (PI_F64 / (n as f64)) * (time_idx as f64 + 0.5 + (n as f64) / 2.0) * (k as f64 + 0.5);
            sum += (src[k] as f64) * angle.cos();
        }
        dst[i] = (sum * scale as f64) as f32;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// WMA Voice specific filters and transforms
// ─────────────────────────────────────────────────────────────────────────────

/// 128-point real-to-complex RDFT for WMAVoice postfilter.
///
/// Input: 128 floats in `src`.
/// Output: 65 complex numbers packed into `dst` (130 floats: real, imag, real, imag...).
pub fn rdft_128(src: &[f32; 128], dst: &mut [f32; 130]) {
    for k in 0..=64 {
        let mut sum_re = 0.0f64;
        let mut sum_im = 0.0f64;
        for n in 0..128 {
            let angle = -2.0 * PI_F64 * (n as f64) * (k as f64) / 128.0;
            let (s, c) = angle.sin_cos();
            sum_re += (src[n] as f64) * c;
            sum_im += (src[n] as f64) * s;
        }
        dst[2 * k] = sum_re as f32;
        dst[2 * k + 1] = sum_im as f32;
    }
}

/// 128-point complex-to-real IRDFT for WMAVoice postfilter.
///
/// Input: 65 complex numbers packed into `src` (130 floats: real, imag...).
/// Output: 128 real floats into `dst`.
pub fn irdft_128(src: &[f32; 130], dst: &mut [f32; 128]) {
    for n in 0..128 {
        // DC component
        let mut sum = src[0] as f64;
        // Nyquist component
        let nyquist_sign = if n % 2 == 0 { 1.0 } else { -1.0 };
        sum += (src[128] as f64) * nyquist_sign;
        // Intermediate harmonics
        for k in 1..64 {
            let angle = 2.0 * PI_F64 * (n as f64) * (k as f64) / 128.0;
            let (s, c) = angle.sin_cos();
            let re = src[2 * k] as f64;
            let im = src[2 * k + 1] as f64;
            sum += 2.0 * (re * c - im * s);
        }
        dst[n] = (sum / 128.0) as f32; // normalized by len
    }
}

/// 64-point DCT-I for WMAVoice phase calculation.
///
/// Input: 65 floats (N+1 points with N=64).
/// Output: 65 floats.
pub fn dct_i_64(src: &[f32; 65], dst: &mut [f32; 65]) {
    const N: usize = 64;
    let scale = 1.0f64 / (N as f64);
    for k in 0..=N {
        let sign = if k % 2 == 0 { 1.0 } else { -1.0 };
        let mut sum = 0.5 * ((src[0] as f64) + sign * (src[N] as f64));
        for n in 1..N {
            let angle = PI_F64 * (n as f64) * (k as f64) / (N as f64);
            sum += (src[n] as f64) * angle.cos();
        }
        dst[k] = (sum * scale) as f32;
    }
}

/// 64-point DST-I for WMAVoice phase calculation.
///
/// Input: 64 floats (N points with N=64).
/// Output: 65 floats (padded with 0 at boundaries).
pub fn dst_i_64(src: &[f32; 64], dst: &mut [f32; 65]) {
    const N: usize = 64;
    let scale = 1.0f64 / (N as f64);
    dst[0] = 0.0;
    for k in 0..N {
        let mut sum = 0.0f64;
        for n in 1..=N {
            let angle = PI_F64 * (n as f64) * ((k + 1) as f64) / ((N + 1) as f64);
            sum += (src[n - 1] as f64) * angle.sin();
        }
        dst[k + 1] = (sum * scale) as f32;
    }
}

/// CELP LP synthesis filter:
/// out[n] = in[n] - sum_{i=1}^{filter_length} filter_coeffs[i-1] * out[n-i]
pub fn celp_lp_synthesis_filter(
    out: &mut [f32],
    filter_coeffs: &[f32],
    input: &[f32],
    filter_length: usize,
) {
    for n in 0..input.len() {
        let mut val = input[n] as f64;
        for i in 1..=filter_length {
            if n >= i {
                val -= (filter_coeffs[i - 1] as f64) * (out[n - i] as f64);
            }
        }
        out[n] = val as f32;
    }
}

/// CELP LP zero synthesis filter:
/// out[n] = in[n] + sum_{i=1}^{filter_length} filter_coeffs[i-1] * in[n-i]
pub fn celp_lp_zero_synthesis_filter(
    out: &mut [f32],
    filter_coeffs: &[f32],
    input: &[f32],
    in_offset: usize,
    buffer_length: usize,
    filter_length: usize,
) {
    for n in 0..buffer_length {
        let idx = in_offset + n;
        let mut val = input[idx] as f64;
        for i in 1..=filter_length {
            if idx >= i {
                val += (filter_coeffs[i - 1] as f64) * (input[idx - i] as f64);
            }
        }
        out[n] = val as f32;
    }
}

/// ACELP interpolation filter (ff_acelp_interpolatef).
pub fn acelp_interpolate(
    out: &mut [f32],
    input: &[f32],
    in_center: usize,
    filter_coeffs: &[f32],
    precision: usize,
    frac_pos: usize,
    filter_length: usize,
    length: usize,
) {
    for n in 0..length {
        let mut idx = 0;
        let mut v = 0.0f64;
        for i in 0..filter_length {
            let pos_fwd = in_center + n + i;
            let c_fwd = filter_coeffs.get(idx + frac_pos).copied().unwrap_or(0.0) as f64;
            v += (input.get(pos_fwd).copied().unwrap_or(0.0) as f64) * c_fwd;

            idx += precision;
            let i_next = i + 1;
            let pos_back = (in_center + n).wrapping_sub(i_next);
            let c_back = filter_coeffs
                .get(idx.saturating_sub(frac_pos))
                .copied()
                .unwrap_or(0.0) as f64;
            v += (input.get(pos_back).copied().unwrap_or(0.0) as f64) * c_back;
        }
        out[n] = v as f32;
    }
}

/// LSP to polynomial conversion (ff_acelp_lspd2lpc / lsp2polyf).
fn lsp2polyf(lsp: &[f64], f: &mut [f64], lp_half_order: usize) {
    f[0] = 1.0;
    f[1] = -2.0 * lsp[0];
    for i in 2..=lp_half_order {
        let val = -2.0 * lsp[2 * i - 2];
        f[i] = val * f[i - 1] + 2.0 * f[i - 2];
        for j in (2..i).rev() {
            f[j] += f[j - 1] * val + f[j - 2];
        }
        f[1] += val;
    }
}

/// Convert LSPs in cosine domain to LPC filter coefficients.
pub fn acelp_lspd2lpc(lsp: &[f64], lpc: &mut [f32], lp_half_order: usize) {
    let mut pa = vec![0.0f64; lp_half_order + 2];
    let mut qa = vec![0.0f64; lp_half_order + 2];

    lsp2polyf(&lsp[0..], &mut pa, lp_half_order);
    lsp2polyf(&lsp[1..], &mut qa, lp_half_order);

    let total_order = lp_half_order * 2;
    for k in 0..lp_half_order {
        let paf = pa[k + 1] + pa[k];
        let qaf = qa[k + 1] - qa[k];
        lpc[k] = (0.5 * (paf + qaf)) as f32;
        lpc[total_order - 1 - k] = (0.5 * (paf - qaf)) as f32;
    }
}
