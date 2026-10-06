// Ported from FFmpeg libavcodec/dcadsp.c, libavcodec/synth_filter.c,
// libavcodec/dcadct.c, libavutil/tx_template.c (naive inverse MDCT) and the
// libavutil float/fixed DSP kernels the DCA decoders call
// (vector_fmul_add/reverse/scalar, fmac_scalar, butterflies) (commit
// 2da55bf). Licensed under LGPL-2.1-or-later.

//! The DCA filter bank: float and fixed-point synthesis filters, LFE
//! interpolation, decimation, downmix kernels and the LBR hybrid bank.
//! Integer paths are bit-exact ports; the float paths implement the same
//! arithmetic as FFmpeg's C kernels in f32 (FFmpeg's float paths are
//! themselves only "reference exact" — the FATE lossy tests compare at
//! >= 90 dB SNR, which plain f32 arithmetic meets comfortably).

use crate::math::{clip23, mul15, mul16, mul17, mul22, mul23, norm16, norm23};

// ───────────────────────── dcadct.c ─────────────────────────

fn sum_a(input: &[i32], output: &mut [i32]) {
    for i in 0..output.len() {
        output[i] = input[2 * i] + input[2 * i + 1];
    }
}

fn sum_b(input: &[i32], output: &mut [i32]) {
    output[0] = input[0];
    for i in 1..output.len() {
        output[i] = input[2 * i] + input[2 * i - 1];
    }
}

fn sum_c(input: &[i32], output: &mut [i32]) {
    for i in 0..output.len() {
        output[i] = input[2 * i];
    }
}

fn sum_d(input: &[i32], output: &mut [i32]) {
    output[0] = input[1];
    for i in 1..output.len() {
        output[i] = input[2 * i - 1] + input[2 * i + 1];
    }
}

fn clp_v(input: &mut [i32]) {
    for v in input.iter_mut() {
        *v = clip23(*v);
    }
}

fn dct_a(input: &[i32], output: &mut [i32]) {
    const COS_MOD: [[i32; 8]; 8] = [
        [8348215, 8027397, 7398092, 6484482, 5321677, 3954362, 2435084, 822227],
        [8027397, 5321677, 822227, -3954362, -7398092, -8348215, -6484482, -2435084],
        [7398092, 822227, -6484482, -8027397, -2435084, 5321677, 8348215, 3954362],
        [6484482, -3954362, -8027397, 822227, 8348215, 2435084, -7398092, -5321677],
        [5321677, -7398092, -2435084, 8348215, -822227, -8027397, 3954362, 6484482],
        [3954362, -8348215, 5321677, 2435084, -8027397, 6484482, 822227, -7398092],
        [2435084, -6484482, 8348215, -7398092, 3954362, 822227, -5321677, 8027397],
        [822227, -2435084, 3954362, -5321677, 6484482, -7398092, 8027397, -8348215],
    ];
    for i in 0..8 {
        let mut res: i64 = 0;
        for j in 0..8 {
            res += i64::from(COS_MOD[i][j]) * i64::from(input[j]);
        }
        output[i] = norm23(res);
    }
}

fn dct_b(input: &[i32], output: &mut [i32]) {
    const COS_MOD: [[i32; 7]; 8] = [
        [8227423, 7750063, 6974873, 5931642, 4660461, 3210181, 1636536],
        [6974873, 3210181, -1636536, -5931642, -8227423, -7750063, -4660461],
        [4660461, -3210181, -8227423, -5931642, 1636536, 7750063, 6974873],
        [1636536, -7750063, -4660461, 5931642, 6974873, -3210181, -8227423],
        [-1636536, -7750063, 4660461, 5931642, -6974873, -3210181, 8227423],
        [-4660461, -3210181, 8227423, -5931642, -1636536, 7750063, -6974873],
        [-6974873, 3210181, 1636536, -5931642, 8227423, -7750063, 4660461],
        [-8227423, 7750063, -6974873, 5931642, -4660461, 3210181, -1636536],
    ];
    for i in 0..8 {
        let mut res: i64 = i64::from(input[0]) * (1i64 << 23);
        for j in 0..7 {
            res += i64::from(COS_MOD[i][j]) * i64::from(input[1 + j]);
        }
        output[i] = norm23(res);
    }
}

fn mod_a(input: &[i32], output: &mut [i32]) {
    const COS_MOD: [i32; 16] = [
        4199362, 4240198, 4323885, 4454708, 4639772, 4890013, 5221943, 5660703,
        -6245623, -7040975, -8158494, -9809974, -12450076, -17261920, -28585092, -85479984,
    ];
    for i in 0..8 {
        output[i] = mul23(COS_MOD[i], input[i] + input[8 + i]);
    }
    for (i, k) in (8..16).zip((0..8).rev()) {
        output[i] = mul23(COS_MOD[i], input[k] - input[8 + k]);
    }
}

