// Ported from FFmpeg (commit 2da55bf): libavutil/tx.c (mulinv,
// ff_tx_gen_compound_mapping, split_radix_permutation,
// ff_tx_gen_ptwo_revtab), libavutil/tx_template.c (the split-radix tables
// and codelets, fft5/fft7/fft9, ff_tx_fft, ff_tx_fft_naive_small,
// ff_tx_fft_pfa, ff_tx_rdft_init, the rdft r2c/c2r and r2r/r2i mod2
// codelets, ff_tx_dctI, ff_tx_dstI) and libavutil/tx_priv.h (BF, CMUL,
// SMUL).
// GNU Lesser General Public License 2.1 or later.

//! The float C codelets of FFmpeg's `av_tx` that the WMA Voice postfilter
//! runs, in the transform trees FFmpeg builds for them without CPU
//! extensions (`ffmpeg -cpuflags 0 -v debug` prints them):
//!
//! - `AV_TX_FLOAT_RDFT` 128, forward and inverse: `rdft_r2c` / `rdft_c2r`
//!   over the out-of-place `fft` wrapper and the split-radix `fft64_ns`;
//! - `AV_TX_FLOAT_DCT_I` 64: `dctI` over `rdft_r2r_mod2` 126 over
//!   `fft_pfa` 63 (`fft7_ns`, then `fft9_ns`);
//! - `AV_TX_FLOAT_DST_I` 64: `dstI` over `rdft_r2i_mod2` 130 over
//!   `fft_pfa` 65 (`fft_naive_small` 13, then `fft5_ns`).
//!
//! The arithmetic follows the C statement by statement. FFmpeg's arm64
//! builds compile with clang's default `-ffp-contract=on`, which fuses
//! every `a*b ± c` inside one C expression into a single fused
//! multiply-add (the left product when both operands are products); the
//! `mul_add` calls below sit exactly where clang emits `llvm.fmuladd`, so
//! the results match FFmpeg's C path bit for bit.

use core::f64::consts::PI;

#[derive(Clone, Copy, Debug, Default)]
struct Cplx {
    re: f32,
    im: f32,
}

/// The split-radix cosine tables (`ff_tx_init_tab_<len>`):
/// `cos(2πi/len)` for `i < len/4`, then a zero.
fn sr_table<const N: usize>(len: usize) -> [f32; N] {
    let freq = 2.0 * PI / len as f64;
    let mut tab = [0.0; N];
    for (i, v) in tab.iter_mut().take(len / 4).enumerate() {
        *v = (i as f64 * freq).cos() as f32;
    }
    tab
}

/// `split_radix_permutation` (tx.c).
fn split_radix_permutation(i: i32, len: i32, inv: bool) -> i32 {
    let len = len >> 1;
    if len <= 1 {
        return i & 1;
    }
    if i & len == 0 {
        return split_radix_permutation(i, len, inv) * 2;
    }
    let len = len >> 1;
    split_radix_permutation(i, len, inv) * 4 + 1 - 2 * (((i & len) == 0) as i32 ^ inv as i32)
}

/// `BUTTERFLIES(a0, a1, a2, a3)` with `t1, t2, t5, t6` already set.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn butterflies(z: &mut [Cplx], a0: usize, a1: usize, a2: usize, a3: usize, t1: f32, t2: f32, t5: f32, t6: f32) {
    let r0 = z[a0].re;
    let i0 = z[a0].im;
    let r1 = z[a1].re;
    let i1 = z[a1].im;
    let t3 = t5 - t1;
    let t5 = t5 + t1;
    z[a2].re = r0 - t5;
    z[a0].re = r0 + t5;
    z[a3].im = i1 - t3;
    z[a1].im = i1 + t3;
    let t4 = t2 - t6;
    let t6 = t2 + t6;
    z[a3].re = r1 - t4;
    z[a1].re = r1 + t4;
    z[a2].im = i0 - t6;
    z[a0].im = i0 + t6;
}

/// `CMUL(dre, dim, are, aim, bre, bim)`: `(are*bre - aim*bim, are*bim +
/// aim*bre)`, contracted.
#[inline(always)]
fn cmul(are: f32, aim: f32, bre: f32, bim: f32) -> (f32, f32) {
    (are.mul_add(bre, -(aim * bim)), are.mul_add(bim, aim * bre))
}

/// `SMUL(dre, dim, are, aim, bre, bim)`: `(are*bre - aim*bim, are*bim -
/// aim*bre)`, contracted.
#[inline(always)]
fn smul(are: f32, aim: f32, bre: f32, bim: f32) -> (f32, f32) {
    (are.mul_add(bre, -(aim * bim)), are.mul_add(bim, -(aim * bre)))
}

