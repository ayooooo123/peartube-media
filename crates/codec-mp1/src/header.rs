// The MPEG audio frame header.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/mpegaudiodecheader.c
// (ff_mpegaudio_decode_header), mpegaudiodecheader.h (ff_mpa_check_header)
// and mpegaudiotabs.h (ff_mpa_bitrate_tab, ff_mpa_freq_tab).
// Copyright (c) 2001, 2002 Fabrice Bellard; LGPL-2.1-or-later (see
// LICENSE).

/// `MPA_JSTEREO`
pub(crate) const MPA_JSTEREO: u32 = 1;
/// `MPA_MONO`
pub(crate) const MPA_MONO: u32 = 3;

/// `ff_mpa_bitrate_tab[lsf][layer - 1]`, in kbit/s.
const BITRATE_TAB: [[[u16; 15]; 3]; 2] = [
    [
        [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
    ],
    [
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ],
];

/// `ff_mpa_freq_tab`
const FREQ_TAB: [u32; 3] = [44100, 48000, 32000];

/// `MPADecodeHeader`: the fields the Layer I decoder uses.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Header {
    pub(crate) layer: u32,
    pub(crate) sample_rate: u32,
    pub(crate) error_protection: bool,
    pub(crate) mode: u32,
    pub(crate) mode_ext: u32,
    pub(crate) nb_channels: usize,
    /// The frame's bytes; `None` for a free-format frame (bitrate index 0).
    pub(crate) frame_size: Option<usize>,
}

/// `ff_mpa_check_header`
fn check(header: u32) -> bool {
    header & 0xFFE0_0000 == 0xFFE0_0000
        && header & (3 << 19) != 1 << 19
        && header & (3 << 17) != 0
        && header & (0xF << 12) != 0xF << 12
        && header & (3 << 10) != 3 << 10
}

/// `ff_mpegaudio_decode_header`: `None` when the word is no frame header.
pub(crate) fn decode(header: u32) -> Option<Header> {
    if !check(header) {
        return None;
    }
    let (lsf, mpeg25) = if header & (1 << 20) != 0 { (u32::from(header & (1 << 19) == 0), 0) } else { (1, 1) };
    let layer = 4 - ((header >> 17) & 3);
    let sample_rate = FREQ_TAB[((header >> 10) & 3) as usize] >> (lsf + mpeg25);
    let bitrate_index = ((header >> 12) & 0xF) as usize;
    let padding = (header >> 9) & 1;
    let mode = (header >> 6) & 3;
    let frame_size = (bitrate_index != 0).then(|| {
        let kbps = u32::from(BITRATE_TAB[lsf as usize][layer as usize - 1][bitrate_index]);
        (match layer {
            1 => (kbps * 12000 / sample_rate + padding) * 4,
            2 => kbps * 144_000 / sample_rate + padding,
            _ => kbps * 144_000 / (sample_rate << lsf) + padding,
        }) as usize
    });
    Some(Header {
        layer,
        sample_rate,
        error_protection: (header >> 16) & 1 == 0,
        mode,
        mode_ext: (header >> 4) & 3,
        nb_channels: if mode == MPA_MONO { 1 } else { 2 },
        frame_size,
    })
}