fn mod_b(input: &mut [i32], output: &mut [i32]) {
    const COS_MOD: [i32; 8] = [4214598, 4383036, 4755871, 5425934, 6611520, 8897610, 14448934, 42791536];
    for i in 0..8 {
        input[8 + i] = mul23(COS_MOD[i], input[8 + i]);
    }
    for i in 0..8 {
        output[i] = input[i] + input[8 + i];
    }
    for (i, k) in (8..16).zip((0..8).rev()) {
        output[i] = input[k] - input[8 + k];
    }
}

fn mod_c(input: &[i32], output: &mut [i32]) {
    const COS_MOD: [i32; 32] = [
        1048892, 1051425, 1056522, 1064244, 1074689, 1087987, 1104313, 1123884,
        1146975, 1173922, 1205139, 1241133, 1282529, 1330095, 1384791, 1447815,
        -1520688, -1605358, -1704360, -1821051, -1959964, -2127368, -2332183, -2587535,
        -2913561, -3342802, -3931480, -4785806, -6133390, -8566050, -14253820, -42727120,
    ];
    for i in 0..16 {
        output[i] = mul23(COS_MOD[i], input[i] + input[16 + i]);
    }
    for (i, k) in (16..32).zip((0..16).rev()) {
        output[i] = mul23(COS_MOD[i], input[k] - input[16 + k]);
    }
}

fn mod64_a(input: &[i32], output: &mut [i32]) {
    const COS_MOD: [i32; 32] = [
        4195568, 4205700, 4226086, 4256977, 4298755, 4351949, 4417251, 4495537,
        4587901, 4695690, 4820557, 4964534, 5130115, 5320382, 5539164, 5791261,
        -6082752, -6421430, -6817439, -7284203, -7839855, -8509474, -9328732, -10350140,
        -11654242, -13371208, -15725922, -19143224, -24533560, -34264200, -57015280, -170908480,
    ];
    for i in 0..16 {
        output[i] = mul23(COS_MOD[i], input[i] + input[16 + i]);
    }
    for (i, k) in (16..32).zip((0..16).rev()) {
        output[i] = mul23(COS_MOD[i], input[k] - input[16 + k]);
    }
}

fn mod64_b(input: &mut [i32], output: &mut [i32]) {
    const COS_MOD: [i32; 16] = [
        4199362, 4240198, 4323885, 4454708, 4639772, 4890013, 5221943, 5660703,
        6245623, 7040975, 8158494, 9809974, 12450076, 17261920, 28585092, 85479984,
    ];
    for i in 0..16 {
        input[16 + i] = mul23(COS_MOD[i], input[16 + i]);
    }
    for i in 0..16 {
        output[i] = input[i] + input[16 + i];
    }
    for (i, k) in (16..32).zip((0..16).rev()) {
        output[i] = input[k] - input[16 + k];
    }
}

fn mod64_c(input: &[i32], output: &mut [i32]) {
    const COS_MOD: [i32; 64] = [
        741511, 741958, 742853, 744199, 746001, 748262, 750992, 754197,
        757888, 762077, 766777, 772003, 777772, 784105, 791021, 798546,
        806707, 815532, 825054, 835311, 846342, 858193, 870912, 884554,
        899181, 914860, 931667, 949686, 969011, 989747, 1012012, 1035941,
        -1061684, -1089412, -1119320, -1151629, -1186595, -1224511, -1265719, -1310613,
        -1359657, -1413400, -1472490, -1537703, -1609974, -1690442, -1780506, -1881904,
        -1996824, -2128058, -2279225, -2455101, -2662128, -2909200, -3208956, -3579983,
        -4050785, -4667404, -5509372, -6726913, -8641940, -12091426, -20144284, -60420720,
    ];
    for i in 0..32 {
        output[i] = mul23(COS_MOD[i], input[i] + input[32 + i]);
    }
    for (i, k) in (32..64).zip((0..32).rev()) {
        output[i] = mul23(COS_MOD[i], input[k] - input[32 + k]);
    }
}

