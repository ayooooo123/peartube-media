// Ported from FFmpeg libavformat/dtsdec.c (raw DTS demuxer + probe) and
// libavformat/dtshddec.c (DTS-HD DTSHDHDR wrapper) (commit 2da55bf).
// Licensed under LGPL-2.1-or-later.

//! The raw `dts` demuxer (FFmpeg's `ff_raw_read_partial_packet` framing
//! plus full frame-length parsing from `dca_parser.c`) and the `dtshd`
//! chunked wrapper demuxer.

use crate::dca::{self, CoreFrameHeader, DCA_CORE_FRAME_HEADER_SIZE};
use oxideav_core::{
    CodecParameters, ContainerRegistry, Demuxer, Error, MediaType, Packet, ProbeData, ProbeScore,
    ReadSeek, Result, SampleFormat, StreamInfo, TimeBase,
};
use std::io::{Read, Seek, SeekFrom};

const RAW_PACKET_SIZE: usize = 1024;

// ───────────────────────── probe (dtsdec.c) ─────────────────────────

/// `dts_probe`. Scores like FFmpeg: `AVPROBE_SCORE_EXTENSION + 1` (26) on
/// a solid marker histogram or EXSS run, 0 otherwise.
pub fn dts_probe(p: &ProbeData) -> ProbeScore {
    let buf = p.buf;
    let mut state: u32 = 0xffff_ffff;
    let mut markers = [0usize; 4 * 16];
    let mut exss_markers = 0usize;
    let mut exss_nextpos = 0usize;
    let mut sum: u64 = 0;
    let mut diff: u64 = 0;
    let mut diffcount: u64 = 1;
    let scan_end = buf.len().saturating_sub(2);

    // FFmpeg starts at FFMIN(4096, size); we scan from 0 (whole head buffer).
    let mut pos = 0usize.min(scan_end);

    while pos + 2 <= scan_end {
        let w = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        state = (state << 16) | u32::from(w);

        if pos >= 4 {
            let v0 = i16::from_le_bytes([buf[pos], buf[pos + 1]]);
            let v1 = i16::from_le_bytes([buf[pos - 4], buf[pos - 4 + 1]]);
            if v0 != 0 || v1 != 0 {
                diff += (v0 as i64 - v1 as i64).unsigned_abs() as u64;
                diffcount += 1;
            }
        }

        // extension substream (EXSS)
        if state == dca::DCA_SYNCWORD_SUBSTREAM {
            if pos >= exss_nextpos {
                // init_get_bits(&gb, buf - 2, 96); skip 42 bits: header
                // fields sit at bit 42 from the sync word.
                if pos + 2 + 12 <= buf.len() {
                    let gb = crate::bitreader::BitReader::new(&buf[pos..]);
                    let mut gb = gb;
                    gb.skip(42);
                    let wide_hdr = gb.get_bits(1);
                    let hdr_size = gb.get_bits(8 + 4 * wide_hdr) as usize + 1;
                    let framesize = gb.get_bits(16 + 4 * wide_hdr) as usize + 1;
                    let hdr_ok = hdr_size % 4 == 0 && framesize % 4 == 0 && hdr_size >= 16 && framesize >= hdr_size;
                    if hdr_ok && pos + hdr_size <= buf.len() {
                        // av_crc(..., 0xffff, buf + 3, hdr_size - 5): the C
                        // reads from `buf` which points at pos (after the
                        // 2-byte advance) — CRC over header bytes 1..
                        let crc_start = pos + 3;
                        let crc_len = hdr_size - 5;
                        if crc_start + crc_len <= buf.len()
                            && crate::crc16::check_crc_strict(buf, crc_start * 8, (crc_start + crc_len) * 8)
                        {
                            if pos == exss_nextpos {
                                exss_markers += 1;
                            } else {
                                exss_markers = exss_markers.saturating_sub(1).max(1);
                            }
                            exss_nextpos = pos + framesize;
                            pos += 2;
                            continue;
                        }
                    }
                }
            }
            pos += 2;
            continue;
        }

        // regular bitstream markers, with the second-word checks the C does
        let marker = if pos + 4 <= buf.len() {
            let w2 = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]);
            if state == dca::DCA_SYNCWORD_CORE_BE && (w2 & 0xFC00) == 0xFC00 {
                Some(0usize)
            } else if state == dca::DCA_SYNCWORD_CORE_LE && (w2 & 0x00FC) == 0x00FC {
                Some(1)
            } else if state == dca::DCA_SYNCWORD_CORE_14B_BE && (w2 & 0xFFF0) == 0x07F0 {
                Some(2)
            } else if state == dca::DCA_SYNCWORD_CORE_14B_LE && (w2 & 0xF0FF) == 0xF007 {
                Some(3)
            } else {
                None
            }
        } else {
            None
        };

        let Some(marker) = marker else {
            pos += 2;
            continue;
        };

        // Convert the 18-byte header to BE and parse it
        let hdr_src = &buf[pos.min(buf.len())..];
        let mut hdr = vec![0u8; DCA_CORE_FRAME_HEADER_SIZE];
        if dca::convert_bitstream(hdr_src, &mut hdr).is_none() {
            pos += 2;
            continue;
        }
        let mut h = CoreFrameHeader::default();
        let mut gb = crate::bitreader::BitReader::new(&hdr);
        if dca::parse_core_frame_header(&mut h, &mut gb).is_err() {
            pos += 2;
            continue;
        }

        let idx = marker + 4 * h.sr_code as usize;
        if idx < markers.len() {
            markers[idx] += 1;
        }
        pos += 2;
    }

    if exss_markers > 3 {
        return oxideav_core::PROBE_SCORE_EXTENSION + 1;
    }

    let mut max = 0usize;
    for (i, &m) in markers.iter().enumerate() {
        sum += m as u64;
        if markers[max] < m {
            max = i;
        }
    }

    if markers[max] > 3
        && buf.len() / markers[max] < 32 * 1024
        && (markers[max] as u64) * 4 > sum * 3 / 1
        && diff / diffcount > 600
        && (markers[max] as u64) * 4 > sum * 3
    {
        return oxideav_core::PROBE_SCORE_EXTENSION + 1;
    }

    0
}

