// Ported from FFmpeg libavformat/vc1dec.c, libavformat/vc1test.c,
// libavcodec/vc1_parser.c, and seek.c's seek_frame_generic over
// demux-seek-core (commit 2da55bf).
// License: LGPL-2.1-or-later.

use std::io::{Read, Seek, SeekFrom};
use demux_seek_core::{read_on, Allowance, Index};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, ProbeData, ProbeScore, Rational, ReadSeek, Result, StreamInfo,
    TimeBase, PROBE_SCORE_EXTENSION,
};

use crate::bits::BitReader;

/// Codec id of the VC-1 Advanced Profile streams the `vc1` demuxer emits.
pub const CODEC_ID_VC1: &str = "vc1";
/// Codec id of the WMV3 (VC-1 Simple/Main) streams in `.rcv` files.
pub const CODEC_ID_WMV3: &str = "wmv3";

// ───────────────────────── Generic index (seek.c) ─────────────────────────

/// A demuxer seekable the way seek.c seek_frame_generic seeks it
/// (AVFMT_GENERIC_INDEX: av_read_frame indexes each key packet it
/// returns).
trait GenericSeek {
    /// Where reading was, for a failed seek to give back.
    type Reading;
    fn index(&self) -> &Index;
    /// The next packet (indexing it when key), with its key flag and dts.
    fn read(&mut self) -> Result<(bool, Option<i64>)>;
    /// Read on from `pos` (ff_read_frame_flush), the dts at `ts`
    /// (avpriv_update_cur_dts) or, at the start of the data, where opening
    /// left it.
    fn restart(&mut self, pos: u64, ts: Option<i64>) -> Result<()>;
    fn data_offset(&self) -> u64;
    fn allowance(&mut self) -> &mut Allowance;
    fn take_reading(&mut self) -> Result<Self::Reading>;
    fn give_back(&mut self, reading: Self::Reading) -> Result<()>;
}

/// seek_frame_generic with AVSEEK_FLAG_BACKWARD: the last key packet at
/// or before `ts` among those returned so far; past the last of them
/// packets are read on, within the seek's allowance, until a key packet
/// starts after the target or more than 1000 others did. A seek that
/// fails leaves reading where it was.
fn seek_generic<D: GenericSeek>(d: &mut D, ts: i64) -> Result<i64> {
    let found = d.index().search(ts, true);
    if found.is_none() && d.index().entries().first().is_some_and(|e| ts < e.timestamp) {
        return Err(Error::invalid("seek before the first key frame"));
    }
    let reading = d.take_reading()?;
    d.allowance().start();
    let landed = land(d, ts, found);
    let landed = d.allowance().finish(landed);
    if landed.is_err() {
        d.give_back(reading)?;
    }
    landed
}

fn land<D: GenericSeek>(d: &mut D, ts: i64, mut found: Option<usize>) -> Result<i64> {
    if found.is_none() || found == Some(d.index().entries().len() - 1) {
        match d.index().entries().last().copied() {
            Some(e) => d.restart(e.pos as u64, Some(e.timestamp))?,
            None => {
                let at = d.data_offset();
                d.restart(at, None)?
            }
        }
        read_on(ts, || d.read())?;
        found = d.index().search(ts, true);
    }
    let Some(i) = found else {
        return Err(Error::invalid("no key frame to seek to"));
    };
    let e = d.index().entries()[i];
    d.restart(e.pos as u64, Some(e.timestamp))?;
    Ok(e.timestamp)
}

// ───────────────────────── VC-1 Test Format (.rcv) ─────────────────────────

pub fn probe_vc1test(probe: &ProbeData) -> ProbeScore {
    let p = probe.buf;
    if p.len() < 24 {
        return 0;
    }
    let size = u32::from_le_bytes([p[4], p[5], p[6], p[7]]) as usize;
    if p[3] != 0xC5 || size < 4 || size > p.len().saturating_sub(20) {
        return 0;
    }
    if u32::from_le_bytes([p[size + 16], p[size + 17], p[size + 18], p[size + 19]]) != 0xC {
        return 0;
    }
    PROBE_SCORE_EXTENSION
}