/// `imdct_half_32` — the fixed-point 32-point inverse DCT-IV half used by
/// `synth_filter_fixed`.
pub fn imdct_half_32(output: &mut [i32], input: &[i32]) {
    let mut buf_a = [0i32; 32];
    let mut buf_b = [0i32; 32];

    let mut mag: i64 = 0;
    for &v in input.iter() {
        mag += v.unsigned_abs() as i64;
    }

    let shift: u32 = if mag > 0x400000 { 2 } else { 0 };
    let round: i32 = if shift > 0 { 1 << (shift - 1) } else { 0 };

    for i in 0..32 {
        buf_a[i] = (input[i] + round) >> shift;
    }

    sum_a(&buf_a, &mut buf_b[0..16]);
    sum_b(&buf_a, &mut buf_b[16..32]);
    clp_v(&mut buf_b);

    sum_a(&buf_b[0..16], &mut buf_a[0..8]);
    sum_b(&buf_b[0..16], &mut buf_a[8..16]);
    sum_c(&buf_b[16..32], &mut buf_a[16..24]);
    sum_d(&buf_b[16..32], &mut buf_a[24..32]);
    clp_v(&mut buf_a);

    dct_a(&buf_a[0..8], &mut buf_b[0..8]);
    dct_b(&buf_a[8..16], &mut buf_b[8..16]);
    dct_b(&buf_a[16..24], &mut buf_b[16..24]);
    dct_b(&buf_a[24..32], &mut buf_b[24..32]);
    clp_v(&mut buf_b);

    mod_a(&buf_b[0..16], &mut buf_a[0..16]);
    mod_b(&mut buf_b[16..32], &mut buf_a[16..32]);
    clp_v(&mut buf_a);

    mod_c(&buf_a, &mut buf_b);

    for v in buf_b.iter_mut() {
        *v = clip23(*v * (1 << shift));
    }

    for i in 0..16 {
        let k = 31 - i;
        output[i] = clip23(buf_b[i] - buf_b[k]);
        output[16 + i] = clip23(buf_b[i] + buf_b[k]);
    }
}

/// `imdct_half_64` — the fixed-point 64-point variant.
pub fn imdct_half_64(output: &mut [i32], input: &[i32]) {
    let mut buf_a = [0i32; 64];
    let mut buf_b = [0i32; 64];

    let mut mag: i64 = 0;
    for &v in input.iter() {
        mag += v.unsigned_abs() as i64;
    }

    let shift: u32 = if mag > 0x400000 { 2 } else { 0 };
    let round: i32 = if shift > 0 { 1 << (shift - 1) } else { 0 };

    for i in 0..64 {
        buf_a[i] = (input[i] + round) >> shift;
    }

    sum_a(&buf_a[0..64], &mut buf_b[0..32]);
    sum_b(&buf_a[0..64], &mut buf_b[32..64]);
    clp_v(&mut buf_b);

    sum_a(&buf_b[0..32], &mut buf_a[0..16]);
    sum_b(&buf_b[0..32], &mut buf_a[16..32]);
    sum_c(&buf_b[32..64], &mut buf_a[32..48]);
    sum_d(&buf_b[32..64], &mut buf_a[48..64]);
    clp_v(&mut buf_a);

    sum_a(&buf_a[0..16], &mut buf_b[0..8]);
    sum_b(&buf_a[0..16], &mut buf_b[8..16]);
    sum_c(&buf_a[16..32], &mut buf_b[16..24]);
    sum_d(&buf_a[16..32], &mut buf_b[24..32]);
    sum_c(&buf_a[32..48], &mut buf_b[32..40]);
    sum_d(&buf_a[32..48], &mut buf_b[40..48]);
    sum_c(&buf_a[48..64], &mut buf_b[48..56]);
    sum_d(&buf_a[48..64], &mut buf_b[56..64]);
    clp_v(&mut buf_b);

    dct_a(&buf_b[0..8], &mut buf_a[0..8]);
    dct_b(&buf_b[8..16], &mut buf_a[8..16]);
    dct_b(&buf_b[16..24], &mut buf_a[16..24]);
    dct_b(&buf_b[24..32], &mut buf_a[24..32]);
    dct_b(&buf_b[32..40], &mut buf_a[32..40]);
    dct_b(&buf_b[40..48], &mut buf_a[40..48]);
    dct_b(&buf_b[48..56], &mut buf_a[48..56]);
    dct_b(&buf_b[56..64], &mut buf_a[56..64]);
    clp_v(&mut buf_a);

    mod_a(&buf_a[0..16], &mut buf_b[0..16]);
    mod_b(&mut buf_a[16..32], &mut buf_b[16..32]);
    mod_b(&mut buf_a[32..48], &mut buf_b[32..48]);
    mod_b(&mut buf_a[48..64], &mut buf_b[48..64]);
    clp_v(&mut buf_b);

    mod64_a(&buf_b[0..32], &mut buf_a[0..32]);
    mod64_b(&mut buf_b[32..64], &mut buf_a[32..64]);
    clp_v(&mut buf_a);

    mod64_c(&buf_a, &mut buf_b);

    for v in buf_b.iter_mut() {
        *v = clip23(*v * (1 << shift));
    }

    for i in 0..32 {
        let k = 63 - i;
        output[i] = clip23(buf_b[i] - buf_b[k]);
        output[32 + i] = clip23(buf_b[i] + buf_b[k]);
    }
}

// ───────────────────────── naive inverse MDCT (tx_template.c) ─────────────────────────

