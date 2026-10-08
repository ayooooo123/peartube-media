// Ported from FFmpeg (commit 2da55bf): libavformat/m4vdec.c
// (mpeg4video_probe), libavcodec/mpeg4video_parser.c
// (mpeg4_find_frame_end, mpeg4_decode_header) and the parts of
// libavcodec/mpeg4videodec.c the parser runs for picture types and times
// (ff_mpeg4_parse_picture_header, decode_vol_header,
// decode_studio_vol_header, decode_vop_header, decode_studio_vop_header,
// decode_studiovisualobject, mpeg4_decode_gop_header,
// mpeg4_decode_profile_level), with the timestamps libavformat/demux.c
// (compute_pkt_fields, compute_frame_duration) gives the parsed units.
// License: LGPL-2.1-or-later
//
// Raw MPEG-4 Part 2 video (FFmpeg's m4v, also behind `.h263` names): each
// unit is a VOP with the headers before it, cut where FFmpeg's parser cuts
// it; key when the VOP is an I-VOP. The parser reads every unit's VOP time
// (PARSER_FLAG_USE_CODEC_TS), so a unit's pts is its VOP time in
// 1/1200000; the demuxer layer adds the dts: a B-VOP's pts, or with
// B-frame delay the previous I/P-VOP's pts.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, ProbeData, ProbeScore, ReadSeek, Result,
    StreamInfo, TimeBase, PROBE_SCORE_EXTENSION,
};

use crate::parser::{Combine, Split, Unit, END_NOT_FOUND};
use crate::rawvideo::{add_stable, rescale, returned, RawVideoDemuxer, Stamp, Units, RAW_VIDEO_CLOCK, RELATIVE_TS_BASE};

const VOS_STARTCODE: u32 = 0x1B0;
const GOP_STARTCODE: u32 = 0x1B3;
const VISUAL_OBJ_STARTCODE: u32 = 0x1B5;
const VOP_STARTCODE: u32 = 0x1B6;
const SLICE_STARTCODE: u32 = 0x1B7;
const EXT_STARTCODE: u32 = 0x1B8;

/// AV_PICTURE_TYPE_I, _P, _B, _S.
const PICT_I: u8 = 1;
const PICT_P: u8 = 2;
const PICT_B: u8 = 3;
const PICT_S: u8 = 4;

const RECT_SHAPE: u32 = 0;
const BIN_ONLY_SHAPE: u32 = 2;
const GRAY_SHAPE: u32 = 3;
const SIMPLE_VO_TYPE: u32 = 1;
const ADV_SIMPLE_VO_TYPE: u32 = 17;
const SIMPLE_STUDIO_VO_TYPE: u32 = 14;
const CORE_STUDIO_VO_TYPE: u32 = 15;
const STATIC_SPRITE: u32 = 1;
const GMC_SPRITE: u32 = 2;
/// AV_PROFILE_MPEG4_SIMPLE_STUDIO
const PROFILE_SIMPLE_STUDIO: u32 = 14;
/// FF_ASPECT_EXTENDED
const ASPECT_EXTENDED: u32 = 15;
/// CHROMA_420, CHROMA_422 (mpegvideo.h)
const CHROMA_420: u32 = 1;
const CHROMA_422: u32 = 2;

/// mpeg4video_probe.
pub fn probe_m4v(probe: &ProbeData) -> ProbeScore {
    let mut temp: u32 = u32::MAX;
    let (mut vo, mut vol, mut vop, mut viso, mut res, mut res_main) = (0, 0, 0, 0, 0, 0);
    for &b in probe.buf {
        temp = (temp << 8) | u32::from(b);
        if temp & 0xFFFF_FE00 != 0 || temp < 2 {
            continue;
        }
        if temp == VOP_STARTCODE {
            vop += 1;
        } else if temp == VISUAL_OBJ_STARTCODE {
            viso += 1;
        } else if (0x100..0x120).contains(&temp) {
            vo += 1;
        } else if (0x120..0x130).contains(&temp) {
            vol += 1;
        } else if temp == SLICE_STARTCODE || temp == EXT_STARTCODE {
            res_main += 1;
        } else if !(0x1AF < temp && temp < 0x1B7) && !(0x1B9 < temp && temp < 0x1C4) {
            res += 1;
        }
    }
    // Reserved codes of the main profile count when it looks like one.
    if res_main > 0 && 2 * res_main < vop {
        res += res_main;
    }
    if vop >= viso && vop >= vol && vo >= vol && vol > 0 && res == 0 {
        return if vop + vo > 4 { PROBE_SCORE_EXTENSION } else { PROBE_SCORE_EXTENSION / 2 };
    }
    if vop >= viso && vop >= vol && vo >= vol && vol > 0 && vop + vo > 4 {
        return PROBE_SCORE_EXTENSION / 10;
    }
    0
}

