// Ported from FFmpeg libavcodec/mlp.h, libavcodec/mlp_parse.h, libavcodec/mlp.c,
// libavcodec/mlp_parse.c (commit 2da55bf). Licensed under LGPL-2.1-or-later.

//! Constants, structures and tables shared by the MLP and TrueHD decoders.

/// `SYNC_MLP`: stream type byte for MLP.
use crate::tables::{THD_CHANCOUNT, THD_LAYOUT};

pub const SYNC_MLP: u8 = 0xbb;
/// `SYNC_TRUEHD`: stream type byte for TrueHD.
pub const SYNC_TRUEHD: u8 = 0xba;

/// Last possible matrix channel for MLP.
pub const MAX_MATRIX_CHANNEL_MLP: usize = 5;
/// Last possible matrix channel for TrueHD.
pub const MAX_MATRIX_CHANNEL_TRUEHD: usize = 7;
/// Maximum number of channels in a valid stream.
/// MLP: 5.1 + 2 noise channels -> 8; TrueHD: 7.1 -> 8.
pub const MAX_CHANNELS: usize = 8;

/// Maximum number of matrices used in decoding (TrueHD).
pub const MAX_MATRICES_MLP: usize = 6;
/// Maximum number of matrices used in decoding (TrueHD).
pub const MAX_MATRICES_TRUEHD: usize = 8;
/// Maximum number of matrices over both.
pub const MAX_MATRICES: usize = 8;

/// Maximum number of substreams that can be decoded.
/// MLP's limit is 2; TrueHD supports at least up to 3.
pub const MAX_SUBSTREAMS: usize = 4;

/// Which multiple of 48000 the maximum sample rate is.
pub const MAX_RATEFACTOR: u32 = 4;
/// Maximum sample frequency seen in files.
pub const MAX_SAMPLERATE: u32 = MAX_RATEFACTOR * 48000;
/// Maximum number of audio samples within one access unit.
pub const MAX_BLOCKSIZE: usize = 40 * MAX_RATEFACTOR as usize;
/// Next power of two greater than [`MAX_BLOCKSIZE`].
pub const MAX_BLOCKSIZE_POW2: usize = 64 * MAX_RATEFACTOR as usize;

/// Number of allowed filters.
pub const NUM_FILTERS: usize = 2;
/// Filter index: FIR.
pub const FIR: usize = 0;
/// Filter index: IIR.
pub const IIR: usize = 1;
/// The maximum number of taps in FIR filters.
pub const MAX_FIR_ORDER: usize = 8;
/// The maximum number of taps in IIR filters.
pub const MAX_IIR_ORDER: usize = 4;

/// Code that signals end of a stream (the 16-bit half is checked inline).
#[allow(dead_code)]
pub const END_OF_STREAM: u32 = 0xd234d234;

pub const PARAM_BLOCKSIZE: u8 = 1 << 7;
pub const PARAM_MATRIX: u8 = 1 << 6;
pub const PARAM_OUTSHIFT: u8 = 1 << 5;
pub const PARAM_QUANTSTEP: u8 = 1 << 4;
pub const PARAM_FIR: u8 = 1 << 3;
pub const PARAM_IIR: u8 = 1 << 2;
pub const PARAM_HUFFOFFSET: u8 = 1 << 1;
pub const PARAM_PRESENCE: u8 = 1 << 0;

/// Filter data: order, shift, state.
#[derive(Clone, Copy, Debug)]
pub struct FilterParams {
    /// Number of taps in the filter.
    pub order: u8,
    /// Right shift to apply to the filter output.
    pub shift: u8,
    pub state: [i32; MAX_FIR_ORDER],
}

impl Default for FilterParams {
    fn default() -> Self {
        Self {
            order: 0,
            shift: 0,
            state: [0; MAX_FIR_ORDER],
        }
    }
}

/// Sample data coding information for one channel.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChannelParams {
    pub filter_params: [FilterParams; NUM_FILTERS],
    pub coeff: [[i32; MAX_FIR_ORDER]; NUM_FILTERS],
    /// Offset to apply to residual values.
    pub huff_offset: i16,
    /// Sign/rounding-corrected version of `huff_offset`.
    pub sign_huff_offset: i32,
    /// Which VLC codebook to use to read residuals.
    pub codebook: u8,
    /// Size of the residual suffix not encoded by the VLC.
    pub huff_lsbs: u8,
}

