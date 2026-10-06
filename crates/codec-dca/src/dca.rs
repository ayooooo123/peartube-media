// Ported from FFmpeg libavcodec/dca.h, dca_syncwords.h, dca_core.h
// (constants) and dca.c (core frame header parse, bitstream conversion)
// (commit 2da55bf). Licensed under LGPL-2.1-or-later.

//! Shared DCA constants: speaker layout masks, sync words, extension
//! masks, and the core frame header parser plus the 14/24-bit bitstream
//! normalizer every DCA entry point uses.

use crate::bitreader::BitReader;
use crate::data::{FF_DCA_BITS_PER_SAMPLE, FF_DCA_SAMPLE_RATES};

pub const DCA_CORE_FRAME_HEADER_SIZE: usize = 18;

pub const DCA_CHANNELS: usize = 7;
pub const DCA_SUBBANDS: usize = 32;
pub const DCA_SUBBANDS_X96: usize = 64;
pub const DCA_SUBFRAMES: usize = 16;
pub const DCA_SUBBAND_SAMPLES: usize = 8;
pub const DCA_PCMBLOCK_SAMPLES: usize = 32;
pub const DCA_LFE_HISTORY: usize = 8;
pub const DCA_ABITS_MAX: i32 = 26;

pub const DCA_CORE_CHANNELS_MAX: usize = 6;
pub const DCA_DMIX_CHANNELS_MAX: usize = 4;
pub const DCA_XXCH_CHANNELS_MAX: usize = 2;
pub const DCA_EXSS_CHANNELS_MAX: usize = 8;
pub const DCA_EXSS_CHSETS_MAX: usize = 4;

pub const DCA_FILTER_MODE_X96: i32 = 0x01;
pub const DCA_FILTER_MODE_FIXED: i32 = 0x02;

pub const DCA_SPEAKER_COUNT: usize = 32;

// ───────────────────────── dca_syncwords.h ─────────────────────────

pub const DCA_SYNCWORD_CORE_BE: u32 = 0x7FFE8001;
pub const DCA_SYNCWORD_CORE_LE: u32 = 0xFE7F0180;
pub const DCA_SYNCWORD_CORE_14B_BE: u32 = 0x1FFFE800;
pub const DCA_SYNCWORD_CORE_14B_LE: u32 = 0xFF1F00E8;
pub const DCA_SYNCWORD_XCH: u32 = 0x5A5A5A5A;
pub const DCA_SYNCWORD_XXCH: u32 = 0x47004A03;
pub const DCA_SYNCWORD_X96: u32 = 0x1D95F262;
pub const DCA_SYNCWORD_XBR: u32 = 0x655E315E;
pub const DCA_SYNCWORD_LBR: u32 = 0x0A801921;
pub const DCA_SYNCWORD_XLL: u32 = 0x41A29547;
pub const DCA_SYNCWORD_SUBSTREAM: u32 = 0x64582025;
pub const DCA_SYNCWORD_SUBSTREAM_CORE: u32 = 0x02B09261;
pub const DCA_SYNCWORD_REV1AUX: u32 = 0x9A1105A0;
pub const DCA_SYNCWORD_XLL_X: u32 = 0x02000850;
pub const DCA_SYNCWORD_XLL_X_IMAX: u32 = 0xF14000D0;

// ───────────────────────── DCASpeaker / masks ─────────────────────────

pub mod speaker {
    pub const C: usize = 0;
    pub const L: usize = 1;
    pub const R: usize = 2;
    pub const LS: usize = 3;
    pub const RS: usize = 4;
    pub const LFE1: usize = 5;
    pub const CS: usize = 6;
    pub const LSR: usize = 7;
    pub const RSR: usize = 8;
    pub const LSS: usize = 9;
    pub const RSS: usize = 10;
    pub const LC: usize = 11;
    pub const RC: usize = 12;
    pub const LH: usize = 13;
    pub const CH: usize = 14;
    pub const RH: usize = 15;
    pub const LFE2: usize = 16;
    pub const LW: usize = 17;
    pub const RW: usize = 18;
    pub const OH: usize = 19;
    pub const LHS: usize = 20;
    pub const RHS: usize = 21;
    pub const CHR: usize = 22;
    pub const LHR: usize = 23;
    pub const RHR: usize = 24;
    pub const CL: usize = 25;
    pub const LL: usize = 26;
    pub const RL: usize = 27;
    pub const RSV1: usize = 28;
    pub const RSV2: usize = 29;
    pub const RSV3: usize = 30;
    pub const RSV4: usize = 31;
}

