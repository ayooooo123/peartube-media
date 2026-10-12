// Ported from FFmpeg libavutil/tx_template.c, tx_tab.c and tx.c (commit
// 2da55bf), LGPL-2.1-or-later.
//
//! The AV TX float MDCT as FFmpeg's DCA decoders use it:
//! `av_tx_init(AV_TX_FLOAT_MDCT, inv=1, len, &scale, flags)`. The FFT
//! codelets (`fft2_ns`..`fft64_ns`, split-radix combine), the power-of-two
//! revtab (`split_radix_permutation`), the MDCT exponent table
//! (`ff_tx_mdct_gen_exp`) and the inverse MDCT (`ff_tx_mdct_inv`) are
//! translated operation-for-operation in f32 so the rounding matches the
//! C build (IEEE f32, no FMA contraction — the float CMUL/BF macros are
//! plain mul/add/sub).

/// Complex FFT butterfly helpers (`BF`): x = a - b, y = a + b.
#[inline(always)]
fn bf(x: &mut f32, y: &mut f32, a: f32, b: f32) {
    *x = a - b;
    *y = a + b;
}

/// `CMUL`: dre = are*bre - aim*bim; dim = are*bim + aim*bre.
/// The reference build contracts one product of each sum into an FMA
/// (clang -ffp-contract=on on aarch64); mul_add replicates that rounding.
#[inline(always)]
fn cmul(are: f32, aim: f32, bre: f32, bim: f32) -> (f32, f32) {
    (are.mul_add(bre, -(aim * bim)), are.mul_add(bim, aim * bre))
}

#[derive(Clone, Copy, Default)]
struct Cx {
    re: f32,
    im: f32,
}

/// `split_radix_permutation` (tx.c). Returns a signed value (the C's int;
/// it can reach -1, which the revtab folds modulo len).
fn split_radix_permutation(i: usize, len: usize, inv: bool) -> i64 {
    let len = len >> 1;
    if len <= 1 {
        return i64::from(i as u32 & 1);
    }
    if i & len == 0 {
        return split_radix_permutation(i, len, inv) * 2;
    }
    let len = len >> 1;
    split_radix_permutation(i, len, inv) * 4 + 1 - 2 * i64::from((i & len == 0) != inv)
}

/// `ff_tx_gen_ptwo_revtab` with `FF_TX_MAP_GATHER` (the direction the
/// in-place sub-transform of `ff_tx_mdct_inv` uses): map[i] = -perm & (len-1).
fn revtab_gather(len: usize, inv: bool) -> Vec<usize> {
    (0..len)
        .map(|i| {
            let perm = split_radix_permutation(i, len, inv);
            ((-(perm as i64)) as usize) & (len - 1)
        })
        .collect()
}

/// cos tables `ff_tx_tab_N`: N/4 entries cos(i*2π/N) plus one zero.
fn sr_tab(len: usize) -> Vec<f32> {
    let freq = 2.0 * std::f64::consts::PI / len as f64;
    let mut tab = Vec::with_capacity(len / 4 + 1);
    for i in 0..len / 4 {
        tab.push((i as f64 * freq).cos() as f32);
    }
    tab.push(0.0);
    tab
}

fn tabs() -> &'static [Vec<f32>] {
    use std::sync::LazyLock;
    static TABS: LazyLock<Vec<Vec<f32>>> = LazyLock::new(|| [8usize, 16, 32, 64].iter().map(|&n| sr_tab(n)).collect());
    &TABS
}

fn tab(n: usize) -> &'static [f32] {
    &tabs()[(n.trailing_zeros() as usize) - 3]
}

/// `TRANSFORM(a0, a1, a2, a3, wre, wim)`.
fn transform(z: &mut [Cx], a0: usize, a1: usize, a2: usize, a3: usize, wre: f32, wim: f32) {
    let (t1, t2) = cmul(z[a2].re, z[a2].im, wre, -wim);
    let (t5, t6) = cmul(z[a3].re, z[a3].im, wre, wim);
    // BUTTERFLIES with pre-set t5/t6 (C's macro reuses those locals).
    let r0 = z[a0].re;
    let i0 = z[a0].im;
    let r1 = z[a1].re;
    let i1 = z[a1].im;
    let mut t3 = 0.0;
    let mut t5v = t5;
    bf(&mut t3, &mut t5v, t5, t1);
    let mut nv = 0.0;
    bf(&mut z[a2].re, &mut nv, r0, t5v);
    z[a0].re = nv;
    let mut nv2 = 0.0;
    bf(&mut z[a3].im, &mut nv2, i1, t3);
    z[a1].im = nv2;
    let mut t4 = 0.0;
    let mut t6v = t6;
    bf(&mut t4, &mut t6v, t2, t6);
    let mut nv3 = 0.0;
    bf(&mut z[a3].re, &mut nv3, r1, t4);
    z[a1].re = nv3;
    let mut nv4 = 0.0;
    bf(&mut z[a2].im, &mut nv4, i0, t6v);
    z[a0].im = nv4;
}

