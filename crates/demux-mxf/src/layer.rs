// Ported from FFmpeg (commit 2da55bf): libavformat/demux.c (parse_packet,
// compute_pkt_fields, update_initial_timestamps, select_from_pts_buffer,
// compute_frame_duration); libavcodec/parser.c (av_parser_parse2,
// ff_fetch_timestamp, ff_combine_frame); mpeg12.c (ff_mpeg1_find_frame_end);
// mpegvideo_parser.c (mpegvideo_extract_headers); mpeg4video_parser.c
// (mpeg4_find_frame_end) with mpeg4videodec.c (decode_vol_header,
// decode_vop_header); h264_parser.c (parse_nal_units) with h264_ps.c and
// h264_sei.c; hevc/parser.c; utils.c (avpriv_find_start_code);
// libavutil/mathematics.c (av_add_stable) and rational.c (av_reduce).
// License: LGPL-2.1-or-later

//! What FFmpeg's demuxer layer makes of the MXF demuxer's packets before
//! av_read_frame returns them: the key flag its parsers give, the dts
//! and durations compute_pkt_fields fills in, and, for MPEG-1/2 and
//! MPEG-4 video FFmpeg parses in full or for timestamps, the frames its
//! parser cuts. Other parsers are modelled only for what they report;
//! FFmpeg's re-framing of MPEG audio and AAC essence is not.

use oxideav_core::MediaType;

use crate::index::{rescale, Q};
use crate::structure::Parsing;

const NOPTS: i64 = i64::MIN;
const RELATIVE_TS_BASE: i64 = i64::MAX - (1 << 48);
const MAX_REORDER_DELAY: usize = 16;

fn is_relative(ts: i64) -> bool {
    ts > RELATIVE_TS_BASE - (1 << 48)
}

/// AVPictureType as the parsers set it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pict {
    None,
    I,
    P,
    B,
    S,
    Si,
    Sp,
}

/// ff_is_intra_only for the codecs MXF maps (codec_desc.c).
fn intra_only(codec: &str) -> bool {
    codec.starts_with("pcm_")
        || matches!(
            codec,
            "mjpeg" | "rawvideo" | "dvvideo" | "jpeg2000" | "tiff" | "dnxhd" | "v210" | "prores" | "hqx" | "hq_hqa" | "avui"
                | "dnxuc" | "mp2" | "dolby_e"
        )
}

/// The codecs FFmpeg has a parser for, among those MXF maps.
fn has_parser(codec: &str) -> bool {
    matches!(
        codec,
        "mpeg1video" | "mpeg2video" | "mpeg4" | "h264" | "hevc" | "vc1" | "dirac" | "dnxhd" | "jpeg2000" | "mjpeg"
            | "prores" | "ffv1" | "mp2" | "aac" | "dolby_e"
    )
}

#[derive(Default)]
struct H264Params {
    /// max_num_ref_frames per SPS id.
    sps_refs: Vec<Option<u32>>,
    /// (SPS id, num_ref_idx_l0_default_active) per PPS id.
    pps: Vec<Option<(usize, u32)>>,
}

/// AV_PARSER_PTS_NB.
const PTS_NB: usize = 4;
/// END_NOT_FOUND.
const END_NOT_FOUND: i64 = -100;
/// AV_INPUT_BUFFER_PADDING_SIZE.
const PADDING: i64 = 64;

/// ParseContext: the bytes of a frame being assembled.
#[derive(Default)]
struct ParseContext {
    buffer: Vec<u8>,
    index: usize,
    last_index: usize,
    state: u32,
    frame_start_found: i32,
    overread: usize,
    overread_index: usize,
}

/// The parser of one stream: FFmpeg's AVCodecParserContext, its frame
/// assembly and timestamp bookkeeping, and what its codec parser finds.
struct Parser {
    codec: &'static str,
    /// PARSER_FLAG_COMPLETE_FRAMES (AVSTREAM_PARSE_HEADERS): packets are
    /// whole frames.
    complete_frames: bool,
    /// AVSTREAM_PARSE_TIMESTAMPS: frame timestamps move by their offset.
    timestamps: bool,
    /// AVCodecParserContext.pict_type (av_parser_init sets I, the MPEG
    /// video parser's init NONE).
    pict_type: Pict,
    /// AVCodecParserContext.key_frame (-1 unknown).
    key_frame: i32,
    h264: H264Params,
    /// mpeg4video_parser: the extradata, until the first picture parses it.
    mpeg4_extradata: Option<Vec<u8>>,
    pc: ParseContext,
    cur_frame_start_index: usize,
    cur_frame_offset: [i64; PTS_NB],
    cur_frame_end: [i64; PTS_NB],
    cur_frame_pts: [i64; PTS_NB],
    cur_frame_dts: [i64; PTS_NB],
    cur_offset: i64,
    next_frame_offset: i64,
    frame_offset: i64,
    fetched_offset: bool,
    fetch_timestamp: bool,
    pts: i64,
    dts: i64,
    offset: i64,
}

/// A packet as av_read_frame returns it.
pub struct Out {
    pub data: Vec<u8>,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: Option<i64>,
    pub key: bool,
}