/// `TRANSFORM(a0, a1, a2, a3, wre, wim)`.
#[inline(always)]
fn transform(z: &mut [Cplx], a0: usize, a1: usize, a2: usize, a3: usize, wre: f32, wim: f32) {
    let (t1, t2) = cmul(z[a2].re, z[a2].im, wre, -wim);
    let (t5, t6) = cmul(z[a3].re, z[a3].im, wre, wim);
    butterflies(z, a0, a1, a2, a3, t1, t2, t5, t6);
}

/// The split-radix FFT of 64 points with its tables, run in place on
/// preshuffled input (`ff_tx_fft64_ns` and the codelets it calls).
struct SplitRadix64 {
    tab8: [f32; 3],
    tab16: [f32; 5],
    tab32: [f32; 9],
    tab64: [f32; 17],
}

impl SplitRadix64 {
    fn new() -> Self {
        Self { tab8: sr_table(8), tab16: sr_table(16), tab32: sr_table(32), tab64: sr_table(64) }
    }

    /// `ff_tx_fft4_ns`.
    fn fft4(z: &mut [Cplx]) {
        let (s0, s1, s2, s3) = (z[0], z[1], z[2], z[3]);
        let t3 = s0.re - s1.re;
        let t1 = s0.re + s1.re;
        let t8 = s3.re - s2.re;
        let t6 = s3.re + s2.re;
        z[2].re = t1 - t6;
        z[0].re = t1 + t6;
        let t4 = s0.im - s1.im;
        let t2 = s0.im + s1.im;
        let t7 = s2.im - s3.im;
        let t5 = s2.im + s3.im;
        z[3].im = t4 - t8;
        z[1].im = t4 + t8;
        z[3].re = t3 - t7;
        z[1].re = t3 + t7;
        z[2].im = t2 - t5;
        z[0].im = t2 + t5;
    }

    /// `ff_tx_fft8_ns`.
    fn fft8(&self, z: &mut [Cplx]) {
        let cos = self.tab8[1];
        Self::fft4(z);
        let (s4, s5, s6, s7) = (z[4], z[5], z[6], z[7]);
        let t1 = s4.re - -s5.re;
        z[5].re = s4.re + -s5.re;
        let t2 = s4.im - -s5.im;
        z[5].im = s4.im + -s5.im;
        let t5 = s6.re - -s7.re;
        z[7].re = s6.re + -s7.re;
        let t6 = s6.im - -s7.im;
        z[7].im = s6.im + -s7.im;
        butterflies(z, 0, 2, 4, 6, t1, t2, t5, t6);
        transform(z, 1, 3, 5, 7, cos, cos);
    }

    /// `ff_tx_fft16_ns`.
    fn fft16(&self, z: &mut [Cplx]) {
        let cos = &self.tab16;
        self.fft8(&mut z[0..8]);
        Self::fft4(&mut z[8..12]);
        Self::fft4(&mut z[12..16]);
        let t1 = z[8].re;
        let t2 = z[8].im;
        let t5 = z[12].re;
        let t6 = z[12].im;
        butterflies(z, 0, 4, 8, 12, t1, t2, t5, t6);
        transform(z, 2, 6, 10, 14, cos[2], cos[2]);
        transform(z, 1, 5, 9, 13, cos[1], cos[3]);
        transform(z, 3, 7, 11, 15, cos[3], cos[1]);
    }

    /// `ff_tx_fft_sr_combine`: `z[0..8len]` from its four quarters.
    fn combine(z: &mut [Cplx], cos: &[f32], len: usize) {
        let (o1, o2, o3) = (2 * len, 4 * len, 6 * len);
        let mut i = 0;
        while i < len {
            let zb = 2 * i;
            let c = &cos[2 * i..];
            // `wim` starts at cos + o1 - 7 and moves back by 8 per round.
            let w = &cos[o1 - 7 - 2 * i..];
            transform(z, zb, zb + o1, zb + o2, zb + o3, c[0], w[7]);
            transform(z, zb + 2, zb + o1 + 2, zb + o2 + 2, zb + o3 + 2, c[2], w[5]);
            transform(z, zb + 4, zb + o1 + 4, zb + o2 + 4, zb + o3 + 4, c[4], w[3]);
            transform(z, zb + 6, zb + o1 + 6, zb + o2 + 6, zb + o3 + 6, c[6], w[1]);
            transform(z, zb + 1, zb + o1 + 1, zb + o2 + 1, zb + o3 + 1, c[1], w[6]);
            transform(z, zb + 3, zb + o1 + 3, zb + o2 + 3, zb + o3 + 3, c[3], w[4]);
            transform(z, zb + 5, zb + o1 + 5, zb + o2 + 5, zb + o3 + 5, c[5], w[2]);
            transform(z, zb + 7, zb + o1 + 7, zb + o2 + 7, zb + o3 + 7, c[7], w[0]);
            i += 4;
        }
    }

    /// `ff_tx_fft32_ns`.
    fn fft32(&self, z: &mut [Cplx]) {
        self.fft16(&mut z[0..16]);
        self.fft8(&mut z[16..24]);
        self.fft8(&mut z[24..32]);
        Self::combine(z, &self.tab32, 4);
    }

