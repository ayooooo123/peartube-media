// Ported from FFmpeg (commit 2da55bf): libavcodec/h264_parser.c
// (parse_nal_units, scan_mmco_reset and the timestamp derivation of
// h264_parse), h264_ps.c (ff_h264_decode_seq_parameter_set with
// decode_vui_parameters, decode_hrd_parameters and the scaling lists;
// ff_h264_decode_picture_parameter_set), h2645_vui.c, h264_sei.c
// (ff_h264_sei_decode, ff_h264_sei_process_picture_timing),
// h2645_sei.c (ff_h2645_sei_message_decode), itut35.c
// (ff_itut_t35_parse_buffer), atsc_a53.c (ff_parse_a53_cc), h264_parse.c
// (ff_h264_parse_ref_count, ff_h264_pred_weight_table, ff_h264_init_poc),
// h2645_parse.c (ff_h2645_extract_rbsp), golomb.h and get_bits.h (the
// checked reader), and from libavutil av_reduce, av_rescale_rnd and
// av_image_check_size2.
// License: LGPL-2.1-or-later
//
// What FFmpeg's h264 parser learns from each access unit it cuts: the
// key flag parse_packet gives the unit's packet, and the timestamps
// h264_parse derives for units the container left untimed from
// buffering period and picture timing SEIs (HRD timing). The parameter
// sets, picture order count and SEI state carry over from unit to unit
// as in the parser context; a new parser (after a seek) starts them
// over, while the frame rate it set on the codec context stays.
//
// Bits read past the end of an access unit are zeros. FFmpeg reads
// whatever follows the unit in memory there (zero padding, or the next
// unit of the same demuxed packet); only damaged headers read that far.
// The payload parsers of registered user data FFmpeg runs for AOM film
// grain, HDR10+, SMPTE ST 2094-50 and HDR Vivid are not ported: such a
// message counts as read, where FFmpeg's failure to parse one would end
// its walk of that SEI.

use std::borrow::Cow;
use std::sync::Arc;

use crate::parser::VideoCut;

/// AVERROR_INVALIDDATA
const INVALIDDATA: i32 = -0x4144_4E49;
/// AVERROR_PS_NOT_FOUND (h264_sei.c)
const PS_NOT_FOUND: i32 = -0x5350_3FF8;
/// AV_NOPTS_VALUE
const NOPTS: i64 = i64::MIN;

/// AV_PICTURE_TYPE_I / _P / _B
const PICT_I: u32 = 1;
const PICT_P: u32 = 2;
const PICT_B: u32 = 3;
/// ff_h264_golomb_to_pict_type: P, B, I, SP, SI
const GOLOMB_TO_PICT_TYPE: [u32; 5] = [2, 3, 1, 6, 5];

/// PICT_TOP_FIELD, PICT_BOTTOM_FIELD, PICT_FRAME
const PICT_TOP_FIELD: u32 = 1;
const PICT_BOTTOM_FIELD: u32 = 2;
const PICT_FRAME: u32 = 3;

/// H264_MAX_MMCO_COUNT
const MAX_MMCO_COUNT: usize = 66;

/// av_log2, 0 for 0.
fn av_log2(x: u32) -> u32 {
    if x == 0 { 0 } else { 31 - x.leading_zeros() }
}

/// GetBitContext with the checked reader: the position never passes eight
/// bits beyond the end; reads take the bits found there, which are those
/// after the data in `buf` (the NAL's raw bytes when it had no escapes),
/// zeros past `buf`.
struct Gb<'a> {
    buf: &'a [u8],
    size: usize,
    index: usize,
}

impl<'a> Gb<'a> {
    fn new(buf: &'a [u8], size_bytes: usize) -> Self {
        Self { buf, size: size_bytes * 8, index: 0 }
    }

    fn bit_at(&self, k: usize) -> u32 {
        self.buf.get(k / 8).map_or(0, |b| u32::from((b >> (7 - k % 8)) & 1))
    }

    /// The `n` (at most 32) bits at the position.
    fn peek(&self, n: u32) -> u32 {
        (0..n as usize).fold(0u32, |v, i| (v << 1) | self.bit_at(self.index + i))
    }

    fn skip(&mut self, n: usize) {
        self.index = self.index.saturating_add(n).min(self.size + 8);
    }

    /// get_bits / get_bits_long for up to 32 bits: one read of the bits
    /// at the position, as a 64-bit build of FFmpeg makes it.
    fn bits(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.skip(n as usize);
        v
    }

    fn bit(&mut self) -> bool {
        self.bits(1) == 1
    }

    fn left(&self) -> i64 {
        self.size as i64 - self.index as i64
    }

    /// get_ue_golomb_long
    fn ue_long(&mut self) -> u32 {
        let buf = self.peek(32);
        let log = 31 - av_log2(buf);
        self.skip(log as usize);
        self.bits(log + 1).wrapping_sub(1)
    }

    /// get_ue_golomb_31 (ff_ue_golomb_vlc_code on nine bits): exact up
    /// to 30, 31 or 32 for longer codes.
    fn ue31(&mut self) -> u32 {
        let w = self.peek(9);
        let zeros = 9 - (32 - w.leading_zeros());
        let len = 2 * zeros + 1;
        self.skip(len as usize);
        if zeros <= 4 {
            (w >> (9 - len)) - 1
        } else if zeros == 5 && w & 7 == 0 {
            31
        } else {
            32
        }
    }

    /// get_ue_golomb: exact up to twelve leading zeros, then
    /// AVERROR_INVALIDDATA.
    fn ue(&mut self) -> i32 {
        let buf = self.peek(32);
        if buf >= 1 << 27 {
            return self.ue31() as i32;
        }
        let log = 2 * av_log2(buf) as i32 - 31;
        self.skip((32 - log) as usize);
        if log < 7 {
            return INVALIDDATA;
        }
        ((buf >> log) - 1) as i32
    }

    /// get_se_golomb
    fn se(&mut self) -> i32 {
        let buf = self.peek(32);
        if buf >= 1 << 27 {
            let k = self.ue31();
            return if k & 1 == 1 { (k as i32 + 1) / 2 } else { -(k as i32 / 2) };
        }
        let log = av_log2(buf);
        self.skip((31 - log) as usize);
        let buf = self.peek(32) >> log;
        self.skip((32 - log) as usize);
        let sign = (buf & 1).wrapping_neg();
        ((buf >> 1) ^ sign).wrapping_sub(sign) as i32
    }

    /// get_se_golomb_long
    fn se_long(&mut self) -> i32 {
        let buf = self.ue_long();
        let sign = (buf & 1).wrapping_sub(1);
        ((buf >> 1) ^ sign).wrapping_add(1) as i32
    }
}