/// One stream's state in the demuxer layer.
pub struct StreamLayer {
    media: MediaType,
    codec: &'static str,
    time_base: Q,
    /// Frame rate for a video packet's duration.
    frame_rate: Q,
    sample_rate: i32,
    channels: i32,
    parser: Option<Parser>,
    has_b_frames: i32,
    cur_dts: i64,
    first_dts: i64,
    last_ip_pts: i64,
    last_ip_duration: i64,
    pts_buffer: [i64; MAX_REORDER_DELAY + 1],
    pts_reorder_error: [i64; MAX_REORDER_DELAY + 1],
    pts_reorder_error_count: [u8; MAX_REORDER_DELAY + 1],
    last_dts_for_order_check: i64,
    dts_ordered: u32,
    dts_misordered: u32,
}

impl StreamLayer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        media: MediaType,
        codec: &'static str,
        need_parsing: Parsing,
        time_base: Q,
        r_frame_rate: Option<Q>,
        sample_rate: i32,
        channels: i32,
        extradata: Option<&[u8]>,
    ) -> Self {
        // The parsers modelled: frame assembly for MPEG-1/2 and MPEG-4 video
        // (AVSTREAM_PARSE_TIMESTAMPS/FULL); other parsed codecs get their
        // packets whole, as with AVSTREAM_PARSE_HEADERS.
        let parser = (need_parsing != Parsing::None && has_parser(codec)).then(|| {
            let split = need_parsing != Parsing::Headers && matches!(codec, "mpeg1video" | "mpeg2video" | "mpeg4");
            Parser::new(codec, extradata, !split, need_parsing == Parsing::Timestamps)
        });
        Self {
            media,
            codec,
            time_base,
            frame_rate: r_frame_rate.unwrap_or((time_base.1, time_base.0)),
            sample_rate,
            channels,
            parser,
            has_b_frames: 0,
            cur_dts: RELATIVE_TS_BASE,
            first_dts: NOPTS,
            last_ip_pts: NOPTS,
            last_ip_duration: 0,
            pts_buffer: [NOPTS; MAX_REORDER_DELAY + 1],
            pts_reorder_error: [0; MAX_REORDER_DELAY + 1],
            pts_reorder_error_count: [0; MAX_REORDER_DELAY + 1],
            last_dts_for_order_check: NOPTS,
            dts_ordered: 0,
            dts_misordered: 0,
        }
    }

    /// ff_read_frame_flush, then avpriv_update_cur_dts: a new parser, the
    /// interpolation state cleared, the dts origin at `cur_dts`.
    pub fn flush(&mut self, cur_dts: i64, extradata: Option<&[u8]>) {
        if let Some(p) = &mut self.parser {
            *p = Parser::new(p.codec, extradata, p.complete_frames, p.timestamps);
        }
        self.last_ip_pts = NOPTS;
        self.last_dts_for_order_check = NOPTS;
        self.pts_buffer = [NOPTS; MAX_REORDER_DELAY + 1];
        self.cur_dts = cur_dts;
    }

    /// The packets av_read_frame returns for one packet of the MXF
    /// demuxer (pts, dts and duration as mxf_set_pts gave them): the
    /// packet itself, or the frames the stream's parser cuts from it.
    pub fn packets(&mut self, data: Vec<u8>, pts: Option<i64>, dts: Option<i64>, duration: Option<i64>, pos: i64) -> Vec<Out> {
        let mut pts = pts.unwrap_or(NOPTS);
        let mut dts = dts.unwrap_or(NOPTS);
        let duration = duration.unwrap_or(0);
        if self.parser.is_none() {
            let mut d = duration;
            self.compute_pkt_fields(data.len(), &mut pts, &mut dts, &mut d, NOPTS, NOPTS, None);
            let key = self.media == MediaType::Data || intra_only(self.codec);
            return vec![self.out(data, pts, dts, d, key)];
        }
        self.parse_packet(&data, pts, dts, duration, pos, false)
    }

    /// The end of the input: the frames the parser still holds
    /// (parse_packet with flush), its state closed.
    pub fn finish(&mut self) -> Vec<Out> {
        if self.parser.as_ref().is_none_or(|p| p.complete_frames) {
            return Vec::new();
        }
        let out = self.parse_packet(&[], NOPTS, NOPTS, 0, -1, true);
        if let Some(p) = &mut self.parser {
            *p = Parser::new(p.codec, None, p.complete_frames, p.timestamps);
        }
        out
    }

    /// parse_packet.
    fn parse_packet(&mut self, data: &[u8], pts: i64, dts: i64, duration: i64, pos: i64, flush: bool) -> Vec<Out> {
        let mut outs = Vec::new();
        let (mut pts, mut dts, mut pos) = (pts, dts, pos);
        let mut at = 0usize;
        let mut got_output = flush;
        while at < data.len() || (flush && got_output) {
            let (next_pts, next_dts) = (pts, dts);
            let Some(p) = self.parser.as_mut() else { break };
            let (len, frame) = p.parse2(&data[at..], pts, dts, pos, &mut self.has_b_frames);
            pts = NOPTS;
            dts = NOPTS;
            pos = -1;
            at += len;
            let Some(frame) = frame.filter(|f| !f.is_empty()) else {
                got_output = false;
                continue;
            };
            got_output = true;
            let (mut fpts, mut fdts) = (p.pts, p.dts);
            let mut fduration = if p.complete_frames { duration } else { 0 };
            let key = p.key_frame == 1 || (p.key_frame == -1 && p.pict_type == Pict::I);
            let pc_offset = p.timestamps.then_some(p.offset);
            self.compute_pkt_fields(frame.len(), &mut fpts, &mut fdts, &mut fduration, next_dts, next_pts, pc_offset);
            let key = key || self.media == MediaType::Data || intra_only(self.codec);
            outs.push(self.out(frame, fpts, fdts, fduration, key));
        }
        outs
    }

    fn out(&self, data: Vec<u8>, pts: i64, dts: i64, duration: i64, key: bool) -> Out {
        // av_read_frame: relative timestamps come out from 0.
        let ts = |ts: i64| match ts {
            NOPTS => None,
            ts if is_relative(ts) => Some(ts - RELATIVE_TS_BASE),
            ts => Some(ts),
        };
        Out { data, pts: ts(pts), dts: ts(dts), duration: (duration > 0).then_some(duration), key }
    }

    /// compute_frame_duration: (num, den) seconds, (0, 0) when unknown.
    fn frame_duration(&self, size: usize) -> (i64, i64) {
        match self.media {
            MediaType::Video => (i64::from(self.frame_rate.1), i64::from(self.frame_rate.0)),
            MediaType::Audio => {
                // av_get_audio_frame_duration2, PCM.
                let bps = crate::structure::bits_per_sample(self.codec);
                if !self.codec.starts_with("pcm_") || bps <= 0 || self.channels <= 0 || self.sample_rate <= 0 {
                    return (0, 0);
                }
                let frame_size = size as i64 / (i64::from(self.channels) * i64::from(bps) / 8).max(1);
                if frame_size <= 0 {
                    return (0, 0);
                }
                (frame_size, i64::from(self.sample_rate))
            }
            _ => (0, 0),
        }
    }

    fn update_initial_timestamps(&mut self, dts: i64) {
        if self.first_dts != NOPTS || dts == NOPTS || self.cur_dts == NOPTS || self.cur_dts < i64::from(i32::MIN) + RELATIVE_TS_BASE || is_relative(dts) {
            return;
        }
        self.first_dts = dts - (self.cur_dts - RELATIVE_TS_BASE);
        self.cur_dts = dts;
    }

    fn select_from_pts_buffer(&mut self, dts: i64, onein_oneout: bool) -> i64 {
        let mut dts = dts;
        if !onein_oneout {
            let delay = (self.has_b_frames.max(0) as usize).min(MAX_REORDER_DELAY);
            if dts == NOPTS {
                let mut best_score = i64::MAX;
                for i in 0..delay {
                    if self.pts_reorder_error_count[i] != 0 {
                        let score = self.pts_reorder_error[i] / i64::from(self.pts_reorder_error_count[i]);
                        if score < best_score {
                            best_score = score;
                            dts = self.pts_buffer[i];
                        }
                    }
                }
            } else {
                for i in 0..delay {
                    if self.pts_buffer[i] != NOPTS {
                        let diff = (self.pts_buffer[i].wrapping_sub(dts)).wrapping_abs().wrapping_add(self.pts_reorder_error[i]);
                        let diff = diff.max(self.pts_reorder_error[i]);
                        self.pts_reorder_error[i] = diff;
                        self.pts_reorder_error_count[i] = self.pts_reorder_error_count[i].saturating_add(1);
                        if self.pts_reorder_error_count[i] > 250 {
                            self.pts_reorder_error[i] >>= 1;
                            self.pts_reorder_error_count[i] >>= 1;
                        }
                    }
                }
            }
        }
        if dts == NOPTS {
            dts = self.pts_buffer[0];
        }
        dts
    }

    /// compute_pkt_fields. `next_dts`/`next_pts` are the timestamps of the
    /// packet the frame was parsed from, `pc_offset` the frame's offset in
    /// it where the stream is parsed for timestamps.
    #[allow(clippy::too_many_arguments)]
    fn compute_pkt_fields(
        &mut self,
        size: usize,
        pts: &mut i64,
        dts: &mut i64,
        duration: &mut i64,
        next_dts: i64,
        next_pts: i64,
        pc_offset: Option<i64>,
    ) {
        let onein_oneout = !matches!(self.codec, "h264" | "hevc" | "vvc");
        let pc = self.parser.as_ref().map(|p| p.pict_type);
        if self.media == MediaType::Video && *dts != NOPTS {
            if *dts == *pts && self.last_dts_for_order_check != NOPTS {
                if self.last_dts_for_order_check <= *dts {
                    self.dts_ordered += 1;
                } else {
                    self.dts_misordered += 1;
                }
                if self.dts_ordered + self.dts_misordered > 250 {
                    self.dts_ordered >>= 1;
                    self.dts_misordered >>= 1;
                }
            }
            self.last_dts_for_order_check = *dts;
            if self.dts_ordered < 8 * self.dts_misordered && *dts == *pts {
                *dts = NOPTS;
            }
        }
        if pc == Some(Pict::B) && self.has_b_frames == 0 {
            self.has_b_frames = 1;
        }
        let delay = self.has_b_frames;
        let mut presentation_delayed = delay != 0 && pc.is_some_and(|t| t != Pict::B);
        // Some MPEG-2 in MPEG-PS lacks dts: both are discarded.
        if delay == 1 && *dts == *pts && *dts != NOPTS && presentation_delayed {
            *dts = NOPTS;
        }
        // The duration as a rational number of seconds.
        let mut duration_q = reduce(duration.saturating_mul(i64::from(self.time_base.0)), i64::from(self.time_base.1));
        if *duration <= 0 {
            let (num, den) = self.frame_duration(size);
            if num != 0 && den != 0 {
                duration_q = (num, den);
                *duration = duration_ticks(num, den, self.time_base);
            }
        }
        // Correct timestamps by the byte offset where the demuxer only has
        // them on packet boundaries.
        if let Some(offset) = pc_offset.filter(|_| size > 0) {
            let offset = rescale(offset, *duration, size as i64);
            if *pts != NOPTS {
                *pts = pts.wrapping_add(offset);
            }
            if *dts != NOPTS {
                *dts = dts.wrapping_add(offset);
            }
        }
        if *dts != NOPTS && *pts != NOPTS && *pts > *dts {
            presentation_delayed = true;
        }
        // Interpolate PTS and DTS where missing; H.264 is skipped as its
        // delay and has_b_frames are not reliable.
        if (delay == 0 || (delay == 1 && pc.is_some())) && onein_oneout {
            if presentation_delayed {
                if *dts == NOPTS {
                    *dts = self.last_ip_pts;
                }
                self.update_initial_timestamps(*dts);
                if *dts == NOPTS {
                    *dts = self.cur_dts;
                }
                // The dts advances by the duration of the frame displayed,
                // the last I- or P-frame.
                if self.last_ip_duration == 0 && (*duration as u64) <= i32::MAX as u64 {
                    self.last_ip_duration = *duration;
                }
                if *dts != NOPTS {
                    self.cur_dts = dts.saturating_add(self.last_ip_duration);
                }
                if *dts != NOPTS
                    && *pts == NOPTS
                    && self.last_ip_duration > 0
                    && (self.cur_dts as u64).wrapping_sub(next_dts as u64).wrapping_add(1) <= 2
                    && next_dts != next_pts
                    && next_pts != NOPTS
                {
                    *pts = next_dts;
                }
                if (*duration as u64) <= i32::MAX as u64 {
                    self.last_ip_duration = *duration;
                }
                self.last_ip_pts = *pts;
            } else if *pts != NOPTS || *dts != NOPTS || *duration > 0 {
                // Presentation is not delayed: PTS and DTS are the same.
                if *pts == NOPTS {
                    *pts = *dts;
                }
                self.update_initial_timestamps(*pts);
                if *pts == NOPTS {
                    *pts = self.cur_dts;
                }
                *dts = *pts;
                if *pts != NOPTS && duration_q.0 >= 0 {
                    self.cur_dts = add_stable(self.time_base, *pts, duration_q);
                }
            }
        }
        if *pts != NOPTS && delay >= 0 && delay as usize <= MAX_REORDER_DELAY {
            self.pts_buffer[0] = *pts;
            let mut i = 0;
            while i < delay as usize && self.pts_buffer[i] > self.pts_buffer[i + 1] {
                self.pts_buffer.swap(i, i + 1);
                i += 1;
            }
            // has_decode_delay_been_guessed: always, past find_stream_info.
            *dts = self.select_from_pts_buffer(*dts, onein_oneout);
        }
        if !onein_oneout {
            self.update_initial_timestamps(*dts);
        }
        if *dts > self.cur_dts {
            self.cur_dts = *dts;
        }
    }
}