// ───────────────────────── dca_parser.c frame finding ─────────────────────────

const IS_EXSS: u32 = dca::DCA_SYNCWORD_SUBSTREAM;

fn core_marker_of(state: u64) -> u32 {
    (state >> 16) as u32
}

fn is_core_marker(state: u64) -> bool {
    (state & 0xFFFF_FFFF_F0FF) == ((u64::from(dca::DCA_SYNCWORD_CORE_14B_LE) << 16) | 0xF007)
        || (state & 0xFFFF_FFFF_FFF0) == ((u64::from(dca::DCA_SYNCWORD_CORE_14B_BE) << 16) | 0x07F0)
        || (state & 0xFFFF_FFFF_00FC) == ((u64::from(dca::DCA_SYNCWORD_CORE_LE) << 16) | 0x00FC)
        || (state & 0xFFFF_FFFF_FC00) == ((u64::from(dca::DCA_SYNCWORD_CORE_BE) << 16) | 0xFC00)
}

fn is_exss_marker(state: u64) -> bool {
    (state & 0xFFFF_FFFF) == u64::from(IS_EXSS)
}

fn is_marker(state: u64) -> bool {
    is_core_marker(state) || is_exss_marker(state)
}

fn state_le(state: u64) -> u64 {
    ((state & 0xFF00_FF00) >> 8) | ((state & 0x00FF_00FF) << 8)
}

fn state_14(state: u64) -> u64 {
    ((state & 0x3FFF_0000) >> 8) | ((state & 0x0000_3FFF) >> 6)
}

fn core_framesize(state: u64) -> usize {
    (((state >> 4) & 0x3FFF) as usize) + 1
}

fn exss_framesize(state: u64) -> usize {
    if state & 0x20_0000_0000 != 0 {
        ((state >> 5) & 0xFFFFF) as usize + 1
    } else {
        ((state >> 13) & 0x0FFFF) as usize + 1
    }
}

/// `dca_find_frame_end` state.
#[derive(Default)]
struct ParseState {
    frame_start_found: i32,
    state64: u64,
    size: usize,
    framesize: usize,
    lastmarker: u32,
    startpos: usize,
}