pub mod mask {
    pub const C: u32 = 0x0000_0001;
    pub const L: u32 = 0x0000_0002;
    pub const R: u32 = 0x0000_0004;
    pub const LS: u32 = 0x0000_0008;
    pub const RS: u32 = 0x0000_0010;
    pub const LFE1: u32 = 0x0000_0020;
    pub const CS: u32 = 0x0000_0040;
    pub const LSR: u32 = 0x0000_0080;
    pub const RSR: u32 = 0x0000_0100;
    pub const LSS: u32 = 0x0000_0200;
    pub const RSS: u32 = 0x0000_0400;
    pub const LC: u32 = 0x0000_0800;
    pub const RC: u32 = 0x0000_1000;
    pub const LH: u32 = 0x0000_2000;
    pub const CH: u32 = 0x0000_4000;
    pub const RH: u32 = 0x0000_8000;
    pub const LFE2: u32 = 0x0001_0000;
    pub const LW: u32 = 0x0002_0000;
    pub const RW: u32 = 0x0004_0000;
    pub const OH: u32 = 0x0008_0000;
    pub const LHS: u32 = 0x0010_0000;
    pub const RHS: u32 = 0x0020_0000;
    pub const CHR: u32 = 0x0040_0000;
    pub const LHR: u32 = 0x0080_0000;
    pub const RHR: u32 = 0x0100_0000;
    pub const CL: u32 = 0x0200_0000;
    pub const LL: u32 = 0x0400_0000;
    pub const RL: u32 = 0x0800_0000;

    pub const MONO: u32 = C;
    pub const STEREO: u32 = L | R;
    pub const TWO_POINT1: u32 = STEREO | LFE1;
    pub const THREE_0: u32 = STEREO | C;
    pub const TWO_1: u32 = STEREO | CS;
    pub const THREE_1: u32 = THREE_0 | CS;
    pub const TWO_2: u32 = STEREO | LS | RS;
    pub const FIVE_POINT0: u32 = THREE_0 | LS | RS;
    pub const FIVE_POINT1: u32 = FIVE_POINT0 | LFE1;
    pub const SEVEN_POINT0_WIDE: u32 = FIVE_POINT0 | LW | RW;
    pub const SEVEN_POINT1_WIDE: u32 = SEVEN_POINT0_WIDE | LFE1;
}

pub fn has_stereo(mask: u32) -> bool {
    mask & mask::STEREO == mask::STEREO
}

pub mod speaker_pair {
    pub const C: u16 = 0x0001;
    pub const LR: u16 = 0x0002;
    pub const LSRS: u16 = 0x0004;
    pub const LFE1: u16 = 0x0008;
    pub const CS: u16 = 0x0010;
    pub const LHRH: u16 = 0x0020;
    pub const LSRRSR: u16 = 0x0040;
    pub const CH: u16 = 0x0080;
    pub const OH: u16 = 0x0100;
    pub const LCRC: u16 = 0x0200;
    pub const LWRW: u16 = 0x0400;
    pub const LSSRSS: u16 = 0x0800;
    pub const LFE2: u16 = 0x1000;
    pub const LHSRHS: u16 = 0x2000;
    pub const CHR: u16 = 0x4000;
    pub const LHRRHR: u16 = 0x8000;
}

/// `ff_dca_count_chs_for_mask`.
pub fn count_chs_for_mask(mask: u32) -> i32 {
    (((mask & 0xffff) | ((mask & 0xae66) << 16)).count_ones()) as i32
}

pub const DCA_REPR_TYPE_LTRT: i32 = 2;
pub const DCA_REPR_TYPE_LHRH: i32 = 3;

/// CSS extension masks (dca.h DCAExtensionMask, low nibble).
pub mod css_mask {
    pub const CORE: i32 = 0x001;
    pub const XXCH: i32 = 0x002;
    pub const X96: i32 = 0x004;
    pub const XCH: i32 = 0x008;
    pub const CSS_MASK: i32 = 0x00f;
}

