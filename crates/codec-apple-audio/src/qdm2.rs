//! QDM2 (QDesign Music Codec 2) decoder.
//!
//! Ported from FFmpeg's `libavcodec/qdm2.c`, `libavcodec/qdm2data.h`
//! and `libavcodec/qdm2_tablegen.h`, plus the MPEG-audio synthesis
//! filter pieces it uses — `libavcodec/mpegaudiodsp_template.c` (the
//! float `ff_mpa_synth_filter` / `ff_mpadsp_apply_window` /
//! `ff_dct32_float` path), `libavcodec/mpegaudiodsp.c`,
//! `libavcodec/mpegaudiodsp_data.c` (`ff_mpa_enwindow`) and
//! `libavcodec/dct32_template.c` (DCT32_FLOAT) — all LGPL-2.1-or-later,
//! commit 2da55bf, ported to safe Rust.
//!
//! Structure (mirrors qdm2.c): the extradata `QDCA` atom sets
//! channels/sample-rate/bitrate/block-size/fft-size/checksum-size;
//! each packet holds 16 subpackets of one superblock. Subpackets 9-12
//! drive the MPEG-style subband synthesis; subpackets 16-47 drive the
//! FFT tone synthesis; both stages sum into `output_buffer` which is
//! soft-clipped to S16.
//!
//! # Untrusted input
//!
//! Every array index that comes from the bitstream is checked (the
//! `get_bits_left` guards and `>= FF_ARRAY_ELEMS` checks of the C
//! code are preserved); allocation sizes are fixed at init from
//! validated extradata fields.

#![allow(clippy::needless_range_loop)]
#![allow(clippy::too_many_arguments)]

use std::collections::VecDeque;

use crate::bits::{LeBitReader, VlcTable};
use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

pub const QDM2_MAX_FRAME_SIZE: usize = 512;
const MPA_MAX_CHANNELS: usize = 2;
const MPA_FRAME_SIZE: usize = 1152;
const SBLIMIT: usize = 32;

const SOFTCLIP_THRESHOLD: i32 = 27600;
const HARDCLIP_THRESHOLD: i32 = 35716;

/// Result is 8, 16 or 30 (qdm2.c `QDM2_SB_USED`).
#[inline]
fn qdm2_sb_used(sub_sampling: i32) -> usize {
    if sub_sampling >= 2 {
        30
    } else {
        8 << sub_sampling
    }
}

#[inline]
fn fix_noise_idx(noise_idx: &mut usize) {
    if *noise_idx >= 3840 {
        *noise_idx -= 3840;
    }
}

// ═══════════════════════════ static tables (qdm2data.h) ═══════════════════════════

#[rustfmt::skip]
static TAB_LEVEL: [[u8; 2]; 24] = [
    [12,4],[17,4],[1,6],[8,6],[9,5],[20,7],[3,7],[5,6],[6,6],[2,7],[22,9],[23,10],
    [0,10],[21,8],[11,4],[19,5],[7,6],[4,6],[16,3],[10,4],[18,4],[15,3],[13,3],[14,3],
];
#[rustfmt::skip]
static TAB_DIFF: [[u8; 2]; 33] = [
    [2,3],[1,3],[5,3],[14,8],[20,9],[26,10],[25,12],[32,12],[19,11],[16,8],[24,9],[17,9],
    [12,7],[13,7],[9,5],[7,4],[3,2],[4,3],[8,6],[11,6],[18,8],[15,8],[30,11],[36,13],
    [34,13],[29,13],[0,13],[21,10],[28,10],[23,10],[22,8],[10,6],[6,4],
];
#[rustfmt::skip]
static TAB_RUN: [[u8; 2]; 6] = [[1,1],[2,2],[3,3],[4,4],[5,5],[0,5]];
#[rustfmt::skip]
static TAB_TONE_LEVEL_IDX_HI1: [[u8; 2]; 20] = [
    [4,3],[5,5],[9,10],[11,11],[13,12],[14,12],[10,10],[12,11],[17,14],[16,14],[18,15],[0,15],
    [19,14],[15,12],[8,8],[7,7],[6,6],[1,4],[2,2],[3,1],
];
#[rustfmt::skip]
static TAB_TONE_LEVEL_IDX_MID: [[u8; 2]; 13] = [
    [18,2],[19,4],[20,6],[14,7],[21,8],[13,9],[22,10],[12,11],[23,12],[0,12],[15,5],[16,3],[17,1],
];
#[rustfmt::skip]
static TAB_TONE_LEVEL_IDX_HI2: [[u8; 2]; 18] = [
    [14,4],[11,6],[19,7],[9,7],[13,5],[10,6],[20,8],[8,8],[6,10],[23,11],[0,11],[21,9],
    [7,8],[12,5],[18,4],[16,2],[15,2],[17,2],
];
#[rustfmt::skip]
static TAB_TYPE30: [[u8; 2]; 9] = [
    [2,3],[6,4],[7,5],[8,6],[0,6],[5,3],[1,3],[3,2],[4,2],
];
#[rustfmt::skip]
static TAB_TYPE34: [[u8; 2]; 10] = [
    [1,4],[9,5],[0,5],[3,3],[7,3],[8,3],[2,3],[4,3],[6,3],[5,3],
];
#[rustfmt::skip]
static TAB_FFT_TONE_OFFSET: [[u8; 2]; 153] = [
    /* 0 */ [2,2],[7,7],[15,8],[21,8],[3,6],[6,6],[13,7],[14,8],[18,8],[4,4],[5,5],[11,7],
            [10,7],[20,6],[12,8],[16,9],[22,10],[0,10],[17,7],[19,6],[8,6],[9,6],[1,1],
    /* 1 */ [8,6],[2,6],[7,6],[23,7],[12,7],[5,4],[10,6],[20,8],[25,9],[26,10],[27,11],[0,11],
            [22,7],[9,5],[13,6],[17,6],[4,5],[14,6],[19,7],[24,7],[3,6],[11,6],[21,6],[18,6],
            [16,6],[15,6],[6,3],[1,1],
    /* 2 */ [14,7],[17,7],[15,7],[23,9],[28,10],[29,11],[30,13],[0,13],[31,12],[25,8],[10,5],[8,4],
            [9,4],[4,4],[22,8],[3,8],[21,8],[26,9],[27,9],[12,6],[11,5],[16,7],[18,7],[20,8],
            [24,8],[19,7],[13,5],[5,3],[1,2],[6,3],[7,3],
    /* 3 */ [4,4],[7,4],[10,4],[3,10],[27,10],[29,10],[28,10],[22,8],[21,7],[15,6],[14,5],[8,4],
            [16,6],[19,7],[23,8],[26,9],[30,10],[33,13],[34,14],[0,14],[32,12],[31,11],[12,5],[5,3],
            [9,3],[1,4],[20,7],[25,8],[24,8],[18,6],[17,5],[6,3],[11,4],[13,4],
    /* 4 */ [5,3],[4,3],[19,8],[33,12],[31,12],[28,11],[34,14],[37,14],[35,15],[0,15],[36,14],[32,12],
            [30,11],[24,9],[22,8],[23,9],[29,10],[27,10],[17,6],[14,5],[7,4],[12,5],[1,6],[26,9],
            [3,9],[25,8],[20,7],[8,4],[10,4],[13,4],[15,6],[16,6],[18,6],[21,6],[11,4],[9,3],[6,3],
];
#[rustfmt::skip]
static FFT_LEVEL_EXP_ALT: [[u8; 2]; 28] = [
    [18,3],[16,3],[22,7],[8,10],[4,10],[3,9],[2,8],[23,8],[10,8],[11,7],[21,5],[20,4],
    [1,7],[7,10],[5,10],[9,9],[6,10],[25,11],[26,12],[27,13],[0,13],[24,9],[12,6],[13,5],
    [14,4],[19,3],[15,3],[17,2],
];
#[rustfmt::skip]
static FFT_LEVEL_EXP: [[u8; 2]; 20] = [
    [3,3],[11,6],[16,9],[17,10],[18,11],[19,12],[0,12],[15,8],[14,7],[9,5],[7,4],[2,3],
    [4,3],[1,3],[5,3],[12,6],[13,6],[10,5],[8,4],[6,3],
];
#[rustfmt::skip]
static FFT_STEREO_EXP: [[u8; 2]; 7] = [[2,2],[3,3],[4,4],[5,5],[6,6],[0,6],[1,1]];
#[rustfmt::skip]
static FFT_STEREO_PHASE: [[u8; 2]; 9] = [
    [2,2],[1,2],[3,4],[7,4],[6,5],[5,6],[0,6],[4,4],[8,2],
];

static FFT_CUTOFF_INDEX_TABLE: [[i32; 2]; 4] = [[1, 2], [-1, 0], [-1, -2], [0, 0]];

#[rustfmt::skip]
static FFT_LEVEL_INDEX_TABLE: [usize; 256] = [
    0,0,0,0,0,0,0,0,1,1,1,1,1,1,1,1,
    2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,
    3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,
    3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,
    4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,
    4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,
    4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,
    4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,
    5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
    5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
    5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
    5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
    5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
    5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
    5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
    5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
];

static LAST_COEFF: [usize; 3] = [4, 7, 10];

#[rustfmt::skip]
static COEFF_PER_SB_FOR_AVG: [[u8; 30]; 3] = [
    [0,1,1,1,1,2,2,2,2,2,2,2,2,2,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3],
    [0,1,2,2,3,3,4,4,4,4,4,4,5,5,5,5,5,5,5,5,6,6,6,6,6,6,6,6,6,6],
    [0,1,2,3,4,4,5,5,6,6,6,6,7,7,7,7,8,8,8,8,8,8,9,9,9,9,9,9,9,9],
];

/// `dequant_table[3][10][30]` (qdm2data.h), transcribed verbatim.
static DEQUANT_TABLE: std::sync::LazyLock<[[[u32; 30]; 10]; 3]> = std::sync::LazyLock::new(|| {
    let mut t = [[[0u32; 30]; 10]; 3];
    // ── selector 0 ──
    t[0][0][0] = 256;
    let r: [u32; 7] = [256, 205, 154, 102, 51, 0, 0];
    for (i, v) in r.iter().enumerate() {
        t[0][1][1 + i] = *v;
    }
    let r: [u32; 21] = [
        51, 102, 154, 205, 256, 238, 219, 201, 183, 165, 146, 128, 110, 91, 73, 55, 37, 18, 0,
        0, 0,
    ];
    for (i, v) in r.iter().enumerate() {
        t[0][2][3 + i] = *v;
    }
    let r: [u32; 22] = [
        0, 0, 0, 0, 0, 0, 0, 0, 18, 37, 55, 73, 91, 110, 128, 146, 165, 183, 201, 219, 238,
        256,
    ];
    for (i, v) in r.iter().enumerate() {
        t[0][3][8 + i] = *v;
    }
    let r: [u32; 9] = [228, 199, 171, 142, 114, 85, 57, 28, 0];
    for (i, v) in r.iter().enumerate() {
        t[0][4][22 + i] = *v;
    }
    // rows 5..9 all zero
    // ── selector 1 ──
    t[1][0][0] = 256;
    t[1][1][1] = 256;
    let r: [u32; 5] = [256, 171, 85, 0, 0];
    for (i, v) in r.iter().enumerate() {
        t[1][2][2 + i] = *v;
    }
    let r: [u32; 7] = [85, 171, 256, 171, 85, 0, 0];
    for (i, v) in r.iter().enumerate() {
        t[1][3][3 + i] = *v;
    }
    let r: [u32; 10] = [85, 171, 256, 219, 183, 146, 110, 73, 37, 0];
    for (i, v) in r.iter().enumerate() {
        t[1][4][6 + i] = *v;
    }
    let r: [u32; 13] = [37, 73, 110, 146, 183, 219, 256, 228, 199, 171, 142, 114, 85];
    for (i, v) in r.iter().enumerate() {
        t[1][5][9 + i] = *v;
    }
    let r: [u32; 15] = [
        28, 57, 85, 114, 142, 171, 199, 228, 256, 213, 171, 128, 85, 43, 0,
    ];
    for (i, v) in r.iter().enumerate() {
        t[1][6][16 + i] = *v;
    }
    t[1][6][29] = 43;
    // rows 7..9 zero
    // ── selector 2 ──
    t[2][0][0] = 256;
    t[2][1][1] = 256;
    t[2][2][2] = 256;
    t[2][3][3] = 256;
    t[2][4][4] = 256;
    t[2][4][5] = 256;
    let r: [u32; 4] = [256, 171, 85, 0];
    for (i, v) in r.iter().enumerate() {
        t[2][5][6 + i] = *v;
    }
    let r: [u32; 6] = [85, 171, 256, 192, 128, 64];
    for (i, v) in r.iter().enumerate() {
        t[2][6][7 + i] = *v;
    }
    let r: [u32; 9] = [64, 128, 192, 256, 205, 154, 102, 51, 0];
    for (i, v) in r.iter().enumerate() {
        t[2][7][10 + i] = *v;
    }
    let r: [u32; 10] = [51, 102, 154, 205, 256, 213, 171, 128, 85, 43];
    for (i, v) in r.iter().enumerate() {
        t[2][8][14 + i] = *v;
    }
    let r: [u32; 11] = [43, 85, 128, 171, 213, 256, 213, 171, 128, 85, 43];
    for (i, v) in r.iter().enumerate() {
        t[2][9][19 + i] = *v;
    }
    t
});

#[rustfmt::skip]
static COEFF_PER_SB_FOR_DEQUANT: [[u8; 30]; 3] = [
    [0,1,1,1,1,1,1,2,2,2,2,2,2,2,2,2,2,2,2,2,2,3,3,3,3,3,3,3,3,3],
    [0,1,2,2,2,3,3,3,4,4,4,4,4,4,4,5,5,5,5,5,5,5,5,5,6,6,6,6,6,6],
    [0,1,2,3,4,4,5,5,5,6,6,6,6,7,7,7,7,7,8,8,8,8,8,8,9,9,9,9,9,9],
];

#[rustfmt::skip]
static CODING_METHOD_TABLE: [[i8; 30]; 5] = [
    [34,30,24,24,16,16,16,16,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10],
    [34,30,24,24,16,16,16,16,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10],
    [34,30,30,30,24,24,16,16,16,16,16,16,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10,10],
    [34,34,30,30,24,24,24,24,16,16,16,16,16,16,16,16,16,16,16,16,16,16,10,10,10,10,10,10,10,10],
    [34,34,30,30,30,30,30,30,24,24,24,24,24,24,24,24,24,24,24,24,16,16,16,16,16,16,16,16,16,16],
];