/// avpriv_find_start_code from `from` to one byte past the end (its
/// zero padding): the index of the NAL header after the first 00 00 01,
/// or the end.
fn next_nal(buf: &[u8], from: usize) -> usize {
    let mut k = from;
    while k + 3 <= buf.len() {
        if buf[k] == 0 && buf[k + 1] == 0 && buf[k + 2] == 1 {
            return k + 3;
        }
        k += 1;
    }
    buf.len()
}

/// A NAL as ff_h2645_extract_rbsp leaves it: its bytes (header first),
/// the first `size` of them its own.
struct Nal<'a> {
    data: Cow<'a, [u8]>,
    size: usize,
}

/// ff_h2645_extract_rbsp (small padding) on the `length` bytes from
/// `src`: the NAL up to the next start code with escapes removed, and
/// the raw bytes it took. Without an escape the NAL is `src` itself.
fn extract_rbsp(src: &[u8], length: usize) -> (Nal<'_>, usize) {
    let mut length = length;
    // The first 00 00 03 or 00 00 01 at least three bytes from the end.
    let mut found = None;
    let mut i = 0;
    while i + 2 < length {
        if src[i] == 0 && src[i + 1] == 0 && (src[i + 2] == 3 || src[i + 2] == 1) {
            found = Some(i);
            break;
        }
        i += 1;
    }
    let i = match found {
        Some(i) if src[i + 2] == 1 => {
            length = i;
            i
        }
        Some(i) => i,
        None => length,
    };
    if i + 1 >= length {
        return (Nal { data: Cow::Borrowed(src), size: length }, length);
    }
    let mut dst = src[..i].to_vec();
    let mut si = i;
    while si + 2 < length {
        if src[si + 2] > 3 {
            dst.extend_from_slice(&src[si..si + 2]);
            si += 2;
        } else if src[si] == 0 && src[si + 1] == 0 && src[si + 2] != 0 {
            if src[si + 2] == 3 {
                dst.extend_from_slice(&[0, 0]);
                si += 3;
                continue;
            }
            // the next start code
            let size = dst.len();
            return (Nal { data: Cow::Owned(dst), size }, si);
        }
        dst.push(src[si]);
        si += 1;
    }
    dst.extend_from_slice(&src[si..length]);
    let size = dst.len();
    (Nal { data: Cow::Owned(dst), size }, length)
}

/// av_reduce: `num/den` within `max`, as (numerator, denominator).
fn av_reduce(num: i64, den: i64, max: i64) -> (i32, i32) {
    fn gcd(mut a: i64, mut b: i64) -> i64 {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    }
    let negative = (num < 0) ^ (den < 0);
    let (mut num, mut den) = (num.unsigned_abs() as i64, den.unsigned_abs() as i64);
    let g = gcd(num, den);
    if g != 0 {
        num /= g;
        den /= g;
    }
    let (mut a0, mut a1) = ((0i64, 1i64), (1i64, 0i64));
    if num <= max && den <= max {
        a1 = (num, den);
        den = 0;
    }
    while den != 0 {
        let x = num / den;
        let next_den = num - den * x;
        let a2n = x * a1.0 + a0.0;
        let a2d = x * a1.1 + a0.1;
        if a2n > max || a2d > max {
            let mut x = x;
            if a1.0 != 0 {
                x = (max - a0.0) / a1.0;
            }
            if a1.1 != 0 {
                x = x.min((max - a0.1) / a1.1);
            }
            if i128::from(den) * i128::from(2 * x * a1.1 + a0.1) > i128::from(num) * i128::from(a1.1) {
                a1 = (x * a1.0 + a0.0, x * a1.1 + a0.1);
            }
            break;
        }
        a0 = a1;
        a1 = (a2n, a2d);
        num = den;
        den = next_den;
    }
    let n = a1.0 as i32;
    (if negative { -n } else { n }, a1.1 as i32)
}

/// av_rescale(a, b, c): to nearest, ties away from zero; INT64_MIN when
/// the result does not fit.
fn av_rescale(a: i64, b: i64, c: i64) -> i64 {
    if c <= 0 || b < 0 {
        return NOPTS;
    }
    let magnitude = |a: i64| -> i64 {
        let r = (i128::from(a) * i128::from(b) + i128::from(c / 2)) / i128::from(c);
        i64::try_from(r).unwrap_or(NOPTS)
    };
    if a < 0 {
        (magnitude(-a.max(-i64::MAX)) as u64).wrapping_neg() as i64
    } else {
        magnitude(a)
    }
}

/// av_image_check_size for a picture without a pixel format.
fn image_size_ok(w: u32, h: u32) -> bool {
    let stride = 8 * i64::from(w) + 128 * 8;
    !(w == 0
        || h == 0
        || w > i32::MAX as u32
        || h > i32::MAX as u32
        || stride >= i64::from(i32::MAX)
        || (stride as u64).wrapping_mul(u64::from(h) + 128) >= i32::MAX as u64)
}

/// The SPS fields the parser uses.
struct Sps {
    ref_frame_count: u32,
    bit_depth_luma: u32,
    chroma_format_idc: u32,
    log2_max_frame_num: u32,
    frame_mbs_only: bool,
    poc_type: u32,
    log2_max_poc_lsb: u32,
    delta_pic_order_always_zero: bool,
    offset_for_non_ref_pic: i32,
    offset_for_top_to_bottom_field: i32,
    offset_for_ref_frame: Vec<i32>,
    timing_info_present: bool,
    num_units_in_tick: u32,
    time_scale: u32,
    nal_hrd: bool,
    vcl_hrd: bool,
    cpb_removal_delay_length: u32,
    dpb_output_delay_length: u32,
    pic_struct_present: bool,
}

/// The PPS fields the parser uses, with the SPS it referred to when it
/// was read.
struct Pps {
    sps: Arc<Sps>,
    ref_count: [u32; 2],
    pic_order_present: bool,
    weighted_pred: bool,
    weighted_bipred_idc: u32,
    redundant_pic_cnt_present: bool,
}

/// H264POCContext
#[derive(Default, Clone, Copy)]
struct Poc {
    poc_lsb: i32,
    poc_msb: i32,
    delta_poc_bottom: i32,
    delta_poc: [i32; 2],
    frame_num: i32,
    prev_poc_msb: i32,
    prev_poc_lsb: i32,
    frame_num_offset: i32,
    prev_frame_num_offset: i32,
    prev_frame_num: i32,
}

/// H264SEIContext as far as the parser uses it.
struct Sei {
    recovery_frame_cnt: i32,
    cpb_removal_delay: i32,
    dpb_output_delay: i32,
    picture_timing_present: bool,
    /// H264SEIPictureTiming.payload: bytes past a shorter payload keep
    /// those of an earlier one.
    payload: [u8; 40],
    payload_size: usize,
    buffering_period_present: bool,
    pic_struct: u32,
    x264_build: i32,
}

