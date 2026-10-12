// Ported from FFmpeg (commit 2da55bf): libavcodec/h264data.c
// (ff_h264_i_mb_type_info, ff_h264_golomb_to_intra4x4_cbp,
// ff_h264_golomb_to_inter_cbp, ff_h264_chroma_qp, ff_h264_quant_rem6,
// ff_h264_quant_div6, ff_h264_dequant4_coeff_init), libavcodec/svq3.c
// (svq3_scan, luma_dc_zigzag_scan, svq3_pred_0, svq3_pred_1,
// svq3_dct_tables, svq3_dequant_coeff), libavcodec/mathtables.c
// (ff_zigzag_scan), libavcodec/h264data.c (ff_h264_chroma_dc_scan) and
// libavcodec/h264_parse.h (scan8).
// License: LGPL-2.1-or-later

//! The constant tables SVQ3 decodes with.

/// H.264 intra macroblock type info: `(pred_mode, cbp)` per MB type
/// (FFmpeg `ff_h264_i_mb_type_info`). Entry 0 is INTRA4x4 (`pred_mode`
/// -1); entry 25 would be I_PCM which SVQ3 never produces.
pub const I_MB_TYPE_INFO: [(i8, i32); 25] = [
    (-1, -1), // 0: INTRA4x4 (unused here)
    (2, 0),
    (1, 0),
    (0, 0),
    (3, 0),
    (2, 16),
    (1, 16),
    (0, 16),
    (3, 16),
    (2, 32),
    (1, 32),
    (0, 32),
    (3, 32),
    (2, 15 + 0),
    (1, 15 + 0),
    (0, 15 + 0),
    (3, 15 + 0),
    (2, 15 + 16),
    (1, 15 + 16),
    (0, 15 + 16),
    (3, 15 + 16),
    (2, 15 + 32),
    (1, 15 + 32),
    (0, 15 + 32),
    (3, 15 + 32),
];

pub const GOLOMB_TO_INTRA4X4_CBP: [u8; 48] = [
    47, 31, 15, 0, 23, 27, 29, 30, 7, 11, 13, 14, 39, 43, 45, 46, 16, 3, 5, 10, 12, 19, 21, 26, 28,
    35, 37, 42, 44, 1, 2, 4, 8, 17, 18, 20, 24, 6, 9, 22, 25, 32, 33, 34, 36, 40, 38, 41,
];

pub const GOLOMB_TO_INTER_CBP: [u8; 48] = [
    0, 16, 1, 2, 4, 8, 32, 3, 5, 10, 12, 15, 47, 7, 11, 13, 14, 6, 9, 31, 35, 37, 42, 44, 33, 34,
    36, 40, 39, 43, 45, 46, 17, 18, 20, 24, 19, 21, 26, 28, 23, 27, 29, 30, 22, 25, 38, 41,
];

/// `ff_h264_chroma_qp[0]` (CHROMA_QP_TABLE_END(8)); SVQ3 reads entries
/// `qscale + 12` for qscale 0..=31.
pub const CHROMA_QP: [u8; 52] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
];

/// `ff_h264_quant_rem6[qP + 6*(8-8)]` = q % 6 over the 8-bit qP range
/// (0..=51 in SVQ3's use).
pub const QUANT_REM6: [u8; 52] = {
    let mut t = [0u8; 52];
    let mut q = 0usize;
    while q < 52 {
        t[q] = (q % 6) as u8;
        q += 1;
    }
    t
};

/// `ff_h264_quant_div6[qP + 6*(8-8)]` = q / 6 over 0..=51.
pub const QUANT_DIV6: [u8; 52] = {
    let mut t = [0u8; 52];
    let mut q = 0usize;
    while q < 52 {
        t[q] = (q / 6) as u8;
        q += 1;
    }
    t
};

/// `ff_h264_dequant4_coeff_init[6][3]`.
pub const DEQUANT4_COEFF_INIT: [[u8; 3]; 6] = [
    [10, 13, 16],
    [11, 14, 18],
    [13, 16, 20],
    [14, 18, 23],
    [16, 20, 25],
    [18, 23, 29],
];

