// QDM2 (QDesign Music Codec 2) decoder.
//
// Ported from FFmpeg libavcodec/qdm2.c, qdm2data.h and qdm2_tablegen.h,
// and the MPEG audio synthesis it calls: mpegaudiodsp_template.c (float),
// mpegaudiodsp_data.c and dct32_template.c (commit 2da55bf),
// LGPL-2.1-or-later.

//! QDM2: a superblock per packet, its subpackets driving two synthesis
//! paths that add into one output buffer, 16 frames per superblock:
//!
//! - subpackets 9 to 12: coefficients for an MPEG audio style subband
//!   synthesis filter (`ff_mpa_synth_filter_float`: `dct32`, window);
//! - subpackets 16 to 47: tones, synthesized in the frequency domain and
//!   turned to sound by an inverse real FFT (`tx`, FFmpeg's C path).
//!
//! The output is soft-clipped to interleaved S16, 16 * `frame_size`
//! samples per channel per packet of `checksum_size` bytes. Arithmetic
//! follows FFmpeg's C, with the multiply-adds clang fuses on arm64 fused
//! here too, so the output matches FFmpeg's (`-cpuflags 0`). Bitstream
//! reads past a subpacket see the bytes after it, as FFmpeg's do; every
//! index taken from the stream is checked.

use std::collections::VecDeque;
use std::sync::LazyLock;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

use crate::getbits::GetBitsLe;
use crate::tx::{Complex, RdftC2r};
use crate::vlc::Vlc;