/// FFmpeg's checked GetBitContext over a parsed unit: zeros past its end,
/// the position held 8 bits past it.
struct Bits<'a> {
    data: &'a [u8],
    size: u64,
    pos: u64,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, size: data.len() as u64 * 8, pos: 0 }
    }

    fn left(&self) -> i64 {
        self.size as i64 - self.pos as i64
    }

    fn show(&self, n: u32) -> u32 {
        (0..u64::from(n)).fold(0u32, |v, k| {
            let p = self.pos + k;
            let bit = self.data.get((p / 8) as usize).map_or(0, |b| (b >> (7 - p % 8)) & 1);
            (v << 1) | u32::from(bit)
        })
    }

    fn skip(&mut self, n: u32) {
        self.pos = (self.pos + u64::from(n)).min(self.size + 8);
    }

    fn get(&mut self, n: u32) -> u32 {
        let v = self.show(n);
        self.skip(n);
        v
    }

    fn bit(&mut self) -> bool {
        self.get(1) == 1
    }

    fn align(&mut self) {
        let r = (8 - self.pos % 8) % 8;
        self.skip(r as u32);
    }
}

/// av_log2.
fn av_log2(v: u32) -> u32 {
    if v == 0 { 0 } else { 31 - v.leading_zeros() }
}

/// What mpeg4_decode_header leaves in the parser's MPEG-4 context and the
/// codec context: the picture type, the VOP time and the VOL fields its
/// parsing needs.
#[derive(Clone)]
struct Headers {
    /// avctx->framerate: time_increment_resolution and the fixed VOP
    /// increment.
    framerate: (i64, i64),
    /// avctx->profile, once a VOS header gave one.
    profile: Option<u32>,
    studio_profile: bool,
    /// avctx->bits_per_raw_sample: a studio VOL's bit depth.
    bits_per_raw_sample: u32,
    vo_type: u32,
    vol_control_parameters: bool,
    low_delay: bool,
    shape: u32,
    time_increment_bits: u32,
    vol_sprite_usage: u32,
    pict_type: u8,
    time_base: i64,
    last_time_base: i64,
    time: i64,
    last_non_b_time: i64,
    width: u32,
    height: u32,
}

impl Default for Headers {
    fn default() -> Self {
        Self {
            // avcodec_alloc_context3
            framerate: (0, 1),
            profile: None,
            studio_profile: false,
            bits_per_raw_sample: 0,
            vo_type: 0,
            vol_control_parameters: false,
            low_delay: false,
            shape: RECT_SHAPE,
            time_increment_bits: 0,
            vol_sprite_usage: 0,
            pict_type: 0,
            time_base: 0,
            last_time_base: 0,
            time: 0,
            last_non_b_time: 0,
            width: 0,
            height: 0,
        }
    }
}

/// An error return of FFmpeg's header parsing (the unit gets no pts).
struct Invalid;