pub struct Vc1TestDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    fps: u32,
    pts: i64,
    /// Where the frames start.
    data_offset: u64,
    index: Index,
    /// What the seek under way may still read.
    allowance: Allowance,
}

impl Vc1TestDemuxer {
    pub fn open(mut input: Box<dyn ReadSeek>) -> Result<Self> {
        let mut hdr = [0u8; 8];
        input.read_exact(&mut hdr).map_err(Error::Io)?;
        let frames = (hdr[0] as u32) | ((hdr[1] as u32) << 8) | ((hdr[2] as u32) << 16);
        if hdr[3] != 0xC5 {
            return Err(Error::invalid("vc1test: missing 0xC5 marker"));
        }
        let size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
        if size < 4 {
            return Err(Error::invalid("vc1test: header size < 4"));
        }

        let mut extradata = vec![0u8; 4];
        input.read_exact(&mut extradata).map_err(Error::Io)?;
        if size > 4 {
            input.seek(SeekFrom::Current((size - 4) as i64)).map_err(Error::Io)?;
        }

        let mut meta = [0u8; 24];
        input.read_exact(&mut meta).map_err(Error::Io)?;
        let height = u32::from_le_bytes([meta[0], meta[1], meta[2], meta[3]]);
        let width = u32::from_le_bytes([meta[4], meta[5], meta[6], meta[7]]);
        let magic = u32::from_le_bytes([meta[8], meta[9], meta[10], meta[11]]);
        if magic != 0xC {
            return Err(Error::invalid("vc1test: invalid magic marker"));
        }
        let mut fps = u32::from_le_bytes([meta[20], meta[21], meta[22], meta[23]]);

        let time_base = if fps == 0xFFFFFFFF {
            TimeBase::new(1, 1000)
        } else {
            if fps == 0 {
                fps = 1;
            }
            TimeBase::new(1, fps as i64)
        };

        let mut params = CodecParameters::video(CodecId::new(CODEC_ID_WMV3));
        params.width = Some(width);
        params.height = Some(height);
        params.extradata = extradata;

        let stream = StreamInfo {
            index: 0,
            params,
            time_base,
            duration: Some(frames as i64),
            start_time: Some(0),
        };

        let data_offset = input.stream_position().map_err(Error::Io)?;
        let allowance = Allowance::default();
        Ok(Self {
            input: Box::new(allowance.meter(input)),
            streams: vec![stream],
            fps,
            pts: 0,
            data_offset,
            index: Index::default(),
            allowance,
        })
    }

    /// vc1t_read_packet: one frame and where its frame header starts.
    fn read_frame(&mut self) -> Result<(Packet, u64)> {
        let pos = self.input.stream_position().map_err(Error::Io)?;
        let mut hdr = [0u8; 8];
        match self.input.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::Eof),
            Err(e) => return Err(Error::Io(e)),
        }

        let frame_size = (hdr[0] as usize) | ((hdr[1] as usize) << 8) | ((hdr[2] as usize) << 16);
        let keyframe = (hdr[3] & 0x80) != 0;
        let file_pts = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        self.allowance.spend(1, 0)?;

        let mut data = vec![0u8; frame_size];
        self.input.read_exact(&mut data).map_err(Error::Io)?;

        let pts = if self.fps == 0xFFFFFFFF {
            Some(file_pts as i64)
        } else {
            let p = self.pts;
            self.pts += 1;
            Some(p)
        };

        let mut pkt = Packet {
            stream_index: 0,
            time_base: self.streams[0].time_base,
            pts,
            dts: pts,
            duration: Some(1),
            flags: Default::default(),
            data,
        };
        pkt.flags.keyframe = keyframe;
        if let (true, Some(dts)) = (keyframe, pkt.dts) {
            self.index.add(pos as i64, dts, 0, 0, true);
        }
        Ok((pkt, pos))
    }
}

impl GenericSeek for Vc1TestDemuxer {
    /// The input position and the frame counter.
    type Reading = (u64, i64);

    fn index(&self) -> &Index {
        &self.index
    }

    fn read(&mut self) -> Result<(bool, Option<i64>)> {
        let (pkt, _) = self.read_frame()?;
        Ok((pkt.flags.keyframe, pkt.dts))
    }