/// `tab_level`
#[rustfmt::skip]
const TAB_LEVEL: [[u8; 2]; 24] = [[12, 4], [17, 4], [1, 6], [8, 6], [9, 5], [20, 7], [3, 7], [5, 6], [6, 6], [2, 7], [22, 9], [23, 10], [0, 10], [21, 8], [11, 4], [19, 5], [7, 6], [4, 6], [16, 3], [10, 4], [18, 4], [15, 3], [13, 3], [14, 3]];
/// `tab_diff`
#[rustfmt::skip]
const TAB_DIFF: [[u8; 2]; 33] = [[2, 3], [1, 3], [5, 3], [14, 8], [20, 9], [26, 10], [25, 12], [32, 12], [19, 11], [16, 8], [24, 9], [17, 9], [12, 7], [13, 7], [9, 5], [7, 4], [3, 2], [4, 3], [8, 6], [11, 6], [18, 8], [15, 8], [30, 11], [36, 13], [34, 13], [29, 13], [0, 13], [21, 10], [28, 10], [23, 10], [22, 8], [10, 6], [6, 4]];
/// `tab_run`
#[rustfmt::skip]
const TAB_RUN: [[u8; 2]; 6] = [[1, 1], [2, 2], [3, 3], [4, 4], [5, 5], [0, 5]];
/// `tab_tone_level_idx_hi1`
#[rustfmt::skip]
const TAB_TONE_LEVEL_IDX_HI1: [[u8; 2]; 20] = [[4, 3], [5, 5], [9, 10], [11, 11], [13, 12], [14, 12], [10, 10], [12, 11], [17, 14], [16, 14], [18, 15], [0, 15], [19, 14], [15, 12], [8, 8], [7, 7], [6, 6], [1, 4], [2, 2], [3, 1]];
/// `tab_tone_level_idx_mid`
#[rustfmt::skip]
const TAB_TONE_LEVEL_IDX_MID: [[u8; 2]; 13] = [[18, 2], [19, 4], [20, 6], [14, 7], [21, 8], [13, 9], [22, 10], [12, 11], [23, 12], [0, 12], [15, 5], [16, 3], [17, 1]];
/// `tab_tone_level_idx_hi2`
#[rustfmt::skip]
const TAB_TONE_LEVEL_IDX_HI2: [[u8; 2]; 18] = [[14, 4], [11, 6], [19, 7], [9, 7], [13, 5], [10, 6], [20, 8], [8, 8], [6, 10], [23, 11], [0, 11], [21, 9], [7, 8], [12, 5], [18, 4], [16, 2], [15, 2], [17, 2]];
/// `tab_type30`
#[rustfmt::skip]
const TAB_TYPE30: [[u8; 2]; 9] = [[2, 3], [6, 4], [7, 5], [8, 6], [0, 6], [5, 3], [1, 3], [3, 2], [4, 2]];
/// `tab_type34`
#[rustfmt::skip]
const TAB_TYPE34: [[u8; 2]; 10] = [[1, 4], [9, 5], [0, 5], [3, 3], [7, 3], [8, 3], [2, 3], [4, 3], [6, 3], [5, 3]];
/// `tab_fft_tone_offset`
#[rustfmt::skip]
const TAB_FFT_TONE_OFFSET: [[u8; 2]; 153] = [[2, 2], [7, 7], [15, 8], [21, 8], [3, 6], [6, 6], [13, 7], [14, 8], [18, 8], [4, 4], [5, 5], [11, 7], [10, 7], [20, 6], [12, 8], [16, 9], [22, 10], [0, 10], [17, 7], [19, 6], [8, 6], [9, 6], [1, 1], [8, 6], [2, 6], [7, 6], [23, 7], [12, 7], [5, 4], [10, 6], [20, 8], [25, 9], [26, 10], [27, 11], [0, 11], [22, 7], [9, 5], [13, 6], [17, 6], [4, 5], [14, 6], [19, 7], [24, 7], [3, 6], [11, 6], [21, 6], [18, 6], [16, 6], [15, 6], [6, 3], [1, 1], [14, 7], [17, 7], [15, 7], [23, 9], [28, 10], [29, 11], [30, 13], [0, 13], [31, 12], [25, 8], [10, 5], [8, 4], [9, 4], [4, 4], [22, 8], [3, 8], [21, 8], [26, 9], [27, 9], [12, 6], [11, 5], [16, 7], [18, 7], [20, 8], [24, 8], [19, 7], [13, 5], [5, 3], [1, 2], [6, 3], [7, 3], [4, 4], [7, 4], [10, 4], [3, 10], [27, 10], [29, 10], [28, 10], [22, 8], [21, 7], [15, 6], [14, 5], [8, 4], [16, 6], [19, 7], [23, 8], [26, 9], [30, 10], [33, 13], [34, 14], [0, 14], [32, 12], [31, 11], [12, 5], [5, 3], [9, 3], [1, 4], [20, 7], [25, 8], [24, 8], [18, 6], [17, 5], [6, 3], [11, 4], [13, 4], [5, 3], [4, 3], [19, 8], [33, 12], [31, 12], [28, 11], [34, 14], [37, 14], [35, 15], [0, 15], [36, 14], [32, 12], [30, 11], [24, 9], [22, 8], [23, 9], [29, 10], [27, 10], [17, 6], [14, 5], [7, 4], [12, 5], [1, 6], [26, 9], [3, 9], [25, 8], [20, 7], [8, 4], [10, 4], [13, 4], [15, 6], [16, 6], [18, 6], [21, 6], [11, 4], [9, 3], [6, 3]];
/// `fft_level_exp_alt`
#[rustfmt::skip]
const FFT_LEVEL_EXP_ALT: [[u8; 2]; 28] = [[18, 3], [16, 3], [22, 7], [8, 10], [4, 10], [3, 9], [2, 8], [23, 8], [10, 8], [11, 7], [21, 5], [20, 4], [1, 7], [7, 10], [5, 10], [9, 9], [6, 10], [25, 11], [26, 12], [27, 13], [0, 13], [24, 9], [12, 6], [13, 5], [14, 4], [19, 3], [15, 3], [17, 2]];
/// `fft_level_exp`
#[rustfmt::skip]
const FFT_LEVEL_EXP: [[u8; 2]; 20] = [[3, 3], [11, 6], [16, 9], [17, 10], [18, 11], [19, 12], [0, 12], [15, 8], [14, 7], [9, 5], [7, 4], [2, 3], [4, 3], [1, 3], [5, 3], [12, 6], [13, 6], [10, 5], [8, 4], [6, 3]];
/// `fft_stereo_exp`
#[rustfmt::skip]
const FFT_STEREO_EXP: [[u8; 2]; 7] = [[2, 2], [3, 3], [4, 4], [5, 5], [6, 6], [0, 6], [1, 1]];
/// `fft_stereo_phase`
#[rustfmt::skip]
const FFT_STEREO_PHASE: [[u8; 2]; 9] = [[2, 2], [1, 2], [3, 4], [7, 4], [6, 5], [5, 6], [0, 6], [4, 4], [8, 2]];
/// `tab_fft_tone_offset_sizes`
#[rustfmt::skip]
const TAB_FFT_TONE_OFFSET_SIZES: [usize; 5] = [23, 28, 31, 34, 37];
/// `fft_cutoff_index_table`
#[rustfmt::skip]
const FFT_CUTOFF_INDEX_TABLE: [[i32; 2]; 4] = [[1, 2], [-1, 0], [-1, -2], [0, 0]];
/// `fft_level_index_table`
#[rustfmt::skip]
const FFT_LEVEL_INDEX_TABLE: [i16; 256] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5];
/// `last_coeff`
#[rustfmt::skip]
const LAST_COEFF: [u8; 3] = [4, 7, 10];
/// `coeff_per_sb_for_avg`
#[rustfmt::skip]
const COEFF_PER_SB_FOR_AVG: [[u8; 30]; 3] = [[0, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3], [0, 1, 2, 2, 3, 3, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5, 5, 5, 5, 5, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6], [0, 1, 2, 3, 4, 4, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8, 8, 8, 8, 9, 9, 9, 9, 9, 9, 9, 9]];
/// `dequant_table`
#[rustfmt::skip]
const DEQUANT_TABLE: [[[u32; 30]; 10]; 3] = [[[256, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 256, 256, 205, 154, 102, 51, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 51, 102, 154, 205, 256, 238, 219, 201, 183, 165, 146, 128, 110, 91, 73, 55, 37, 18, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 18, 37, 55, 73, 91, 110, 128, 146, 165, 183, 201, 219, 238, 256, 228, 199, 171, 142, 114, 85, 57, 28], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]], [[256, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 256, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 256, 171, 85, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 85, 171, 256, 171, 85, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 85, 171, 256, 219, 183, 146, 110, 73, 37, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 0, 37, 73, 110, 146, 183, 219, 256, 228, 199, 171, 142, 114, 85, 57, 28, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 28, 57, 85, 114, 142, 171, 199, 228, 256, 213, 171, 128, 85, 43], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]], [[256, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 256, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 256, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 256, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 256, 256, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 256, 171, 85, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 85, 171, 256, 192, 128, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 64, 128, 192, 256, 205, 154, 102, 51, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 51, 102, 154, 205, 256, 213, 171, 128, 85, 43, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 43, 85, 128, 171, 213, 256, 213, 171, 128, 85, 43]]];
/// `coeff_per_sb_for_dequant`
#[rustfmt::skip]
const COEFF_PER_SB_FOR_DEQUANT: [[u8; 30]; 3] = [[0, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3, 3], [0, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5, 5, 5, 5, 5, 5, 6, 6, 6, 6, 6, 6], [0, 1, 2, 3, 4, 4, 5, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 7, 8, 8, 8, 8, 8, 8, 9, 9, 9, 9, 9, 9]];
/// `coding_method_table`
#[rustfmt::skip]
const CODING_METHOD_TABLE: [[i8; 30]; 5] = [[34, 30, 24, 24, 16, 16, 16, 16, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10], [34, 30, 24, 24, 16, 16, 16, 16, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10], [34, 30, 30, 30, 24, 24, 16, 16, 16, 16, 16, 16, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10], [34, 34, 30, 30, 24, 24, 24, 24, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 10, 10, 10, 10, 10, 10, 10, 10], [34, 34, 30, 30, 30, 30, 30, 30, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16]];
/// `vlc_stage3_values`
#[rustfmt::skip]
const VLC_STAGE3_VALUES: [i32; 60] = [0, 1, 2, 3, 4, 6, 8, 10, 12, 16, 20, 24, 28, 36, 44, 52, 60, 76, 92, 108, 124, 156, 188, 220, 252, 316, 380, 444, 508, 636, 764, 892, 1020, 1276, 1532, 1788, 2044, 2556, 3068, 3580, 4092, 5116, 6140, 7164, 8188, 10236, 12284, 14332, 16380, 20476, 24572, 28668, 32764, 40956, 49148, 57340, 65532, 81916, 98300, 114684];
/// `fft_subpackets`
#[rustfmt::skip]
const FFT_SUBPACKETS: [u8; 32] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 0, 0];
/// `fft_tone_sample_table`
#[rustfmt::skip]
const FFT_TONE_SAMPLE_TABLE: [[[f32; 5]; 16]; 4] = [[[0.0100000000, -0.0037037037, -0.0020000000, -0.0069444444, -0.0018416207], [0.0416666667, 0.0000000000, 0.0000000000, -0.0208333333, -0.0123456791], [0.1250000000, 0.0558035709, 0.0330687836, -0.0164473690, -0.0097465888], [0.1562500000, 0.0625000000, 0.0370370370, -0.0062500000, -0.0037037037], [0.1996007860, 0.0781250000, 0.0462962948, 0.0022727272, 0.0013468013], [0.2000000000, 0.0625000000, 0.0370370373, 0.0208333333, 0.0074074073], [0.2127659619, 0.0555555556, 0.0329218097, 0.0208333333, 0.0123456791], [0.2173913121, 0.0473484844, 0.0280583613, 0.0347222239, 0.0205761325], [0.2173913121, 0.0347222239, 0.0205761325, 0.0473484844, 0.0280583613], [0.2127659619, 0.0208333333, 0.0123456791, 0.0555555556, 0.0329218097], [0.2000000000, 0.0208333333, 0.0074074073, 0.0625000000, 0.0370370370], [0.1996007860, 0.0022727272, 0.0013468013, 0.0781250000, 0.0462962948], [0.1562500000, -0.0062500000, -0.0037037037, 0.0625000000, 0.0370370370], [0.1250000000, -0.0164473690, -0.0097465888, 0.0558035709, 0.0330687836], [0.0416666667, -0.0208333333, -0.0123456791, 0.0000000000, 0.0000000000], [0.0100000000, -0.0069444444, -0.0018416207, -0.0037037037, -0.0020000000]], [[0.0050000000, -0.0200000000, 0.0125000000, -0.3030303030, 0.0020000000], [0.1041666642, 0.0400000000, -0.0250000000, 0.0333333333, -0.0200000000], [0.1250000000, 0.0100000000, 0.0142857144, -0.0500000007, -0.0200000000], [0.1562500000, -0.0006250000, -0.00049382716, -0.000625000, -0.00049382716], [0.1562500000, -0.0006250000, -0.00049382716, -0.000625000, -0.00049382716], [0.1250000000, -0.0500000000, -0.0200000000, 0.0100000000, 0.0142857144], [0.1041666667, 0.0333333333, -0.0200000000, 0.0400000000, -0.0250000000], [0.0050000000, -0.3030303030, 0.0020000001, -0.0200000000, 0.0125000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000]], [[0.1428571492, 0.1250000000, -0.0285714287, -0.0357142873, 0.0208333333], [0.1818181818, 0.0588235296, 0.0333333333, 0.0212765951, 0.0100000000], [0.1818181818, 0.0212765951, 0.0100000000, 0.0588235296, 0.0333333333], [0.1428571492, -0.0357142873, 0.0208333333, 0.1250000000, -0.0285714287], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000]], [[0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000], [0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000, 0.0000000000]]];
/// `fft_tone_level_table`
#[rustfmt::skip]
const FFT_TONE_LEVEL_TABLE: [[f32; 64]; 2] = [[0.17677669, 0.42677650, 0.60355347, 0.85355347, 1.20710683, 1.68359375, 2.37500000, 3.36718750, 4.75000000, 6.73437500, 9.50000000, 13.4687500, 19.0000000, 26.9375000, 38.0000000, 53.8750000, 76.0000000, 107.750000, 152.000000, 215.500000, 304.000000, 431.000000, 608.000000, 862.000000, 1216.00000, 1724.00000, 2432.00000, 3448.00000, 4864.00000, 6896.00000, 9728.00000, 13792.0000, 19456.0000, 27584.0000, 38912.0000, 55168.0000, 77824.0000, 110336.000, 155648.000, 220672.000, 311296.000, 441344.000, 622592.000, 882688.000, 1245184.00, 1765376.00, 2490368.00, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000], [0.59375000, 0.84179688, 1.18750000, 1.68359375, 2.37500000, 3.36718750, 4.75000000, 6.73437500, 9.50000000, 13.4687500, 19.0000000, 26.9375000, 38.0000000, 53.8750000, 76.0000000, 107.750000, 152.000000, 215.500000, 304.000000, 431.000000, 608.000000, 862.000000, 1216.00000, 1724.00000, 2432.00000, 3448.00000, 4864.00000, 6896.00000, 9728.00000, 13792.0000, 19456.0000, 27584.0000, 38912.0000, 55168.0000, 77824.0000, 110336.000, 155648.000, 220672.000, 311296.000, 441344.000, 622592.000, 882688.000, 1245184.00, 1765376.00, 2490368.00, 3530752.00, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000, 0.00000000]];
/// `fft_tone_envelope_table`
#[rustfmt::skip]
const FFT_TONE_ENVELOPE_TABLE: [[f32; 31]; 4] = [[0.009607375, 0.038060248, 0.084265202, 0.146446645, 0.222214907, 0.308658302, 0.402454883, 0.500000060, 0.597545207, 0.691341758, 0.777785182, 0.853553414, 0.915734828, 0.961939812, 0.990392685, 1.00000000, 0.990392625, 0.961939752, 0.915734768, 0.853553295, 0.777785063, 0.691341639, 0.597545087, 0.500000000, 0.402454853, 0.308658272, 0.222214878, 0.146446615, 0.084265172, 0.038060218, 0.009607345], [0.038060248, 0.146446645, 0.308658302, 0.500000060, 0.691341758, 0.853553414, 0.961939812, 1.00000000, 0.961939752, 0.853553295, 0.691341639, 0.500000000, 0.308658272, 0.146446615, 0.038060218, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000], [0.146446645, 0.500000060, 0.853553414, 1.00000000, 0.853553295, 0.500000000, 0.146446615, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000], [0.500000060, 1.00000000, 0.500000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000, 0.000000000]];
/// `sb_noise_attenuation`
#[rustfmt::skip]
const SB_NOISE_ATTENUATION: [f32; 32] = [0.0, 0.0, 0.3, 0.4, 0.5, 0.7, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];
/// `dequant_1bit`
#[rustfmt::skip]
const DEQUANT_1BIT: [[f32; 3]; 2] = [[-0.920000, 0.000000, 0.920000], [-0.890000, 0.000000, 0.890000]];
/// `type30_dequant`
#[rustfmt::skip]
const TYPE30_DEQUANT: [f32; 8] = [-1.0, -0.625, -0.291666656732559, 0.0, 0.25, 0.5, 0.75, 1.0];
/// `type34_delta`
#[rustfmt::skip]
const TYPE34_DELTA: [f32; 10] = [-1.0, -0.60947573184967, -0.333333343267441, -0.138071194291115, 0.0, 0.138071194291115, 0.333333343267441, 0.60947573184967, 1.0, 0.0];
/// `ff_mpa_enwindow` (mpegaudiodsp_data.c)
#[rustfmt::skip]
const MPA_ENWINDOW: [i32; 257] = [
    0, -1, -1, -1, -1, -1, -1, -2,
    -2, -2, -2, -3, -3, -4, -4, -5,
    -5, -6, -7, -7, -8, -9, -10, -11,
    -13, -14, -16, -17, -19, -21, -24, -26,
    -29, -31, -35, -38, -41, -45, -49, -53,
    -58, -63, -68, -73, -79, -85, -91, -97,
    -104, -111, -117, -125, -132, -139, -147, -154,
    -161, -169, -176, -183, -190, -196, -202, -208,
    213, 218, 222, 225, 227, 228, 228, 227,
    224, 221, 215, 208, 200, 189, 177, 163,
    146, 127, 106, 83, 57, 29, -2, -36,
    -72, -111, -153, -197, -244, -294, -347, -401,
    -459, -519, -581, -645, -711, -779, -848, -919,
    -991, -1064, -1137, -1210, -1283, -1356, -1428, -1498,
    -1567, -1634, -1698, -1759, -1817, -1870, -1919, -1962,
    -2001, -2032, -2057, -2075, -2085, -2087, -2080, -2063,
    2037, 2000, 1952, 1893, 1822, 1739, 1644, 1535,
    1414, 1280, 1131, 970, 794, 605, 402, 185,
    -45, -288, -545, -814, -1095, -1388, -1692, -2006,
    -2330, -2663, -3004, -3351, -3705, -4063, -4425, -4788,
    -5153, -5517, -5879, -6237, -6589, -6935, -7271, -7597,
    -7910, -8209, -8491, -8755, -8998, -9219, -9416, -9585,
    -9727, -9838, -9916, -9959, -9966, -9935, -9863, -9750,
    -9592, -9389, -9139, -8840, -8492, -8092, -7640, -7134,
    6574, 5959, 5288, 4561, 3776, 2935, 2037, 1082,
    70, -998, -2122, -3300, -4533, -5818, -7154, -8540,
    -9975, -11455, -12980, -14548, -16155, -17799, -19478, -21189,
    -22929, -24694, -26482, -28289, -30112, -31947, -33791, -35640,
    -37489, -39336, -41176, -43006, -44821, -46617, -48390, -50137,
    -51853, -53534, -55178, -56778, -58333, -59838, -61289, -62684,
    -64019, -65290, -66494, -67629, -68692, -69679, -70590, -71420,
    -72169, -72835, -73415, -73908, -74313, -74630, -74856, -74992,
    75038,
];

const SOFTCLIP_THRESHOLD: i32 = 27600;
const HARDCLIP_THRESHOLD: i32 = 35716;
const QDM2_MAX_FRAME_SIZE: i32 = 512;
const MPA_MAX_CHANNELS: usize = 2;
const MPA_FRAME_SIZE: i32 = 1152;
const SBLIMIT: usize = 32;
const FFT_TONES: usize = 1000;
const FFT_COEFS: usize = 1000;

/// `QDM2_SB_USED`: 8, 16 or 30.
fn sb_used(sub_sampling: i32) -> usize {
    if sub_sampling >= 2 { 30 } else { 8 << sub_sampling }
}

/// `av_log2`
fn av_log2(v: u32) -> i32 {
    31 - (v | 1).leading_zeros() as i32
}

/// The tables qdm2_tablegen.h and mpegaudiodsp compute at init.
struct Tables {
    softclip: Vec<u16>,
    noise_table: Vec<f32>,
    random_dequant_index: [[u8; 5]; 256],
    random_dequant_type24: [[u8; 3]; 128],
    noise_samples: [f32; 128],
    synth_window: Vec<f32>,
    level: Vlc,
    diff: Vlc,
    run: Vlc,
    fft_level_exp_alt: Vlc,
    fft_level_exp: Vlc,
    fft_stereo_exp: Vlc,
    fft_stereo_phase: Vlc,
    tone_level_idx_hi1: Vlc,
    tone_level_idx_mid: Vlc,
    tone_level_idx_hi2: Vlc,
    type30: Vlc,
    type34: Vlc,
    fft_tone_offset: Vec<Vlc>,
}