impl Headers {
    /// ff_mpeg4_parse_picture_header(header = 0, parse_only = 1).
    fn parse(&mut self, unit: &[u8]) -> std::result::Result<(), Invalid> {
        let mut gb = Bits::new(unit);
        if !self.studio_profile && self.bits_per_raw_sample != 8 {
            self.bits_per_raw_sample = 0;
        }
        let mut startcode: u32 = 0xFF;
        let mut vol = 0;
        loop {
            if gb.pos >= gb.size {
                // The end of the unit without a VOP.
                return Err(Invalid);
            }
            startcode = (startcode << 8) | gb.get(8);
            if startcode & 0xFFFF_FF00 != 0x100 {
                continue;
            }
            if (0x120..=0x12F).contains(&startcode) {
                if vol > 0 {
                    // Ignoring multiple VOL headers.
                    continue;
                }
                vol += 1;
                self.decode_vol_header(&mut gb)?;
            } else if startcode == GOP_STARTCODE {
                self.decode_gop_header(&mut gb);
            } else if startcode == VOS_STARTCODE {
                let profile = gb.get(4);
                let mut level = gb.get(4);
                if profile == 0 && level == 8 {
                    level = 0;
                }
                if profile == PROFILE_SIMPLE_STUDIO && level > 0 && level < 9 {
                    self.studio_profile = true;
                } else if self.studio_profile {
                    return Err(Invalid);
                }
                self.profile = Some(profile);
            } else if startcode == VISUAL_OBJ_STARTCODE {
                if self.studio_profile {
                    // decode_studiovisualobject: video objects only.
                    gb.skip(4);
                    if gb.get(4) != 1 {
                        return Err(Invalid);
                    }
                }
            } else if startcode == VOP_STARTCODE {
                break;
            }
            // User data, extensions and the rest are skipped by the scan.
            gb.align();
            startcode = 0xFF;
        }
        if self.studio_profile {
            if self.bits_per_raw_sample == 0 {
                return Err(Invalid);
            }
            self.decode_studio_vop_header(&mut gb);
            Ok(())
        } else {
            self.decode_vop_header(&mut gb)
        }
    }

    /// mpeg4_decode_gop_header: the time code's seconds become the time
    /// base.
    fn decode_gop_header(&mut self, gb: &mut Bits) {
        if gb.show(23) == 0 {
            return;
        }
        let hours = i64::from(gb.get(5));
        let minutes = i64::from(gb.get(6));
        gb.skip(1);
        let seconds = i64::from(gb.get(6));
        self.time_base = seconds + 60 * (minutes + 60 * hours);
        gb.skip(2);
    }

    /// decode_vol_header, as far as its fields and failures reach.
    fn decode_vol_header(&mut self, gb: &mut Bits) -> std::result::Result<(), Invalid> {
        gb.skip(1); // random access
        self.vo_type = gb.get(8);
        if self.vo_type == CORE_STUDIO_VO_TYPE || self.vo_type == SIMPLE_STUDIO_VO_TYPE {
            if self.profile.is_some_and(|p| p != PROFILE_SIMPLE_STUDIO) {
                return Err(Invalid);
            }
            self.studio_profile = true;
            self.profile = Some(PROFILE_SIMPLE_STUDIO);
            return self.decode_studio_vol_header(gb);
        } else if self.studio_profile {
            return Err(Invalid);
        }
        let vo_ver_id = if gb.bit() {
            let v = gb.get(4);
            gb.skip(3); // vo_priority
            v
        } else {
            1
        };
        if gb.get(4) == ASPECT_EXTENDED {
            gb.skip(16); // par_width, par_height
        }
        self.vol_control_parameters = gb.bit();
        if self.vol_control_parameters {
            gb.skip(2); // chroma_format
            self.low_delay = gb.bit();
            if gb.bit() {
                // vbv parameters, each followed by a marker
                for n in [15, 15, 15] {
                    gb.skip(n + 1);
                }
                gb.skip(3);
                gb.skip(11 + 1);
                gb.skip(15 + 1);
            }
        } else {
            // The parser decodes no picture: picture_number stays 0.
            self.low_delay = self.vo_type == SIMPLE_VO_TYPE || self.vo_type == ADV_SIMPLE_VO_TYPE;
        }
        self.shape = gb.get(2);
        if self.shape == GRAY_SHAPE && vo_ver_id != 1 {
            gb.skip(4); // video_object_layer_shape_extension
        }
        gb.skip(1); // marker
        let resolution = gb.get(16);
        self.framerate.0 = i64::from(resolution);
        if resolution == 0 {
            return Err(Invalid);
        }
        self.time_increment_bits = (av_log2(resolution - 1) + 1).max(1);
        gb.skip(1); // marker
        self.framerate.1 = if gb.bit() { i64::from(gb.get(self.time_increment_bits)) } else { 1 };
        if self.shape != BIN_ONLY_SHAPE {
            if self.shape == RECT_SHAPE {
                gb.skip(1);
                let width = gb.get(13);
                gb.skip(1);
                let height = gb.get(13);
                gb.skip(1);
                if width != 0 && height != 0 {
                    (self.width, self.height) = (width, height);
                }
            }
            gb.skip(1); // interlaced
            gb.skip(1); // obmc disable
            self.vol_sprite_usage = if vo_ver_id == 1 { gb.get(1) } else { gb.get(2) };
            if self.vol_sprite_usage == STATIC_SPRITE || self.vol_sprite_usage == GMC_SPRITE {
                if self.vol_sprite_usage == STATIC_SPRITE {
                    gb.skip(4 * 14); // sprite width, height, left, top and markers
                }
                if gb.get(6) > 3 {
                    return Err(Invalid); // sprite_warping_points
                }
                gb.skip(2 + 1); // accuracy, brightness change
                if self.vol_sprite_usage == STATIC_SPRITE {
                    gb.skip(1); // low_latency_sprite
                }
            }
            if gb.bit() {
                gb.skip(4 + 4); // not_8_bit: quant_precision, bits_per_pixel
            }
            if gb.bit() {
                // mpeg_quant: custom intra, then non-intra matrices
                for _ in 0..2 {
                    if gb.bit() {
                        for _ in 0..64 {
                            if gb.left() < 8 {
                                return Err(Invalid);
                            }
                            if gb.get(8) == 0 {
                                break;
                            }
                        }
                    }
                }
            }
            if vo_ver_id != 1 {
                gb.skip(1); // quarter_sample
            }
            if gb.left() < 4 {
                return Err(Invalid); // VOL header truncated
            }
        }
        Ok(())
    }