    fn restart(&mut self, pos: u64, ts: Option<i64>) -> Result<()> {
        self.input.seek(SeekFrom::Start(pos)).map_err(Error::Io)?;
        self.pts = ts.unwrap_or(0);
        Ok(())
    }

    fn data_offset(&self) -> u64 {
        self.data_offset
    }

    fn allowance(&mut self) -> &mut Allowance {
        &mut self.allowance
    }

    fn take_reading(&mut self) -> Result<(u64, i64)> {
        Ok((self.input.stream_position()?, self.pts))
    }

    fn give_back(&mut self, (at, pts): (u64, i64)) -> Result<()> {
        self.input.seek(SeekFrom::Start(at))?;
        self.pts = pts;
        Ok(())
    }
}

impl Demuxer for Vc1TestDemuxer {
    fn format_name(&self) -> &str {
        "vc1test"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        Ok(self.read_frame()?.0)
    }

    /// vc1test.c is AVFMT_GENERIC_INDEX: seek.c seek_frame_generic over
    /// the key frames the file flags. FFmpeg times frames only with a
    /// millisecond time base or without B-frame delay, and cannot seek the
    /// others; this demuxer numbers their frames and seeks those too.
    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        seek_generic(self, pts)
    }
}

// ───────────────────────── Raw VC-1 Elementary Stream (.vc1) ─────────────────────────

pub fn probe_vc1(probe: &ProbeData) -> ProbeScore {
    let p = probe.buf;
    if p.len() < 16 {
        return 0;
    }
    let mut seq = 0;
    let mut entry = 0;
    let mut invalid = 0;
    let mut frame = 0;
    let mut i = 0;

    while i + 4 <= p.len() {
        if p[i] == 0 && p[i + 1] == 0 && p[i + 2] == 1 {
            let code = p[i + 3];
            i += 4;
            match code {
                0x0F => {
                    // Sequence header
                    if i + 3 <= p.len() {
                        let profile = (p[i] & 0xC0) >> 6;
                        if profile != 3 {
                            seq = 0;
                            invalid += 1;
                            continue;
                        }
                        let level = (p[i] & 0x38) >> 3;
                        if level >= 5 {
                            seq = 0;
                            invalid += 1;
                            continue;
                        }
                        let chroma = (p[i] & 0x06) >> 1;
                        if chroma != 1 {
                            seq = 0;
                            invalid += 1;
                            continue;
                        }
                        seq += 1;
                    }
                }
                0x0E => {
                    // Entry point
                    if seq == 0 {
                        invalid += 1;
                        continue;
                    }
                    entry += 1;
                }
                0x0D | 0x0C | 0x0B => {
                    // Frame, field, slice
                    if seq > 0 && entry > 0 {
                        frame += 1;
                    }
                }
                _ => {}
            }
        } else {
            i += 1;
        }
    }

    if frame > 1 && (frame >> 1) > invalid {
        PROBE_SCORE_EXTENSION / 2 + 1
    } else if frame >= 1 {
        PROBE_SCORE_EXTENSION / 4
    } else {
        0
    }
}

/// FFmpeg's raw-video time base.
const VC1_TIME_BASE: i64 = 1_200_000;
/// Bytes of each header `vc1_parse` unescapes and reads (`UNESCAPED_THRESHOLD`).
const VC1_HEADER_BYTES: usize = 37;

/// What FFmpeg's `vc1_parse` learns from the sequence and picture headers,
/// which libavformat turns into packet durations and key flags.
#[derive(Default)]
struct Vc1EsHeaders {
    max_coded_size: Option<(u32, u32)>,
    broadcast: bool,
    interlace: bool,
    tfcntrflag: bool,
    psf: bool,
    /// `avctx->framerate` (frames per second, num/den) from the display
    /// extension.
    framerate: Option<(i64, i64)>,
    rff: bool,
    rptfrm: i64,
    repeat_pict: i64,
    /// The last picture header was an I picture (`pict_type == I`).
    key: bool,
}