    /// `ff_tx_fft64_ns`.
    fn fft64(&self, z: &mut [Cplx]) {
        self.fft32(&mut z[0..32]);
        self.fft16(&mut z[32..48]);
        self.fft16(&mut z[48..64]);
        Self::combine(z, &self.tab64, 8);
    }
}

/// A forward or inverse 128-point RDFT (`AV_TX_FLOAT_RDFT`, scale 1).
pub struct Rdft128 {
    inv: bool,
    sr: SplitRadix64,
    /// `fft64_ns`'s gather map (`ff_tx_gen_ptwo_revtab`), which the `fft`
    /// wrapper applies.
    map: [u8; 64],
    fact: [f32; 8],
    tcos: [f32; 32],
    tsin: [f32; 32],
}

impl Rdft128 {
    /// `av_tx_init(AV_TX_FLOAT_RDFT, inv, 128, scale = 1.0)`.
    pub fn new(inv: bool) -> Self {
        let len = 128usize;
        let mut map = [0u8; 64];
        for (i, m) in map.iter_mut().enumerate() {
            *m = (-split_radix_permutation(i as i32, 64, inv) & 63) as u8;
        }
        let (fact, tcos, tsin) = rdft_tables::<32>(len, inv, 1.0, false);
        Self { inv, sr: SplitRadix64::new(), map, fact, tcos, tsin }
    }

    /// The pre/post-processing shared by `rdft_r2c` and `rdft_c2r`.
    fn post(&self, data: &mut [Cplx; 65]) {
        let (len2, len4) = (64, 32);
        let fact = &self.fact;
        let t0 = data[0].re;
        data[0].re = t0 + data[0].im;
        data[0].im = t0 - data[0].im;
        data[0].re = fact[0] * data[0].re;
        data[0].im = fact[1] * data[0].im;
        data[len4].re = fact[2] * data[len4].re;
        data[len4].im = fact[3] * data[len4].im;
        for i in 1..len4 {
            let (a, b) = (data[i], data[len2 - i]);
            let t0re = fact[4] * (a.re + b.re);
            let t0im = fact[5] * (a.im - b.im);
            let t1re = fact[6] * (a.im + b.im);
            let t1im = fact[7] * (a.re - b.re);
            let (t2re, t2im) = cmul(t1re, t1im, self.tcos[i], self.tsin[i]);
            data[i].re = t0re + t2re;
            data[i].im = t2im - t0im;
            data[len2 - i].re = t0re - t2re;
            data[len2 - i].im = t2im + t0im;
        }
    }

    /// Forward transform (`rdft_r2c`): the 128 reals `input[..128]` to 65
    /// complex values `out[2k] + i·out[2k + 1]` (`out[..130]`).
    pub fn forward(&self, out: &mut [f32], input: &[f32]) {
        let (out, input) = (&mut out[..130], &input[..128]);
        debug_assert!(!self.inv);
        let mut data = [Cplx::default(); 65];
        for (d, &m) in data.iter_mut().zip(self.map.iter()) {
            let k = 2 * m as usize;
            *d = Cplx { re: input[k], im: input[k + 1] };
        }
        self.sr.fft64(&mut data[..64]);
        self.post(&mut data);
        data[64].re = data[0].im;
        data[0].im = 0.0;
        data[64].im = 0.0;
        for (o, d) in out.chunks_exact_mut(2).zip(data.iter()) {
            o[0] = d.re;
            o[1] = d.im;
        }
    }

    /// Inverse transform (`rdft_c2r`): 65 complex values `input[..130]` to
    /// 128 reals `out[..128]`. FFmpeg overwrites its input; callers here
    /// never reread it.
    pub fn inverse(&self, out: &mut [f32], input: &[f32]) {
        let (out, input) = (&mut out[..128], &input[..130]);
        debug_assert!(self.inv);
        let mut data = [Cplx::default(); 65];
        for (d, i) in data.iter_mut().zip(input.chunks_exact(2)) {
            *d = Cplx { re: i[0], im: i[1] };
        }
        data[0].im = data[64].re;
        self.post(&mut data);
        let mut z = [Cplx::default(); 64];
        for (d, &m) in z.iter_mut().zip(self.map.iter()) {
            *d = data[m as usize];
        }
        self.sr.fft64(&mut z);
        for (o, d) in out.chunks_exact_mut(2).zip(z.iter()) {
            o[0] = d.re;
            o[1] = d.im;
        }
    }
}