impl Default for Sei {
    fn default() -> Self {
        Self {
            recovery_frame_cnt: -1,
            cpb_removal_delay: -1,
            dpb_output_delay: 0,
            picture_timing_present: false,
            payload: [0; 40],
            payload_size: 0,
            buffering_period_present: false,
            pic_struct: 0,
            x264_build: -1,
        }
    }
}

/// H264ParseContext and the codec context fields the h264 parser reads
/// and writes.
pub(crate) struct H264Parse {
    sps: Vec<Option<Arc<Sps>>>,
    pps: Vec<Option<Arc<Pps>>>,
    poc: Poc,
    sei: Sei,
    reference_dts: i64,
    /// AVCodecContext.framerate (num, den)
    framerate: (i32, i32),
    /// AVCodecContext.pkt_timebase (num, den)
    pkt_timebase: (i64, i64),
    key_frame: bool,
    /// AVCodecParserContext.pict_type and repeat_pict
    pict_type: u32,
    repeat_pict: i32,
}

impl H264Parse {
    pub fn new(pkt_timebase: (i64, i64)) -> Self {
        Self {
            sps: vec![None; 32],
            pps: vec![None; 256],
            poc: Poc::default(),
            sei: Sei::default(),
            reference_dts: NOPTS,
            framerate: (0, 1),
            pkt_timebase,
            key_frame: false,
            pict_type: PICT_I,
            repeat_pict: 0,
        }
    }

    /// A new parser for the same codec context (ff_read_frame_flush):
    /// parameter sets, picture order and reference timestamp start over;
    /// the frame rate the parser set on the codec context stays.
    pub fn fresh(&self) -> Self {
        Self { framerate: self.framerate, ..Self::new(self.pkt_timebase) }
    }

    /// parse_nal_units on the access unit `buf`.
    pub fn parse_nal_units(&mut self, buf: &[u8]) {
        self.key_frame = false;
        self.pict_type = PICT_I;
        // ff_h264_sei_uninit, and x264_build unknown; the picture timing
        // payload buffer keeps its bytes.
        self.sei = Sei { payload: self.sei.payload, ..Sei::default() };
        let mut at = 0;
        loop {
            at = next_nal(buf, at);
            if at >= buf.len() {
                return;
            }
            let header = buf[at];
            let mut length = buf.len() - at;
            if matches!(header & 0x1F, 1 | 2 | 5) {
                // IDR and disposable slices are read to 60 bytes, others
                // up to their MMCOs (1000 bytes).
                let cap = if header & 0x1F == 5 || (header >> 5) & 3 == 0 { 60 } else { 1000 };
                length = length.min(cap);
            }
            let (nal, consumed) = extract_rbsp(&buf[at..], length);
            at += consumed;
            let mut gb = Gb::new(&nal.data, nal.size);
            gb.bit();
            let ref_idc = gb.bits(2);
            match gb.bits(5) {
                7 => self.decode_sps(&mut gb),
                8 => self.decode_pps(&mut gb),
                6 => {
                    let size = nal.size.saturating_sub(1);
                    let _ = self.decode_sei(nal.data.get(1..).unwrap_or(&[]), size);
                }
                kind @ (1 | 2 | 5) => {
                    let _ = self.slice(&mut gb, kind, ref_idc);
                    return;
                }
                _ => {}
            }
        }
    }

    /// What the parser set for the last unit parsed: key_frame, whether
    /// pict_type is B, and the duration compute_frame_duration (demux.c)
    /// takes from it: a frame of the frame rate the parser set on the
    /// codec context, counted as two fields (AV_CODEC_PROP_FIELDS) times
    /// 1 + repeat_pict, in pkt_timebase; 0 without a frame rate.
    pub fn video(&self) -> VideoCut {
        let (num, den) = (i64::from(self.framerate.0), i64::from(self.framerate.1));
        let (tb_num, tb_den) = self.pkt_timebase;
        let frame = if tb_num * 1000 > tb_den {
            (tb_num, tb_den)
        } else if den * 1000 > num {
            let (mut n, mut d) = av_reduce(den, num * 2, i64::from(i32::MAX));
            if self.repeat_pict != 0 {
                (n, d) = av_reduce(i64::from(n) * (1 + i64::from(self.repeat_pict)), i64::from(d), i64::from(i32::MAX));
            }
            (i64::from(n), i64::from(d))
        } else {
            (0, 0)
        };
        // av_rescale_rnd(1, num * time_base.den, den * time_base.num, AV_ROUND_DOWN)
        let duration = match (frame.0 * tb_den, frame.1 * tb_num) {
            (b, c) if frame.0 != 0 && frame.1 != 0 && b >= 0 && c > 0 => b / c,
            _ => 0,
        };
        VideoCut { key: self.key_frame, b_picture: self.pict_type == PICT_B, duration, rate_known: num != 0 }
    }

    /// h264_parse after parse_nal_units: the timestamps of the unit just
    /// parsed, from those fetched for it (`pts`, `dts`) and its picture
    /// timing and buffering period SEIs.
    pub fn timestamps(&mut self, pts: Option<i64>, dts: Option<i64>) -> (Option<i64>, Option<i64>) {
        let (mut pts, mut dts) = (pts.unwrap_or(NOPTS), dts.unwrap_or(NOPTS));
        let mut time_base = (0i64, 1i64);
        if self.framerate.0 != 0 {
            let doubled = av_reduce(i64::from(self.framerate.0) * 2, i64::from(self.framerate.1), i64::from(i32::MAX));
            time_base = (i64::from(doubled.1), i64::from(doubled.0));
        }
        let (sync_point, ref_dts_delta, pts_dts_delta) = if self.sei.cpb_removal_delay >= 0 {
            (i32::from(self.sei.buffering_period_present), self.sei.cpb_removal_delay, self.sei.dpb_output_delay)
        } else {
            (i32::MIN, i32::MIN, i32::MIN)
        };
        if sync_point >= 0 {
            let den = time_base.1 * self.pkt_timebase.0;
            if den > 0 {
                let num = time_base.0 * self.pkt_timebase.1;
                if dts != NOPTS {
                    // got DTS from the stream, update reference timestamp
                    self.reference_dts = dts.saturating_sub(av_rescale(i64::from(ref_dts_delta), num, den));
                } else if self.reference_dts != NOPTS {
                    // compute DTS based on reference timestamp
                    dts = self.reference_dts.saturating_add(av_rescale(i64::from(ref_dts_delta), num, den));
                }
                if self.reference_dts != NOPTS && pts == NOPTS {
                    let delta = av_rescale(i64::from(pts_dts_delta), num, den);
                    let sum = (dts as u64).wrapping_add(delta as u64) as i64;
                    if sum == dts.saturating_add(delta) {
                        pts = sum;
                    }
                }
                if sync_point > 0 {
                    // new reference
                    self.reference_dts = dts;
                }
            }
        }
        let some = |t: i64| (t != NOPTS).then_some(t);
        (some(pts), some(dts))
    }

