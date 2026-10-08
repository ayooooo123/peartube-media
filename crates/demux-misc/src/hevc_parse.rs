// Ported from FFmpeg (commit 2da55bf): libavcodec/hevc/parser.c (parse_nal_units,
// hevc_parse_slice_header), hevc/ps.c (ff_hevc_decode_nal_vps, parse_ptl,
// decode_profile_tier_level, decode_hrd, decode_sublayer_hrd, ff_hevc_parse_sps,
// ff_hevc_decode_nal_sps, map_pixel_format, read_window, scaling_list_data,
// ff_hevc_decode_short_term_rps, decode_vui, the start of ff_hevc_decode_nal_pps),
// hevc/sei.c (ff_hevc_decode_nal_sei, decode_nal_sei_message,
// decode_nal_sei_pic_timing, decode_nal_sei_active_parameter_sets),
// h2645_vui.c (ff_h2645_decode_common_vui_params), h2645_parse.c
// (ff_h2645_packet_split, ff_h2645_extract_rbsp, get_bit_length,
// hevc_parse_nal_header), golomb.h and libavformat/demux.c
// (compute_frame_duration).
// License: LGPL-2.1-or-later

//! What FFmpeg's hevc parser makes of an access unit for its demuxer
//! layer: the key flag, the frame rate it sets on the codec context from
//! the VPS or VUI timing, and repeat_pict from picture timing SEIs. The
//! parameter sets are parsed as far as their acceptance and those values
//! need: VPS and SPS in full up to the timing (the SPS to its end), a PPS
//! up to its SPS id and dependent-slice flag; FFmpeg's later checks of a
//! PPS body, the VPS extension and SEI payloads other than picture timing
//! and active parameter sets are not modelled.

use crate::parser::VideoCut;

const MAX_SUB_LAYERS: u32 = 7;
const MAX_DPB_SIZE: u64 = 16;
const MAX_REFS: u64 = 16;
const MAX_SHORT_TERM_REF_PIC_SETS: u64 = 64;
const MAX_LONG_TERM_REF_PICS: u64 = 32;
const MAX_LOG2_CTB_SIZE: u64 = 6;
const MAX_PALETTE_PREDICTOR_SIZE: i64 = 128;
/// HEVC_SEI_PIC_STRUCT_FRAME_DOUBLING and _TRIPLING.
const PIC_STRUCT_FRAME_DOUBLING: u8 = 7;
const PIC_STRUCT_FRAME_TRIPLING: u8 = 8;
/// AV_PICTURE_STRUCTURE_UNKNOWN, _TOP_FIELD, _BOTTOM_FIELD.
const PICTURE_STRUCTURE_UNKNOWN: u8 = 0;
const PICTURE_STRUCTURE_TOP_FIELD: u8 = 1;
const PICTURE_STRUCTURE_BOTTOM_FIELD: u8 = 2;
/// avpriv_find_start_code's state when nothing was read.
const SUB_WIDTH_C: [u64; 4] = [1, 2, 2, 1];
const SUB_HEIGHT_C: [u64; 4] = [1, 2, 1, 1];

/// FFmpeg's checked GetBitContext over an RBSP: `size` bits, reads past
/// them giving the data's later bits (the stop bit) then zeros, the
/// position held at `size + 8`.
struct Bits<'a> {
    data: &'a [u8],
    size: u64,
    pos: u64,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], size: u64) -> Self {
        Self { data, size, pos: 0 }
    }

    fn left(&self) -> i64 {
        self.size as i64 - self.pos as i64
    }

    fn bit_at(&self, p: u64) -> u64 {
        match self.data.get((p / 8) as usize) {
            Some(b) => u64::from((b >> (7 - p % 8)) & 1),
            None => 0,
        }
    }

    fn show(&self, n: u32) -> u64 {
        (0..u64::from(n)).fold(0, |v, k| (v << 1) | self.bit_at(self.pos + k))
    }

    fn skip(&mut self, n: u64) {
        self.pos = (self.pos + n).min(self.size + 8);
    }

    fn bits(&mut self, n: u32) -> u64 {
        let v = self.show(n);
        self.skip(u64::from(n));
        v
    }

    fn bit(&mut self) -> bool {
        self.bits(1) == 1
    }

    /// get_ue_golomb_long: at most 31 leading zeros, the value wrapping
    /// as FFmpeg's unsigned arithmetic does.
    fn ue(&mut self) -> u64 {
        let buf = self.show(32);
        let log = if buf == 0 { 31 } else { 31 - (63 - buf.leading_zeros()) };
        self.skip(u64::from(log));
        (self.bits(log + 1) as u32).wrapping_sub(1) as u64
    }

    /// get_se_golomb_long.
    fn se(&mut self) -> i64 {
        let buf = self.ue() as u32 as i64 + 1;
        if buf & 1 != 0 { -(buf >> 1) } else { buf >> 1 }
    }
}