/// av_reduce of num/den (den > 0 assumed where it matters).
fn reduce(num: i64, den: i64) -> (i64, i64) {
    fn gcd(a: i64, b: i64) -> i64 {
        if b == 0 { a.abs() } else { gcd(b, a % b) }
    }
    let g = gcd(num, den);
    if g == 0 { (num, den) } else { (num / g, den / g) }
}

/// av_add_stable(ts_tb, ts, inc_tb, 1).
fn add_stable(ts_tb: Q, ts: i64, inc_tb: (i64, i64)) -> i64 {
    let m = i128::from(inc_tb.0) * i128::from(ts_tb.1);
    let d = i128::from(inc_tb.1) * i128::from(ts_tb.0);
    if d == 0 {
        return ts;
    }
    if m % d == 0 && i128::from(ts) <= i128::from(i64::MAX) - m / d {
        return (i128::from(ts) + m / d) as i64;
    }
    if m < d {
        return ts;
    }
    let to_inc = |v: i64| rescale(v, i64::from(ts_tb.0).saturating_mul(inc_tb.1), inc_tb.0.saturating_mul(i64::from(ts_tb.1)));
    let to_ts = |v: i64| rescale(v, inc_tb.0.saturating_mul(i64::from(ts_tb.1)), i64::from(ts_tb.0).saturating_mul(inc_tb.1));
    let old = to_inc(ts);
    let old_ts = to_ts(old);
    if old == i64::MAX || old == NOPTS || old_ts == NOPTS {
        return ts;
    }
    to_ts(old + 1).saturating_add(ts - old_ts)
}