/// `ff_tx_rdft_init`'s tables: the 8 factors, then `FFALIGN(len, 4)/4`
/// cosines and sines (`L`).
fn rdft_tables<const L: usize>(len: usize, inv: bool, scale: f32, r2r: bool) -> ([f32; 8], [f32; L], [f32; L]) {
    let scale_d = scale as f64;
    let scale_f = scale;
    let f = 2.0 * PI / len as f64;
    let m = if inv { 2.0 * scale_d } else { scale_d };
    let inv_d = inv as i32 as f64;
    let fact = [
        ((if inv { 0.5 } else { 1.0 }) * m) as f32,
        (if inv { 0.5 * m } else { 1.0 * m }) as f32,
        m as f32,
        (-m) as f32,
        ((0.5 - 0.0) * m) as f32,
        if r2r { 1.0 / scale_f } else { ((0.0 - 0.5) * m) as f32 },
        ((0.5 - inv_d) * m) as f32,
        (-(0.5 - inv_d) * m) as f32,
    ];
    let len4 = len.div_ceil(4);
    debug_assert_eq!(len4, L);
    let mut tcos = [0.0; L];
    let mut tsin = [0.0; L];
    let sign = if inv { 1.0 } else { -1.0 };
    for i in 0..L {
        tcos[i] = (i as f64 * f).cos() as f32;
        tsin[i] = ((len - i * 4) as f64 / 4.0 * f).cos() as f32 * sign;
    }
    (fact, tcos, tsin)
}

/// `DECL_RDFT_HALF(..., mod2 = 1)` on `out`, which holds the `len/2`-point
/// complex FFT of the input: `rdft_r2r_mod2` (`r2r`) or `rdft_r2i_mod2`.
fn rdft_half_mod2(out: &mut [f32], len: usize, r2r: bool, fact: &[f32; 8], tcos: &[f32], tsin: &[f32]) {
    let len2 = len >> 1;
    let len4 = len >> 2;
    let mut tmp_dc = out[0];
    out[0] = tmp_dc + out[1];
    tmp_dc -= out[1];
    out[0] *= fact[0];
    tmp_dc *= fact[1];
    out[2 * len4] *= fact[2];

    let half = |out: &[f32], i: usize, j: usize| -> (f32, f32, f32) {
        let (sf_re, sf_im) = (out[2 * i], out[2 * i + 1]);
        let (sl_re, sl_im) = (out[2 * j], out[2 * j + 1]);
        let t0 = if r2r { fact[4] * (sf_re + sl_re) } else { fact[5] * (sf_im - sl_im) };
        let t1 = fact[6] * (sf_im + sl_im);
        let t2 = fact[7] * (sf_re - sl_re);
        (t0, t1, t2)
    };

    let (t0, t1, t2) = half(out, len4, len4 + 1);
    let tmp_mid = if r2r {
        let t3 = t1.mul_add(tcos[len4], -(t2 * tsin[len4]));
        t0 - t3
    } else {
        let t3 = t1.mul_add(tsin[len4], t2 * tcos[len4]);
        t0 + t3
    };

    for i in 1..=len4 {
        let (t0, t1, t2) = half(out, i, len2 - i);
        if r2r {
            let t3 = t1.mul_add(tcos[i], -(t2 * tsin[i]));
            out[i] = t0 + t3;
            out[len - i] = t0 - t3;
        } else {
            let t3 = t1.mul_add(tsin[i], t2 * tcos[i]);
            out[i - 1] = t3 - t0;
            out[len - i - 1] = t0 + t3;
        }
    }

    for i in 1..(len4 + !r2r as usize) {
        out[len2 - i] = out[len - i];
    }

    if r2r {
        out[len2] = tmp_dc;
        out[len4 + 1] = tmp_mid * fact[5];
    } else {
        out[len4] = tmp_mid;
    }
}

/// The odd-length factor transforms a prime-factor FFT combines.
enum Factor {
    /// `fft5_ns` with `ff_tx_tab_53`.
    Fft5([f32; 12]),
    /// `fft7_ns` with `ff_tx_tab_7` (as complex pairs).
    Fft7([Cplx; 3]),
    /// `fft9_ns` with `ff_tx_tab_9` (as complex pairs).
    Fft9([Cplx; 4]),
    /// `fft_naive_small`: the length and its `exp[i*j]` table.
    Naive(usize, Vec<Cplx>),
}

impl Factor {
    fn fft5() -> Self {
        let c = |x: f64| x.cos() as f32;
        let s = |x: f64| x.sin() as f32;
        Factor::Fft5([
            c(2.0 * PI / 5.0),
            c(2.0 * PI / 5.0),
            c(2.0 * PI / 10.0),
            c(2.0 * PI / 10.0),
            s(2.0 * PI / 5.0),
            s(2.0 * PI / 5.0),
            s(2.0 * PI / 10.0),
            s(2.0 * PI / 10.0),
            c(2.0 * PI / 12.0),
            c(2.0 * PI / 12.0),
            c(2.0 * PI / 6.0),
            c(8.0 * PI / 6.0),
        ])
    }

    fn fft7() -> Self {
        let c = |x: f64| x.cos() as f32;
        let s = |x: f64| x.sin() as f32;
        Factor::Fft7([
            Cplx { re: c(2.0 * PI / 7.0), im: s(2.0 * PI / 7.0) },
            Cplx { re: s(2.0 * PI / 28.0), im: c(2.0 * PI / 28.0) },
            Cplx { re: c(2.0 * PI / 14.0), im: s(2.0 * PI / 14.0) },
        ])
    }