    /// decode_studio_vol_header.
    fn decode_studio_vol_header(&mut self, gb: &mut Bits) -> std::result::Result<(), Invalid> {
        gb.skip(4); // video_object_layer_verid
        self.shape = gb.get(2);
        gb.skip(4 + 1); // shape extension, progressive_sequence
        if self.shape != RECT_SHAPE {
            return Err(Invalid);
        }
        let rgb = gb.bit();
        let chroma_format = gb.get(2);
        if chroma_format == 0 || chroma_format == CHROMA_420 || (rgb && chroma_format == CHROMA_422) {
            return Err(Invalid);
        }
        let depth = gb.get(4);
        if depth != 10 {
            return Err(Invalid);
        }
        self.bits_per_raw_sample = depth;
        gb.skip(1);
        let width = gb.get(14);
        gb.skip(1);
        let height = gb.get(14);
        gb.skip(1);
        if width != 0 && height != 0 {
            (self.width, self.height) = (width, height);
        }
        if gb.get(4) == ASPECT_EXTENDED {
            gb.skip(16);
        }
        // frame_rate_code, bit rates and buffer sizes with their markers
        gb.skip(4 + 16 + 16 + 16 + 3 + 12 + 16);
        self.low_delay = gb.bit();
        gb.skip(1); // mpeg2_stream
        Ok(())
    }

    /// decode_vop_header up to the VOP time (parse_only): the picture
    /// type, FFmpeg's guess at time_increment_bits when the VOL's does not
    /// fit, and the time.
    fn decode_vop_header(&mut self, gb: &mut Bits) -> std::result::Result<(), Invalid> {
        self.pict_type = gb.get(2) as u8 + PICT_I;
        if self.pict_type == PICT_B && self.low_delay && !self.vol_control_parameters {
            // "low_delay flag set incorrectly, clearing it"
            self.low_delay = false;
        }
        let mut time_incr = 0i64;
        while gb.bit() {
            time_incr += 1;
        }
        gb.skip(1); // marker
        if self.time_increment_bits == 0 || gb.show(self.time_increment_bits + 1) & 1 == 0 {
            // time_increment_bits set "based on bitstream analysis"
            self.time_increment_bits = 1;
            while self.time_increment_bits < 16 {
                let bits = self.time_increment_bits;
                if self.pict_type == PICT_P || (self.pict_type == PICT_S && self.vol_sprite_usage == GMC_SPRITE) {
                    if gb.show(bits + 6) & 0x37 == 0x30 {
                        break;
                    }
                } else if gb.show(bits + 5) & 0x1F == 0x18 {
                    break;
                }
                self.time_increment_bits += 1;
            }
        }
        let time_increment = i64::from(gb.get(self.time_increment_bits));
        let rate = self.framerate.0;
        if self.pict_type != PICT_B {
            self.last_time_base = self.time_base;
            self.time_base = self.time_base.wrapping_add(time_incr);
            self.time = self.time_base.wrapping_mul(rate).wrapping_add(time_increment);
            self.last_non_b_time = self.time;
        } else {
            self.time = self.last_time_base.wrapping_add(time_incr).wrapping_mul(rate).wrapping_add(time_increment);
        }
        Ok(())
    }