impl Parser {
    /// av_parser_init (pict_type I, key_frame -1) with the codec parser's
    /// init (the MPEG video parser starts at no picture type).
    fn new(codec: &'static str, extradata: Option<&[u8]>, complete_frames: bool, timestamps: bool) -> Self {
        Parser {
            codec,
            complete_frames,
            timestamps,
            pict_type: if matches!(codec, "mpeg1video" | "mpeg2video") { Pict::None } else { Pict::I },
            key_frame: -1,
            h264: H264Params::default(),
            mpeg4_extradata: if codec == "mpeg4" { extradata.map(<[u8]>::to_vec) } else { None },
            pc: ParseContext::default(),
            cur_frame_start_index: 0,
            cur_frame_offset: [0; PTS_NB],
            cur_frame_end: [0; PTS_NB],
            cur_frame_pts: [0; PTS_NB],
            cur_frame_dts: [0; PTS_NB],
            cur_offset: 0,
            next_frame_offset: 0,
            frame_offset: 0,
            fetched_offset: false,
            fetch_timestamp: true,
            pts: NOPTS,
            dts: NOPTS,
            offset: 0,
        }
    }

    /// av_parser_parse2: the bytes of `buf` consumed and the frame
    /// completed, timed in self.pts/self.dts.
    fn parse2(&mut self, buf: &[u8], pts: i64, dts: i64, pos: i64, has_b_frames: &mut i32) -> (usize, Option<Vec<u8>>) {
        if !self.fetched_offset {
            self.next_frame_offset = pos;
            self.cur_offset = pos;
            self.fetched_offset = true;
        }
        let size = buf.len() as i64;
        if size > 0 && self.cur_offset + size != self.cur_frame_end[self.cur_frame_start_index] {
            // A new packet descriptor.
            let i = (self.cur_frame_start_index + 1) & (PTS_NB - 1);
            self.cur_frame_start_index = i;
            self.cur_frame_offset[i] = self.cur_offset;
            self.cur_frame_end[i] = self.cur_offset + size;
            self.cur_frame_pts[i] = pts;
            self.cur_frame_dts[i] = dts;
        }
        if self.fetch_timestamp {
            self.fetch_timestamp = false;
            self.fetch(0, false, false);
        }
        let (index, frame) = self.codec_parse(buf, has_b_frames);
        if frame.as_ref().is_some_and(|f| !f.is_empty()) {
            // The data of the frame just completed.
            self.frame_offset = self.next_frame_offset;
            // Where the next frame starts.
            self.next_frame_offset = self.cur_offset + index;
            self.fetch_timestamp = true;
        }
        let index = index.max(0);
        self.cur_offset += index;
        (index as usize, frame)
    }