/// EXSS extension masks (dca.h DCAExtensionMask, high nibble).
pub mod exss_mask {
    pub const CORE: i32 = 0x010;
    pub const XBR: i32 = 0x020;
    pub const EXSS_XXCH: i32 = 0x040;
    pub const EXSS_X96: i32 = 0x080;
    pub const LBR: i32 = 0x100;
    pub const XLL: i32 = 0x200;
    pub const RSV1: i32 = 0x400;
    pub const RSV2: i32 = 0x800;
    pub const EXSS_MASK: i32 = 0xff0;
}

pub mod dmix_type {
    pub const TYPE_1_0: i32 = 0;
    pub const LORO: i32 = 1;
    pub const LTRT: i32 = 2;
    pub const TYPE_3_0: i32 = 3;
    pub const TYPE_2_1: i32 = 4;
    pub const TYPE_2_2: i32 = 5;
    pub const TYPE_3_1: i32 = 6;
    pub const COUNT: usize = 7;
}

pub mod amode {
    pub const MONO: usize = 0;
    pub const MONO_DUAL: usize = 1;
    pub const STEREO: usize = 2;
    pub const STEREO_SUMDIFF: usize = 3;
    pub const STEREO_TOTAL: usize = 4;
    pub const AMODE_3F: usize = 5;
    pub const AMODE_2F1R: usize = 6;
    pub const AMODE_3F1R: usize = 7;
    pub const AMODE_2F2R: usize = 8;
    pub const AMODE_3F2R: usize = 9;
    pub const COUNT: usize = 10;
}

pub mod ext_audio_type {
    pub const XCH: i32 = 0;
    pub const X96: i32 = 2;
    pub const XXCH: i32 = 6;
}

pub mod lfe_flag {
    pub const NONE: i32 = 0;
    pub const FLAG_128: i32 = 1;
    pub const FLAG_64: i32 = 2;
    pub const INVALID: i32 = 3;
}

/// `DCACoreFrameHeader`.
#[derive(Clone, Copy, Debug, Default)]
pub struct CoreFrameHeader {
    pub normal_frame: u8,
    pub deficit_samples: u8,
    pub crc_present: u8,
    pub npcmblocks: u8,
    pub frame_size: u16,
    pub audio_mode: u8,
    pub sr_code: u8,
    pub br_code: u8,
    pub drc_present: u8,
    pub ts_present: u8,
    pub aux_present: u8,
    pub hdcd_master: u8,
    pub ext_audio_type: u8,
    pub ext_audio_present: u8,
    pub sync_ssf: u8,
    pub lfe_present: u8,
    pub predictor_history: u8,
    pub filter_perfect: u8,
    pub encoder_rev: u8,
    pub copy_hist: u8,
    pub pcmr_code: u8,
    pub sumdiff_front: u8,
    pub sumdiff_surround: u8,
    pub dn_code: u8,
}

/// `ff_dca_parse_core_frame_header` error codes (dca.h DCAParseError).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    SyncWord,
    DeficitSamples,
    PcmBlocks,
    FrameSize,
    Amode,
    SampleRate,
    ReservedBit,
    LfeFlag,
    PcmRes,
}