    /// decode_studio_vop_header up to the picture type.
    fn decode_studio_vop_header(&mut self, gb: &mut Bits) {
        if gb.left() <= 32 {
            return;
        }
        gb.skip(4 * 17 + 4); // SMPTE time code with markers, reserved bits
        gb.skip(10 + 2); // temporal_reference, vop_structure
        self.pict_type = gb.get(2) as u8 + PICT_I;
    }
}

/// mpeg4video_parser.c: a VOP with the headers before it, with what the
/// parser reads from it and the demuxer layer's timing.
pub(crate) struct Mpeg4Video {
    pc: Combine,
    frame_start_found: bool,
    headers: Headers,
    /// sti->cur_dts, last_IP_pts and last_IP_duration
    cur_dts: i64,
    last_ip_pts: Option<i64>,
    last_ip_duration: i64,
}

impl Default for Mpeg4Video {
    fn default() -> Self {
        Self {
            pc: Combine::default(),
            frame_start_found: false,
            headers: Headers::default(),
            cur_dts: RELATIVE_TS_BASE,
            last_ip_pts: None,
            last_ip_duration: 0,
        }
    }
}

impl Mpeg4Video {
    /// mpeg4_find_frame_end.
    fn find_frame_end(&mut self, buf: &[u8]) -> isize {
        let mut vop_found = self.frame_start_found;
        let mut state = self.pc.state;
        let mut i = 0;
        if !vop_found {
            while i < buf.len() {
                state = (state << 8) | u32::from(buf[i]);
                i += 1;
                if state == VOP_STARTCODE {
                    vop_found = true;
                    break;
                }
            }
        }
        if vop_found {
            // EOF ends the frame.
            if buf.is_empty() {
                return 0;
            }
            while i < buf.len() {
                state = (state << 8) | u32::from(buf[i]);
                if state & 0xFFFF_FF00 == 0x100 && state != SLICE_STARTCODE && state != EXT_STARTCODE {
                    self.frame_start_found = false;
                    self.pc.state = u32::MAX;
                    return i as isize - 3;
                }
                i += 1;
            }
        }
        self.frame_start_found = vop_found;
        self.pc.state = state;
        END_NOT_FOUND
    }

    /// compute_frame_duration: a frame of the codec frame rate in seconds
    /// (num, den), the raw demuxer's 25 fps before any VOL
    /// (AVFMT_NOTIMESTAMPS), none (0, 1) for a resolution above a
    /// thousand times its increment.
    fn frame_duration(&self) -> (i64, i64) {
        let (num, den) = self.headers.framerate;
        if num == 0 {
            (1, 25)
        } else if den.saturating_mul(1000) <= num {
            (0, 1)
        } else {
            (den, num)
        }
    }

    /// compute_pkt_fields for a unit whose parse set `pts` (or none),
    /// lasting `frame` seconds, `duration` in 1/1200000.
    fn stamp(&mut self, pts: Option<i64>, frame: (i64, i64), duration: i64) -> (Option<i64>, Option<i64>) {
        // The parser sets avctx->has_b_frames = !low_delay.
        let delay = !self.headers.low_delay;
        let known = |t: i64| t <= RELATIVE_TS_BASE - (1 << 48);
        let (pts, dts) = if delay && self.headers.pict_type != PICT_B {
            // Presentation delayed: the dts is the previous I/P-VOP's pts.
            // At the start FFmpeg anchors the relative clock to the next
            // unit's dts, the pts of this one: a frame before it.
            let dts = match (self.last_ip_pts, pts) {
                (Some(d), _) => d,
                (None, Some(p)) if !known(self.cur_dts) => p - duration,
                (None, _) => self.cur_dts,
            };
            if self.last_ip_duration == 0 {
                self.last_ip_duration = duration;
            }
            self.cur_dts = dts.saturating_add(self.last_ip_duration);
            self.last_ip_duration = duration;
            self.last_ip_pts = pts;
            (pts, Some(dts))
        } else if pts.is_some() || duration > 0 {
            // Not delayed: the dts is the pts.
            let pts = pts.unwrap_or(self.cur_dts);
            self.cur_dts = add_stable(pts, frame.0, frame.1);
            (Some(pts), Some(pts))
        } else {
            (None, None)
        };
        if let Some(d) = dts {
            self.cur_dts = self.cur_dts.max(d);
        }
        (pts.map(returned), dts.map(returned))
    }
}