/// `ff_tx_mdct_naive_inv` with `len` = 2 * frame (the full window size the
/// synth filter passes). `input` holds `len/2` frequency-domain values,
/// `output` receives `len` time samples: `dst[i] = sum_d * scale`,
/// `dst[i + len/2] = -sum_u * scale`, with the exact cos factors
/// FFmpeg's naive MDCT uses. Only the lengths the DCA decoders need
/// (32/64 for the core synth filters, any power of two <= 4096 for LBR)
/// are supported; the input must have `len/2` entries.
pub fn imdct_naive_inv(input: &[f32], output: &mut [f32], scale: f32) {
    let len = output.len() >> 1; // s->len >> 1 where s->len == output.len()
    let len2 = len * 2;
    let phase = std::f64::consts::PI / (4.0 * len as f64);

    for i in 0..len {
        let mut sum_d = 0.0f64;
        let mut sum_u = 0.0f64;
        let i_d = phase * ((4 * len - 2 * i - 1) as f64);
        let i_u = phase * ((3 * len2 + 2 * i + 1) as f64);
        for &val in input.iter().take(len2) {
            sum_d += i_d.cos() * val as f64;
            sum_u += i_u.cos() * val as f64;
        }
        output[i] = (sum_d * scale as f64) as f32;
        output[i + len] = (-sum_u * scale as f64) as f32;
    }
}

// ───────────────────────── synth_filter.c ─────────────────────────

/// `synth_filter_float`: 32-subband synthesis. `synth_buf` is the 512-entry
/// history window (`hist1` in FFmpeg), `synth_buf_offset` its rotating
/// offset, `synth_buf2` the 32-entry overlap carrier (`hist2`), `window`
/// the 512-entry synthesis window, `out` receives 32 samples from the
/// 32-subband input `in`.
pub fn synth_filter_float(
    imdct: &dyn Fn(&[f32], &mut [f32]),
    synth_buf: &mut [f32; 512],
    synth_buf_offset: &mut i32,
    synth_buf2: &mut [f32; 32],
    window: &[f32],
    out: &mut [f32],
    input: &[f32],
    scale: f32,
) {
    let offset = *synth_buf_offset as usize;
    // imdct writes the 32 outputs at synth_buf[offset..offset+32] (the C
    // base+offset), wrapping circularly.
    let mut tmp = [0f32; 32];
    imdct(input, &mut tmp);
    for (k, &v) in tmp.iter().enumerate() {
        synth_buf[(offset + k) % 512] = v;
    }

    for i in 0..16 {
        let mut a = synth_buf2[i];
        let mut b = synth_buf2[i + 16];
        let mut c = 0.0f32;
        let mut d = 0.0f32;
        // synth_buf here is the C `synth_buf` pointer (base + offset);
        // indices are relative to it, like the C code.
        // synth_buf is a 512-entry circular window whose origin sits at
        // `offset`; C's relative indices map to absolute cells mod 512.
        let sb = |k: isize| -> f32 { synth_buf[(((k + offset as isize) % 512 + 512) % 512) as usize] };
        let mut j = 0usize;
        while j < 512 - offset {
            a += window[i + j] * (-sb((15 - i + j) as isize));
            b += window[i + j + 16] * sb((i + j) as isize);
            c += window[i + j + 32] * sb((16 + i + j) as isize);
            d += window[i + j + 48] * sb((31 - i + j) as isize);
            j += 64;
        }
        while j < 512 {
            let jj = j as isize;
            let ii = i as isize;
            a += window[i + j] * (-sb((15 - ii + jj - 512)));
            b += window[i + j + 16] * sb((ii + jj - 512));
            c += window[i + j + 32] * sb((16 + ii + jj - 512));
            d += window[i + j + 48] * sb((31 - ii + jj - 512));
            j += 64;
        }
        out[i] = a * scale;
        out[i + 16] = b * scale;
        synth_buf2[i] = c;
        synth_buf2[i + 16] = d;
    }

    *synth_buf_offset = (*synth_buf_offset - 32) & 511;
}

/// `synth_filter_float_64`: 64-subband variant with a 1024-entry history.
#[allow(clippy::too_many_arguments)]
pub fn synth_filter_float_64(
    imdct: &dyn Fn(&[f32], &mut [f32]),
    synth_buf: &mut [f32; 1024],
    synth_buf_offset: &mut i32,
    synth_buf2: &mut [f32; 64],
    window: &[f32],
    out: &mut [f32],
    input: &[f32],
    scale: f32,
) {
    let offset = *synth_buf_offset as usize;
    imdct(input, &mut synth_buf[offset..offset + 64]);

    for i in 0..32 {
        let mut a = synth_buf2[i];
        let mut b = synth_buf2[i + 32];
        let mut c = 0.0f32;
        let mut d = 0.0f32;
        let mut j = 0usize;
        while j < 1024 - offset {
            a += window[i + j] * (-synth_buf[31 - i + j + offset]);
            b += window[i + j + 32] * (synth_buf[i + j + offset]);
            c += window[i + j + 64] * (synth_buf[32 + i + j + offset]);
            d += window[i + j + 96] * (synth_buf[63 - i + j + offset]);
            j += 128;
        }
        while j < 1024 {
            a += window[i + j] * (-synth_buf[31 - i + j + offset - 1024]);
            b += window[i + j + 32] * (synth_buf[i + j + offset - 1024]);
            c += window[i + j + 64] * (synth_buf[32 + i + j + offset - 1024]);
            d += window[i + j + 96] * (synth_buf[63 - i + j + offset - 1024]);
            j += 128;
        }
        out[i] = a * scale;
        out[i + 32] = b * scale;
        synth_buf2[i] = c;
        synth_buf2[i + 32] = d;
    }

    *synth_buf_offset = (*synth_buf_offset - 64) & 1023;
}