impl ChannelParams {
    /// The restart-header defaults: 24-bit raw PCM, no filters.
    pub fn reset_to_defaults(&mut self) {
        self.filter_params[FIR].order = 0;
        self.filter_params[IIR].order = 0;
        self.filter_params[FIR].shift = 0;
        self.filter_params[IIR].shift = 0;
        self.huff_offset = 0;
        self.sign_huff_offset = -(1 << 23);
        self.codebook = 0;
        self.huff_lsbs = 24;
        // coeff/state are not reset by FFmpeg's restart header either
        // (they are overwritten on use via filter order); keep them.
    }
}

/// `THDChannelModifier` values from `mlp.h`. Part of the ported surface:
/// the values ride in the major sync and drive matrix-encoding metadata.
#[allow(dead_code)]
pub mod thd_modifier {
    pub const NOTINDICATED: u8 = 0x0;
    pub const STEREO: u8 = 0x0; // Stereo (not Dolby Surround)
    pub const LTRT: u8 = 0x1; // Dolby Surround
    pub const LBINRBIN: u8 = 0x2; // Dolby Headphone
    pub const MONO: u8 = 0x3; // Mono or Dual Mono
    pub const NOTSURROUNDEX: u8 = 0x1; // Not Dolby Digital EX
    pub const SURROUNDEX: u8 = 0x2; // Dolby Digital EX
}

/// FFmpeg's `mlp_samplerate`: `(in & 8 ? 44100 : 48000) << (in & 7)`, 0 for 0xF.
#[inline]
pub fn mlp_samplerate(ratebits: u32) -> u32 {
    if ratebits == 0xF {
        return 0;
    }
    (if ratebits & 8 != 0 { 44100 } else { 48000 }) << (ratebits & 7)
}

/// `truehd_channels`: sum of `thd_chancount[i] * ((chanmap >> i) & 1)`.
#[inline]
pub fn truehd_channels(chanmap: u32) -> u32 {
    THD_CHANCOUNT
        .iter()
        .enumerate()
        .map(|(i, &count)| u32::from(count) * ((chanmap >> i) & 1))
        .sum()
}

/// `truehd_layout`: OR of `thd_layout[i]` for every set bit of `chanmap`.
#[inline]
pub fn truehd_layout(chanmap: u32) -> u64 {
    THD_LAYOUT
        .iter()
        .enumerate()
        .filter(|(i, _)| (chanmap >> i) & 1 != 0)
        .fold(0u64, |layout, (_, &bits)| layout | bits)
}

/// `layout_truehd`: inverse of [`truehd_layout`] (used by parsers/muxers).
#[inline]
#[allow(dead_code)]
pub fn layout_truehd(layout: u64) -> u32 {
    THD_LAYOUT
        .iter()
        .enumerate()
        .filter(|&(_, &bits)| layout & bits == bits)
        .fold(0u32, |chanmap, (i, _)| chanmap | (1 << i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samplerate_matches_ffmpeg() {
        assert_eq!(mlp_samplerate(0), 48000);
        assert_eq!(mlp_samplerate(1), 96000);
        assert_eq!(mlp_samplerate(3), 384000);
        assert_eq!(mlp_samplerate(8), 44100);
        assert_eq!(mlp_samplerate(9), 88200);
        assert_eq!(mlp_samplerate(0xF), 0);
    }

    #[test]
    fn channel_tables_match_ffmpeg() {
        // chanmap 2 = LFE(1) + LRvh(2): three channels.
        assert_eq!(truehd_channels(0b10100), 3);
        assert_eq!(truehd_layout(0b10100), (1 << 3) | (1 << 12) | (1 << 14));
        // Mono: chanmap 2 (C only).
        assert_eq!(truehd_channels(2), 1);
        assert_eq!(truehd_layout(2), 0x4);
        // 5.1(back): LR + C + LFE + LRrs = bits 0, 1, 2, 6 (chanmap 0x47).
        assert_eq!(truehd_channels(0x47), 6);
        assert_eq!(truehd_layout(0x47), 0x3f);
    }
}