    /// ff_h264_decode_seq_parameter_set (truncation not ignored).
    fn decode_sps(&mut self, gb: &mut Gb) {
        let Some((id, sps)) = read_sps(gb) else { return };
        self.sps[id] = Some(Arc::new(sps));
    }

    /// ff_h264_decode_picture_parameter_set as the parser calls it, with
    /// a bit length of 0: nothing past redundant_pic_cnt_present_flag is
    /// read.
    fn decode_pps(&mut self, gb: &mut Gb) {
        let pps_id = gb.ue() as u32;
        if pps_id >= 256 {
            return;
        }
        let sps_id = gb.ue31() as usize;
        let Some(sps) = self.sps.get(sps_id).cloned().flatten() else { return };
        if sps.bit_depth_luma > 14 || sps.bit_depth_luma == 11 || sps.bit_depth_luma == 13 {
            return;
        }
        gb.bit(); // entropy_coding_mode_flag
        let pic_order_present = gb.bit();
        let slice_group_count = gb.ue().wrapping_add(1);
        if slice_group_count > 1 {
            // FMO
            return;
        }
        let ref_count = [gb.ue().wrapping_add(1) as u32, gb.ue().wrapping_add(1) as u32];
        if ref_count[0].wrapping_sub(1) > 31 || ref_count[1].wrapping_sub(1) > 31 {
            return;
        }
        let weighted_pred = gb.bit();
        let weighted_bipred_idc = gb.bits(2);
        gb.se(); // pic_init_qp_minus26
        gb.se(); // pic_init_qs_minus26
        let chroma_qp_index_offset = gb.se();
        if !(-12..=12).contains(&chroma_qp_index_offset) {
            return;
        }
        gb.bit(); // deblocking_filter_control_present_flag
        gb.bit(); // constrained_intra_pred_flag
        let redundant_pic_cnt_present = gb.bit();
        self.pps[pps_id as usize] = Some(Arc::new(Pps {
            sps,
            ref_count,
            pic_order_present,
            weighted_pred,
            weighted_bipred_idc,
            redundant_pic_cnt_present,
        }));
    }

    /// ff_h264_sei_decode on the SEI payload bytes `sei` (the NAL after
    /// its header; past `len` only for bit reads beyond a message).
    fn decode_sei(&mut self, sei: &[u8], len: usize) -> i32 {
        let mut p = 0usize;
        let mut master = 0;
        while len - p > 2 && (sei[p] != 0 || sei[p + 1] != 0) {
            let mut kind: i32 = 0;
            loop {
                if len == p {
                    return INVALIDDATA;
                }
                let byte = sei[p];
                kind = kind.wrapping_add(i32::from(byte));
                p += 1;
                if byte != 255 {
                    break;
                }
            }
            let mut size: u32 = 0;
            loop {
                if len == p {
                    return INVALIDDATA;
                }
                let byte = sei[p];
                size = size.wrapping_add(u32::from(byte));
                p += 1;
                if byte != 255 {
                    break;
                }
            }
            if size as usize > len - p {
                return INVALIDDATA;
            }
            let size = size as usize;
            let payload = &sei[p..p + size];
            let mut bits = Gb::new(&sei[p..], size);
            let ret = match kind {
                // picture timing
                1 => {
                    if size > self.sei.payload.len() {
                        INVALIDDATA
                    } else {
                        self.sei.payload[..size].copy_from_slice(payload);
                        self.sei.payload_size = size;
                        self.sei.picture_timing_present = true;
                        0
                    }
                }
                // recovery point
                6 => {
                    let count = bits.ue_long();
                    if count >= 1 << 16 {
                        INVALIDDATA
                    } else {
                        self.sei.recovery_frame_cnt = count as i32;
                        0
                    }
                }
                // buffering period
                0 => {
                    let sps_id = bits.ue31();
                    if sps_id > 31 {
                        INVALIDDATA
                    } else if self.sps[sps_id as usize].is_none() {
                        PS_NOT_FOUND
                    } else {
                        self.sei.buffering_period_present = true;
                        0
                    }
                }
                // green metadata
                56 => 0,
                _ => self.h2645_sei(kind, payload, &mut bits),
            };
            if ret < 0 && ret != PS_NOT_FOUND {
                return ret;
            }
            if ret < 0 {
                master = ret;
            }
            p += size;
        }
        master
    }

    /// ff_h2645_sei_message_decode for H.264: whether FFmpeg fails on the
    /// message (negative) or reads it.
    fn h2645_sei(&mut self, kind: i32, payload: &[u8], bits: &mut Gb) -> i32 {
        let size = payload.len();
        match kind {
            // user data registered by ITU-T T.35
            4 => itut_t35(payload),
            // user data unregistered
            5 => {
                if size < 16 || size >= i32::MAX as usize - 1 {
                    return INVALIDDATA;
                }
                if let Some(build) = x264_build(&payload[16..]) {
                    if build > 0 {
                        self.sei.x264_build = build;
                    }
                    if build == 1 && payload[16..].starts_with(b"x264 - core 0000") {
                        self.sei.x264_build = 67;
                    }
                }
                0
            }
            // film grain characteristics
            19 => film_grain(bits),
            // alternative transfer characteristics
            147 if size < 1 => INVALIDDATA,
            // ambient viewing environment
            148 => {
                if size < 8 {
                    return INVALIDDATA;
                }
                let illuminance = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let x = u16::from_be_bytes([payload[4], payload[5]]);
                let y = u16::from_be_bytes([payload[6], payload[7]]);
                if illuminance == 0 || x > 50000 || y > 50000 { INVALIDDATA } else { 0 }
            }
            // mastering display colour volume
            137 if size < 24 => INVALIDDATA,
            // content light level
            144 if size < 4 => INVALIDDATA,
            _ => 0,
        }
    }