/// ff_h2645_packet_split for Annex B HEVC: each NAL unit's RBSP (with
/// its two-byte header) and size in bits, its type and layer; units whose
/// header is invalid or whose layer is 63 left out.
fn split_nals(buf: &[u8]) -> Vec<(Vec<u8>, u64, u8, u8)> {
    let mut nals = Vec::new();
    let mut pos = 0usize;
    while buf.len() - pos >= 4 {
        // find_next_start_code
        let mut i = pos;
        while i + 3 < buf.len() && !(buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1) {
            i += 1;
        }
        pos = if i + 3 < buf.len() { i + 3 } else { buf.len() };
        if pos == buf.len() {
            break;
        }
        let (data, consumed) = extract_rbsp(&buf[pos..]);
        pos += consumed;
        // See FFmpeg commit 3566042a0.
        let skip_trailing_zeros = !(buf.len() - pos >= 4 && buf[pos..pos + 4] == [0, 0, 1, 0xE0]);
        let size_bits = bit_length(&data, 2, skip_trailing_zeros);
        if data.is_empty() || size_bits <= 0 {
            continue;
        }
        let mut gb = Bits::new(&data, size_bits as u64);
        // hevc_parse_nal_header
        let forbidden = gb.bit();
        let ty = gb.bits(6) as u8;
        let layer = gb.bits(6) as u8;
        let temporal_id = gb.bits(3) as i64 - 1;
        if layer == 63 || forbidden || temporal_id < 0 {
            continue;
        }
        nals.push((data, size_bits as u64, ty, layer));
    }
    nals
}

/// ff_h2645_extract_rbsp: the RBSP up to the next start code, emulation
/// prevention bytes removed; how many input bytes it took.
fn extract_rbsp(src: &[u8]) -> (Vec<u8>, usize) {
    let length = src.len();
    // The NAL ends at the first 00 00 01 or 00 00 02 (the loop below only
    // skips 00 00 03, which cannot hide one); its RBSP is no longer. Only
    // that is reserved: an access unit of many NALs keeps buffers in
    // proportion to its size.
    let end = (0..length.saturating_sub(2)).find(|&i| src[i] == 0 && src[i + 1] == 0 && matches!(src[i + 2], 1 | 2)).unwrap_or(length);
    let mut dst = Vec::with_capacity(end);
    let mut si = 0;
    while si + 2 < length {
        if src[si] == 0 && src[si + 1] == 0 && src[si + 2] <= 3 {
            match src[si + 2] {
                3 => {
                    dst.extend_from_slice(&[0, 0]);
                    si += 3;
                    continue;
                }
                // The next start code.
                1 | 2 => return (dst, si),
                _ => {}
            }
        }
        dst.push(src[si]);
        si += 1;
    }
    dst.extend_from_slice(&src[si..]);
    (dst, length)
}

/// get_bit_length: the bits before the stop bit, trailing zero bytes
/// dropped; 0 or less for none.
fn bit_length(data: &[u8], min_size: usize, skip_trailing_zeros: bool) -> i64 {
    let mut size = data.len();
    while skip_trailing_zeros && size > 0 && data[size - 1] == 0 {
        size -= 1;
    }
    if size == 0 {
        return 0;
    }
    let mut trailing = 0;
    if size <= min_size {
        if data.len() < min_size {
            return -1;
        }
        size = min_size;
    } else {
        let v = data[size - 1];
        // The stop bit and the zeros after it, or nothing when damaged.
        if v != 0 {
            trailing = i64::from(v.trailing_zeros()) + 1;
        }
    }
    size as i64 * 8 - trailing
}

#[derive(Clone)]
struct Vps {
    data: Vec<u8>,
    max_sub_layers: u32,
    /// (num_units_in_tick, time_scale) where vps_timing_info_present_flag.
    timing: Option<(u32, u32)>,
}

#[derive(Clone)]
struct Sps {
    data: Vec<u8>,
    /// The timing of the VPS it referenced when parsed.
    vps_timing: Option<(u32, u32)>,
    vui_timing: Option<(u32, u32)>,
    frame_field_info_present: bool,
    log2_ctb_size: u32,
    width: u64,
    height: u64,
}

#[derive(Clone)]
struct Pps {
    sps_id: usize,
    /// The SPS it was parsed with.
    sps: Sps,
    dependent_slice_segments_enabled: bool,
}

/// The parser's parameter sets, SEI state and what it set on the codec
/// context, across the units of a stream.
pub(crate) struct HevcParse {
    vps: Vec<Option<Vps>>,
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    /// HEVCSEI: the last picture timing's picture_struct, the active SPS.
    picture_struct: u8,
    active_sps: usize,
    /// AVCodecParserContext.key_frame and repeat_pict.
    key_frame: bool,
    repeat_pict: i64,
    /// AVCodecContext.framerate (num, den).
    framerate: (i64, i64),
    /// The stream time base, (num, den).
    pkt_timebase: (i64, i64),
}

impl HevcParse {
    pub fn new(pkt_timebase: (i64, i64)) -> Self {
        Self {
            vps: vec![None; 16],
            sps: vec![None; 16],
            pps: vec![None; 64],
            picture_struct: PICTURE_STRUCTURE_UNKNOWN,
            active_sps: 0,
            key_frame: false,
            repeat_pict: 0,
            framerate: (0, 1),
            pkt_timebase,
        }
    }

