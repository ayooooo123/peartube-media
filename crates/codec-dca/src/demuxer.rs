// Ported from FFmpeg libavformat/dtsdec.c (raw DTS demuxer + probe) and
// libavformat/dtshddec.c (DTS-HD DTSHDHDR wrapper) (commit 2da55bf).
// Licensed under LGPL-2.1-or-later.

//! The raw `dts` demuxer (FFmpeg's `ff_raw_read_partial_packet` framing
//! plus full frame-length parsing from `dca_parser.c`) and the `dtshd`
//! chunked wrapper demuxer.

use crate::bitreader::BitReader;
use crate::data::{FF_DCA_FREQ_RANGES, FF_DCA_SAMPLE_RATES, FF_DCA_SAMPLING_FREQS};
use crate::dca::{self, CoreFrameHeader, DCA_CORE_FRAME_HEADER_SIZE};
use crate::decoder::MAX_PACKET_SIZE;
use crate::exss::{ExssParser, exss_parse};
use crate::lbr::{DCA_LBR_HEADER_DECODER_INIT, DCA_LBR_HEADER_SYNC_ONLY};
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
    /// Bytes of the caller's buffer already fed through the state machine.
    /// FFmpeg hands the parser only new input; the demuxers here pass their
    /// whole pending buffer, so a call resumes where the last one stopped.
    scanned: usize,
}

impl ParseState {
    /// Returns the position of the first byte of the next frame, or `None`.
    /// After `Some(end)` the caller drops `buf[..end]` and the next call
    /// scans the remainder from its start with the reset state, as
    /// `ff_combine_frame` restarts the parser at the frame boundary.
    fn find_frame_end(&mut self, buf: &[u8]) -> Option<usize> {
        let mut start_found = self.frame_start_found;
        let mut state = self.state64;
        let mut size = self.size;

        let mut i = self.scanned.min(buf.len());
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
                    self.scanned = 0;
                    let back = if is_exss_marker(state) { 3 } else { 5 };
                    return Some(i.saturating_sub(back));
                }
                i += 1;
            }
        }

        self.frame_start_found = start_found;
        self.state64 = state;
        self.size = size;
        self.scanned = buf.len();
        None
    }
}

/// Bytes of the next frame's marker read before the parser knows the
/// frame ahead of it ended: a core marker is recognised on its sixth byte.
const MARKER_TAIL: usize = 5;

/// The longest sync marker (core, in any of its four encodings): a frame
/// is known to start this many bytes after its first byte arrived.
pub(crate) const MARKER_LEN: usize = MARKER_TAIL + 1;

/// Input kept while no frame has started: a marker starts at most six
/// bytes before the byte that completes it.
const SCAN_TAIL: usize = 8;

/// The most a [`FrameSplitter`] holds: the largest frame the decoder takes
/// (a 16 KiB core and a 1 MiB extension substream), the start of the
/// marker that would end it, and one byte to tell that it does not.
const MAX_HELD: usize = MAX_PACKET_SIZE + MARKER_TAIL + 1;

/// Of a dropped frame: the bytes kept to read its header from.
const DROPPED_HEAD: usize = 4096;

/// A frame that grew past what the decoder takes and was dropped: the
/// stream offset of its first byte, and its first bytes, from which its
/// header still tells its duration.
pub(crate) struct Oversized {
    pub(crate) at: u64,
    pub(crate) head: Vec<u8>,
}

/// FFmpeg's dca parser over input that arrives in pieces: whole frames,
/// cut where `dca_find_frame_end` cuts them, the bytes ahead of the first
/// frame dropped as its initial padding and bytes between frames kept
/// with the frame before them. It holds one frame in progress, never one
/// longer than [`MAX_PACKET_SIZE`]: such a frame is an error, after which
/// cutting resumes at the next marker.
#[derive(Default)]
pub(crate) struct FrameSplitter {
    pc: ParseState,
    /// The frame in progress, or input scanned before one starts.
    pending: Vec<u8>,
    /// Stream offset of `pending[0]`.
    base: u64,
    /// Input was dropped since the last frame for lack of a frame start.
    skipped: bool,
}

