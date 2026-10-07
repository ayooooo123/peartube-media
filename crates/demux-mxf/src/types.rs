// Ported from FFmpeg libavformat/mxfdec.c, mxf.c, mxf.h (commit 2da55bf)
// License: LGPL-2.1-or-later

//! Keys, enums and UL matching shared by the demuxer's parts.

/// A SMPTE universal label or instance UID.
pub type Uid = [u8; 16];

/// mxf.h MXFWrappingIndicatorType.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapType {
    NormalWrap,
    D10D11Wrap,
    RawAWrap,
    RawVWrap,
    J2KWrap,
}

/// mxf.h MXFCodecUL, its `id` typed per table.
#[derive(Clone, Copy, Debug)]
pub struct CodecUl<T: 'static> {
    pub uid: Uid,
    pub matching_len: usize,
    pub id: T,
    pub wrapping_indicator_pos: usize,
    pub wrapping_indicator_type: WrapType,
}

/// MXFPartitionType.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PartitionType {
    #[default]
    Header,
    BodyPartition,
    Footer,
}

/// MXFOP (0 where no partition pack set it yet).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Op {
    #[default]
    Unset,
    Op1a,
    Op1b,
    Op1c,
    Op2a,
    Op2b,
    Op2c,
    Op3a,
    Op3b,
    Op3c,
    OpAtom,
    OpSonyOpt,
}

/// MXFWrappingScheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Wrapping {
    #[default]
    Unknown,
    Frame,
    Clip,
}

pub const MXF_MAX_CHUNK_SIZE: u64 = 32 << 20;
/// S377m-2004 section 5.5 and S377-1-2009 section 6.5, the +1 to be
/// slightly more tolerant.
pub const RUN_IN_MAX: u64 = 65535 + 1;
/// FF_SANE_NB_CHANNELS.
pub const SANE_NB_CHANNELS: i32 = 512;

pub const HEADER_PARTITION_PACK_KEY: [u8; 14] =
    [0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, 0x02];
pub const ESSENCE_ELEMENT_KEY: [u8; 12] = [0x06, 0x0e, 0x2b, 0x34, 0x01, 0x02, 0x01, 0x01, 0x0d, 0x01, 0x03, 0x01];
pub const AVID_ESSENCE_ELEMENT_KEY: [u8; 12] = [0x06, 0x0e, 0x2b, 0x34, 0x01, 0x02, 0x01, 0x01, 0x0e, 0x04, 0x03, 0x01];
pub const CANOPUS_ESSENCE_ELEMENT_KEY: [u8; 12] = [0x06, 0x0e, 0x2b, 0x34, 0x01, 0x02, 0x01, 0x0a, 0x0e, 0x0f, 0x03, 0x01];
pub const SYSTEM_ITEM_KEY_CP: [u8; 13] = [0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x03, 0x01, 0x04];
pub const SYSTEM_ITEM_KEY_GC: [u8; 13] = [0x06, 0x0e, 0x2b, 0x34, 0x02, 0x53, 0x01, 0x01, 0x0d, 0x01, 0x03, 0x01, 0x14];
pub const KLV_KEY: [u8; 4] = [0x06, 0x0e, 0x2b, 0x34];
pub const RANDOM_INDEX_PACK_KEY: Uid =
    [0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, 0x11, 0x01, 0x00];
pub const CRYPTO_SOURCE_CONTAINER_UL: Uid =
    [0x06, 0x0e, 0x2b, 0x34, 0x01, 0x01, 0x01, 0x09, 0x06, 0x01, 0x01, 0x02, 0x02, 0x00, 0x00, 0x00];
pub const ENCRYPTED_TRIPLET_KEY: Uid =
    [0x06, 0x0e, 0x2b, 0x34, 0x02, 0x04, 0x01, 0x07, 0x0d, 0x01, 0x03, 0x01, 0x02, 0x7e, 0x01, 0x00];
pub const ENCRYPTED_ESSENCE_CONTAINER: Uid =
    [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x07, 0x0d, 0x01, 0x03, 0x01, 0x02, 0x0b, 0x01, 0x00];
pub const SONY_MPEG4_EXTRADATA: Uid =
    [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x01, 0x0e, 0x06, 0x06, 0x02, 0x02, 0x01, 0x00, 0x00];
pub const FFV1_EXTRADATA: Uid =
    [0x06, 0x0e, 0x2b, 0x34, 0x01, 0x01, 0x01, 0x0e, 0x04, 0x01, 0x06, 0x0c, 0x01, 0x00, 0x00, 0x00];
pub const SUB_DESCRIPTOR: Uid =
    [0x06, 0x0e, 0x2b, 0x34, 0x01, 0x01, 0x01, 0x09, 0x06, 0x01, 0x01, 0x04, 0x06, 0x10, 0x00, 0x00];
/// The SMPTE ST 422 (JPEG 2000) essence container prefix, mxf_is_st_422.
pub const ST_422_ESSENCE_CONTAINER_UL: [u8; 14] =
    [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x07, 0x0d, 0x01, 0x03, 0x01, 0x02, 0x0c];

/// IS_KLV_KEY: `key` starts with `prefix`.
pub fn is_klv_key(key: &[u8], prefix: &[u8]) -> bool {
    key.len() >= prefix.len() && &key[..prefix.len()] == prefix
}

/// mxf_match_uid: the first `len` bytes equal, the version byte (7) aside.
pub fn match_uid(key: &Uid, prefix: &Uid, len: usize) -> bool {
    (0..len.min(16)).all(|i| i == 7 || key[i] == prefix[i])
}

/// mxf_get_codec_ul: the first entry matching `uid` over its
/// matching_len, else the table's all-zero terminating entry.
pub fn get_codec_ul<T: Copy>(uls: &'static [CodecUl<T>], uid: &Uid) -> &'static CodecUl<T> {
    let last = uls.len() - 1;
    uls[..last].iter().find(|ul| match_uid(&ul.uid, uid, ul.matching_len)).unwrap_or(&uls[last])
}

/// mxf_is_partition_pack_key: any partition pack key (key[13] 2 to 4).
pub fn is_partition_pack_key(key: &Uid) -> bool {
    key[..13] == HEADER_PARTITION_PACK_KEY[..13] && (2..=4).contains(&key[13])
}

/// The essence element keys mxf_read_packet and mxf_read_header take.
pub fn is_essence_element_key(key: &Uid) -> bool {
    is_klv_key(key, &ESSENCE_ELEMENT_KEY) || is_klv_key(key, &CANOPUS_ESSENCE_ELEMENT_KEY) || is_klv_key(key, &AVID_ESSENCE_ELEMENT_KEY)
}