    /// ff_fetch_timestamp.
    fn fetch(&mut self, off: i64, remove: bool, fuzzy: bool) {
        if !fuzzy {
            self.dts = NOPTS;
            self.pts = NOPTS;
            self.offset = 0;
        }
        for i in 0..PTS_NB {
            if self.cur_offset + off >= self.cur_frame_offset[i]
                && (self.frame_offset < self.cur_frame_offset[i] || (self.frame_offset == 0 && self.next_frame_offset == 0))
                && self.cur_frame_end[i] != 0
            {
                if !fuzzy || self.cur_frame_dts[i] != NOPTS {
                    self.dts = self.cur_frame_dts[i];
                    self.pts = self.cur_frame_pts[i];
                    self.offset = self.next_frame_offset - self.cur_frame_offset[i];
                }
                if remove {
                    self.cur_frame_offset[i] = i64::MAX;
                }
                if self.cur_offset + off < self.cur_frame_end[i] {
                    break;
                }
            }
        }
    }

    /// The codec parser: (index, frame) as parser_parse returns them.
    fn codec_parse(&mut self, buf: &[u8], has_b_frames: &mut i32) -> (i64, Option<Vec<u8>>) {
        let (next, frame) = if self.complete_frames {
            (buf.len() as i64, buf.to_vec())
        } else {
            let next = match self.codec {
                "mpeg4" => self.mpeg4_find_frame_end(buf),
                _ => self.mpeg1_find_frame_end(buf),
            };
            match self.pc.combine(next, buf) {
                Some(frame) => (next, frame),
                None => return (buf.len() as i64, None),
            }
        };
        match self.codec {
            "mpeg1video" | "mpeg2video" => self.mpegvideo(&frame, has_b_frames),
            "mpeg4" => self.mpeg4(&frame, has_b_frames),
            "h264" => self.h264(&frame),
            "hevc" => self.hevc(&frame),
            // dnxhd, jpeg2000, mjpeg: key_frame -1, pict_type I; prores and
            // the others keep the defaults too.
            _ => {}
        }
        (next, Some(frame))
    }