impl Vc1EsHeaders {
    /// Reads the sequence and frame headers of every unit in `data`.
    fn scan(&mut self, data: &[u8]) {
        let mut i = 0;
        while i + 4 <= data.len() {
            if data[i] != 0 || data[i + 1] != 0 || data[i + 2] != 1 {
                i += 1;
                continue;
            }
            let code = data[i + 3];
            if code == 0x0F || code == 0x0D {
                let head = Self::unescape_head(&data[i + 4..]);
                let mut gb = BitReader::new(&head);
                if code == 0x0F {
                    self.sequence_header(&mut gb);
                } else {
                    self.frame_header(&mut gb);
                }
            }
            i += 4;
        }
    }

    /// The first `VC1_HEADER_BYTES` of a unit with emulation prevention
    /// bytes (`00 00 03`) removed, up to the next start code.
    fn unescape_head(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(VC1_HEADER_BYTES);
        let mut zeros = 0;
        for (k, &b) in payload.iter().enumerate() {
            if out.len() >= VC1_HEADER_BYTES || (zeros >= 2 && b == 1 && k >= 2) {
                break;
            }
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            out.push(b);
        }
        out
    }

    /// The fields of `decode_sequence_header_adv` that time packets.
    fn sequence_header(&mut self, gb: &mut BitReader) {
        if gb.read(2) != 3 {
            return;
        }
        gb.skip(3); // level
        if gb.read(2) != 1 {
            return; // only 4:2:0 is valid
        }
        gb.skip(3 + 5 + 1); // frmrtq_postproc, bitrtq_postproc, postprocflag
        let w = (gb.read(12) + 1) * 2;
        let h = (gb.read(12) + 1) * 2;
        self.max_coded_size = Some((w, h));
        self.broadcast = gb.read_bit() != 0;
        self.interlace = gb.read_bit() != 0;
        self.tfcntrflag = gb.read_bit() != 0;
        gb.skip(2); // finterpflag, reserved
        self.psf = gb.read_bit() != 0;
        if self.psf || gb.read_bit() == 0 {
            return;
        }
        gb.skip(28); // display size
        let ar = if gb.read_bit() != 0 { gb.read(4) } else { 0 };
        if ar == 15 {
            gb.skip(16);
        }
        if gb.read_bit() != 0 {
            if gb.read_bit() != 0 {
                self.framerate = Some((gb.read(16) as i64 + 1, 32));
            } else {
                const FPS_NR: [i64; 7] = [24, 25, 30, 50, 60, 48, 72];
                const FPS_DR: [i64; 2] = [1000, 1001];
                let nr = gb.read(8) as usize;
                let dr = gb.read(4) as usize;
                if (1..8).contains(&nr) && (1..3).contains(&dr) {
                    self.framerate = Some((FPS_NR[nr - 1] * 1000, FPS_DR[dr - 1]));
                }
            }
        }
    }

    /// The start of `ff_vc1_parse_frame_header_adv`: picture type and the
    /// pulldown flags (`vc1_extract_header`'s `repeat_pict`).
    fn frame_header(&mut self, gb: &mut BitReader) {
        let field_mode = self.interlace && gb.decode012() == 2;
        self.key = if field_mode { gb.read(3) & 6 == 0 } else { gb.get_unary(0, 4) == 2 };
        if self.tfcntrflag {
            gb.skip(8);
        }
        self.repeat_pict = if self.broadcast {
            if !self.interlace || self.psf {
                self.rptfrm = gb.read(2) as i64;
            } else {
                gb.skip(1); // tff
                self.rff = gb.read_bit() != 0;
            }
            if self.rff {
                2
            } else if self.rptfrm != 0 {
                self.rptfrm * 2 + 1
            } else {
                1
            }
        } else {
            0
        };
    }

    /// libavformat's `compute_frame_duration` in `VC1_TIME_BASE` ticks: the
    /// coded frame rate (two fields per frame, plus repeats) when the
    /// sequence header has one, else the raw demuxer's 25 fps.
    fn duration(&self) -> i64 {
        match self.framerate {
            Some((num, den)) if den * 1000 > num => den * (1 + self.repeat_pict) * VC1_TIME_BASE / (num * 2),
            Some(_) => 0,
            None => VC1_TIME_BASE / 25,
        }
    }

