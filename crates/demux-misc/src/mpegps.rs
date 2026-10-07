// Ported from FFmpeg libavformat/mpeg.c (commit 2da55bf), with the stream
// discovery and parser stage of libavformat/demux.c (find_stream_info,
// probe_codec, parse_packet, compute_pkt_fields), the timestamp seek of
// libavformat/seek.c (ff_seek_frame_binary, ff_read_frame_flush,
// avpriv_update_cur_dts) and the CVD/OGT subpicture substreams of VLC
// modules/demux/mpeg/ps.h (commit 2e358f3).
// License: LGPL-2.1-or-later
//
// MPEG-1/2 program stream demuxer (.mpg/.mpeg/.vob). The container
// declares no streams (FFmpeg's AVFMTCTX_NOHEADER), so opening reads
// ahead, at most FFmpeg's default probe size, and creates every stream
// it meets the way avformat_find_stream_info does; whatever that read is
// queued and played, and the stream list never changes afterwards. A
// stream first seen later is dropped.
//
// Packets are the PES payloads FFmpeg's demuxer returns: private stream
// 1 is split by substream id with FFmpeg's substream headers stripped,
// the program stream map types elementary streams, DVD navigation
// packets surface once DVD PCI/DSI structures are recognised. Four
// kinds of stream differ from FFmpeg's raw PES output:
// - MPEG audio and AC-3 / E-AC-3 come out as frames, as FFmpeg's
//   mpegaudio and ac3 parsers cut them, timed as FFmpeg's demuxer layer
//   times them (a frame without a PES timestamp follows the one before);
//   their decoders take one frame per packet.
// - H.264 comes out as the access units FFmpeg's h264 parser cuts, keyed
//   and timed as its parser and demuxer layer key and time them; its
//   decoder takes one access unit per packet. A stream probed as H.264
//   is parsed from its first packet, as FFmpeg holds the packets back
//   until the probe ends.
// - DVD subpictures (substreams 0x20-0x3f) are reassembled into whole
//   units across PES packets by FFmpeg's dvdsub parser, each unit keeping
//   the timestamps of its first PES.
// - CVD (substreams 0x00-0x03) and SVCD OGT (0x70) subpictures, which
//   FFmpeg skips, are carried as VLC carries them: the PES payload with
//   its leading substream id.
// Other video stays in PES payloads: its decoders take the elementary
// stream in pieces.

use std::collections::VecDeque;
use std::io::{BufReader, Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, MediaType,
    Packet, ProbeData, ProbeScore, ReadSeek, Result, SampleFormat, StreamInfo, TimeBase,
    PROBE_SCORE_EXTENSION,
};

use crate::parser::{mpa_decode_header, returned, Ac3, AudioClock, DvdSub, H264Clock, MpegAudio, Parser, Unit};
use crate::rawvideo::{Units, H264};
use demux_seek_core::{gen_search, Allowance, Index};

const PACK_START_CODE: u32 = 0x1BA;
const SYSTEM_HEADER_START_CODE: u32 = 0x1BB;
const PROGRAM_STREAM_MAP: u32 = 0x1BC;
const PRIVATE_STREAM_1: u32 = 0x1BD;
const PADDING_STREAM: u32 = 0x1BE;
const PRIVATE_STREAM_2: u32 = 0x1BF;

/// mpeg.c MAX_SYNC_SIZE: how far one search for a start code reads.
const MAX_SYNC_SIZE: i64 = 100_000;
/// FFmpeg's default probesize: input bytes read at open to find streams.
const PROBE_SIZE: u64 = 5_000_000;
/// avformat_find_stream_info's analyze durations in 90 kHz ticks: 5 s
/// once every stream has its parameters, else MPEG's 7 s per stream, or
/// 30 s for a subtitle stream.
const ANALYZE_ALL: i64 = 5 * 90_000;
const ANALYZE_STREAM: i64 = 7 * 90_000;
const ANALYZE_SUBTITLE: i64 = 30 * 90_000;
/// Elementary-stream bytes kept per stream while discovering, from which
/// its identity and parameters come.
const HEAD_BYTES: usize = 1 << 20;

const TIME_BASE: TimeBase = TimeBase::new(1, 90_000);

/// ff_parse_pes_pts (mpeg.h)
fn parse_pes_pts(buf: &[u8; 5]) -> i64 {
    (i64::from(buf[0] & 0x0E) << 29)
        | ((i64::from(u16::from_be_bytes([buf[1], buf[2]])) >> 1) << 15)
        | i64::from(u16::from_be_bytes([buf[3], buf[4]]) >> 1)
}

/// `p[i]`, or 0 past the end (FFmpeg's probe buffers carry zero padding).
fn at(p: &[u8], i: usize) -> u8 {
    p.get(i).copied().unwrap_or(0)
}

/// check_pes (mpeg.c), with `i` at the start code's last byte.
fn check_pes(p: &[u8], i: usize) -> bool {
    let pes2 = (at(p, i + 3) & 0xC0) == 0x80
        && (at(p, i + 4) & 0xC0) != 0x40
        && ((at(p, i + 4) & 0xC0) == 0x00 || (at(p, i + 4) & 0xC0) >> 2 == (at(p, i + 6) & 0xF0));
    let mut q = i + 3;
    while q < p.len() && p[q] == 0xFF {
        q += 1;
    }
    if (at(p, q) & 0xC0) == 0x40 {
        q += 2;
    }
    let pes1 = if (at(p, q) & 0xF0) == 0x20 {
        at(p, q) & at(p, q + 2) & at(p, q + 4) & 1 != 0
    } else if (at(p, q) & 0xF0) == 0x30 {
        at(p, q) & at(p, q + 2) & at(p, q + 4) & at(p, q + 5) & at(p, q + 7) & at(p, q + 9) & 1 != 0
    } else {
        at(p, q) == 0x0F
    };
    pes1 || pes2
}