impl FrameSplitter {
    /// Bytes [`push`](Self::push) takes now; at least one after
    /// [`next_frame`](Self::next_frame) returned `Ok(None)`.
    pub(crate) fn room(&self) -> usize {
        MAX_HELD.saturating_sub(self.pending.len())
    }

    /// Append input, at most [`room`](Self::room) bytes.
    pub(crate) fn push(&mut self, data: &[u8]) {
        debug_assert!(data.len() <= self.room());
        self.pending.extend_from_slice(data);
    }

    /// The next complete frame, and the stream offset of its first byte;
    /// `Ok(None)` until more input completes one.
    pub(crate) fn next_frame(&mut self) -> std::result::Result<Option<(u64, Vec<u8>)>, Oversized> {
        if let Some(end) = self.pc.find_frame_end(&self.pending) {
            let start = std::mem::take(&mut self.pc.startpos).min(end);
            let at = self.base + start as u64;
            let len = end - start;
            let frame = if len > MAX_PACKET_SIZE {
                Err(Oversized { at, head: self.pending[start..start + DROPPED_HEAD].to_vec() })
            } else {
                Ok(Some((at, self.pending[start..end].to_vec())))
            };
            self.drop_front(end);
            self.skipped = false;
            return frame;
        }
        if self.pc.frame_start_found != 0 {
            // Initial padding before the first frame.
            let lead = std::mem::take(&mut self.pc.startpos).min(self.pending.len());
            if lead > 0 {
                self.drop_front(lead);
                self.skipped = true;
            }
            if self.pending.len() > MAX_PACKET_SIZE + MARKER_TAIL {
                // Not a frame the decoder takes, whatever ends it: start
                // over on what may begin the next marker.
                let dropped = Oversized { at: self.base, head: self.pending[..DROPPED_HEAD].to_vec() };
                let keep = self.pending.split_off(self.pending.len() - SCAN_TAIL);
                self.base += self.pending.len() as u64;
                self.pending = keep;
                self.pc = ParseState::default();
                return Err(dropped);
            }
        } else if self.pending.len() > SCAN_TAIL {
            let junk = self.pending.len() - SCAN_TAIL;
            self.drop_front(junk);
            self.pc.size -= junk;
            self.skipped = true;
        }
        Ok(None)
    }

    /// The end of the input: the frame in progress, as FFmpeg's parser
    /// flush hands it over, and the stream offset of its first byte. One
    /// longer than [`MAX_PACKET_SIZE`] is the error
    /// [`next_frame`](Self::next_frame) gives a marker-ended one. Starts
    /// over afterwards.
    pub(crate) fn finish(&mut self) -> std::result::Result<Option<(u64, Vec<u8>)>, Oversized> {
        let start = self.pc.startpos.min(self.pending.len());
        let at = self.base + start as u64;
        let frame = &self.pending[start..];
        let result = if self.pc.frame_start_found == 0 {
            Ok(None)
        } else if frame.len() > MAX_PACKET_SIZE {
            Err(Oversized { at, head: frame[..DROPPED_HEAD].to_vec() })
        } else {
            Ok(Some((at, frame.to_vec())))
        };
        *self = Self::default();
        result
    }

    /// The stream offset of the frame in progress, once its start is known.
    pub(crate) fn frame_start(&self) -> Option<u64> {
        (self.pc.frame_start_found != 0).then(|| self.base + self.pc.startpos as u64)
    }

    /// Whether input was dropped for lack of a frame start since the last
    /// frame.
    pub(crate) fn skipped(&self) -> bool {
        self.skipped
    }

    /// Drop `n` scanned bytes from the front.
    fn drop_front(&mut self, n: usize) {
        self.pending.drain(..n);
        self.base += n as u64;
        self.pc.scanned = self.pc.scanned.saturating_sub(n);
    }
}

// ───────────────────────── raw `dts` demuxer ─────────────────────────

