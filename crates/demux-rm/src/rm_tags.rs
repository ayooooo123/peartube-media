// Ported from FFmpeg libavformat/rm.c, commit 2da55bf
// License: GNU Lesser General Public License (LGPL) version 2.1 or later

use oxideav_core::{CodecId, MediaType};

pub const DEINT_ID_GENR: u32 = u32::from_le_bytes(*b"genr");
pub const DEINT_ID_INT0: u32 = u32::from_le_bytes(*b"Int0");
pub const DEINT_ID_INT4: u32 = u32::from_le_bytes(*b"Int4");
pub const DEINT_ID_SIPR: u32 = u32::from_le_bytes(*b"sipr");
pub const DEINT_ID_VBRF: u32 = u32::from_le_bytes(*b"vbrf");
pub const DEINT_ID_VBRS: u32 = u32::from_le_bytes(*b"vbrs");

pub const RM_METADATA_KEYS: [&str; 4] = ["title", "author", "copyright", "comment"];

/// Map a 4-byte RealMedia codec tag to its (CodecId, MediaType) pair.
pub fn codec_id_from_rm_tag(tag: &[u8; 4]) -> Option<(CodecId, MediaType)> {
    match tag {
        b"RV10" => Some((CodecId::new("rv10"), MediaType::Video)),
        b"RV20" | b"RVTR" => Some((CodecId::new("rv20"), MediaType::Video)),
        b"RV30" => Some((CodecId::new("rv30"), MediaType::Video)),
        b"RV40" => Some((CodecId::new("rv40"), MediaType::Video)),
        b"RV60" => Some((CodecId::new("rv60"), MediaType::Video)),
        b"dnet" => Some((CodecId::new("ac3"), MediaType::Audio)),
        b"14_4" | b"lpcJ" => Some((CodecId::new("ra_144"), MediaType::Audio)),
        b"28_8" => Some((CodecId::new("ra_288"), MediaType::Audio)),
        b"cook" => Some((CodecId::new("cook"), MediaType::Audio)),
        b"atrc" => Some((CodecId::new("atrac3"), MediaType::Audio)),
        b"sipr" => Some((CodecId::new("sipr"), MediaType::Audio)),
        b"raac" | b"racp" => Some((CodecId::new("aac"), MediaType::Audio)),
        b"LSD:" => Some((CodecId::new("ralf"), MediaType::Audio)),
        b"CLV1" => Some((CodecId::new("clearvideo"), MediaType::Video)),
        _ => None,
    }
}
