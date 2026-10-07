// Complex FFT of a power-of-two length.
//
// Ported from FFmpeg libavutil/tx.c and tx_template.c (commit 2da55bf),
// LGPL-2.1-or-later: the C codelets `av_tx_init(AV_TX_FLOAT_FFT)` runs
// without CPU-specific code (`-cpuflags 0`).

//! FFmpeg's C path for a float FFT of 2^k points, as QDMC and QDM2 use it:
//! the `fft` codelet gathers the input through the split-radix
//! permutation (`ff_tx_gen_ptwo_revtab`; for the inverse the permutation
//! also mirrors the input), then the in-place split-radix codelets
//! (`fft2_ns` ... `fft16_ns`, `ff_tx_fft_sr_combine`) run on it with the
//! `ff_tx_tab_N` cosine tables. FFmpeg's arm64 build lets clang fuse each
//! `a * b ± c * d` of `CMUL` into a fused multiply-add of the left
//! product; `mul_add` does the same here, so the output matches FFmpeg's
//! bit for bit.

use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Complex {
    pub re: f32,
    pub im: f32,
}

/// `ff_tx_tab_N` for N = 8 .. 1024 (index log2(N) - 3): cos(i * 2π / N)
/// for i < N / 4, then 0.
static TABLES: LazyLock<Vec<Vec<f32>>> = LazyLock::new(|| {
    (3..=10)
        .map(|k| {
            let len = 1usize << k;
            let freq = 2.0 * std::f64::consts::PI / len as f64;
            let mut tab: Vec<f32> = (0..len / 4).map(|i| (i as f64 * freq).cos() as f32).collect();
            tab.push(0.0);
            tab
        })
        .collect()
});

fn tab(len: usize) -> &'static [f32] {
    &TABLES[len.trailing_zeros() as usize - 3]
}

/// `split_radix_permutation`
fn split_radix_permutation(i: i32, len: i32, inv: bool) -> i32 {
    let len = len >> 1;
    if len <= 1 {
        return i & 1;
    }
    if i & len == 0 {
        return split_radix_permutation(i, len, inv) * 2;
    }
    let len = len >> 1;
    split_radix_permutation(i, len, inv) * 4 + 1 - 2 * ((i & len == 0) ^ inv) as i32
}

/// An FFT of a fixed power-of-two length (16 to 1024 points).
pub struct Fft {
    len: usize,
    /// The gather map of `ff_tx_gen_ptwo_revtab` (`FF_TX_MAP_GATHER`).
    map: Vec<usize>,
}

impl Fft {
    /// `av_tx_init(AV_TX_FLOAT_FFT, inv, len)`
    pub fn new(len: usize, inverse: bool) -> Self {
        assert!(len.is_power_of_two() && (16..=1024).contains(&len), "fft length {len}");
        let map = (0..len as i32)
            .map(|i| (-split_radix_permutation(i, len as i32, inverse) & (len as i32 - 1)) as usize)
            .collect();
        Self { len, map }
    }

    /// `ff_tx_fft`: `out = DFT(input)`, unnormalized; the inverse uses the
    /// positive exponent.
    pub fn run(&self, out: &mut [Complex], input: &[Complex]) {
        let out = &mut out[..self.len];
        for (o, &m) in out.iter_mut().zip(&self.map) {
            *o = input[m];
        }
        fft_ns(out);
    }
}

/// `BF(x, y, a, b)`: (a - b, a + b).
fn bf(a: f32, b: f32) -> (f32, f32) {
    (a - b, a + b)
}

/// `CMUL(dre, dim, are, aim, bre, bim)` as clang builds it on arm64.
fn cmul(are: f32, aim: f32, bre: f32, bim: f32) -> (f32, f32) {
    (are.mul_add(bre, -(aim * bim)), are.mul_add(bim, aim * bre))
}

/// `BUTTERFLIES(a0, a1, a2, a3)` with the temporaries t1, t2, t5, t6.
fn butterflies(z: &mut [Complex], [a0, a1, a2, a3]: [usize; 4], t1: f32, t2: f32, t5: f32, t6: f32) {
    let (r0, i0, r1, i1) = (z[a0].re, z[a0].im, z[a1].re, z[a1].im);
    let (t3, t5) = bf(t5, t1);
    (z[a2].re, z[a0].re) = bf(r0, t5);
    (z[a3].im, z[a1].im) = bf(i1, t3);
    let (t4, t6) = bf(t2, t6);
    (z[a3].re, z[a1].re) = bf(r1, t4);
    (z[a2].im, z[a0].im) = bf(i0, t6);
}

/// `TRANSFORM(a0, a1, a2, a3, wre, wim)`
fn transform(z: &mut [Complex], a: [usize; 4], wre: f32, wim: f32) {
    let (t1, t2) = cmul(z[a[2]].re, z[a[2]].im, wre, -wim);
    let (t5, t6) = cmul(z[a[3]].re, z[a[3]].im, wre, wim);
    butterflies(z, a, t1, t2, t5, t6);
}