/// `synth_filter_fixed`: 32-subband fixed-point synthesis.
pub fn synth_filter_fixed(
    synth_buf: &mut [i32; 512],
    synth_buf_offset: &mut i32,
    synth_buf2: &mut [i32; 32],
    window: &[i32],
    out: &mut [i32],
    input: &[i32],
) {
    let offset = *synth_buf_offset as usize;
    let mut tmp32 = [0i32; 32];
    crate::dsp::imdct_half_32(&mut tmp32, input);
    for (k, &v) in tmp32.iter().enumerate() {
        synth_buf[(offset + k) % 512] = v;
    }

    for i in 0..16 {
        let mut a = i64::from(synth_buf2[i]) * (1i64 << 21);
        let mut b = i64::from(synth_buf2[i + 16]) * (1i64 << 21);
        let mut c: i64 = 0;
        let mut d: i64 = 0;
        let sb = |k: isize| -> i32 { synth_buf[(((k + offset as isize) % 512 + 512) % 512) as usize] };
        let mut j = 0usize;
        while j < 512 - offset {
            a += i64::from(window[i + j]) * i64::from(sb((i + j) as isize));
            b += i64::from(window[i + j + 16]) * i64::from(sb((15 - i + j) as isize));
            c += i64::from(window[i + j + 32]) * i64::from(sb((16 + i + j) as isize));
            d += i64::from(window[i + j + 48]) * i64::from(sb((31 - i + j) as isize));
            j += 64;
        }
        while j < 512 {
            a += i64::from(window[i + j]) * i64::from(sb((i + j - 512) as isize));
            b += i64::from(window[i + j + 16]) * i64::from(sb((15 - i + j - 512) as isize));
            c += i64::from(window[i + j + 32]) * i64::from(sb((16 + i + j - 512) as isize));
            d += i64::from(window[i + j + 48]) * i64::from(sb((31 - i + j - 512) as isize));
            j += 64;
        }
        out[i] = clip23(crate::math::norm21(a));
        out[i + 16] = clip23(crate::math::norm21(b));
        synth_buf2[i] = crate::math::norm21(c);
        synth_buf2[i + 16] = crate::math::norm21(d);
    }

    *synth_buf_offset = (*synth_buf_offset - 32) & 511;
}

/// `synth_filter_fixed_64`: 64-subband fixed-point synthesis.
#[allow(clippy::too_many_arguments)]
pub fn synth_filter_fixed_64(
    synth_buf: &mut [i32; 1024],
    synth_buf_offset: &mut i32,
    synth_buf2: &mut [i32; 64],
    window: &[i32],
    out: &mut [i32],
    input: &[i32],
) {
    let offset = *synth_buf_offset as usize;
    let mut tmp64 = [0i32; 64];
    crate::dsp::imdct_half_64(&mut tmp64, input);
    for (k, &v) in tmp64.iter().enumerate() {
        synth_buf[(offset + k) % 1024] = v;
    }

    for i in 0..32 {
        let mut a = i64::from(synth_buf2[i]) * (1i64 << 20);
        let mut b = i64::from(synth_buf2[i + 32]) * (1i64 << 20);
        let mut c: i64 = 0;
        let mut d: i64 = 0;
        let sb = |k: isize| -> i32 { synth_buf[(((k + offset as isize) % 1024 + 1024) % 1024) as usize] };
        let mut j = 0usize;
        while j < 1024 - offset {
            a += i64::from(window[i + j]) * i64::from(sb((i + j) as isize));
            b += i64::from(window[i + j + 32]) * i64::from(sb((31 - i + j) as isize));
            c += i64::from(window[i + j + 64]) * i64::from(sb((32 + i + j) as isize));
            d += i64::from(window[i + j + 96]) * i64::from(sb((63 - i + j) as isize));
            j += 128;
        }
        while j < 1024 {
            a += i64::from(window[i + j]) * i64::from(sb((i + j - 1024) as isize));
            b += i64::from(window[i + j + 32]) * i64::from(sb((31 - i + j - 1024) as isize));
            c += i64::from(window[i + j + 64]) * i64::from(sb((32 + i + j - 1024) as isize));
            d += i64::from(window[i + j + 96]) * i64::from(sb((63 - i + j - 1024) as isize));
            j += 128;
        }
        out[i] = clip23(crate::math::norm20(a));
        out[i + 32] = clip23(crate::math::norm20(b));
        synth_buf2[i] = crate::math::norm20(c);
        synth_buf2[i + 32] = crate::math::norm20(d);
    }

    *synth_buf_offset = (*synth_buf_offset - 64) & 1023;
}