/// Raw DTS demuxer: FFmpeg's `ff_raw_read_partial_packet` (1024-byte
/// reads) reassembled into whole frames by the `dca_parser.c` state
/// machine, since OxideAV has no separate parser stage.
pub struct RawDtsDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    /// Frames cut from the input read so far.
    split: FrameSplitter,
    /// Timestamps of the frames cut so far.
    clock: FrameClock,
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

        let mut split = FrameSplitter::default();
        split.push(&head);
        Ok(Box::new(RawDtsDemuxer {
            input,
            streams: vec![stream],
            split,
            clock: FrameClock::default(),
            eof: false,
        }))
    }

    /// `ff_raw_read_partial_packet`: up to 1024 more bytes, as many as the
    /// splitter takes.
    fn read_more(&mut self) -> Result<()> {
        let mut buf = [0u8; RAW_PACKET_SIZE];
        let want = RAW_PACKET_SIZE.min(self.split.room());
        let mut filled = 0usize;
        while filled < want {
            let n = self.input.read(&mut buf[filled..want])?;
            if n == 0 {
                self.eof = true;
                break;
            }
            filled += n;
        }
        self.split.push(&buf[..filled]);
        Ok(())
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
            match self.split.next_frame() {
                Err(frame) => return Err(self.clock.drop_frame(&self.streams[0], &frame)),
                Ok(Some((_, frame))) if frame.len() < MIN_FRAME => continue,
                Ok(Some((_, frame))) => return Ok(self.clock.packet(&self.streams[0], frame)),
                Ok(None) => {}
            }
            if self.eof {
                // FFmpeg flushes the parser at the end of the input; the
                // tail frame it holds comes out when it looks like one.
                return match self.split.finish() {
                    Err(frame) => Err(self.clock.drop_frame(&self.streams[0], &frame)),
                    Ok(Some((_, frame))) if frame.len() > MIN_FRAME => Ok(self.clock.packet(&self.streams[0], frame)),
                    Ok(_) => Err(Error::Eof),
                };
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

fn oversized() -> Error {
    Error::invalid(format!("dts: frame longer than the {MAX_PACKET_SIZE} bytes the decoder takes"))
}

/// `dca_parse_params` (dca_parser.c): a frame's duration in samples at its
/// own rate, and that rate — from the core frame header, or for a frame
/// that starts with an extension substream, from its LBR or XLL asset.
/// `lbr_sr_code` is the parser context's `sr_code`: an LBR sync-only
/// header reuses the rate of the last decoder-init header.
pub(crate) fn parse_params(frame: &[u8], lbr_sr_code: &mut Option<u8>) -> Option<(u64, u32)> {
    if frame.len() < DCA_CORE_FRAME_HEADER_SIZE {
        return None;
    }
    if u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) == dca::DCA_SYNCWORD_SUBSTREAM {
        let mut exss = ExssParser::default();
        exss_parse(&mut exss, frame).ok()?;
        let asset = &exss.assets[0];
        let component = |offset: usize, size: usize| frame.get(offset..offset.checked_add(size)?);
        if asset.has_lbr() {
            let mut gb = BitReader::new(component(asset.lbr_offset, asset.lbr_size)?);
            if gb.get_bits_long(32) != dca::DCA_SYNCWORD_LBR {
                return None;
            }
            match gb.get_bits(8) as u8 {
                DCA_LBR_HEADER_DECODER_INIT => *lbr_sr_code = Some(gb.get_bits(8) as u8),
                DCA_LBR_HEADER_SYNC_ONLY => {}
                _ => return None,
            }
            let code = usize::from((*lbr_sr_code)?);
            let rate = *FF_DCA_SAMPLING_FREQS.get(code)?;
            return Some((1024 << FF_DCA_FREQ_RANGES[code], rate));
        }
        if asset.has_xll() {
            let mut gb = BitReader::new(component(asset.xll_offset, asset.xll_size)?);
            if gb.get_bits_long(32) != dca::DCA_SYNCWORD_XLL || gb.get_bits(4) != 0 {
                return None;
            }
            gb.skip(8);
            let header_bits = gb.get_bits(5) + 1;
            gb.skip(header_bits);
            gb.skip(4);
            let nsamples_log2 = gb.get_bits(4) + gb.get_bits(4);
            if nsamples_log2 > 24 {
                return None;
            }
            let rate = u32::try_from(asset.max_sample_rate).ok()?;
            return Some((u64::from(1 + u32::from(rate > 96_000)) << nsamples_log2, rate));
        }
        return None;
    }
    let mut hdr = [0u8; DCA_CORE_FRAME_HEADER_SIZE];
    dca::convert_bitstream(&frame[..DCA_CORE_FRAME_HEADER_SIZE], &mut hdr)?;
    let mut h = CoreFrameHeader::default();
    dca::parse_core_frame_header(&mut h, &mut BitReader::new(&hdr)).ok()?;
    let samples = u64::from(h.npcmblocks) * dca::DCA_PCMBLOCK_SAMPLES as u64;
    Some((samples, FF_DCA_SAMPLE_RATES[usize::from(h.sr_code)]))
}

/// Packet timing for DTS input that carries no timestamps, as libavformat
/// stamps a parsed raw stream: a frame lasts its [`parse_params`] duration
/// rescaled to the stream's sample rate (`dca_parse`'s `s->duration`; 0
/// when the header does not parse), and its pts and dts are the sum of
/// the durations before it (`cur_dts`), starting at 0.
#[derive(Default)]
struct FrameClock {
    lbr_sr_code: Option<u8>,
    next_pts: i64,
}

impl FrameClock {
    fn packet(&mut self, stream: &StreamInfo, frame: Vec<u8>) -> Packet {
        let pts = self.next_pts;
        let duration = self.advance(stream, &frame);
        Packet::new(0, stream.time_base, frame)
            .with_pts(pts)
            .with_dts(pts)
            .with_duration(duration)
            .with_keyframe(true)
    }

    /// Move past a frame: its duration from its header, rescaled to the
    /// stream's sample rate, 0 when the header does not parse.
    fn advance(&mut self, stream: &StreamInfo, frame: &[u8]) -> i64 {
        let duration = match parse_params(frame, &mut self.lbr_sr_code) {
            Some((samples, rate)) if rate != 0 => match stream.params.sample_rate {
                // av_rescale(duration, avctx->sample_rate, sample_rate)
                Some(to) => ((u128::from(samples) * u128::from(to) + u128::from(rate / 2)) / u128::from(rate))
                    .try_into()
                    .unwrap_or(i64::MAX),
                None => i64::try_from(samples).unwrap_or(i64::MAX),
            },
            _ => 0,
        };
        self.next_pts = self.next_pts.saturating_add(duration);
        duration
    }

    /// Move past a frame longer than the decoder takes, as FFmpeg times
    /// the oversized packet it would return (the next frame follows it),
    /// and the error for it.
    fn drop_frame(&mut self, stream: &StreamInfo, frame: &Oversized) -> Error {
        self.advance(stream, &frame.head);
        oversized()
    }
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
    /// Frames cut from the STRMDATA read so far.
    split: FrameSplitter,
    data_end: u64,
    /// Sample rate from AUPR_HDR.
    sample_rate: u32,
    duration_samples: u64,
    /// Read position inside the STRMDATA extent.
    pos: u64,
    eof: bool,
    clock: FrameClock,
}

impl DtshdDemuxer {
    /// `dtshd_read_header`. The input is seekable, so like FFmpeg on a
    /// seekable source it reads every chunk header (an AUPR_HDR after the
    /// STRMDATA chunk still counts) and then returns to the stream data.
    fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn oxideav_core::CodecResolver) -> Result<Box<dyn Demuxer>> {
        input.seek(SeekFrom::Start(0))?;

        let mut sample_rate = 0u32;
        let mut duration_samples = 0u64;
        let mut initial_padding = 0u16;
        let mut orig_nb_samples = 0u64;
        let mut channels = 0u16;
        let mut data_end = 0u64;
        let mut data_start = 0u64;

        let mut chunk_type = [0u8; 8];
        let mut chunk_size = [0u8; 8];
        loop {
            if input.read_exact(&mut chunk_type).is_err() || input.read_exact(&mut chunk_size).is_err() {
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

            let skip = if ctype == STRMDATA {
                data_start = input.stream_position()?;
                data_end = data_start.checked_add(csize).ok_or_else(|| Error::InvalidData("dtshd: bad extent".into()))?;
                if data_end <= csize {
                    return Err(Error::InvalidData("dtshd: bad extent".into()));
                }
                csize
            } else if ctype == AUPR_HDR {
                if csize < 21 {
                    return Err(Error::InvalidData("dtshd: AUPR_HDR too small".into()));
                }
                // skip(3) rb24 rate, rb32 num_frames, rb16 samples_per_frame,
                // rb32+r8 orig_nb_samples, rb16 channel mask, rb16 padding.
                let mut buf = [0u8; 21];
                input.read_exact(&mut buf)?;
                sample_rate = u32::from(buf[3]) << 16 | u32::from(buf[4]) << 8 | u32::from(buf[5]);
                if sample_rate == 0 {
                    return Err(Error::InvalidData("dtshd: zero sample rate".into()));
                }
                duration_samples = u64::from(u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]))
                    * u64::from(u16::from_be_bytes([buf[10], buf[11]]));
                orig_nb_samples = u64::from(u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]])) << 8
                    | u64::from(buf[16]);
                channels = dca_count_chs_for_mask(u16::from_be_bytes([buf[17], buf[18]]));
                initial_padding = u16::from_be_bytes([buf[19], buf[20]]);
                csize - 21
            } else {
                // FILEINFO and others
                csize
            };
            let skip = i64::try_from(skip).map_err(|_| Error::InvalidData("dtshd: chunk size too big".into()))?;
            input.seek(SeekFrom::Current(skip))?;
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
        // FFmpeg skips `start_skip_samples` (the initial padding) and drops
        // samples from `first_discard_sample` = orig_nb_samples + padding
        // on, unless that is 0: the decoder keeps `orig_nb_samples` after
        // the padding.
        if initial_padding != 0 {
            params.options.insert(String::from("dtshd_initial_padding"), initial_padding.to_string());
        }
        if orig_nb_samples + u64::from(initial_padding) != 0 {
            params.options.insert(String::from("dtshd_keep_samples"), orig_nb_samples.to_string());
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
            split: FrameSplitter::default(),
            data_end,
            sample_rate,
            duration_samples,
            pos: data_start,
            eof: false,
            clock: FrameClock::default(),
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
        loop {
            match self.split.next_frame() {
                Err(frame) => return Err(self.clock.drop_frame(&self.streams[0], &frame)),
                Ok(Some((_, frame))) if frame.len() < MIN_FRAME => continue,
                Ok(Some((_, frame))) => return Ok(self.clock.packet(&self.streams[0], frame)),
                Ok(None) => {}
            }
            if self.eof {
                // The parser flush emits what it holds as the last frame,
                // after any initial padding.
                return match self.split.finish() {
                    Err(frame) => Err(self.clock.drop_frame(&self.streams[0], &frame)),
                    Ok(Some((_, frame))) if !frame.is_empty() => Ok(self.clock.packet(&self.streams[0], frame)),
                    Ok(_) => Err(Error::Eof),
                };
            }
            let left = self.data_end.saturating_sub(self.pos);
            let chunk = left.min(RAW_PACKET_SIZE as u64).min(self.split.room() as u64) as usize;
            if chunk == 0 {
                self.eof = true;
                continue;
            }
            let mut buf = [0u8; RAW_PACKET_SIZE];
            self.input.read_exact(&mut buf[..chunk])?;
            self.split.push(&buf[..chunk]);
            self.pos += chunk as u64;
        }
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
        i64::try_from(u128::from(self.duration_samples) * 1_000_000 / u128::from(self.sample_rate)).ok()
    }
}

/// `dtshd_probe`: the DTSHDHDR chunk that opens every DTS-HD file.
fn dtshd_probe(p: &ProbeData) -> ProbeScore {
    if p.buf.starts_with(b"DTSHDHDR") {
        oxideav_core::MAX_PROBE_SCORE
    } else {
        0
    }
}

/// Install the `dts` and `dtshd` demuxers on the container registry.
pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("dts", open_dts);
    reg.register_demuxer("dtshd", open_dtshd);
    reg.register_probe("dts", dts_probe);
    reg.register_probe("dtshd", dtshd_probe);
    reg.register_extension("dts", "dts");
    reg.register_extension("dtshd", "dtshd");
}

fn open_dts(input: Box<dyn ReadSeek>, codecs: &dyn oxideav_core::CodecResolver) -> Result<Box<dyn Demuxer>> {
    RawDtsDemuxer::open(input, codecs)
}

fn open_dtshd(input: Box<dyn ReadSeek>, codecs: &dyn oxideav_core::CodecResolver) -> Result<Box<dyn Demuxer>> {
    DtshdDemuxer::open(input, codecs)
}