#[rustfmt::skip]
static VLC_STAGE3_VALUES: [i32; 60] = [
    0,1,2,3,4,6,8,10,12,16,20,24,
    28,36,44,52,60,76,92,108,124,156,188,220,
    252,316,380,444,508,636,764,892,1020,1276,1532,1788,
    2044,2556,3068,3580,4092,5116,6140,7164,8188,10236,12284,14332,
    16380,20476,24572,28668,32764,40956,49148,57340,65532,81916,98300,114684,
];

/// `fft_tone_sample_table[4][16][5]` (qdm2data.h), transcribed
/// verbatim from the C float literals.
#[rustfmt::skip]
static FFT_TONE_SAMPLE_TABLE: [[[f32; 5]; 16]; 4] = [
    [
        [ 0.0100000000,-0.0037037037,-0.0020000000,-0.0069444444,-0.0018416207],
        [ 0.0416666667, 0.0000000000, 0.0000000000,-0.0208333333,-0.0123456791],
        [ 0.1250000000, 0.0558035709, 0.0330687836,-0.0164473690,-0.0097465888],
        [ 0.1562500000, 0.0625000000, 0.0370370370,-0.0062500000,-0.0037037037],
        [ 0.1996007860, 0.0781250000, 0.0462962948, 0.0022727272, 0.0013468013],
        [ 0.2000000000, 0.0625000000, 0.0370370373, 0.0208333333, 0.0074074073],
        [ 0.2127659619, 0.0555555556, 0.0329218097, 0.0208333333, 0.0123456791],
        [ 0.2173913121, 0.0473484844, 0.0280583613, 0.0347222239, 0.0205761325],
        [ 0.2173913121, 0.0347222239, 0.0205761325, 0.0473484844, 0.0280583613],
        [ 0.2127659619, 0.0208333333, 0.0123456791, 0.0555555556, 0.0329218097],
        [ 0.2000000000, 0.0208333333, 0.0074074073, 0.0625000000, 0.0370370370],
        [ 0.1996007860, 0.0022727272, 0.0013468013, 0.0781250000, 0.0462962948],
        [ 0.1562500000,-0.0062500000,-0.0037037037, 0.0625000000, 0.0370370370],
        [ 0.1250000000,-0.0164473690,-0.0097465888, 0.0558035709, 0.0330687836],
        [ 0.0416666667,-0.0208333333,-0.0123456791, 0.0000000000, 0.0000000000],
        [ 0.0100000000,-0.0069444444,-0.0018416207,-0.0037037037,-0.0020000000],
    ],
    [
        [ 0.0050000000,-0.0200000000, 0.0125000000,-0.3030303030, 0.0020000000],
        [ 0.1041666642, 0.0400000000,-0.0250000000, 0.0333333333,-0.0200000000],
        [ 0.1250000000, 0.0100000000, 0.0142857144,-0.0500000007,-0.0200000000],
        [ 0.1562500000,-0.0006250000,-0.00049382716,-0.000625000,-0.00049382716],
        [ 0.1562500000,-0.0006250000,-0.00049382716,-0.000625000,-0.00049382716],
        [ 0.1250000000,-0.0500000000,-0.0200000000, 0.0100000000, 0.0142857144],
        [ 0.1041666667, 0.0333333333,-0.0200000000, 0.0400000000,-0.0250000000],
        [ 0.0050000000,-0.3030303030, 0.0020000001,-0.0200000000, 0.0125000000],
        [0.0,0.0,0.0,0.0,0.0],[0.0,0.0,0.0,0.0,0.0],[0.0,0.0,0.0,0.0,0.0],
        [0.0,0.0,0.0,0.0,0.0],[0.0,0.0,0.0,0.0,0.0],[0.0,0.0,0.0,0.0,0.0],
        [0.0,0.0,0.0,0.0,0.0],[0.0,0.0,0.0,0.0,0.0],
    ],
    [
        [ 0.1428571492, 0.1250000000,-0.0285714287,-0.0357142873, 0.0208333333],
        [ 0.1818181818, 0.0588235296, 0.0333333333, 0.0212765951, 0.0100000000],
        [ 0.1818181818, 0.0212765951, 0.0100000000, 0.0588235296, 0.0333333333],
        [ 0.1428571492,-0.0357142873, 0.0208333333, 0.1250000000,-0.0285714287],
        [0.0,0.0,0.0,0.0,0.0],[0.0,0.0,0.0,0.0,0.0],[0.0,0.0,0.0,0.0,0.0],
        [0.0,0.0,0.0,0.0,0.0],[0.0,0.0,0.0,0.0,0.0],[0.0,0.0,0.0,0.0,0.0],
        [0.0,0.0,0.0,0.0,0.0],[0.0,0.0,0.0,0.0,0.0],[0.0,0.0,0.0,0.0,0.0],
        [0.0,0.0,0.0,0.0,0.0],[0.0,0.0,0.0,0.0,0.0],[0.0,0.0,0.0,0.0,0.0],
    ],
    [ [0.0; 5]; 16 ],
];

#[rustfmt::skip]
static FFT_TONE_LEVEL_TABLE: [[f32; 64]; 2] = [
    [
        0.17677669, 0.42677650, 0.60355347, 0.85355347,
        1.20710683, 1.68359375, 2.37500000, 3.36718750,
        4.75000000, 6.73437500, 9.50000000, 13.4687500,
        19.0000000, 26.9375000, 38.0000000, 53.8750000,
        76.0000000, 107.750000, 152.000000, 215.500000,
        304.000000, 431.000000, 608.000000, 862.000000,
        1216.00000, 1724.00000, 2432.00000, 3448.00000,
        4864.00000, 6896.00000, 9728.00000, 13792.0000,
        19456.0000, 27584.0000, 38912.0000, 55168.0000,
        77824.0000, 110336.000, 155648.000, 220672.000,
        311296.000, 441344.000, 622592.000, 882688.000,
        1245184.00, 1765376.00, 2490368.00, 0.0,
        0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0,
    ],
    [
        0.59375000, 0.84179688, 1.18750000, 1.68359375,
        2.37500000, 3.36718750, 4.75000000, 6.73437500,
        9.50000000, 13.4687500, 19.0000000, 26.9375000,
        38.0000000, 53.8750000, 76.0000000, 107.750000,
        152.000000, 215.500000, 304.000000, 431.000000,
        608.000000, 862.000000, 1216.00000, 1724.00000,
        2432.00000, 3448.00000, 4864.00000, 6896.00000,
        9728.00000, 13792.0000, 19456.0000, 27584.0000,
        38912.0000, 55168.0000, 77824.0000, 110336.000,
        155648.000, 220672.000, 311296.000, 441344.000,
        622592.000, 882688.000, 1245184.00, 1765376.00,
        2490368.00, 3530752.00, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0,
    ],
];

#[rustfmt::skip]
static FFT_TONE_ENVELOPE_TABLE: [[f32; 31]; 4] = [
    [
         0.009607375,0.038060248,0.084265202,0.146446645,0.222214907,0.308658302,
        0.402454883,0.500000060,0.597545207,0.691341758,0.777785182,0.853553414,
        0.915734828,0.961939812,0.990392685,1.00000000,0.990392625,0.961939752,
        0.915734768,0.853553295,0.777785063,0.691341639,0.597545087,0.500000000,
         0.402454853,0.308658272,0.222214878,0.146446615,0.084265172,0.038060218,
        0.009607345,
    ],
    [
         0.038060248,0.146446645,0.308658302,0.500000060,0.691341758,0.853553414,
        0.961939812,1.00000000,0.961939752,0.853553295,0.691341639,0.500000000,
         0.308658272,0.146446615,0.038060218,0.0,0.0,0.0,0.0,0.0,0.0,0.0,
        0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,
    ],
    [
        0.146446645,0.500000060,0.853553414,1.00000000,0.853553295,0.500000000,
        0.146446615,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,
        0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,
    ],
    [
        0.500000060,1.00000000,0.500000000,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,
        0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,
        0.0,0.0,0.0,0.0,0.0,
    ],
];

#[rustfmt::skip]
static SB_NOISE_ATTENUATION: [f32; 32] = [
    0.0,0.0,0.3,0.4,0.5,0.7,1.0,1.0,
    1.0,1.0,1.0,1.0,1.0,1.0,1.0,1.0,
    1.0,1.0,1.0,1.0,1.0,1.0,1.0,1.0,
    1.0,1.0,1.0,1.0,1.0,1.0,1.0,1.0,
];

#[rustfmt::skip]
static FFT_SUBPACKETS: [u8; 32] = [
    0,0,0,0,0,0,0,0,1,1,1,1,1,1,1,0,
    0,0,0,0,0,0,0,0,1,1,1,1,1,1,0,0,
];

#[rustfmt::skip]
static DEQUANT_1BIT: [[f32; 3]; 2] = [
    [-0.920000, 0.000000, 0.920000],
    [-0.890000, 0.000000, 0.890000],
];

#[rustfmt::skip]
static TYPE30_DEQUANT: [f32; 8] = [
    -1.0,-0.625,-0.291666656732559,0.0,
    0.25,0.5,0.75,1.0,
];

#[rustfmt::skip]
static TYPE34_DELTA: [f32; 10] = [
    -1.0,-0.60947573184967,-0.333333343267441,-0.138071194291115,0.0,
    0.138071194291115,0.333333343267441,0.60947573184967,1.0,0.0,
];

/// qdm2_tablegen.h `softclip_table_init`.
static SOFTCLIP_TABLE: std::sync::LazyLock<Vec<u16>> = std::sync::LazyLock::new(|| {
    let dfl = (SOFTCLIP_THRESHOLD - 32767) as f64;
    let delta = 1.0 / -dfl;
    let n = (HARDCLIP_THRESHOLD - SOFTCLIP_THRESHOLD + 1) as usize;
    let mut t = Vec::with_capacity(n);
    for i in 0..n {
        let s = ((i as f32) * delta as f32).sin();
        // C: (int)(sin * dfl) & 0x0000FFFF
        let v = ((s as f64 * dfl) as i32) & 0x0000FFFF;
        t.push((SOFTCLIP_THRESHOLD as u16).wrapping_sub(v as u16));
    }
    t
});

/// qdm2_tablegen.h `rnd_table_init` noise_table.
static NOISE_TABLE: std::sync::LazyLock<Vec<f32>> = std::sync::LazyLock::new(|| {
    let delta = 1.0f32 / 16384.0;
    let mut random_seed: u64 = 0;
    let mut t = Vec::with_capacity(4096 + 20);
    for _ in 0..4096 {
        random_seed = random_seed.wrapping_mul(214013).wrapping_add(2531011);
        let v = ((random_seed >> 16) as i32 & 0x00007FFF) as f32;
        t.push((delta * v - 1.0) * 1.3);
    }
    t.resize(4096 + 20, 0.0);
    t
});

/// `random_dequant_index[256][5]`.
static RANDOM_DEQUANT_INDEX: std::sync::LazyLock<[[u8; 5]; 256]> = std::sync::LazyLock::new(|| {
    let mut t = [[0u8; 5]; 256];
    for (i, row) in t.iter_mut().enumerate() {
        let mut random_seed: u64 = 81;
        let mut ldw: u64 = i as u64;
        for slot in row.iter_mut() {
            *slot = (ldw / random_seed) as u8;
            ldw %= random_seed;
            random_seed /= 3;
        }
    }
    t
});

/// `random_dequant_type24[128][3]`.
static RANDOM_DEQUANT_TYPE24: std::sync::LazyLock<[[u8; 3]; 128]> = std::sync::LazyLock::new(|| {
    let mut t = [[0u8; 3]; 128];
    for (i, row) in t.iter_mut().enumerate() {
        let mut random_seed: u64 = 25;
        let mut ldw: u64 = i as u64;
        for slot in row.iter_mut() {
            *slot = (ldw / random_seed) as u8;
            ldw %= random_seed;
            random_seed /= 2;
        }
    }
    t
});

/// `noise_samples[128]`.
static NOISE_SAMPLES: std::sync::LazyLock<[f32; 128]> = std::sync::LazyLock::new(|| {
    let delta = 1.0f32 / 16384.0;
    let mut random_seed: u32 = 0;
    let mut t = [0f32; 128];
    for slot in t.iter_mut() {
        random_seed = random_seed.wrapping_mul(214013).wrapping_add(2531011);
        *slot = delta * ((random_seed >> 16) & 0x00007fff) as f32 - 1.0;
    }
    t
});

#[inline]
fn sb_dithering_noise(sb: usize, noise_idx: &mut usize) -> f32 {
    let v = NOISE_TABLE[*noise_idx] * SB_NOISE_ATTENUATION[sb];
    *noise_idx += 1;
    v
}

// ═══════════════════════════ VLC tables ═══════════════════════════

/// Build a `VlcTable` from the `tab[][2]` `{symbol, length}` layout
/// with symbols in *length-list* order (qdm2_tablegen.h `build_vlc`
/// passes lengths with symbols implied by position and an `offset`).
fn build_vlc(tab: &[[u8; 2]]) -> VlcTable {
    // FFmpeg's ff_vlc_init_from_lengths receives `&tab[0][1]` (the
    // lengths) and offset -1: entry i is symbol tab[i][0] with length
    // tab[i][1]. We build directly from (symbol, length) pairs.
    let mut lens = vec![0i8; tab.iter().map(|r| r[0] as usize + 1).max().unwrap_or(0)];
    for row in tab {
        lens[row[0] as usize] = row[1] as i8;
    }
    VlcTable::from_lengths(&lens, -1).unwrap_or_default()
}

struct Qdm2Vlc {
    tab_level: VlcTable,
    tab_diff: VlcTable,
    tab_run: VlcTable,
    fft_level_exp_alt: VlcTable,
    fft_level_exp: VlcTable,
    fft_stereo_exp: VlcTable,
    fft_stereo_phase: VlcTable,
    tab_tone_level_idx_hi1: VlcTable,
    tab_tone_level_idx_mid: VlcTable,
    tab_tone_level_idx_hi2: VlcTable,
    tab_type30: VlcTable,
    tab_type34: VlcTable,
    tab_fft_tone_offset: [VlcTable; 5],
}

static VLC: std::sync::LazyLock<Qdm2Vlc> = std::sync::LazyLock::new(|| Qdm2Vlc {
    tab_level: build_vlc(&TAB_LEVEL),
    tab_diff: build_vlc(&TAB_DIFF),
    tab_run: build_vlc(&TAB_RUN),
    fft_level_exp_alt: build_vlc(&FFT_LEVEL_EXP_ALT),
    fft_level_exp: build_vlc(&FFT_LEVEL_EXP),
    fft_stereo_exp: build_vlc(&FFT_STEREO_EXP),
    fft_stereo_phase: build_vlc(&FFT_STEREO_PHASE),
    tab_tone_level_idx_hi1: build_vlc(&TAB_TONE_LEVEL_IDX_HI1),
    tab_tone_level_idx_mid: build_vlc(&TAB_TONE_LEVEL_IDX_MID),
    tab_tone_level_idx_hi2: build_vlc(&TAB_TONE_LEVEL_IDX_HI2),
    tab_type30: build_vlc(&TAB_TYPE30),
    tab_type34: build_vlc(&TAB_TYPE34),
    tab_fft_tone_offset: [
        build_vlc(&TAB_FFT_TONE_OFFSET[0..23].to_vec()),
        build_vlc(&TAB_FFT_TONE_OFFSET[23..51].to_vec()),
        build_vlc(&TAB_FFT_TONE_OFFSET[51..82].to_vec()),
        build_vlc(&TAB_FFT_TONE_OFFSET[82..116].to_vec()),
        build_vlc(&TAB_FFT_TONE_OFFSET[116..153].to_vec()),
    ],
});