/// mpegps_probe.
pub fn probe_mpegps(probe: &ProbeData) -> ProbeScore {
    let p = probe.buf;
    let mut code: u32 = 0xFFFF_FFFF;
    let (mut sys, mut pspack, mut priv1, mut vid, mut audio, mut invalid) = (0, 0, 0, 0, 0, 0);
    let mut endpes = 0usize;
    let mut i = 0usize;
    while i < p.len() {
        code = (code << 8).wrapping_add(u32::from(p[i]));
        if (code & 0xFFFF_FF00) == 0x100 {
            let len = (usize::from(at(p, i + 1)) << 8) | usize::from(at(p, i + 2));
            let pes = endpes <= i && check_pes(p, i);
            let pack = (at(p, i + 1) & 0xC0) == 0x40 || (at(p, i + 1) & 0xF0) == 0x20;
            if code == SYSTEM_HEADER_START_CODE {
                sys += 1;
            } else if code == PACK_START_CODE && pack {
                pspack += 1;
            } else if (code & 0xF0) == 0xE0 && pes {
                endpes = i + len;
                vid += 1;
            } else if (code & 0xE0) == 0xC0 && pes {
                // skip the payload: no start code emulation from audio
                audio += 1;
                i += len;
            } else if code == PRIVATE_STREAM_1 && pes {
                priv1 += 1;
                i += len;
            } else if code == 0x1FD && pes {
                vid += 1; // VC-1
            } else if ((code & 0xF0) == 0xE0 || (code & 0xE0) == 0xC0 || code == PRIVATE_STREAM_1) && !pes {
                invalid += 1;
            }
        }
        i += 1;
    }

    let score = if vid + audio > invalid + 1 { PROBE_SCORE_EXTENSION / 2 } else { 0 };
    if sys > invalid && sys * 9 <= pspack * 10 {
        return if audio > 12 || vid > 3 || pspack > 2 {
            PROBE_SCORE_EXTENSION + 2
        } else {
            PROBE_SCORE_EXTENSION / 2 + u8::from(audio + vid + pspack > 1)
        };
    }
    if pspack > invalid && (priv1 + vid + audio) * 10 >= pspack * 9 {
        return if pspack > 2 { PROBE_SCORE_EXTENSION + 2 } else { PROBE_SCORE_EXTENSION / 2 };
    }
    if ((vid > 0) ^ (audio > 0)) && (audio > 4 || vid > 1) && sys == 0 && pspack == 0 && p.len() > 2048 && vid + audio > invalid {
        // PES stream
        return if audio > 12 || vid > 6 + 2 * invalid {
            PROBE_SCORE_EXTENSION + 1
        } else {
            PROBE_SCORE_EXTENSION / 2
        };
    }
    score
}

/// How a stream's packets reach the caller.
enum Framing {
    /// One packet per PES payload, as FFmpeg's demuxer returns it.
    Pes,
    /// CVD/OGT: the PES payload behind its substream id byte.
    SubstreamId(u8),
    /// DVD subpicture units reassembled by the dvdsub parser.
    Spu(Parser<DvdSub>),
    /// MPEG audio frames from the mpegaudio parser.
    Mpa(Parser<MpegAudio>, AudioClock),
    /// AC-3 / E-AC-3 frames from the ac3 parser.
    Ac3(Parser<Ac3>, AudioClock),
    /// H.264 access units from the h264 parser.
    H264(Parser<H264>, H264Clock),
}

struct Track {
    /// FFmpeg's stream id: the start code, or the private stream 1
    /// substream id.
    id: u32,
    /// FFmpeg's codec name; `None` while a video stream awaits probing.
    codec: Option<&'static str>,
    media: MediaType,
    /// The first elementary-stream bytes (discovery only).
    head: Vec<u8>,
    /// Probe sizes already tried (FFmpeg probes at each power of two).
    probed_at: usize,
    probe_done: bool,
    /// Its parameters are known (has_codec_parameters).
    ready: bool,
    first_ts: Option<i64>,
    start_time: Option<i64>,
    framing: Framing,
    /// The dts of the stream's PES headers read (mpegps_read_pes_header
    /// indexes each), which bound a seek's search.
    index: Index,
    /// While a video stream's codec is probed: where its PES payloads
    /// queued so far start, for the parser that takes them once the
    /// probe settles on H.264.
    probing: Vec<i64>,
}

/// One PES header: its stream id after private-stream-1 / extension
/// remapping, payload length still to read, timestamps, file position.
struct PesHeader {
    startcode: u32,
    len: i64,
    pts: Option<i64>,
    dts: Option<i64>,
    pos: i64,
}

/// How far reading for the next PES got.
enum Next<T> {
    Found(T),
    /// The end of the input.
    End,
    /// Discovery's input budget ran out first, or a search for a start
    /// code read MAX_SYNC_SIZE bytes without one (FFERROR_REDO); reading
    /// resumes where it stopped.
    Budget,
}

pub struct MpegPsDemuxer {
    input: BufReader<Box<dyn ReadSeek>>,
    streams: Vec<StreamInfo>,
    tracks: Vec<Track>,
    psm_es_type: [u8; 256],
    /// mpeg.c's sofdec: 1 Sofdec, -1 not, 0 not known yet.
    sofdec: i32,
    dvd: bool,
    imkh_cctv: bool,
    raw_ac3: bool,
    discovering: bool,
    /// While discovering: the input position no scan for a start code
    /// passes.
    limit: Option<i64>,
    /// PES payload bytes delivered while discovering (FFmpeg's read_size).
    probed: u64,
    queue: VecDeque<Packet>,
    eof: bool,
    /// Where the packs start (FFmpeg's data_offset, past an IMKH or
    /// Sofdec signature).
    data_offset: i64,
    /// What the seek under way may still read.
    allowance: Allowance,
}