// ───────────────────────── dcadsp.c ─────────────────────────

/// `decode_hf_c`: high-frequency VQ decode.
#[allow(clippy::too_many_arguments)]
pub fn decode_hf(
    dst: &mut [&mut [i32]],
    vq_index: &[i32],
    hf_vq: &[[i8; 32]],
    scale_factors: &[[i32; 2]],
    sb_start: usize,
    sb_end: usize,
    ofs: usize,
    len: usize,
) {
    for i in sb_start..sb_end {
        let coeff = &hf_vq[vq_index[i] as usize];
        let scale = scale_factors[i][0];
        for j in 0..len {
            dst[i][j + ofs] = clip23((i32::from(coeff[j]) * scale + (1 << 3)) >> 4);
        }
    }
}

/// `decode_joint_c`: joint intensity decoding.
pub fn decode_joint(
    dst: &mut [&mut [i32]],
    src: &[&[i32]],
    scale_factors: &[i32],
    sb_start: usize,
    sb_end: usize,
    ofs: usize,
    len: usize,
) {
    for i in sb_start..sb_end {
        let scale = scale_factors[i];
        for j in 0..len {
            dst[i][j + ofs] = clip23(mul17(src[i][j + ofs], scale));
        }
    }
}

/// `lfe_fir_float_c` (both decimation selects), history-extended input:
/// `lfe_samples` starts at the first new sample; indexes down to
/// `-(history)` are valid (the caller includes `DCA_LFE_HISTORY` samples
/// in front).
pub fn lfe_fir_float_ext(
    pcm_samples: &mut [f32],
    lfe_samples: &[i32],
    filter_coeff: &[f32],
    npcmblocks: usize,
    dec_select: usize,
    hist: usize,
) {
    lfe_fir_float_hist(pcm_samples, lfe_samples, filter_coeff, npcmblocks, dec_select, hist)
}

fn lfe_fir_float_hist(
    pcm_samples: &mut [f32],
    lfe_samples: &[i32],
    filter_coeff: &[f32],
    npcmblocks: usize,
    dec_select: usize,
    hist: usize,
) {
    let factor = 64usize << dec_select;
    let ncoeffs = 8usize >> dec_select;
    let nlfesamples = npcmblocks >> (dec_select + 1);
    let mut pcm_pos = 0usize;
    let mut lfe_pos = 0usize;

    for _ in 0..nlfesamples {
        // One decimated sample generates 64 or 128 interpolated ones
        for j in 0..factor / 2 {
            let mut a = 0.0f32;
            let mut b = 0.0f32;
            for k in 0..ncoeffs {
                let s = lfe_samples[lfe_pos + hist - k] as f32;
                a += filter_coeff[j * ncoeffs + k] * s;
                b += filter_coeff[255 - j * ncoeffs - k] * s;
            }
            pcm_samples[pcm_pos + j] = a;
            pcm_samples[pcm_pos + factor / 2 + j] = b;
        }
        lfe_pos += 1;
        pcm_pos += factor;
    }
}

/// `lfe_x96_float_c`.
pub fn lfe_x96_float(dst: &mut [f32], src: &[f32], hist: &mut f32, len: usize) {
    let mut prev = *hist;
    let mut dpos = 0usize;
    for i in 0..len {
        let a = 0.25 * src[i] + 0.75 * prev;
        let b = 0.75 * src[i] + 0.25 * prev;
        prev = src[i];
        dst[dpos] = a;
        dst[dpos + 1] = b;
        dpos += 2;
    }
    *hist = prev;
}

/// `lfe_fir_fixed_c` with history-extended input (see `lfe_fir_float_ext`).
pub fn lfe_fir_fixed_ext(
    pcm_samples: &mut [i32],
    lfe_samples: &[i32],
    filter_coeff: &[i32],
    npcmblocks: usize,
    hist: usize,
) {
    lfe_fir_fixed_hist(pcm_samples, lfe_samples, filter_coeff, npcmblocks, hist)
}

fn lfe_fir_fixed_hist(
    pcm_samples: &mut [i32],
    lfe_samples: &[i32],
    filter_coeff: &[i32],
    npcmblocks: usize,
    hist: usize,
) {
    let nlfesamples = npcmblocks >> 1;
    let mut pcm_pos = 0usize;
    let mut lfe_pos = 0usize;

    for _ in 0..nlfesamples {
        for j in 0..32 {
            let mut a: i64 = 0;
            let mut b: i64 = 0;
            for k in 0..8 {
                let s = i64::from(lfe_samples[lfe_pos + hist - k]);
                a += i64::from(filter_coeff[j * 8 + k]) * s;
                b += i64::from(filter_coeff[255 - j * 8 - k]) * s;
            }
            pcm_samples[pcm_pos + j] = clip23(norm23(a));
            pcm_samples[pcm_pos + 32 + j] = clip23(norm23(b));
        }
        lfe_pos += 1;
        pcm_pos += 64;
    }
}