impl ParseState {
    /// Returns the position of the first byte of the next frame, or `None`.
    fn find_frame_end(&mut self, buf: &[u8]) -> Option<usize> {
        let mut start_found = self.frame_start_found;
        let mut state = self.state64;
        let mut size = self.size;

        let mut i = 0usize;
        if start_found == 0 {
            while i < buf.len() {
                size += 1;
                state = (state << 8) | u64::from(buf[i]);

                if is_marker(state)
                    && (self.lastmarker == 0
                        || self.lastmarker == core_marker_of(state)
                        || self.lastmarker == dca::DCA_SYNCWORD_SUBSTREAM)
                {
                    if self.lastmarker == 0 {
                        self.startpos = if is_exss_marker(state) { size - 4 } else { size - 6 };
                    }

                    self.lastmarker = if is_exss_marker(state) {
                        (state & 0xFFFF_FFFF) as u32
                    } else {
                        core_marker_of(state)
                    };

                    start_found = 1;
                    size = 0;

                    i += 1;
                    break;
                }
                i += 1;
            }
        }

        if start_found != 0 {
            while i < buf.len() {
                size += 1;
                state = (state << 8) | u64::from(buf[i]);

                if start_found == 1 {
                    match self.lastmarker {
                        m if m == dca::DCA_SYNCWORD_CORE_BE => {
                            if size == 2 {
                                self.framesize = core_framesize(state);
                                start_found = 2;
                            }
                        }
                        m if m == dca::DCA_SYNCWORD_CORE_LE => {
                            if size == 2 {
                                self.framesize = core_framesize(state_le(state));
                                start_found = 4;
                            }
                        }
                        m if m == dca::DCA_SYNCWORD_CORE_14B_BE => {
                            if size == 4 {
                                self.framesize = core_framesize(state_14(state));
                                start_found = 4;
                            }
                        }
                        m if m == dca::DCA_SYNCWORD_CORE_14B_LE => {
                            if size == 4 {
                                self.framesize = core_framesize(state_14(state_le(state)));
                                start_found = 4;
                            }
                        }
                        m if m == dca::DCA_SYNCWORD_SUBSTREAM => {
                            if size == 6 {
                                self.framesize = exss_framesize(state);
                                start_found = 4;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                    continue;
                }

                if start_found == 2 && is_exss_marker(state) && self.framesize <= size + 2 {
                    self.framesize = size + 2;
                    start_found = 3;
                    i += 1;
                    continue;
                }

                if start_found == 3 {
                    if size == self.framesize + 4 {
                        self.framesize += exss_framesize(state);
                        start_found = 4;
                    }
                    i += 1;
                    continue;
                }

                if self.framesize > size {
                    i += 1;
                    continue;
                }

                if is_marker(state)
                    && (self.lastmarker == core_marker_of(state)
                        || self.lastmarker == dca::DCA_SYNCWORD_SUBSTREAM)
                {
                    self.frame_start_found = 0;
                    self.state64 = u64::MAX; // -1
                    self.size = 0;
                    return Some(if is_exss_marker(state) { i - 3 } else { i - 5 });
                }
                i += 1;
            }
        }

        self.frame_start_found = start_found;
        self.state64 = state;
        self.size = size;
        None
    }
}

// ───────────────────────── raw `dts` demuxer ─────────────────────────

/// Raw DTS demuxer: FFmpeg's `ff_raw_read_partial_packet` (1024-byte
/// reads) reassembled into whole frames by the `dca_parser.c` state
/// machine, since OxideAV has no separate parser stage.
pub struct RawDtsDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    pc: ParseState,
    /// Bytes not yet consumed by the parser.
    pending: Vec<u8>,
    /// Sample position of the next frame.
    next_pts: u64,
    eof: bool,
}

impl RawDtsDemuxer {
    fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn oxideav_core::CodecResolver) -> Result<Box<dyn Demuxer>> {
        input.seek(SeekFrom::Start(0))?;

        // Read the head to sniff the first frame header (rate/channels for
        // the stream params; ff_raw_audio_read_header leaves them unset,
        // but our CodecParameters want a rate — fill from the first frame).
        let mut head = vec![0u8; 256 * 1024];
        let mut filled = 0usize;
        loop {
            let n = input.read(&mut head[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
            if filled == head.len() {
                break;
            }
        }
        head.truncate(filled);

        // Parse the first frame header for stream parameters (after
        // bitstream conversion like dts_parse_params does).
        let (sample_rate, channels, npcmblocks) = sniff_params(&head);

        let codec_id = oxideav_core::CodecId::new(crate::CODEC_ID_STR_DCA);
        let mut params = CodecParameters::audio(codec_id);
        params.media_type = MediaType::Audio;
        if sample_rate > 0 {
            params.sample_rate = Some(sample_rate);
        }
        if channels > 0 {
            params.channels = Some(channels);
        }
        if npcmblocks > 0 {
            params.sample_format = Some(SampleFormat::F32);
        }

        let stream = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, i64::from(sample_rate.max(1))),
            duration: None,
            start_time: Some(0),
            params,
        };

        Ok(Box::new(RawDtsDemuxer {
            input,
            streams: vec![stream],
            pc: ParseState {
                lastmarker: 0,
                ..ParseState::default()
            },
            pending: head,
            next_pts: 0,
            eof: false,
        }))
    }

    fn read_more(&mut self) -> Result<usize> {
        let mut buf = vec![0u8; RAW_PACKET_SIZE];
        let mut filled = 0usize;
        loop {
            let n = self.input.read(&mut buf[filled..])?;
            if n == 0 {
                self.eof = true;
                break;
            }
            filled += n;
            if filled == buf.len() {
                break;
            }
        }
        self.pending.extend_from_slice(&buf[..filled]);
        Ok(filled)
    }
}

/// Parse the first core frame header for stream parameters.
fn sniff_params(head: &[u8]) -> (u32, u16, usize) {
    // Find the first parseable header.
    for i in 0..head.len().saturating_sub(DCA_CORE_FRAME_HEADER_SIZE) {
        let mut hdr = vec![0u8; DCA_CORE_FRAME_HEADER_SIZE];
        if dca::convert_bitstream(&head[i..], &mut hdr).is_none() {
            continue;
        }
        let mut gb = crate::bitreader::BitReader::new(&hdr);
        let mut h = CoreFrameHeader::default();
        if dca::parse_core_frame_header(&mut h, &mut gb).is_ok() {
            let channels = dca_count_chs(h.audio_mode, h.lfe_present);
            return (crate::data::FF_DCA_SAMPLE_RATES[h.sr_code as usize], channels, h.npcmblocks as usize);
        }
    }
    (0, 0, 0)
}

/// Channels for core audio mode + LFE flag.
fn dca_count_chs(audio_mode: u8, lfe: u8) -> u16 {
    const N: [u16; 10] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5];
    let base = N.get(audio_mode as usize).copied().unwrap_or(0);
    base + u16::from(lfe != 0)
}