    /// The slice header case of parse_nal_units, for the first slice of
    /// the unit. `Err` where FFmpeg goes to `fail`.
    fn slice(&mut self, gb: &mut Gb, kind: u32, ref_idc: u32) -> Result<(), ()> {
        if kind == 5 {
            self.key_frame = true;
            self.poc.prev_frame_num = 0;
            self.poc.prev_frame_num_offset = 0;
            self.poc.prev_poc_msb = 0;
            self.poc.prev_poc_lsb = 0;
        }
        gb.ue_long(); // first_mb_in_slice
        let slice_type = gb.ue31();
        let pict_type = GOLOMB_TO_PICT_TYPE[(slice_type % 5) as usize];
        self.pict_type = pict_type;
        if self.sei.recovery_frame_cnt >= 0 {
            // key frame, since recovery_frame_cnt is set
            self.key_frame = true;
        }
        let pps_id = gb.ue() as u32;
        if pps_id >= 256 {
            return Err(());
        }
        let pps = self.pps[pps_id as usize].clone().ok_or(())?;
        let sps = pps.sps.clone();
        // heuristic to detect non marked keyframes
        if sps.ref_frame_count <= 1 && pps.ref_count[0] <= 1 && pict_type == PICT_I {
            self.key_frame = true;
        }
        self.poc.frame_num = gb.bits(sps.log2_max_frame_num) as i32;
        let structure = if sps.frame_mbs_only {
            PICT_FRAME
        } else if gb.bit() {
            PICT_TOP_FIELD + gb.bits(1)
        } else {
            PICT_FRAME
        };
        if kind == 5 {
            gb.ue_long(); // idr_pic_id
        }
        if sps.poc_type == 0 {
            self.poc.poc_lsb = gb.bits(sps.log2_max_poc_lsb) as i32;
            if pps.pic_order_present && structure == PICT_FRAME {
                self.poc.delta_poc_bottom = gb.se();
            }
        }
        if sps.poc_type == 1 && !sps.delta_pic_order_always_zero {
            self.poc.delta_poc[0] = gb.se();
            if pps.pic_order_present && structure == PICT_FRAME {
                self.poc.delta_poc[1] = gb.se();
            }
        }
        let field_poc = self.init_poc(&sps, structure, ref_idc)?;
        let mut got_reset = false;
        if ref_idc != 0 && kind != 5 {
            got_reset = scan_mmco_reset(gb, &sps, &pps, pict_type, structure)?;
        }
        let poc = &mut self.poc;
        poc.prev_frame_num = if got_reset { 0 } else { poc.frame_num };
        poc.prev_frame_num_offset = if got_reset { 0 } else { poc.frame_num_offset };
        if ref_idc != 0 {
            if !got_reset {
                poc.prev_poc_msb = poc.poc_msb;
                poc.prev_poc_lsb = poc.poc_lsb;
            } else {
                poc.prev_poc_msb = 0;
                poc.prev_poc_lsb = if structure == PICT_BOTTOM_FIELD { 0 } else { field_poc[0] };
            }
        }
        if self.sei.picture_timing_present && !self.process_picture_timing(&sps) {
            self.sei.picture_timing_present = false;
        }
        let frame = i32::from(structure == PICT_FRAME);
        self.repeat_pict = if sps.pic_struct_present && self.sei.picture_timing_present {
            match self.sei.pic_struct {
                // top or bottom field
                1 | 2 => 0,
                // frame, top-bottom, bottom-top
                0 | 3 | 4 => 1,
                5 | 6 => 2,
                // frame doubling, tripling
                7 => 3,
                8 => 5,
                _ => frame,
            }
        } else {
            frame
        };
        if sps.timing_info_present {
            let mut den = i64::from(sps.time_scale);
            if (self.sei.x264_build as u32) < 44 {
                den *= 2;
            }
            let (den_part, num_part) = av_reduce(i64::from(sps.num_units_in_tick.wrapping_mul(2)), den, 1 << 30);
            self.framerate = (num_part, den_part);
        }
        Ok(())
    }

    /// ff_h264_init_poc: the field POCs, `Err` when one exceeds an int.
    fn init_poc(&mut self, sps: &Sps, structure: u32, ref_idc: u32) -> Result<[i32; 2], ()> {
        let pc = &mut self.poc;
        let max_frame_num = 1i32 << sps.log2_max_frame_num;
        pc.frame_num_offset = pc.prev_frame_num_offset;
        if pc.frame_num < pc.prev_frame_num {
            pc.frame_num_offset = pc.frame_num_offset.wrapping_add(max_frame_num);
        }
        let field: [i64; 2] = match sps.poc_type {
            0 => {
                let max_poc_lsb = 1i32 << sps.log2_max_poc_lsb;
                if pc.prev_poc_lsb < 0 {
                    pc.prev_poc_lsb = pc.poc_lsb;
                }
                pc.poc_msb = if pc.poc_lsb < pc.prev_poc_lsb
                    && pc.prev_poc_lsb.wrapping_sub(pc.poc_lsb) >= max_poc_lsb / 2
                {
                    pc.prev_poc_msb.wrapping_add(max_poc_lsb)
                } else if pc.poc_lsb > pc.prev_poc_lsb && pc.prev_poc_lsb.wrapping_sub(pc.poc_lsb) < -max_poc_lsb / 2 {
                    pc.prev_poc_msb.wrapping_sub(max_poc_lsb)
                } else {
                    pc.prev_poc_msb
                };
                let top = i64::from(pc.poc_msb.wrapping_add(pc.poc_lsb));
                let bottom = if structure == PICT_FRAME { top + i64::from(pc.delta_poc_bottom) } else { top };
                [top, bottom]
            }
            1 => {
                let cycle = sps.offset_for_ref_frame.len() as i32;
                let mut abs_frame_num = if cycle != 0 { pc.frame_num_offset.wrapping_add(pc.frame_num) } else { 0 };
                if ref_idc == 0 && abs_frame_num > 0 {
                    abs_frame_num -= 1;
                }
                let per_cycle = sps.offset_for_ref_frame.iter().fold(0i64, |s, &o| s.wrapping_add(i64::from(o)));
                let mut expected = 0i64;
                if abs_frame_num > 0 {
                    let cycles = (abs_frame_num - 1) / cycle;
                    let in_cycle = (abs_frame_num - 1) % cycle;
                    expected = i64::from(cycles).wrapping_mul(per_cycle);
                    for &o in &sps.offset_for_ref_frame[..=in_cycle as usize] {
                        expected = expected.wrapping_add(i64::from(o));
                    }
                }
                if ref_idc == 0 {
                    expected = expected.wrapping_add(i64::from(sps.offset_for_non_ref_pic));
                }
                let top = expected.wrapping_add(i64::from(pc.delta_poc[0]));
                let mut bottom = top.wrapping_add(i64::from(sps.offset_for_top_to_bottom_field));
                if structure == PICT_FRAME {
                    bottom = bottom.wrapping_add(i64::from(pc.delta_poc[1]));
                }
                [top, bottom]
            }
            _ => {
                let mut poc = pc.frame_num_offset.wrapping_add(pc.frame_num).wrapping_mul(2);
                if ref_idc == 0 {
                    poc = poc.wrapping_sub(1);
                }
                [i64::from(poc), i64::from(poc)]
            }
        };
        if field.iter().any(|&f| f != i64::from(f as i32)) {
            return Err(());
        }
        let mut out = [i32::MAX; 2];
        if structure != PICT_BOTTOM_FIELD {
            out[0] = field[0] as i32;
        }
        if structure != PICT_TOP_FIELD {
            out[1] = field[1] as i32;
        }
        Ok(out)
    }