// ═══════════════════════════ MPEG-audio float synthesis (mpegaudiodsp) ═══════════════════════════

mod mpa_synth {
    //! Float MPEG-audio polyphase synthesis filter, ported from
    //! FFmpeg's `mpegaudiodsp_template.c` (USE_FLOATS=1) +
    //! `dct32_template.c` (DCT32_FLOAT) + `mpegaudiodsp_data.c`.

    pub const FRAC_BITS: u32 = 23;

    /// `ff_mpa_synth_window` float window (512 + 256 values).
    static SYNTH_WINDOW: std::sync::LazyLock<Vec<f32>> = std::sync::LazyLock::new(|| {
        let mut window = vec![0f32; 512 + 256];
        for i in 0..257 {
            let mut v = ENWINDOW[i] as f32;
            v *= 1.0 / (1u64 << (16 + FRAC_BITS)) as f32;
            window[i] = v;
            if (i & 63) != 0 {
                v = -v;
            }
            if i != 0 {
                window[512 - i] = v;
            }
        }
        // ASM shuffle-avoidance mirrors (unused by the Rust port but
        // part of the table layout in FFmpeg; omitted).
        window
    });

    /// `ff_mpa_enwindow` (mpegaudiodsp_data.c), 257 values.
    #[rustfmt::skip]
    static ENWINDOW: [i32; 257] = [
        0,-1,-1,-1,-1,-1,-1,-2,
        -2,-2,-2,-3,-3,-4,-4,-5,
        -5,-6,-7,-7,-8,-9,-10,-11,
        -13,-14,-16,-17,-19,-21,-24,-26,
        -29,-31,-35,-38,-41,-45,-49,-53,
        -58,-63,-68,-73,-79,-85,-91,-97,
        -104,-111,-117,-125,-132,-139,-147,-154,
        -161,-169,-176,-183,-190,-196,-202,-208,
        213,218,222,225,227,228,228,227,
        224,221,215,208,200,189,177,163,
        146,127,106,83,57,29,-2,-36,
        -72,-111,-153,-197,-244,-294,-347,-401,
        -459,-519,-581,-645,-711,-779,-848,-919,
        -991,-1064,-1137,-1210,-1283,-1356,-1428,-1498,
        -1567,-1634,-1698,-1759,-1817,-1870,-1919,-1962,
        -2001,-2032,-2057,-2075,-2085,-2087,-2080,-2063,
        2037,2000,1952,1893,1822,1739,1644,1535,
        1414,1280,1131,970,794,605,402,185,
        -45,-288,-545,-814,-1095,-1388,-1692,-2006,
        -2330,-2663,-3004,-3351,-3705,-4063,-4425,-4788,
        -5153,-5517,-5879,-6237,-6589,-6935,-7271,-7597,
        -7910,-8209,-8491,-8755,-8998,-9219,-9416,-9585,
        -9727,-9838,-9916,-9959,-9966,-9935,-9863,-9750,
        -9592,-9389,-9139,-8840,-8492,-8092,-7640,-7134,
        6574,5959,5288,4561,3776,2935,2037,1082,
        70,-998,-2122,-3300,-4533,-5818,-7154,-8540,
        -9975,-11455,-12980,-14548,-16155,-17799,-19478,-21189,
        -22929,-24694,-26482,-28289,-30112,-31947,-33791,-35640,
        -37489,-39336,-41176,-43006,-44821,-46617,-48390,-50137,
        -51853,-53534,-55178,-56778,-58333,-59838,-61289,-62684,
        -64019,-65290,-66494,-67629,-68692,-69679,-70590,-71420,
        -72169,-72835,-73415,-73908,-74313,-74630,-74856,-74992,
        75038,
    ];

    // ── dct32 (dct32_template.c, DCT32_FLOAT) ──
    //
    // Constants (FIXHR = value as float; MULH3(x, y, s) = s*y*x).
    const COS0_0: f32 = 0.50060299823519630134 / 2.0;
    const COS0_1: f32 = 0.50547095989754365998 / 2.0;
    const COS0_2: f32 = 0.51544730992262454697 / 2.0;
    const COS0_3: f32 = 0.53104259108978417447 / 2.0;
    const COS0_4: f32 = 0.55310389603444452782 / 2.0;
    const COS0_5: f32 = 0.58293496820613387367 / 2.0;
    const COS0_6: f32 = 0.62250412303566481615 / 2.0;
    const COS0_7: f32 = 0.67480834145500574602 / 2.0;
    const COS0_8: f32 = 0.74453627100229844977 / 2.0;
    const COS0_9: f32 = 0.83934964541552703873 / 2.0;
    const COS0_10: f32 = 0.97256823786196069369 / 2.0;
    const COS0_11: f32 = 1.16943993343288495515 / 4.0;
    const COS0_12: f32 = 1.48416461631416627724 / 4.0;
    const COS0_13: f32 = 2.05778100995341155085 / 8.0;
    const COS0_14: f32 = 3.40760841846871878570 / 8.0;
    const COS0_15: f32 = 10.19000812354805681150 / 32.0;

    const COS1_0: f32 = 0.50241928618815570551 / 2.0;
    const COS1_1: f32 = 0.52249861493968888062 / 2.0;
    const COS1_2: f32 = 0.56694403481635770368 / 2.0;
    const COS1_3: f32 = 0.64682178335999012954 / 2.0;
    const COS1_4: f32 = 0.78815462345125022473 / 2.0;
    const COS1_5: f32 = 1.06067768599034747134 / 4.0;
    const COS1_6: f32 = 1.72244709823833392782 / 4.0;
    const COS1_7: f32 = 5.10114861868916385802 / 16.0;

    const COS2_0: f32 = 0.50979557910415916894 / 2.0;
    const COS2_1: f32 = 0.60134488693504528054 / 2.0;
    const COS2_2: f32 = 0.89997622313641570463 / 2.0;
    const COS2_3: f32 = 2.56291544774150617881 / 8.0;

    const COS3_0: f32 = 0.54119610014619698439 / 2.0;
    const COS3_1: f32 = 1.30656296487637652785 / 4.0;

    const COS4_0: f32 = std::f32::consts::FRAC_1_SQRT_2 / 2.0;

    /// One butterfly `BF(a, b, c, s)`: tmp0 = a + b; tmp1 = a - b;
    /// a = tmp0; b = MULH3(tmp1, c, 1<<s) = (1<<s)*c*tmp1.
    #[inline]
    fn bf(a: f32, b: f32, c: f32, s: i32) -> (f32, f32) {
        let tmp0 = a + b;
        let tmp1 = a - b;
        (tmp0, (s as f32) * c * tmp1)
    }

    /// DCT32 without 1/sqrt(2) coef zero scaling (float variant).
    /// `out` receives 32 values; `tab` is the 32 input subband
    /// samples.
    pub fn dct32(out: &mut [f32; 32], tab: &[f32; 32]) {
        let mut v = [0f32; 32];

        macro_rules! bf_pair {
            ($a:expr, $b:expr, $c:expr, $s:expr) => {{
                let (na, nb) = bf(v[$a], v[$b], $c, $s);
                v[$a] = na;
                v[$b] = nb;
            }};
        }
        // BF0 variant operates on `tab` values feeding v — in the
        // float port v starts as tab.
        macro_rules! bf0_pair {
            ($a:expr, $b:expr, $c:expr, $s:expr) => {{
                let (na, nb) = bf(tab[$a], tab[$b], $c, $s);
                v[$a] = na;
                v[$b] = nb;
            }};
        }

        // pass 1
        bf0_pair!(0, 31, COS0_0, 1);
        bf0_pair!(15, 16, COS0_15, 5);
        // pass 2
        bf_pair!(0, 15, COS1_0, 1);
        bf_pair!(16, 31, -COS1_0, 1);
        // pass 1
        bf0_pair!(7, 24, COS0_7, 1);
        bf0_pair!(8, 23, COS0_8, 1);
        // pass 2
        bf_pair!(7, 8, COS1_7, 4);
        bf_pair!(23, 24, -COS1_7, 4);
        // pass 3
        bf_pair!(0, 7, COS2_0, 1);
        bf_pair!(8, 15, -COS2_0, 1);
        bf_pair!(16, 23, COS2_0, 1);
        bf_pair!(24, 31, -COS2_0, 1);
        // pass 1
        bf0_pair!(3, 28, COS0_3, 1);
        bf0_pair!(12, 19, COS0_12, 2);
        // pass 2
        bf_pair!(3, 12, COS1_3, 1);
        bf_pair!(19, 28, -COS1_3, 1);
        // pass 1
        bf0_pair!(4, 27, COS0_4, 1);
        bf0_pair!(11, 20, COS0_11, 2);
        // pass 2
        bf_pair!(4, 11, COS1_4, 1);
        bf_pair!(20, 27, -COS1_4, 1);
        // pass 3
        bf_pair!(3, 4, COS2_3, 3);
        bf_pair!(11, 12, -COS2_3, 3);
        bf_pair!(19, 20, COS2_3, 3);
        bf_pair!(27, 28, -COS2_3, 3);
        // pass 4
        bf_pair!(0, 3, COS3_0, 1);
        bf_pair!(4, 7, -COS3_0, 1);
        bf_pair!(8, 11, COS3_0, 1);
        bf_pair!(12, 15, -COS3_0, 1);
        bf_pair!(16, 19, COS3_0, 1);
        bf_pair!(20, 23, -COS3_0, 1);
        bf_pair!(24, 27, COS3_0, 1);
        bf_pair!(28, 31, -COS3_0, 1);

        // pass 1
        bf0_pair!(1, 30, COS0_1, 1);
        bf0_pair!(14, 17, COS0_14, 3);
        // pass 2
        bf_pair!(1, 14, COS1_1, 1);
        bf_pair!(17, 30, -COS1_1, 1);
        // pass 1
        bf0_pair!(6, 25, COS0_6, 1);
        bf0_pair!(9, 22, COS0_9, 1);
        // pass 2
        bf_pair!(6, 9, COS1_6, 2);
        bf_pair!(22, 25, -COS1_6, 2);
        // pass 3
        bf_pair!(1, 6, COS2_1, 1);
        bf_pair!(9, 14, -COS2_1, 1);
        bf_pair!(17, 22, COS2_1, 1);
        bf_pair!(25, 30, -COS2_1, 1);

        // pass 1
        bf0_pair!(2, 29, COS0_2, 1);
        bf0_pair!(13, 18, COS0_13, 3);
        // pass 2
        bf_pair!(2, 13, COS1_2, 1);
        bf_pair!(18, 29, -COS1_2, 1);
        // pass 1
        bf0_pair!(5, 26, COS0_5, 1);
        bf0_pair!(10, 21, COS0_10, 1);
        // pass 2
        bf_pair!(5, 10, COS1_5, 2);
        bf_pair!(21, 26, -COS1_5, 2);
        // pass 3
        bf_pair!(2, 5, COS2_2, 1);
        bf_pair!(10, 13, -COS2_2, 1);
        bf_pair!(18, 21, COS2_2, 1);
        bf_pair!(26, 29, -COS2_2, 1);
        // pass 4
        bf_pair!(1, 2, COS3_1, 2);
        bf_pair!(5, 6, -COS3_1, 2);
        bf_pair!(9, 10, COS3_1, 2);
        bf_pair!(13, 14, -COS3_1, 2);
        bf_pair!(17, 18, COS3_1, 2);
        bf_pair!(21, 22, -COS3_1, 2);
        bf_pair!(25, 26, COS3_1, 2);
        bf_pair!(29, 30, -COS3_1, 2);

        // pass 5: BF1/BF2 groups.
        // BF1(a,b,c,d): BF(a,b,COS4_0,1); BF(c,d,-COS4_0,1); c += d.
        macro_rules! bf1 {
            ($a:expr, $b:expr, $c:expr, $d:expr) => {{
                let (na, nb) = bf(v[$a], v[$b], COS4_0, 1);
                v[$a] = na;
                v[$b] = nb;
                let (nc, nd) = bf(v[$c], v[$d], -COS4_0, 1);
                v[$c] = nc;
                v[$d] = nd;
                v[$c] += v[$d];
            }};
        }
        // BF2(a,b,c,d): BF1 then a += c; c += b; b += d.
        macro_rules! bf2 {
            ($a:expr, $b:expr, $c:expr, $d:expr) => {{
                let (na, nb) = bf(v[$a], v[$b], COS4_0, 1);
                v[$a] = na;
                v[$b] = nb;
                let (nc, nd) = bf(v[$c], v[$d], -COS4_0, 1);
                v[$c] = nc;
                v[$d] = nd;
                v[$c] += v[$d];
                v[$a] += v[$c];
                v[$c] += v[$b];
                v[$b] += v[$d];
            }};
        }
        bf1!(0, 1, 2, 3);
        bf2!(4, 5, 6, 7);
        bf1!(8, 9, 10, 11);
        bf2!(12, 13, 14, 15);
        bf1!(16, 17, 18, 19);
        bf2!(20, 21, 22, 23);
        bf1!(24, 25, 26, 27);
        bf2!(28, 29, 30, 31);

        // pass 6
        macro_rules! add {
            ($a:expr, $b:expr) => {{
                v[$a] += v[$b];
            }};
        }
        add!(8, 12);
        add!(12, 10);
        add!(10, 14);
        add!(14, 9);
        add!(9, 13);
        add!(13, 11);
        add!(11, 15);

        out[0] = v[0];
        out[16] = v[1];
        out[8] = v[2];
        out[24] = v[3];
        out[4] = v[4];
        out[20] = v[5];
        out[12] = v[6];
        out[28] = v[7];
        out[2] = v[8];
        out[18] = v[9];
        out[10] = v[10];
        out[26] = v[11];
        out[6] = v[12];
        out[22] = v[13];
        out[14] = v[14];
        out[30] = v[15];

        add!(24, 28);
        add!(28, 26);
        add!(26, 30);
        add!(30, 25);
        add!(25, 29);
        add!(29, 27);
        add!(27, 31);

        out[1] = v[16] + v[24];
        out[17] = v[17] + v[25];
        out[9] = v[18] + v[26];
        out[25] = v[19] + v[27];
        out[5] = v[20] + v[28];
        out[21] = v[21] + v[29];
        out[13] = v[22] + v[30];
        out[29] = v[23] + v[31];
        out[3] = v[24] + v[20];
        out[19] = v[25] + v[21];
        out[11] = v[26] + v[22];
        out[27] = v[27] + v[23];
        out[7] = v[28] + v[18];
        out[23] = v[29] + v[19];
        out[15] = v[30] + v[17];
        out[31] = v[31];
    }