    /// mpeg4video_parser mpeg4_find_frame_end.
    fn mpeg4_find_frame_end(&mut self, buf: &[u8]) -> i64 {
        const VOP_STARTCODE: u32 = 0x1B6;
        const SLICE_STARTCODE: u32 = 0x1B7;
        const EXT_STARTCODE: u32 = 0x1B8;
        let mut vop_found = self.pc.frame_start_found != 0;
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
            // EOF considered as the end of the frame.
            if buf.is_empty() {
                return 0;
            }
            while i < buf.len() {
                state = (state << 8) | u32::from(buf[i]);
                if state & 0xFFFF_FF00 == 0x100 && state != SLICE_STARTCODE && state != EXT_STARTCODE {
                    self.pc.frame_start_found = 0;
                    self.pc.state = u32::MAX;
                    return i as i64 - 3;
                }
                i += 1;
            }
        }
        self.pc.frame_start_found = i32::from(vop_found);
        self.pc.state = state;
        END_NOT_FOUND
    }

    /// mpeg12.c ff_mpeg1_find_frame_end.
    fn mpeg1_find_frame_end(&mut self, buf: &[u8]) -> i64 {
        const PICTURE_START_CODE: u32 = 0x100;
        const SLICE_MIN_START_CODE: u32 = 0x101;
        const SLICE_MAX_START_CODE: u32 = 0x1AF;
        const SEQ_END_CODE: u32 = 0x1B7;
        const SEQ_START_CODE: u32 = 0x1B3;
        const EXT_START_CODE: u32 = 0x1B5;
        let mut state = self.pc.state;
        // EOF considered as the end of the frame.
        if buf.is_empty() {
            return 0;
        }
        // 0 frame start -> 1/4, 1 first_SEQEXT -> 0/2, 2 first field
        // start -> 3/0, 3 second_SEQEXT -> 2/0, 4 searching end.
        let mut i: i64 = 0;
        let n = buf.len() as i64;
        while i < n {
            if self.pc.frame_start_found & 1 != 0 {
                let b = buf[i as usize];
                if state == EXT_START_CODE && b & 0xF0 != 0x80 {
                    self.pc.frame_start_found -= 1;
                } else if state == EXT_START_CODE + 2 {
                    if b & 3 == 3 {
                        self.pc.frame_start_found = 0;
                    } else {
                        self.pc.frame_start_found = (self.pc.frame_start_found + 1) & 3;
                    }
                }
                state = state.wrapping_add(1);
            } else {
                i = avpriv_find_start_code(buf, i as usize, &mut state) as i64 - 1;
                if self.pc.frame_start_found == 0 && (SLICE_MIN_START_CODE..=SLICE_MAX_START_CODE).contains(&state) {
                    i += 1;
                    self.pc.frame_start_found = 4;
                }
                if state == SEQ_END_CODE {
                    self.pc.frame_start_found = 0;
                    self.pc.state = u32::MAX;
                    return i + 1;
                }
                if self.pc.frame_start_found == 2 && state == SEQ_START_CODE {
                    self.pc.frame_start_found = 0;
                }
                if self.pc.frame_start_found < 4 && state == EXT_START_CODE {
                    self.pc.frame_start_found += 1;
                }
                if self.pc.frame_start_found == 4
                    && state & 0xFFFF_FF00 == 0x100
                    && !(SLICE_MIN_START_CODE..=SLICE_MAX_START_CODE).contains(&state)
                {
                    self.pc.frame_start_found = 0;
                    self.pc.state = u32::MAX;
                    return i - 3;
                }
                if self.pc.frame_start_found == 0 && state == PICTURE_START_CODE {
                    self.fetch(i - 3, true, i > 3);
                }
            }
            i += 1;
        }
        self.pc.state = state;
        END_NOT_FOUND
    }

    /// mpegvideo_extract_headers: the last picture header's type, the
    /// sequence extension's low_delay.
    fn mpegvideo(&mut self, buf: &[u8], has_b_frames: &mut i32) {
        let mut i = 0;
        while let Some(at) = find_start_code(buf, i) {
            let code = buf[at + 3];
            let rest = &buf[at + 4..];
            i = at + 4;
            match code {
                0x00 if rest.len() >= 2 => self.pict_type = pict_from_mpeg((rest[1] >> 3) & 7),
                0xB5 if rest.len() >= 6 && rest[0] >> 4 == 1 => *has_b_frames = i32::from(rest[5] >> 7 == 0),
                _ => {}
            }
        }
    }

    /// mpeg4_decode_header: av_mpeg4_decode_header over the extradata for
    /// the first picture, then the frame, to its first VOP.
    fn mpeg4(&mut self, buf: &[u8], has_b_frames: &mut i32) {
        if let Some(extra) = self.mpeg4_extradata.take() {
            self.mpeg4_headers(&extra, has_b_frames);
        }
        self.mpeg4_headers(buf, has_b_frames);
    }

    fn mpeg4_headers(&mut self, buf: &[u8], has_b_frames: &mut i32) {
        let mut i = 0;
        while let Some(at) = find_start_code(buf, i) {
            let code = buf[at + 3];
            let rest = &buf[at + 4..];
            i = at + 4;
            if (0x20..=0x2F).contains(&code) {
                if let Some(low_delay) = mpeg4_vol_low_delay(rest) {
                    *has_b_frames = i32::from(!low_delay);
                }
            } else if code == 0xB6 {
                if let Some(&b) = rest.first() {
                    self.pict_type = match b >> 6 {
                        0 => Pict::I,
                        1 => Pict::P,
                        2 => Pict::B,
                        _ => Pict::S,
                    };
                }
                return;
            }
        }
    }

    /// h264_parser parse_nal_units: key for an IDR slice, a recovery point
    /// SEI before the slice, or an I slice of a stream with at most one
    /// reference frame.
    fn h264(&mut self, buf: &[u8]) {
        self.key_frame = 0;
        let mut recovery = false;
        let mut i = 0;
        while let Some(at) = find_start_code(buf, i) {
            let start = at + 3;
            let end = find_start_code(buf, start).map_or(buf.len(), |n| n);
            i = start;
            let Some(&head) = buf.get(start) else { break };
            let rbsp = unescape(&buf[start + 1..end]);
            match head & 0x1F {
                7 => self.h264_sps(&rbsp),
                8 => self.h264_pps(&rbsp),
                6 => recovery |= sei_has_recovery_point(&rbsp),
                5 | 1 | 2 => {
                    if head & 0x1F == 5 {
                        self.key_frame = 1;
                    }
                    let mut r = Bits::new(&rbsp);
                    let _first_mb = r.ue();
                    let Some(slice_type) = r.ue() else { return };
                    self.pict_type = [Pict::P, Pict::B, Pict::I, Pict::Sp, Pict::Si][(slice_type % 5) as usize];
                    if recovery {
                        self.key_frame = 1;
                    }
                    let Some(pps_id) = r.ue() else { return };
                    let Some(Some((sps_id, ref_count0))) = self.h264.pps.get(pps_id as usize).copied() else { return };
                    let Some(Some(ref_frames)) = self.h264.sps_refs.get(sps_id).copied() else { return };
                    if ref_frames <= 1 && ref_count0 <= 1 && self.pict_type == Pict::I {
                        self.key_frame = 1;
                    }
                    return;
                }
                _ => {}
            }
        }
    }

    fn h264_sps(&mut self, rbsp: &[u8]) {
        let mut r = Bits::new(rbsp);
        let Some(profile_idc) = r.bits(8) else { return };
        r.skip(16); // constraint flags, level_idc
        let Some(sps_id) = r.ue().filter(|&id| id < 32) else { return };
        if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
            let Some(chroma_format_idc) = r.ue() else { return };
            if chroma_format_idc == 3 {
                r.skip(1);
            }
            r.ue(); // bit_depth_luma
            r.ue(); // bit_depth_chroma
            r.skip(1); // qpprime_y_zero_transform_bypass
            if r.bits(1) == Some(1) {
                let lists = if chroma_format_idc == 3 { 12 } else { 8 };
                for n in 0..lists {
                    if r.bits(1) == Some(1) {
                        let size = if n < 6 { 16 } else { 64 };
                        let (mut last, mut next) = (8i64, 8i64);
                        for _ in 0..size {
                            if next != 0 {
                                let Some(delta) = r.se() else { return };
                                next = (last + delta + 256) % 256;
                            }
                            last = if next == 0 { last } else { next };
                        }
                    }
                }
            }
        }
        r.ue(); // log2_max_frame_num_minus4
        match r.ue() {
            Some(0) => {
                r.ue(); // log2_max_poc_lsb_minus4
            }
            Some(1) => {
                r.skip(1);
                r.se();
                r.se();
                let Some(n) = r.ue() else { return };
                for _ in 0..n.min(256) {
                    r.se();
                }
            }
            Some(_) => {}
            None => return,
        }
        let Some(max_num_ref_frames) = r.ue() else { return };
        let id = sps_id as usize;
        if self.h264.sps_refs.len() <= id {
            self.h264.sps_refs.resize(id + 1, None);
        }
        self.h264.sps_refs[id] = Some(max_num_ref_frames);
    }

    fn h264_pps(&mut self, rbsp: &[u8]) {
        let mut r = Bits::new(rbsp);
        let Some(pps_id) = r.ue().filter(|&id| id < 256) else { return };
        let Some(sps_id) = r.ue().filter(|&id| id < 32) else { return };
        r.skip(2); // entropy_coding_mode, bottom_field_pic_order_in_frame_present
        let Some(num_slice_groups_minus1) = r.ue() else { return };
        if num_slice_groups_minus1 > 0 {
            let Some(map_type) = r.ue() else { return };
            match map_type {
                0 => {
                    for _ in 0..=num_slice_groups_minus1.min(8) {
                        r.ue();
                    }
                }
                2 => {
                    for _ in 0..num_slice_groups_minus1.min(8) {
                        r.ue();
                        r.ue();
                    }
                }
                3..=5 => {
                    r.skip(1);
                    r.ue();
                }
                6 => {
                    let Some(n) = r.ue() else { return };
                    let bits = 32 - num_slice_groups_minus1.leading_zeros();
                    for _ in 0..=n.min(1 << 16) {
                        r.skip(bits as usize);
                    }
                }
                _ => {}
            }
        }
        let Some(l0) = r.ue() else { return };
        let id = pps_id as usize;
        if self.h264.pps.len() <= id {
            self.h264.pps.resize(id + 1, None);
        }
        self.h264.pps[id] = Some((sps_id as usize, l0 + 1));
    }

    /// hevc_parser: key for an IRAP picture's first slice.
    fn hevc(&mut self, buf: &[u8]) {
        self.key_frame = 0;
        let mut i = 0;
        while let Some(at) = find_start_code(buf, i) {
            i = at + 3;
            let Some(&head) = buf.get(at + 3) else { break };
            let nal_type = (head >> 1) & 0x3F;
            if nal_type <= 21 {
                self.key_frame = i32::from((16..=21).contains(&nal_type));
                return;
            }
        }
    }
}