    fn fft9() -> Self {
        let c = |x: f64| x.cos() as f32;
        let s = |x: f64| x.sin() as f32;
        let t2 = c(2.0 * PI / 9.0);
        let t3 = s(2.0 * PI / 9.0);
        let t4 = c(2.0 * PI / 36.0);
        let t5 = s(2.0 * PI / 36.0);
        Factor::Fft9([
            Cplx { re: c(2.0 * PI / 3.0), im: s(2.0 * PI / 3.0) },
            Cplx { re: t2, im: t3 },
            Cplx { re: t4, im: t5 },
            Cplx { re: t2 + t5, im: t3 - t4 },
        ])
    }

    /// `ff_tx_fft_init_naive_small` (forward).
    fn naive(len: usize) -> Self {
        let phase = -2.0 * PI / len as f64;
        let mut exp = vec![Cplx::default(); len * len];
        for i in 0..len {
            for j in 0..len {
                let factor = phase * i as f64 * j as f64;
                exp[i * j] = Cplx { re: factor.cos() as f32, im: factor.sin() as f32 };
            }
        }
        Factor::Naive(len, exp)
    }

    /// The transform of `inp` written to `out[o + k*stride]`. Every codelet
    /// reads all of its input before writing, so in-place callers may pass
    /// a copy of the input.
    fn run(&self, inp: &[Cplx], out: &mut [Cplx], o: usize, stride: usize) {
        match self {
            Factor::Fft5(tab) => fft5(inp, out, o, stride, tab),
            Factor::Fft7(tab) => fft7(inp, out, o, stride, tab),
            Factor::Fft9(tab) => fft9(inp, out, o, stride, tab),
            Factor::Naive(len, exp) => {
                for i in 0..*len {
                    let (mut re, mut im) = (0f32, 0f32);
                    for (j, s) in inp[..*len].iter().enumerate() {
                        let m = exp[i * j];
                        let (rre, rim) = cmul(s.re, s.im, m.re, m.im);
                        re += rre;
                        im += rim;
                    }
                    out[o + i * stride] = Cplx { re, im };
                }
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Factor::Fft5(_) => 5,
            Factor::Fft7(_) => 7,
            Factor::Fft9(_) => 9,
            Factor::Naive(len, _) => *len,
        }
    }
}

/// `DECL_FFT5(fft5, 0, 1, 2, 3, 4)`.
fn fft5(inp: &[Cplx], out: &mut [Cplx], o: usize, stride: usize, tab: &[f32; 12]) {
    let dc = inp[0];
    let t1im = inp[1].re - inp[4].re;
    let t0re = inp[1].re + inp[4].re;
    let t1re = inp[1].im - inp[4].im;
    let t0im = inp[1].im + inp[4].im;
    let t3im = inp[2].re - inp[3].re;
    let t2re = inp[2].re + inp[3].re;
    let t3re = inp[2].im - inp[3].im;
    let t2im = inp[2].im + inp[3].im;

    out[o] = Cplx { re: dc.re + t0re + t2re, im: dc.im + t0im + t2im };

    let (t4re, t0re) = smul(tab[0], tab[2], t2re, t0re);
    let (t4im, t0im) = smul(tab[0], tab[2], t2im, t0im);
    let (t5re, t1re) = cmul(tab[4], tab[6], t3re, t1re);
    let (t5im, t1im) = cmul(tab[4], tab[6], t3im, t1im);

    let z0re = t0re - t1re;
    let z3re = t0re + t1re;
    let z0im = t0im - t1im;
    let z3im = t0im + t1im;
    let z2re = t4re - t5re;
    let z1re = t4re + t5re;
    let z2im = t4im - t5im;
    let z1im = t4im + t5im;

    out[o + stride] = Cplx { re: dc.re + z3re, im: dc.im + z0im };
    out[o + 2 * stride] = Cplx { re: dc.re + z2re, im: dc.im + z1im };
    out[o + 3 * stride] = Cplx { re: dc.re + z1re, im: dc.im + z2im };
    out[o + 4 * stride] = Cplx { re: dc.re + z0re, im: dc.im + z3im };
}

/// `fft7` (float).
fn fft7(inp: &[Cplx], out: &mut [Cplx], o: usize, stride: usize, tab: &[Cplx; 3]) {
    let dc = inp[0];
    let mut t = [Cplx::default(); 6];
    let mut z = [Cplx::default(); 3];
    t[1].re = inp[1].re - inp[6].re;
    t[0].re = inp[1].re + inp[6].re;
    t[1].im = inp[1].im - inp[6].im;
    t[0].im = inp[1].im + inp[6].im;
    t[3].re = inp[2].re - inp[5].re;
    t[2].re = inp[2].re + inp[5].re;
    t[3].im = inp[2].im - inp[5].im;
    t[2].im = inp[2].im + inp[5].im;
    t[5].re = inp[3].re - inp[4].re;
    t[4].re = inp[3].re + inp[4].re;
    t[5].im = inp[3].im - inp[4].im;
    t[4].im = inp[3].im + inp[4].im;

    out[o] = Cplx { re: dc.re + t[0].re + t[2].re + t[4].re, im: dc.im + t[0].im + t[2].im + t[4].im };

    // `a*b - c*d - e*f` contracts to fma(-e, f, fma(a, b, -(c*d))) and
    // `a*b + c*d ± e*f` to fma(±e, f, fma(a, b, c*d)).
    z[0].re = (-tab[1].re).mul_add(t[2].re, tab[0].re.mul_add(t[0].re, -(tab[2].re * t[4].re)));
    z[1].re = (-tab[2].re).mul_add(t[2].re, tab[0].re.mul_add(t[4].re, -(tab[1].re * t[0].re)));
    z[2].re = (-tab[1].re).mul_add(t[4].re, tab[0].re.mul_add(t[2].re, -(tab[2].re * t[0].re)));
    z[0].im = (-tab[2].re).mul_add(t[4].im, tab[0].re.mul_add(t[0].im, -(tab[1].re * t[2].im)));
    z[1].im = (-tab[2].re).mul_add(t[2].im, tab[0].re.mul_add(t[4].im, -(tab[1].re * t[0].im)));
    z[2].im = (-tab[1].re).mul_add(t[4].im, tab[0].re.mul_add(t[2].im, -(tab[2].re * t[0].im)));

    t[0].re = (-tab[0].im).mul_add(t[3].im, tab[2].im.mul_add(t[1].im, tab[1].im * t[5].im));
    t[2].re = (-tab[1].im).mul_add(t[1].im, tab[0].im.mul_add(t[5].im, tab[2].im * t[3].im));
    t[4].re = tab[0].im.mul_add(t[1].im, tab[2].im.mul_add(t[5].im, tab[1].im * t[3].im));
    t[0].im = tab[2].im.mul_add(t[5].re, tab[0].im.mul_add(t[1].re, tab[1].im * t[3].re));
    t[2].im = (-tab[1].im).mul_add(t[1].re, tab[2].im.mul_add(t[3].re, tab[0].im * t[5].re));
    t[4].im = (-tab[0].im).mul_add(t[3].re, tab[2].im.mul_add(t[1].re, tab[1].im * t[5].re));

    t[1].re = z[0].re - t[4].re;
    z[0].re += t[4].re;
    t[3].re = z[1].re - t[2].re;
    z[1].re += t[2].re;
    t[5].re = z[2].re - t[0].re;
    z[2].re += t[0].re;
    t[1].im = z[0].im - t[0].im;
    z[0].im += t[0].im;
    t[3].im = z[1].im - t[2].im;
    z[1].im += t[2].im;
    t[5].im = z[2].im - t[4].im;
    z[2].im += t[4].im;

    out[o + stride] = Cplx { re: dc.re + z[0].re, im: dc.im + t[1].im };
    out[o + 2 * stride] = Cplx { re: dc.re + t[3].re, im: dc.im + z[1].im };
    out[o + 3 * stride] = Cplx { re: dc.re + z[2].re, im: dc.im + t[5].im };
    out[o + 4 * stride] = Cplx { re: dc.re + t[5].re, im: dc.im + z[2].im };
    out[o + 5 * stride] = Cplx { re: dc.re + z[1].re, im: dc.im + t[3].im };
    out[o + 6 * stride] = Cplx { re: dc.re + t[1].re, im: dc.im + z[0].im };
}

/// `fft9` (float).
fn fft9(inp: &[Cplx], out: &mut [Cplx], o: usize, stride: usize, tab: &[Cplx; 4]) {
    let dc = inp[0];
    let mut t = [Cplx::default(); 8];
    let mut w = [Cplx::default(); 4];
    let mut x = [Cplx::default(); 5];
    let mut y = [Cplx::default(); 5];
    let mut z = [Cplx::default(); 2];
    t[1].re = inp[1].re - inp[8].re;
    t[0].re = inp[1].re + inp[8].re;
    t[1].im = inp[1].im - inp[8].im;
    t[0].im = inp[1].im + inp[8].im;
    t[3].re = inp[2].re - inp[7].re;
    t[2].re = inp[2].re + inp[7].re;
    t[3].im = inp[2].im - inp[7].im;
    t[2].im = inp[2].im + inp[7].im;
    t[5].re = inp[3].re - inp[6].re;
    t[4].re = inp[3].re + inp[6].re;
    t[5].im = inp[3].im - inp[6].im;
    t[4].im = inp[3].im + inp[6].im;
    t[7].re = inp[4].re - inp[5].re;
    t[6].re = inp[4].re + inp[5].re;
    t[7].im = inp[4].im - inp[5].im;
    t[6].im = inp[4].im + inp[5].im;

    w[0].re = t[0].re - t[6].re;
    w[0].im = t[0].im - t[6].im;
    w[1].re = t[2].re - t[6].re;
    w[1].im = t[2].im - t[6].im;
    w[2].re = t[1].re - t[7].re;
    w[2].im = t[1].im - t[7].im;
    w[3].re = t[3].re + t[7].re;
    w[3].im = t[3].im + t[7].im;

    z[0].re = dc.re + t[4].re;
    z[0].im = dc.im + t[4].im;

    z[1].re = t[0].re + t[2].re + t[6].re;
    z[1].im = t[0].im + t[2].im + t[6].im;

    out[o] = Cplx { re: z[0].re + z[1].re, im: z[0].im + z[1].im };

    y[3].re = tab[0].im * (t[1].re - t[3].re + t[7].re);
    y[3].im = tab[0].im * (t[1].im - t[3].im + t[7].im);

    x[3].re = tab[0].re.mul_add(z[1].re, z[0].re);
    x[3].im = tab[0].re.mul_add(z[1].im, z[0].im);
    z[0].re = tab[0].re.mul_add(t[4].re, dc.re);
    z[0].im = tab[0].re.mul_add(t[4].im, dc.im);

    x[1].re = tab[1].re.mul_add(w[0].re, tab[2].im * w[1].re);
    x[1].im = tab[1].re.mul_add(w[0].im, tab[2].im * w[1].im);
    x[2].re = tab[2].im.mul_add(w[0].re, -(tab[3].re * w[1].re));
    x[2].im = tab[2].im.mul_add(w[0].im, -(tab[3].re * w[1].im));
    y[1].re = tab[1].im.mul_add(w[2].re, tab[2].re * w[3].re);
    y[1].im = tab[1].im.mul_add(w[2].im, tab[2].re * w[3].im);
    y[2].re = tab[2].re.mul_add(w[2].re, -(tab[3].im * w[3].re));
    y[2].im = tab[2].re.mul_add(w[2].im, -(tab[3].im * w[3].im));

    y[0].re = tab[0].im * t[5].re;
    y[0].im = tab[0].im * t[5].im;

    x[4].re = x[1].re + x[2].re;
    x[4].im = x[1].im + x[2].im;

    y[4].re = y[1].re - y[2].re;
    y[4].im = y[1].im - y[2].im;
    x[1].re = z[0].re + x[1].re;
    x[1].im = z[0].im + x[1].im;
    y[1].re = y[0].re + y[1].re;
    y[1].im = y[0].im + y[1].im;
    x[2].re = z[0].re + x[2].re;
    x[2].im = z[0].im + x[2].im;
    y[2].re -= y[0].re;
    y[2].im -= y[0].im;
    x[4].re = z[0].re - x[4].re;
    x[4].im = z[0].im - x[4].im;
    y[4].re = y[0].re - y[4].re;
    y[4].im = y[0].im - y[4].im;

    out[o + stride] = Cplx { re: x[1].re + y[1].im, im: x[1].im - y[1].re };
    out[o + 2 * stride] = Cplx { re: x[2].re + y[2].im, im: x[2].im - y[2].re };
    out[o + 3 * stride] = Cplx { re: x[3].re + y[3].im, im: x[3].im - y[3].re };
    out[o + 4 * stride] = Cplx { re: x[4].re + y[4].im, im: x[4].im - y[4].re };
    out[o + 5 * stride] = Cplx { re: x[4].re - y[4].im, im: x[4].im + y[4].re };
    out[o + 6 * stride] = Cplx { re: x[3].re - y[3].im, im: x[3].im + y[3].re };
    out[o + 7 * stride] = Cplx { re: x[2].re - y[2].im, im: x[2].im + y[2].re };
    out[o + 8 * stride] = Cplx { re: x[1].re - y[1].im, im: x[1].im + y[1].re };
}

/// `mulinv` (tx.c): the inverse of `n` modulo `m`.
fn mulinv(n: usize, m: usize) -> usize {
    let n = n % m;
    (1..m).find(|x| (n * x) % m == 1).unwrap_or(0)
}

/// A forward prime-factor FFT (`ff_tx_fft_pfa`) of `first.len() *
/// second.len()` points; both sub-transforms have identity maps.
struct Pfa {
    first: Factor,
    second: Factor,
    in_map: Vec<usize>,
    out_map: Vec<usize>,
    tmp: Vec<Cplx>,
}

impl Pfa {
    fn new(first: Factor, second: Factor) -> Self {
        let (n, m) = (first.len(), second.len());
        let len = n * m;
        // ff_tx_gen_compound_mapping(s, NULL, 0, n, m): gather maps.
        let m_inv = mulinv(m, n);
        let n_inv = mulinv(n, m);
        let mut in_map = vec![0; len];
        let mut out_map = vec![0; len];
        for j in 0..m {
            for i in 0..n {
                in_map[j * n + i] = (i * m + j * n) % len;
                out_map[(i * m * m_inv + j * n * n_inv) % len] = i * m + j;
            }
        }
        Self { first, second, in_map, out_map, tmp: vec![Cplx::default(); len] }
    }