    /// ff_h264_sei_process_picture_timing as far as the parser uses it:
    /// the CPB and DPB delays and pic_struct; false for an invalid
    /// pic_struct.
    fn process_picture_timing(&mut self, sps: &Sps) -> bool {
        let payload = self.sei.payload;
        let mut gb = Gb::new(&payload, self.sei.payload_size);
        if sps.nal_hrd || sps.vcl_hrd {
            self.sei.cpb_removal_delay = gb.bits(sps.cpb_removal_delay_length) as i32;
            self.sei.dpb_output_delay = gb.bits(sps.dpb_output_delay_length) as i32;
        }
        if sps.pic_struct_present {
            self.sei.pic_struct = gb.bits(4);
            return self.sei.pic_struct <= 8;
        }
        true
    }
}

/// ff_h264_decode_seq_parameter_set: the SPS and its id, `None` where
/// FFmpeg rejects it.
fn read_sps(gb: &mut Gb) -> Option<(usize, Sps)> {
    let profile_idc = gb.bits(8);
    gb.bits(8); // constraint_set0..5_flag, reserved_zero_2bits
    gb.bits(8); // level_idc
    let sps_id = gb.ue31() as usize;
    if sps_id >= 32 {
        return None;
    }
    let (mut chroma_format_idc, mut bit_depth_luma) = (1, 8);
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 144) {
        chroma_format_idc = gb.ue31();
        if chroma_format_idc > 3 || (chroma_format_idc == 3 && gb.bit()) {
            // separate color planes are not supported
            return None;
        }
        bit_depth_luma = gb.ue31() + 8;
        let bit_depth_chroma = gb.ue31() + 8;
        if bit_depth_chroma != bit_depth_luma || !(8..=14).contains(&bit_depth_luma) {
            return None;
        }
        gb.bit(); // qpprime_y_zero_transform_bypass_flag
        if gb.bit() && !scaling_matrices(gb, chroma_format_idc) {
            return None;
        }
    }
    let log2_max_frame_num = gb.ue31() + 4;
    if log2_max_frame_num > 16 {
        return None;
    }
    let poc_type = gb.ue31();
    let mut sps = Sps {
        ref_frame_count: 0,
        bit_depth_luma,
        chroma_format_idc,
        log2_max_frame_num,
        frame_mbs_only: true,
        poc_type,
        log2_max_poc_lsb: 0,
        delta_pic_order_always_zero: false,
        offset_for_non_ref_pic: 0,
        offset_for_top_to_bottom_field: 0,
        offset_for_ref_frame: Vec::new(),
        timing_info_present: false,
        num_units_in_tick: 0,
        time_scale: 0,
        nal_hrd: false,
        vcl_hrd: false,
        cpb_removal_delay_length: 0,
        dpb_output_delay_length: 0,
        pic_struct_present: false,
    };
    match poc_type {
        0 => {
            let t = gb.ue31();
            if t > 12 {
                return None;
            }
            sps.log2_max_poc_lsb = t + 4;
        }
        1 => {
            sps.delta_pic_order_always_zero = gb.bit();
            sps.offset_for_non_ref_pic = gb.se_long();
            sps.offset_for_top_to_bottom_field = gb.se_long();
            if sps.offset_for_non_ref_pic == i32::MIN || sps.offset_for_top_to_bottom_field == i32::MIN {
                return None;
            }
            let cycle = gb.ue() as u32;
            if cycle >= 256 {
                return None;
            }
            for _ in 0..cycle {
                let offset = gb.se_long();
                if offset == i32::MIN {
                    return None;
                }
                sps.offset_for_ref_frame.push(offset);
            }
        }
        2 => {}
        _ => return None,
    }
    sps.ref_frame_count = gb.ue31();
    if sps.ref_frame_count > 16 {
        return None;
    }
    gb.bit(); // gaps_in_frame_num_allowed_flag
    let mb_width = gb.ue().wrapping_add(1);
    let mut mb_height = gb.ue().wrapping_add(1);
    sps.frame_mbs_only = gb.bit();
    if mb_height as u32 >= i32::MAX as u32 / 2 {
        return None;
    }
    mb_height = mb_height.wrapping_mul(2 - i32::from(sps.frame_mbs_only));
    if !sps.frame_mbs_only {
        gb.bit(); // mb_adaptive_frame_field_flag
    }
    if mb_width as u32 >= i32::MAX as u32 / 16
        || mb_height as u32 >= i32::MAX as u32 / 16
        || !image_size_ok(16 * mb_width as u32, 16 * mb_height as u32)
    {
        return None;
    }
    gb.bit(); // direct_8x8_inference_flag
    if gb.bit() {
        let [left, right, top, bottom] = [gb.ue() as u32, gb.ue() as u32, gb.ue() as u32, gb.ue() as u32];
        let (width, height) = (16 * mb_width as u32, 16 * mb_height as u32);
        let vsub = u32::from(chroma_format_idc == 1);
        let hsub = u32::from(chroma_format_idc == 1 || chroma_format_idc == 2);
        let step_x = 1u32 << hsub;
        let step_y = (2 - u32::from(sps.frame_mbs_only)) << vsub;
        let limit = |step: u32| i32::MAX as u32 / 4 / step;
        if left > limit(step_x)
            || right > limit(step_x)
            || top > limit(step_y)
            || bottom > limit(step_y)
            || (left + right) * step_x >= width
            || (top + bottom) * step_y >= height
        {
            return None;
        }
    }
    if gb.bit() && !vui_parameters(gb, &mut sps) {
        return None;
    }
    if gb.left() < 0 {
        // overread
        return None;
    }
    Some((sps_id, sps))
}

/// decode_scaling_matrices for an SPS with its present flag set: false
/// when a list holds an invalid delta.
fn scaling_matrices(gb: &mut Gb, chroma_format_idc: u32) -> bool {
    let lists = 6 + 2 + if chroma_format_idc == 3 { 4 } else { 0 };
    let mut ok = true;
    for list in 0..lists {
        let size = if list < 6 { 16 } else { 64 };
        // decode_scaling_list
        if !gb.bit() {
            continue;
        }
        let (mut last, mut next) = (8i32, 8i32);
        for i in 0..size {
            if next != 0 {
                let v = gb.se();
                if !(-128..=127).contains(&v) {
                    ok = false;
                    break;
                }
                next = (last + v) & 0xFF;
            }
            if i == 0 && next == 0 {
                break;
            }
            if next != 0 {
                last = next;
            }
        }
    }
    ok
}