fn vlc(tab: &[[u8; 2]]) -> Vlc {
    Vlc::from_lengths(tab.iter().map(|&[sym, len]| (i32::from(sym), i32::from(len))), -1).expect("qdm2 VLC table")
}

static TABLES: LazyLock<Tables> = LazyLock::new(|| {
    // softclip_table_init
    let dfl = f64::from(SOFTCLIP_THRESHOLD - 32767);
    let delta = (1.0 / -dfl) as f32;
    let softclip = (0..=HARDCLIP_THRESHOLD - SOFTCLIP_THRESHOLD)
        .map(|i| {
            let v = (f64::from(i as f32 * delta).sin() * dfl) as i32;
            (SOFTCLIP_THRESHOLD - (v & 0xFFFF)) as u16
        })
        .collect();

    // rnd_table_init
    let delta = (1.0f64 / 16384.0) as f32;
    let mut noise_table = vec![0f32; 4096 + 20];
    let mut seed: u64 = 0;
    for n in noise_table.iter_mut().take(4096) {
        seed = seed.wrapping_mul(214013).wrapping_add(2531011);
        let x = (((seed as i32) >> 16) & 0x7FFF) as f32;
        *n = ((f64::from(delta * x) - 1.0) * 1.3) as f32;
    }
    let mut random_dequant_index = [[0u8; 5]; 256];
    for (i, row) in random_dequant_index.iter_mut().enumerate() {
        let (mut seed, mut ldw) = (81u32, i as u32);
        for v in row.iter_mut() {
            *v = (ldw / seed) as u8;
            ldw %= seed;
            seed /= 3;
        }
    }
    let mut random_dequant_type24 = [[0u8; 3]; 128];
    for (i, row) in random_dequant_type24.iter_mut().enumerate() {
        let (mut seed, mut ldw) = (25u32, i as u32);
        for v in row.iter_mut() {
            *v = (ldw / seed) as u8;
            ldw %= seed;
            seed /= 5;
        }
    }

    // init_noise_samples
    let mut noise_samples = [0f32; 128];
    let mut seed: u32 = 0;
    for n in noise_samples.iter_mut() {
        seed = seed.wrapping_mul(214013).wrapping_add(2531011);
        let x = ((seed >> 16) & 0x7fff) as f32;
        *n = (f64::from(delta * x) - 1.0) as f32;
    }

    // mpa_synth_init (float): ff_mpa_synth_window_float
    let mut synth_window = vec![0f32; 512 + 256];
    for i in 0..257 {
        let mut v = (f64::from(MPA_ENWINDOW[i] as f32) * (1.0 / (1u64 << (16 + 23)) as f64)) as f32;
        synth_window[i] = v;
        if i & 63 != 0 {
            v = -v;
        }
        if i != 0 {
            synth_window[512 - i] = v;
        }
    }
    for i in 0..8 {
        for j in 0..16 {
            synth_window[512 + 16 * i + j] = synth_window[64 * i + 32 - j];
        }
    }
    for i in 0..8 {
        for j in 0..16 {
            synth_window[512 + 128 + 16 * i + j] = synth_window[64 * i + 48 - j];
        }
    }

    let mut offset = 0;
    let fft_tone_offset = TAB_FFT_TONE_OFFSET_SIZES
        .iter()
        .map(|&n| {
            let table = vlc(&TAB_FFT_TONE_OFFSET[offset..offset + n]);
            offset += n;
            table
        })
        .collect();
    Tables {
        softclip,
        noise_table,
        random_dequant_index,
        random_dequant_type24,
        noise_samples,
        synth_window,
        level: vlc(&TAB_LEVEL),
        diff: vlc(&TAB_DIFF),
        run: vlc(&TAB_RUN),
        fft_level_exp_alt: vlc(&FFT_LEVEL_EXP_ALT),
        fft_level_exp: vlc(&FFT_LEVEL_EXP),
        fft_stereo_exp: vlc(&FFT_STEREO_EXP),
        fft_stereo_phase: vlc(&FFT_STEREO_PHASE),
        tone_level_idx_hi1: vlc(&TAB_TONE_LEVEL_IDX_HI1),
        tone_level_idx_mid: vlc(&TAB_TONE_LEVEL_IDX_MID),
        tone_level_idx_hi2: vlc(&TAB_TONE_LEVEL_IDX_HI2),
        type30: vlc(&TAB_TYPE30),
        type34: vlc(&TAB_TYPE34),
        fft_tone_offset,
    }
});

/// `qdm2_get_vlc`
fn get_vlc(gb: &mut GetBitsLe, vlc: &Vlc, flag: bool) -> i32 {
    // Symbol 0 (-1 with the offset) and a miss both mean: the value
    // follows in 1 to 8 explicit bits.
    let mut value = vlc.read(gb).unwrap_or(-1);
    if value < 0 {
        let n = gb.get(3) + 1;
        value = gb.get(n) as i32;
    }
    if flag {
        if value >= 60 {
            return 0;
        }
        let mut tmp = VLC_STAGE3_VALUES[value as usize];
        if value & !3 > 0 {
            tmp += gb.get((value >> 2) as u32) as i32;
        }
        value = tmp;
    }
    value
}

/// `qdm2_get_se_vlc`
fn get_se_vlc(vlc: &Vlc, gb: &mut GetBitsLe) -> i32 {
    let value = get_vlc(gb, vlc, false);
    if value & 1 != 0 { (value + 1) >> 1 } else { -(value >> 1) }
}

/// `qdm2_packet_checksum`
fn packet_checksum(data: &[u8], length: usize, mut value: i32) -> u16 {
    for i in 0..length {
        value = value.wrapping_sub(i32::from(data.get(i).copied().unwrap_or(0)));
    }
    (value & 0xffff) as u16
}

/// `init_get_bits8(buf + start, size)`: `None` where FFmpeg fails (a
/// negative or oversized size).
fn reader(buf: &[u8], start: usize, size: u32) -> Option<GetBitsLe<'_>> {
    let size = size as i32;
    if size < 0 || size > i32::MAX / 8 || i64::from(size) * 8 >= i64::from(i32::MAX) - 512 {
        return None;
    }
    Some(GetBitsLe::with_size(buf.get(start..).unwrap_or(&[]), size as usize))
}

#[derive(Clone, Copy, Default)]
struct SubPacket {
    kind: i32,
    size: u32,
    /// Where its data starts in the frame's bytes.
    data: usize,
}

/// `qdm2_decode_sub_packet_header`, the reader starting at `base` in the
/// frame's bytes.
fn decode_sub_packet_header(gb: &mut GetBitsLe, base: usize) -> SubPacket {
    let mut kind = gb.get(8) as i32;
    if kind == 0 {
        return SubPacket::default();
    }
    let mut size = gb.get(8);
    if kind & 0x80 != 0 {
        size <<= 8;
        size |= gb.get(8);
        kind &= 0x7f;
    }
    if kind == 0x7f {
        kind |= (gb.get(8) << 8) as i32;
    }
    SubPacket { kind, size, data: base + gb.bits_count() / 8 }
}

#[derive(Clone, Copy, Default)]
struct FftTone {
    level: f32,
    /// `complex`: channel and bin.
    ch: usize,
    bin: usize,
    /// `table`: `fft_tone_sample_table[duration][index]`.
    table: (usize, usize),
    phase: i32,
    phase_shift: i32,
    duration: i32,
    time_index: i16,
    cutoff: i16,
}

#[derive(Clone, Copy, Default)]
struct FftCoefficient {
    sub_packet: i16,
    channel: u8,
    offset: i16,
    exp: i16,
    phase: u8,
}

/// `QDM2Context`
struct Qdm2 {
    nb_channels: usize,
    channels: usize,
    group_size: i32,
    fft_size: usize,
    checksum_size: usize,
    group_order: i32,
    frame_size: usize,
    frequency_range: i32,
    sub_sampling: i32,
    coeff_per_sb_select: usize,
    cm_table_select: usize,

    sub_packets: [SubPacket; 16],
    list_b: Vec<usize>,
    list_d: Vec<usize>,

    fft_tones: Vec<FftTone>,
    fft_tone_start: usize,
    fft_tone_end: usize,
    fft_coefs: Vec<FftCoefficient>,
    fft_coefs_index: usize,
    fft_coefs_min_index: [i32; 5],
    fft_coefs_max_index: [i32; 5],
    fft_level_exp: [i32; 6],
    rdft: RdftC2r,
    fft_complex: [Vec<Complex>; MPA_MAX_CHANNELS],
    fft_temp: [Vec<Complex>; MPA_MAX_CHANNELS],

    output_buffer: Vec<f32>,

    synth_buf: [Vec<f32>; MPA_MAX_CHANNELS],
    synth_buf_offset: [usize; MPA_MAX_CHANNELS],
    /// `sb_samples[ch][128][SBLIMIT]`
    sb_samples: [Vec<[f32; SBLIMIT]>; MPA_MAX_CHANNELS],
    samples: Vec<f32>,

    tone_level: [[[f32; 64]; 30]; 2],
    /// `coding_method[ch][30][64]`, flat: FFmpeg's writes can run past a
    /// row into the next.
    coding_method: Vec<i8>,
    quantized_coeffs: [[[i8; 8]; 10]; 2],
    tone_level_idx_base: [[[i8; 8]; 30]; 2],
    tone_level_idx_hi1: [[[[i8; 8]; 8]; 3]; 2],
    tone_level_idx_mid: [[[i8; 8]; 26]; 2],
    tone_level_idx_hi2: [[i8; 26]; 2],
    tone_level_idx: [[[i8; 64]; 30]; 2],

    has_errors: bool,
    superblocktype_2_3: bool,
    do_synth_filter: bool,
    sub_packet: usize,
    noise_idx: usize,
}

/// The error FFmpeg's functions return: invalid data or a missing feature.
type R<T> = std::result::Result<T, Error>;

fn invalid(what: &str) -> Error {
    Error::invalid(format!("qdm2: {what}"))
}