    fn run(&mut self, out: &mut [Cplx], inp: &[Cplx]) {
        let (n, m) = (self.first.len(), self.second.len());
        let mut exp = [Cplx::default(); 16];
        for i in 0..m {
            for (j, e) in exp[..n].iter_mut().enumerate() {
                *e = inp[self.in_map[i * n + j]];
            }
            self.first.run(&exp[..n], &mut self.tmp, i, m);
        }
        for i in 0..n {
            let mut col = [Cplx::default(); 16];
            col[..m].copy_from_slice(&self.tmp[m * i..m * i + m]);
            self.second.run(&col[..m], &mut self.tmp, m * i, 1);
        }
        for (o, &k) in out.iter_mut().zip(self.out_map.iter()) {
            *o = self.tmp[k];
        }
    }
}

/// A half-complex forward RDFT whose length is 2 mod 4, over a PFA FFT.
struct RdftHalfMod2 {
    len: usize,
    r2r: bool,
    pfa: Pfa,
    fact: [f32; 8],
    tcos: [f32; 33],
    tsin: [f32; 33],
}

impl RdftHalfMod2 {
    fn new(len: usize, r2r: bool, scale: f32, pfa: Pfa) -> Self {
        let mut tcos = [0.0; 33];
        let mut tsin = [0.0; 33];
        let fact = if len.div_ceil(4) == 33 {
            let (fact, c, s) = rdft_tables::<33>(len, false, scale, r2r);
            tcos = c;
            tsin = s;
            fact
        } else {
            let (fact, c, s) = rdft_tables::<32>(len, false, scale, r2r);
            tcos[..32].copy_from_slice(&c);
            tsin[..32].copy_from_slice(&s);
            fact
        };
        Self { len, r2r, pfa, fact, tcos, tsin }
    }