    /// After a seek FFmpeg's new parser knows no sequence header yet; the
    /// frame rate it set is the codec context's, which stays.
    fn reset(&self) -> Self {
        Self { framerate: self.framerate, ..Self::default() }
    }
}

pub struct Vc1Demuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    buffer: Vec<u8>,
    /// Where `buffer` starts in the input.
    buffer_pos: u64,
    /// Next buffer offset the frame-boundary search examines.
    scan: usize,
    /// A frame or field start code was seen in the pending packet
    /// (`frame_start_found`).
    pic_found: bool,
    eof_reached: bool,
    headers: Vc1EsHeaders,
    next_dts: i64,
    index: Index,
    /// What the seek under way may still read.
    allowance: Allowance,
}

/// Where the raw VC-1 parser was, given back when a seek fails.
struct Vc1Reading {
    at: u64,
    buffer: Vec<u8>,
    buffer_pos: u64,
    scan: usize,
    pic_found: bool,
    eof_reached: bool,
    headers: Vc1EsHeaders,
    next_dts: i64,
}

impl Vc1Demuxer {
    pub fn open(mut input: Box<dyn ReadSeek>) -> Result<Self> {
        let buffer_pos = input.stream_position().map_err(Error::Io)?;
        let mut buffer = vec![0u8; 16384];
        let n = input.read(&mut buffer).map_err(Error::Io)?;
        buffer.truncate(n);

        // Size and frame rate from the first sequence header.
        let mut first = Vc1EsHeaders::default();
        if let Some(i) = buffer.windows(4).position(|w| w == [0, 0, 1, 0x0F]) {
            first.scan(&buffer[i..(i + 4 + VC1_HEADER_BYTES).min(buffer.len())]);
        }
        let mut params = CodecParameters::video(CodecId::new(CODEC_ID_VC1));
        params.width = first.max_coded_size.map(|s| s.0);
        params.height = first.max_coded_size.map(|s| s.1);
        params.frame_rate = Some(match first.framerate {
            Some((num, den)) => Rational::new(num, den),
            None => Rational::new(25, 1),
        });
        let stream = StreamInfo {
            index: 0,
            params,
            time_base: TimeBase::new(1, VC1_TIME_BASE),
            duration: None,
            start_time: Some(0),
        };
        let allowance = Allowance::default();
        Ok(Self {
            input: Box::new(allowance.meter(input)),
            streams: vec![stream],
            buffer,
            buffer_pos,
            scan: 0,
            pic_found: false,
            eof_reached: n == 0,
            headers: Vc1EsHeaders::default(),
            next_dts: 0,
            index: Index::default(),
            allowance,
        })
    }

    /// `vc1_parse`'s frame split: once a frame or field start code was seen,
    /// the next start code other than field, slice or end-of-sequence
    /// begins the next packet. Resumes where the previous call stopped.
    fn find_frame_end(&mut self) -> Option<usize> {
        let buf = &self.buffer;
        let mut i = self.scan;
        while i + 4 <= buf.len() {
            if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
                let code = buf[i + 3];
                if !self.pic_found {
                    self.pic_found = code == 0x0D || code == 0x0C;
                } else if code != 0x0C && code != 0x0B && code != 0x0A {
                    self.scan = 0;
                    self.pic_found = false;
                    return Some(i);
                }
                i += 4;
            } else {
                i += 1;
            }
        }
        self.scan = i;
        None
    }

    /// One packet per frame, timed and flagged like FFmpeg's raw demuxer:
    /// dts advances by each frame's duration, I pictures are key frames.
    /// FFmpeg leaves the pts of some frames unset; ours equals the dts.
    fn packet(&mut self, data: Vec<u8>) -> Packet {
        let pos = self.buffer_pos;
        self.buffer_pos += data.len() as u64;
        self.headers.scan(&data);
        let duration = self.headers.duration();
        let ts = self.next_dts;
        self.next_dts += duration;
        let mut pkt = Packet {
            stream_index: 0,
            time_base: self.streams[0].time_base,
            pts: Some(ts),
            dts: Some(ts),
            duration: Some(duration),
            flags: Default::default(),
            data,
        };
        pkt.flags.keyframe = self.headers.key;
        if pkt.flags.keyframe {
            self.index.add(pos as i64, ts, 0, 0, true);
        }
        pkt
    }
}