/// decode_vui_parameters: false where FFmpeg rejects the SPS.
fn vui_parameters(gb: &mut Gb, sps: &mut Sps) -> bool {
    // ff_h2645_decode_common_vui_params
    if gb.bit() && gb.bits(8) == 255 {
        // EXTENDED_SAR
        gb.bits(16);
        gb.bits(16);
    }
    if gb.bit() {
        gb.bit(); // overscan_appropriate_flag
    }
    if gb.bit() {
        gb.bits(3); // video_format
        gb.bit(); // video_full_range_flag
        if gb.bit() {
            gb.bits(8);
            gb.bits(8);
            gb.bits(8);
        }
    }
    if gb.bit() {
        gb.ue31();
        gb.ue31();
    }
    if gb.peek(1) == 1 && gb.left() < 10 {
        // Truncated VUI
        return true;
    }
    sps.timing_info_present = gb.bit();
    if sps.timing_info_present {
        let num_units_in_tick = gb.bits(32);
        let time_scale = gb.bits(32);
        if num_units_in_tick == 0 || time_scale == 0 {
            sps.timing_info_present = false;
        } else {
            sps.num_units_in_tick = num_units_in_tick;
            sps.time_scale = time_scale;
        }
        gb.bit(); // fixed_frame_rate_flag
    }
    sps.nal_hrd = gb.bit();
    if sps.nal_hrd && !hrd_parameters(gb, sps) {
        return false;
    }
    sps.vcl_hrd = gb.bit();
    if sps.vcl_hrd && !hrd_parameters(gb, sps) {
        return false;
    }
    if sps.nal_hrd || sps.vcl_hrd {
        gb.bit(); // low_delay_hrd_flag
    }
    sps.pic_struct_present = gb.bit();
    if gb.left() == 0 {
        return true;
    }
    if gb.bit() {
        // bitstream_restriction_flag
        gb.bit();
        for _ in 0..4 {
            gb.ue31();
        }
        let mut num_reorder_frames = gb.ue31();
        gb.ue31(); // max_dec_frame_buffering
        if gb.left() < 0 {
            num_reorder_frames = 0;
        }
        if num_reorder_frames > 16 {
            return false;
        }
    }
    true
}

/// decode_hrd_parameters: false for more than 32 CPBs.
fn hrd_parameters(gb: &mut Gb, sps: &mut Sps) -> bool {
    let cpb_count = gb.ue31() + 1;
    if cpb_count > 32 {
        return false;
    }
    gb.bits(4); // bit_rate_scale
    gb.bits(4); // cpb_size_scale
    for _ in 0..cpb_count {
        gb.ue_long();
        gb.ue_long();
        gb.bit();
    }
    gb.bits(5); // initial_cpb_removal_delay_length_minus1
    sps.cpb_removal_delay_length = gb.bits(5) + 1;
    sps.dpb_output_delay_length = gb.bits(5) + 1;
    gb.bits(5); // time_offset_length
    true
}

/// scan_mmco_reset: whether the slice resets with MMCO 5; `Err` where
/// FFmpeg fails.
fn scan_mmco_reset(gb: &mut Gb, sps: &Sps, pps: &Pps, pict_type: u32, structure: u32) -> Result<bool, ()> {
    let slice_type_nos = pict_type & 3;
    if pps.redundant_pic_cnt_present {
        gb.ue(); // redundant_pic_count
    }
    if slice_type_nos == PICT_B {
        gb.bit(); // direct_spatial_mv_pred
    }
    // ff_h264_parse_ref_count
    let mut ref_count = [pps.ref_count[0] as i32, pps.ref_count[1] as i32];
    let mut list_count = 0;
    if slice_type_nos != PICT_I {
        let max: u32 = if structure == PICT_FRAME { 15 } else { 31 };
        if gb.bit() {
            ref_count[0] = gb.ue().wrapping_add(1);
            ref_count[1] = if slice_type_nos == PICT_B { gb.ue().wrapping_add(1) } else { 1 };
        }
        list_count = if slice_type_nos == PICT_B { 2 } else { 1 };
        if ref_count[0].wrapping_sub(1) as u32 > max || (list_count == 2 && ref_count[1].wrapping_sub(1) as u32 > max) {
            return Err(());
        } else if ref_count[1].wrapping_sub(1) as u32 > max {
            ref_count[1] = 0;
        }
    } else {
        ref_count = [0, 0];
    }
    if slice_type_nos != PICT_I {
        for &count in &ref_count[..list_count] {
            if gb.bit() {
                let mut index = 0i32;
                loop {
                    let reordering_of_pic_nums_idc = gb.ue31();
                    if reordering_of_pic_nums_idc < 3 {
                        gb.ue_long();
                    } else if reordering_of_pic_nums_idc > 3 {
                        return Err(());
                    } else {
                        break;
                    }
                    if index >= count {
                        return Err(());
                    }
                    index += 1;
                }
            }
        }
    }
    if (pps.weighted_pred && slice_type_nos == PICT_P) || (pps.weighted_bipred_idc == 1 && slice_type_nos == PICT_B) {
        pred_weight_table(gb, sps, ref_count, slice_type_nos);
    }
    if gb.bit() {
        // adaptive_ref_pic_marking_mode_flag
        for _ in 0..MAX_MMCO_COUNT {
            if gb.left() < 1 {
                return Err(());
            }
            let opcode = gb.ue31();
            if opcode > 6 {
                return Err(());
            }
            match opcode {
                0 => return Ok(false),
                5 => return Ok(true),
                _ => {}
            }
            if opcode == 1 || opcode == 3 {
                gb.ue_long(); // difference_of_pic_nums_minus1
            }
            if matches!(opcode, 2 | 3 | 4 | 6) {
                gb.ue31();
            }
        }
    }
    Ok(false)
}

/// ff_h264_pred_weight_table as far as it reads: it stops at a weight
/// outside -128..=127.
fn pred_weight_table(gb: &mut Gb, sps: &Sps, ref_count: [i32; 2], slice_type_nos: u32) {
    let int8 = |v: i32| v == i32::from(v as i8);
    gb.ue31(); // luma_log2_weight_denom
    if sps.chroma_format_idc != 0 {
        gb.ue31(); // chroma_log2_weight_denom
    }
    for &count in &ref_count {
        for _ in 0..count.max(0) {
            if gb.bit() {
                let (weight, offset) = (gb.se(), gb.se());
                if !int8(weight) || !int8(offset) {
                    return;
                }
            }
            if sps.chroma_format_idc != 0 && gb.bit() {
                for _ in 0..2 {
                    let (weight, offset) = (gb.se(), gb.se());
                    if !int8(weight) || !int8(offset) {
                        return;
                    }
                }
            }
        }
        if slice_type_nos != PICT_B {
            break;
        }
    }
}