fn pict_from_mpeg(t: u8) -> Pict {
    match t {
        1 => Pict::I,
        2 => Pict::P,
        3 => Pict::B,
        4 => Pict::S,
        _ => Pict::None,
    }
}

/// avpriv_find_start_code (libavcodec/utils.c): from `p`, the position
/// after the next start code, `state` its last four bytes.
fn avpriv_find_start_code(buf: &[u8], mut p: usize, state: &mut u32) -> usize {
    let end = buf.len();
    if p >= end {
        return end;
    }
    for _ in 0..3 {
        let tmp = *state << 8;
        *state = tmp.wrapping_add(u32::from(buf[p]));
        p += 1;
        if tmp == 0x100 || p == end {
            return p;
        }
    }
    while p < end {
        if buf[p - 1] > 1 {
            p += 3;
        } else if buf[p - 2] != 0 {
            p += 2;
        } else if buf[p - 3] != 0 || buf[p - 1] != 1 {
            p += 1;
        } else {
            p += 1;
            break;
        }
    }
    let p = p.min(end) - 4;
    *state = u32::from_be_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]);
    p + 4
}

/// The largest frame the parsers assemble.
const MAX_ASSEMBLED_FRAME: usize = 64 << 20;

impl ParseContext {
    /// ff_combine_frame: the frame `next` ends (relative to `buf`, maybe
    /// before it), None while it has not ended.
    fn combine(&mut self, mut next: i64, buf: &[u8]) -> Option<Vec<u8>> {
        // Copy the bytes the last frame read past into the buffer.
        while self.overread > 0 {
            let b = self.buffer.get(self.overread_index).copied().unwrap_or(0);
            self.overread_index += 1;
            self.put(self.index, &[b]);
            self.index += 1;
            self.overread -= 1;
        }
        let size = buf.len() as i64;
        if next > size {
            return None;
        }
        // Flush what remains at the end of the input.
        if size == 0 && next == END_NOT_FOUND {
            next = 0;
        }
        self.last_index = self.index;
        if next == END_NOT_FOUND {
            // A frame larger than this is dropped (untrusted input; FFmpeg
            // grows its buffer to INT_MAX).
            if self.index + buf.len() > MAX_ASSEMBLED_FRAME {
                self.index = 0;
                self.last_index = 0;
                return None;
            }
            self.put(self.index, buf);
            self.index += buf.len();
            return None;
        }
        let frame_len = (self.index as i64 + next).max(0) as usize;
        self.overread_index = frame_len;
        let frame = if self.index > 0 {
            // Append to the buffer, the padding too where the input has it.
            if next > -PADDING {
                let take = ((next + PADDING).max(0) as usize).min(buf.len());
                self.put(self.index, &buf[..take]);
            }
            self.index = 0;
            self.buffer[..frame_len.min(self.buffer.len())].to_vec()
        } else {
            buf[..frame_len.min(buf.len())].to_vec()
        };
        if next < -8 {
            self.overread += (-8 - next) as usize;
            next = -8;
        }
        // Store the bytes read past the frame.
        while next < 0 {
            let b = self.buffer.get((self.last_index as i64 + next) as usize).copied().unwrap_or(0);
            self.state = (self.state << 8) | u32::from(b);
            self.overread += 1;
            next += 1;
        }
        Some(frame)
    }