    /// `out[..len]` from the `len` reals `inp`.
    fn run(&mut self, out: &mut [f32], inp: &[f32]) {
        let n = self.len / 2;
        let mut src = [Cplx::default(); 65];
        for (s, c) in src[..n].iter_mut().zip(inp.chunks_exact(2)) {
            *s = Cplx { re: c[0], im: c[1] };
        }
        let mut dst = [Cplx::default(); 65];
        self.pfa.run(&mut dst[..n], &src[..n]);
        for (o, d) in out.chunks_exact_mut(2).zip(dst[..n].iter()) {
            o[0] = d.re;
            o[1] = d.im;
        }
        rdft_half_mod2(out, self.len, self.r2r, &self.fact, &self.tcos, &self.tsin);
    }
}

/// `AV_TX_FLOAT_DCT_I` of length 64 with scale 1/64.
pub struct DctI64 {
    rdft: RdftHalfMod2,
    tmp: [f32; 130],
}

impl DctI64 {
    pub fn new() -> Self {
        let pfa = Pfa::new(Factor::fft7(), Factor::fft9());
        Self { rdft: RdftHalfMod2::new(126, true, 1.0 / 64.0, pfa), tmp: [0.0; 130] }
    }

    /// `ff_tx_dctI`: reads `src[..64]`, writes `out[..126]`.
    pub fn run(&mut self, out: &mut [f32], src: &[f32]) {
        let src = &src[..64];
        let len = 63;
        for i in 0..len {
            self.tmp[2 * len - i] = src[i];
            self.tmp[i] = src[i];
        }
        self.tmp[len] = src[len];
        self.rdft.run(&mut out[..126], &self.tmp[..126]);
    }
}

impl Default for DctI64 {
    fn default() -> Self {
        Self::new()
    }
}

/// `AV_TX_FLOAT_DST_I` of length 64 with scale 1/64.
pub struct DstI64 {
    rdft: RdftHalfMod2,
    tmp: [f32; 130],
}

impl DstI64 {
    pub fn new() -> Self {
        let pfa = Pfa::new(Factor::naive(13), Factor::fft5());
        Self { rdft: RdftHalfMod2::new(130, false, 1.0 / 64.0, pfa), tmp: [0.0; 130] }
    }

    /// `ff_tx_dstI`: reads `src[..64]`, writes `out[..130]`.
    pub fn run(&mut self, out: &mut [f32], src: &[f32]) {
        let src = &src[..64];
        let len = 65;
        self.tmp[0] = 0.0;
        for i in 1..len {
            let a = src[i - 1];
            self.tmp[i] = -a;
            self.tmp[2 * len - i] = a;
        }
        self.tmp[len] = 0.0;
        self.rdft.run(&mut out[..130], &self.tmp);
    }
}

impl Default for DstI64 {
    fn default() -> Self {
        Self::new()
    }
}