impl Qdm2 {
    /// `qdm2_decode_init`; also the sample rate.
    fn new(extradata: &[u8]) -> Result<(Self, u32)> {
        if extradata.len() < 48 {
            return Err(invalid("extradata missing or truncated"));
        }
        let mut pos = 0;
        while extradata.len() - pos > 8 && &extradata[pos..pos + 8] != b"frmaQDM2" {
            pos += 1;
        }
        let left = extradata.len() - pos;
        if left < 44 {
            return Err(invalid(&format!("not enough extradata ({left})")));
        }
        pos += 8;
        let be32 = |at: usize| u32::from_be_bytes(extradata[at..at + 4].try_into().unwrap());
        let size = be32(pos) as usize;
        if size > extradata.len() - pos - 4 {
            return Err(invalid("extradata size too small"));
        }
        if &extradata[pos + 4..pos + 8] != b"QDCA" {
            return Err(invalid("invalid extradata, expecting QDCA"));
        }
        let channels = be32(pos + 12) as i32;
        if channels <= 0 || channels > MPA_MAX_CHANNELS as i32 {
            return Err(invalid("invalid number of channels"));
        }
        let channels = channels as usize;
        let sample_rate = be32(pos + 16);
        let bit_rate = i64::from(be32(pos + 20));
        let group_size = be32(pos + 24) as i32;
        let fft_size = be32(pos + 28) as i32;
        let checksum_size = be32(pos + 32) as i32;
        if checksum_size as u32 >= 1 << 28 || checksum_size <= 1 {
            return Err(invalid(&format!("data block size invalid ({checksum_size})")));
        }
        let fft_order = av_log2(fft_size as u32) + 1;
        if !(7..=9).contains(&fft_order) {
            return Err(Error::unsupported(format!("qdm2: unknown FFT order {fft_order}")));
        }
        let group_order = av_log2(group_size as u32) + 1;
        let frame_size = group_size / 16;
        if frame_size > QDM2_MAX_FRAME_SIZE {
            return Err(invalid("frame size"));
        }
        // FFmpeg fails every frame of a stream whose frames are empty.
        if frame_size <= 0 {
            return Err(invalid("frame size"));
        }
        let sub_sampling = fft_order - 7;
        let frequency_range = 255 / (1 << (2 - sub_sampling));
        if (frame_size * 4) >> sub_sampling > MPA_FRAME_SIZE {
            return Err(Error::unsupported("qdm2: large frames"));
        }
        let tmp: i64 = match sub_sampling * 2 + channels as i32 - 1 {
            0 => 40,
            1 => 48,
            2 => 56,
            3 => 72,
            4 => 80,
            5 => 100,
            _ => i64::from(sub_sampling),
        };
        let mut tmp_val = 0;
        for (factor, value) in [(1000, 1), (1440, 2), (1760, 3), (2240, 4)] {
            if tmp * factor < bit_rate {
                tmp_val = value;
            }
        }
        let coeff_per_sb_select = if bit_rate <= 8000 {
            0
        } else if bit_rate < 16000 {
            1
        } else {
            2
        };
        if fft_size != 1 << (fft_order - 1) {
            return Err(invalid(&format!("FFT size {fft_size} not power of 2")));
        }
        let fft_size = fft_size as usize;
        let q = Self {
            nb_channels: channels,
            channels,
            group_size,
            fft_size,
            checksum_size: checksum_size as usize,
            group_order,
            frame_size: frame_size as usize,
            frequency_range,
            sub_sampling,
            coeff_per_sb_select,
            cm_table_select: tmp_val,
            sub_packets: [SubPacket::default(); 16],
            list_b: Vec::new(),
            list_d: Vec::new(),
            fft_tones: vec![FftTone::default(); FFT_TONES],
            fft_tone_start: 0,
            fft_tone_end: 0,
            fft_coefs: vec![FftCoefficient::default(); FFT_COEFS],
            fft_coefs_index: 0,
            fft_coefs_min_index: [0; 5],
            fft_coefs_max_index: [0; 5],
            fft_level_exp: [0; 6],
            rdft: RdftC2r::new(2 * fft_size, 1.0 / 2.0),
            fft_complex: [vec![Complex::default(); 256 + 1], vec![Complex::default(); 256 + 1]],
            fft_temp: [vec![Complex::default(); 256], vec![Complex::default(); 256]],
            output_buffer: vec![0.0; QDM2_MAX_FRAME_SIZE as usize * MPA_MAX_CHANNELS * 2],
            synth_buf: [vec![0.0; 512 * 2], vec![0.0; 512 * 2]],
            synth_buf_offset: [0; MPA_MAX_CHANNELS],
            sb_samples: [vec![[0.0; SBLIMIT]; 128], vec![[0.0; SBLIMIT]; 128]],
            samples: vec![0.0; MPA_MAX_CHANNELS * MPA_FRAME_SIZE as usize],
            tone_level: [[[0.0; 64]; 30]; 2],
            coding_method: vec![0; 2 * 30 * 64],
            quantized_coeffs: [[[0; 8]; 10]; 2],
            tone_level_idx_base: [[[0; 8]; 30]; 2],
            tone_level_idx_hi1: [[[[0; 8]; 8]; 3]; 2],
            tone_level_idx_mid: [[[0; 8]; 26]; 2],
            tone_level_idx_hi2: [[0; 26]; 2],
            tone_level_idx: [[[0; 64]; 30]; 2],
            has_errors: false,
            superblocktype_2_3: false,
            do_synth_filter: false,
            sub_packet: 0,
            noise_idx: 0,
        };
        Ok((q, sample_rate))
    }

    fn cm(ch: usize, sb: usize, j: usize) -> usize {
        ch * 30 * 64 + sb * 64 + j
    }

    /// `SB_DITHERING_NOISE(sb, noise_idx)`
    fn dithering_noise(&mut self, sb: usize) -> f32 {
        let v = TABLES.noise_table[self.noise_idx] * SB_NOISE_ATTENUATION[sb];
        self.noise_idx += 1;
        v
    }

    /// `FIX_NOISE_IDX`
    fn fix_noise_idx(&mut self) {
        if self.noise_idx >= 3840 {
            self.noise_idx -= 3840;
        }
    }

    /// `average_quantized_coeffs`
    fn average_quantized_coeffs(&mut self) {
        let n = usize::from(COEFF_PER_SB_FOR_AVG[self.coeff_per_sb_select][sb_used(self.sub_sampling) - 1]) + 1;
        for ch in 0..self.nb_channels {
            for i in 0..n {
                let mut sum: i32 = self.quantized_coeffs[ch][i].iter().map(|&c| i32::from(c)).sum();
                sum /= 8;
                if sum > 0 {
                    sum -= 1;
                }
                self.quantized_coeffs[ch][i] = [sum as i8; 8];
            }
        }
    }

    /// `build_sb_samples_from_noise`
    fn build_sb_samples_from_noise(&mut self, sb: usize) -> R<()> {
        self.fix_noise_idx();
        if self.nb_channels == 0 {
            return Err(invalid("no channels"));
        }
        for ch in 0..self.nb_channels {
            for j in 0..64 {
                let a = self.dithering_noise(sb) * self.tone_level[ch][sb][j];
                self.sb_samples[ch][j * 2][sb] = a;
                let b = self.dithering_noise(sb) * self.tone_level[ch][sb][j];
                self.sb_samples[ch][j * 2 + 1][sb] = b;
            }
        }
        Ok(())
    }

    /// `fix_coding_method_array`: false where FFmpeg returns -1.
    fn fix_coding_method_array(&mut self, sb: usize, channels: usize) -> bool {
        for ch in 0..channels {
            let mut j = 0;
            while j < 64 {
                let cm = i32::from(self.coding_method[Self::cm(ch, sb, j)]);
                if cm < 8 {
                    return false;
                }
                let (run, case_val) = if cm - 8 > 22 {
                    (1, 8)
                } else {
                    match SWITCHTABLE[(cm - 8) as usize] {
                        0 => (10, 10),
                        1 => (1, 16),
                        2 => (5, 24),
                        3 => (3, 30),
                        4 => (1, 30),
                        _ => (1, 8),
                    }
                };
                for k in 0..run {
                    if j + k < 128 {
                        let sbjk = sb + (j + k) / 64;
                        if sbjk > 29 {
                            continue;
                        }
                        if self.coding_method[Self::cm(ch, sbjk, (j + k) % 64)] > self.coding_method[Self::cm(ch, sb, j)]
                            && k > 0
                        {
                            // FFmpeg's untested path: memset k, then 3,
                            // bytes from [ch][sb][j + k], past the row.
                            let at = Self::cm(ch, sb, j + k);
                            for n in [k, 3] {
                                let end = (at + n).min(self.coding_method.len());
                                self.coding_method[at.min(end)..end].fill(case_val as i8);
                            }
                        }
                    }
                }
                j += run;
            }
        }
        true
    }

    /// `fill_tone_level_array`
    fn fill_tone_level_array(&mut self, flag: bool) {
        let sel = self.coeff_per_sb_select;
        for ch in 0..self.nb_channels {
            for sb in 0..30 {
                for i in 0..8 {
                    let tab = usize::from(COEFF_PER_SB_FOR_DEQUANT[sel][sb]);
                    let term = |t: usize| {
                        i32::from(self.quantized_coeffs[ch][t][i]).wrapping_mul(DEQUANT_TABLE[sel][t][sb] as i32)
                    };
                    let mut tmp = if (tab as i32) < i32::from(LAST_COEFF[sel]) - 1 {
                        term(tab + 1).wrapping_add(term(tab))
                    } else {
                        term(tab)
                    };
                    if tmp < 0 {
                        tmp += 0xff;
                    }
                    self.tone_level_idx_base[ch][sb][i] = ((tmp / 256) & 0xff) as u8 as i8;
                }
            }
        }

        let sb_used = sb_used(self.sub_sampling);
        if self.superblocktype_2_3 && !flag {
            for sb in 0..sb_used {
                for ch in 0..self.nb_channels {
                    for i in 0..64 {
                        let idx = self.tone_level_idx_base[ch][sb][i / 8];
                        self.tone_level_idx[ch][sb][i] = idx;
                        self.tone_level[ch][sb][i] =
                            if idx < 0 { 0.0 } else { FFT_TONE_LEVEL_TABLE[0][(idx & 0x3f) as usize] };
                    }
                }
            }
            return;
        }
        let tab = if self.superblocktype_2_3 { 0 } else { 1 };
        let level = |tmp: i32, b23: bool| {
            if tmp < 0 || (!b23 && tmp == 0) { 0.0 } else { FFT_TONE_LEVEL_TABLE[tab][(tmp & 0x3f) as usize] }
        };
        for sb in 0..sb_used {
            for ch in 0..self.nb_channels {
                for i in 0..64 {
                    let base = i32::from(self.tone_level_idx_base[ch][sb][i / 8]);
                    let tmp = if (4..=23).contains(&sb) {
                        let tmp = base
                            - i32::from(self.tone_level_idx_hi1[ch][sb / 8][i / 8][i % 8])
                            - i32::from(self.tone_level_idx_mid[ch][sb - 4][i / 8])
                            - i32::from(self.tone_level_idx_hi2[ch][sb - 4]);
                        self.tone_level_idx[ch][sb][i] = (tmp & 0xff) as u8 as i8;
                        tmp
                    } else if sb > 4 {
                        let tmp = base
                            - i32::from(self.tone_level_idx_hi1[ch][2][i / 8][i % 8])
                            - i32::from(self.tone_level_idx_hi2[ch][sb - 4]);
                        self.tone_level_idx[ch][sb][i] = (tmp & 0xff) as u8 as i8;
                        tmp
                    } else {
                        self.tone_level_idx[ch][sb][i] = base as i8;
                        base
                    };
                    self.tone_level[ch][sb][i] = level(tmp, self.superblocktype_2_3);
                }
            }
        }
    }

    /// `fill_coding_method_array`
    fn fill_coding_method_array(&mut self) -> R<()> {
        if !self.superblocktype_2_3 {
            return Err(Error::unsupported("qdm2: !superblocktype_2_3"));
        }
        for ch in 0..self.nb_channels {
            for sb in 0..30 {
                let v = CODING_METHOD_TABLE[self.cm_table_select][sb];
                self.coding_method[Self::cm(ch, sb, 0)..Self::cm(ch, sb, 64)].fill(v);
            }
        }
        Ok(())
    }