/// `lfe_x96_fixed_c`.
pub fn lfe_x96_fixed(dst: &mut [i32], src: &[i32], hist: &mut i32, len: usize) {
    let mut prev = *hist;
    let mut dpos = 0usize;
    for i in 0..len {
        let a = 2097471i64 * i64::from(src[i]) + 6291137i64 * i64::from(prev);
        let b = 6291137i64 * i64::from(src[i]) + 2097471i64 * i64::from(prev);
        prev = src[i];
        dst[dpos] = clip23(norm23(a));
        dst[dpos + 1] = clip23(norm23(b));
        dpos += 2;
    }
    *hist = prev;
}

/// `decor_c`.
pub fn decor(dst: &mut [i32], src: &[i32], coeff: i32, len: usize) {
    for i in 0..len {
        dst[i] = dst[i].wrapping_add((src[i].wrapping_mul(coeff) + (1 << 2)) >> 3);
    }
}

/// `dmix_sub_xch_c`.
pub fn dmix_sub_xch(dst1: &mut [i32], dst2: &mut [i32], src: &[i32], len: usize) {
    for i in 0..len {
        let cs = mul23(src[i], 5931520); // M_SQRT1_2 * (1 << 23)
        dst1[i] -= cs;
        dst2[i] -= cs;
    }
}

/// `dmix_sub_c`.
pub fn dmix_sub(dst: &mut [i32], src: &[i32], coeff: i32, len: usize) {
    for i in 0..len {
        dst[i] = dst[i].wrapping_sub(mul15(src[i], coeff));
    }
}

/// `dmix_add_c`.
pub fn dmix_add(dst: &mut [i32], src: &[i32], coeff: i32, len: usize) {
    for i in 0..len {
        dst[i] = dst[i].wrapping_add(mul15(src[i], coeff));
    }
}

/// `dmix_scale_c`.
pub fn dmix_scale(dst: &mut [i32], scale: i32, len: usize) {
    for v in dst.iter_mut().take(len) {
        *v = mul15(*v, scale);
    }
}

/// `dmix_scale_inv_c`.
pub fn dmix_scale_inv(dst: &mut [i32], scale_inv: i32, len: usize) {
    for v in dst.iter_mut().take(len) {
        *v = mul16(*v, scale_inv);
    }
}

fn filter0(dst: &mut [i32], src: &[i32], coeff: i32, len: usize) {
    for i in 0..len {
        dst[i] = dst[i].wrapping_sub(mul22(src[i], coeff));
    }
}

/// `assemble_freq_bands_c`. `src0`/`src1` are the history-extended band
/// buffers: `DCA_XLL_DECI_HISTORY_MAX` (8) history samples before a
/// `len`-sample window, so each slice is `8 + len` long and windows are
/// taken at `offset = 8 - i .. 8 - i + len`. `dst` receives `2 * len`
/// interleaved samples.
pub fn assemble_freq_bands(dst: &mut [i32], src0: &mut [i32], src1: &mut [i32], coeff: &[i32], len: usize) {
    {
        let s1 = src1.to_vec();
        filter0(&mut src0[..len], &s1[..len], coeff[0], len);
    }
    {
        let s0 = src0.to_vec();
        filter0(&mut src1[..len], &s0[..len], coeff[1], len);
    }
    {
        let s1 = src1.to_vec();
        filter0(&mut src0[..len], &s1[..len], coeff[2], len);
    }
    {
        let s0 = src0.to_vec();
        filter0(&mut src1[..len], &s0[..len], coeff[3], len);
    }

    // `i++, src0--`: pass i reads src0 at base 8 - i (over the history),
    // src1 stays at base 8. Both filters index [0..len) from their bases.
    for i in 0..8usize {
        let b0 = 8 - i;
        let b1 = 8;
        let s0 = src0.to_vec();
        let s1 = src1.to_vec();
        filter1_sh(&mut src0[b0..b0 + len], &s1[b1..b1 + len], coeff[i + 4], len);
        filter1_sh(&mut src1[b1..b1 + len], &s0[b0..b0 + len], coeff[i + 12], len);
        filter1_sh(&mut src0[b0..b0 + len], &s1[b1..b1 + len], coeff[i + 4], len);
    }

    // Final read: after the loop src0's base sits at 0 (history start) and
    // src1's at 8. `*dst++ = *src1++` reads src1[8+n]; `*dst++ = *++src0`
    // (pre-increment from base 0) reads src0[1+n].
    let mut dpos = 0usize;
    for n in 0..len {
        dst[dpos] = src1[8 + n];
        dst[dpos + 1] = src0[1 + n];
        dpos += 2;
    }
}