/// `fft2_ns`.
fn fft2(src: &[Cx], dst: &mut [Cx]) {
    let mut tre = 0.0;
    let mut tim = 0.0;
    bf(&mut tre, &mut dst[0].re, src[0].re, src[1].re);
    bf(&mut tim, &mut dst[0].im, src[0].im, src[1].im);
    dst[1] = Cx { re: tre, im: tim };
}

/// `fft4_ns`.
fn fft4(src: &[Cx], dst: &mut [Cx]) {
    let mut t1 = 0.0;
    let mut t3 = 0.0;
    bf(&mut t3, &mut t1, src[0].re, src[1].re);
    let mut t8 = 0.0;
    let mut t6 = 0.0;
    bf(&mut t8, &mut t6, src[3].re, src[2].re);
    let mut nv = 0.0;
    bf(&mut dst[2].re, &mut nv, t1, t6);
    dst[0].re = nv;
    let mut t2 = 0.0;
    let mut t4 = 0.0;
    bf(&mut t4, &mut t2, src[0].im, src[1].im);
    let mut t7 = 0.0;
    let mut t5 = 0.0;
    bf(&mut t7, &mut t5, src[2].im, src[3].im);
    let mut nv1 = 0.0;
    bf(&mut dst[3].im, &mut nv1, t4, t8);
    dst[1].im = nv1;
    let mut nv2 = 0.0;
    bf(&mut dst[3].re, &mut nv2, t3, t7);
    dst[1].re = nv2;
    let mut nv3 = 0.0;
    bf(&mut dst[2].im, &mut nv3, t2, t5);
    dst[0].im = nv3;
}

/// `fft8_ns`.
fn fft8(src: &[Cx], dst: &mut [Cx]) {
    let cos = tab(8)[1];
    fft4(src, dst);

    let mut t1 = 0.0;
    let mut t2 = 0.0;
    bf(&mut t1, &mut dst[5].re, src[4].re, -src[5].re);
    bf(&mut t2, &mut dst[5].im, src[4].im, -src[5].im);
    let mut t5 = 0.0;
    let mut t6 = 0.0;
    bf(&mut t5, &mut dst[7].re, src[6].re, -src[7].re);
    bf(&mut t6, &mut dst[7].im, src[6].im, -src[7].im);

    // BUTTERFLIES(dst[0], dst[2], dst[4], dst[6])
    let r0 = dst[0].re;
    let i0 = dst[0].im;
    let r1 = dst[2].re;
    let i1 = dst[2].im;
    let mut t3 = 0.0;
    let mut t5v = t5;
    bf(&mut t3, &mut t5v, t5, t1);
    let mut nv = 0.0;
    bf(&mut dst[4].re, &mut nv, r0, t5v);
    dst[0].re = nv;
    let mut nv2 = 0.0;
    bf(&mut dst[6].im, &mut nv2, i1, t3);
    dst[2].im = nv2;
    let mut t4 = 0.0;
    let mut t6v = t6;
    bf(&mut t4, &mut t6v, t2, t6);
    let mut nv3 = 0.0;
    bf(&mut dst[6].re, &mut nv3, r1, t4);
    dst[2].re = nv3;
    let mut nv4 = 0.0;
    bf(&mut dst[4].im, &mut nv4, i0, t6v);
    dst[0].im = nv4;

    // TRANSFORM(dst[1], dst[3], dst[5], dst[7], cos, cos)
    transform(dst, 1, 3, 5, 7, cos, cos);
}

/// `fft16_ns`.
fn fft16(src: &[Cx], dst: &mut [Cx]) {
    let cos = tab(16);
    fft8(src, dst);
    fft4(&src[8..], &mut dst[8..]);
    fft4(&src[12..], &mut dst[12..]);

    let t1 = dst[8].re;
    let t2 = dst[8].im;
    let t5 = dst[12].re;
    let t6 = dst[12].im;
    // BUTTERFLIES(dst[0], dst[4], dst[8], dst[12])
    let r0 = dst[0].re;
    let i0 = dst[0].im;
    let r1 = dst[4].re;
    let i1 = dst[4].im;
    let mut t3 = 0.0;
    let mut t5v = t5;
    bf(&mut t3, &mut t5v, t5, t1);
    let mut nv = 0.0;
    bf(&mut dst[8].re, &mut nv, r0, t5v);
    dst[0].re = nv;
    let mut nv2 = 0.0;
    bf(&mut dst[12].im, &mut nv2, i1, t3);
    dst[4].im = nv2;
    let mut t4 = 0.0;
    let mut t6v = t6;
    bf(&mut t4, &mut t6v, t2, t6);
    let mut nv3 = 0.0;
    bf(&mut dst[12].re, &mut nv3, r1, t4);
    dst[4].re = nv3;
    let mut nv4 = 0.0;
    bf(&mut dst[8].im, &mut nv4, i0, t6v);
    dst[0].im = nv4;

    let c1 = cos[1];
    let c2 = cos[2];
    let c3 = cos[3];
    transform(dst, 2, 6, 10, 14, c2, c2);
    transform(dst, 1, 5, 9, 13, c1, c3);
    transform(dst, 3, 7, 11, 15, c3, c1);
}