impl MpegPsDemuxer {
    fn byte(&mut self) -> Result<Option<u8>> {
        let mut b = [0u8; 1];
        loop {
            match self.input.read(&mut b) {
                Ok(0) => return Ok(None),
                Ok(_) => return Ok(Some(b[0])),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Fill `buf` as far as the input goes; returns the bytes read.
    fn read_up_to(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut got = 0;
        while got < buf.len() {
            match self.input.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(got)
    }

    /// avio_rb16; `None` at the end of the input.
    fn rb16(&mut self) -> Result<Option<i64>> {
        let mut b = [0u8; 2];
        Ok((self.read_up_to(&mut b)? == 2).then(|| i64::from(u16::from_be_bytes(b))))
    }

    fn skip(&mut self, n: i64) -> Result<()> {
        self.input.seek_relative(n)?;
        Ok(())
    }

    fn position(&mut self) -> Result<i64> {
        Ok(self.input.stream_position()? as i64)
    }

    /// get_pts: the 5-byte timestamp starting with `first`, if 4 more
    /// bytes follow.
    fn get_pts(&mut self, first: Option<u8>) -> Result<Option<i64>> {
        let mut buf = [0u8; 5];
        let start = match first {
            Some(c) => {
                buf[0] = c;
                1
            }
            None => 0,
        };
        let need = 5 - start;
        Ok((self.read_up_to(&mut buf[start..])? == need).then(|| parse_pes_pts(&buf)))
    }

    /// mpegps_read_header: IMKH CCTV and Sofdec signatures.
    fn read_header(&mut self) -> Result<()> {
        let last_pos = self.position()?;
        let mut buffer = [0u8; 6];
        let n = self.read_up_to(&mut buffer)?;
        // avio_get_str stops after a NUL
        let used = buffer[..n].iter().position(|&b| b == 0).map_or(n, |z| z + 1);
        let text = &buffer[..used.min(n)];
        if text.starts_with(b"IMKH") {
            self.imkh_cctv = true;
            self.input.seek(SeekFrom::Start((last_pos + used as i64) as u64))?;
        } else if text.starts_with(b"Sofdec") {
            self.sofdec = 1;
            self.input.seek(SeekFrom::Start((last_pos + used as i64) as u64))?;
        } else {
            self.input.seek(SeekFrom::Start(last_pos as u64))?;
        }
        Ok(())
    }

    /// mpegps_psm_parse (ISO/IEC 13818-1 table 2-35).
    fn psm_parse(&mut self) -> Result<()> {
        let Some(psm_length) = self.rb16()? else { return Ok(()) };
        self.skip(2)?;
        let Some(ps_info_length) = self.rb16()? else { return Ok(()) };
        self.skip(ps_info_length)?;
        if self.rb16()?.is_none() {
            return Ok(());
        }
        // es_map_length is ignored: FFmpeg trusts psm_length
        let mut es_map_length = psm_length - ps_info_length - 10;
        while es_map_length >= 4 {
            let mut entry = [0u8; 4];
            if self.read_up_to(&mut entry)? < 4 {
                return Ok(());
            }
            let es_info_length = i64::from(u16::from_be_bytes([entry[2], entry[3]]));
            self.psm_es_type[usize::from(entry[1])] = entry[0];
            self.skip(es_info_length)?;
            es_map_length -= 4 + es_info_length;
        }
        self.skip(4) // crc32
    }

    /// The private stream 2 packet behind its start code, when it is not a
    /// DVD navigation packet to deliver: mpeg.c's Sofdec / DVD detection.
    /// Returns true when the packet is to be parsed as a stream packet.
    fn private_stream_2(&mut self) -> Result<bool> {
        if self.sofdec == 0 {
            let Some(len) = self.rb16()? else { return Ok(false) };
            let mut ps2buf = vec![0u8; len as usize];
            let read = self.read_up_to(&mut ps2buf)?;
            if read == ps2buf.len() {
                if len >= 6 {
                    if let Some(s) = ps2buf[..ps2buf.len() - 5].iter().position(|&b| b == b'S') {
                        self.sofdec = i32::from(&ps2buf[s + 1..s + 6] == b"ofdec");
                    }
                }
                if self.sofdec == 0 {
                    self.sofdec = -1;
                }
                if self.sofdec < 0 {
                    let bcd = |b: u8| u32::from(b >> 4) * 10 + u32::from(b & 0x0F);
                    let time_ok = |h: u8, m: u8, s: u8| {
                        bcd(h) <= 23 && bcd(m) <= 59 && bcd(s) <= 59 && (h & 0x0F) < 10 && (m & 0x0F) < 10 && (s & 0x0F) < 10
                    };
                    if len == 980 && ps2buf[0] == 0 {
                        // PCI structure?
                        let startpts = u32::from_be_bytes(ps2buf[0x0D..0x11].try_into().unwrap());
                        let endpts = u32::from_be_bytes(ps2buf[0x11..0x15].try_into().unwrap());
                        self.dvd = time_ok(ps2buf[0x19], ps2buf[0x1A], ps2buf[0x1B]) && endpts >= startpts;
                    } else if len == 1018 && ps2buf[0] == 1 {
                        // DSI structure?
                        self.dvd = time_ok(ps2buf[0x1D], ps2buf[0x1E], ps2buf[0x1F]);
                    }
                }
            }
            // Not a DVD packet: ignored. Otherwise back to its length field.
            if !self.dvd {
                return Ok(false);
            }
            self.skip(-(read as i64 + 2))?;
            Ok(true)
        } else if !self.dvd {
            if let Some(len) = self.rb16()? {
                self.skip(len)?;
            }
            Ok(false)
        } else {
            Ok(true)
        }
    }

    /// mpegps_read_pes_header. While discovering, the scan for a start
    /// code stops at the input budget, leaving any bytes that may begin
    /// one for playback to read again. `bounded` stops it after
    /// MAX_SYNC_SIZE bytes, where FFmpeg's scan returns FFERROR_REDO
    /// (which a timestamp search takes for no timestamp). Every PES with
    /// a dts goes into the index of the streams of its id.
    fn read_pes_header(&mut self, bounded: bool) -> Result<Next<PesHeader>> {
        let mut last_sync = self.position()?;
        let mut error_redo = false;
        loop {
            if error_redo {
                self.input.seek(SeekFrom::Start(last_sync as u64))?;
                error_redo = false;
            }
            let room = match self.limit {
                Some(limit) => limit - self.position()?,
                None => i64::MAX,
            };
            let room = if bounded { room.min(MAX_SYNC_SIZE) } else { room };
            // find_next_start_code
            let mut state: u32 = 0xFF;
            let mut scanned = 0i64;
            let mut startcode = loop {
                if scanned >= room {
                    self.allowance.spend(0, scanned as u64)?;
                    self.skip(-scanned.min(3))?;
                    return Ok(Next::Budget);
                }
                let Some(v) = self.byte()? else { return Ok(Next::End) };
                scanned += 1;
                if state == 0x000001 {
                    break 0x100 | u32::from(v);
                }
                state = ((state << 8) | u32::from(v)) & 0xFF_FFFF;
            };
            self.allowance.spend(1, scanned as u64)?;
            last_sync = self.position()?;

            match startcode {
                PACK_START_CODE | SYSTEM_HEADER_START_CODE => continue,
                PADDING_STREAM => {
                    if let Some(len) = self.rb16()? {
                        self.allowance.spend(0, len.max(0) as u64)?;
                        self.skip(len)?;
                    }
                    continue;
                }
                PRIVATE_STREAM_2 => {
                    if !self.private_stream_2()? {
                        continue;
                    }
                }
                PROGRAM_STREAM_MAP => {
                    self.psm_parse()?;
                    continue;
                }
                _ => {}
            }
            if !((0x1C0..=0x1DF).contains(&startcode)
                || (0x1E0..=0x1EF).contains(&startcode)
                || startcode == PRIVATE_STREAM_1
                || startcode == PRIVATE_STREAM_2
                || startcode == 0x1FD)
            {
                continue;
            }
            let pos = self.position()? - 4;
            let Some(mut len) = self.rb16()? else { return Ok(Next::End) };
            let mut pts = None;
            let mut dts = None;
            if startcode != PRIVATE_STREAM_2 {
                // stuffing
                let stuffed = loop {
                    if len < 1 {
                        break None;
                    }
                    let Some(b) = self.byte()? else { return Ok(Next::End) };
                    len -= 1;
                    if b != 0xFF {
                        break Some(b);
                    }
                };
                let Some(mut c) = stuffed else {
                    error_redo = true;
                    continue;
                };
                if (c & 0xC0) == 0x40 {
                    // buffer scale & size
                    self.skip(1)?;
                    let Some(b) = self.byte()? else { return Ok(Next::End) };
                    c = b;
                    len -= 2;
                }
                if (c & 0xE0) == 0x20 {
                    pts = self.get_pts(Some(c))?;
                    dts = pts;
                    len -= 4;
                    if c & 0x10 != 0 {
                        dts = self.get_pts(None)?;
                        len -= 5;
                    }
                } else if (c & 0xC0) == 0x80 {
                    // MPEG-2 PES
                    let Some(flags_byte) = self.byte()? else { return Ok(Next::End) };
                    let Some(header_len_byte) = self.byte()? else { return Ok(Next::End) };
                    let mut flags = flags_byte;
                    let mut header_len = i64::from(header_len_byte);
                    len -= 2;
                    if header_len > len {
                        error_redo = true;
                        continue;
                    }
                    len -= header_len;
                    if flags & 0x80 != 0 {
                        pts = self.get_pts(None)?;
                        dts = pts;
                        header_len -= 5;
                        if flags & 0x40 != 0 {
                            dts = self.get_pts(None)?;
                            header_len -= 5;
                        }
                    }
                    if flags & 0x3F != 0 && header_len == 0 {
                        flags &= 0xC0;
                    }
                    if flags & 0x01 != 0 {
                        // PES extension
                        let Some(mut pes_ext) = self.byte()? else { return Ok(Next::End) };
                        header_len -= 1;
                        // PES private data, pack header field, sequence
                        // counter, P-STD buffer
                        let mut skip = i64::from((pes_ext >> 4) & 0xB);
                        skip += skip & 0x9;
                        if pes_ext & 0x40 != 0 || skip > header_len {
                            pes_ext = 0;
                            skip = 0;
                        }
                        self.skip(skip)?;
                        header_len -= skip;
                        if pes_ext & 0x01 != 0 {
                            // PES extension 2
                            let Some(ext2_len) = self.byte()? else { return Ok(Next::End) };
                            header_len -= 1;
                            if (ext2_len & 0x7F) > 0 {
                                let Some(id_ext) = self.byte()? else { return Ok(Next::End) };
                                if id_ext & 0x80 == 0 {
                                    startcode = ((startcode & 0xFF) << 8) | u32::from(id_ext);
                                }
                                header_len -= 1;
                            }
                        }
                    }
                    if header_len < 0 {
                        error_redo = true;
                        continue;
                    }
                    self.skip(header_len)?;
                } else if c != 0x0F {
                    continue;
                }
            }

            if startcode == PRIVATE_STREAM_1 {
                let Some(sub) = self.byte()? else { return Ok(Next::End) };
                startcode = u32::from(sub);
                self.raw_ac3 = false;
                if sub == 0x0B {
                    let Some(second) = self.byte()? else { return Ok(Next::End) };
                    if second == 0x77 {
                        startcode = 0x80;
                        self.raw_ac3 = true;
                        self.skip(-2)?;
                    } else {
                        self.skip(-1)?;
                    }
                } else {
                    len -= 1;
                }
            }
            if len < 0 {
                error_redo = true;
                continue;
            }
            if let Some(dts) = dts {
                for track in self.tracks.iter_mut().filter(|t| t.id == startcode) {
                    track.index.add(pos, dts, 0, 0, true);
                }
            }
            return Ok(Next::Found(PesHeader { startcode, len, pts, dts, pos }));
        }
    }

    /// mpegps_read_dts: from `*pos` on, the dts of the first PES of stream
    /// `id` that has one, `*pos` moved to its start code; none when the
    /// input ends or a start code is more than MAX_SYNC_SIZE away first.
    fn read_dts(&mut self, pos: &mut i64, id: u32) -> Result<Option<i64>> {
        let Ok(at) = u64::try_from(*pos) else { return Ok(None) };
        self.input.seek(SeekFrom::Start(at))?;
        loop {
            let header = match self.read_pes_header(true)? {
                Next::Found(header) => header,
                Next::End | Next::Budget => return Ok(None),
            };
            if header.startcode == id && header.dts.is_some() {
                *pos = header.pos;
                return Ok(header.dts);
            }
            self.allowance.spend(0, header.len.max(0) as u64)?;
            self.skip(header.len)?;
        }
    }

    /// After a seek: reading resumes at `pos` (ff_read_frame_flush), each
    /// parser new, each audio clock at `ts` (avpriv_update_cur_dts; every
    /// stream has the same time base).
    fn restart(&mut self, pos: i64, ts: i64) -> Result<()> {
        self.input.seek(SeekFrom::Start(pos as u64))?;
        self.queue.clear();
        self.eof = false;
        for track in &mut self.tracks {
            // What the parsers set on the codec context (codec, sample
            // rate) outlives them.
            match &mut track.framing {
                Framing::Pes | Framing::SubstreamId(_) => {}
                Framing::Spu(parser) => *parser = Parser::new(DvdSub::default()),
                Framing::Mpa(parser, clock) => {
                    *parser = Parser::new(parser.split.reset());
                    clock.seeked(ts);
                }
                Framing::Ac3(parser, clock) => {
                    *parser = Parser::new(parser.split.reset());
                    clock.seeked(ts);
                }
                Framing::H264(parser, clock) => {
                    *parser = Parser::new(parser.split.fresh());
                    clock.seeked(ts);
                }
            }
        }
        Ok(())
    }
}

impl MpegPsDemuxer {
    /// mpegps_read_packet: reads PES packets until one belongs to a
    /// stream (creating streams while discovering) and queues what it
    /// yields: the stream and the packet's timestamp, the end of the
    /// input, or, while discovering, the input budget running out.
    fn read_packet(&mut self) -> Result<Next<(usize, Option<i64>)>> {
        loop {
            let PesHeader { startcode, mut len, pts, dts, pos } = match self.read_pes_header(false)? {
                Next::Found(header) => header,
                Next::End => return Ok(Next::End),
                Next::Budget => return Ok(Next::Budget),
            };
            // DVD-Video LPCM carries a dynamic range byte where DVD-Audio
            // LPCM and MLP do not: (pcm_dvd, pcm_dvda).
            let mut lpcm = (true, false);
            if (0x80..=0xCF).contains(&startcode) {
                if len < 4 {
                    self.skip(len)?;
                    continue;
                }
                if !self.raw_ac3 {
                    if (0xA0..=0xAF).contains(&startcode) {
                        if len < 6 {
                            self.skip(len)?;
                            continue;
                        }
                        let mut header = [0u8; 6];
                        if self.read_up_to(&mut header)? != 6 {
                            return Ok(Next::End);
                        }
                        self.skip(-6)?;
                        let pcm_dvd = header[5] == 0x80;
                        lpcm = (pcm_dvd, startcode == 0xA0 && !pcm_dvd);
                    } else {
                        // audio substream header
                        self.skip(3)?;
                        len -= 3;
                        if (0xB0..=0xBF).contains(&startcode) {
                            // MLP/TrueHD audio has a 4-byte header
                            self.skip(1)?;
                            len -= 1;
                        }
                    }
                }
            }

            let track = match self.tracks.iter().position(|t| t.id == startcode) {
                Some(track) => track,
                None if self.discovering => match self.new_track(startcode, len, lpcm)? {
                    Some(track) => track,
                    None => {
                        self.skip(len)?;
                        continue;
                    }
                },
                None => {
                    // a stream first met after open is not one of ours
                    self.skip(len)?;
                    continue;
                }
            };

            let codec = self.tracks[track].codec;
            if (0xA0..=0xAF).contains(&startcode) && !self.raw_ac3 {
                // Substream headers of codecs whose decoders do not expect
                // them; PCM_DVDA parses its header from the packet.
                let header_len = match codec {
                    Some("mlp") => 9,
                    Some("pcm_dvd") => 3,
                    _ => 0,
                };
                if len <= header_len {
                    self.skip(len)?;
                    continue;
                }
                self.skip(header_len)?;
                len -= header_len;
            } else if (0xA0..=0xAF).contains(&startcode) && codec == Some("mlp") {
                if len < 6 {
                    self.skip(len)?;
                    continue;
                }
                self.skip(6)?;
                len -= 6;
            }

            let mut data = vec![0u8; len as usize];
            let got = self.read_up_to(&mut data)?;
            if got == 0 && len > 0 {
                return Ok(Next::End);
            }
            data.truncate(got);
            let buffered = match &self.tracks[track].framing {
                Framing::Mpa(parser, _) => parser.split.buffered_bytes(),
                Framing::Ac3(parser, _) => parser.split.buffered_bytes(),
                _ => 0,
            };
            if buffered + data.len() > 8 * 1024 * 1024 {
                return Err(Error::invalid("mpeg: audio access unit exceeds 8 MiB"));
            }
            if let Framing::H264(parser, _) = &self.tracks[track].framing {
                if parser.split.buffered_bytes() + data.len() > 32 * 1024 * 1024 {
                    return Err(Error::invalid("mpeg: H.264 access unit exceeds 32 MiB"));
                }
            }
            self.deliver(track, data, pts, dts, pos);
            return Ok(Next::Found((track, dts.or(pts))));
        }
    }

    /// mpegps_read_packet's codec ladder for a stream id met for the
    /// first time, plus VLC's CVD/OGT substreams. `None` skips the packet.
    fn new_track(&mut self, startcode: u32, len: i64, (pcm_dvd, pcm_dvda): (bool, bool)) -> Result<Option<usize>> {
        use MediaType::{Audio, Data, Subtitle, Video};
        let es_type = self.psm_es_type[(startcode & 0xFF) as usize];
        let (codec, media) = match es_type {
            // FFmpeg types PSM MPEG-1 video as MPEG-2; the stream's headers
            // say which it is (see `mpeg_video`).
            0x01 | 0x02 => (Some("mpeg2video"), Video),
            0x03 | 0x04 => (Some("mp3"), Audio),
            0x0F => (Some("aac"), Audio),
            0x10 => (Some("mpeg4"), Video),
            0x1B => (Some("h264"), Video),
            0x24 => (Some("hevc"), Video),
            0x33 => (Some("vvc"), Video),
            0x81 => (Some("ac3"), Audio),
            0x90 => (Some("pcm_alaw"), Audio),
            0x91 if self.imkh_cctv => (Some("pcm_mulaw"), Audio),
            _ => match startcode {
                0x1E0..=0x1EF => {
                    // An AVS sequence header makes it CAVS; anything else
                    // is identified from its content (FFmpeg's
                    // request_probe).
                    let mut head = [0u8; 8];
                    let n = self.read_up_to(&mut head)?;
                    self.skip(-(n as i64))?;
                    let cavs = n == 8 && head[..4] == [0, 0, 1, 0xB0] && (head[6] != 0 || head[7] != 1);
                    (cavs.then_some("cavs"), Video)
                }
                PRIVATE_STREAM_2 => (Some("dvd_nav_packet"), Data),
                0x1C0..=0x1DF => {
                    let codec = if self.sofdec > 0 {
                        "adpcm_adx"
                    } else if self.imkh_cctv && startcode == 0x1C0 && len > 80 {
                        "pcm_alaw"
                    } else {
                        "mp2"
                    };
                    (Some(codec), Audio)
                }
                0x80..=0x87 | 0xC0..=0xCF => (Some("ac3"), Audio),
                // 0x90-0x97 is reserved for SDDS in DVD specs
                0x88..=0x8F | 0x98..=0x9F => (Some("dts"), Audio),
                0xA0..=0xAF => {
                    let codec = if pcm_dvda {
                        "pcm_dvda"
                    } else if !pcm_dvd {
                        "mlp"
                    } else {
                        "pcm_dvd"
                    };
                    (Some(codec), Audio)
                }
                0xB0..=0xBF => (Some("truehd"), Audio),
                0x20..=0x3F => (Some("dvd_subtitle"), Subtitle),
                0xFD55..=0xFD5F => (Some("vc1"), Video),
                0x69 | 0x49 => (Some("ivtv_vbi"), Subtitle),
                // VLC ps.h ps_track_fill: CVD and SVCD OGT subpictures,
                // passed on with their substream id.
                0x00..=0x03 => (Some("cvd_subtitle"), Subtitle),
                0x70 => (Some("ogt"), Subtitle),
                _ => return Ok(None),
            },
        };
        let framing = match codec {
            Some("dvd_subtitle") => Framing::Spu(Parser::new(DvdSub::default())),
            Some("cvd_subtitle" | "ogt") => Framing::SubstreamId(startcode as u8),
            Some(codec @ ("mp2" | "mp3")) => Framing::Mpa(Parser::new(MpegAudio::new(codec)), AudioClock::new(1, 90_000, 33)),
            Some("ac3") => Framing::Ac3(Parser::new(Ac3::new("ac3")), AudioClock::new(1, 90_000, 33)),
            Some("h264") => {
                let (parser, clock) = h264_parsing();
                Framing::H264(parser, clock)
            }
            _ => Framing::Pes,
        };
        self.tracks.push(Track {
            id: startcode,
            codec,
            media,
            head: Vec::new(),
            probed_at: 0,
            probe_done: codec.is_some(),
            ready: false,
            first_ts: None,
            start_time: None,
            framing,
            index: Index::default(),
            probing: Vec::new(),
        });
        Ok(Some(self.tracks.len() - 1))
    }

    /// Queue what one PES payload of `track` yields.
    fn deliver(&mut self, track: usize, data: Vec<u8>, pts: Option<i64>, dts: Option<i64>, pos: i64) {
        if self.discovering {
            let t = &mut self.tracks[track];
            self.probed += data.len() as u64;
            if t.start_time.is_none() {
                t.start_time = pts;
            }
            if t.head.len() < HEAD_BYTES {
                let take = (HEAD_BYTES - t.head.len()).min(data.len());
                t.head.extend_from_slice(&data[..take]);
            }
            if !t.probe_done {
                t.probe(false);
                if t.probe_done {
                    self.settle(track);
                }
            }
            let t = &mut self.tracks[track];
            if !t.ready {
                t.ready = t.params_ready();
            }
            if !t.probe_done {
                t.probing.push(pos);
            }
        }
        let t = &mut self.tracks[track];
        let index = track as u32;
        match &mut t.framing {
            Framing::Pes => self.queue.push_back(packet(index, data, pts, dts)),
            Framing::SubstreamId(id) => {
                let mut with_id = Vec::with_capacity(data.len() + 1);
                with_id.push(*id);
                with_id.extend_from_slice(&data);
                self.queue.push_back(packet(index, with_id, pts, dts));
            }
            Framing::Spu(parser) => {
                let mut units = Vec::new();
                parser.push(&data, pts, dts, pos, &mut units);
                self.queue.extend(units.into_iter().map(|u| unit_packet(index, u)));
            }
            Framing::Mpa(parser, clock) => {
                let mut units = Vec::new();
                parser.push(&data, pts, dts, pos, &mut units);
                stamp_all(units, clock, index, &mut self.queue);
            }
            Framing::Ac3(parser, clock) => {
                let mut units = Vec::new();
                parser.push(&data, pts, dts, pos, &mut units);
                stamp_all(units, clock, index, &mut self.queue);
            }
            Framing::H264(parser, clock) => {
                let mut units = Vec::new();
                parser.push(&data, pts, dts, pos, &mut units);
                stamp_h264(units, clock, index, &mut self.queue);
            }
        }
    }

    /// The probe of `track` ended (probe_codec): FFmpeg holds every packet
    /// from the probed stream's first on until then (ff_read_packet's
    /// raw_packet_buffer), then parses them in order. A stream probed as
    /// H.264 has its PES payloads queued so far cut into access units
    /// where they wait.
    fn settle(&mut self, track: usize) {
        let t = &mut self.tracks[track];
        let mut positions = std::mem::take(&mut t.probing).into_iter();
        if t.codec != Some("h264") || !matches!(t.framing, Framing::Pes) {
            return;
        }
        let (mut parser, mut clock) = h264_parsing();
        let index = track as u32;
        let mut units = Vec::new();
        for packet in std::mem::take(&mut self.queue) {
            if packet.stream_index != index {
                self.queue.push_back(packet);
                continue;
            }
            parser.push(&packet.data, packet.pts, packet.dts, positions.next().unwrap_or(-1), &mut units);
            stamp_h264(std::mem::take(&mut units), &mut clock, index, &mut self.queue);
        }
        self.tracks[track].framing = Framing::H264(parser, clock);
    }

    /// The end of the input: parsers hand over what they still hold.
    fn end_of_input(&mut self) {
        self.eof = true;
        for (index, t) in self.tracks.iter_mut().enumerate() {
            let index = index as u32;
            let mut units = Vec::new();
            match &mut t.framing {
                Framing::Spu(parser) => {
                    parser.flush(&mut units);
                    self.queue.extend(units.into_iter().map(|u| unit_packet(index, u)));
                }
                Framing::Mpa(parser, clock) => {
                    parser.flush(&mut units);
                    stamp_all(units, clock, index, &mut self.queue);
                }
                Framing::Ac3(parser, clock) => {
                    parser.flush(&mut units);
                    stamp_all(units, clock, index, &mut self.queue);
                }
                Framing::H264(parser, clock) => {
                    parser.flush(&mut units);
                    stamp_h264(units, clock, index, &mut self.queue);
                }
                Framing::Pes | Framing::SubstreamId(_) => {}
            }
        }
    }

    /// avformat_find_stream_info for a header-less container: read until
    /// the probe size, the end of the input, or a stream's timestamps span
    /// the analyze duration, then fix the streams. The probe size bounds
    /// two things on their own: the PES payload kept for playback (FFmpeg's
    /// read_size), and how far the search for packets goes into the input,
    /// scans and skips past input that yields none included (a PES whose
    /// start code lies within it is still read whole). Running out is not
    /// the end of the input: the parsers keep what they hold, and playback
    /// reads on from where discovery stopped.
    fn discover(&mut self) -> Result<()> {
        let budget_end = self.position()? + PROBE_SIZE as i64;
        self.limit = Some(budget_end);
        loop {
            if self.position()? >= budget_end || self.probed >= PROBE_SIZE {
                break;
            }
            let (track, ts) = match self.read_packet()? {
                Next::Found(read) => read,
                Next::End => {
                    self.finish_probes();
                    self.end_of_input();
                    break;
                }
                Next::Budget => break,
            };
            let Some(ts) = ts else { continue };
            let first = *self.tracks[track].first_ts.get_or_insert(ts);
            let limit = if self.tracks.iter().all(|t| t.ready) {
                ANALYZE_ALL
            } else if self.tracks[track].media == MediaType::Subtitle {
                ANALYZE_SUBTITLE
            } else {
                ANALYZE_STREAM
            };
            if ts - first >= limit {
                break;
            }
        }
        self.discovering = false;
        self.limit = None;
        self.finish_probes();
        self.streams = self.tracks.iter().enumerate().map(|(i, t)| t.stream_info(i as u32)).collect();
        for t in &mut self.tracks {
            t.head = Vec::new();
        }
        Ok(())
    }

    /// avformat_find_stream_info ends every probe still running with what
    /// it has (a forced probe_codec); a stream found to be H.264 is then
    /// parsed from its first packet.
    fn finish_probes(&mut self) {
        for track in 0..self.tracks.len() {
            if !self.tracks[track].probe_done {
                self.tracks[track].probe(true);
                self.tracks[track].probe_done = true;
                self.settle(track);
            }
        }
    }
}

/// The h264 parser and timing of an H.264 stream: time base (and
/// pkt_timebase) 1/90000, 33-bit timestamps.
fn h264_parsing() -> (Parser<H264>, H264Clock) {
    (Parser::new(H264::new((1, 90_000))), H264Clock::new(33))
}

fn packet(index: u32, data: Vec<u8>, pts: Option<i64>, dts: Option<i64>) -> Packet {
    let mut p = Packet::new(index, TIME_BASE, data);
    p.pts = pts;
    p.dts = dts;
    p.flags.keyframe = true;
    p
}

/// A parsed subtitle unit, stamped as compute_pkt_fields stamps a stream
/// without frame durations: dts follows pts.
fn unit_packet(index: u32, unit: Unit) -> Packet {
    let ts = unit.pts.or(unit.dts);
    packet(index, unit.data, ts, ts)
}

/// Parsed audio frames, timed by their stream's clock and queued.
fn stamp_all(units: Vec<Unit>, clock: &mut AudioClock, index: u32, queue: &mut VecDeque<Packet>) {
    for unit in units {
        let packet = clock.stamp(unit, index, TIME_BASE, queue);
        queue.push_back(packet);
    }
}

/// Parsed H.264 units, timed and queued.
fn stamp_h264(units: Vec<Unit>, clock: &mut H264Clock, index: u32, queue: &mut VecDeque<Packet>) {
    for unit in units {
        let packet = clock.stamp(unit, index, TIME_BASE, queue);
        queue.push_back(packet);
    }
}

/// floor(log2(x)), 0 for 0 (av_log2).
fn log2(x: usize) -> u32 {
    if x == 0 { 0 } else { usize::BITS - 1 - x.leading_zeros() }
}

/// set_codec_from_probe_data on elementary-stream bytes, for the video
/// formats it maps: the best probe score wins; a tie decides nothing.
fn probe_video_es(es: &[u8]) -> Option<(&'static str, u8)> {
    let probe = ProbeData { buf: es, ext: None };
    let scores = [
        ("h264", crate::h264::probe_h264(&probe)),
        ("hevc", crate::hevc::probe_hevc(&probe)),
        ("mpeg2video", crate::mpegvideo::probe_mpegvideo(&probe)),
    ];
    let best = scores.iter().map(|&(_, s)| s).max()?;
    let mut winners = scores.iter().filter(|&&(_, s)| s == best && s > 0);
    let winner = winners.next()?;
    winners.next().is_none().then_some((winner.0, best))
}

impl Track {
    /// probe_codec: probe the stream's bytes each time they reach a new
    /// power of two (and at the end); a score above
    /// AVPROBE_SCORE_STREAM_RETRY settles it, a lower one stands until a
    /// later probe replaces it.
    fn probe(&mut self, end: bool) {
        let size = self.head.len();
        if !end && log2(size) == log2(self.probed_at) {
            return;
        }
        self.probed_at = size;
        if let Some((codec, score)) = probe_video_es(&self.head) {
            self.codec = Some(codec);
            if score > 24 {
                self.probe_done = true;
            }
        }
    }

    /// has_codec_parameters, as far as this demuxer fills parameters.
    fn params_ready(&self) -> bool {
        match self.media {
            MediaType::Audio => {
                let p = self.parameters();
                p.sample_rate.is_some() && p.channels.is_some()
            }
            MediaType::Video => self.parameters().width.is_some(),
            _ => true,
        }
    }

    /// The stream's codec and parameters from its first bytes, as
    /// FFmpeg's parsers and decoders report them after find_stream_info.
    fn parameters(&self) -> CodecParameters {
        /// Headers come from the start of a stream; scans stop here.
        const AUDIO_SCAN: usize = 64 * 1024;
        const VIDEO_SCAN: usize = 256 * 1024;
        let codec = self.codec.unwrap_or("none");
        let id = CodecId::new(codec);
        let mut p = match self.media {
            MediaType::Video => CodecParameters::video(id),
            MediaType::Audio => CodecParameters::audio(id),
            MediaType::Subtitle => CodecParameters::subtitle(id),
            _ => CodecParameters::data(id),
        };
        let audio_head = &self.head[..self.head.len().min(AUDIO_SCAN)];
        let video_head = &self.head[..self.head.len().min(VIDEO_SCAN)];
        let audio: Option<(&'static str, u32, u16, Option<SampleFormat>)> = match codec {
            "mpeg2video" => {
                if let Some((codec, width, height)) = mpeg_video(video_head) {
                    p.codec_id = CodecId::new(codec);
                    p.width = Some(width);
                    p.height = Some(height);
                }
                None
            }
            "cavs" => {
                if let Some((width, height)) = cavs_sequence(video_head) {
                    p.width = Some(width);
                    p.height = Some(height);
                }
                None
            }
            "mp2" | "mp3" => mpeg_audio(audio_head).map(|(codec, rate, channels)| (codec, rate, channels, None)),
            "ac3" => ac3_frame(audio_head)
                .map(|h| (if h.bitstream_id > 10 { "eac3" } else { "ac3" }, h.sample_rate, h.channels, None)),
            "dts" => dts_core(audio_head).map(|(rate, channels)| ("dts", rate, channels, None)),
            // pcm_dvd_parse_header: quantization, frequency, channels
            "pcm_dvd" => audio_head.get(1).map(|&h| {
                const RATES: [u32; 4] = [48000, 96000, 44100, 32000];
                let format = if h >> 6 == 0 { SampleFormat::S16 } else { SampleFormat::S32 };
                ("pcm_dvd", RATES[usize::from((h >> 4) & 3)], 1 + u16::from(h & 7), Some(format))
            }),
            "pcm_dvda" => {
                pcm_dvda_layout(audio_head).map(|(rate, channels, format)| ("pcm_dvda", rate, channels, Some(format)))
            }
            "pcm_alaw" | "pcm_mulaw" => Some((codec, 8000, 1, None)),
            _ => None,
        };
        if let Some((codec, rate, channels, format)) = audio {
            p.codec_id = CodecId::new(codec);
            p.sample_rate = Some(rate);
            p.channels = Some(channels);
            p.sample_format = format;
        }
        p
    }

    fn stream_info(&self, index: u32) -> StreamInfo {
        StreamInfo {
            index,
            params: self.parameters(),
            time_base: TIME_BASE,
            duration: None,
            start_time: self.start_time,
        }
    }
}

/// The offset just past `00 00 01 code` in `es` at or after `from`.
fn start_code(es: &[u8], from: usize, code: impl Fn(u8) -> bool) -> Option<usize> {
    (from..es.len().saturating_sub(3))
        .find(|&i| es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 && code(es[i + 3]))
        .map(|i| i + 4)
}

/// FFmpeg's mpegvideo parser on the first sequence header: MPEG-1 unless
/// a sequence extension follows it, and the frame size including the
/// extension's high bits.
fn mpeg_video(es: &[u8]) -> Option<(&'static str, u32, u32)> {
    let seq = start_code(es, 0, |c| c == 0xB3)?;
    let b = es.get(seq..seq + 3)?;
    let mut width = (u32::from(b[0]) << 4) | u32::from(b[1] >> 4);
    let mut height = (u32::from(b[1] & 0x0F) << 8) | u32::from(b[2]);
    let mut codec = "mpeg1video";
    let mut at = seq;
    while let Some(next) = start_code(es, at, |_| true) {
        let code = es[next - 1];
        if code == 0xB5 && es.get(next).is_some_and(|b| b >> 4 == 1) {
            if let Some(ext) = es.get(next..next + 3) {
                width |= ((u32::from(ext[1]) & 1) << 13) | ((u32::from(ext[2]) & 0x80) << 5);
                height |= (u32::from(ext[2]) & 0x60) << 7;
                codec = "mpeg2video";
            }
            break;
        }
        if code == 0x00 || code == 0xB3 {
            break;
        }
        at = next;
    }
    Some((codec, width, height))
}

/// The AVS sequence header (cavsdec.c decode_seq_header): width and height.
fn cavs_sequence(es: &[u8]) -> Option<(u32, u32)> {
    let seq = start_code(es, 0, |c| c == 0xB0)?;
    let b = es.get(seq..seq + 6)?;
    // profile (8), level (8), progressive_sequence (1), width (14), height (14)
    let bits = u32::from_be_bytes([b[2], b[3], b[4], b[5]]);
    Some(((bits >> 17) & 0x3FFF, (bits >> 3) & 0x3FFF))
}

/// What FFmpeg's mpegaudio parser reports once two consecutive headers
/// agree (its header_count threshold): codec, sample rate, channels.
fn mpeg_audio(es: &[u8]) -> Option<(&'static str, u32, u16)> {
    // header + layer + frequency + lsf/mpeg25
    const SAME_HEADER_MASK: u32 = 0xFFE0_0000 | (3 << 17) | (3 << 10) | (3 << 19);
    let word = |i: usize| es.get(i..i + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    (0..es.len()).find_map(|i| {
        let h = word(i)?;
        let first = mpa_decode_header(h)?;
        let next = word(i + first.frame_bytes)?;
        mpa_decode_header(next)?;
        ((next & SAME_HEADER_MASK) == (h & SAME_HEADER_MASK)).then_some((first.codec, first.sample_rate, first.channels))
    })
}

/// The first AC-3/E-AC-3 syncframe whose CRC holds, which FFmpeg's ac3
/// parser takes the stream's parameters from.
fn ac3_frame(es: &[u8]) -> Option<crate::ac3::Ac3Header> {
    (0..es.len().saturating_sub(7)).find_map(|i| {
        if es[i] != 0x0B || es[i + 1] != 0x77 {
            return None;
        }
        let h = crate::ac3::parse_ac3_header(&es[i..])?;
        let frame = es.get(i..i + h.frame_size)?;
        (crate::ac3::crc16_ansi(&frame[2..]) == 0).then_some(h)
    })
}

/// The first DTS core frame header: FFmpeg's sample rate table and the
/// channels of its audio mode plus LFE.
fn dts_core(es: &[u8]) -> Option<(u32, u16)> {
    const RATES: [u32; 16] = [0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 96000, 192000];
    const AMODE: [u16; 10] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5];
    let sync = (0..es.len().saturating_sub(12)).find(|&i| es[i..i + 4] == [0x7F, 0xFE, 0x80, 0x01])?;
    let v = u64::from_be_bytes(es[sync + 4..sync + 12].try_into().ok()?);
    let amode = ((v >> 30) & 0x3F) as usize;
    let rate = RATES[((v >> 26) & 0xF) as usize];
    let lfe = (v >> 9) & 3;
    (rate != 0 && lfe != 3).then_some(())?;
    Some((rate, AMODE.get(amode)? + u16::from(lfe != 0)))
}

/// pcm_dvda_parse_header: sample rate, channels and sample format of a
/// DVD-Audio LPCM packet header (group 1 sets the rate).
fn pcm_dvda_layout(head: &[u8]) -> Option<(u32, u16, SampleFormat)> {
    const GROUPS: [(u16, u16); 21] = [
        (1, 0), (2, 0), (2, 1), (2, 2), (2, 1), (2, 2), (2, 3), (2, 1), (2, 2), (2, 3), (2, 2),
        (2, 3), (2, 4), (3, 1), (3, 2), (3, 1), (3, 2), (3, 3), (4, 1), (4, 1), (4, 2),
    ];
    let (quantization, frequency, assignment) = (*head.get(6)?, *head.get(7)?, *head.get(9)? & 0x1F);
    let (g1, mut g2) = *GROUPS.get(usize::from(assignment))?;
    let (quant1, freq1) = (quantization >> 4, frequency >> 4);
    let (quant2, freq2) = (quantization & 0xF, frequency & 0xF);
    if quant2 == 0xF || freq2 == 0xF {
        g2 = 0;
    }
    if quant1 > 2 || (freq1 & 7) > 2 || (g2 > 0 && (quant2 > 2 || (freq2 & 7) > 2)) {
        return None;
    }
    let rate = (if freq1 & 8 != 0 { 44100 } else { 48000 }) << (freq1 & 7);
    let bits = 16 + 4 * u32::from(quant1.max(if g2 > 0 { quant2 } else { 0 }));
    let format = if bits == 16 { SampleFormat::S16 } else { SampleFormat::S32 };
    Some((rate, g1 + g2, format))
}

pub fn open_mpegps(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut demuxer = MpegPsDemuxer {
        input: BufReader::with_capacity(64 * 1024, input),
        streams: Vec::new(),
        tracks: Vec::new(),
        psm_es_type: [0; 256],
        sofdec: 0,
        dvd: false,
        imkh_cctv: false,
        raw_ac3: false,
        discovering: true,
        limit: None,
        probed: 0,
        queue: VecDeque::new(),
        eof: false,
        data_offset: 0,
        allowance: Allowance::default(),
    };
    demuxer.read_header()?;
    demuxer.data_offset = demuxer.position()?;
    demuxer.discover()?;
    Ok(Box::new(demuxer))
}

impl Demuxer for MpegPsDemuxer {
    fn format_name(&self) -> &str {
        "mpeg"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            if let Some(mut packet) = self.queue.pop_front() {
                packet.pts = returned(packet.pts);
                packet.dts = returned(packet.dts);
                return Ok(packet);
            }
            if self.eof {
                return Err(Error::Eof);
            }
            if let Next::End = self.read_packet()? {
                self.end_of_input();
            }
        }
    }

    /// mpeg.c has no read_seek: FFmpeg bisects with its read_timestamp,
    /// mpegps_read_dts (seek.c ff_seek_frame_binary, ff_gen_search with
    /// AVSEEK_FLAG_BACKWARD), within the bounds the stream's index of PES
    /// dts gives. It lands on the last PES of the stream at or before the
    /// target, mid-GOP as FFmpeg's does. Every step reads at most up to
    /// the next timestamped PES of the stream, with at most MAX_SYNC_SIZE
    /// bytes between start codes, and the whole search within the seek's
    /// allowance. A search that fails leaves reading where it was.
    fn seek_to(&mut self, stream_index: u32, timestamp: i64) -> Result<i64> {
        let Some(track) = self.tracks.get(stream_index as usize) else {
            return Err(Error::invalid("mpeg: no such stream to seek"));
        };
        let (id, bounds) = (track.id, track.index.bounds(timestamp));
        let resume = self.position()?;
        let file_size = self.input.seek(SeekFrom::End(0))? as i64;
        let data_offset = self.data_offset;
        self.allowance.start();
        let found = gen_search(timestamp, bounds, data_offset, file_size, &mut |pos, _limit| self.read_dts(pos, id));
        self.allowance.stop();
        match found {
            Ok(Some((pos, ts))) => {
                self.restart(pos, ts)?;
                Ok(ts)
            }
            failed => {
                self.input.seek(SeekFrom::Start(resume as u64))?;
                Err(failed.err().unwrap_or_else(|| Error::invalid("mpeg: no timestamp to seek by")))
            }
        }
    }
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("mpeg", open_mpegps);
    reg.register_probe("mpeg", probe_mpegps);
    reg.register_extension("mpg", "mpeg");
    reg.register_extension("mpeg", "mpeg");
    reg.register_extension("vob", "mpeg");
    reg.register_extension("mpe", "mpeg");
}