/// `filter1` with an already-shifted source window.
fn filter1_sh(dst: &mut [i32], src: &[i32], coeff: i32, len: usize) {
    for i in 0..len {
        dst[i] = dst[i].wrapping_sub(mul23(src[i], coeff));
    }
}

/// `lbr_bank_c`: short window and 8-point forward MDCT for the LBR hybrid
/// filterbank. `input[ch]` slices must extend 4 history samples before
/// `ofs` and 4 after `ofs + len`.
pub fn lbr_bank(output: &mut [[f32; 4]], input: &[&[f32]], coeff: &[f32], ofs: usize, len: usize) {
    debug_assert!(ofs >= 4, "lbr_bank needs 4 history samples before ofs");
    let sw0 = coeff[0];
    let sw1 = coeff[1];
    let sw2 = coeff[2];
    let sw3 = coeff[3];
    let c1 = coeff[4];
    let c2 = coeff[5];
    let c3 = coeff[6];
    let c4 = coeff[7];
    let al1 = coeff[8];
    let al2 = coeff[9];

    for i in 0..len {
        let src = &input[i][ofs - 4..=ofs + 3];
        let a = src[0] * sw0 - src[3] * sw3;
        let b = src[1] * sw1 - src[2] * sw2;
        let c = src[6] * sw1 + src[5] * sw2;
        let d = src[7] * sw0 + src[4] * sw3;

        output[i][0] = c1 * b - c2 * c + c4 * a - c3 * d;
        output[i][1] = c1 * d - c2 * a - c4 * b - c3 * c;
        output[i][2] = c3 * b + c2 * d - c4 * c + c1 * a;
        output[i][3] = c3 * a - c2 * b + c4 * d - c1 * c;
    }

    // Aliasing cancellation for high frequencies
    let mut i = 12;
    while i + 1 < len {
        let a = output[i][3] * al1;
        let b = output[i + 1][0] * al1;
        output[i][3] += b - a;
        output[i + 1][0] -= b + a;
        let a = output[i][2] * al2;
        let b = output[i + 1][1] * al2;
        output[i][2] += b - a;
        output[i + 1][1] -= b + a;
        i += 1;
    }
}

/// `lfe_iir_c`: cascade of 5 biquads, `factor` outputs per input sample.
pub fn lfe_iir(output: &mut [f32], input: &[f32], iir: &[[f32; 4]], hist: &mut [[f32; 2]], factor: usize) {
    let mut opos = 0usize;
    for &res_in in input.iter().take(64) {
        let mut res = res_in;
        for _ in 0..factor {
            for k in 0..5 {
                let tmp = hist[k][0] * iir[k][0] + hist[k][1] * iir[k][1] + res;
                res = hist[k][0] * iir[k][2] + hist[k][1] * iir[k][3] + tmp;
                hist[k][0] = hist[k][1];
                hist[k][1] = tmp;
            }
            output[opos] = res;
            opos += 1;
            res = 0.0;
        }
    }
}

// ───────────────────────── float/fixed DSP kernels ─────────────────────────

/// `vector_fmul_add`: `dst[i] = src0[i] * src1[i] + src2[i]`.
pub fn vector_fmul_add(dst: &mut [f32], src0: &[f32], src1: &[f32], src2: &[f32], len: usize) {
    for i in 0..len {
        dst[i] = src0[i].mul_add(src1[i], src2[i]);
    }
}

/// `vector_fmul_reverse`: `dst[i] = src0[i] * src1[len-1-i]`.
pub fn vector_fmul_reverse(dst: &mut [f32], src0: &[f32], src1: &[f32], len: usize) {
    for i in 0..len {
        dst[i] = src0[i] * src1[len - 1 - i];
    }
}

/// `vector_fmac_scalar`: `dst[i] += src[i] * mul`.
pub fn vector_fmac_scalar(dst: &mut [f32], src: &[f32], mul: f32, len: usize) {
    for i in 0..len {
        dst[i] += src[i] * mul;
    }
}

/// `vector_fmul_scalar`: `dst[i] = src[i] * mul`.
pub fn vector_fmul_scalar(dst: &mut [f32], src: &[f32], mul: f32, len: usize) {
    for i in 0..len {
        dst[i] = src[i] * mul;
    }
}

/// `butterflies_float`: sum into v1, difference into v2.
pub fn butterflies_float(v1: &mut [f32], v2: &mut [f32], len: usize) {
    for i in 0..len {
        let (a, b) = (v1[i], v2[i]);
        v1[i] = a + b;
        v2[i] = a - b;
    }
}

/// `butterflies_fixed`.
pub fn butterflies_fixed(v1: &mut [i32], v2: &mut [i32], len: usize) {
    for i in 0..len {
        let (a, b) = (v1[i], v2[i]);
        v1[i] = clip23(norm16(i64::from(a) + i64::from(b)));
        v2[i] = clip23(norm16(i64::from(a) - i64::from(b)));
    }
}