impl GenericSeek for Vc1Demuxer {
    type Reading = Vc1Reading;

    fn index(&self) -> &Index {
        &self.index
    }

    fn read(&mut self) -> Result<(bool, Option<i64>)> {
        let pkt = self.next_packet()?;
        Ok((pkt.flags.keyframe, pkt.dts))
    }

    /// A new parser at `pos`; its frames timed from `ts` on, or from 0 at
    /// the start of the data, at the frame rate the codec context kept.
    fn restart(&mut self, pos: u64, ts: Option<i64>) -> Result<()> {
        self.input.seek(SeekFrom::Start(pos)).map_err(Error::Io)?;
        self.buffer.clear();
        self.buffer_pos = pos;
        self.scan = 0;
        self.pic_found = false;
        self.eof_reached = false;
        self.headers = self.headers.reset();
        self.next_dts = ts.unwrap_or(0);
        Ok(())
    }

    fn data_offset(&self) -> u64 {
        0
    }

    fn allowance(&mut self) -> &mut Allowance {
        &mut self.allowance
    }

    fn take_reading(&mut self) -> Result<Vc1Reading> {
        let headers = self.headers.reset();
        Ok(Vc1Reading {
            at: self.input.stream_position()?,
            buffer: std::mem::take(&mut self.buffer),
            buffer_pos: self.buffer_pos,
            scan: self.scan,
            pic_found: self.pic_found,
            eof_reached: self.eof_reached,
            headers: std::mem::replace(&mut self.headers, headers),
            next_dts: self.next_dts,
        })
    }

    fn give_back(&mut self, r: Vc1Reading) -> Result<()> {
        self.input.seek(SeekFrom::Start(r.at))?;
        (self.buffer, self.buffer_pos, self.scan, self.pic_found) = (r.buffer, r.buffer_pos, r.scan, r.pic_found);
        (self.eof_reached, self.headers, self.next_dts) = (r.eof_reached, r.headers, r.next_dts);
        Ok(())
    }
}

impl Demuxer for Vc1Demuxer {
    fn format_name(&self) -> &str {
        "vc1"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let mut chunk = [0u8; 16384];
        loop {
            if let Some(end) = self.find_frame_end() {
                let data = self.buffer.drain(..end).collect();
                return Ok(self.packet(data));
            }
            if self.eof_reached {
                // End of file ends the last frame; trailing bytes without a
                // picture are dropped, as FFmpeg's parser does.
                if self.pic_found && !self.buffer.is_empty() {
                    self.pic_found = false;
                    self.scan = 0;
                    let data = std::mem::take(&mut self.buffer);
                    return Ok(self.packet(data));
                }
                self.buffer_pos += self.buffer.len() as u64;
                self.buffer.clear();
                return Err(Error::Eof);
            }
            let n = self.input.read(&mut chunk).map_err(Error::Io)?;
            self.allowance.spend(1, 0)?;
            if n == 0 {
                self.eof_reached = true;
            } else {
                if self.buffer.len() + n > 16 * 1024 * 1024 {
                    return Err(Error::invalid("vc1: frame exceeded 16 MiB"));
                }
                self.buffer.extend_from_slice(&chunk[..n]);
            }
        }
    }

    /// vc1dec.c is FF_DEF_RAWVIDEO_DEMUXER2 with AVFMT_GENERIC_INDEX:
    /// seek.c seek_frame_generic over the I pictures the parser flags.
    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        seek_generic(self, pts)
    }
}

// ───────────────────────── Registration ─────────────────────────

fn open_vc1(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(Vc1Demuxer::open(input)?))
}

fn open_vc1test(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(Vc1TestDemuxer::open(input)?))
}

pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("vc1", open_vc1);
    reg.register_probe("vc1", probe_vc1);
    reg.register_extension("vc1", "vc1");

    reg.register_demuxer("vc1test", open_vc1test);
    reg.register_probe("vc1test", probe_vc1test);
    reg.register_extension("rcv", "vc1test");
}