/// `ff_tx_fft_sr_combine` for n = 32/64 (len = n4, operates on 8n entries).
fn sr_combine(z: &mut [Cx], cos: &[f32], len: usize) {
    let o1 = 2 * len;
    let o2 = 4 * len;
    let o3 = 6 * len;
    let mut zoff = 0usize;
    let mut coff = 0usize;
    // wim = cos + o1 - 7, decremented by 2*4 per step.
    let mut wimoff = (o1 as isize) - 7;
    while zoff < 2 * len {
        transform(z, zoff, zoff + o1, zoff + o2, zoff + o3, cos[coff], cos[(wimoff + 7) as usize]);
        transform(z, zoff + 2, zoff + o1 + 2, zoff + o2 + 2, zoff + o3 + 2, cos[coff + 2], cos[(wimoff + 5) as usize]);
        transform(z, zoff + 4, zoff + o1 + 4, zoff + o2 + 4, zoff + o3 + 4, cos[coff + 4], cos[(wimoff + 3) as usize]);
        transform(z, zoff + 6, zoff + o1 + 6, zoff + o2 + 6, zoff + o3 + 6, cos[coff + 6], cos[(wimoff + 1) as usize]);

        transform(z, zoff + 1, zoff + o1 + 1, zoff + o2 + 1, zoff + o3 + 1, cos[coff + 1], cos[(wimoff + 6) as usize]);
        transform(z, zoff + 3, zoff + o1 + 3, zoff + o2 + 3, zoff + o3 + 3, cos[coff + 3], cos[(wimoff + 4) as usize]);
        transform(z, zoff + 5, zoff + o1 + 5, zoff + o2 + 5, zoff + o3 + 5, cos[coff + 5], cos[(wimoff + 2) as usize]);
        transform(z, zoff + 7, zoff + o1 + 7, zoff + o2 + 7, zoff + o3 + 7, cos[coff + 7], cos[wimoff as usize]);

        zoff += 2 * 4;
        coff += 2 * 4;
        wimoff -= 2 * 4;
    }
}

/// `ff_tx_fft32_ns` (DECL_SR_CODELET(32,16,8)); uses its own tab_32.
fn fft32(src: &[Cx], dst: &mut [Cx]) {
    fft16(src, dst);
    fft8(&src[16..], &mut dst[16..]);
    fft8(&src[24..], &mut dst[24..]);
    sr_combine(dst, tab(32), 8 >> 1);
}

/// `ff_tx_fft64_ns` (DECL_SR_CODELET(64,32,16)); uses its own tab_64.
fn fft64(src: &[Cx], dst: &mut [Cx]) {
    fft32(src, dst);
    fft16(&src[32..], &mut dst[32..]);
    fft16(&src[48..], &mut dst[48..]);
    sr_combine(dst, tab(64), 16 >> 1);
}

/// The FFT codelet for a power-of-two length 2..64 (`_ns` variants, which
/// are what the MDCT's in-place sub-transform resolves to).
fn fft_dispatch(src: &[Cx], dst: &mut [Cx], len: usize) {
    match len {
        2 => fft2(src, dst),
        4 => fft4(src, dst),
        8 => fft8(src, dst),
        16 => fft16(src, dst),
        32 => fft32(src, dst),
        64 => fft64(src, dst),
        _ => unreachable!("av_tx port covers FFT lengths 2..=64 only (got {len})"),
    }
}

/// One configured inverse MDCT (`av_tx_init(AV_TX_FLOAT_MDCT, inv=1, len,
/// &scale, 0)`, the half-transform the synth filter and LBR call).
pub struct MdctInv {
    /// FFT length = mdct len / 2.
    fft_len: usize,
    /// `s->exp` (len/2 complex twiddles from `ff_tx_mdct_gen_exp`).
    exp: Vec<Cx>,
    /// The MDCT's sub_map (`s->map`, doubled by two at init for inv).
    map: Vec<usize>,
}

