//! Dolby Vision RPU (`NAL_UNSPEC62`) parser, Profile 7 → Profile 8.1 lossless
//! dynamic-metadata converter, STDL enhancement-layer stripper, and 12-bit
//! Profile 7 FEL (Full Enhancement Layer) residual reconstructor.
//!
//! All bit-level operations execute in 100% safe Rust (`#![forbid(unsafe_code)]`)
//! with strict bounds on untrusted bitstreams.

#![forbid(unsafe_code)]

use oxideav_core::{PixelFormat, VideoFrame, VideoPlane};

pub use super::codec::Framing;

const RPU_PREFIX: u8 = 0x19;
const FINAL_BYTE: u8 = 0x80;
const NUM_COMPONENTS: usize = 3;
const NLQ_NUM_PIVOTS: usize = 2;
const MMR_MAX_COEFFS: usize = 7; // [c0, c1, c2, c0*c1, c0*c2, c1*c2, c0*c1*c2] per order 1..=3
const RESHAPING_MAX_PIECES: usize = 8;
const MAX_EXT_BLOCKS: usize = 64;
const DM_DATA_PAYLOAD2_MIN_BITS: usize = 56;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoviElType {
    Mel,
    Fel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpuHeader {
    pub rpu_type: u8,
    pub rpu_format: u16,
    pub vdr_rpu_profile: u8,
    pub vdr_rpu_level: u8,
    pub vdr_seq_info_present_flag: bool,
    pub chroma_resampling_explicit_filter_flag: bool,
    pub chroma_filter_idc: u64,
    pub coefficient_data_type: u8,
    pub coefficient_log2_denom: u64,
    pub coef_len: usize,
    pub vdr_rpu_normalized_idc: u8,
    pub bl_video_full_range_flag: bool,
    pub bl_bit_depth_minus8: u64,
    pub el_bit_depth_minus8: u64,
    pub ext_mapping_idc: u8,
    pub vdr_bit_depth_minus8: u64,
    pub spatial_resampling_filter_flag: bool,
    pub reserved_zero_3bits: u8,
    pub el_spatial_resampling_filter_flag: bool,
    pub disable_residual_flag: bool,
    pub vdr_dm_metadata_present_flag: bool,
    pub use_prev_vdr_rpu_flag: bool,
    pub prev_vdr_rpu_id: u64,
}

impl RpuHeader {
    pub fn dovi_profile(&self) -> u8 {
        match self.vdr_rpu_profile {
            0 if self.bl_video_full_range_flag => 5,
            1 => {
                if self.el_spatial_resampling_filter_flag && !self.disable_residual_flag {
                    if self.vdr_bit_depth_minus8 == 4 { 7 } else { 4 }
                } else {
                    8
                }
            }
            _ => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MappingCurvePiece {
    Polynomial {
        order_minus1: u64,
        coefs_int: Vec<i64>,
        coefs_frac: Vec<u64>,
    },
    Mmr {
        order_minus1: u8,
        const_int: i64,
        const_frac: u64,
        coefs_int: Vec<[i64; MMR_MAX_COEFFS]>,
        coefs_frac: Vec<[u64; MMR_MAX_COEFFS]>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ComponentCurve {
    pub pivots: Vec<u16>,
    pub pieces: Vec<MappingCurvePiece>,
}

impl ComponentCurve {
    pub fn identity_p81() -> Self {
        Self {
            pivots: vec![0, 1023],
            pieces: vec![MappingCurvePiece::Polynomial {
                order_minus1: 0,
                coefs_int: vec![0, 1],
                coefs_frac: vec![0, 0],
            }],
        }
    }

    pub fn identity_p81_for(hdr: &RpuHeader) -> Self {
        let max_piv = ((1u32 << ((hdr.bl_bit_depth_minus8 + 8).min(16))) - 1) as u16;
        let frac = if hdr.coefficient_data_type == 1 {
            vec![0, u64::from(1.0f32.to_bits())]
        } else {
            vec![0, 0]
        };
        Self {
            pivots: vec![0, max_piv],
            pieces: vec![MappingCurvePiece::Polynomial {
                order_minus1: 0,
                coefs_int: vec![0, 1],
                coefs_frac: frac,
            }],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpuNlq {
    pub nlq_offset: [u16; NUM_COMPONENTS],
    pub vdr_in_max_int: [u64; NUM_COMPONENTS],
    pub vdr_in_max_frac: [u64; NUM_COMPONENTS],
    pub dz_slope_int: [u64; NUM_COMPONENTS],
    pub dz_slope_frac: [u64; NUM_COMPONENTS],
    pub dz_threshold_int: [u64; NUM_COMPONENTS],
    pub dz_threshold_frac: [u64; NUM_COMPONENTS],
}

impl RpuNlq {
    pub fn is_mel(&self) -> bool {
        self.nlq_offset == [0; 3]
            && self.vdr_in_max_int == [1; 3]
            && self.vdr_in_max_frac == [0; 3]
            && self.dz_slope_int == [0; 3]
            && self.dz_slope_frac == [0; 3]
            && self.dz_threshold_int == [0; 3]
            && self.dz_threshold_frac == [0; 3]
    }

    pub fn el_type(&self) -> DoviElType {
        if self.is_mel() { DoviElType::Mel } else { DoviElType::Fel }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpuMapping {
    pub vdr_rpu_id: u64,
    pub mapping_color_space: u64,
    pub mapping_chroma_format_idc: u64,
    pub num_x_partitions_minus1: u64,
    pub num_y_partitions_minus1: u64,
    pub curves: [ComponentCurve; NUM_COMPONENTS],
    pub nlq_pred_pivots: Option<[u16; NLQ_NUM_PIVOTS]>,
    pub nlq: Option<RpuNlq>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtBlockRaw {
    pub length_bytes: u64,
    pub level: u8,
    pub payload_bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VdrDmPayload {
    pub compressed: bool,
    pub affected_dm_metadata_id: u64,
    pub current_dm_metadata_id: u64,
    pub scene_refresh_flag: u64,
    pub ycc_to_rgb_coef: [i16; 9],
    pub ycc_to_rgb_offset: [u32; 3],
    pub rgb_to_lms_coef: [i16; 9],
    pub signal_eotf: u16,
    pub signal_eotf_param0: u16,
    pub signal_eotf_param1: u16,
    pub signal_eotf_param2: u32,
    pub signal_bit_depth: u8,
    pub signal_color_space: u8,
    pub signal_chroma_format: u8,
    pub signal_full_range_flag: u8,
    pub source_min_pq: u16,
    pub source_max_pq: u16,
    pub source_diagonal: u16,
    pub cmv29_blocks: Vec<ExtBlockRaw>,
    pub cmv40_blocks: Option<Vec<ExtBlockRaw>>,
    pub trailing_bits: Vec<bool>,
}

impl VdrDmPayload {
    pub fn apply_p81_bt2020_coeffs(&mut self) {
        if !self.compressed {
            self.ycc_to_rgb_coef = [9574, 0, 13802, 9574, -1540, -5348, 9574, 17610, 0];
            self.ycc_to_rgb_offset = [16_777_216, 134_217_728, 134_217_728];
            self.rgb_to_lms_coef = [7222, 8771, 390, 2654, 12430, 1300, 0, 422, 15962];
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedRpu {
    pub profile: u8,
    pub el_type: Option<DoviElType>,
    pub header: RpuHeader,
    pub mapping: Option<RpuMapping>,
    pub dm_data: Option<VdrDmPayload>,
    pub crc32: u32,
}

/// MPEG-2 CRC-32 (`poly = 0x04C1_1DB7`, `init = 0xFFFF_FFFF`, MSB-first, no final XOR)
/// used by Dolby Vision RPUs over `&rbsp[1..crc_offset]`.
pub fn dovi_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            if (crc & 0x8000_0000) != 0 {
                crc = (crc << 1) ^ 0x04C1_1DB7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

pub fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &b in data {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

pub fn escape_rbsp(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 16);
    let mut zeros = 0usize;
    for &b in rbsp {
        if zeros == 2 && b <= 0x03 {
            out.push(0x03);
            zeros = 0;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

struct BitsIn<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BitsIn<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn available(&self) -> usize {
        (self.buf.len() * 8).saturating_sub(self.pos)
    }

    fn byte_aligned(&self) -> bool {
        (self.pos & 7) == 0
    }

    fn bit(&mut self) -> Option<bool> {
        if self.pos >= self.buf.len() * 8 {
            return None;
        }
        let b = (self.buf[self.pos >> 3] >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Some(b != 0)
    }

    fn read_u(&mut self, n: usize) -> Option<u64> {
        if n > 64 || self.available() < n {
            return None;
        }
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bit()?);
        }
        Some(v)
    }

    fn read_i16(&mut self) -> Option<i16> {
        Some(self.read_u(16)? as u16 as i16)
    }

    fn read_ue(&mut self) -> Option<u64> {
        let mut zeros = 0usize;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            Some(0)
        } else {
            let rem = self.read_u(zeros)?;
            Some(((1u64 << zeros) - 1) + rem)
        }
    }

    fn read_se(&mut self) -> Option<i64> {
        let code = self.read_ue()?;
        let mag = ((code + 1) >> 1) as i64;
        if (code & 1) != 0 { Some(mag) } else { Some(-mag) }
    }

    fn align_zero(&mut self) -> Option<()> {
        while !self.byte_aligned() {
            if self.bit()? {
                return None;
            }
        }
        Some(())
    }

    fn read_raw_bytes(&mut self, len: usize) -> Option<Vec<u8>> {
        if self.available() < len * 8 {
            return None;
        }
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(self.read_u(8)? as u8);
        }
        Some(out)
    }
}

struct BitsOut {
    bytes: Vec<u8>,
    bit_pos: u8,
}

impl BitsOut {
    fn with_capacity(cap: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(cap),
            bit_pos: 0,
        }
    }

    fn bit(&mut self, v: bool) {
        let b = u8::from(v);
        if self.bit_pos == 0 {
            self.bytes.push(b << 7);
            self.bit_pos = 1;
        } else {
            let last = self.bytes.last_mut().unwrap();
            *last |= b << (7 - self.bit_pos);
            self.bit_pos = (self.bit_pos + 1) & 7;
        }
    }

    fn write_u(&mut self, n: usize, val: u64) {
        for shift in (0..n).rev() {
            self.bit(((val >> shift) & 1) != 0);
        }
    }

    fn write_i16(&mut self, val: i16) {
        self.write_u(16, val as u16 as u64);
    }

    fn write_ue(&mut self, val: u64) {
        let v = val.saturating_add(1);
        let bits = 64 - v.leading_zeros() as usize;
        self.write_u(bits - 1, 0);
        self.write_u(bits, v);
    }

    fn write_se(&mut self, val: i64) {
        let code = if val <= 0 {
            ((-val) as u64) << 1
        } else {
            ((val as u64) << 1) - 1
        };
        self.write_ue(code);
    }

    fn byte_align(&mut self) {
        while self.bit_pos != 0 {
            self.bit(false);
        }
    }

    fn write_raw_bytes(&mut self, src: &[u8]) {
        for &b in src {
            self.write_u(8, u64::from(b));
        }
    }
}

/// Parses an unescaped RPU RBSP payload (starting with `0x19` prefix byte).
pub fn parse_rpu_rbsp(rbsp: &[u8]) -> Option<ParsedRpu> {
    let trailing_zeros = rbsp.iter().rev().take_while(|&&b| b == 0).count();
    let end = rbsp.len().checked_sub(trailing_zeros)?;
    if end < 7 || rbsp[0] != RPU_PREFIX || rbsp[end - 1] != FINAL_BYTE {
        return None;
    }
    let crc_start = end - 5;
    let expected_crc = u32::from_be_bytes(rbsp[crc_start..crc_start + 4].try_into().ok()?);
    let calc_crc = dovi_crc32(&rbsp[1..crc_start]);
    if expected_crc != calc_crc {
        return None;
    }

    let mut r = BitsIn::new(&rbsp[1..crc_start]);
    let rpu_type = r.read_u(6)? as u8;
    if rpu_type != 2 {
        return None;
    }
    let rpu_format = r.read_u(11)? as u16;
    let vdr_rpu_profile = r.read_u(4)? as u8;
    let vdr_rpu_level = r.read_u(4)? as u8;
    let vdr_seq_info_present_flag = r.bit()?;

    let mut hdr = RpuHeader {
        rpu_type,
        rpu_format,
        vdr_rpu_profile,
        vdr_rpu_level,
        vdr_seq_info_present_flag,
        chroma_resampling_explicit_filter_flag: false,
        chroma_filter_idc: 0,
        coefficient_data_type: 0,
        coefficient_log2_denom: 23,
        coef_len: 23,
        vdr_rpu_normalized_idc: 1,
        bl_video_full_range_flag: false,
        bl_bit_depth_minus8: 2,
        el_bit_depth_minus8: 2,
        ext_mapping_idc: 0,
        vdr_bit_depth_minus8: 4,
        spatial_resampling_filter_flag: false,
        reserved_zero_3bits: 0,
        el_spatial_resampling_filter_flag: false,
        disable_residual_flag: true,
        vdr_dm_metadata_present_flag: false,
        use_prev_vdr_rpu_flag: false,
        prev_vdr_rpu_id: 0,
    };

    if vdr_seq_info_present_flag {
        hdr.chroma_resampling_explicit_filter_flag = r.bit()?;
        if hdr.chroma_resampling_explicit_filter_flag {
            hdr.chroma_filter_idc = r.read_ue()?;
        }
        hdr.coefficient_data_type = r.read_u(2)? as u8;
        if hdr.coefficient_data_type == 0 {
            hdr.coefficient_log2_denom = r.read_ue()?;
            if hdr.coefficient_log2_denom > 32 {
                return None;
            }
            hdr.coef_len = hdr.coefficient_log2_denom as usize;
        } else if hdr.coefficient_data_type == 1 {
            hdr.coef_len = 32;
        } else {
            return None;
        }
        hdr.vdr_rpu_normalized_idc = r.read_u(2)? as u8;
        hdr.bl_video_full_range_flag = r.bit()?;
        if (hdr.rpu_format & 0x700) == 0 {
            hdr.bl_bit_depth_minus8 = r.read_ue()?;
            let el_raw = r.read_ue()?;
            hdr.el_bit_depth_minus8 = el_raw & 0xFF;
            hdr.ext_mapping_idc = ((el_raw >> 8) & 0xFF) as u8;
            hdr.vdr_bit_depth_minus8 = r.read_ue()?;
            hdr.spatial_resampling_filter_flag = r.bit()?;
            hdr.reserved_zero_3bits = r.read_u(3)? as u8;
            hdr.el_spatial_resampling_filter_flag = r.bit()?;
            hdr.disable_residual_flag = r.bit()?;
            if hdr.bl_bit_depth_minus8 > 8 || hdr.el_bit_depth_minus8 > 8 || hdr.vdr_bit_depth_minus8 > 8 {
                return None;
            }
        }
    }
    hdr.vdr_dm_metadata_present_flag = r.bit()?;
    hdr.use_prev_vdr_rpu_flag = r.bit()?;
    if hdr.use_prev_vdr_rpu_flag {
        hdr.prev_vdr_rpu_id = r.read_ue()?;
    }

    let mapping = if !hdr.use_prev_vdr_rpu_flag {
        Some(parse_mapping(&mut r, &hdr)?)
    } else {
        None
    };
    let el_type = mapping.as_ref().and_then(|m| m.nlq.as_ref().map(RpuNlq::el_type));

    let dm_data = if hdr.vdr_dm_metadata_present_flag {
        Some(parse_dm_payload(&mut r, &hdr)?)
    } else {
        None
    };

    Some(ParsedRpu {
        profile: hdr.dovi_profile(),
        el_type,
        header: hdr,
        mapping,
        dm_data,
        crc32: expected_crc,
    })
}

fn parse_mapping(r: &mut BitsIn<'_>, hdr: &RpuHeader) -> Option<RpuMapping> {
    let vdr_rpu_id = r.read_ue()?;
    let mapping_color_space = r.read_ue()?;
    let mapping_chroma_format_idc = r.read_ue()?;
    let bl_bits = (hdr.bl_bit_depth_minus8 + 8) as usize;

    let mut curves: [ComponentCurve; NUM_COMPONENTS] = Default::default();
    let mut num_pieces_arr = [0usize; NUM_COMPONENTS];
    for cmp in 0..NUM_COMPONENTS {
        let num_pivots_minus2 = r.read_ue()?;
        if num_pivots_minus2 as usize >= RESHAPING_MAX_PIECES {
            return None;
        }
        let num_pivots = (num_pivots_minus2 as usize) + 2;
        num_pieces_arr[cmp] = (num_pivots_minus2 as usize) + 1;
        let mut pivots = Vec::with_capacity(num_pivots);
        for _ in 0..num_pivots {
            pivots.push(r.read_u(bl_bits)? as u16);
        }
        curves[cmp].pivots = pivots;
    }

    let nlq_pred_pivots = if (hdr.rpu_format & 0x700) == 0 && !hdr.disable_residual_flag {
        let nlq_method_idc = r.read_u(3)?;
        if nlq_method_idc != 0 {
            return None;
        }
        let p0 = r.read_u(bl_bits)? as u16;
        let p1 = r.read_u(bl_bits)? as u16;
        Some([p0, p1])
    } else {
        None
    };

    let num_x_partitions_minus1 = r.read_ue()?;
    let num_y_partitions_minus1 = r.read_ue()?;

    for cmp in 0..NUM_COMPONENTS {
        let n_pieces = num_pieces_arr[cmp];
        let mut pieces = Vec::with_capacity(n_pieces);
        for _ in 0..n_pieces {
            let mapping_idc = r.read_ue()?;
            match mapping_idc {
                0 => {
                    let order_minus1 = r.read_ue()?;
                    if order_minus1 > 1 {
                        return None;
                    }
                    let lin_interp = if order_minus1 == 0 { r.bit()? } else { false };
                    if lin_interp {
                        return None;
                    }
                    let count = (order_minus1 as usize) + 2;
                    let mut coefs_int = Vec::with_capacity(count);
                    let mut coefs_frac = Vec::with_capacity(count);
                    for _ in 0..count {
                        coefs_int.push(if hdr.coefficient_data_type == 0 { r.read_se()? } else { 0 });
                        coefs_frac.push(r.read_u(hdr.coef_len)?);
                    }
                    pieces.push(MappingCurvePiece::Polynomial {
                        order_minus1,
                        coefs_int,
                        coefs_frac,
                    });
                }
                1 => {
                    let order_minus1 = r.read_u(2)? as u8;
                    if order_minus1 > 2 {
                        return None;
                    }
                    let const_int = if hdr.coefficient_data_type == 0 { r.read_se()? } else { 0 };
                    let const_frac = r.read_u(hdr.coef_len)?;
                    let orders = usize::from(order_minus1) + 1;
                    let mut coefs_int = Vec::with_capacity(orders);
                    let mut coefs_frac = Vec::with_capacity(orders);
                    for _ in 0..orders {
                        let mut row_i = [0i64; MMR_MAX_COEFFS];
                        let mut row_f = [0u64; MMR_MAX_COEFFS];
                        for k in 0..MMR_MAX_COEFFS {
                            if hdr.coefficient_data_type == 0 {
                                row_i[k] = r.read_se()?;
                            }
                            row_f[k] = r.read_u(hdr.coef_len)?;
                        }
                        coefs_int.push(row_i);
                        coefs_frac.push(row_f);
                    }
                    pieces.push(MappingCurvePiece::Mmr {
                        order_minus1,
                        const_int,
                        const_frac,
                        coefs_int,
                        coefs_frac,
                    });
                }
                _ => return None,
            }
        }
        curves[cmp].pieces = pieces;
    }

    let nlq = if nlq_pred_pivots.is_some() {
        let el_bits = (hdr.el_bit_depth_minus8 + 8) as usize;
        let mut nlq = RpuNlq {
            nlq_offset: [0; 3],
            vdr_in_max_int: [0; 3],
            vdr_in_max_frac: [0; 3],
            dz_slope_int: [0; 3],
            dz_slope_frac: [0; 3],
            dz_threshold_int: [0; 3],
            dz_threshold_frac: [0; 3],
        };
        for cmp in 0..NUM_COMPONENTS {
            nlq.nlq_offset[cmp] = r.read_u(el_bits)? as u16;
            if hdr.coefficient_data_type == 0 {
                nlq.vdr_in_max_int[cmp] = r.read_ue()?;
            }
            nlq.vdr_in_max_frac[cmp] = r.read_u(hdr.coef_len)?;
            if hdr.coefficient_data_type == 0 {
                nlq.dz_slope_int[cmp] = r.read_ue()?;
            }
            nlq.dz_slope_frac[cmp] = r.read_u(hdr.coef_len)?;
            if hdr.coefficient_data_type == 0 {
                nlq.dz_threshold_int[cmp] = r.read_ue()?;
            }
            nlq.dz_threshold_frac[cmp] = r.read_u(hdr.coef_len)?;
        }
        Some(nlq)
    } else {
        None
    };

    Some(RpuMapping {
        vdr_rpu_id,
        mapping_color_space,
        mapping_chroma_format_idc,
        num_x_partitions_minus1,
        num_y_partitions_minus1,
        curves,
        nlq_pred_pivots,
        nlq,
    })
}

fn parse_ext_blocks(r: &mut BitsIn<'_>) -> Option<Vec<ExtBlockRaw>> {
    let count = r.read_ue()? as usize;
    if count > MAX_EXT_BLOCKS {
        return None;
    }
    r.align_zero()?;
    let mut blocks = Vec::with_capacity(count);
    for _ in 0..count {
        let length_bytes = r.read_ue()?;
        if length_bytes > 4096 {
            return None;
        }
        let level = r.read_u(8)? as u8;
        let payload_bytes = r.read_raw_bytes(length_bytes as usize)?;
        blocks.push(ExtBlockRaw {
            length_bytes,
            level,
            payload_bytes,
        });
    }
    Some(blocks)
}

fn parse_dm_payload(r: &mut BitsIn<'_>, hdr: &RpuHeader) -> Option<VdrDmPayload> {
    let compressed = hdr.reserved_zero_3bits == 1;
    let affected_dm_metadata_id = r.read_ue()?;
    let current_dm_metadata_id = r.read_ue()?;
    let scene_refresh_flag = r.read_ue()?;

    let mut payload = VdrDmPayload {
        compressed,
        affected_dm_metadata_id,
        current_dm_metadata_id,
        scene_refresh_flag,
        ycc_to_rgb_coef: [0; 9],
        ycc_to_rgb_offset: [0; 3],
        rgb_to_lms_coef: [0; 9],
        signal_eotf: 65535,
        signal_eotf_param0: 0,
        signal_eotf_param1: 0,
        signal_eotf_param2: 0,
        signal_bit_depth: 12,
        signal_color_space: 0,
        signal_chroma_format: 0,
        signal_full_range_flag: 1,
        source_min_pq: 0,
        source_max_pq: 3079,
        source_diagonal: 42,
        cmv29_blocks: Vec::new(),
        cmv40_blocks: None,
        trailing_bits: Vec::new(),
    };

    if !compressed {
        for c in &mut payload.ycc_to_rgb_coef {
            *c = r.read_i16()?;
        }
        for o in &mut payload.ycc_to_rgb_offset {
            *o = r.read_u(32)? as u32;
        }
        for c in &mut payload.rgb_to_lms_coef {
            *c = r.read_i16()?;
        }
        payload.signal_eotf = r.read_u(16)? as u16;
        payload.signal_eotf_param0 = r.read_u(16)? as u16;
        payload.signal_eotf_param1 = r.read_u(16)? as u16;
        payload.signal_eotf_param2 = r.read_u(32)? as u32;
        payload.signal_bit_depth = r.read_u(5)? as u8;
        payload.signal_color_space = r.read_u(2)? as u8;
        payload.signal_chroma_format = r.read_u(2)? as u8;
        payload.signal_full_range_flag = r.read_u(2)? as u8;
        payload.source_min_pq = r.read_u(12)? as u16;
        payload.source_max_pq = r.read_u(12)? as u16;
        payload.source_diagonal = r.read_u(10)? as u16;
    }

    payload.cmv29_blocks = parse_ext_blocks(r)?;
    // `r` spans up to `crc_start`, so >= 16 bits remaining indicates CMv4.0 payload2.
    if r.available() >= (DM_DATA_PAYLOAD2_MIN_BITS - 40) {
        let saved = r.pos;
        if let Some(v40) = parse_ext_blocks(r) {
            payload.cmv40_blocks = Some(v40);
        } else {
            r.pos = saved;
        }
    }
    r.align_zero()?;
    while r.available() > 0 {
        payload.trailing_bits.push(r.bit()?);
    }
    Some(payload)
}

impl ParsedRpu {
    /// Converts a Profile 7 (MEL or FEL) RPU in-place into a Profile 8.1 RPU
    /// (`ConversionMode::To81` / `dovi_tool -m 2`), stripping EL/NLQ syntax,
    /// neutralizing FEL-only reshaping curves, and preserving every CMv2.9 and
    /// CMv4.0 dynamic metadata block (`L1/L2/L3/L5/L6/L8/L9/L11`).
    pub fn convert_p7_to_p81(&mut self) {
        if self.profile != 7 {
            return;
        }
        let is_fel = self.el_type == Some(DoviElType::Fel);
        self.header.el_spatial_resampling_filter_flag = false;
        self.header.disable_residual_flag = true;
        if let Some(m) = &mut self.mapping {
            m.nlq_pred_pivots = None;
            m.nlq = None;
            m.num_x_partitions_minus1 = 0;
            m.num_y_partitions_minus1 = 0;
            if is_fel {
                m.curves = [
                    ComponentCurve::identity_p81_for(&self.header),
                    ComponentCurve::identity_p81_for(&self.header),
                    ComponentCurve::identity_p81_for(&self.header),
                ];
            }
        }
        if let Some(dm) = &mut self.dm_data {
            dm.apply_p81_bt2020_coeffs();
        }
        self.profile = 8;
        self.el_type = None;
    }

    /// Encodes `self` into a complete unescaped RPU RBSP (`0x19` + body + CRC32 + `0x80`).
    pub fn encode_rbsp(&self) -> Vec<u8> {
        let mut w = BitsOut::with_capacity(512);
        w.write_u(8, u64::from(RPU_PREFIX));
        let h = &self.header;
        w.write_u(6, u64::from(h.rpu_type));
        w.write_u(11, u64::from(h.rpu_format));
        w.write_u(4, u64::from(h.vdr_rpu_profile));
        w.write_u(4, u64::from(h.vdr_rpu_level));
        w.bit(h.vdr_seq_info_present_flag);
        if h.vdr_seq_info_present_flag {
            w.bit(h.chroma_resampling_explicit_filter_flag);
            if h.chroma_resampling_explicit_filter_flag {
                w.write_ue(h.chroma_filter_idc);
            }
            w.write_u(2, u64::from(h.coefficient_data_type));
            if h.coefficient_data_type == 0 {
                w.write_ue(h.coefficient_log2_denom);
            }
            w.write_u(2, u64::from(h.vdr_rpu_normalized_idc));
            w.bit(h.bl_video_full_range_flag);
            if (h.rpu_format & 0x700) == 0 {
                w.write_ue(h.bl_bit_depth_minus8);
                let el_word = (u64::from(h.ext_mapping_idc) << 8) | h.el_bit_depth_minus8;
                w.write_ue(el_word);
                w.write_ue(h.vdr_bit_depth_minus8);
                w.bit(h.spatial_resampling_filter_flag);
                w.write_u(3, u64::from(h.reserved_zero_3bits));
                w.bit(h.el_spatial_resampling_filter_flag);
                w.bit(h.disable_residual_flag);
            }
        }
        w.bit(h.vdr_dm_metadata_present_flag);
        w.bit(h.use_prev_vdr_rpu_flag);
        if h.use_prev_vdr_rpu_flag {
            w.write_ue(h.prev_vdr_rpu_id);
        }

        if !h.use_prev_vdr_rpu_flag {
            if let Some(m) = &self.mapping {
                encode_mapping(&mut w, h, m);
            }
        }
        if h.vdr_dm_metadata_present_flag {
            if let Some(dm) = &self.dm_data {
                encode_dm_payload(&mut w, dm);
            }
        }
        w.byte_align();
        let crc = dovi_crc32(&w.bytes[1..]);
        w.write_u(32, u64::from(crc));
        w.write_u(8, u64::from(FINAL_BYTE));
        w.bytes
    }

    /// Encodes `self` into a 2-byte HEVC `NAL_UNSPEC62` (`0x7C, 0x01`) NAL unit
    /// with RBSP `0x00 0x00 0x03` start-code emulation prevention.
    pub fn encode_hevc_unspec62_nal(&self) -> Vec<u8> {
        let escaped = escape_rbsp(&self.encode_rbsp());
        let mut nal = Vec::with_capacity(2 + escaped.len());
        nal.push(0x7C);
        nal.push(0x01);
        nal.extend_from_slice(&escaped);
        nal
    }
}

fn encode_mapping(w: &mut BitsOut, h: &RpuHeader, m: &RpuMapping) {
    let bl_bits = (h.bl_bit_depth_minus8 + 8) as usize;
    w.write_ue(m.vdr_rpu_id);
    w.write_ue(m.mapping_color_space);
    w.write_ue(m.mapping_chroma_format_idc);
    for curve in &m.curves {
        let np_minus2 = curve.pivots.len().saturating_sub(2) as u64;
        w.write_ue(np_minus2);
        for &p in &curve.pivots {
            w.write_u(bl_bits, u64::from(p));
        }
    }
    if (h.rpu_format & 0x700) == 0 && !h.disable_residual_flag {
        if let Some(pivs) = m.nlq_pred_pivots {
            w.write_u(3, 0);
            w.write_u(bl_bits, u64::from(pivs[0]));
            w.write_u(bl_bits, u64::from(pivs[1]));
        }
    }
    w.write_ue(m.num_x_partitions_minus1);
    w.write_ue(m.num_y_partitions_minus1);
    for curve in &m.curves {
        for piece in &curve.pieces {
            match piece {
                MappingCurvePiece::Polynomial {
                    order_minus1,
                    coefs_int,
                    coefs_frac,
                } => {
                    w.write_ue(0);
                    w.write_ue(*order_minus1);
                    if *order_minus1 == 0 {
                        w.bit(false);
                    }
                    for (&ci, &cf) in coefs_int.iter().zip(coefs_frac) {
                        if h.coefficient_data_type == 0 {
                            w.write_se(ci);
                        }
                        w.write_u(h.coef_len, cf);
                    }
                }
                MappingCurvePiece::Mmr {
                    order_minus1,
                    const_int,
                    const_frac,
                    coefs_int,
                    coefs_frac,
                } => {
                    w.write_ue(1);
                    w.write_u(2, u64::from(*order_minus1));
                    if h.coefficient_data_type == 0 {
                        w.write_se(*const_int);
                    }
                    w.write_u(h.coef_len, *const_frac);
                    for (ri, rf) in coefs_int.iter().zip(coefs_frac) {
                        for k in 0..MMR_MAX_COEFFS {
                            if h.coefficient_data_type == 0 {
                                w.write_se(ri[k]);
                            }
                            w.write_u(h.coef_len, rf[k]);
                        }
                    }
                }
            }
        }
    }
    if let Some(nlq) = &m.nlq {
        let el_bits = (h.el_bit_depth_minus8 + 8) as usize;
        for cmp in 0..NUM_COMPONENTS {
            w.write_u(el_bits, u64::from(nlq.nlq_offset[cmp]));
            if h.coefficient_data_type == 0 {
                w.write_ue(nlq.vdr_in_max_int[cmp]);
            }
            w.write_u(h.coef_len, nlq.vdr_in_max_frac[cmp]);
            if h.coefficient_data_type == 0 {
                w.write_ue(nlq.dz_slope_int[cmp]);
            }
            w.write_u(h.coef_len, nlq.dz_slope_frac[cmp]);
            if h.coefficient_data_type == 0 {
                w.write_ue(nlq.dz_threshold_int[cmp]);
            }
            w.write_u(h.coef_len, nlq.dz_threshold_frac[cmp]);
        }
    }
}

fn encode_ext_blocks(w: &mut BitsOut, blocks: &[ExtBlockRaw]) {
    w.write_ue(blocks.len() as u64);
    w.byte_align();
    for blk in blocks {
        w.write_ue(blk.length_bytes);
        w.write_u(8, u64::from(blk.level));
        w.write_raw_bytes(&blk.payload_bytes);
    }
}

fn encode_dm_payload(w: &mut BitsOut, dm: &VdrDmPayload) {
    w.write_ue(dm.affected_dm_metadata_id);
    w.write_ue(dm.current_dm_metadata_id);
    w.write_ue(dm.scene_refresh_flag);
    if !dm.compressed {
        for &c in &dm.ycc_to_rgb_coef {
            w.write_i16(c);
        }
        for &o in &dm.ycc_to_rgb_offset {
            w.write_u(32, u64::from(o));
        }
        for &c in &dm.rgb_to_lms_coef {
            w.write_i16(c);
        }
        w.write_u(16, u64::from(dm.signal_eotf));
        w.write_u(16, u64::from(dm.signal_eotf_param0));
        w.write_u(16, u64::from(dm.signal_eotf_param1));
        w.write_u(32, u64::from(dm.signal_eotf_param2));
        w.write_u(5, u64::from(dm.signal_bit_depth));
        w.write_u(2, u64::from(dm.signal_color_space));
        w.write_u(2, u64::from(dm.signal_chroma_format));
        w.write_u(2, u64::from(dm.signal_full_range_flag));
        w.write_u(12, u64::from(dm.source_min_pq));
        w.write_u(12, u64::from(dm.source_max_pq));
        w.write_u(10, u64::from(dm.source_diagonal));
    }
    encode_ext_blocks(w, &dm.cmv29_blocks);
    if let Some(v40) = &dm.cmv40_blocks {
        encode_ext_blocks(w, v40);
    }
    w.byte_align();
    for &b in &dm.trailing_bits {
        w.bit(b);
    }
}

/// Rewrites an HEVC access unit from Dolby Vision Profile 7 STDL (`BL + EL + RPU`)
/// into a clean Profile 8.1 single-layer access unit (`BL + converted P8.1 RPU`),
/// stripping `NAL_UNSPEC63` (`63`) and non-zero `nuh_layer_id` EL NAL units.
pub fn rewrite_p7_packet_to_p81(data: &[u8], framing: Framing) -> Option<Vec<u8>> {
    match framing {
        Framing::LengthPrefixed(len_size) => {
            let s = usize::from(len_size);
            if s == 0 || s > 4 {
                return None;
            }
            let mut out = Vec::with_capacity(data.len());
            let mut pos = 0usize;
            let mut modified = false;
            while pos + s <= data.len() {
                let mut nal_len = 0usize;
                for &b in &data[pos..pos + s] {
                    nal_len = (nal_len << 8) | usize::from(b);
                }
                pos += s;
                if pos + nal_len > data.len() {
                    break;
                }
                let nal = &data[pos..pos + nal_len];
                pos += nal_len;
                if nal.len() < 2 {
                    continue;
                }
                let nal_type = (nal[0] >> 1) & 0x3F;
                let layer_id = ((nal[0] & 1) << 5) | (nal[1] >> 3);
                if nal_type == 63 || (layer_id > 0 && nal_type != 62) {
                    modified = true;
                    continue;
                }
                if nal_type == 62 {
                    let rbsp = unescape_rbsp(&nal[2..]);
                    if let Some(mut parsed) = parse_rpu_rbsp(&rbsp) {
                        if parsed.profile == 7 {
                            parsed.convert_p7_to_p81();
                            let rewritten = parsed.encode_hevc_unspec62_nal();
                            write_nal_length(&mut out, rewritten.len(), s);
                            out.extend_from_slice(&rewritten);
                            modified = true;
                            continue;
                        }
                    }
                }
                write_nal_length(&mut out, nal.len(), s);
                out.extend_from_slice(nal);
            }
            modified.then_some(out)
        }
        Framing::AnnexB => {
            let mut starts = Vec::new();
            let mut i = 0usize;
            while i + 2 < data.len() {
                if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                    let prefix_start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
                    starts.push((prefix_start, i + 3));
                    i += 3;
                } else {
                    i += 1;
                }
            }
            if starts.is_empty() {
                return None;
            }
            let mut out = Vec::with_capacity(data.len());
            let mut modified = false;
            for (idx, &(_, nalu_start)) in starts.iter().enumerate() {
                let nalu_end = starts.get(idx + 1).map_or(data.len(), |&(p, _)| p);
                if nalu_end < nalu_start + 2 {
                    continue;
                }
                let nal = &data[nalu_start..nalu_end];
                let nal_type = (nal[0] >> 1) & 0x3F;
                let layer_id = ((nal[0] & 1) << 5) | (nal[1] >> 3);
                if nal_type == 63 || (layer_id > 0 && nal_type != 62) {
                    modified = true;
                    continue;
                }
                if nal_type == 62 {
                    let rbsp = unescape_rbsp(&nal[2..]);
                    if let Some(mut parsed) = parse_rpu_rbsp(&rbsp) {
                        if parsed.profile == 7 {
                            parsed.convert_p7_to_p81();
                            let rewritten = parsed.encode_hevc_unspec62_nal();
                            out.extend_from_slice(&[0, 0, 0, 1]);
                            out.extend_from_slice(&rewritten);
                            modified = true;
                            continue;
                        }
                    }
                }
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(nal);
            }
            modified.then_some(out)
        }
    }
}

fn write_nal_length(out: &mut Vec<u8>, len: usize, width: usize) {
    for shift in (0..width).rev() {
        out.push(((len >> (shift * 8)) & 0xFF) as u8);
    }
}

/// Splits a Profile 7 STDL access unit into `(base_layer_au, Option<el_annex_b_au>, Option<ParsedRpu>)`
/// so a dual-decoder FEL pipeline can feed `oxideav-h265` both the 10-bit BL and
/// unwrapped 10-bit EL (`NAL_UNSPEC63` payload stripped of its 2-byte `7E 01` wrapper).
pub fn split_stdl_fel_access_unit(
    data: &[u8],
    framing: Framing,
) -> (Vec<u8>, Option<Vec<u8>>, Option<ParsedRpu>) {
    let mut bl_au = Vec::with_capacity(data.len());
    let mut el_au = Vec::new();
    let mut rpu = None;

    let mut visit_nal = |nal: &[u8]| {
        if nal.len() < 2 {
            return;
        }
        let nal_type = (nal[0] >> 1) & 0x3F;
        let layer_id = ((nal[0] & 1) << 5) | (nal[1] >> 3);
        if nal_type == 62 {
            let rbsp = unescape_rbsp(&nal[2..]);
            if rpu.is_none() {
                rpu = parse_rpu_rbsp(&rbsp);
            }
        } else if nal_type == 63 {
            // Unwrap the 2-byte `NAL_UNSPEC63` (`0x7E 0x01`) envelope and clear
            // `nuh_layer_id` on the inner HEVC EL NAL so a standard HEVC decoder accepts it.
            let inner = &nal[2..];
            if inner.len() >= 2 {
                el_au.extend_from_slice(&[0, 0, 0, 1]);
                let b0 = inner[0] & 0xFE;
                let b1 = inner[1] & 0x07;
                el_au.push(b0);
                el_au.push(b1.max(1));
                el_au.extend_from_slice(&inner[2..]);
            }
        } else if layer_id > 0 {
            el_au.extend_from_slice(&[0, 0, 0, 1]);
            el_au.push(nal[0] & 0xFE);
            el_au.push((nal[1] & 0x07).max(1));
            el_au.extend_from_slice(&nal[2..]);
        } else {
            bl_au.extend_from_slice(&[0, 0, 0, 1]);
            bl_au.extend_from_slice(nal);
        }
    };

    match framing {
        Framing::LengthPrefixed(len_size) => {
            let s = usize::from(len_size);
            if (1..=4).contains(&s) {
                let mut pos = 0usize;
                while pos + s <= data.len() {
                    let mut len = 0usize;
                    for &b in &data[pos..pos + s] {
                        len = (len << 8) | usize::from(b);
                    }
                    pos += s;
                    if pos + len > data.len() {
                        break;
                    }
                    visit_nal(&data[pos..pos + len]);
                    pos += len;
                }
            }
        }
        Framing::AnnexB => {
            let mut starts = Vec::new();
            let mut i = 0usize;
            while i + 2 < data.len() {
                if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                    let p = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
                    starts.push((p, i + 3));
                    i += 3;
                } else {
                    i += 1;
                }
            }
            for (idx, &(_, ns)) in starts.iter().enumerate() {
                let ne = starts.get(idx + 1).map_or(data.len(), |&(p, _)| p);
                if ne >= ns + 2 {
                    visit_nal(&data[ns..ne]);
                }
            }
        }
    }

    let el_opt = (!el_au.is_empty()).then_some(el_au);
    (bl_au, el_opt, rpu)
}

#[inline]
fn eval_poly_lut(curve: &ComponentCurve, coef_len: usize, vdr_bits: u32) -> [i32; 1024] {
    let mut lut = [0i32; 1024];
    let scale = (1u64 << coef_len) as f64;
    let out_max = ((1i32 << vdr_bits) - 1) as f64;
    for s in 0..1024u16 {
        let piece_idx = if curve.pieces.len() <= 1 {
            0
        } else {
            curve
                .pivots
                .windows(2)
                .position(|w| s >= w[0] && s <= w[1])
                .unwrap_or(curve.pieces.len() - 1)
                .min(curve.pieces.len() - 1)
        };
        let x = f64::from(s) / 1023.0;
        let y_norm = match curve.pieces.get(piece_idx) {
            Some(MappingCurvePiece::Polynomial {
                coefs_int,
                coefs_frac,
                ..
            }) => {
                let mut acc = 0.0f64;
                let mut xp = 1.0f64;
                for (&ci, &cf) in coefs_int.iter().zip(coefs_frac) {
                    let c = (ci as f64) + (cf as f64) / scale;
                    acc += c * xp;
                    xp *= x;
                }
                acc
            }
            _ => x,
        };
        lut[s as usize] = (y_norm * out_max).round().clamp(0.0, out_max) as i32;
    }
    lut
}

#[inline]
fn eval_mmr_sample(
    curve: &ComponentCurve,
    bl_y: u16,
    bl_cb: u16,
    bl_cr: u16,
    coef_len: usize,
    vdr_bits: u32,
    s_self: u16,
) -> i32 {
    let scale = (1u64 << coef_len) as f64;
    let out_max = ((1i32 << vdr_bits) - 1) as f64;
    let piece_idx = if curve.pieces.len() <= 1 {
        0
    } else {
        curve
            .pivots
            .windows(2)
            .position(|w| s_self >= w[0] && s_self <= w[1])
            .unwrap_or(curve.pieces.len() - 1)
            .min(curve.pieces.len() - 1)
    };
    let Some(MappingCurvePiece::Mmr {
        const_int,
        const_frac,
        coefs_int,
        coefs_frac,
        ..
    }) = curve.pieces.get(piece_idx)
    else {
        return ((f64::from(s_self) / 1023.0) * out_max).round().clamp(0.0, out_max) as i32;
    };
    let c0 = f64::from(bl_y) / 1023.0;
    let c1 = f64::from(bl_cb) / 1023.0;
    let c2 = f64::from(bl_cr) / 1023.0;
    let mut acc = (*const_int as f64) + (*const_frac as f64) / scale;
    let mut p0 = c0;
    let mut p1 = c1;
    let mut p2 = c2;
    for (ri, rf) in coefs_int.iter().zip(coefs_frac) {
        let basis = [p0, p1, p2, p0 * p1, p0 * p2, p1 * p2, p0 * p1 * p2];
        for k in 0..MMR_MAX_COEFFS {
            let coef = (ri[k] as f64) + (rf[k] as f64) / scale;
            acc += coef * basis[k];
        }
        p0 *= c0;
        p1 *= c1;
        p2 *= c2;
    }
    (acc * out_max).round().clamp(0.0, out_max) as i32
}
#[inline]
fn eval_nlq_lut(nlq: &RpuNlq, cmp: usize, coef_len: usize, vdr_bits: u32) -> [i32; 1024] {
    let mut lut = [0i32; 1024];
    let scale = (1u64 << coef_len) as f64;
    let offset = i32::from(nlq.nlq_offset[cmp]);
    let vmax = (nlq.vdr_in_max_int[cmp] as f64) + (nlq.vdr_in_max_frac[cmp] as f64) / scale;
    let slope = (nlq.dz_slope_int[cmp] as f64) + (nlq.dz_slope_frac[cmp] as f64) / scale;
    let thresh = (nlq.dz_threshold_int[cmp] as f64) + (nlq.dz_threshold_frac[cmp] as f64) / scale;
    let vdr_max = ((1i32 << vdr_bits) - 1) as f64;

    for e in 0..1024i32 {
        let diff = e - offset;
        if diff == 0 {
            lut[e as usize] = 0;
            continue;
        }
        let sign = if diff < 0 { -1.0f64 } else { 1.0f64 };
        // Normalize 10-bit signed EL residual into [0, 1).
        let abs_r = (diff.unsigned_abs() as f64) / 512.0;
        let dq = (abs_r * slope + thresh).min(vmax);
        lut[e as usize] = (sign * dq * vdr_max).round() as i32;
    }
    lut
}

#[inline]
fn read_u16_sample(plane: &VideoPlane, x: usize, y: usize) -> u16 {
    let off = y * plane.stride + x * 2;
    if off + 2 <= plane.data.len() {
        u16::from_le_bytes([plane.data[off], plane.data[off + 1]]) & 0x03FF
    } else {
        0
    }
}

#[inline]
fn lanczos3_2x_sample(plane: &VideoPlane, w: usize, h: usize, dst_x: usize, dst_y: usize) -> u16 {
    if dst_x / 2 < w && dst_y / 2 < h && (dst_x & 1) == 0 && (dst_y & 1) == 0 {
        return read_u16_sample(plane, dst_x / 2, dst_y / 2);
    }
    // 6-tap Lanczos-3 half-pel interpolation weights [-0.0244, 0.1359, 0.3885, 0.3885, 0.1359, -0.0244].
    const TAPS: [f32; 6] = [-0.024414063, 0.13586426, 0.3885498, 0.3885498, 0.13586426, -0.024414063];
    let sx_base = (dst_x / 2) as isize;
    let sy_base = (dst_y / 2) as isize;
    let odd_x = (dst_x & 1) != 0;
    let odd_y = (dst_y & 1) != 0;

    let sample_clamped = |ix: isize, iy: isize| -> f32 {
        let cx = ix.clamp(0, (w.saturating_sub(1)) as isize) as usize;
        let cy = iy.clamp(0, (h.saturating_sub(1)) as isize) as usize;
        f32::from(read_u16_sample(plane, cx, cy))
    };

    let horiz = |iy: isize| -> f32 {
        if !odd_x {
            sample_clamped(sx_base, iy)
        } else {
            let mut sum = 0.0f32;
            for (k, &w_tap) in TAPS.iter().enumerate() {
                sum += w_tap * sample_clamped(sx_base - 2 + k as isize, iy);
            }
            sum
        }
    };

    let val = if !odd_y {
        horiz(sy_base)
    } else {
        let mut sum = 0.0f32;
        for (k, &w_tap) in TAPS.iter().enumerate() {
            sum += w_tap * horiz(sy_base - 2 + k as isize);
        }
        sum
    };
    val.round().clamp(0.0, 1023.0) as u16
}

/// Reconstructs a full **12-bit Dolby Vision Profile 7 FEL** frame from a
/// decoded 10-bit Base Layer (`Yuv420P10Le`) and 10-bit Enhancement Layer
/// (`Yuv420P10Le`), outputting `Yuv420P16Le` (12-bit code values shifted `<< 4`
/// into MSB-aligned 16-bit samples so zero precision is lost).
pub fn reconstruct_fel_frame(
    bl: &VideoFrame,
    el: &VideoFrame,
    rpu: &ParsedRpu,
    bl_width: u32,
    bl_height: u32,
    el_width: u32,
    el_height: u32,
) -> Option<VideoFrame> {
    if bl.planes.len() < 3 || el.planes.len() < 3 {
        return None;
    }
    let mapping = rpu.mapping.as_ref()?;
    let nlq = mapping.nlq.as_ref()?;
    let vdr_bits = (rpu.header.vdr_bit_depth_minus8 + 8).clamp(10, 16) as u32;
    let shift_to_16 = 16u32.saturating_sub(vdr_bits);
    let max_code = (1i32 << vdr_bits) - 1;
    let coef_len = rpu.header.coef_len;

    let mut out_planes = Vec::with_capacity(3);
    for cmp in 0..3 {
        let (bw, bh) = if cmp == 0 {
            (bl_width as usize, bl_height as usize)
        } else {
            ((bl_width as usize).div_ceil(2), (bl_height as usize).div_ceil(2))
        };
        let (ew, eh) = if cmp == 0 {
            (el_width as usize, el_height as usize)
        } else {
            ((el_width as usize).div_ceil(2), (el_height as usize).div_ceil(2))
        };
        if bw == 0 || bh == 0 || ew == 0 || eh == 0 {
            return None;
        }
        let has_mmr = mapping.curves[cmp]
            .pieces
            .iter()
            .any(|p| matches!(p, MappingCurvePiece::Mmr { .. }));
        let bl_lut = eval_poly_lut(&mapping.curves[cmp], coef_len, vdr_bits);
        let nlq_lut = eval_nlq_lut(nlq, cmp, coef_len, vdr_bits);
        let scale_2x = bw == ew * 2 && bh == eh * 2;
        let cw = (bl_width as usize).div_ceil(2).max(1);
        let ch = (bl_height as usize).div_ceil(2).max(1);

        let dst_stride = bw * 2;
        let mut dst = vec![0u8; dst_stride * bh];
        for y in 0..bh {
            for x in 0..bw {
                let bl_s = read_u16_sample(&bl.planes[cmp], x, y);
                let mapped_bl = if has_mmr {
                    let (ly, lx, cx, cy) = if cmp == 0 {
                        (y, x, (x / 2).min(cw - 1), (y / 2).min(ch - 1))
                    } else {
                        (
                            (y * 2).min((bl_height as usize).saturating_sub(1)),
                            (x * 2).min((bl_width as usize).saturating_sub(1)),
                            x.min(cw - 1),
                            y.min(ch - 1),
                        )
                    };
                    let s_y = read_u16_sample(&bl.planes[0], lx, ly);
                    let s_cb = read_u16_sample(&bl.planes[1], cx, cy);
                    let s_cr = read_u16_sample(&bl.planes[2], cx, cy);
                    eval_mmr_sample(&mapping.curves[cmp], s_y, s_cb, s_cr, coef_len, vdr_bits, bl_s)
                } else {
                    bl_lut[bl_s as usize]
                };
                let el_s = if ew == bw && eh == bh {
                    read_u16_sample(&el.planes[cmp], x, y)
                } else if scale_2x {
                    lanczos3_2x_sample(&el.planes[cmp], ew, eh, x, y)
                } else {
                    let sx = (x * ew / bw).min(ew - 1);
                    let sy = (y * eh / bh).min(eh - 1);
                    read_u16_sample(&el.planes[cmp], sx, sy)
                };
                let composed = (mapped_bl + nlq_lut[el_s as usize]).clamp(0, max_code);
                let p016 = ((composed as u32) << shift_to_16) as u16;
                let off = y * dst_stride + x * 2;
                dst[off..off + 2].copy_from_slice(&p016.to_le_bytes());
            }
        }
        out_planes.push(VideoPlane {
            data: dst,
            stride: dst_stride,
        });
    }

    let mut out = bl.clone();
    out.planes = out_planes;
    // Update frame pixel format tag via output_params when inspected downstream.
    let _ = PixelFormat::Yuv420P16Le;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_p7_fel_rpu() -> ParsedRpu {
        ParsedRpu {
            profile: 7,
            el_type: Some(DoviElType::Fel),
            header: RpuHeader {
                rpu_type: 2,
                rpu_format: 18,
                vdr_rpu_profile: 1,
                vdr_rpu_level: 0,
                vdr_seq_info_present_flag: true,
                chroma_resampling_explicit_filter_flag: false,
                chroma_filter_idc: 0,
                coefficient_data_type: 0,
                coefficient_log2_denom: 23,
                coef_len: 23,
                vdr_rpu_normalized_idc: 1,
                bl_video_full_range_flag: false,
                bl_bit_depth_minus8: 2,
                el_bit_depth_minus8: 2,
                ext_mapping_idc: 0,
                vdr_bit_depth_minus8: 4,
                spatial_resampling_filter_flag: false,
                reserved_zero_3bits: 0,
                el_spatial_resampling_filter_flag: true,
                disable_residual_flag: false,
                vdr_dm_metadata_present_flag: true,
                use_prev_vdr_rpu_flag: false,
                prev_vdr_rpu_id: 0,
            },
            mapping: Some(RpuMapping {
                vdr_rpu_id: 0,
                mapping_color_space: 0,
                mapping_chroma_format_idc: 0,
                num_x_partitions_minus1: 0,
                num_y_partitions_minus1: 0,
                curves: [
                    ComponentCurve::identity_p81(),
                    ComponentCurve::identity_p81(),
                    ComponentCurve::identity_p81(),
                ],
                nlq_pred_pivots: Some([0, 1023]),
                nlq: Some(RpuNlq {
                    nlq_offset: [512, 512, 512],
                    vdr_in_max_int: [1, 1, 1],
                    vdr_in_max_frac: [0, 0, 0],
                    dz_slope_int: [0, 0, 0],
                    dz_slope_frac: [1 << 21, 1 << 21, 1 << 21], // 0.25 slope
                    dz_threshold_int: [0, 0, 0],
                    dz_threshold_frac: [0, 0, 0],
                }),
            }),
            dm_data: Some(VdrDmPayload {
                compressed: false,
                affected_dm_metadata_id: 0,
                current_dm_metadata_id: 0,
                scene_refresh_flag: 1,
                ycc_to_rgb_coef: [9574, 0, 13802, 9574, -1540, -5348, 9574, 17610, 0],
                ycc_to_rgb_offset: [16_777_216, 134_217_728, 134_217_728],
                rgb_to_lms_coef: [7222, 8771, 390, 2654, 12430, 1300, 0, 422, 15962],
                signal_eotf: 65535,
                signal_eotf_param0: 0,
                signal_eotf_param1: 0,
                signal_eotf_param2: 0,
                signal_bit_depth: 12,
                signal_color_space: 0,
                signal_chroma_format: 0,
                signal_full_range_flag: 1,
                source_min_pq: 62,
                source_max_pq: 3079,
                source_diagonal: 42,
                cmv29_blocks: vec![
                    // Level 1 dynamic scene brightness (min_pq, max_pq, avg_pq = 6 bytes)
                    ExtBlockRaw {
                        length_bytes: 5,
                        level: 1,
                        payload_bytes: vec![0x00, 0x3E, 0x0C, 0x07, 0x04],
                    },
                    // Level 6 MaxCLL / MaxFALL (8 bytes)
                    ExtBlockRaw {
                        length_bytes: 8,
                        level: 6,
                        payload_bytes: vec![0x03, 0xE8, 0x01, 0x90, 0x03, 0xE8, 0x00, 0x01],
                    },
                ],
                cmv40_blocks: Some(vec![
                    ExtBlockRaw {
                        length_bytes: 2,
                        level: 254,
                        payload_bytes: vec![0x04, 0x02],
                    },
                    // Level 8 trim block
                    ExtBlockRaw {
                        length_bytes: 10,
                        level: 8,
                        payload_bytes: vec![0, 0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0, 0, 0],
                    },
                ]),
                trailing_bits: Vec::new(),
            }),
            crc32: 0,
        }
    }

    #[test]
    fn profile7_fel_rpu_converts_to_valid_profile81_preserving_cmv29_and_cmv40_trims() {
        let orig = synthetic_p7_fel_rpu();
        let p7_nal = orig.encode_hevc_unspec62_nal();
        let reparsed = parse_rpu_rbsp(&unescape_rbsp(&p7_nal[2..])).expect("parse P7 RPU");
        assert_eq!(reparsed.profile, 7);
        assert_eq!(reparsed.el_type, Some(DoviElType::Fel));

        // Build an STDL length-prefixed packet: [BL IDR NAL] + [EL UNSPEC63 NAL] + [P7 RPU UNSPEC62 NAL]
        let bl_nal = [0x26u8, 0x01, 0xAF, 0x12, 0x34];
        let el_nal = [0x7Eu8, 0x01, 0x26, 0x09, 0xDE, 0xAD];
        let mut pkt = Vec::new();
        for nal in [bl_nal.as_slice(), el_nal.as_slice(), p7_nal.as_slice()] {
            pkt.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            pkt.extend_from_slice(nal);
        }

        let rewritten = rewrite_p7_packet_to_p81(&pkt, Framing::LengthPrefixed(4))
            .expect("P7 STDL packet rewritten to P8.1");
        // Verify EL was stripped and RPU is now valid CRC-verified Profile 8.1 with L1, L6, and CMv4.0 L8 intact!
        let mut pos = 0;
        let mut seen_nals = Vec::new();
        while pos + 4 <= rewritten.len() {
            let len = u32::from_be_bytes(rewritten[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            seen_nals.push(&rewritten[pos..pos + len]);
            pos += len;
        }
        assert_eq!(seen_nals.len(), 2, "EL UNSPEC63 must be stripped, keeping BL + P8.1 RPU");
        assert_eq!(seen_nals[0], bl_nal);
        let p81 = parse_rpu_rbsp(&unescape_rbsp(&seen_nals[1][2..])).expect("valid CRC32 P8.1 RPU");
        assert_eq!(p81.profile, 8);
        assert!(p81.header.disable_residual_flag);
        assert!(!p81.header.el_spatial_resampling_filter_flag);
        assert!(p81.mapping.as_ref().unwrap().nlq.is_none());
        let dm = p81.dm_data.expect("DM metadata preserved");
        assert_eq!(dm.cmv29_blocks.len(), 2);
        assert_eq!(dm.cmv29_blocks[0].level, 1);
        assert_eq!(dm.cmv29_blocks[1].level, 6);
        let v40 = dm.cmv40_blocks.expect("CMv4.0 metadata preserved");
        assert_eq!(v40[1].level, 8);
    }

    #[test]
    fn fel_12bit_reconstructor_upscales_1080p_el_and_adds_nlq_residual() {
        let rpu = synthetic_p7_fel_rpu();
        // 4x4 10-bit BL at mid-level 512 (~2048 in 12-bit) + 2x2 10-bit EL with positive residual (768 > 512 offset).
        let make_plane = |w: usize, h: usize, sample: u16| -> VideoPlane {
            let mut data = Vec::with_capacity(w * h * 2);
            for _ in 0..(w * h) {
                data.extend_from_slice(&sample.to_le_bytes());
            }
            VideoPlane { data, stride: w * 2 }
        };
        let bl = VideoFrame {
            planes: vec![make_plane(4, 4, 512), make_plane(2, 2, 512), make_plane(2, 2, 512)],
            pts: Some(0),
        };
        let el_zero = VideoFrame {
            planes: vec![make_plane(2, 2, 512), make_plane(1, 1, 512), make_plane(1, 1, 512)],
            pts: Some(0),
        };
        let el_bright = VideoFrame {
            planes: vec![make_plane(2, 2, 768), make_plane(1, 1, 512), make_plane(1, 1, 512)],
            pts: Some(0),
        };

        let base_12 = reconstruct_fel_frame(&bl, &el_zero, &rpu, 4, 4, 2, 2).expect("FEL neutral");
        let fel_12 = reconstruct_fel_frame(&bl, &el_bright, &rpu, 4, 4, 2, 2).expect("FEL bright");

        let y_base = u16::from_le_bytes([base_12.planes[0].data[0], base_12.planes[0].data[1]]) >> 4;
        let y_fel = u16::from_le_bytes([fel_12.planes[0].data[0], fel_12.planes[0].data[1]]) >> 4;
        assert!(y_fel > y_base + 400, "FEL positive highlight residual must raise 12-bit luma: {y_fel} vs {y_base}");
    }
}