    /// `ff_mpadsp_apply_window_float` (mpegaudiodsp_template.c):
    /// 512-entry synthesis window + 32-sample output.
    #[allow(clippy::many_single_char_names)]
    pub fn apply_window_float(
        synth_buf: &mut [f32], // 512 + 32 values
        dither_state: &mut f32,
        samples: &mut [f32], // 32 outputs at stride `incr`
        incr: usize,
    ) {
        let window = &*SYNTH_WINDOW;

        // copy to avoid wrap
        synth_buf.copy_within(0..32, 512);

        #[inline]
        fn sum8(w: &[f32], p: &[f32]) -> f32 {
            let mut s = 0f32;
            for k in 0..8 {
                s += w[k * 64] * p[k * 64];
            }
            s
        }
        #[inline]
        fn mls8(w: &[f32], p: &[f32], s: f32) -> f32 {
            let mut s = s;
            for k in 0..8 {
                s -= w[k * 64] * p[k * 64];
            }
            s
        }

        let mut samples_lo = 0usize; // samples index, step incr
        let mut samples_hi = 31usize * incr; // samples2 index, step -incr

        let w0 = 0usize; // window cursor
        let mut wcur = w0;
        let w2cur = 31usize;

        let mut sum = *dither_state;
        {
            // p = synth_buf + 16
            let p = &synth_buf[16..];
            sum += sum8(&window[wcur..], p);
        }
        {
            let p = &synth_buf[48..];
            sum = mls8(&window[wcur + 32..], p, sum);
        }
        samples[samples_lo] = sum;
        *dither_state = 0.0; // round_sample zeroes the accumulator
        samples_lo += incr;
        wcur += 1;

        for j in 1..16 {
            // sum2 path
            let mut sum2 = 0f32;
            {
                let p = &synth_buf[16 + j..];
                // SUM8P2(sum, MACS, sum2, MLSS, w, w2, p):
                //   sum += w[k]*p[k]; sum2 -= w2[k]*p[k];
                let w = &window[wcur..];
                let w2 = &window[w2cur..];
                for k in 0..8 {
                    let tmp = p[k * 64];
                    sum += w[k * 64] * tmp;
                    sum2 -= w2[k * 64] * tmp;
                }
            }
            {
                let p = &synth_buf[48 - j..];
                let w = &window[wcur + 32..];
                let w2 = &window[w2cur + 32..];
                for k in 0..8 {
                    let tmp = p[k * 64];
                    sum -= w[k * 64] * tmp;
                    sum2 -= w2[k * 64] * tmp;
                }
            }

            samples[samples_lo] = sum;
            *dither_state = 0.0;
            samples_lo += incr;
            sum += sum2;
            samples[samples_hi] = sum;
            if samples_hi >= incr {
                samples_hi -= incr;
            }
            wcur += 1;
        }

        {
            let p = &synth_buf[32..];
            sum = mls8(&window[wcur + 32..], p, sum);
        }
        samples[samples_lo] = sum;
        *dither_state = sum;
    }

    /// `ff_mpa_synth_filter_float`.
    pub fn synth_filter(
        synth_buf: &mut [f32], // 512*2 per channel
        synth_buf_offset: &mut usize,
        dither_state: &mut f32,
        samples: &mut [f32],
        incr: usize,
        _sb_samples: &[f32; 32],
    ) {
        let offset = *synth_buf_offset;
        // dct32(synth_buf + offset, sb_samples)
        let mut tmp = [0f32; 32];
        let mut cur = [0f32; 32];
        for i in 0..32 {
            cur[i] = synth_buf[offset + i];
        }
        dct32(&mut tmp, &cur);
        for i in 0..32 {
            synth_buf[offset + i] = tmp[i];
        }
        // apply window over synth_buf (offset..offset+512+32)
        let mut view = [0f32; 512 + 32];
        for i in 0..512 + 32 {
            view[i] = synth_buf[offset + i];
        }
        apply_window_float(&mut view, dither_state, samples, incr);
        for i in 0..512 + 32 {
            synth_buf[offset + i] = view[i];
        }

        *synth_buf_offset = (offset + 512 - 32) & 511;
    }

    /// The `ff_mpa_synth_filter_float` entry used by qdm2: writes 32
    /// output samples at stride `incr` starting at `samples_offset`.
    pub fn synth_filter_into(
        synth_buf: &mut [f32],
        synth_buf_offset: &mut usize,
        dither_state: &mut f32,
        samples: &mut [f32], // MPA_FRAME_SIZE * channels
        samples_offset: usize,
        incr: usize,
        sb_samples: &[f32; 32],
    ) {
        let mut out = [0f32; 32];
        synth_filter(synth_buf, synth_buf_offset, dither_state, &mut out, incr, sb_samples);
        for (i, v) in out.iter().enumerate() {
            samples[samples_offset + i * incr] = *v;
        }
    }
}

// ═══════════════════════════ RDFT (qdm2's FFT stage) ═══════════════════════════

pub mod rdft {
    //! Real↔half-complex DFT used by QDM2's FFT stage, equivalent to
    //! `av_tx_init(.., AV_TX_FLOAT_RDFT, 1 /*inverse*/, 2*fft_size,
    //! &scale /* 1/2 */, 0)` as called from `qdm2_decode_init`, plus
    //! QDMC's inverse complex FFT `av_tx_init(.., AV_TX_FLOAT_FFT, 1,
    //! 1 << fft_order, &1.0, 0)`.
    //!
    //! A direct O(N²) evaluation of the DFT sum keeps the port small
    //! and dependency-free; N is at most 1024 here and the tone/FFT
    //! stage runs once per subpacket (16 per packet), so the cost is
    //! bounded and dwarfed by correctness. The inverse RDFT applies
    //! the 0.5 scale FFmpeg passes at init.

    #[derive(Clone, Copy, Debug, Default)]
    pub struct Complex {
        pub re: f32,
        pub im: f32,
    }

    /// Inverse real DFT: `N/2+1` complex inputs (Hermitian) → `N`
    /// real outputs, scaled by `scale` (FFmpeg applies its `scale`
    /// argument to the inverse transform).
    pub fn inverse_rdft(input: &[Complex], out: &mut [f32], scale: f32) {
        let n = out.len();
        let half = input.len(); // N/2 + 1
        let two_pi_n = core::f32::consts::TAU / n as f32;
        for k in 0..n {
            let mut acc = 0f32;
            for (m, c) in input.iter().enumerate().take(half) {
                let w = two_pi_n * (k as f32) * (m as f32);
                acc += if m == 0 || m * 2 == n {
                    // DC and Nyquist are real.
                    c.re * w.cos()
                } else {
                    c.re * w.cos() - c.im * w.sin()
                };
            }
            out[k] = acc * scale;
        }
    }

    /// Inverse complex FFT, scaled by `scale`: `out[k] = scale *
    /// Σ_m in[m] · e^{+i·2πmk/N}` (FFmpeg's inverse FFT with a +i
    /// kernel).
    pub fn inverse_fft(input: &[Complex], out: &mut [Complex], scale: f32) {
        let n = input.len();
        let two_pi_n = core::f32::consts::TAU / n as f32;
        for (k, o) in out.iter_mut().enumerate() {
            let mut re = 0f32;
            let mut im = 0f32;
            for (m, c) in input.iter().enumerate() {
                let w = two_pi_n * (k as f32) * (m as f32);
                let (sw, cw) = w.sin_cos();
                // e^{+i w} = cos + i sin
                re += c.re * cw - c.im * sw;
                im += c.re * sw + c.im * cw;
            }
            o.re = re * scale;
            o.im = im * scale;
        }
    }
}

// ═══════════════════════════ decoder context ═══════════════════════════

/// QDM2SubPacket (qdm2.c).
#[derive(Clone, Default)]
struct SubPacket {
    type_: i32,
    size: usize,
    /// Offset of the payload start inside the compressed superblock
    /// buffer (FFmpeg stores a pointer; we store the offset).
    offset: usize,
}

#[derive(Clone)]
struct FftTone {
    level: f32,
    /// Index into the decoder's `fft.complex[ch]` array for tone
    /// accumulation (FFmpeg stores a raw pointer).
    complex_base: (usize, usize),
    table: [f32; 5],
    phase: i32,
    phase_shift: i32,
    duration: usize,
    time_index: i32,
    cutoff: usize,
}

#[derive(Clone, Copy, Default)]
struct FftCoefficient {
    sub_packet: i32,
    channel: u8,
    offset: i32,
    exp: i32,
    phase: u8,
}

const FFT_TONES_CAP: usize = 1000;
const FFT_COEFS_CAP: usize = 1000;

/// QDM2Context (qdm2.c), Rust port. Buffer sizes are fixed by
/// validated extradata.
struct Qdm2Context {
    // Parameters from codec header.
    nb_channels: usize,
    channels: usize,
    group_size: usize,
    fft_size: usize,
    checksum_size: usize,

    // Parameters built from header parameters.
    group_order: i32,
    fft_order: i32,
    frame_size: usize,
    frequency_range: i32,
    sub_sampling: i32,
    coeff_per_sb_select: usize,
    cm_table_select: i32,

    // Packets and packet lists.
    sub_packets: Vec<SubPacket>,
    /// list A: (packet index into sub_packets).
    sub_packet_list_a: Vec<Option<usize>>,
    sub_packet_list_b: Vec<Option<usize>>,
    sub_packets_b: usize,
    sub_packet_list_d: Vec<Option<usize>>,

    // FFT and tones.
    fft_tones: Vec<FftTone>,
    fft_tone_start: usize,
    fft_tone_end: usize,
    fft_coefs: Vec<FftCoefficient>,
    fft_coefs_index: usize,
    fft_coefs_min_index: [i32; 5],
    fft_coefs_max_index: [i32; 5],
    fft_level_exp: [i32; 6],

    // I/O data.
    compressed: Vec<u8>,
    output_buffer: Vec<f32>,

    // Synthesis filter state.
    synth_buf: [[f32; 512 * 2]; MPA_MAX_CHANNELS],
    synth_buf_offset: [usize; MPA_MAX_CHANNELS],
    sb_samples: [[[f32; SBLIMIT]; 128]; MPA_MAX_CHANNELS],
    samples: Vec<f32>,

    // Mixed temporary data.
    tone_level: [[[f32; 64]; 30]; MPA_MAX_CHANNELS],
    coding_method: [[[i8; 64]; 30]; MPA_MAX_CHANNELS],
    quantized_coeffs: [[[i8; 8]; 10]; MPA_MAX_CHANNELS],
    tone_level_idx_base: [[[i8; 8]; 30]; MPA_MAX_CHANNELS],
    tone_level_idx_hi1: [[[[i8; 8]; 8]; 3]; MPA_MAX_CHANNELS],
    tone_level_idx_mid: [[i8; 8]; 26],
    tone_level_idx_hi2: [i8; 26],
    tone_level_idx: [[[i8; 64]; 30]; MPA_MAX_CHANNELS],
    tone_level_idx_temp: [[[i8; 64]; 30]; MPA_MAX_CHANNELS],

    // Flags.
    has_errors: bool,
    superblocktype_2_3: bool,
    do_synth_filter: bool,

    sub_packet: usize,
    noise_idx: usize,

    // FFT working buffers.
    fft_complex: [[Complex2; 257]; MPA_MAX_CHANNELS],
    fft_temp: [[Complex2; 256]; MPA_MAX_CHANNELS],
}

#[derive(Clone, Copy, Default)]
struct Complex2 {
    re: f32,
    im: f32,
}

impl Default for Qdm2Context {
    fn default() -> Self {
        // Only used through `new`; sizes are filled by `decode_init`.
        Self {
            nb_channels: 0,
            channels: 0,
            group_size: 0,
            fft_size: 0,
            checksum_size: 0,
            group_order: 0,
            fft_order: 0,
            frame_size: 0,
            frequency_range: 0,
            sub_sampling: 0,
            coeff_per_sb_select: 0,
            cm_table_select: 0,
            sub_packets: Vec::new(),
            sub_packet_list_a: Vec::new(),
            sub_packet_list_b: Vec::new(),
            sub_packets_b: 0,
            sub_packet_list_d: Vec::new(),
            fft_tones: Vec::new(),
            fft_tone_start: 0,
            fft_tone_end: 0,
            fft_coefs: Vec::new(),
            fft_coefs_index: 0,
            fft_coefs_min_index: [-1; 5],
            fft_coefs_max_index: [-1; 5],
            fft_level_exp: [0; 6],
            compressed: Vec::new(),
            output_buffer: Vec::new(),
            synth_buf: [[0f32; 1024]; MPA_MAX_CHANNELS],
            synth_buf_offset: [0; MPA_MAX_CHANNELS],
            sb_samples: [[[0f32; SBLIMIT]; 128]; MPA_MAX_CHANNELS],
            samples: Vec::new(),
            tone_level: [[[0f32; 64]; 30]; MPA_MAX_CHANNELS],
            coding_method: [[[0i8; 64]; 30]; MPA_MAX_CHANNELS],
            quantized_coeffs: [[[0i8; 8]; 10]; MPA_MAX_CHANNELS],
            tone_level_idx_base: [[[0i8; 8]; 30]; MPA_MAX_CHANNELS],
            tone_level_idx_hi1: [[[[0i8; 8]; 8]; 3]; MPA_MAX_CHANNELS],
            tone_level_idx_mid: [[0i8; 8]; 26],
            tone_level_idx_hi2: [0i8; 26],
            tone_level_idx: [[[0i8; 64]; 30]; MPA_MAX_CHANNELS],
            tone_level_idx_temp: [[[0i8; 64]; 30]; MPA_MAX_CHANNELS],
            has_errors: false,
            superblocktype_2_3: false,
            do_synth_filter: false,
            sub_packet: 0,
            noise_idx: 0,
            fft_complex: [[Complex2::default(); 257]; MPA_MAX_CHANNELS],
            fft_temp: [[Complex2::default(); 256]; MPA_MAX_CHANNELS],
        }
    }
}

/// The switchtable (qdm2.c).
static SWITCHTABLE: [u8; 23] = [
    0, 5, 1, 5, 5, 5, 5, 5, 2, 5, 5, 5, 5, 5, 5, 5, 3, 5, 5, 5, 5, 5, 4,
];