    /// `synthfilt_build_sb_samples`
    fn synthfilt_build_sb_samples(&mut self, gb: &mut GetBitsLe, length: i32, sb_min: usize, sb_max: usize) -> R<()> {
        if length == 0 {
            for sb in sb_min..sb_max {
                self.build_sb_samples_from_noise(sb)?;
            }
            return Ok(());
        }
        let t = &*TABLES;
        let mut samples = [0f32; 10];
        let mut sign_bits = [0u32; 16];
        let mut type34_div = 0f32;
        for sb in sb_min..sb_max {
            let mut channels = self.nb_channels;
            let joined_stereo = if self.nb_channels <= 1 || sb < 12 {
                0
            } else if sb >= 24 {
                1
            } else if gb.bits_left() >= 1 {
                gb.get1() as usize
            } else {
                0
            };
            if joined_stereo != 0 {
                if gb.bits_left() >= 16 {
                    for s in &mut sign_bits {
                        *s = gb.get1();
                    }
                }
                for j in 0..64 {
                    let (c0, c1) = (Self::cm(0, sb, j), Self::cm(1, sb, j));
                    if self.coding_method[c1] > self.coding_method[c0] {
                        self.coding_method[c0] = self.coding_method[c1];
                    }
                }
                if !self.fix_coding_method_array(sb, self.nb_channels) {
                    self.build_sb_samples_from_noise(sb)?;
                    continue;
                }
                channels = 1;
            }

            for ch in 0..channels {
                self.fix_noise_idx();
                let zero_encoding = gb.bits_left() >= 1 && gb.get1() != 0;
                let mut type34_predictor = 0f32;
                let mut type34_first = true;
                let mut j = 0;
                while j < 128 {
                    let run;
                    match self.coding_method[Self::cm(ch, sb, j / 2)] {
                        8 => {
                            if gb.bits_left() >= 10 {
                                if zero_encoding {
                                    for k in 0..5 {
                                        if j + 2 * k >= 128 {
                                            break;
                                        }
                                        samples[2 * k] = if gb.get1() != 0 {
                                            DEQUANT_1BIT[joined_stereo][2 * gb.get1() as usize]
                                        } else {
                                            0.0
                                        };
                                    }
                                } else {
                                    let n = gb.get(8) as usize;
                                    if n >= 243 {
                                        return Err(invalid("invalid 8bit codeword"));
                                    }
                                    for k in 0..5 {
                                        samples[2 * k] =
                                            DEQUANT_1BIT[joined_stereo][usize::from(t.random_dequant_index[n][k])];
                                    }
                                }
                                for k in 0..5 {
                                    samples[2 * k + 1] = self.dithering_noise(sb);
                                }
                            } else {
                                for s in &mut samples {
                                    *s = self.dithering_noise(sb);
                                }
                            }
                            run = 10;
                        }
                        10 => {
                            if gb.bits_left() >= 1 {
                                let mut f = 0.81f64 as f32;
                                if gb.get1() != 0 {
                                    f = -f;
                                }
                                let ns = t.noise_samples[((sb + 1) * (j + 5 * ch + 1)) & 127];
                                f = (f64::from(f) - f64::from(ns) * 9.0 / 40.0) as f32;
                                samples[0] = f;
                            } else {
                                samples[0] = self.dithering_noise(sb);
                            }
                            run = 1;
                        }
                        16 => {
                            if gb.bits_left() >= 10 {
                                if zero_encoding {
                                    for k in 0..5 {
                                        if j + k >= 128 {
                                            break;
                                        }
                                        samples[k] = if gb.get1() == 0 {
                                            0.0
                                        } else {
                                            DEQUANT_1BIT[joined_stereo][2 * gb.get1() as usize]
                                        };
                                    }
                                } else {
                                    let n = gb.get(8) as usize;
                                    if n >= 243 {
                                        return Err(invalid("invalid 8bit codeword"));
                                    }
                                    for k in 0..5 {
                                        samples[k] = DEQUANT_1BIT[joined_stereo][usize::from(t.random_dequant_index[n][k])];
                                    }
                                }
                            } else {
                                for s in samples.iter_mut().take(5) {
                                    *s = self.dithering_noise(sb);
                                }
                            }
                            run = 5;
                        }
                        24 => {
                            if gb.bits_left() >= 7 {
                                let n = gb.get(7) as usize;
                                if n >= 125 {
                                    return Err(invalid("invalid 7bit codeword"));
                                }
                                for k in 0..3 {
                                    samples[k] = ((f64::from(t.random_dequant_type24[n][k]) - 2.0) * 0.5) as f32;
                                }
                            } else {
                                for s in samples.iter_mut().take(3) {
                                    *s = self.dithering_noise(sb);
                                }
                            }
                            run = 3;
                        }
                        30 => {
                            if gb.bits_left() >= 4 {
                                let index = get_vlc(gb, &t.type30, false) as u32 as usize;
                                if index >= TYPE30_DEQUANT.len() {
                                    return Err(invalid("index out of type30_dequant"));
                                }
                                samples[0] = TYPE30_DEQUANT[index];
                            } else {
                                samples[0] = self.dithering_noise(sb);
                            }
                            run = 1;
                        }
                        34 => {
                            if gb.bits_left() >= 7 {
                                if type34_first {
                                    type34_div = (1i32 << gb.get(2)) as f32;
                                    samples[0] = ((f64::from(gb.get(5) as f32) - 16.0) / 15.0) as f32;
                                    type34_predictor = samples[0];
                                    type34_first = false;
                                } else {
                                    let index = get_vlc(gb, &t.type34, false) as u32 as usize;
                                    if index >= TYPE34_DELTA.len() {
                                        return Err(invalid("index out of type34_delta"));
                                    }
                                    samples[0] = TYPE34_DELTA[index] / type34_div + type34_predictor;
                                    type34_predictor = samples[0];
                                }
                            } else {
                                samples[0] = self.dithering_noise(sb);
                            }
                            run = 1;
                        }
                        _ => {
                            samples[0] = self.dithering_noise(sb);
                            run = 1;
                        }
                    }

                    if joined_stereo != 0 {
                        let mut k = 0;
                        while k < run && j + k < 128 {
                            let jk = j + k;
                            self.sb_samples[0][jk][sb] = self.tone_level[0][sb][jk / 2] * samples[k];
                            if self.nb_channels == 2 {
                                self.sb_samples[1][jk][sb] = if sign_bits[jk / 8] != 0 {
                                    self.tone_level[1][sb][jk / 2] * -samples[k]
                                } else {
                                    self.tone_level[1][sb][jk / 2] * samples[k]
                                };
                            }
                            k += 1;
                        }
                    } else {
                        for k in 0..run {
                            if j + k < 128 {
                                self.sb_samples[ch][j + k][sb] = self.tone_level[ch][sb][(j + k) / 2] * samples[k];
                            }
                        }
                    }
                    j += run;
                }
            }
        }
        Ok(())
    }

    /// `init_quantized_coeffs_elem0` for channel `ch`.
    fn init_quantized_coeffs_elem0(&mut self, ch: usize, gb: &mut GetBitsLe) -> R<()> {
        let t = &*TABLES;
        if gb.bits_left() < 16 {
            return Err(invalid("short subpacket 10"));
        }
        let mut level = get_vlc(gb, &t.level, false);
        self.quantized_coeffs[ch][0][0] = level as i8;
        let mut i = 0;
        while i < 7 {
            if gb.bits_left() < 16 {
                return Err(invalid("short subpacket 10"));
            }
            let run = get_vlc(gb, &t.run, false) + 1;
            if i + run >= 8 {
                return Err(invalid("run"));
            }
            if gb.bits_left() < 16 {
                return Err(invalid("short subpacket 10"));
            }
            let diff = get_se_vlc(&t.diff, gb);
            for k in 1..=run {
                self.quantized_coeffs[ch][0][(i + k) as usize] = level.wrapping_add((k * diff) / run) as i8;
            }
            level += diff;
            i += run;
        }
        Ok(())
    }

    /// `init_tone_level_dequantization`
    fn init_tone_level_dequantization(&mut self, gb: &mut GetBitsLe) -> R<()> {
        let t = &*TABLES;
        for ch in 0..self.nb_channels {
            self.init_quantized_coeffs_elem0(ch, gb)?;
            if gb.bits_left() < 16 {
                self.quantized_coeffs[ch][0] = [0; 8];
                break;
            }
        }

        let n = (self.sub_sampling + 1) as usize;
        for sb in 0..n {
            for ch in 0..self.nb_channels {
                for j in 0..8 {
                    if gb.bits_left() < 1 {
                        break;
                    }
                    if gb.get1() != 0 {
                        for k in 0..8 {
                            if gb.bits_left() < 16 {
                                break;
                            }
                            self.tone_level_idx_hi1[ch][sb][j][k] = get_vlc(gb, &t.tone_level_idx_hi1, false) as i8;
                        }
                    } else {
                        self.tone_level_idx_hi1[ch][sb][j] = [0; 8];
                    }
                }
            }
        }

        let n = sb_used(self.sub_sampling) - 4;
        for sb in 0..n {
            for ch in 0..self.nb_channels {
                if gb.bits_left() < 16 {
                    break;
                }
                let v = get_vlc(gb, &t.tone_level_idx_hi2, false) as i8;
                self.tone_level_idx_hi2[ch][sb] = v;
                if sb > 19 {
                    self.tone_level_idx_hi2[ch][sb] = v.wrapping_sub(16);
                } else {
                    self.tone_level_idx_mid[ch][sb] = [-16; 8];
                }
            }
        }

        let n = sb_used(self.sub_sampling) - 5;
        for sb in 0..n {
            for ch in 0..self.nb_channels {
                for j in 0..8 {
                    if gb.bits_left() < 16 {
                        break;
                    }
                    self.tone_level_idx_mid[ch][sb][j] = (get_vlc(gb, &t.tone_level_idx_mid, false) - 32) as i8;
                }
            }
        }
        Ok(())
    }

    /// `process_subpacket_9`
    fn process_subpacket_9(&mut self, buf: &[u8], node: SubPacket) -> R<()> {
        let t = &*TABLES;
        let mut gb = reader(buf, node.data, node.size).ok_or_else(|| invalid("subpacket 9 size"))?;
        let n = usize::from(COEFF_PER_SB_FOR_AVG[self.coeff_per_sb_select][sb_used(self.sub_sampling) - 1]) + 1;
        for i in 1..n {
            for ch in 0..self.nb_channels {
                let mut level = get_vlc(&mut gb, &t.level, false);
                self.quantized_coeffs[ch][i][0] = level as i8;
                let mut j = 0;
                while j < 8 - 1 {
                    let run = get_vlc(&mut gb, &t.run, false) + 1;
                    let diff = get_se_vlc(&t.diff, &mut gb);
                    if j + run >= 8 {
                        return Err(invalid("run in subpacket 9"));
                    }
                    for k in 1..=run {
                        self.quantized_coeffs[ch][i][(j + k) as usize] = level.wrapping_add((k * diff) / run) as i8;
                    }
                    level += diff;
                    j += run;
                }
            }
        }
        for ch in 0..self.nb_channels {
            self.quantized_coeffs[ch][0] = [0; 8];
        }
        Ok(())
    }

    /// `process_subpacket_10`
    fn process_subpacket_10(&mut self, buf: &[u8], node: Option<SubPacket>) -> R<()> {
        match node {
            Some(node) => {
                let mut gb = reader(buf, node.data, node.size).ok_or_else(|| invalid("subpacket 10 size"))?;
                self.init_tone_level_dequantization(&mut gb)?;
                self.fill_tone_level_array(true);
            }
            None => self.fill_tone_level_array(false),
        }
        Ok(())
    }

    /// `process_subpacket_11`
    fn process_subpacket_11(&mut self, buf: &[u8], node: Option<SubPacket>) -> R<()> {
        let mut gb = GetBitsLe::new(&[]);
        let mut length = 0;
        if let Some(node) = node {
            gb = reader(buf, node.data, node.size).ok_or_else(|| invalid("subpacket 11 size"))?;
            length = node.size.wrapping_mul(8) as i32;
        }
        if length >= 32 {
            let c = gb.get(13);
            if c > 3 {
                self.fill_coding_method_array()?;
            }
        }
        self.synthfilt_build_sb_samples(&mut gb, length, 0, 8)
    }