/// decode_film_grain_characteristics for H.264: INVALIDDATA for more
/// than six model values.
fn film_grain(gb: &mut Gb) -> i32 {
    if gb.bit() {
        // film_grain_characteristics_cancel_flag
        return 0;
    }
    gb.bits(2); // model_id
    if gb.bit() {
        gb.bits(3);
        gb.bits(3);
        gb.bit();
        gb.bits(8);
        gb.bits(8);
        gb.bits(8);
    }
    gb.bits(2); // blending_mode_id
    gb.bits(4); // log2_scale_factor
    let present = [gb.bit(), gb.bit(), gb.bit()];
    for present in present {
        if !present {
            continue;
        }
        let intervals = gb.bits(8) + 1;
        let model_values = gb.bits(3) + 1;
        if model_values > 6 {
            return INVALIDDATA;
        }
        for _ in 0..intervals {
            gb.bits(8);
            gb.bits(8);
            for _ in 0..model_values {
                gb.se_long();
            }
        }
    }
    gb.ue_long(); // film_grain_characteristics_repetition_period
    0
}

/// decode_registered_user_data: ff_itut_t35_parse_buffer, then the A/53
/// caption check of ff_itut_t35_parse_payload_to_struct.
fn itut_t35(payload: &[u8]) -> i32 {
    // bytestream2: reads past the end give 0 and do not move
    let mut p = 0usize;
    let left = |p: usize| payload.len() - p;
    let take = |p: &mut usize, n: usize| -> u32 {
        if payload.len() - *p < n {
            return 0;
        }
        let v = payload[*p..*p + n].iter().fold(0u32, |v, &b| (v << 8) | u32::from(b));
        *p += n;
        v
    };
    let country = take(&mut p, 1);
    if country == 0xFF {
        if left(p) < 1 {
            return INVALIDDATA;
        }
        p += 1; // itu_t_t35_country_code_extension_byte
    }
    let mut a53 = false;
    match country {
        0xB5 => {
            if left(p) < 2 {
                return INVALIDDATA;
            }
            match take(&mut p, 2) {
                // ATSC
                0x0031 => {
                    if left(p) < 4 {
                        return INVALIDDATA;
                    }
                    match take(&mut p, 4) {
                        // DTG1: afd_data
                        0x4454_4731 => {
                            if left(p) < 2 {
                                return INVALIDDATA;
                            }
                            if take(&mut p, 1) & 0x40 == 0 {
                                return 0;
                            }
                        }
                        // GA94: closed captions
                        0x4741_3934 => a53 = true,
                        _ => return 0,
                    }
                }
                // AOM
                0x5890 => {
                    if left(p) < 1 {
                        return INVALIDDATA;
                    }
                    if take(&mut p, 1) != 1 {
                        return 0;
                    }
                }
                // Samsung
                0x003C => {
                    if left(p) < 3 {
                        return INVALIDDATA;
                    }
                    let (code, application) = (take(&mut p, 2), take(&mut p, 1));
                    if code != 1 || application != 4 {
                        return 0;
                    }
                }
                // Dolby
                0x003B => {
                    if left(p) < 4 {
                        return INVALIDDATA;
                    }
                    if take(&mut p, 4) != 0x800 {
                        return 0;
                    }
                }
                // SMPTE
                0x0090 => {
                    if left(p) < 2 {
                        return INVALIDDATA;
                    }
                    if take(&mut p, 2) != 1 {
                        return 0;
                    }
                }
                _ => return 0,
            }
        }
        0xB4 => {
            if left(p) < 3 {
                return INVALIDDATA;
            }
            p += 1; // t35_uk_country_code_second_octet
            if take(&mut p, 2) != 0x5000 {
                return 0;
            }
        }
        0x26 => {
            if left(p) < 2 {
                return INVALIDDATA;
            }
            if take(&mut p, 2) != 0x0004 {
                return 0;
            }
            if left(p) < 2 {
                return INVALIDDATA;
            }
            if take(&mut p, 2) != 0x0005 {
                return 0;
            }
        }
        _ => return 0,
    }
    if left(p) == 0 {
        return INVALIDDATA;
    }
    if !a53 {
        return 0;
    }
    // ff_parse_a53_cc
    let data = &payload[p..];
    if data.len() < 3 {
        return INVALIDDATA;
    }
    if data[0] != 3 || data[1] & 0x40 == 0 {
        return 0;
    }
    let cc_count = usize::from(data[1] & 0x1F);
    if cc_count != 0 && cc_count * 3 >= data.len() - 3 {
        return INVALIDDATA;
    }
    0
}

/// sscanf(user_data + 16, "x264 - core %d") on the NUL-terminated user
/// data: the build when it matched.
fn x264_build(data: &[u8]) -> Option<i32> {
    let text = data.split(|&b| b == 0).next().unwrap_or(&[]);
    let space = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r');
    let mut p = 0;
    for &want in b"x264 - core " {
        if want == b' ' {
            while p < text.len() && space(text[p]) {
                p += 1;
            }
        } else if text.get(p) == Some(&want) {
            p += 1;
        } else {
            return None;
        }
    }
    while p < text.len() && space(text[p]) {
        p += 1;
    }
    let negative = match text.get(p) {
        Some(b'-') => {
            p += 1;
            true
        }
        Some(b'+') => {
            p += 1;
            false
        }
        _ => false,
    };
    let digits = text[p..].iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let value = text[p..p + digits].iter().fold(0i64, |v, &d| (v * 10 + i64::from(d - b'0')).min(i64::from(u32::MAX)));
    let value = if negative { -value } else { value };
    Some(value as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// libavutil's arithmetic on the values HRD timing feeds it: a frame
    /// rate reduced within its bound, deltas rounded half away from zero,
    /// an overflow as AV_NOPTS_VALUE.
    #[test]
    fn reduce_and_rescale_match_libavutil() {
        assert_eq!(av_reduce(2, 50, 1 << 30), (1, 25));
        // past the bound: the best approximation within it
        assert_eq!(av_reduce(1_000_000_007, 3, 1000), (1000, 1));
        assert_eq!(av_rescale(3, 90_000, 50), 5400);
        assert_eq!(av_rescale(-3, 90_000, 50), -5400);
        assert_eq!(av_rescale(1, 1, 2), 1);
        assert_eq!(av_rescale(-1, 1, 2), -1);
        assert_eq!(av_rescale(i64::MAX, 2, 1), NOPTS);
    }
}