/// `ff_dca_parse_core_frame_header`: parse and validate a core frame
/// header from the first 120 bits of `gb`.
pub fn parse_core_frame_header(h: &mut CoreFrameHeader, gb: &mut BitReader) -> Result<(), ParseError> {
    if gb.get_bits_long(32) != DCA_SYNCWORD_CORE_BE {
        return Err(ParseError::SyncWord);
    }

    h.normal_frame = gb.get_bits(1) as u8;
    h.deficit_samples = gb.get_bits(5) as u8 + 1;
    if h.deficit_samples as usize != DCA_PCMBLOCK_SAMPLES {
        return Err(ParseError::DeficitSamples);
    }

    h.crc_present = gb.get_bits(1) as u8;
    h.npcmblocks = gb.get_bits(7) as u8 + 1;
    if h.npcmblocks as usize & (DCA_SUBBAND_SAMPLES - 1) != 0 {
        return Err(ParseError::PcmBlocks);
    }

    h.frame_size = gb.get_bits(14) as u16 + 1;
    if h.frame_size < 96 {
        return Err(ParseError::FrameSize);
    }

    h.audio_mode = gb.get_bits(6) as u8;
    if h.audio_mode as usize >= amode::COUNT {
        return Err(ParseError::Amode);
    }

    h.sr_code = gb.get_bits(4) as u8;
    if FF_DCA_SAMPLE_RATES[h.sr_code as usize] == 0 {
        return Err(ParseError::SampleRate);
    }

    h.br_code = gb.get_bits(5) as u8;
    if gb.get_bits(1) != 0 {
        return Err(ParseError::ReservedBit);
    }

    h.drc_present = gb.get_bits(1) as u8;
    h.ts_present = gb.get_bits(1) as u8;
    h.aux_present = gb.get_bits(1) as u8;
    h.hdcd_master = gb.get_bits(1) as u8;
    h.ext_audio_type = gb.get_bits(3) as u8;
    h.ext_audio_present = gb.get_bits(1) as u8;
    h.sync_ssf = gb.get_bits(1) as u8;
    h.lfe_present = gb.get_bits(2) as u8;
    if h.lfe_present as i32 == lfe_flag::INVALID {
        return Err(ParseError::LfeFlag);
    }

    h.predictor_history = gb.get_bits(1) as u8;
    if h.crc_present != 0 {
        gb.skip(16);
    }
    h.filter_perfect = gb.get_bits(1) as u8;
    h.encoder_rev = gb.get_bits(4) as u8;
    h.copy_hist = gb.get_bits(2) as u8;
    h.pcmr_code = gb.get_bits(3) as u8;
    if FF_DCA_BITS_PER_SAMPLE[h.pcmr_code as usize] == 0 {
        return Err(ParseError::PcmRes);
    }

    h.sumdiff_front = gb.get_bits(1) as u8;
    h.sumdiff_surround = gb.get_bits(1) as u8;
    h.dn_code = gb.get_bits(4) as u8;
    Ok(())
}

/// `avpriv_dca_convert_bitstream`: convert a DCA frame in any of the four
/// core encodings (or an EXSS substream) to big-endian 16-bit words.
/// Returns the converted length, or `None` when the sync word is unknown.
pub fn convert_bitstream(src: &[u8], dst: &mut [u8]) -> Option<usize> {
    let mut src_size = src.len();
    if src_size > dst.len() {
        src_size = dst.len();
    }
    if src.len() < 4 {
        return None;
    }
    let mrk = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);

    match mrk {
        DCA_SYNCWORD_CORE_BE | DCA_SYNCWORD_SUBSTREAM => {
            dst[..src_size].copy_from_slice(&src[..src_size]);
            Some(src_size)
        }
        DCA_SYNCWORD_CORE_LE => {
            for i in 0..(src_size + 1) >> 1 {
                let s = i * 2;
                if s + 2 <= src.len() {
                    dst[s] = src[s + 1];
                    dst[s + 1] = src[s];
                }
            }
            Some(src_size)
        }
        DCA_SYNCWORD_CORE_14B_BE | DCA_SYNCWORD_CORE_14B_LE => {
            // Repack 14-bit words into 16-bit words (init_put_bits path).
            let mut acc: u32 = 0;
            let mut nbits: u32 = 0;
            let mut out = 0usize;
            for i in 0..(src_size + 1) >> 1 {
                let s = i * 2;
                let w = if s + 2 <= src.len() {
                    u16::from_be_bytes([src[s], src[s + 1]])
                } else {
                    u16::from_be_bytes([src[s], 0])
                };
                let w = if mrk == DCA_SYNCWORD_CORE_14B_BE {
                    w & 0x3FFF
                } else {
                    u16::from_be(w.swap_bytes()) & 0x3FFF
                };
                acc = (acc << 14) | u32::from(w);
                nbits += 14;
                while nbits >= 8 && out < dst.len() {
                    nbits -= 8;
                    dst[out] = ((acc >> nbits) & 0xFF) as u8;
                    out += 1;
                }
            }
            if nbits > 0 && out < dst.len() {
                dst[out] = ((acc << (8 - nbits)) & 0xFF) as u8;
                out += 1;
            }
            Some(out)
        }
        _ => None,
    }
}

/// Packet flags (dcadec.h).
pub mod decoder_packets {
    pub const DCA_PACKET_CORE: i32 = 0x01;
    pub const DCA_PACKET_EXSS: i32 = 0x02;
    pub const DCA_PACKET_XLL: i32 = 0x04;
    pub const DCA_PACKET_LBR: i32 = 0x08;
    pub const DCA_PACKET_MASK: i32 = 0x0f;
    pub const DCA_PACKET_RECOVERY: i32 = 0x10;
    pub const DCA_PACKET_RESIDUAL: i32 = 0x20;
}