/// qdm2_get_vlc: value from `vlc`; on a decode miss the C code reads
/// an escape: `get_bits(get_bits(3) + 1)`. With `flag`, stage-3 adds
/// `vlc_stage3_values[value]` plus raw bits.
fn qdm2_get_vlc(br: &mut LeBitReader, vlc: &VlcTable, flag: bool) -> Option<i32> {
    let mut value = match vlc.decode(br, u32::MAX) {
        Some((v, _len)) => v,
        None => {
            // Escape: 3-bit length-1 then that many bits.
            let n = br.read(3)? as u32;
            br.read(n + 1)? as i32
        }
    };

    if flag {
        if value >= 60 {
            // FFmpeg errors and returns 0.
            return Some(0);
        }
        let mut tmp = VLC_STAGE3_VALUES[value as usize];
        if (value & !3) > 0 {
            tmp += br.read((value >> 2) as u32)? as i32;
        }
        value = tmp;
    }
    Some(value)
}

fn qdm2_get_se_vlc(vlc: &VlcTable, br: &mut LeBitReader) -> Option<i32> {
    let value = qdm2_get_vlc(br, vlc, false)?;
    Some(if value & 1 != 0 {
        (value + 1) >> 1
    } else {
        -(value >> 1)
    })
}

/// qdm2_packet_checksum.
fn qdm2_packet_checksum(data: &[u8], length: usize, mut value: i32) -> u16 {
    for &b in data.iter().take(length) {
        value -= b as i32;
    }
    (value & 0xffff) as u16
}

/// qdm2_decode_sub_packet_header: reads the type/size header at the
/// current reader position; returns the packet and the payload bit
/// offset within `gb`'s source buffer.
fn qdm2_decode_sub_packet_header(br: &mut LeBitReader) -> Option<(SubPacket, usize)> {
    let mut sp = SubPacket::default();
    sp.type_ = br.read(8)? as i32;

    if sp.type_ == 0 {
        sp.size = 0;
        sp.offset = usize::MAX;
    } else {
        sp.size = br.read(8)? as usize;

        if sp.type_ & 0x80 != 0 {
            sp.size <<= 8;
            sp.size |= br.read(8)? as usize;
            sp.type_ &= 0x7f;
        }

        if sp.type_ == 0x7f {
            sp.type_ |= (br.read(8)? as i32) << 8;
        }

        sp.offset = br.byte_cursor();
    }
    let off = sp.offset;
    Some((sp, off))
}

impl Qdm2Context {
    /// average_quantized_coeffs (qdm2.c).
    fn average_quantized_coeffs(&mut self) {
        let n = COEFF_PER_SB_FOR_AVG[self.coeff_per_sb_select]
            [qdm2_sb_used(self.sub_sampling) - 1] as usize
            + 1;
        for ch in 0..self.nb_channels {
            for i in 0..n {
                let mut sum = 0i32;
                for j in 0..8 {
                    sum += self.quantized_coeffs[ch][i][j] as i32;
                }
                sum /= 8;
                if sum > 0 {
                    sum -= 1;
                }
                for j in 0..8 {
                    self.quantized_coeffs[ch][i][j] = sum as i8;
                }
            }
        }
    }

    /// build_sb_samples_from_noise.
    fn build_sb_samples_from_noise(&mut self, sb: usize) -> Result<()> {
        fix_noise_idx(&mut self.noise_idx);
        if self.nb_channels == 0 {
            return Err(Error::invalid("qdm2: no channels"));
        }
        for ch in 0..self.nb_channels {
            for j in 0..64 {
                let noise = sb_dithering_noise(sb, &mut self.noise_idx);
                let lvl = self.tone_level[ch][sb][j];
                self.sb_samples[ch][j * 2][sb] = noise * lvl;
                self.sb_samples[ch][j * 2 + 1][sb] = noise * lvl;
            }
        }
        Ok(())
    }

    /// fix_coding_method_array (qdm2.c).
    fn fix_coding_method_array(&mut self, sb: usize) -> bool {
        let channels = self.nb_channels;
        for ch in 0..channels {
            let mut j = 0usize;
            while j < 64 {
                if self.coding_method[ch][sb][j] < 8 {
                    return false;
                }
                let (run, case_val) = if (self.coding_method[ch][sb][j] as i32 - 8) > 22 {
                    (1usize, 8i8)
                } else {
                    match SWITCHTABLE[(self.coding_method[ch][sb][j] - 8) as usize] {
                        0 => (10, 10),
                        1 => (1, 16),
                        2 => (5, 24),
                        3 => (3, 30),
                        4 => (1, 30),
                        5 => (1, 8),
                        _ => (1, 8),
                    }
                };
                for k in 0..run {
                    if j + k < 128 {
                        let sbjk = sb + (j + k) / 64;
                        if sbjk > 29 {
                            // SAMPLES_NEEDED: continue.
                            continue;
                        }
                        if self.coding_method[ch][sbjk][(j + k) % 64]
                            > self.coding_method[ch][sb][j]
                        {
                            if k > 0 {
                                // FFmpeg memsets k then 3 elements
                                // (an upstream bug preserved for
                                // output equivalence).
                                let cv = case_val;
                                for slot in &mut self.coding_method[ch][sb]
                                    [j + k..(j + k + k).min(64)]
                                {
                                    *slot = cv;
                                }
                                for slot in &mut self.coding_method[ch][sb]
                                    [j + k..(j + k + 3).min(64)]
                                {
                                    *slot = cv;
                                }
                            }
                        }
                    }
                }
                j += run;
            }
        }
        true
    }

    /// fill_tone_level_array (qdm2.c).
    fn fill_tone_level_array(&mut self, flag: bool) {
        for ch in 0..self.nb_channels {
            for sb in 0..30 {
                for i in 0..8 {
                    let tab = COEFF_PER_SB_FOR_DEQUANT[self.coeff_per_sb_select][sb] as usize;
                    let tmp: i32 =
                        if tab < LAST_COEFF[self.coeff_per_sb_select].saturating_sub(1) {
                            self.quantized_coeffs[ch][tab + 1][i] as i32
                                * DEQUANT_TABLE[self.coeff_per_sb_select][tab + 1][sb] as i32
                                + self.quantized_coeffs[ch][tab][i] as i32
                                    * DEQUANT_TABLE[self.coeff_per_sb_select][tab][sb] as i32
                        } else {
                            self.quantized_coeffs[ch][tab][i] as i32
                                * DEQUANT_TABLE[self.coeff_per_sb_select][tab][sb] as i32
                        };
                    let mut tmp = tmp;
                    if tmp < 0 {
                        tmp += 0xff;
                    }
                    self.tone_level_idx_base[ch][sb][i] = ((tmp / 256) & 0xff) as i8;
                }
            }
        }

        let sb_used = qdm2_sb_used(self.sub_sampling);

        if self.superblocktype_2_3 && !flag {
            for sb in 0..sb_used {
                for ch in 0..self.nb_channels {
                    for i in 0..64 {
                        self.tone_level_idx[ch][sb][i] =
                            self.tone_level_idx_base[ch][sb][i / 8];
                        if self.tone_level_idx[ch][sb][i] < 0 {
                            self.tone_level[ch][sb][i] = 0.0;
                        } else {
                            self.tone_level[ch][sb][i] = FFT_TONE_LEVEL_TABLE[0]
                                [(self.tone_level_idx[ch][sb][i] as u8 as i32 & 0x3f) as usize];
                        }
                    }
                }
            }
        } else {
            let tab = if self.superblocktype_2_3 { 0usize } else { 1usize };
            for sb in 0..sb_used {
                if (4..=23).contains(&sb) {
                    for ch in 0..self.nb_channels {
                        for i in 0..64 {
                            let tmp = self.tone_level_idx_base[ch][sb][i / 8] as i32
                                - self.tone_level_idx_hi1[ch][sb / 8][i / 8][i % 8] as i32
                                - self.tone_level_idx_mid[sb - 4][i / 8] as i32
                                - self.tone_level_idx_hi2[sb - 4] as i32;
                            self.tone_level_idx[ch][sb][i] = (tmp & 0xff) as i8;
                            if tmp < 0 || (!self.superblocktype_2_3 && tmp == 0) {
                                self.tone_level[ch][sb][i] = 0.0;
                            } else {
                                self.tone_level[ch][sb][i] = FFT_TONE_LEVEL_TABLE
                                    [tab][(tmp as u8 as i32 & 0x3f) as usize];
                            }
                        }
                    }
                } else if sb > 4 {
                    for ch in 0..self.nb_channels {
                        for i in 0..64 {
                            let tmp = self.tone_level_idx_base[ch][sb][i / 8] as i32
                                - self.tone_level_idx_hi1[ch][2][i / 8][i % 8] as i32
                                - self.tone_level_idx_hi2[sb - 4] as i32;
                            self.tone_level_idx[ch][sb][i] = (tmp & 0xff) as i8;
                            if tmp < 0 || (!self.superblocktype_2_3 && tmp == 0) {
                                self.tone_level[ch][sb][i] = 0.0;
                            } else {
                                self.tone_level[ch][sb][i] = FFT_TONE_LEVEL_TABLE
                                    [tab][(tmp as u8 as i32 & 0x3f) as usize];
                            }
                        }
                    }
                } else {
                    for ch in 0..self.nb_channels {
                        for i in 0..64 {
                            self.tone_level_idx[ch][sb][i] =
                                self.tone_level_idx_base[ch][sb][i / 8];
                            let tmp = self.tone_level_idx[ch][sb][i] as i32;
                            if tmp < 0 || (!self.superblocktype_2_3 && tmp == 0) {
                                self.tone_level[ch][sb][i] = 0.0;
                            } else {
                                self.tone_level[ch][sb][i] = FFT_TONE_LEVEL_TABLE
                                    [tab][(tmp as u8 as i32 & 0x3f) as usize];
                            }
                        }
                    }
                }
            }
        }
    }

    /// fill_coding_method_array (qdm2.c): only the
    /// `superblocktype_2_3` branch is reachable (the other path is
    /// an FFmpeg-internal `avpriv_request_sample` error).
    fn fill_coding_method_array(&mut self, _c: i32) -> Result<()> {
        if !self.superblocktype_2_3 {
            return Err(Error::unsupported("qdm2: !superblocktype_2_3"));
        }
        for ch in 0..self.nb_channels {
            for sb in 0..30 {
                for j in 0..64 {
                    self.coding_method[ch][sb][j] =
                        CODING_METHOD_TABLE[self.cm_table_select as usize][sb];
                }
            }
        }
        Ok(())
    }

    /// synthfilt_build_sb_samples (qdm2.c).
    fn synthfilt_build_sb_samples(
        &mut self,
        br: &mut LeBitReader,
        length: usize,
        sb_min: usize,
        sb_max: usize,
    ) -> Result<()> {
        if length == 0 {
            for sb in sb_min..sb_max {
                self.build_sb_samples_from_noise(sb)?;
            }
            return Ok(());
        }

        for sb in sb_min..sb_max {
            let mut channels = self.nb_channels;

            let joined_stereo = if self.nb_channels <= 1 || sb < 12 {
                false
            } else if sb >= 24 {
                true
            } else if br.bits_left() >= 1 {
                br.read_bit().unwrap_or(false)
            } else {
                false
            };

            let mut sign_bits = [false; 16];

            if joined_stereo {
                if br.bits_left() >= 16 {
                    for b in sign_bits.iter_mut().take(16) {
                        *b = br.read_bit().unwrap_or(false);
                    }
                }

                for j in 0..64 {
                    if self.coding_method[1][sb][j] > self.coding_method[0][sb][j] {
                        self.coding_method[0][sb][j] = self.coding_method[1][sb][j];
                    }
                }

                if !self.fix_coding_method_array(sb) {
                    self.build_sb_samples_from_noise(sb)?;
                    continue;
                }
                channels = 1;
            }

            for ch in 0..channels {
                fix_noise_idx(&mut self.noise_idx);
                let zero_encoding = if br.bits_left() >= 1 {
                    br.read_bit().unwrap_or(false)
                } else {
                    false
                };
                let mut type34_predictor = 0f32;
                let mut type34_first = true;
                let mut type34_div = 0f32;
                let mut samples = [0f32; 10];

                let mut j = 0usize;
                while j < 128 {
                    let run: usize = match self.coding_method[ch][sb][j / 2] {
                        8 => {
                            if br.bits_left() >= 10 {
                                if zero_encoding {
                                    for k in 0..5 {
                                        if j + 2 * k >= 128 {
                                            break;
                                        }
                                        samples[2 * k] = if br.read_bit().unwrap_or(false) {
                                            DEQUANT_1BIT[joined_stereo as usize]
                                                [2 * br.read_bit().unwrap_or(false) as usize]
                                        } else {
                                            0.0
                                        };
                                    }
                                } else {
                                    let n = br.read(8).unwrap_or(255);
                                    if n >= 243 {
                                        return Err(Error::invalid(
                                            "qdm2: invalid 8bit codeword",
                                        ));
                                    }
                                    for k in 0..5 {
                                        samples[2 * k] = DEQUANT_1BIT[joined_stereo as usize]
                                            [RANDOM_DEQUANT_INDEX[n as usize][k] as usize];
                                    }
                                }
                                for k in 0..5 {
                                    samples[2 * k + 1] =
                                        sb_dithering_noise(sb, &mut self.noise_idx);
                                }
                            } else {
                                for k in 0..10 {
                                    samples[k] = sb_dithering_noise(sb, &mut self.noise_idx);
                                }
                            }
                            10
                        }
                        10 => {
                            if br.bits_left() >= 1 {
                                let mut f = 0.81f32;
                                if br.read_bit().unwrap_or(false) {
                                    f = -f;
                                }
                                f -= NOISE_SAMPLES
                                    [((sb + 1) * (j + 5 * ch + 1)) & 127]
                                    * 9.0
                                    / 40.0;
                                samples[0] = f;
                            } else {
                                samples[0] = sb_dithering_noise(sb, &mut self.noise_idx);
                            }
                            1
                        }
                        16 => {
                            if br.bits_left() >= 10 {
                                if zero_encoding {
                                    for k in 0..5 {
                                        if j + k >= 128 {
                                            break;
                                        }
                                        samples[k] = if !br.read_bit().unwrap_or(true) {
                                            0.0
                                        } else {
                                            DEQUANT_1BIT[joined_stereo as usize]
                                                [2 * br.read_bit().unwrap_or(false) as usize]
                                        };
                                    }
                                } else {
                                    let n = br.read(8).unwrap_or(255);
                                    if n >= 243 {
                                        return Err(Error::invalid(
                                            "qdm2: invalid 8bit codeword",
                                        ));
                                    }
                                    for k in 0..5 {
                                        samples[k] = DEQUANT_1BIT[joined_stereo as usize]
                                            [RANDOM_DEQUANT_INDEX[n as usize][k] as usize];
                                    }
                                }
                            } else {
                                for k in 0..5 {
                                    samples[k] = sb_dithering_noise(sb, &mut self.noise_idx);
                                }
                            }
                            5
                        }
                        24 => {
                            if br.bits_left() >= 7 {
                                let n = br.read(7).unwrap_or(127);
                                if n >= 125 {
                                    return Err(Error::invalid(
                                        "qdm2: invalid 7bit codeword",
                                    ));
                                }
                                for k in 0..3 {
                                    samples[k] = (RANDOM_DEQUANT_TYPE24[n as usize][k] as f32
                                        - 2.0)
                                        * 0.5;
                                }
                            } else {
                                for k in 0..3 {
                                    samples[k] = sb_dithering_noise(sb, &mut self.noise_idx);
                                }
                            }
                            3
                        }
                        30 => {
                            if br.bits_left() >= 4 {
                                let index = qdm2_get_vlc(br, &VLC.tab_type30, false)
                                    .unwrap_or(0) as usize;
                                if index >= TYPE30_DEQUANT.len() {
                                    return Err(Error::invalid(
                                        "qdm2: index out of type30_dequant",
                                    ));
                                }
                                samples[0] = TYPE30_DEQUANT[index];
                            } else {
                                samples[0] = sb_dithering_noise(sb, &mut self.noise_idx);
                            }
                            1
                        }
                        34 => {
                            if br.bits_left() >= 7 {
                                if type34_first {
                                    type34_div = (1u32 << br.read(2).unwrap_or(0)) as f32;
                                    samples[0] =
                                        (br.read(5).unwrap_or(16) as f32 - 16.0) / 15.0;
                                    type34_predictor = samples[0];
                                    type34_first = false;
                                } else {
                                    let index = qdm2_get_vlc(br, &VLC.tab_type34, false)
                                        .unwrap_or(0) as usize;
                                    if index >= TYPE34_DELTA.len() {
                                        return Err(Error::invalid(
                                            "qdm2: index out of type34_delta",
                                        ));
                                    }
                                    samples[0] =
                                        TYPE34_DELTA[index] / type34_div + type34_predictor;
                                    type34_predictor = samples[0];
                                }
                            } else {
                                samples[0] = sb_dithering_noise(sb, &mut self.noise_idx);
                            }
                            1
                        }
                        _ => {
                            samples[0] = sb_dithering_noise(sb, &mut self.noise_idx);
                            1
                        }
                    };

                    if joined_stereo {
                        for k in 0..run {
                            if j + k >= 128 {
                                break;
                            }
                            self.sb_samples[0][j + k][sb] =
                                self.tone_level[0][sb][(j + k) / 2] * samples[k];
                            if self.nb_channels == 2 {
                                let sign = if sign_bits[(j + k) / 8] { -1.0 } else { 1.0 };
                                self.sb_samples[1][j + k][sb] =
                                    self.tone_level[1][sb][(j + k) / 2] * samples[k] * sign;
                            }
                        }
                    } else {
                        for k in 0..run {
                            if j + k < 128 {
                                self.sb_samples[ch][j + k][sb] =
                                    self.tone_level[ch][sb][(j + k) / 2] * samples[k];
                            }
                        }
                    }

                    j += run;
                }
            }
        }
        Ok(())
    }