impl Demuxer for RawDtsDemuxer {
    fn format_name(&self) -> &str {
        "dts"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            // Try to find a complete frame in `pending`.
            let end = self.pc.find_frame_end(&self.pending);
            if let Some(end) = end {
                // Frame data is pending[..end]; skip initial padding.
                self.pc.startpos = self.pc.startpos.min(end);
                let start = self.pc.startpos;
                self.pc.startpos = 0;
                let frame = self.pending[start..end].to_vec();
                self.pending.drain(..end);

                if frame.len() < MIN_FRAME {
                    continue;
                }

                let samples = frame_samples(&frame).unwrap_or(0);
                let pts = self.next_pts;
                self.next_pts += samples as u64;

                let tb = self.streams[0].time_base;
                return Ok(Packet::new(0, tb, frame)
                    .with_pts(pts as i64)
                    .with_dts(pts as i64)
                    .with_keyframe(true));
            }

            if self.eof {
                // Drain trailing frame if the parser holds one (FFmpeg
                // flushes the parser at EOF; without it the tail frame is
                // lost, so emit what remains when it looks like a frame).
                if self.pending.len() > MIN_FRAME && self.pc.lastmarker != 0 {
                    let start = self.pc.startpos.min(self.pending.len());
                    self.pc.startpos = 0;
                    let frame = self.pending[start..].to_vec();
                    self.pending.clear();
                    self.pc.lastmarker = 0;
                    let samples = frame_samples(&frame).unwrap_or(0);
                    let pts = self.next_pts;
                    self.next_pts += samples as u64;
                    let tb = self.streams[0].time_base;
                    return Ok(Packet::new(0, tb, frame)
                        .with_pts(pts as i64)
                        .with_dts(pts as i64)
                        .with_keyframe(true));
                }
                return Err(Error::Eof);
            }

            self.read_more()?;
        }
    }

    fn seek_to(&mut self, _stream_index: u32, _pts: i64) -> Result<i64> {
        Err(Error::unsupported("raw DTS demuxer does not support seeking"))
    }

    fn duration_micros(&self) -> Option<i64> {
        None
    }
}

const MIN_FRAME: usize = 16;