/// SVQ3's dual scan for 4x4 residual blocks (`svq3.c`).
pub const SVQ3_SCAN: [u8; 16] = [
    0 + 0 * 4, 1 + 0 * 4, 2 + 0 * 4, 2 + 1 * 4, 2 + 2 * 4, 3 + 0 * 4, 3 + 1 * 4, 3 + 2 * 4, 0 + 1 * 4,
    0 + 2 * 4, 1 + 1 * 4, 1 + 2 * 4, 0 + 3 * 4, 1 + 3 * 4, 2 + 3 * 4, 3 + 3 * 4,
];

/// Zig-zag scan (`ff_zigzag_scan`).
pub const ZIGZAG_SCAN: [u8; 16] = [
    0 + 0 * 4, 1 + 0 * 4, 0 + 1 * 4, 0 + 2 * 4, 1 + 1 * 4, 2 + 0 * 4, 3 + 0 * 4, 2 + 1 * 4, 1 + 2 * 4,
    0 + 3 * 4, 1 + 3 * 4, 2 + 2 * 4, 3 + 1 * 4, 3 + 2 * 4, 2 + 3 * 4, 3 + 3 * 4,
];

/// Luma DC zig-zag scan for INTRA16x16 DC (`luma_dc_zigzag_scan`).
pub const LUMA_DC_ZIGZAG_SCAN: [u8; 16] = [
    0 * 16 + 0 * 64, 1 * 16 + 0 * 64, 2 * 16 + 0 * 64, 0 * 16 + 2 * 64, 3 * 16 + 0 * 64,
    0 * 16 + 1 * 64, 1 * 16 + 1 * 64, 2 * 16 + 1 * 64, 1 * 16 + 2 * 64, 2 * 16 + 2 * 64,
    3 * 16 + 2 * 64, 0 * 16 + 3 * 64, 3 * 16 + 1 * 64, 1 * 16 + 3 * 64, 2 * 16 + 3 * 64,
    3 * 16 + 3 * 64,
];

/// Chroma DC scan (`ff_h264_chroma_dc_scan`): offsets of the four 2x2
/// positions inside the 16x16 `mb` block layout.
pub const CHROMA_DC_SCAN: [u8; 4] = [(0 + 0 * 2) * 16, (1 + 0 * 2) * 16, (0 + 1 * 2) * 16, (1 + 1 * 2) * 16];

/// Luma 4x4 prediction-pair table (`svq3_pred_0`).
pub const SVQ3_PRED_0: [[u8; 2]; 25] = [
    [0, 0],
    [1, 0], [0, 1],
    [0, 2], [1, 1], [2, 0],
    [3, 0], [2, 1], [1, 2], [0, 3],
    [0, 4], [1, 3], [2, 2], [3, 1], [4, 0],
    [4, 1], [3, 2], [2, 3], [1, 4],
    [2, 4], [3, 3], [4, 2],
    [4, 3], [3, 4],
    [4, 4],
];

/// Prediction-derivation table (`svq3_pred_1`), indexed
/// `[top+1][left+1][svq3_pred_0_entry]`.
pub const SVQ3_PRED_1: [[[i8; 5]; 6]; 6] = [
    [[2, -1, -1, -1, -1], [2, 1, -1, -1, -1], [1, 2, -1, -1, -1], [2, 1, -1, -1, -1], [1, 2, -1, -1, -1], [1, 2, -1, -1, -1]],
    [[0, 2, -1, -1, -1], [0, 2, 1, 4, 3], [0, 1, 2, 4, 3], [0, 2, 1, 4, 3], [2, 0, 1, 3, 4], [0, 4, 2, 1, 3]],
    [[2, 0, -1, -1, -1], [2, 1, 0, 4, 3], [1, 2, 4, 0, 3], [2, 1, 0, 4, 3], [2, 1, 4, 3, 0], [1, 2, 4, 0, 3]],
    [[2, 0, -1, -1, -1], [2, 0, 1, 4, 3], [1, 2, 0, 4, 3], [2, 1, 0, 4, 3], [2, 1, 3, 4, 0], [2, 4, 1, 0, 3]],
    [[0, 2, -1, -1, -1], [0, 2, 1, 3, 4], [1, 2, 3, 0, 4], [2, 0, 1, 3, 4], [2, 1, 3, 0, 4], [2, 0, 4, 3, 1]],
    [[0, 2, -1, -1, -1], [0, 2, 4, 1, 3], [1, 4, 2, 0, 3], [4, 2, 0, 1, 3], [2, 0, 1, 4, 3], [4, 2, 1, 0, 3]],
];