    /// `process_subpacket_12`. FFmpeg opens its reader with a size of 0
    /// (`length` before it is set), so every read is skipped: the
    /// subbands get noise.
    fn process_subpacket_12(&mut self, buf: &[u8], node: Option<SubPacket>) -> R<()> {
        let mut gb = GetBitsLe::new(&[]);
        let mut length = 0;
        if let Some(node) = node {
            gb = reader(buf, node.data, 0).ok_or_else(|| invalid("subpacket 12 size"))?;
            length = node.size.wrapping_mul(8) as i32;
        }
        let sb_max = sb_used(self.sub_sampling);
        self.synthfilt_build_sb_samples(&mut gb, length, 8, sb_max)
    }

    /// `qdm2_search_subpacket_type_in_list` on list D.
    fn find_d(&self, kind: i32) -> Option<SubPacket> {
        self.list_d.iter().map(|&i| self.sub_packets[i]).find(|p| p.kind == kind)
    }

    /// `process_synthesis_subpackets`
    fn process_synthesis_subpackets(&mut self, buf: &[u8]) -> R<()> {
        let nodes = [self.find_d(9), self.find_d(10), self.find_d(11), self.find_d(12)];
        if let Some(node) = nodes[0] {
            self.process_subpacket_9(buf, node)?;
        }
        self.process_subpacket_10(buf, nodes[1])?;
        let with_9_10 = nodes[0].is_some() && nodes[1].is_some();
        self.process_subpacket_11(buf, nodes[2].filter(|_| with_9_10))?;
        self.process_subpacket_12(buf, nodes[3].filter(|_| with_9_10))
    }

    /// `qdm2_decode_super_block`; `buf` is the frame's bytes, of which the
    /// first `checksum_size` are the superblock.
    fn decode_super_block(&mut self, buf: &[u8]) -> R<()> {
        let t = &*TABLES;
        self.tone_level_idx_hi1 = [[[[0; 8]; 8]; 3]; 2];
        self.tone_level_idx_mid = [[[0; 8]; 26]; 2];
        self.tone_level_idx_hi2 = [[0; 26]; 2];
        self.list_b.clear();
        self.list_d.clear();

        self.average_quantized_coeffs();

        let compressed_size = self.checksum_size;
        let mut gb = reader(buf, 0, compressed_size as u32).ok_or_else(|| invalid("superblock size"))?;
        let header = decode_sub_packet_header(&mut gb, 0);
        if header.kind < 2 || header.kind >= 8 {
            self.has_errors = true;
            return Err(invalid("bad superblock type"));
        }
        self.superblocktype_2_3 = header.kind == 2 || header.kind == 3;
        let mut packet_bytes = compressed_size as i32 - (gb.bits_count() / 8) as i32;

        let mut gb = reader(buf, header.data, header.size).ok_or_else(|| invalid("superblock header size"))?;
        if header.kind == 2 || header.kind == 4 || header.kind == 5 {
            let mut csum = 257 * gb.get(8) as i32;
            csum += 2 * gb.get(8) as i32;
            if packet_checksum(buf, self.checksum_size, csum) != 0 {
                self.has_errors = true;
                return Err(invalid("bad packet checksum"));
            }
        }

        for e in &mut self.fft_level_exp {
            *e -= 1;
            if *e < 0 {
                *e = 0;
            }
        }

        let mut next_index: u32 = 0;
        let mut i = 0;
        while packet_bytes > 0 {
            if i >= self.sub_packets.len() {
                return Err(Error::unsupported("qdm2: too many packet bytes"));
            }
            if i > 0 {
                gb = reader(buf, header.data, header.size).ok_or_else(|| invalid("superblock header size"))?;
                gb.skip(next_index as usize * 8);
                if next_index >= header.size {
                    break;
                }
            }
            let mut packet = decode_sub_packet_header(&mut gb, header.data);
            next_index = packet.size.wrapping_add((gb.bits_count() / 8) as u32);
            let sub_packet_size = (u32::from(packet.size > 0xff) + packet.size + 2) as i32;
            if packet.kind == 0 {
                break;
            }
            if sub_packet_size > packet_bytes {
                if packet.kind != 10 && packet.kind != 11 && packet.kind != 12 {
                    break;
                }
                packet.size = packet.size.wrapping_add((packet_bytes - sub_packet_size) as u32);
            }
            packet_bytes -= sub_packet_size;
            self.sub_packets[i] = packet;

            match packet.kind {
                8 => return Err(Error::unsupported("qdm2: packet type 8")),
                9..=12 => self.list_d.push(i),
                13 => {
                    for e in &mut self.fft_level_exp {
                        *e = gb.get(6) as i32;
                    }
                }
                14 => {
                    for e in &mut self.fft_level_exp {
                        *e = get_vlc(&mut gb, &t.fft_level_exp, false);
                    }
                }
                15 => return Err(Error::unsupported("qdm2: packet type 15")),
                16..=47 if FFT_SUBPACKETS[(packet.kind - 16) as usize] == 0 => self.list_b.push(i),
                _ => {}
            }
            i += 1;
        }

        if !self.list_d.is_empty() {
            self.process_synthesis_subpackets(buf)?;
            self.do_synth_filter = true;
        } else if self.do_synth_filter {
            self.process_subpacket_10(buf, None)?;
            self.process_subpacket_11(buf, None)?;
            self.process_subpacket_12(buf, None)?;
        }
        Ok(())
    }

    /// `qdm2_fft_init_coefficient`
    fn fft_init_coefficient(&mut self, sub_packet: i32, offset: i32, duration: usize, channel: i32, exp: i32, phase: i32) {
        if self.fft_coefs_min_index[duration] < 0 {
            self.fft_coefs_min_index[duration] = self.fft_coefs_index as i32;
        }
        self.fft_coefs[self.fft_coefs_index] = FftCoefficient {
            sub_packet: (if sub_packet >= 16 { sub_packet - 16 } else { sub_packet }) as i16,
            channel: channel as u8,
            offset: offset as i16,
            exp: exp as i16,
            phase: phase as u8,
        };
        self.fft_coefs_index += 1;
    }

    /// `qdm2_fft_decode_tones`
    fn fft_decode_tones(&mut self, duration: i32, gb: &mut GetBitsLe, b: bool) -> R<()> {
        let t = &*TABLES;
        let mut local_int_4 = 0i32;
        let mut local_int_28 = 0i32;
        let local_int_20 = 2i32;
        let local_int_8 = 4 - duration;
        let local_int_10 = 1i32 << (self.group_order - duration - 1);
        let mut offset = 1i32;
        let offset_vlc = &t.fft_tone_offset[local_int_8 as usize];

        while gb.bits_left() > 0 {
            if self.superblocktype_2_3 {
                let mut n;
                loop {
                    n = get_vlc(gb, offset_vlc, true);
                    if n >= 2 {
                        break;
                    }
                    if gb.bits_left() < 0 {
                        return Err(invalid("overread in fft_decode_tones"));
                    }
                    // Unbounded until the loop ends (FFmpeg checks the
                    // position after it), so these wrap as C's ints do.
                    offset = 1;
                    if n == 0 {
                        local_int_4 = local_int_4.wrapping_add(local_int_10);
                        local_int_28 = local_int_28.wrapping_add(1 << local_int_8);
                    } else {
                        local_int_4 = local_int_4.wrapping_add(8 * local_int_10);
                        local_int_28 = local_int_28.wrapping_add(8 << local_int_8);
                    }
                }
                offset += n - 2;
            } else {
                if local_int_10 <= 2 {
                    return Err(invalid("fft_decode_tones stuck"));
                }
                offset += get_vlc(gb, offset_vlc, true);
                while offset >= local_int_10 - 1 {
                    offset += 1 - (local_int_10 - 1);
                    local_int_4 += local_int_10;
                    local_int_28 += 1 << local_int_8;
                }
            }

            if local_int_4 >= self.group_size {
                return Err(invalid("tone position"));
            }
            let local_int_14 = offset >> local_int_8;
            if local_int_14 < 0 || local_int_14 as usize >= FFT_LEVEL_INDEX_TABLE.len() {
                return Err(invalid("tone offset"));
            }
            let (channel, stereo) = if self.nb_channels > 1 { (gb.get1() as i32, gb.get1() != 0) } else { (0, false) };

            let mut exp = get_vlc(gb, if b { &t.fft_level_exp } else { &t.fft_level_exp_alt }, false);
            exp += self.fft_level_exp[FFT_LEVEL_INDEX_TABLE[local_int_14 as usize] as usize];
            exp = exp.max(0);
            let phase = gb.get(3) as i32;
            let (mut stereo_exp, mut stereo_phase) = (0, 0);
            if stereo {
                stereo_exp = exp - get_vlc(gb, &t.fft_stereo_exp, false);
                stereo_phase = phase - get_vlc(gb, &t.fft_stereo_phase, false);
                if stereo_phase < 0 {
                    stereo_phase += 8;
                }
            }

            if self.frequency_range > local_int_14 + 1 {
                let sub_packet = local_int_20.wrapping_add(local_int_28);
                if self.fft_coefs_index + usize::from(stereo) >= FFT_COEFS {
                    return Err(invalid("too many tones"));
                }
                let d = duration as usize;
                self.fft_init_coefficient(sub_packet, offset, d, channel, exp, phase);
                if stereo {
                    self.fft_init_coefficient(sub_packet, offset, d, 1 - channel, stereo_exp, stereo_phase);
                }
            }
            offset += 1;
        }
        Ok(())
    }