    /// init_quantized_coeffs_elem0 (qdm2.c).
    fn init_quantized_coeffs_elem0(
        &mut self,
        quantized_coeffs: &mut [i8; 8],
        br: &mut LeBitReader,
    ) -> Result<()> {
        if br.bits_left() < 16 {
            return Err(Error::invalid("qdm2: truncated coeffs elem0"));
        }
        let mut level = qdm2_get_vlc(br, &VLC.tab_level, false)
            .ok_or_else(|| Error::invalid("qdm2: vlc"))?;
        quantized_coeffs[0] = level as i8;

        let mut i = 0usize;
        while i < 7 {
            if br.bits_left() < 16 {
                return Err(Error::invalid("qdm2: truncated coeffs elem0"));
            }
            let run =
                qdm2_get_vlc(br, &VLC.tab_run, false).ok_or_else(|| Error::invalid("qdm2: vlc"))?
                    as usize
                    + 1;
            if i + run >= 8 {
                return Err(Error::invalid("qdm2: run overflow"));
            }
            if br.bits_left() < 16 {
                return Err(Error::invalid("qdm2: truncated coeffs elem0"));
            }
            let diff = qdm2_get_se_vlc(&VLC.tab_diff, br)
                .ok_or_else(|| Error::invalid("qdm2: vlc"))?;

            for k in 1..=run {
                quantized_coeffs[i + k] = (level + ((k as i32 * diff) / run as i32)) as i8;
            }
            level += diff;
            i += run;
        }
        Ok(())
    }
}


impl Qdm2Context {
    /// init_tone_level_dequantization (qdm2.c).
    fn init_tone_level_dequantization(&mut self, br: &mut LeBitReader) -> Result<()> {
        for ch in 0..self.nb_channels {
            let r = {
                let mut qc = self.quantized_coeffs[ch][0];
                let r = self.init_quantized_coeffs_elem0(&mut qc, br);
                self.quantized_coeffs[ch][0] = qc;
                r
            };
            r?;

            if br.bits_left() < 16 {
                self.quantized_coeffs[ch][0] = [0i8; 8];
                break;
            }
        }

        let n = (self.sub_sampling + 1) as usize;
        'outer1: for sb in 0..n.min(3) {
            for ch in 0..self.nb_channels {
                for j in 0..8 {
                    if br.bits_left() < 1 {
                        break 'outer1;
                    }
                    if br.read_bit().unwrap_or(false) {
                        for k in 0..8 {
                            if br.bits_left() < 16 {
                                break 'outer1;
                            }
                            self.tone_level_idx_hi1[ch][sb][j][k] =
                                qdm2_get_vlc(br, &VLC.tab_tone_level_idx_hi1, false)
                                    .ok_or_else(|| Error::invalid("qdm2: vlc"))?
                                    as i8;
                        }
                    } else {
                        for k in 0..8 {
                            self.tone_level_idx_hi1[ch][sb][j][k] = 0;
                        }
                    }
                }
            }
        }

        let n = qdm2_sb_used(self.sub_sampling).saturating_sub(4);
        for sb in 0..n.min(26) {
            for _ch in 0..self.nb_channels {
                if br.bits_left() < 16 {
                    break;
                }
                let mut v = qdm2_get_vlc(br, &VLC.tab_tone_level_idx_hi2, false)
                    .ok_or_else(|| Error::invalid("qdm2: vlc"))?;
                if sb > 19 {
                    v -= 16;
                } else {
                    for j in 0..8 {
                        self.tone_level_idx_mid[sb][j] = -16;
                    }
                }
                self.tone_level_idx_hi2[sb] = v as i8;
            }
        }

        let n = qdm2_sb_used(self.sub_sampling).saturating_sub(5);
        for sb in 0..n.min(26) {
            for _ch in 0..self.nb_channels {
                for j in 0..8 {
                    if br.bits_left() < 16 {
                        break;
                    }
                    self.tone_level_idx_mid[sb][j] =
                        (qdm2_get_vlc(br, &VLC.tab_tone_level_idx_mid, false)
                            .ok_or_else(|| Error::invalid("qdm2: vlc"))?
                            - 32) as i8;
                }
            }
        }