/// Run/level tables (`svq3_dct_tables`).
pub const SVQ3_DCT_TABLES: [[(u8, u8); 16]; 2] = [
    [
        (0, 0), (0, 1), (1, 1), (2, 1), (0, 2), (3, 1), (4, 1), (5, 1), (0, 3), (1, 2), (2, 2),
        (6, 1), (7, 1), (8, 1), (9, 1), (0, 4),
    ],
    [
        (0, 0), (0, 1), (1, 1), (0, 2), (2, 1), (0, 3), (0, 4), (0, 5), (3, 1), (4, 1), (1, 2),
        (1, 3), (0, 6), (0, 7), (0, 8), (0, 9),
    ],
];

/// SVQ3's own dequant coefficients (`svq3_dequant_coeff`), indexed by
/// qP 0..=31.
pub const SVQ3_DEQUANT_COEFF: [u32; 32] = [
    3881, 4351, 4890, 5481, 6154, 6914, 7761, 8718, 9781, 10987, 12339, 13828, 15523, 17435, 19561,
    21873, 24552, 27656, 30847, 34870, 38807, 43747, 49103, 54683, 61694, 68745, 77615, 89113,
    100253, 109366, 126635, 141533,
];

/// H.264 `scan8`: cache index of each of the 16 luma / 8+8 chroma 4x4
/// blocks plus the 3 DC positions (FFmpeg `scan8[16*3+3]`).
pub const SCAN8: [u8; 51] = scan8();

const fn scan8() -> [u8; 51] {
    let mut t = [0u8; 51];
    let rows: [u8; 48] = [
        4 + 1 * 8, 5 + 1 * 8, 4 + 2 * 8, 5 + 2 * 8, 6 + 1 * 8, 7 + 1 * 8, 6 + 2 * 8, 7 + 2 * 8,
        4 + 3 * 8, 5 + 3 * 8, 4 + 4 * 8, 5 + 4 * 8, 6 + 3 * 8, 7 + 3 * 8, 6 + 4 * 8, 7 + 4 * 8,
        4 + 6 * 8, 5 + 6 * 8, 4 + 7 * 8, 5 + 7 * 8, 6 + 6 * 8, 7 + 6 * 8, 6 + 7 * 8, 7 + 7 * 8,
        4 + 8 * 8, 5 + 8 * 8, 4 + 9 * 8, 5 + 9 * 8, 6 + 8 * 8, 7 + 8 * 8, 6 + 9 * 8, 7 + 9 * 8,
        4 + 11 * 8, 5 + 11 * 8, 4 + 12 * 8, 5 + 12 * 8, 6 + 11 * 8, 7 + 11 * 8, 6 + 12 * 8, 7 + 12 * 8,
        4 + 13 * 8, 5 + 13 * 8, 4 + 14 * 8, 5 + 14 * 8, 6 + 13 * 8, 7 + 13 * 8, 6 + 14 * 8, 7 + 14 * 8,
    ];
    let mut i = 0usize;
    while i < 48 {
        t[i] = rows[i];
        i += 1;
    }
    t[48] = 0 + 0 * 8;
    t[49] = 0 + 5 * 8;
    t[50] = 0 + 10 * 8;
    t
}

/// Sentinel FFmpeg uses in `ref_cache` for unavailable references
/// (`PART_NOT_AVAILABLE`).
pub const PART_NOT_AVAILABLE: i8 = -2;