    /// `qdm2_decode_fft_packets`
    fn decode_fft_packets(&mut self, buf: &[u8]) -> R<()> {
        if self.list_b.is_empty() {
            return Err(invalid("no FFT subpackets"));
        }
        self.fft_coefs_index = 0;
        self.fft_coefs_min_index = [-1; 5];

        let mut max = 256;
        for i in 0..self.list_b.len() {
            let mut min = 0;
            let mut packet = None;
            for &p in &self.list_b {
                let value = self.sub_packets[p].kind;
                if value > min && value < max {
                    min = value;
                    packet = Some(self.sub_packets[p]);
                }
            }
            max = min;
            let Some(packet) = packet else { return Err(invalid("FFT subpacket order")) };
            let fft_sub = |kind: i32| (16..48).contains(&kind) && FFT_SUBPACKETS[(kind - 16) as usize] == 0;
            if i == 0 && !fft_sub(packet.kind) {
                return Err(invalid("FFT subpacket type"));
            }
            let mut gb = reader(buf, packet.data, packet.size).ok_or_else(|| invalid("FFT subpacket size"))?;
            let unknown_flag = (32..48).contains(&packet.kind) && FFT_SUBPACKETS[(packet.kind - 16) as usize] == 0;
            let kind = packet.kind;
            // FFmpeg ignores what qdm2_fft_decode_tones returns.
            if (17..24).contains(&kind) || (33..40).contains(&kind) {
                let duration = self.sub_sampling + 5 - (kind & 15);
                if (0..4).contains(&duration) {
                    let _ = self.fft_decode_tones(duration, &mut gb, unknown_flag);
                }
            } else if kind == 31 {
                for j in 0..4 {
                    let _ = self.fft_decode_tones(j, &mut gb, unknown_flag);
                }
            } else if kind == 46 {
                for e in &mut self.fft_level_exp {
                    *e = gb.get(6) as i32;
                }
                for j in 0..4 {
                    let _ = self.fft_decode_tones(j, &mut gb, unknown_flag);
                }
            }
        }

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

    /// `qdm2_fft_generate_tone`
    fn fft_generate_tone(&mut self, mut tone: FftTone) {
        let iscale = 2.0 * std::f64::consts::PI / 512.0;
        tone.phase = tone.phase.wrapping_add(tone.phase_shift);
        let level = FFT_TONE_ENVELOPE_TABLE[tone.duration as usize][tone.time_index as usize] * tone.level;
        let c_im = (f64::from(level) * (f64::from(tone.phase) * iscale).sin()) as f32;
        let c_re = (f64::from(level) * (f64::from(tone.phase) * iscale).cos()) as f32;
        let bins = &mut self.fft_complex[tone.ch];
        let at = |k: i32| (tone.bin as i32 + k) as usize;
        if tone.duration >= 3 || tone.cutoff >= 3 {
            bins[at(0)].im += c_im;
            bins[at(0)].re += c_re;
            bins[at(1)].im -= c_im;
            bins[at(1)].re -= c_re;
        } else {
            let table = &FFT_TONE_SAMPLE_TABLE[tone.table.0][tone.table.1];
            let f = [
                table[3] - table[0],
                -table[4],
                (1.0 - f64::from(table[2]) - f64::from(table[3])) as f32,
                (f64::from(table[1] + table[4]) - 1.0) as f32,
                table[0] - table[1],
                table[2],
            ];
            for i in 0..2 {
                let k = at(FFT_CUTOFF_INDEX_TABLE[tone.cutoff as usize][i]);
                bins[k].re = c_re.mul_add(f[i], bins[k].re);
                let g = if i32::from(tone.cutoff) <= i as i32 { -f[i] } else { f[i] };
                bins[k].im = c_im.mul_add(g, bins[k].im);
            }
            for i in 0..4 {
                bins[at(i)].re = c_re.mul_add(f[i as usize + 2], bins[at(i)].re);
                bins[at(i)].im = c_im.mul_add(f[i as usize + 2], bins[at(i)].im);
            }
        }
        tone.time_index += 1;
        if i32::from(tone.time_index) < (1 << (5 - tone.duration)) - 1 {
            self.fft_tones[self.fft_tone_end] = tone;
            self.fft_tone_end = (self.fft_tone_end + 1) % FFT_TONES;
        }
    }

    /// `qdm2_fft_tone_synthesizer`
    fn fft_tone_synthesizer(&mut self, sub_packet: usize) {
        let iscale = 0.25 * std::f64::consts::PI;
        for ch in 0..self.channels {
            self.fft_complex[ch][..self.fft_size].fill(Complex::default());
        }
        let level_table = if self.superblocktype_2_3 { 0 } else { 1 };

        // FFT tones with duration 4 (1 FFT period)
        if self.fft_coefs_min_index[4] >= 0 {
            for i in self.fft_coefs_min_index[4]..self.fft_coefs_max_index[4] {
                let c = self.fft_coefs[i as usize];
                if i32::from(c.sub_packet) != sub_packet as i32 {
                    break;
                }
                let ch = if self.channels == 1 { 0 } else { usize::from(c.channel) };
                let level = if c.exp < 0 { 0.0 } else { FFT_TONE_LEVEL_TABLE[level_table][(c.exp & 63) as usize] };
                let re = (f64::from(level) * (f64::from(c.phase) * iscale).cos()) as f32;
                let im = (f64::from(level) * (f64::from(c.phase) * iscale).sin()) as f32;
                let at = c.offset as usize;
                self.fft_complex[ch][at].re += re;
                self.fft_complex[ch][at].im += im;
                self.fft_complex[ch][at + 1].re -= re;
                self.fft_complex[ch][at + 1].im -= im;
            }
        }

        // existing FFT tones
        let end = self.fft_tone_end;
        while end != self.fft_tone_start {
            let tone = self.fft_tones[self.fft_tone_start];
            self.fft_generate_tone(tone);
            self.fft_tone_start = (self.fft_tone_start + 1) % FFT_TONES;
        }

        // new FFT tones with duration 0 (long) to 3 (short)
        for i in 0..4usize {
            if self.fft_coefs_min_index[i] < 0 {
                continue;
            }
            let mut j = self.fft_coefs_min_index[i];
            while j < self.fft_coefs_max_index[i] {
                let c = self.fft_coefs[j as usize];
                if i32::from(c.sub_packet) != sub_packet as i32 {
                    break;
                }
                let four_i = 4 - i as i32;
                let offset = i32::from(c.offset) >> four_i;
                let ch = if self.channels == 1 { 0 } else { usize::from(c.channel) };
                if offset < self.frequency_range {
                    let cutoff = if offset < 2 { offset } else if offset >= 60 { 3 } else { 2 };
                    let tone = FftTone {
                        level: if c.exp < 0 { 0.0 } else { FFT_TONE_LEVEL_TABLE[level_table][(c.exp & 63) as usize] },
                        ch,
                        bin: offset as usize,
                        table: (i, (i32::from(c.offset) - (offset << four_i)) as usize),
                        phase: 64 * i32::from(c.phase) - (offset << 8) - 128,
                        phase_shift: (2 * i32::from(c.offset) + 1) << (7 - four_i),
                        duration: i as i32,
                        time_index: 0,
                        cutoff: cutoff as i16,
                    };
                    self.fft_generate_tone(tone);
                }
                j += 1;
            }
            self.fft_coefs_min_index[i] = j;
        }
    }

    /// `qdm2_calculate_fft`
    fn calculate_fft(&mut self, channel: usize) {
        let gain = if self.channels == 1 && self.nb_channels == 2 { 0.5f32 } else { 1.0 };
        let fft_size = self.fft_size;
        let complex = &mut self.fft_complex[channel];
        complex[0].re *= 2.0;
        complex[0].im = 0.0;
        complex[fft_size] = Complex::default();
        self.rdft.run(&mut self.fft_temp[channel], complex);
        let mut out = channel;
        for i in 0..fft_size.next_multiple_of(8) {
            let tmp = self.fft_temp[channel][i];
            self.output_buffer[out] = tmp.re.mul_add(gain, self.output_buffer[out]);
            self.output_buffer[out + self.channels] = tmp.im.mul_add(gain, self.output_buffer[out + self.channels]);
            out += 2 * self.channels;
        }
    }

    /// `qdm2_synthesis_filter`
    fn synthesis_filter(&mut self, index: usize) {
        let sb_used = sb_used(self.sub_sampling);
        for ch in 0..self.channels {
            for i in 0..8 {
                self.sb_samples[ch][8 * index + i][sb_used..SBLIMIT].fill(0.0);
            }
        }
        let window = &TABLES.synth_window;
        let mut dither_state = 0i32;
        for ch in 0..self.nb_channels {
            let mut at = ch;
            for i in 0..8 {
                let sb = self.sb_samples[ch][8 * index + i];
                synth_filter(
                    &mut self.synth_buf[ch],
                    &mut self.synth_buf_offset[ch],
                    window,
                    &mut dither_state,
                    &mut self.samples[at..],
                    self.nb_channels,
                    &sb,
                );
                at += 32 * self.nb_channels;
            }
        }
        let sub_sampling = (4 >> self.sub_sampling) as usize;
        for ch in 0..self.channels {
            for i in 0..self.frame_size {
                let o = self.channels * i + ch;
                let s = self.samples[self.nb_channels * sub_sampling * i + ch];
                self.output_buffer[o] = ((1 << 23) as f32).mul_add(s, self.output_buffer[o]);
            }
        }
    }

    /// `qdm2_decode`: one of the 16 frames of the superblock in `buf`.
    fn decode(&mut self, buf: &[u8], out: &mut [i16]) -> R<()> {
        let frame_size = self.frame_size * self.channels;
        if frame_size > self.output_buffer.len() / 2 {
            return Err(invalid("frame size"));
        }
        self.output_buffer.copy_within(frame_size..2 * frame_size, 0);
        self.output_buffer[frame_size..2 * frame_size].fill(0.0);

        if self.sub_packet == 0 {
            self.has_errors = false;
            self.decode_super_block(buf)?;
        }
        if !self.has_errors {
            if self.sub_packet == 2 {
                self.decode_fft_packets(buf)?;
            }
            self.fft_tone_synthesizer(self.sub_packet);
        }
        for ch in 0..self.channels {
            self.calculate_fft(ch);
        }
        if !self.has_errors && self.do_synth_filter {
            self.synthesis_filter(self.sub_packet);
        }
        self.sub_packet = (self.sub_packet + 1) % 16;

        let softclip = &TABLES.softclip;
        for (o, &v) in out[..frame_size].iter_mut().zip(&self.output_buffer[..frame_size]) {
            let mut value = v as i32;
            if value > SOFTCLIP_THRESHOLD {
                value = if value > HARDCLIP_THRESHOLD {
                    32767
                } else {
                    i32::from(softclip[(value - SOFTCLIP_THRESHOLD) as usize])
                };
            } else if value < -SOFTCLIP_THRESHOLD {
                value = if value < -HARDCLIP_THRESHOLD {
                    -32767
                } else {
                    -i32::from(softclip[(-value - SOFTCLIP_THRESHOLD) as usize])
                };
            }
            *o = value as i16;
        }
        Ok(())
    }
}

/// `switchtable`
const SWITCHTABLE: [i32; 23] = [0, 5, 1, 5, 5, 5, 5, 5, 2, 5, 5, 5, 5, 5, 5, 5, 3, 5, 5, 5, 5, 5, 4];

/// `ff_dct32_float` (dct32_template.c), as clang builds it: no
/// multiply-add to fuse.
fn dct32(out: &mut [f32], tab: &[f32; 32]) {
    const COS0: [f64; 16] = [
        0.50060299823519630134 / 2.0,
        0.50547095989754365998 / 2.0,
        0.51544730992262454697 / 2.0,
        0.53104259108978417447 / 2.0,
        0.55310389603444452782 / 2.0,
        0.58293496820613387367 / 2.0,
        0.62250412303566481615 / 2.0,
        0.67480834145500574602 / 2.0,
        0.74453627100229844977 / 2.0,
        0.83934964541552703873 / 2.0,
        0.97256823786196069369 / 2.0,
        1.16943993343288495515 / 4.0,
        1.48416461631416627724 / 4.0,
        2.05778100995341155085 / 8.0,
        3.40760841846871878570 / 8.0,
        10.19000812354805681150 / 32.0,
    ];
    const COS1: [f64; 8] = [
        0.50241928618815570551 / 2.0,
        0.52249861493968888062 / 2.0,
        0.56694403481635770368 / 2.0,
        0.64682178335999012954 / 2.0,
        0.78815462345125022473 / 2.0,
        1.06067768599034747134 / 4.0,
        1.72244709823833392782 / 4.0,
        5.10114861868916385802 / 16.0,
    ];
    const COS2: [f64; 4] = [
        0.50979557910415916894 / 2.0,
        0.60134488693504528054 / 2.0,
        0.89997622313641570463 / 2.0,
        2.56291544774150617881 / 8.0,
    ];
    const COS3: [f64; 2] = [0.54119610014619698439 / 2.0, 1.30656296487637652785 / 4.0];
    let c0 = |i: usize| COS0[i] as f32;
    let c1 = |i: usize| COS1[i] as f32;
    let c2 = |i: usize| COS2[i] as f32;
    let c3 = |i: usize| COS3[i] as f32;
    let cos4 = (std::f64::consts::FRAC_1_SQRT_2 / 2.0) as f32;

    let mut v = [0f32; 32];
    // BF(a, b, c, s): a + b, then ((1 << s) * c) * (a - b).
    let bf = |v: &mut [f32; 32], a: usize, b: usize, c: f32, s: u32| {
        let (t0, t1) = (v[a] + v[b], v[a] - v[b]);
        v[a] = t0;
        v[b] = ((1i32 << s) as f32 * c) * t1;
    };
    let bf0 = |v: &mut [f32; 32], a: usize, b: usize, c: f32, s: u32| {
        let (t0, t1) = (tab[a] + tab[b], tab[a] - tab[b]);
        v[a] = t0;
        v[b] = ((1i32 << s) as f32 * c) * t1;
    };
    let bf1 = |v: &mut [f32; 32], a: usize, b: usize, c: usize, d: usize| {
        bf(v, a, b, cos4, 1);
        bf(v, c, d, -cos4, 1);
        v[c] += v[d];
    };
    let bf2 = |v: &mut [f32; 32], a: usize, b: usize, c: usize, d: usize| {
        bf(v, a, b, cos4, 1);
        bf(v, c, d, -cos4, 1);
        v[c] += v[d];
        v[a] += v[c];
        v[c] += v[b];
        v[b] += v[d];
    };

    bf0(&mut v, 0, 31, c0(0), 1);
    bf0(&mut v, 15, 16, c0(15), 5);
    bf(&mut v, 0, 15, c1(0), 1);
    bf(&mut v, 16, 31, -c1(0), 1);
    bf0(&mut v, 7, 24, c0(7), 1);
    bf0(&mut v, 8, 23, c0(8), 1);
    bf(&mut v, 7, 8, c1(7), 4);
    bf(&mut v, 23, 24, -c1(7), 4);
    bf(&mut v, 0, 7, c2(0), 1);
    bf(&mut v, 8, 15, -c2(0), 1);
    bf(&mut v, 16, 23, c2(0), 1);
    bf(&mut v, 24, 31, -c2(0), 1);
    bf0(&mut v, 3, 28, c0(3), 1);
    bf0(&mut v, 12, 19, c0(12), 2);
    bf(&mut v, 3, 12, c1(3), 1);
    bf(&mut v, 19, 28, -c1(3), 1);
    bf0(&mut v, 4, 27, c0(4), 1);
    bf0(&mut v, 11, 20, c0(11), 2);
    bf(&mut v, 4, 11, c1(4), 1);
    bf(&mut v, 20, 27, -c1(4), 1);
    bf(&mut v, 3, 4, c2(3), 3);
    bf(&mut v, 11, 12, -c2(3), 3);
    bf(&mut v, 19, 20, c2(3), 3);
    bf(&mut v, 27, 28, -c2(3), 3);
    bf(&mut v, 0, 3, c3(0), 1);
    bf(&mut v, 4, 7, -c3(0), 1);
    bf(&mut v, 8, 11, c3(0), 1);
    bf(&mut v, 12, 15, -c3(0), 1);
    bf(&mut v, 16, 19, c3(0), 1);
    bf(&mut v, 20, 23, -c3(0), 1);
    bf(&mut v, 24, 27, c3(0), 1);
    bf(&mut v, 28, 31, -c3(0), 1);

    bf0(&mut v, 1, 30, c0(1), 1);
    bf0(&mut v, 14, 17, c0(14), 3);
    bf(&mut v, 1, 14, c1(1), 1);
    bf(&mut v, 17, 30, -c1(1), 1);
    bf0(&mut v, 6, 25, c0(6), 1);
    bf0(&mut v, 9, 22, c0(9), 1);
    bf(&mut v, 6, 9, c1(6), 2);
    bf(&mut v, 22, 25, -c1(6), 2);
    bf(&mut v, 1, 6, c2(1), 1);
    bf(&mut v, 9, 14, -c2(1), 1);
    bf(&mut v, 17, 22, c2(1), 1);
    bf(&mut v, 25, 30, -c2(1), 1);

    bf0(&mut v, 2, 29, c0(2), 1);
    bf0(&mut v, 13, 18, c0(13), 3);
    bf(&mut v, 2, 13, c1(2), 1);
    bf(&mut v, 18, 29, -c1(2), 1);
    bf0(&mut v, 5, 26, c0(5), 1);
    bf0(&mut v, 10, 21, c0(10), 1);
    bf(&mut v, 5, 10, c1(5), 2);
    bf(&mut v, 21, 26, -c1(5), 2);
    bf(&mut v, 2, 5, c2(2), 1);
    bf(&mut v, 10, 13, -c2(2), 1);
    bf(&mut v, 18, 21, c2(2), 1);
    bf(&mut v, 26, 29, -c2(2), 1);
    bf(&mut v, 1, 2, c3(1), 2);
    bf(&mut v, 5, 6, -c3(1), 2);
    bf(&mut v, 9, 10, c3(1), 2);
    bf(&mut v, 13, 14, -c3(1), 2);
    bf(&mut v, 17, 18, c3(1), 2);
    bf(&mut v, 21, 22, -c3(1), 2);
    bf(&mut v, 25, 26, c3(1), 2);
    bf(&mut v, 29, 30, -c3(1), 2);

    bf1(&mut v, 0, 1, 2, 3);
    bf2(&mut v, 4, 5, 6, 7);
    bf1(&mut v, 8, 9, 10, 11);
    bf2(&mut v, 12, 13, 14, 15);
    bf1(&mut v, 16, 17, 18, 19);
    bf2(&mut v, 20, 21, 22, 23);
    bf1(&mut v, 24, 25, 26, 27);
    bf2(&mut v, 28, 29, 30, 31);

    for (a, b) in [(8, 12), (12, 10), (10, 14), (14, 9), (9, 13), (13, 11), (11, 15)] {
        v[a] += v[b];
    }
    for (i, at) in [0, 16, 8, 24, 4, 20, 12, 28, 2, 18, 10, 26, 6, 22, 14, 30].into_iter().enumerate() {
        out[at] = v[i];
    }
    for (a, b) in [(24, 28), (28, 26), (26, 30), (30, 25), (25, 29), (29, 27), (27, 31)] {
        v[a] += v[b];
    }
    for (at, a, b) in [
        (1, 16, 24),
        (17, 17, 25),
        (9, 18, 26),
        (25, 19, 27),
        (5, 20, 28),
        (21, 21, 29),
        (13, 22, 30),
        (29, 23, 31),
        (3, 24, 20),
        (19, 25, 21),
        (11, 26, 22),
        (27, 27, 23),
        (7, 28, 18),
        (23, 29, 19),
        (15, 30, 17),
    ] {
        out[at] = v[a] + v[b];
    }
    out[31] = v[31];
}

/// `ff_mpadsp_apply_window_float`: `MACS` (`sum += w * p`) and `MLSS`
/// (`sum -= w * p`) each fused, as clang builds them on arm64.
fn apply_window(synth_buf: &mut [f32], window: &[f32], dither_state: &mut i32, samples: &mut [f32], incr: usize) {
    synth_buf.copy_within(0..32, 512);
    let macs = |sum: &mut f32, w: f32, p: f32| *sum = w.mul_add(p, *sum);
    let mlss = |sum: &mut f32, w: f32, p: f32| *sum = (-w).mul_add(p, *sum);

    let mut sum = *dither_state as f32;
    for k in 0..8 {
        macs(&mut sum, window[k * 64], synth_buf[16 + k * 64]);
    }
    for k in 0..8 {
        mlss(&mut sum, window[32 + k * 64], synth_buf[48 + k * 64]);
    }
    samples[0] = std::mem::take(&mut sum);
    let mut out = incr;
    let mut out2 = 31 * incr;
    let (mut w, mut w2) = (1usize, 31usize);
    for j in 1..16 {
        let mut sum2 = 0f32;
        for k in 0..8 {
            let tmp = synth_buf[16 + j + k * 64];
            macs(&mut sum, window[w + k * 64], tmp);
            mlss(&mut sum2, window[w2 + k * 64], tmp);
        }
        for k in 0..8 {
            let tmp = synth_buf[48 - j + k * 64];
            mlss(&mut sum, window[w + 32 + k * 64], tmp);
            mlss(&mut sum2, window[w2 + 32 + k * 64], tmp);
        }
        samples[out] = std::mem::take(&mut sum);
        out += incr;
        sum += sum2;
        samples[out2] = std::mem::take(&mut sum);
        out2 -= incr;
        w += 1;
        w2 -= 1;
    }
    for k in 0..8 {
        mlss(&mut sum, window[w + 32 + k * 64], synth_buf[32 + k * 64]);
    }
    samples[out] = std::mem::take(&mut sum);
    *dither_state = sum as i32;
}

/// `ff_mpa_synth_filter_float`
fn synth_filter(
    synth_buf: &mut [f32],
    synth_buf_offset: &mut usize,
    window: &[f32],
    dither_state: &mut i32,
    samples: &mut [f32],
    incr: usize,
    sb_samples: &[f32; 32],
) {
    let offset = *synth_buf_offset;
    let buf = &mut synth_buf[offset..];
    dct32(buf, sb_samples);
    apply_window(buf, window, dither_state, samples, incr);
    *synth_buf_offset = offset.wrapping_sub(32) & 511;
}

/// The `qdm2` decoder.
pub struct Qdm2Decoder {
    codec_id: CodecId,
    q: Qdm2,
    /// The rate the `QDCA` atom gives, as FFmpeg takes it.
    sample_rate: u32,
    out: VecDeque<Frame>,
}

impl Decoder for Qdm2Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    /// `qdm2_decode_frame`, called as libavcodec calls it: every
    /// `checksum_size` bytes of the packet is one superblock and one frame
    /// of 16 * `frame_size` samples; a shorter remainder is invalid.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut data = &packet.data[..];
        let mut pts = packet.pts;
        let checksum_size = self.q.checksum_size;
        while !data.is_empty() {
            if data.len() < checksum_size {
                return Err(invalid("packet shorter than its superblock"));
            }
            self.q.sub_packet = 0;
            let frame_size = self.q.frame_size * self.q.channels;
            let mut out = vec![0i16; 16 * frame_size];
            for sub in out.chunks_exact_mut(frame_size) {
                self.q.decode(data, sub)?;
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
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat { sample_format: SampleFormat::S16, sample_rate: self.sample_rate, channels: self.q.channels as u16 })
    }
}