impl MdctInv {
    /// `ff_tx_mdct_init` + `ff_tx_mdct_gen_exp` for the inverse transform.
    /// `scale` is FFmpeg's `scale_d` (1.0 for the core, negative for LBR).
    pub fn new(len: usize, scale: f64) -> Self {
        let fft_len = len >> 1;
        // PRESHUFFLE sub-transform: the map comes from the FFT's revtab
        // (GATHER for the inverse sub-transform).
        let map: Vec<usize> = revtab_gather(fft_len, true);
        // gen_exp: len4 = len >> 1 here (the whole folding half); alloc is
        // 2*len4 with pre_tab, values written at [len4..2*len4), then
        // exp[i] = exp[len4 + pre_tab[i]] folds the first half. The raw
        // second half stays and the post-twiddle phase reads it (exp += len2).
        let len4 = len >> 1;
        let theta = if scale < 0.0 { len4 as f64 } else { 0.0 } + 1.0 / 8.0;
        let sc = scale.abs().sqrt();
        let mut exp = vec![Cx::default(); 2 * len4];
        for i in 0..len4 {
            let alpha = std::f64::consts::FRAC_PI_2 * (i as f64 + theta) / len4 as f64;
            exp[len4 + i] = Cx {
                re: (alpha.cos() * sc) as f32,
                im: (alpha.sin() * sc) as f32,
            };
        }
        for i in 0..len4 {
            exp[i] = exp[len4 + map[i]];
        }
        // Saves a multiply in a hot path: map[i] <<= 1.
        let map: Vec<usize> = map.iter().map(|&m| m << 1).collect();
        Self { fft_len, exp, map }
    }

    /// `ff_tx_mdct_inv`: `input` holds the len real values the caller
    /// passes (for the DCA synth: the 32/64 subband-domain values); the
    /// transform folds them into len/2 complex points, FFTs in place and
    /// writes len real outputs.
    pub fn run(&self, input: &[f32], output: &mut [f32]) {
        let len = self.fft_len * 2;
        let len2 = len >> 1;
        let len4 = len >> 2;
        let stride = 1usize;

        // Folding + pre-reindexing + CMUL3 with exp. in1 = src (forward),
        // in2 = src + (len2*2 - 1)*stride; k is the doubled sub_map value.
        let mut z = vec![Cx::default(); len2];
        let in2base = len2 * 2 - 1;
        for i in 0..len2 {
            let k = self.map[i];
            // tmp = { in2[-k*stride], in1[k*stride] }
            let tmp = Cx { re: input[in2base - k * stride], im: input[k * stride] };
            let e = self.exp[i];
            let (dre, dim) = cmul(tmp.re, tmp.im, e.re, e.im);
            z[i] = Cx { re: dre, im: dim };
        }

        // In-place FFT of len2 points.
        let scratch = z.clone();
        fft_dispatch(&scratch, &mut z, self.fft_len);

        // Post-twiddles (exp += len2: the raw unfolded second half).
        let exp = &self.exp[len2..];
        for i in 0..len4 {
            let i0 = len4 + i;
            let i1 = len4 - i - 1;
            let src1 = Cx { re: z[i1].im, im: z[i1].re };
            let src0 = Cx { re: z[i0].im, im: z[i0].re };
            let (dre, dim) = cmul(src1.re, src1.im, exp[i1].im, exp[i1].re);
            z[i1].re = dre;
            z[i0].im = dim;
            let (dre2, dim2) = cmul(src0.re, src0.im, exp[i0].im, exp[i0].re);
            z[i0].re = dre2;
            z[i1].im = dim2;
        }
        // The TX casts _dst to TXComplex* and writes len2 complex values —
        // interleaved re/im f32 pairs into the synth history window. The
        // synth filter's window sums then read both components as scalars.
        for (k, s) in z.iter().enumerate() {
            output[2 * k] = s.re;
            output[2 * k + 1] = s.im;
        }
    }
}

/// `ff_tx_mdct_inv_full`: the AV_TX_FULL_IMDCT wrapper LBR uses — a full
/// 2*len-point IMDCT produced from the len-point half transform.
pub struct MdctInvFull {
    half: MdctInv,
    len: usize,
}

impl MdctInvFull {
    pub fn new(len: usize, scale: f64) -> Self {
        Self { half: MdctInv::new(len, scale), len }
    }

    /// Full 2*len-point IMDCT: the half transform writes len floats at
    /// dst + len4 (len4 = len/2, all indices against the 2*len output),
    /// then the two mirror loops.
    pub fn run(&self, input: &[f32], output: &mut [f32]) {
        let len = self.len; // full output = 2 * len
        let len2 = len; // >> 1 of the full length
        let len4 = len >> 1;
        self.half.run(input, &mut output[len4..len4 + len]);

        for i in 0..len4 {
            output[i] = -output[len2 - i - 1];
            output[2 * len - i - 1] = output[len2 + i];
        }
    }
}