/// Samples per frame from the frame header (dca_parse_params duration).
fn frame_samples(frame: &[u8]) -> Option<usize> {
    let mut hdr = vec![0u8; DCA_CORE_FRAME_HEADER_SIZE];
    dca::convert_bitstream(frame, &mut hdr)?;
    let mut gb = crate::bitreader::BitReader::new(&hdr);
    let mut h = CoreFrameHeader::default();
    dca::parse_core_frame_header(&mut h, &mut gb).ok()?;
    Some(h.npcmblocks as usize * dca::DCA_PCMBLOCK_SAMPLES)
}

// ───────────────────────── dtshd demuxer ─────────────────────────

const AUPR_HDR: u64 = 0x4155_5052_2D48_4452;
const STRMDATA: u64 = 0x5354_524D_4441_5441;

/// `dtshd` demuxer: chunked wrapper; data arrives in STRMDATA chunks read
/// straight through (FFmpeg reads 1024-byte partial packets from within
/// the STRMDATA extent; framing matches the raw path).
pub struct DtshdDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    pc: ParseState,
    pending: Vec<u8>,
    data_end: u64,
    /// Sample rate from AUPR_HDR.
    sample_rate: u32,
    duration_samples: u64,
    /// Read position inside the STRMDATA extent.
    pos: u64,
    eof: bool,
}

impl DtshdDemuxer {
    fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn oxideav_core::CodecResolver) -> Result<Box<dyn Demuxer>> {
        input.seek(SeekFrom::Start(0))?;

        let mut sample_rate = 0u32;
        let mut duration_samples = 0u64;
        let mut initial_padding = 0u16;
        let mut channels = 0u16;
        let mut data_end = 0u64;
        let mut data_start = 0u64;
        let mut orig_nb_samples = 0u64;
        #[allow(unused_assignments)]
        let read_orig = &mut orig_nb_samples;
        let _ = read_orig;

        let mut chunk_type = [0u8; 8];
        let mut chunk_size = [0u8; 8];
        loop {
            if input.read_exact(&mut chunk_type).is_err() {
                break;
            }
            if input.read_exact(&mut chunk_size).is_err() {
                break;
            }
            let ctype = u64::from_be_bytes(chunk_type);
            let csize = u64::from_be_bytes(chunk_size);

            if csize < 4 {
                return Err(Error::InvalidData("dtshd: chunk size too small".into()));
            }
            if csize > (1u64 << 61) {
                return Err(Error::InvalidData("dtshd: chunk size too big".into()));
            }

            if ctype == STRMDATA {
                data_start = input.stream_position()?;
                data_end = data_start.checked_add(csize).ok_or_else(|| Error::InvalidData("dtshd: bad extent".into()))?;
                if data_end <= csize {
                    return Err(Error::InvalidData("dtshd: bad extent".into()));
                }
                break;
            } else if ctype == AUPR_HDR {
                if csize < 21 {
                    return Err(Error::InvalidData("dtshd: AUPR_HDR too small".into()));
                }
                let mut buf = vec![0u8; 24];
                input.read_exact(&mut buf)?;
                // avio_skip(3) then rb24.
                sample_rate = u32::from(buf[3]) << 16 | u32::from(buf[4]) << 8 | u32::from(buf[5]);
                if sample_rate == 0 {
                    return Err(Error::InvalidData("dtshd: zero sample rate".into()));
                }
                duration_samples = u64::from(u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]));
                duration_samples *= u64::from(u16::from_be_bytes([buf[10], buf[11]]));
                // AUPR_HDR layout: 0..3 skip; 3..6 rate; 6..10 num_frames;
                // 10..12 samples_per_frame; 12..17 orig_nb_samples (5 bytes);
                // 17..19 channel mask; 19..21 initial_padding.
                initial_padding = u16::from_be_bytes([buf[19], buf[20]]);
                orig_nb_samples = u64::from(u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]])) << 8
                    | u64::from(buf[16]);
                let _ = orig_nb_samples;
                channels = dca_count_chs_for_mask(u16::from_be_bytes([buf[17], buf[18]]));
                let skip = csize as usize - 24;
                if skip > 0 {
                    input.seek(SeekFrom::Current(skip as i64))?;
                }
            } else {
                // FILEINFO and others: skip
                input.seek(SeekFrom::Current(csize as i64))?;
            }
        }

        if data_end == 0 {
            return Err(Error::Eof);
        }

        input.seek(SeekFrom::Start(data_start))?;

        let codec_id = oxideav_core::CodecId::new(crate::CODEC_ID_STR_DCA);
        let mut params = CodecParameters::audio(codec_id);
        params.media_type = MediaType::Audio;
        if sample_rate > 0 {
            params.sample_rate = Some(sample_rate);
        }
        if channels > 0 {
            params.channels = Some(channels);
        }
        params.sample_format = Some(SampleFormat::S32);
        // dtshd padding (FFmpeg: skip-samples side data): the decoder trims
        // `initial_padding` leading samples and keeps orig_nb_samples.
        if initial_padding != 0 || duration_samples != 0 {
            params.options.insert(
                String::from("dtshd_initial_padding"),
                initial_padding.to_string(),
            );
            params.options.insert(
                String::from("dtshd_keep_samples"),
                (duration_samples - u64::from(initial_padding)).to_string(),
            );
        }

        let stream = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, i64::from(sample_rate.max(1))),
            duration: if sample_rate > 0 {
                Some(duration_samples as i64)
            } else {
                None
            },
            start_time: Some(i64::from(initial_padding)),
            params,
        };

        Ok(Box::new(DtshdDemuxer {
            input,
            streams: vec![stream],
            pc: ParseState::default(),
            pending: Vec::new(),
            data_end,
            sample_rate,
            duration_samples,
            pos: data_start,
            eof: false,
        }))
    }
}