    fn put(&mut self, at: usize, bytes: &[u8]) {
        if self.buffer.len() < at + bytes.len() {
            self.buffer.resize(at + bytes.len(), 0);
        }
        self.buffer[at..at + bytes.len()].copy_from_slice(bytes);
    }
}

/// The position of the next 00 00 01 at or after `from`.
fn find_start_code(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// The RBSP of a NAL unit payload: emulation prevention bytes removed.
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Whether an SEI RBSP carries a recovery point message (h264_sei.c).
fn sei_has_recovery_point(rbsp: &[u8]) -> bool {
    let mut i = 0;
    while i + 2 <= rbsp.len() && rbsp[i] != 0x80 {
        let mut ty = 0usize;
        while i < rbsp.len() && rbsp[i] == 0xFF {
            ty += 255;
            i += 1;
        }
        let Some(&b) = rbsp.get(i) else { return false };
        ty += usize::from(b);
        i += 1;
        let mut size = 0usize;
        while i < rbsp.len() && rbsp[i] == 0xFF {
            size += 255;
            i += 1;
        }
        let Some(&b) = rbsp.get(i) else { return false };
        size += usize::from(b);
        i += 1;
        if ty == 6 {
            return Bits::new(&rbsp[i.min(rbsp.len())..]).ue().is_some();
        }
        i += size;
    }
    false
}

/// decode_vol_header up to low_delay: the flag where the VOL sets it,
/// else the default of its object type (the parser's context never
/// counts a decoded picture, so the default always applies).
fn mpeg4_vol_low_delay(rest: &[u8]) -> Option<bool> {
    let mut r = Bits::new(rest);
    r.skip(1); // random accessible VOL
    let vo_type = r.bits(8)?;
    if r.bits(1)? == 1 {
        r.skip(4 + 3); // vo_ver_id, vo_priority
    }
    if r.bits(4)? == 15 {
        r.skip(16); // extended pixel aspect ratio
    }
    if r.bits(1)? == 1 {
        r.skip(2); // chroma_format
        return Some(r.bits(1)? == 1);
    }
    // SIMPLE_VO_TYPE and ADV_SIMPLE_VO_TYPE.
    Some(matches!(vo_type, 1 | 17))
}

/// An MSB-first bit reader with Exp-Golomb codes.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bits(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(v)
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        Some(((1u64 << zeros) - 1 + u64::from(rest)).min(u64::from(u32::MAX)) as u32)
    }

    fn se(&mut self) -> Option<i64> {
        let v = i64::from(self.ue()?);
        Some(if v & 1 == 1 { (v + 1) / 2 } else { -(v / 2) })
    }
}

/// av_rescale_rnd(1, num * tb_den, den * tb_num, AV_ROUND_DOWN).
fn duration_ticks(num: i64, den: i64, tb: Q) -> i64 {
    let b = num.saturating_mul(i64::from(tb.1));
    let c = den.saturating_mul(i64::from(tb.0));
    if b < 0 || c <= 0 {
        return 0;
    }
    b / c
}