/// Decoder factory. `params.extradata` carries the `frma`/`QDCA` atoms
/// (MOV sample entry, CAF `kuki`).
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let (q, sample_rate) = Qdm2::new(&params.extradata)?;
    Ok(Box::new(Qdm2Decoder { codec_id: params.codec_id.clone(), q, sample_rate, out: VecDeque::new() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mono, 44100 Hz, 24 kb/s, 8192-sample groups (512-sample frames), a
    /// 128-point FFT, 1000-byte superblocks.
    fn mono_8192() -> Qdm2 {
        let mut extradata = b"frmaQDM2".to_vec();
        extradata.extend_from_slice(&36u32.to_be_bytes());
        extradata.extend_from_slice(b"QDCA");
        for v in [1u32, 1, 44100, 24000, 8192, 128, 1000, 0] {
            extradata.extend_from_slice(&v.to_be_bytes());
        }
        Qdm2::new(&extradata).unwrap().0
    }

    #[test]
    fn long_runs_of_tone_skips_wrap_the_position() {
        // For duration 3 the 1-bit code `1` is the skip value 0: each moves
        // the tone position 1024 on, and 2.4 million of them pass i32::MAX
        // before FFmpeg checks the position. Its C ints wrap; the wrapped
        // (negative) position passes that check, and the tone the trailing
        // zero bits code is kept.
        let mut q = mono_8192();
        q.superblocktype_2_3 = true;
        let skips = vec![0xff; 300_000];
        assert!(q.fft_decode_tones(3, &mut GetBitsLe::new(&skips), false).is_ok());
        assert_eq!(q.fft_coefs_index, 1);
    }
}