    /// parse_nal_units on one access unit.
    pub fn parse_nal_units(&mut self, buf: &[u8]) {
        self.key_frame = false;
        for (data, size, ty, layer) in split_nals(buf) {
            if layer > 0 {
                continue;
            }
            let mut gb = Bits::new(&data, size);
            gb.skip(16);
            match ty {
                32 => self.decode_vps(&mut gb, &data),
                33 => self.decode_sps(&mut gb, &data),
                34 => self.decode_pps(&mut gb),
                39 | 40 => self.decode_sei(&gb, ty),
                0..=9 | 16..=21 => {
                    if self.picture_struct == PIC_STRUCT_FRAME_DOUBLING {
                        self.repeat_pict = 1;
                    } else if self.picture_struct == PIC_STRUCT_FRAME_TRIPLING {
                        self.repeat_pict = 2;
                    }
                    // Anything but a dependent slice segment ends the unit.
                    if !self.slice_header(&mut gb, ty) {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    /// hevc_parse_slice_header as far as the parser's outputs go: whether
    /// parse_nal_units goes on to the next NAL unit (a dependent slice
    /// segment).
    fn slice_header(&mut self, gb: &mut Bits, ty: u8) -> bool {
        let first_slice_in_pic = gb.bit();
        if (16..=23).contains(&ty) {
            self.key_frame = true;
            gb.skip(1); // no_output_of_prior_pics_flag
        }
        let pps_id = gb.ue();
        let Some(pps) = usize::try_from(pps_id).ok().and_then(|id| self.pps.get(id)).and_then(Option::as_ref) else {
            return false;
        };
        let sps = &pps.sps;
        let (num, den) = match (sps.vps_timing, sps.vui_timing) {
            (Some(t), _) | (None, Some(t)) => t,
            (None, None) => (0, 0),
        };
        if num > 0 && den > 0 {
            // av_reduce(&framerate.den, &framerate.num, num, den, 1 << 30)
            let (d, n) = av_reduce(i64::from(num), i64::from(den), 1 << 30);
            self.framerate = (n, d);
        }
        if first_slice_in_pic {
            return false;
        }
        let dependent = pps.dependent_slice_segments_enabled && gb.bit();
        let ctb = 1u64 << sps.log2_ctb_size;
        let ctbs = ((sps.width + ctb - 1) >> sps.log2_ctb_size) * ((sps.height + ctb - 1) >> sps.log2_ctb_size);
        // av_ceil_log2_c, then get_bitsz.
        let len = if ctbs <= 1 { 0 } else { 64 - (ctbs - 1).leading_zeros() };
        let addr = gb.bits(len);
        if addr >= ctbs {
            return false;
        }
        dependent
    }

    /// ff_hevc_decode_nal_vps.
    fn decode_vps(&mut self, gb: &mut Bits, data: &[u8]) {
        let id = gb.bits(4) as usize;
        if self.vps[id].as_ref().is_some_and(|v| v.data == data) {
            return;
        }
        let Some(vps) = parse_vps(gb, data) else { return };
        if gb.left() < 0 && self.vps[id].is_some() {
            return;
        }
        self.vps[id] = Some(vps);
    }

    /// ff_hevc_decode_nal_sps: a new SPS drops the PPSs parsed with the
    /// one it replaces.
    fn decode_sps(&mut self, gb: &mut Bits, data: &[u8]) {
        let Some((id, sps)) = parse_sps(gb, data, &self.vps) else { return };
        if self.sps[id].as_ref().is_some_and(|old| old.data == sps.data) {
            return;
        }
        for pps in &mut self.pps {
            if pps.as_ref().is_some_and(|p| p.sps_id == id) {
                *pps = None;
            }
        }
        self.sps[id] = Some(sps);
    }

    /// The start of ff_hevc_decode_nal_pps: its id, its SPS, which must
    /// exist, and dependent_slice_segments_enabled_flag.
    fn decode_pps(&mut self, gb: &mut Bits) {
        let pps_id = gb.ue();
        if pps_id >= 64 {
            return;
        }
        let sps_id = gb.ue();
        if sps_id >= 16 {
            return;
        }
        let Some(sps) = self.sps[sps_id as usize].clone() else { return };
        let dependent_slice_segments_enabled = gb.bit();
        self.pps[pps_id as usize] = Some(Pps { sps_id: sps_id as usize, sps, dependent_slice_segments_enabled });
    }

    /// ff_hevc_decode_nal_sei: its messages until one fails.
    fn decode_sei(&mut self, gb: &Bits, nal_type: u8) {
        let bytes = &gb.data[2.min(gb.data.len())..];
        let len = ((gb.left().max(0)) / 8) as usize;
        let bytes = &bytes[..len.min(bytes.len())];
        let mut p = 0usize;
        loop {
            // decode_nal_sei_message
            let left = |p: usize| bytes.len() - p;
            let (mut payload_type, mut payload_size) = (0u64, 0u64);
            let mut byte = 0xFF;
            while byte == 0xFF {
                if left(p) < 2 || payload_type > i32::MAX as u64 - 255 {
                    return;
                }
                byte = bytes[p];
                p += 1;
                payload_type += u64::from(byte);
            }
            byte = 0xFF;
            while byte == 0xFF {
                if (left(p) as u64) < 1 + payload_size {
                    return;
                }
                byte = bytes[p];
                p += 1;
                payload_size += u64::from(byte);
            }
            if (left(p) as u64) < payload_size {
                return;
            }
            let payload = &bytes[p..p + payload_size as usize];
            p += payload_size as usize;
            if nal_type == 39 {
                let mut m = Bits::new(payload, payload.len() as u64 * 8);
                match payload_type {
                    // SEI_TYPE_PIC_TIMING
                    1 => {
                        let Some(sps) = self.sps[self.active_sps].as_ref() else { return };
                        if sps.frame_field_info_present {
                            let pic_struct = m.bits(4);
                            self.picture_struct = match pic_struct {
                                2 | 10 | 12 => PICTURE_STRUCTURE_BOTTOM_FIELD,
                                1 | 9 | 11 => PICTURE_STRUCTURE_TOP_FIELD,
                                7 => PIC_STRUCT_FRAME_DOUBLING,
                                8 => PIC_STRUCT_FRAME_TRIPLING,
                                _ => PICTURE_STRUCTURE_UNKNOWN,
                            };
                        }
                    }
                    // SEI_TYPE_ACTIVE_PARAMETER_SETS
                    129 => {
                        m.skip(4 + 1 + 1);
                        let num_sps_ids_minus1 = m.ue() as u32 as i32;
                        if !(0..=15).contains(&num_sps_ids_minus1) {
                            return;
                        }
                        let id = m.ue();
                        if id >= 16 {
                            return;
                        }
                        self.active_sps = id as usize;
                    }
                    _ => {}
                }
            }
            if p >= bytes.len() {
                return;
            }
        }
    }

    /// What the parser set for the unit just parsed: key_frame, and the
    /// duration compute_frame_duration (demux.c) takes from the codec
    /// context's frame rate (a frame per tick: HEVC has no
    /// AV_CODEC_PROP_FIELDS) times 1 + repeat_pict, in the stream time
    /// base; whether there is a frame rate.
    pub fn video(&self) -> VideoCut {
        let (num, den) = self.framerate;
        let (tb_num, tb_den) = self.pkt_timebase;
        let frame = if tb_num * 1000 > tb_den {
            (tb_num, tb_den)
        } else if den * 1000 > num {
            let (mut n, mut d) = av_reduce(den, num, i64::from(i32::MAX));
            if self.repeat_pict != 0 {
                (n, d) = av_reduce(n * (1 + self.repeat_pict), d, i64::from(i32::MAX));
            }
            (n, d)
        } else {
            (0, 0)
        };
        let duration = match (frame.0 * tb_den, frame.1 * tb_num) {
            (b, c) if frame.0 != 0 && frame.1 != 0 && b >= 0 && c > 0 => b / c,
            _ => 0,
        };
        VideoCut { key: self.key_frame, b_picture: false, duration, rate_known: num != 0 }
    }
}

/// av_reduce: num/den in lowest terms, both at most `max` (approximated
/// by continued fractions where they would not fit).
pub(crate) fn av_reduce(num: i64, den: i64, max: i64) -> (i64, i64) {
    let gcd = |mut a: i64, mut b: i64| {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a.abs()
    };
    let sign = (num < 0) != (den < 0);
    let (mut nom, mut den) = (num.unsigned_abs() as i64, den.unsigned_abs() as i64);
    let g = gcd(nom, den);
    if g != 0 {
        nom /= g;
        den /= g;
    }
    if nom <= max && den <= max {
        let n = if sign { -nom } else { nom };
        return (n, den);
    }
    let (mut a0n, mut a0d, mut a1n, mut a1d) = (0i64, 1i64, 1i64, 0i64);
    while den != 0 {
        let x = nom / den;
        let next_den = nom - den * x;
        let a2n = x * a1n + a0n;
        let a2d = x * a1d + a0d;
        if a2n > max || a2d > max {
            if a1n != 0 {
                let x = (max - a0n) / a1n;
                let x2 = if a1d != 0 { (max - a0d) / a1d } else { x };
                let x = x.min(x2);
                if den * (2 * x * a1d + a0d) > nom * a1d {
                    a1n = x * a1n + a0n;
                    a1d = x * a1d + a0d;
                }
            }
            break;
        }
        (a0n, a0d, a1n, a1d) = (a1n, a1d, a2n, a2d);
        nom = den;
        den = next_den;
    }
    let n = if sign { -a1n } else { a1n };
    (n, a1d)
}

/// ff_hevc_decode_nal_vps after the id: the VPS, None where FFmpeg drops
/// it.
fn parse_vps(gb: &mut Bits, data: &[u8]) -> Option<Vps> {
    let base_layer_internal = gb.bit();
    let base_layer_available = gb.bit();
    if !base_layer_internal || !base_layer_available {
        return None;
    }
    let max_layers = gb.bits(6) + 1;
    let max_sub_layers = gb.bits(3) as u32 + 1;
    gb.skip(1); // vps_temporal_id_nesting_flag
    if gb.bits(16) != 0xFFFF {
        return None;
    }
    if max_sub_layers > MAX_SUB_LAYERS {
        return None;
    }
    parse_ptl(gb, max_sub_layers)?;
    let ordering_info = gb.bit();
    let start = if ordering_info { 0 } else { max_sub_layers - 1 };
    for _ in start..max_sub_layers {
        let max_dec_pic_buffering = (gb.ue() as u32).wrapping_add(1);
        let _num_reorder = gb.ue();
        gb.ue(); // vps_max_latency_increase
        if u64::from(max_dec_pic_buffering) > MAX_DPB_SIZE || max_dec_pic_buffering == 0 {
            return None;
        }
    }
    let max_layer_id = gb.bits(6) as i64;
    let num_layer_sets = (gb.ue() as u32).wrapping_add(1) as i64;
    if !(1..=1024).contains(&num_layer_sets) || (num_layer_sets - 1) * (max_layer_id + 1) > gb.left() {
        return None;
    }
    if num_layer_sets > 1 {
        gb.skip((max_layer_id + 1) as u64); // layer_id_included_flag
    }
    if num_layer_sets > 2 {
        gb.skip(((num_layer_sets - 2) * (max_layer_id + 1)) as u64);
    }
    let mut timing = None;
    if gb.bit() {
        let num_units_in_tick = gb.bits(32) as u32;
        let time_scale = gb.bits(32) as u32;
        timing = Some((num_units_in_tick, time_scale));
        if gb.bit() {
            gb.ue(); // vps_num_ticks_poc_diff_one_minus1
        }
        let num_hrd_parameters = gb.ue() as u32;
        if i64::from(num_hrd_parameters) > num_layer_sets {
            return None;
        }
        for i in 0..num_hrd_parameters {
            gb.ue(); // hrd_layer_set_idx
            let common_inf_present = i == 0 || gb.bit();
            decode_hrd(gb, common_inf_present, max_sub_layers);
        }
    }
    // vps_extension_flag: the extension is not parsed (FFmpeg keeps the
    // VPS where its extension is unsupported).
    if max_layers > 1 {
        gb.bit();
    }
    Some(Vps { data: data.to_vec(), max_sub_layers, timing })
}

/// decode_profile_tier_level; None where FFmpeg fails it.
fn decode_profile_tier_level(gb: &mut Bits) -> Option<()> {
    if gb.left() < 2 + 1 + 5 + 32 + 4 + 43 + 1 {
        return None;
    }
    gb.skip(2 + 1); // profile_space, tier_flag
    let mut profile_idc = gb.bits(5);
    let mut compat = [false; 32];
    for (i, c) in compat.iter_mut().enumerate() {
        *c = gb.bit();
        if profile_idc == 0 && i > 0 && *c {
            profile_idc = i as u64;
        }
    }
    gb.skip(4); // progressive, interlaced, non_packed, frame_only
    let check = |idc: u64| profile_idc == idc || compat[idc as usize];
    if (4..=10).any(check) {
        gb.skip(9);
        if check(5) || check(9) || check(10) {
            gb.skip(1 + 33);
        } else {
            gb.skip(34);
        }
    } else if check(2) {
        gb.skip(7 + 1 + 35);
    } else {
        gb.skip(43);
    }
    gb.skip(1); // inbld_flag or reserved
    Some(())
}

/// parse_ptl with profile_present.
fn parse_ptl(gb: &mut Bits, max_num_sub_layers: u32) -> Option<()> {
    decode_profile_tier_level(gb)?;
    let sub = max_num_sub_layers.saturating_sub(1);
    if gb.left() < 8 + if sub > 0 { 16 } else { 0 } {
        return None;
    }
    gb.skip(8); // general_level_idc
    let mut flags = Vec::with_capacity(sub as usize);
    for _ in 0..sub {
        flags.push((gb.bit(), gb.bit()));
    }
    if sub > 0 {
        for _ in sub..8 {
            gb.skip(2); // reserved_zero_2bits
        }
    }
    for (profile_present, level_present) in flags {
        if profile_present {
            decode_profile_tier_level(gb)?;
        }
        if level_present {
            if gb.left() < 8 {
                return None;
            }
            gb.skip(8);
        }
    }
    Some(())
}

/// decode_sublayer_hrd.
fn decode_sublayer_hrd(gb: &mut Bits, nb_cpb: u64, subpic_params_present: bool) {
    for _ in 0..nb_cpb {
        gb.ue(); // bit_rate_value_minus1
        gb.ue(); // cpb_size_value_minus1
        if subpic_params_present {
            gb.ue();
            gb.ue();
        }
        gb.skip(1); // cbr_flag
    }
}

/// decode_hrd on a zeroed HEVCHdrParams; it stops where it fails.
fn decode_hrd(gb: &mut Bits, common_inf_present: bool, max_sublayers: u32) {
    let (mut nal, mut vcl, mut sub_pic) = (false, false, false);
    if common_inf_present {
        nal = gb.bit();
        vcl = gb.bit();
        if nal || vcl {
            sub_pic = gb.bit();
            if sub_pic {
                gb.skip(8 + 5 + 1 + 5);
            }
            gb.skip(4 + 4); // bit_rate_scale, cpb_size_scale
            if sub_pic {
                gb.skip(4);
            }
            gb.skip(5 + 5 + 5);
        }
    }
    for _ in 0..max_sublayers {
        let fixed_pic_rate_general = gb.bit();
        let fixed_pic_rate_within_cvs = !fixed_pic_rate_general && gb.bit();
        let mut low_delay = false;
        if fixed_pic_rate_within_cvs || fixed_pic_rate_general {
            gb.ue(); // elemental_duration_in_tc_minus1
        } else {
            low_delay = gb.bit();
        }
        let mut cpb_cnt_minus1 = 0;
        if !low_delay {
            cpb_cnt_minus1 = gb.ue();
            if cpb_cnt_minus1 > 31 {
                return;
            }
        }
        if nal {
            decode_sublayer_hrd(gb, cpb_cnt_minus1 + 1, sub_pic);
        }
        if vcl {
            decode_sublayer_hrd(gb, cpb_cnt_minus1 + 1, sub_pic);
        }
    }
}

/// read_window: the window's offsets, None where FFmpeg rejects it.
fn read_window(gb: &mut Bits, chroma_format_idc: usize, w: u64, h: u64) -> Option<(u64, u64, u64, u64)> {
    let (horiz, vert) = (SUB_WIDTH_C[chroma_format_idc], SUB_HEIGHT_C[chroma_format_idc]);
    let left = gb.ue() * horiz;
    let right = gb.ue() * horiz;
    let top = gb.ue() * vert;
    let bottom = gb.ue() * vert;
    if w <= left + right || h <= top + bottom {
        return None;
    }
    Some((left, right, top, bottom))
}

/// av_image_check_size(w, h): FFmpeg's limits on a picture size.
fn image_size_ok(w: u64, h: u64) -> bool {
    if w == 0 || h == 0 || w > i32::MAX as u64 || h > i32::MAX as u64 {
        return false;
    }
    let stride = 8 * w + 128 * 8;
    stride < i32::MAX as u64 && stride * (h + 128) < i32::MAX as u64
}

/// scaling_list_data; None where FFmpeg fails it.
fn scaling_list_data(gb: &mut Bits) -> Option<()> {
    for size_id in 0..4u64 {
        let step = if size_id == 3 { 3 } else { 1 };
        let mut matrix_id = 0u64;
        while matrix_id < 6 {
            if !gb.bit() {
                // scaling_list_pred_matrix_id_delta
                let delta = gb.ue() as u32 as u64 * step;
                if delta != 0 && matrix_id < delta {
                    return None;
                }
            } else {
                let coef_num = 64.min(1u64 << (4 + (size_id << 1)));
                if size_id > 1 {
                    let dc = gb.se();
                    if !(-7..=247).contains(&dc) {
                        return None;
                    }
                }
                for _ in 0..coef_num {
                    gb.se();
                }
            }
            matrix_id += step;
        }
    }
    Some(())
}

/// ff_hevc_decode_short_term_rps in an SPS: the RPS's num_delta_pocs,
/// None where FFmpeg fails it.
fn short_term_rps(gb: &mut Bits, index: usize, previous: &[u32]) -> Option<u32> {
    let predict = index != 0 && gb.bit();
    if predict {
        let ref_num_delta_pocs = previous[index - 1];
        gb.skip(1); // delta_rps_sign
        let abs_delta_rps = (gb.ue() as u32).wrapping_add(1);
        if abs_delta_rps > 32768 {
            return None;
        }
        let mut k = 0u32;
        for _ in 0..=ref_num_delta_pocs {
            let used = gb.bit();
            let use_delta = !used && gb.bit();
            if used || use_delta {
                k += 1;
            }
        }
        if k >= 32 {
            return None;
        }
        Some(k)
    } else {
        // num_negative_pics is a uint8_t in FFmpeg, nb_positive_pics not.
        let num_negative = u64::from(gb.ue() as u8);
        let num_positive = gb.ue() as u32 as u64;
        if num_negative >= MAX_REFS || num_positive >= MAX_REFS {
            return None;
        }
        for _ in 0..num_negative + num_positive {
            let delta_poc = (gb.ue() as u32).wrapping_add(1);
            if !(1..=32768).contains(&delta_poc) {
                return None;
            }
            gb.skip(1); // used_by_curr_pic
        }
        Some(u32::from((num_negative + num_positive) as u8))
    }
}

/// ff_h2645_decode_common_vui_params, the bits it reads.
fn common_vui(gb: &mut Bits) {
    if gb.bit() {
        // aspect_ratio_info_present_flag
        if gb.bits(8) == 255 {
            gb.skip(32); // EXTENDED_SAR
        }
    }
    if gb.bit() {
        gb.skip(1); // overscan_appropriate_flag
    }
    if gb.bit() {
        // video_signal_type_present_flag
        gb.skip(3 + 1);
        if gb.bit() {
            gb.skip(24); // colour description
        }
    }
    if gb.bit() {
        // chroma_loc_info_present_flag: get_ue_golomb_31 each
        ue_31(gb);
        ue_31(gb);
    }
}

/// get_ue_golomb_31: ff_golomb_vlc_len and ff_ue_golomb_vlc_code over
/// the next 9 bits (codes longer than 9 bits read as the table has them).
fn ue_31(gb: &mut Bits) -> u64 {
    const LONG_LEN: [u64; 16] = [19, 17, 15, 15, 13, 13, 13, 13, 11, 11, 11, 11, 11, 11, 11, 11];
    let buf = gb.show(9);
    if buf < 16 {
        gb.skip(LONG_LEN[buf as usize]);
        return if buf == 8 { 31 } else { 32 };
    }
    let lz = 8 - (63 - u64::from(buf.leading_zeros()));
    let len = 2 * lz + 1;
    gb.skip(len);
    (buf >> (9 - len)) - 1
}

/// decode_vui: (timing, frame_field_info_present_flag), with FFmpeg's
/// fallback that reparses the timing where the VUI overran the SPS.
fn decode_vui(gb: &mut Bits, chroma_format_idc: usize, width: u64, height: u64, max_sub_layers: u32) -> (Option<(u32, u32)>, bool) {
    common_vui(gb);
    gb.skip(1); // neutral_chroma_indication_flag
    gb.skip(1); // field_seq_flag
    let frame_field_info_present = gb.bit();
    let backup = gb.pos;
    let default_display_window = if gb.left() >= 68 && gb.show(21) == 0x10_0000 { false } else { gb.bit() };
    if default_display_window {
        let _ = read_window(gb, chroma_format_idc, width, height);
    }
    let mut alt = false;
    let mut timing;
    loop {
        timing = None;
        let mut restart = false;
        if gb.bit() {
            if gb.left() < 66 && !alt {
                restart = true;
            } else {
                let num_units_in_tick = gb.bits(32) as u32;
                let time_scale = gb.bits(32) as u32;
                timing = Some((num_units_in_tick, time_scale));
                if gb.bit() {
                    gb.ue(); // vui_num_ticks_poc_diff_one_minus1
                }
                if gb.bit() {
                    decode_hrd(gb, true, max_sub_layers);
                }
            }
        }
        if !restart && gb.bit() {
            // bitstream_restriction_flag
            if gb.left() < 8 && !alt {
                restart = true;
            } else {
                gb.skip(3);
                for _ in 0..5 {
                    gb.ue();
                }
            }
        }
        if !restart && gb.left() < 1 && !alt {
            restart = true;
        }
        if !restart {
            break;
        }
        // The fallback: from before default_display_window_flag, taken as
        // absent.
        gb.pos = backup;
        alt = true;
    }
    (timing, frame_field_info_present)
}

/// ff_hevc_parse_sps for the base layer: the SPS id and what the parser
/// needs, None where FFmpeg rejects the SPS.
fn parse_sps(gb: &mut Bits, data: &[u8], vps_list: &[Option<Vps>]) -> Option<(usize, Sps)> {
    let vps_id = gb.bits(4) as usize;
    let vps = vps_list[vps_id].as_ref()?;
    let max_sub_layers = gb.bits(3) as u32 + 1;
    if max_sub_layers > vps.max_sub_layers {
        return None;
    }
    gb.skip(1); // temporal_id_nesting
    parse_ptl(gb, max_sub_layers)?;
    let sps_id = gb.ue();
    if sps_id >= 16 {
        return None;
    }
    let mut chroma_format_idc = gb.ue();
    if chroma_format_idc > 3 {
        return None;
    }
    if chroma_format_idc == 3 && gb.bit() {
        chroma_format_idc = 0; // separate_colour_plane_flag
    }
    let chroma = chroma_format_idc as usize;
    let width = gb.ue() as u32 as u64;
    let height = gb.ue() as u32 as u64;
    if !image_size_ok(width, height) {
        return None;
    }
    let mut window = (0, 0, 0, 0);
    if gb.bit() {
        window = read_window(gb, chroma, width, height)?;
    }
    let bit_depth = ue_31(gb) + 8;
    if bit_depth > 16 {
        return None;
    }
    let bit_depth_chroma = ue_31(gb) + 8;
    if bit_depth_chroma > 16 {
        return None;
    }
    if chroma_format_idc != 0 && bit_depth_chroma != bit_depth {
        return None;
    }
    // map_pixel_format
    if !matches!(bit_depth, 8 | 9 | 10 | 12) {
        return None;
    }
    let log2_max_poc_lsb = gb.ue() + 4;
    if log2_max_poc_lsb > 16 {
        return None;
    }
    let ordering_info = gb.bit();
    let start = if ordering_info { 0 } else { max_sub_layers - 1 };
    for _ in start..max_sub_layers {
        let max_dec_pic_buffering = (gb.ue() as u32).wrapping_add(1) as u64;
        let num_reorder = gb.ue() as u32 as u64;
        gb.ue(); // max_latency_increase
        if max_dec_pic_buffering > MAX_DPB_SIZE {
            return None;
        }
        if num_reorder > max_dec_pic_buffering.wrapping_sub(1) && num_reorder > MAX_DPB_SIZE - 1 {
            return None;
        }
    }
    let log2_min_cb_size = gb.ue() as u32 as u64 + 3;
    let log2_diff_max_min_cb = gb.ue() as u32 as u64;
    let log2_min_tb_size = gb.ue() as u32 as u64 + 2;
    let log2_diff_max_min_tb = gb.ue() as u32 as u64;
    let log2_max_trafo_size = log2_diff_max_min_tb + log2_min_tb_size;
    if !(3..=30).contains(&log2_min_cb_size) || log2_diff_max_min_cb > 30 {
        return None;
    }
    if log2_min_tb_size >= log2_min_cb_size || log2_min_tb_size < 2 || log2_diff_max_min_tb > 30 {
        return None;
    }
    let depth_inter = gb.ue() as u32 as u64;
    let depth_intra = gb.ue() as u32 as u64;
    if gb.bit() {
        // scaling_list_enabled: sps_scaling_list_data_present_flag
        if gb.bit() {
            scaling_list_data(gb)?;
        }
    }
    gb.skip(2); // amp, sao
    if gb.bit() {
        // pcm_enabled
        let pcm_depth = gb.bits(4) + 1;
        let pcm_depth_chroma = gb.bits(4) + 1;
        gb.ue();
        gb.ue();
        if pcm_depth.max(pcm_depth_chroma) > bit_depth {
            return None;
        }
        gb.skip(1); // pcm_loop_filter_disabled
    }
    let nb_st_rps = gb.ue();
    if nb_st_rps > MAX_SHORT_TERM_REF_PIC_SETS {
        return None;
    }
    let mut rps = Vec::with_capacity(nb_st_rps as usize);
    for i in 0..nb_st_rps as usize {
        let n = short_term_rps(gb, i, &rps)?;
        rps.push(n);
    }
    if gb.bit() {
        // long_term_ref_pics_present
        let n = u64::from(gb.ue() as u8); // a uint8_t in FFmpeg
        if n > MAX_LONG_TERM_REF_PICS {
            return None;
        }
        for _ in 0..n {
            gb.skip(log2_max_poc_lsb + 1);
        }
    }
    gb.skip(2); // temporal_mvp, strong_intra_smoothing
    let (mut vui_timing, mut frame_field_info_present) = (None, false);
    if gb.bit() {
        (vui_timing, frame_field_info_present) = decode_vui(gb, chroma, width, height, max_sub_layers);
    }
    if gb.bit() {
        // sps_extension_present
        let range = gb.bit();
        let multilayer = gb.bit();
        let three_d = gb.bit();
        let scc = gb.bit();
        gb.skip(4);
        if range {
            gb.skip(9);
        }
        if multilayer {
            gb.skip(1);
        }
        if three_d {
            for i in 0..2 {
                gb.skip(2);
                if i == 0 {
                    gb.ue();
                    gb.skip(4);
                } else {
                    gb.skip(1);
                    gb.ue();
                    gb.skip(5);
                }
            }
        }
        if scc {
            gb.skip(1); // curr_pic_ref_enabled
            if gb.bit() {
                // palette_mode_enabled: get_ue_golomb
                ue_short(gb);
                ue_short(gb);
                if gb.bit() {
                    let n = ue_short(gb) + 1;
                    if n > MAX_PALETTE_PREDICTOR_SIZE {
                        return None;
                    }
                    let comps = if chroma_format_idc == 0 { 1 } else { 3 };
                    for comp in 0..comps {
                        let depth = if comp == 0 { bit_depth } else { bit_depth_chroma };
                        for _ in 0..n.max(0) {
                            gb.skip(depth);
                        }
                    }
                }
            }
            gb.skip(2 + 1);
        }
    }
    // The cropping window is checked only under AV_EF_EXPLODE.
    let _ = window;
    let log2_ctb_size = log2_min_cb_size + log2_diff_max_min_cb;
    if log2_ctb_size > MAX_LOG2_CTB_SIZE || log2_ctb_size < 4 {
        return None;
    }
    if width & ((1 << log2_min_cb_size) - 1) != 0 || height & ((1 << log2_min_cb_size) - 1) != 0 {
        return None;
    }
    if depth_inter > log2_ctb_size - log2_min_tb_size || depth_intra > log2_ctb_size - log2_min_tb_size {
        return None;
    }
    if log2_max_trafo_size > log2_ctb_size.min(5) {
        return None;
    }
    if gb.left() < 0 {
        return None;
    }
    let sps = Sps {
        data: data.to_vec(),
        vps_timing: vps.timing,
        vui_timing,
        frame_field_info_present,
        log2_ctb_size: log2_ctb_size as u32,
        width,
        height,
    };
    Some((sps_id as usize, sps))
}

/// get_ue_golomb: codes of up to 16 bits as their value, longer ones an
/// error (negative).
fn ue_short(gb: &mut Bits) -> i64 {
    let buf = gb.show(32);
    if buf >= 1 << 27 {
        return ue_31(gb) as i64;
    }
    if buf == 0 {
        gb.skip(32);
        return -1;
    }
    let log = 2 * (63 - buf.leading_zeros() as i64) - 31;
    if log < 7 {
        gb.skip((32 - log).max(0) as u64);
        return -1;
    }
    gb.skip((32 - log) as u64);
    (buf >> log) as i64 - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av_reduce_keeps_small_fractions_and_reduces() {
        assert_eq!(av_reduce(1, 25, i64::from(i32::MAX)), (1, 25));
        assert_eq!(av_reduce(1001, 30000, i64::from(i32::MAX)), (1001, 30000));
        assert_eq!(av_reduce(50, 100, i64::from(i32::MAX)), (1, 2));
    }

    #[test]
    fn rbsp_drops_emulation_prevention_and_stops_at_a_start_code() {
        let (data, used) = extract_rbsp(&[0x40, 0x01, 0, 0, 3, 1, 0x80, 0, 0, 1, 0x42]);
        assert_eq!(data, [0x40, 0x01, 0, 0, 1, 0x80]);
        assert_eq!(used, 7);
        assert_eq!(bit_length(&data, 2, true), 6 * 8 - 8);
    }

    /// An access unit of 65536 three-byte NAL units (384 KiB, filler
    /// units never end an HEVC access unit) keeps RBSP buffers in
    /// proportion to its size, not one buffer of the rest of the unit per
    /// NAL (12 GiB).
    #[test]
    fn many_small_nal_units_keep_memory_linear() {
        let au = [0, 0, 1, 0x4c, 1, 0x80].repeat(65536);
        let nals = split_nals(&au);
        assert_eq!(nals.len(), 65536);
        let kept: usize = nals.iter().map(|(rbsp, ..)| rbsp.capacity()).sum();
        assert!(kept <= 4 * au.len(), "{kept} bytes kept for a {}-byte access unit", au.len());
    }
}