impl Split for Mpeg4Video {
    fn parse(&mut self, buf: &[u8]) -> (isize, Option<Vec<u8>>) {
        let next = self.find_frame_end(buf);
        match self.pc.combine(next, buf) {
            Some(unit) => (next, Some(unit)),
            None => (buf.len() as isize, None),
        }
    }
}

impl Units for Mpeg4Video {
    fn buffered_bytes(&self) -> usize {
        self.pc.buffered_bytes()
    }

    /// Key when the parse leaves an I-VOP (parse_packet: the parser sets
    /// no key_frame); the pts is the VOP time when the parse succeeds and
    /// a VOL gave the time resolution.
    fn unit(&mut self, unit: &Unit, _index: i64) -> Stamp {
        let parsed = self.headers.parse(&unit.data).is_ok();
        let (rate, _) = self.headers.framerate;
        let pts = (parsed && rate > 0).then(|| rescale(self.headers.time, (1, rate), (1, RAW_VIDEO_CLOCK)));
        let frame = self.frame_duration();
        // av_rescale_rnd(1, num * tb.den, den * tb.num, AV_ROUND_DOWN)
        let duration = i64::try_from(i128::from(frame.0) * i128::from(RAW_VIDEO_CLOCK) / i128::from(frame.1)).unwrap_or(i64::MAX);
        let (pts, dts) = self.stamp(pts, frame, duration);
        Stamp::new(self.headers.pict_type == PICT_I, pts, dts, (duration > 0).then_some(duration))
    }

    /// After a seek FFmpeg's new parser starts a new MPEG-4 context (no VOL
    /// fields, time or picture type) over the same codec context (its
    /// frame rate and profile); the stream keeps its last I/P duration and
    /// the clock runs from `ts`.
    fn reset(&self, ts: Option<i64>) -> Option<Self> {
        let headers = Headers { framerate: self.headers.framerate, profile: self.headers.profile, ..Headers::default() };
        Some(Self {
            pc: Combine::default(),
            frame_start_found: false,
            headers,
            cur_dts: ts.unwrap_or(RELATIVE_TS_BASE),
            last_ip_pts: None,
            last_ip_duration: self.last_ip_duration,
        })
    }
}

/// ff_raw_video_read_header: one MPEG-4 stream in 1/1200000, its size
/// from the first VOL the first 64 KiB hold.
pub fn open_m4v(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut head = Vec::new();
    (&mut input).take(64 * 1024).read_to_end(&mut head)?;
    input.seek(SeekFrom::Start(0))?;
    let mut params = CodecParameters::video(CodecId::new("mpeg4"));
    if let Some(at) = head.windows(4).position(|w| w[..3] == [0, 0, 1] && (0x20..=0x2F).contains(&w[3])) {
        let mut headers = Headers::default();
        let mut gb = Bits::new(&head[at + 4..]);
        if headers.decode_vol_header(&mut gb).is_ok() && headers.width > 0 {
            (params.width, params.height) = (Some(headers.width), Some(headers.height));
        }
    }
    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::new(1, RAW_VIDEO_CLOCK),
        duration: None,
        start_time: Some(0),
    };
    Ok(Box::new(RawVideoDemuxer::new("m4v", input, stream, Mpeg4Video::default())))
}

/// FFmpeg's m4v demuxer; `.m4v` names it as in FFmpeg (an MP4 file with
/// that name still opens as MP4, its content probe scoring higher).
pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("m4v", open_m4v);
    reg.register_probe("m4v", probe_m4v);
    reg.register_extension_with_priority("m4v", "m4v", oxideav_core::DEFAULT_PRIORITY - 1);
}