/// `ff_dca_count_chs_for_mask`.
fn dca_count_chs_for_mask(mask: u16) -> u16 {
    let m = (u32::from(mask) & 0xffff) | ((u32::from(mask) & 0xae66) << 16);
    m.count_ones() as u16
}

impl Demuxer for DtshdDemuxer {
    fn format_name(&self) -> &str {
        "dtshd"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        // FFmpeg reads 1024-byte partial packets and reassembles frames in
        // the parser (AVSTREAM_PARSE_FULL_RAW). OxideAV has no parser, so
        // reassemble whole frames here with the dca_parser state machine.
        let mut frame_end = None;
        while frame_end.is_none() {
            frame_end = self.pc.find_frame_end(&self.pending);
            if frame_end.is_some() || self.eof {
                break;
            }
            let left = self.data_end.saturating_sub(self.pos);
            let chunk = left.min(1024);
            if chunk == 0 {
                self.eof = true;
                break;
            }
            let start = self.pending.len();
            self.pending.resize(start + chunk as usize, 0);
            self.input.read_exact(&mut self.pending[start..])?;
            self.pos += chunk;
        }

        let Some(end) = frame_end else {
            // EOF: flush any trailing data as one packet (parser flush).
            if self.pending.is_empty() {
                return Err(Error::Eof);
            }
            let data = std::mem::take(&mut self.pending);
            self.eof = true;
            let tb = self.streams[0].time_base;
            return Ok(Packet::new(0, tb, data).with_keyframe(true));
        };

        self.pc.startpos = self.pc.startpos.min(end);
        let start = self.pc.startpos;
        self.pc.startpos = 0;
        let frame = self.pending[start..end].to_vec();
        self.pending.drain(..end);

        if frame.len() < MIN_FRAME {
            return self.next_packet();
        }

        let tb = self.streams[0].time_base;
        Ok(Packet::new(0, tb, frame).with_keyframe(true))
    }

    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        // Byte-accurate seeking is not possible without a NAVI table;
        // FFmpeg's generic index builds one by reading. Reject.
        let _ = pts;
        Err(Error::unsupported("dtshd demuxer does not support seeking"))
    }

    fn duration_micros(&self) -> Option<i64> {
        if self.sample_rate == 0 {
            return None;
        }
        Some(((self.duration_samples * 1_000_000) / u64::from(self.sample_rate)) as i64)
    }
}

/// Install the `dts` and `dtshd` demuxers on the container registry.
pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("dts", open_dts);
    reg.register_demuxer("dtshd", open_dtshd);
    reg.register_probe("dts", dts_probe);
    reg.register_extension("dts", "dts");
    reg.register_extension("dtshd", "dtshd");
}

fn open_dts(input: Box<dyn ReadSeek>, codecs: &dyn oxideav_core::CodecResolver) -> Result<Box<dyn Demuxer>> {
    RawDtsDemuxer::open(input, codecs)
}

fn open_dtshd(input: Box<dyn ReadSeek>, codecs: &dyn oxideav_core::CodecResolver) -> Result<Box<dyn Demuxer>> {
    DtshdDemuxer::open(input, codecs)
}