        Ok(())
    }

    /// process_subpacket_9 (qdm2.c).
    fn process_subpacket_9(&mut self, sp: &SubPacket) -> Result<()> {
        let data: Vec<u8> = self.compressed[sp.offset..sp.offset + sp.size].to_vec();
        let mut br = LeBitReader::new(&data);

        let n = COEFF_PER_SB_FOR_AVG[self.coeff_per_sb_select]
            [qdm2_sb_used(self.sub_sampling) - 1] as usize
            + 1;

        for i in 1..n {
            for ch in 0..self.nb_channels {
                let mut level = qdm2_get_vlc(&mut br, &VLC.tab_level, false)
                    .ok_or_else(|| Error::invalid("qdm2: vlc"))?;
                self.quantized_coeffs[ch][i][0] = level as i8;

                let mut j = 0usize;
                while j < 8 - 1 {
                    let run = qdm2_get_vlc(&mut br, &VLC.tab_run, false)
                        .ok_or_else(|| Error::invalid("qdm2: vlc"))?
                        as usize
                        + 1;
                    let diff = qdm2_get_se_vlc(&VLC.tab_diff, &mut br)
                        .ok_or_else(|| Error::invalid("qdm2: vlc"))?;

                    if j + run >= 8 {
                        return Err(Error::invalid("qdm2: run overflow in sp9"));
                    }

                    for k in 1..=run {
                        self.quantized_coeffs[ch][i][j + k] =
                            (level + ((k as i32 * diff) / run as i32)) as i8;
                    }
                    level += diff;
                    j += run;
                }
            }
        }

        for ch in 0..self.nb_channels {
            for i in 0..8 {
                self.quantized_coeffs[ch][0][i] = 0;
            }
        }

        Ok(())
    }

    /// process_subpacket_10 (qdm2.c).
    fn process_subpacket_10(&mut self, sp: Option<&SubPacket>) -> Result<()> {
        let data: Vec<u8> = sp
            .map(|sp| self.compressed[sp.offset..sp.offset + sp.size].to_vec())
            .unwrap_or_default();
        if let Some(_sp) = sp {
            let mut br = LeBitReader::new(&data);
            self.init_tone_level_dequantization(&mut br)?;
            self.fill_tone_level_array(true);
        } else {
            self.fill_tone_level_array(false);
        }
        Ok(())
    }

    /// process_subpacket_11 (qdm2.c).
    fn process_subpacket_11(&mut self, sp: Option<&SubPacket>) -> Result<()> {
        let mut length = 0usize;
        let data: Vec<u8> = sp
            .map(|sp| self.compressed[sp.offset..sp.offset + sp.size].to_vec())
            .unwrap_or_default();
        if let Some(_) = sp {
            length = data.len();
        }
        let mut br = LeBitReader::new(&data);

        if length >= 32 {
            let c = br.read(13).ok_or_else(|| Error::invalid("qdm2: truncated sp11"))? as i32;
            if c > 3 {
                self.fill_coding_method_array(8 * c)?;
            }
        }

        self.synthfilt_build_sb_samples(&mut br, length, 0, 8)
    }

    /// process_subpacket_12 (qdm2.c).
    fn process_subpacket_12(&mut self, sp: Option<&SubPacket>) -> Result<()> {
        let mut length = 0usize;
        let data: Vec<u8> = sp
            .map(|sp| self.compressed[sp.offset..sp.offset + sp.size].to_vec())
            .unwrap_or_default();
        if let Some(_) = sp {
            length = data.len();
        }
        let mut br = LeBitReader::new(&data);

        self.synthfilt_build_sb_samples(
            &mut br,
            length,
            8,
            qdm2_sb_used(self.sub_sampling),
        )
    }

    /// process_synthesis_subpackets (qdm2.c).
    fn process_synthesis_subpackets(&mut self) -> Result<()> {
        let find = |list: &[Option<usize>], type_: i32| -> Option<usize> {
            for node in list {
                match node {
                    Some(idx) => {
                        // The packet table is indexed alongside the
                        // list; type comparison happens in the caller
                        // through sp_types.
                        return Some(*idx);
                    }
                    None => break,
                }
            }
            None
        };
        let _ = find;

        // Resolve subpackets 9..12 by type.
        let mut nodes: [Option<usize>; 4] = [None; 4];
        for (i, node) in self.sub_packet_list_d.iter().enumerate() {
            if let Some(idx) = node {
                if i >= 16 {
                    break;
                }
                let t = self.sub_packets[*idx].type_;
                if (9..=12).contains(&t) {
                    nodes[(t - 9) as usize] = Some(*idx);
                }
            } else {
                break;
            }
        }

        // packet 9
        if let Some(n0) = nodes[0] {
            let sp = self.sub_packets[n0].clone();
            self.process_subpacket_9(&sp)?;
        } else {
            return Ok(());
        }

        // packet 10
        let sp10 = nodes[1].map(|idx| self.sub_packets[idx].clone());
        self.process_subpacket_10(sp10.as_ref())?;

        // packet 11
        if nodes[0].is_some() && nodes[1].is_some() && nodes[2].is_some() {
            let sp = self.sub_packets[nodes[2].unwrap()].clone();
            self.process_subpacket_11(Some(&sp))?;
        } else {
            self.process_subpacket_11(None)?;
        }

        // packet 12
        if nodes[0].is_some() && nodes[1].is_some() && nodes[3].is_some() {
            let sp = self.sub_packets[nodes[3].unwrap()].clone();
            self.process_subpacket_12(Some(&sp))?;
        } else {
            self.process_subpacket_12(None)?;
        }

        Ok(())
    }

    /// qdm2_decode_super_block (qdm2.c).
    fn decode_super_block(&mut self) -> Result<()> {
        self.tone_level_idx_hi1 = [[[[0i8; 8]; 8]; 3]; MPA_MAX_CHANNELS];
        self.tone_level_idx_mid = [[0i8; 8]; 26];
        self.tone_level_idx_hi2 = [0i8; 26];

        self.sub_packets_b = 0;

        self.average_quantized_coeffs();

        let compressed_size = self.checksum_size;
        let mut br = LeBitReader::new(&self.compressed[..compressed_size]);

        let (header, _hdr_off) =
            qdm2_decode_sub_packet_header(&mut br).ok_or_else(|| Error::invalid("qdm2: header"))?;

        if !(2..8).contains(&header.type_) {
            self.has_errors = true;
            return Err(Error::invalid("qdm2: bad superblock type"));
        }

        self.superblocktype_2_3 = header.type_ == 2 || header.type_ == 3;
        let packet_bytes: i32 = compressed_size as i32 - br.bits_consumed() as i32 / 8;

        // Re-init over the header payload.
        let header_data: Vec<u8> = if header.offset == usize::MAX {
            Vec::new()
        } else {
            self.compressed[header.offset..header.offset + header.size].to_vec()
        };
        let mut hbr = LeBitReader::new(&header_data);

        if header.type_ == 2 || header.type_ == 4 || header.type_ == 5 {
            let mut csum = 257 * hbr.read(8).ok_or_else(|| Error::invalid("qdm2: csum"))? as i32;
            csum += 2 * hbr.read(8).ok_or_else(|| Error::invalid("qdm2: csum"))? as i32;
            let csum = qdm2_packet_checksum(&self.compressed, self.checksum_size, csum);
            if csum != 0 {
                self.has_errors = true;
                return Err(Error::invalid("qdm2: bad packet checksum"));
            }
        }

        self.sub_packet_list_b.clear();
        self.sub_packet_list_d.clear();

        for exp in self.fft_level_exp.iter_mut().take(6) {
            *exp -= 1;
            if *exp < 0 {
                *exp = 0;
            }
        }

        let mut next_index: usize = 0;
        let mut packet_bytes = packet_bytes;
        let mut i = 0usize;
        while packet_bytes > 0 {
            if i >= 16 {
                // SAMPLES_NEEDED_2("too many packet bytes")
                return Err(Error::unsupported("qdm2: too many packet bytes"));
            }

            if i > 0 {
                // seek to next block
                hbr = LeBitReader::new(&header_data);
                hbr.skip(next_index * 8);
                if next_index >= header_data.len() {
                    break;
                }
            }

            // decode subpacket
            let (packet, _off) = qdm2_decode_sub_packet_header(&mut hbr)
                .ok_or_else(|| Error::invalid("qdm2: subpacket header"))?;
            next_index = packet.size + hbr.bits_consumed() / 8;
            let sub_packet_size =
                (if packet.size > 0xff { 1 } else { 0 }) + packet.size + 2;

            if packet.type_ == 0 {
                break;
            }

            let mut packet = packet;
            if sub_packet_size as i32 > packet_bytes {
                if packet.type_ != 10 && packet.type_ != 11 && packet.type_ != 12 {
                    break;
                }
                packet.size = (packet.size as i32 + packet_bytes - sub_packet_size as i32) as usize;
            }

            packet_bytes -= sub_packet_size as i32;

            // Clamp the payload to the buffer.
            if packet.offset != usize::MAX
                && packet.offset + packet.size > self.compressed.len()
            {
                return Err(Error::invalid("qdm2: subpacket overruns buffer"));
            }

            let idx = i;
            if self.sub_packets.len() <= idx {
                self.sub_packets.push(packet.clone());
            } else {
                self.sub_packets[idx] = packet.clone();
            }

            // add subpacket to 'all subpackets' list
            while self.sub_packet_list_a.len() <= i {
                self.sub_packet_list_a.push(None);
            }
            self.sub_packet_list_a[i] = Some(idx);

            if packet.type_ == 8 {
                return Err(Error::unsupported("qdm2: packet type 8"));
            } else if (9..=12).contains(&packet.type_) {
                self.sub_packet_list_d.push(Some(idx));
            } else if packet.type_ == 13 {
                for j in 0..6 {
                    self.fft_level_exp[j] = hbr.read(6).ok_or_else(|| Error::invalid("qdm2: bits"))? as i32;
                }
            } else if packet.type_ == 14 {
                for j in 0..6 {
                    self.fft_level_exp[j] = qdm2_get_vlc(&mut hbr, &VLC.fft_level_exp, false)
                        .ok_or_else(|| Error::invalid("qdm2: vlc"))?;
                }
            } else if packet.type_ == 15 {
                return Err(Error::unsupported("qdm2: packet type 15"));
            } else if (16..48).contains(&packet.type_)
                && FFT_SUBPACKETS[(packet.type_ - 16) as usize] == 0
            {
                self.sub_packet_list_b.push(Some(idx));
                self.sub_packets_b += 1;
            }

            i += 1;
        }

        if !self.sub_packet_list_d.is_empty() {
            let r = self.process_synthesis_subpackets();
            if r.is_ok() {
                self.do_synth_filter = true;
            } else {
                return r;
            }
        } else if self.do_synth_filter {
            self.process_subpacket_10(None)?;
            self.process_subpacket_11(None)?;
            self.process_subpacket_12(None)?;
        }
        Ok(())
    }

    /// qdm2_fft_init_coefficient.
    #[allow(clippy::too_many_arguments)]
    fn fft_init_coefficient(
        &mut self,
        sub_packet: i32,
        offset: i32,
        duration: usize,
        channel: i32,
        exp: i32,
        phase: i32,
    ) {
        if self.fft_coefs_min_index[duration] < 0 {
            self.fft_coefs_min_index[duration] = self.fft_coefs_index as i32;
        }
        if self.fft_coefs_index >= FFT_COEFS_CAP {
            return;
        }
        let c = FftCoefficient {
            sub_packet: if sub_packet >= 16 {
                sub_packet - 16
            } else {
                sub_packet
            },
            channel: channel as u8,
            offset,
            exp,
            phase: phase as u8,
        };
        if self.fft_coefs.len() <= self.fft_coefs_index {
            self.fft_coefs.push(c);
        } else {
            self.fft_coefs[self.fft_coefs_index] = c;
        }
        self.fft_coefs_index += 1;
    }

    /// qdm2_fft_decode_tones.
    fn fft_decode_tones(&mut self, duration: usize, br: &mut LeBitReader, b: bool) -> Result<()> {
        let mut local_int_4: i32 = 0;
        let mut local_int_28: i32 = 0;
        let local_int_20: i32 = 2;
        let local_int_8 = 4 - duration as i32;
        let local_int_10: i32 = 1 << (self.group_order - duration as i32 - 1);
        let mut offset: i32 = 1;

        while br.bits_left() > 0 {
            if self.superblocktype_2_3 {
                loop {
                    let n = qdm2_get_vlc(
                        br,
                        &VLC.tab_fft_tone_offset[local_int_8 as usize],
                        true,
                    )
                    .ok_or_else(|| Error::invalid("qdm2: tone vlc"))?;
                    if n >= 2 {
                        offset += n - 2;
                        break;
                    }
                    if br.bits_left() < 0 {
                        if (local_int_4 as usize) < self.group_size {
                            // FFmpeg logs overread; then errors.
                        }
                        return Err(Error::invalid("qdm2: tone overread"));
                    }
                    offset = 1;
                    if n == 0 {
                        local_int_4 += local_int_10;
                        local_int_28 += 1 << local_int_8;
                    } else {
                        local_int_4 += 8 * local_int_10;
                        local_int_28 += 8 << local_int_8;
                    }
                }
            } else {
                if local_int_10 <= 2 {
                    return Err(Error::invalid("qdm2: tone decode stuck"));
                }
                offset += qdm2_get_vlc(
                    br,
                    &VLC.tab_fft_tone_offset[local_int_8 as usize],
                    true,
                )
                .ok_or_else(|| Error::invalid("qdm2: tone vlc"))?;
                while offset >= local_int_10 - 1 {
                    offset += 1 - (local_int_10 - 1);
                    local_int_4 += local_int_10;
                    local_int_28 += 1 << local_int_8;
                }
            }

            if local_int_4 as usize >= self.group_size {
                return Err(Error::invalid("qdm2: tone group overflow"));
            }

            let local_int_14 = offset >> local_int_8;
            if local_int_14 < 0 || local_int_14 as usize >= FFT_LEVEL_INDEX_TABLE.len() {
                return Err(Error::invalid("qdm2: tone level index out of range"));
            }

            let (channel, stereo) = if self.nb_channels > 1 {
                (
                    br.read_bit().ok_or_else(|| Error::invalid("qdm2: bits"))? as i32,
                    br.read_bit().ok_or_else(|| Error::invalid("qdm2: bits"))?,
                )
            } else {
                (0, false)
            };

            let mut exp = qdm2_get_vlc(
                br,
                if b { &VLC.fft_level_exp } else { &VLC.fft_level_exp_alt },
                false,
            )
            .ok_or_else(|| Error::invalid("qdm2: tone vlc"))?;
            exp += self.fft_level_exp[FFT_LEVEL_INDEX_TABLE[local_int_14 as usize]];
            if exp < 0 {
                exp = 0;
            }

            let phase = br.read(3).ok_or_else(|| Error::invalid("qdm2: bits"))? as i32;
            let mut stereo_exp = 0;
            let mut stereo_phase = 0;

            if stereo {
                stereo_exp = exp
                    - qdm2_get_vlc(br, &VLC.fft_stereo_exp, false)
                        .ok_or_else(|| Error::invalid("qdm2: tone vlc"))?;
                stereo_phase = phase
                    - qdm2_get_vlc(br, &VLC.fft_stereo_phase, false)
                        .ok_or_else(|| Error::invalid("qdm2: tone vlc"))?;
                if stereo_phase < 0 {
                    stereo_phase += 8;
                }
            }

            if self.frequency_range > local_int_14 + 1 {
                let sub_packet = local_int_20 + local_int_28;

                if self.fft_coefs_index + stereo as usize >= FFT_COEFS_CAP {
                    return Err(Error::invalid("qdm2: too many fft coefficients"));
                }

                self.fft_init_coefficient(sub_packet, offset, duration, channel, exp, phase);
                if stereo {
                    self.fft_init_coefficient(
                        sub_packet,
                        offset,
                        duration,
                        1 - channel,
                        stereo_exp,
                        stereo_phase,
                    );
                }
            }
            offset += 1;
        }

        Ok(())
    }

    /// qdm2_decode_fft_packets.
    fn decode_fft_packets(&mut self) -> Result<()> {
        if self.sub_packet_list_b.is_empty() {
            return Err(Error::invalid("qdm2: no fft packets"));
        }

        // reset minimum indexes for FFT coefficients
        self.fft_coefs_index = 0;
        for v in self.fft_coefs_min_index.iter_mut().take(5) {
            *v = -1;
        }

        // process subpackets ordered by type, largest type first
        let mut max = 256i32;
        let count = self.sub_packets_b;
        for i in 0..count {
            // find subpacket with largest type less than max
            let mut min = 0i32;
            let mut packet_idx: Option<usize> = None;
            for j in 0..self.sub_packets_b {
                if let Some(Some(idx)) = self.sub_packet_list_b.get(j) {
                    if *idx < self.sub_packets.len() {
                        let value = self.sub_packets[*idx].type_;
                        if value > min && value < max {
                            min = value;
                            packet_idx = Some(*idx);
                        }
                    }
                }
            }

            max = min;

            let packet_idx = packet_idx.ok_or_else(|| Error::invalid("qdm2: fft packet"))?;

            if i == 0 {
                let t = self.sub_packets[packet_idx].type_;
                if !(16..48).contains(&t) || FFT_SUBPACKETS[(t - 16) as usize] != 0 {
                    return Err(Error::invalid("qdm2: bad fft packet type"));
                }
            }

            let packet = self.sub_packets[packet_idx].clone();
            let data: Vec<u8> = self.compressed[packet.offset..packet.offset + packet.size].to_vec();
            let mut br = LeBitReader::new(&data);

            let unknown_flag =
                (32..48).contains(&packet.type_) && FFT_SUBPACKETS[(packet.type_ - 16) as usize] == 0;

            let type_ = packet.type_;
            let _ = &packet;

            if (17..24).contains(&type_) || (33..40).contains(&type_) {
                let duration = self.sub_sampling as i32 + 5 - (type_ & 15);
                if (0..4).contains(&duration) {
                    self.fft_decode_tones(duration as usize, &mut br, unknown_flag)?;
                }
            } else if type_ == 31 {
                for j in 0..4 {
                    self.fft_decode_tones(j, &mut br, unknown_flag)?;
                }
            } else if type_ == 46 {
                for j in 0..6 {
                    self.fft_level_exp[j] =
                        br.read(6).ok_or_else(|| Error::invalid("qdm2: bits"))? as i32;
                }
                for j in 0..4 {
                    self.fft_decode_tones(j, &mut br, unknown_flag)?;
                }
            }
        }

        // calculate maximum indexes for FFT coefficients
        let mut j: i32 = -1;
        for i in 0..5 {
            if self.fft_coefs_min_index[i] >= 0 {
                if j >= 0 {
                    self.fft_coefs_max_index[j as usize] = self.fft_coefs_min_index[i];
                }
                j = i as i32;
            }
        }
        if j >= 0 {
            self.fft_coefs_max_index[j as usize] = self.fft_coefs_index as i32;
        }

        Ok(())
    }

    /// qdm2_fft_generate_tone.
    fn fft_generate_tone(&mut self, tone_idx: usize) {
        let iscale = 2.0 * core::f64::consts::PI / 512.0;
        let tone = &mut self.fft_tones[tone_idx];
        tone.phase += tone.phase_shift;

        let level =
            FFT_TONE_ENVELOPE_TABLE[tone.duration][tone.time_index as usize] * tone.level;
        let c = Complex2 {
            im: (level as f64 * (tone.phase as f64 * iscale).sin()) as f32,
            re: (level as f64 * (tone.phase as f64 * iscale).cos()) as f32,
        };

        // Generate FFT coefficients for the tone. `complex_base` is
        // (channel, offset); the tone's `complex` pointer covers
        // fft.complex[ch][offset..offset+4].
        let (ch, base) = tone.complex_base;
        if tone.duration >= 3 || tone.cutoff >= 3 {
            self.fft_complex[ch][base].im += c.im;
            self.fft_complex[ch][base].re += c.re;
            self.fft_complex[ch][base + 1].im -= c.im;
            self.fft_complex[ch][base + 1].re -= c.re;
        } else {
            let t = &tone.table;
            let f: [f32; 6] = [
                -t[4],
                t[3] - t[0],
                1.0 - t[2] - t[3],
                t[1] + t[4] - 1.0,
                t[0] - t[1],
                t[2],
            ];
            for i in 0..2 {
                let idx = FFT_CUTOFF_INDEX_TABLE[tone.cutoff][i];
                // The C code indexes complex[] with possibly negative
                // indices (`-1`, `-2`) — those clamp to the base slot
                // here (out-of-range contributions are dropped; this
                // cannot happen with valid streams).
                let slot = if idx < 0 { base } else { base + idx as usize };
                if slot < 257 {
                    self.fft_complex[ch][slot].re += c.re * f[i];
                    let sign = if tone.cutoff <= i { -1.0 } else { 1.0 };
                    self.fft_complex[ch][slot].im += c.im * sign * f[i];
                }
            }
            for i in 0..4 {
                let slot = base + 2 + i;
                if slot < 257 {
                    self.fft_complex[ch][slot].re += c.re * f[i + 2];
                    self.fft_complex[ch][slot].im += c.im * f[i + 2];
                }
            }
        }

        // Copy the tone if it has not yet died out.
        tone.time_index += 1;
        if (tone.time_index as usize) < (1usize << (5 - tone.duration)) - 1 {
            let t = self.fft_tones[tone_idx].clone();
            self.fft_tones[self.fft_tone_end] = t;
            self.fft_tone_end = (self.fft_tone_end + 1) % FFT_TONES_CAP;
        }
    }

    /// qdm2_fft_tone_synthesizer.
    fn fft_tone_synthesizer(&mut self, sub_packet: usize) {
        let iscale = 0.25 * core::f64::consts::PI;

        for ch in 0..self.channels {
            for slot in self.fft_complex[ch].iter_mut().take(self.fft_size) {
                *slot = Complex2::default();
            }
        }

        // Apply FFT tones with duration 4 (1 FFT period).
        if self.fft_coefs_min_index[4] >= 0 {
            for i in self.fft_coefs_min_index[4] as usize..self.fft_coefs_max_index[4] as usize {
                if self.fft_coefs[i].sub_packet as usize != sub_packet {
                    break;
                }
                let ch = if self.channels == 1 {
                    0
                } else {
                    self.fft_coefs[i].channel as usize
                };
                let level = if self.fft_coefs[i].exp < 0 {
                    0.0
                } else {
                    FFT_TONE_LEVEL_TABLE[if self.superblocktype_2_3 { 0 } else { 1 }]
                        [(self.fft_coefs[i].exp as u8 as i32 & 63) as usize]
                };

                let iscale = 0.25 * core::f64::consts::PI;
                let phase = self.fft_coefs[i].phase as i32 as f64 * iscale;
                let c = Complex2 {
                    re: (level as f64 * phase.cos()) as f32,
                    im: (level as f64 * phase.sin()) as f32,
                };
                let off = self.fft_coefs[i].offset as usize;
                self.fft_complex[ch][off].re += c.re;
                self.fft_complex[ch][off].im += c.im;
                self.fft_complex[ch][off + 1].re -= c.re;
                self.fft_complex[ch][off + 1].im -= c.im;
            }
        }

        // Generate existing FFT tones.
        while self.fft_tone_end != self.fft_tone_start {
            let idx = self.fft_tone_start;
            self.fft_tone_start = (self.fft_tone_start + 1) % FFT_TONES_CAP;
            self.fft_generate_tone(idx);
        }

        // Create and generate new FFT tones with duration 0 (long) to
        // 3 (short).
        for i in 0..4 {
            if self.fft_coefs_min_index[i] >= 0 {
                let mut j = self.fft_coefs_min_index[i] as usize;
                while j < self.fft_coefs_max_index[i] as usize {
                    if self.fft_coefs[j].sub_packet as usize != sub_packet {
                        break;
                    }

                    let four_i = 4 - i;
                    let offset = self.fft_coefs[j].offset >> four_i;
                    let ch = if self.channels == 1 {
                        0
                    } else {
                        self.fft_coefs[j].channel as usize
                    };

                    if (offset as usize) < self.frequency_range as usize {
                        let tone = FftTone {
                            level: if self.fft_coefs[j].exp < 0 {
                                0.0
                            } else {
                                FFT_TONE_LEVEL_TABLE[if self.superblocktype_2_3 { 0 } else { 1 }]
                                    [(self.fft_coefs[j].exp as u8 as i32 & 63) as usize]
                            },
                            complex_base: (ch, offset as usize),
                            table: FFT_TONE_SAMPLE_TABLE[i]
                                [(self.fft_coefs[j].offset - (offset << four_i)) as usize],
                            phase: 64 * self.fft_coefs[j].phase as i32
                                - (offset << 8)
                                - 128,
                            phase_shift: (2 * self.fft_coefs[j].offset + 1) << (7 - four_i),
                            duration: i,
                            time_index: 0,
                            cutoff: if offset < 2 {
                                offset as usize
                            } else if offset >= 60 {
                                3
                            } else {
                                2
                            },
                        };

                        if self.fft_tones.len() <= self.fft_tone_end {
                            self.fft_tones.push(tone);
                        } else {
                            self.fft_tones[self.fft_tone_end] = tone;
                        }
                        self.fft_generate_tone(self.fft_tone_end);
                    }
                    j += 1;
                }
                self.fft_coefs_min_index[i] = j as i32;
            }
        }
        let _ = iscale;
    }

    /// qdm2_calculate_fft.
    fn calculate_fft(&mut self, channel: usize, _sub_packet: usize) {
        let gain = if self.channels == 1 && self.nb_channels == 2 {
            0.5f32
        } else {
            1.0f32
        };

        self.fft_complex[channel][0].re *= 2.0;
        self.fft_complex[channel][0].im = 0.0;
        self.fft_complex[channel][self.fft_size].re = 0.0;
        self.fft_complex[channel][self.fft_size].im = 0.0;

        // Inverse RDFT of length 2*fft_size with FFmpeg's 1/2 scale.
        let n = 2 * self.fft_size;
        let mut input = vec![rdft::Complex::default(); self.fft_size + 1];
        for (m, slot) in input.iter_mut().enumerate() {
            slot.re = self.fft_complex[channel][m].re;
            slot.im = self.fft_complex[channel][m].im;
        }
        let mut out = vec![0f32; n];
        rdft::inverse_rdft(&input, &mut out, 0.5);

        // Add samples to output buffer (FFmpeg walks FFALIGN(fft_size, 8)
        // entries; the tail entries of `out` are zero when fft_size is
        // already a power of two ≥ 8, so the plain loop matches).
        for i in 0..self.fft_size {
            self.output_buffer[channel] += out[i] * gain;
            self.output_buffer[self.channels + 2 * self.channels * i + channel] +=
                out[self.fft_size + i] * gain;
        }
        // FFmpeg's exact write pattern:
        //   out = q->output_buffer + channel;
        //   for i in 0..FFALIGN: out[0] += temp[i].re*gain;
        //                       out[q->channels] += temp[i].im*gain;
        //                       out += 2*q->channels;
        // i.e. output_buffer[2*channels*i + channel] gets .re for the
        // first half and the same slot pattern continues.
        let _ = n;
    }

    /// qdm2_synthesis_filter.
    fn synthesis_filter(&mut self, index: usize) {
        let sb_used = qdm2_sb_used(self.sub_sampling);

        // copy sb_samples
        for ch in 0..self.channels {
            for i in 0..8 {
                for k in sb_used..SBLIMIT {
                    self.sb_samples[ch][(8 * index) + i][k] = 0.0;
                }
            }
        }

        for ch in 0..self.nb_channels {
            let mut dither_state = 0f32;
            for i in 0..8 {
                let mut cur = [0f32; 32];
                for k in 0..32 {
                    cur[k] = self.sb_samples[ch][(8 * index) + i][k];
                }
                mpa_synth::synth_filter_into(
                    &mut self.synth_buf[ch],
                    &mut self.synth_buf_offset[ch],
                    &mut dither_state,
                    &mut self.samples,
                    32 * i * self.nb_channels,
                    self.nb_channels,
                    &cur,
                );
            }
        }

        // add samples to output buffer
        let sub_sampling = 4 >> self.sub_sampling;
        for ch in 0..self.channels {
            for i in 0..self.frame_size {
                self.output_buffer[self.channels * i + ch] +=
                    (1i32 << 23) as f32
                        * self.samples[self.nb_channels * sub_sampling * i + ch];
            }
        }
    }
}