/// `ff_tx_fft_sr_combine(z, cos, len)`
fn sr_combine(z: &mut [Complex], cos: &[f32], len: usize) {
    let (o1, o2, o3) = (2 * len, 4 * len, 6 * len);
    // `wim = cos + o1 - 7`, stepping back 8 as `cos` steps forward 8.
    let wim = |block: usize, k: usize| cos[o1 + k - 7 - 8 * block];
    for block in 0..len / 4 {
        let (z0, c0) = (8 * block, 8 * block);
        for (k, w) in [(0, 7), (2, 5), (4, 3), (6, 1), (1, 6), (3, 4), (5, 2), (7, 0)] {
            let i = z0 + k;
            transform(z, [i, i + o1, i + o2, i + o3], cos[c0 + k], wim(block, w));
        }
    }
}

/// `ff_tx_fft{N}_ns`, in place.
fn fft_ns(z: &mut [Complex]) {
    let n = z.len();
    match n {
        2 => fft2(z),
        4 => fft4(z),
        8 => fft8(z),
        16 => fft16(z),
        _ => {
            let (n2, n4) = (n / 2, n / 4);
            fft_ns(&mut z[..n2]);
            fft_ns(&mut z[n2..n2 + n4]);
            fft_ns(&mut z[n2 + n4..]);
            sr_combine(z, tab(n), n4 >> 1);
        }
    }
}

fn fft2(z: &mut [Complex]) {
    let (s0, s1) = (z[0], z[1]);
    let (tre, d0re) = bf(s0.re, s1.re);
    let (tim, d0im) = bf(s0.im, s1.im);
    z[0] = Complex { re: d0re, im: d0im };
    z[1] = Complex { re: tre, im: tim };
}

fn fft4(z: &mut [Complex]) {
    let s = [z[0], z[1], z[2], z[3]];
    let (t3, t1) = bf(s[0].re, s[1].re);
    let (t8, t6) = bf(s[3].re, s[2].re);
    (z[2].re, z[0].re) = bf(t1, t6);
    let (t4, t2) = bf(s[0].im, s[1].im);
    let (t7, t5) = bf(s[2].im, s[3].im);
    (z[3].im, z[1].im) = bf(t4, t8);
    (z[3].re, z[1].re) = bf(t3, t7);
    (z[2].im, z[0].im) = bf(t2, t5);
}

fn fft8(z: &mut [Complex]) {
    let s = [z[4], z[5], z[6], z[7]];
    let cos = tab(8)[1];
    fft4(&mut z[..4]);
    let (t1, d5re) = bf(s[0].re, -s[1].re);
    let (t2, d5im) = bf(s[0].im, -s[1].im);
    let (t5, d7re) = bf(s[2].re, -s[3].re);
    let (t6, d7im) = bf(s[2].im, -s[3].im);
    z[5] = Complex { re: d5re, im: d5im };
    z[7] = Complex { re: d7re, im: d7im };
    butterflies(z, [0, 2, 4, 6], t1, t2, t5, t6);
    transform(z, [1, 3, 5, 7], cos, cos);
}

fn fft16(z: &mut [Complex]) {
    let cos = tab(16);
    let (c1, c2, c3) = (cos[1], cos[2], cos[3]);
    fft8(&mut z[..8]);
    fft4(&mut z[8..12]);
    fft4(&mut z[12..16]);
    let (t1, t2, t5, t6) = (z[8].re, z[8].im, z[12].re, z[12].im);
    butterflies(z, [0, 4, 8, 12], t1, t2, t5, t6);
    transform(z, [2, 6, 10, 14], c2, c2);
    transform(z, [1, 5, 9, 13], c1, c3);
    transform(z, [3, 7, 11, 15], c3, c1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The definition, in f64.
    fn dft(input: &[Complex], inverse: bool) -> Vec<(f64, f64)> {
        let n = input.len();
        let sign = if inverse { 1.0 } else { -1.0 };
        (0..n)
            .map(|k| {
                input.iter().enumerate().fold((0.0, 0.0), |(re, im), (j, x)| {
                    let a = sign * 2.0 * std::f64::consts::PI * (j * k % n) as f64 / n as f64;
                    let (s, c) = a.sin_cos();
                    (re + f64::from(x.re) * c - f64::from(x.im) * s, im + f64::from(x.re) * s + f64::from(x.im) * c)
                })
            })
            .collect()
    }

    #[test]
    fn matches_the_definition() {
        for len in [16, 32, 64, 128, 256, 512, 1024] {
            let input: Vec<Complex> = (0..len)
                .map(|i| Complex { re: ((i * 7919) % 211) as f32 - 105.0, im: ((i * 104729) % 97) as f32 - 48.0 })
                .collect();
            for inverse in [false, true] {
                let mut out = vec![Complex::default(); len];
                Fft::new(len, inverse).run(&mut out, &input);
                let want = dft(&input, inverse);
                let worst = out
                    .iter()
                    .zip(&want)
                    .map(|(o, w)| (f64::from(o.re) - w.0).abs().max((f64::from(o.im) - w.1).abs()))
                    .fold(0.0, f64::max);
                assert!(worst < 0.05, "len {len} inverse {inverse}: worst error {worst}");
            }
        }
    }
}