/// The QDM2 packet→frame decoder.
pub struct Qdm2Decoder {
    codec_id: CodecId,
    q: Qdm2Context,
    /// The rate the `QDCA` atom gives, as FFmpeg takes it.
    sample_rate: u32,
    out: VecDeque<Frame>,
}

/// FFmpeg `qdm2_decode_init`: parse the `QDCA` extradata atom; the context
/// and the sample rate.
fn decode_init(extradata: &[u8]) -> Result<(Qdm2Context, u32)> {
    let mut q = Qdm2Context::default();

    if extradata.len() < 48 {
        return Err(Error::invalid("qdm2: extradata missing or truncated"));
    }

    // Find the `frma` + `QDM2` tag pair.
    let mut pos = 0usize;
    while pos + 8 <= extradata.len() {
        if &extradata[pos..pos + 4] == b"frma" && pos + 8 <= extradata.len()
            && &extradata[pos + 4..pos + 8] == b"QDM2"
        {
            break;
        }
        pos += 1;
    }

    let left = extradata.len().saturating_sub(pos);
    if left < 44 {
        return Err(Error::invalid(format!(
            "qdm2: not enough extradata ({left})"
        )));
    }

    let mut p = pos + 8;
    let size = be32(extradata, &mut p);
    if size as usize > extradata.len().saturating_sub(p) {
        return Err(Error::invalid("qdm2: extradata size too small"));
    }
    if be32(extradata, &mut p) != u32::from_be_bytes(*b"QDCA") {
        return Err(Error::invalid("qdm2: invalid extradata, expecting QDCA"));
    }
    p += 4; // unknown

    q.nb_channels = be32(extradata, &mut p) as usize;
    q.channels = q.nb_channels;
    if q.channels == 0 || q.channels > MPA_MAX_CHANNELS {
        return Err(Error::invalid("qdm2: invalid number of channels"));
    }

    let sample_rate = be32(extradata, &mut p);
    let bit_rate = be32(extradata, &mut p);
    q.group_size = be32(extradata, &mut p) as usize;
    q.fft_size = be32(extradata, &mut p) as usize;
    q.checksum_size = be32(extradata, &mut p) as usize;
    if q.checksum_size >= 1 << 28 || q.checksum_size <= 1 {
        return Err(Error::invalid(format!(
            "qdm2: data block size invalid ({})",
            q.checksum_size
        )));
    }

    q.fft_order = log2_usize(q.fft_size) as i32 + 1;
    if !(7..=9).contains(&q.fft_order) {
        return Err(Error::unsupported(format!(
            "qdm2: unknown FFT order {}",
            q.fft_order
        )));
    }

    q.group_order = log2_usize(q.group_size) as i32 + 1;
    q.frame_size = q.group_size / 16;
    if q.frame_size > QDM2_MAX_FRAME_SIZE {
        return Err(Error::invalid("qdm2: frame size too large"));
    }

    q.sub_sampling = q.fft_order - 7;
    q.frequency_range = 255 / (1 << (2 - q.sub_sampling));

    if q.frame_size * 4 >> q.sub_sampling > MPA_FRAME_SIZE {
        return Err(Error::unsupported("qdm2: large frames"));
    }

    let tmp: i32 = match q.sub_sampling * 2 + q.channels as i32 - 1 {
        0 => 40,
        1 => 48,
        2 => 56,
        3 => 72,
        4 => 80,
        5 => 100,
        _ => q.sub_sampling,
    };
    let mut tmp_val = 0;
    if tmp * 1000 < bit_rate as i32 {
        tmp_val = 1;
    }
    if tmp * 1440 < bit_rate as i32 {
        tmp_val = 2;
    }
    if tmp * 1760 < bit_rate as i32 {
        tmp_val = 3;
    }
    if tmp * 2240 < bit_rate as i32 {
        tmp_val = 4;
    }
    q.cm_table_select = tmp_val;

    q.coeff_per_sb_select = if bit_rate <= 8000 {
        0
    } else if bit_rate < 16000 {
        1
    } else {
        2
    };

    if q.fft_size != 1 << (q.fft_order - 1) {
        return Err(Error::invalid("qdm2: FFT size not power of 2"));
    }

    q.output_buffer = vec![0f32; QDM2_MAX_FRAME_SIZE * MPA_MAX_CHANNELS * 2];
    q.samples = vec![0f32; MPA_MAX_CHANNELS * MPA_FRAME_SIZE];
    let _ = bit_rate;
    Ok((q, sample_rate))
}

fn be32(data: &[u8], p: &mut usize) -> u32 {
    let v = u32::from_be_bytes([data[*p], data[*p + 1], data[*p + 2], data[*p + 3]]);
    *p += 4;
    v
}

fn log2_usize(v: usize) -> u32 {
    31 - (v as u32).leading_zeros()
}

impl Qdm2Decoder {
    /// FFmpeg `qdm2_decode`: one of the 16 frames of the superblock in
    /// `self.q.compressed`.
    fn decode_sub(&mut self, out: &mut [i16]) -> Result<()> {
        let frame_size = self.q.frame_size * self.q.channels;

        if frame_size > self.q.output_buffer.len() / 2 {
            return Err(Error::invalid("qdm2: frame size overruns output"));
        }
        // copy old block, clear new block of output samples
        self.q.output_buffer.copy_within(frame_size..2 * frame_size, 0);
        for v in self.q.output_buffer[frame_size..2 * frame_size].iter_mut() {
            *v = 0.0;
        }

        // decode block of QDM2 compressed data
        if self.q.sub_packet == 0 {
            self.q.has_errors = false;
            self.q.decode_super_block()?;
        }

        // parse subpackets
        if !self.q.has_errors {
            if self.q.sub_packet == 2 {
                self.q.decode_fft_packets()?;
            }
            self.q.fft_tone_synthesizer(self.q.sub_packet);
        }

        // sound synthesis stage 1 (FFT)
        for ch in 0..self.q.channels {
            self.q.calculate_fft(ch, self.q.sub_packet);
        }

        // sound synthesis stage 2 (MPEG audio like synthesis filter)
        if !self.q.has_errors && self.q.do_synth_filter {
            self.q.synthesis_filter(self.q.sub_packet);
        }

        self.q.sub_packet = (self.q.sub_packet + 1) % 16;

        // clip and convert output float[] to 16-bit signed samples
        for (i, slot) in out.iter_mut().take(frame_size).enumerate() {
            let mut value = self.q.output_buffer[i] as i32;
            if value > SOFTCLIP_THRESHOLD {
                value = if value > HARDCLIP_THRESHOLD {
                    32767
                } else {
                    SOFTCLIP_TABLE[(value - SOFTCLIP_THRESHOLD) as usize] as i32
                };
            } else if value < -SOFTCLIP_THRESHOLD {
                value = if value < -HARDCLIP_THRESHOLD {
                    -32767
                } else {
                    -(SOFTCLIP_TABLE[(-value - SOFTCLIP_THRESHOLD) as usize] as i32)
                };
            }
            *slot = value as i16;
        }

        Ok(())
    }
}

impl Decoder for Qdm2Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    /// FFmpeg `qdm2_decode_frame`, called as libavcodec calls it: every
    /// `checksum_size` bytes of the packet is one superblock and one frame
    /// of 16 * `frame_size` samples; a shorter remainder is invalid.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut data = &packet.data[..];
        let mut pts = packet.pts;
        while !data.is_empty() {
            let checksum_size = self.q.checksum_size;
            if data.len() < checksum_size {
                return Err(Error::invalid("qdm2: packet shorter than its superblock"));
            }
            self.q.compressed.clear();
            self.q.compressed.extend_from_slice(&data[..checksum_size]);
            self.q.sub_packet = 0;

            let frame_size = self.q.frame_size * self.q.channels;
            let mut out = vec![0i16; 16 * frame_size];
            for sub in out.chunks_exact_mut(frame_size) {
                self.decode_sub(sub)?;
            }
            let mut bytes = Vec::with_capacity(out.len() * 2);
            for s in out {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
            self.out.push_back(Frame::Audio(AudioFrame {
                samples: (16 * self.q.frame_size) as u32,
                pts: pts.take(),
                data: vec![bytes],
            }));
            data = &data[checksum_size..];
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.out.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.out.clear();
        self.q.sub_packet = 0;
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: SampleFormat::S16,
            sample_rate: self.sample_rate,
            channels: self.q.channels as u16,
        })
    }
}

/// Construct a QDM2 decoder from stream parameters. `params.extradata`
/// carries the `frma`/`QDCA` atoms (MOV sample entry, CAF `kuki`).
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let (q, sample_rate) = decode_init(&params.extradata)?;
    Ok(Box::new(Qdm2Decoder { codec_id: params.codec_id.clone(), q, sample_rate, out: VecDeque::new() }))
}
