// Ported from FFmpeg libavformat/mpeg.c (commit 2da55bf).
// License: LGPL-2.1-or-later
//
// MPEG-1/2 program stream demuxer (.mpg/.mpeg/.vob). Streams are created
// on the fly the way FFmpeg does (AVFMTCTX_NOHEADER); private stream 1 is
// split by substream id into AC-3 / DTS / LPCM (with FFmpeg's raw-AC3
// detection), the program stream map overrides elementary stream types,
// and subpicture streams map to the DVD subtitle codec. The PES parser
// follows mpegps_read_pes_header: MPEG-1 stuffing, buffer scale/size,
// MPEG-1 PTS/DTS, MPEG-2 PES flags and the PES extension 2 stream-id
// remap.

use std::io::{Read, Seek, SeekFrom};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    MediaType, Packet, ProbeData, ProbeScore, ReadSeek, Result, StreamInfo, TimeBase,
    PROBE_SCORE_EXTENSION,
};

const PACK_START_CODE: u32 = 0x000001BA;
const SYSTEM_HEADER_START_CODE: u32 = 0x000001BB;
const PROGRAM_STREAM_MAP: u32 = 0x000001BC;
const PRIVATE_STREAM_1: u32 = 0x000001BD;
const PADDING_STREAM: u32 = 0x000001BE;
const PRIVATE_STREAM_2: u32 = 0x000001BF;

const MAX_PES_PAYLOAD: i64 = 64 * 1024 * 1024; // untrusted-input cap

/// ff_parse_pes_pts (mpeg.h)
fn parse_pes_pts(buf: &[u8]) -> i64 {
    (i64::from(buf[0] & 0x0E) << 29)
        | ((i64::from(u16::from_be_bytes([buf[1], buf[2]])) >> 1) << 15)
        | i64::from(u16::from_be_bytes([buf[3], buf[4]]) >> 1)
}

/// check_pes from mpeg.c's probe: does the bytes after this start code
/// look like a PES header?
fn check_pes(p: &[u8]) -> bool {
    if p.len() < 5 {
        return false;
    }
    let pes2 = (p[3] & 0xC0) == 0x80
        && (p[4] & 0xC0) != 0x40
        && ((p[4] & 0xC0) == 0x00 || (p[4] & 0xC0) >> 2 == (p.get(6).copied().unwrap_or(0) & 0xF0));
    if pes2 {
        return true;
    }
    let mut idx = 3;
    while idx < p.len() && p[idx] == 0xFF {
        idx += 1;
    }
    if idx + 2 <= p.len() && (p[idx] & 0xC0) == 0x40 {
        idx += 2;
    }
    if idx < p.len() {
        if (p[idx] & 0xE0) == 0x20 || (p[idx] & 0xF0) == 0x30 {
            return true;
        }
        if p[idx] == 0xF {
            return idx + 1 < p.len() && (p[idx + 1] & 6) == 2;
        }
    }
    false
}

/// ISO/IEC 13818-1 table 2-35 program stream map (mpegps_psm_parse).
/// Returns stream-id → PES stream-type pairs, or `None` on a malformed map.
fn parse_psm(data: &[u8]) -> Option<Vec<(u8, u8)>> {
    if data.len() < 10 {
        return None;
    }
    let psm_length = u16::from_be_bytes([data[0], data[1]]) as usize;
    let ps_info_length = u16::from_be_bytes([data[4], data[5]]) as usize;
    let es_map_len = psm_length.checked_sub(ps_info_length + 10)?;
    if 6 + ps_info_length + 2 > data.len() {
        return None;
    }
    let mut out = Vec::new();
    let mut pos = 6 + ps_info_length + 2; // past es_map_length field
    let mut remaining = es_map_len;
    while remaining >= 4 && pos + 4 <= data.len() {
        let es_type = data[pos];
        let es_id = data[pos + 1];
        let es_info_length = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        pos += 4 + es_info_length;
        remaining = remaining.saturating_sub(4 + es_info_length);
        out.push((es_id, es_type));
    }
    Some(out)
}

/// FFmpeg's mpegps_probe, on the probe buffer.
pub fn probe_mpegps(probe: &ProbeData) -> ProbeScore {
    let p = probe.buf;
    if p.len() < 16 {
        return 0;
    }

    let mut code: u32 = 0xFFFF_FFFF;
    let (mut sys, mut pspack, mut priv1, mut vid, mut audio, mut invalid) = (0, 0, 0, 0, 0, 0);
    let mut i = 0usize;
    while i < p.len() {
        code = (code << 8) | u32::from(p[i]);
        if (code & 0xFFFF_FF00) == 0x100 {
            let pes = check_pes(&p[i..]);
            let pack =
                i + 1 < p.len() && ((p[i + 1] & 0xC0) == 0x40 || (p[i + 1] & 0xF0) == 0x20);

            if code == SYSTEM_HEADER_START_CODE {
                sys += 1;
            } else if code == PACK_START_CODE && pack {
                pspack += 1;
            } else if (0x1E0..=0x1EF).contains(&code) {
                if pes {
                    vid += 1;
                } else {
                    invalid += 1;
                }
            } else if (0x1C0..=0x1DF).contains(&code) {
                if pes {
                    audio += 1;
                } else {
                    invalid += 1;
                }
            } else if code == PRIVATE_STREAM_1 {
                if pes {
                    priv1 += 1;
                } else {
                    invalid += 1;
                }
            }
        }
        i += 1;
    }

    if sys > invalid && sys * 9 <= pspack * 10 {
        if audio > 12 || vid > 3 || pspack > 2 {
            PROBE_SCORE_EXTENSION + 2
        } else {
            PROBE_SCORE_EXTENSION / 2 + 1
        }
    } else if pspack > invalid && (priv1 + vid + audio) * 10 >= pspack * 9 {
        if pspack > 2 {
            PROBE_SCORE_EXTENSION + 2
        } else {
            PROBE_SCORE_EXTENSION / 2
        }
    } else if (vid > 0 || audio > 0 || priv1 > 0)
        && probe
            .ext
            .is_some_and(|e| e == "mpg" || e == "mpeg" || e == "vob")
    {
        PROBE_SCORE_EXTENSION
    } else {
        0
    }
}

/// Which codec a private-stream-1 substream id carries
/// (mpegps_read_packet's 0x80..=0xcf ladder).
fn priv1_codec(sub_id: u8) -> (&'static str, bool) {
    match sub_id {
        0x80..=0x87 => ("ac3", false),
        0x88..=0x8F | 0x98..=0x9F => ("dts", false),
        0xA0..=0xAF => ("pcm_dvd", false),
        0xB0..=0xBF => ("truehd", false),
        0xC0..=0xCF => ("ac3", false),
        _ => ("ac3", false),
    }
}

/// Codec for a PSM PES stream type (STREAM_TYPE_* from mpeg.h).
fn psm_codec(es_type: u8) -> Option<(&'static str, MediaType)> {
    Some(match es_type {
        0x01 | 0x02 => ("mpeg2video", MediaType::Video),
        0x03 | 0x04 => ("mp3", MediaType::Audio),
        0x0F => ("aac", MediaType::Audio),
        0x10 => ("mpeg4", MediaType::Video),
        0x1B => ("h264", MediaType::Video),
        0x24 => ("hevc", MediaType::Video),
        0x81 => ("ac3", MediaType::Audio),
        0x82 => ("dts", MediaType::Audio),
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq)]
enum PsKind {
    Audio,
    Video,
    Subtitle,
}

struct PsStream {
    id: u32,
    sub_id: Option<u8>,
    index: u32,
}

pub struct MpegPsDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    states: Vec<PsStream>,
    psm_es_type: Vec<u8>,
}

impl MpegPsDemuxer {
    /// Create the stream for a start code (+ optional substream id),
    /// mirroring mpegps_read_packet's codec ladder.
    fn stream_for(&mut self, startcode: u32, sub_id: Option<u8>) -> u32 {
        if let Some(s) = self
            .states
            .iter()
            .find(|s| s.id == startcode && s.sub_id == sub_id)
        {
            return s.index;
        }

        let psm_type = sub_id
            .and_then(|sid| self.psm_es_type.get(sid as usize).copied())
            .filter(|_| startcode == PRIVATE_STREAM_1);

        let (codec, kind) = if let Some(t) = psm_type.and_then(psm_codec) {
            (t.0, match t.1 {
                MediaType::Video => PsKind::Video,
                MediaType::Audio => PsKind::Audio,
                MediaType::Subtitle => PsKind::Subtitle,
                MediaType::Data | MediaType::Unknown => PsKind::Video,
            })
        } else if startcode == PRIVATE_STREAM_1 {
            if let Some(sid) = sub_id {
                if (0x20..=0x3F).contains(&sid) {
                    // DVD subpicture substream (mpegps_read_packet's
                    // 0x20..=0x3f → dvd_subtitle).
                    ("dvdsub", PsKind::Subtitle)
                } else {
                    let (c, _) = priv1_codec(sid);
                    (c, PsKind::Audio)
                }
            } else {
                ("ac3", PsKind::Audio)
            }
        } else if (0x1E0..=0x1EF).contains(&startcode) {
            ("mpeg2video", PsKind::Video)
        } else if (0x1C0..=0x1DF).contains(&startcode) {
            ("mp2", PsKind::Audio)
        } else if (0x80..=0x87).contains(&startcode) || (0xC0..=0xCF).contains(&startcode) {
            ("ac3", PsKind::Audio)
        } else if (0x88..=0x8F).contains(&startcode) || (0x98..=0x9F).contains(&startcode) {
            ("dts", PsKind::Audio)
        } else if (0x20..=0x3F).contains(&startcode) {
            ("dvdsub", PsKind::Subtitle)
        } else if startcode == PRIVATE_STREAM_2 {
            ("dvdnav", PsKind::Subtitle)
        } else {
            // 0x1FD and friends: FFmpeg probes; carry as MPEG video.
            ("mpeg2video", PsKind::Video)
        };

        let index = self.streams.len() as u32;
        let codec_id = CodecId::new(codec);
        let params = match kind {
            PsKind::Video => CodecParameters::video(codec_id),
            PsKind::Audio => CodecParameters::audio(codec_id),
            PsKind::Subtitle => CodecParameters::subtitle(codec_id),
        };
        self.streams.push(StreamInfo {
            index,
            params,
            time_base: TimeBase::new(1, 90000),
            duration: None,
            start_time: Some(0),
        });
        self.states.push(PsStream {
            id: startcode,
            sub_id,
            index,
        });
        index
    }

    /// mpegps_read_pes_header: scan to the next PES packet. Returns
    /// `(startcode, sub_id, consumed_prefix, len, pts, dts)` where
    /// `consumed_prefix` are payload bytes already read (raw-AC3
    /// detection / the priv1 substream byte).
    #[allow(clippy::type_complexity)]
    fn read_pes_header(&mut self) -> Result<(u32, Option<u8>, Vec<u8>, i64, Option<i64>, Option<i64>)> {
        let mut code: u32 = 0xFFFF_FFFF;
        'pes_scan: loop {
            // find next start code
            let mut b = [0u8; 1];
            loop {
                match self.input.read(&mut b) {
                    Ok(0) => return Err(Error::Eof),
                    Ok(_) => {}
                    Err(e) => return Err(e.into()),
                }
                code = (code << 8) | u32::from(b[0]);
                if (code & 0xFFFF_FF00) == 0x100 {
                    break;
                }
            }
            let mut startcode = code;

            // container-level packets we skip by length
            if startcode == PACK_START_CODE {
                // mpeg.c's read_pes_header does not parse the pack body:
                // it just resyncs with find_next_start_code, which lands
                // right after the 12-byte (MPEG-1) / 14-byte (MPEG-2) pack
                // header. Skip the same amount: 8/10 bytes remain after the
                // 4-byte start code we consumed.
                let mut pack_head = [0u8; 1];
                self.input.read_exact(&mut pack_head)?;
                if (pack_head[0] & 0xC0) == 0x40 {
                    // MPEG-2: SCR(6) + mux rate(3) + padding(1) = 10 more
                    let mut rest = [0u8; 9];
                    self.input.read_exact(&mut rest)?;
                    self.input.seek(SeekFrom::Current(i64::from(rest[8] & 7)))?;
                } else if (pack_head[0] & 0xF0) == 0x20 {
                    // MPEG-1: SCR(5) + mux rate(3) = 8 more
                    let mut rest = [0u8; 7];
                    self.input.read_exact(&mut rest)?;
                } else {
                    // unknown: resync like find_next_start_code would
                    code = 0xFFFF_FFFF;
                    continue;
                }
                code = 0xFFFF_FFFF;
                continue;
            } else if startcode == SYSTEM_HEADER_START_CODE
                || startcode == PADDING_STREAM
                || startcode == PRIVATE_STREAM_2
            {
                let mut len_buf = [0u8; 2];
                self.input.read_exact(&mut len_buf)?;
                let len = u16::from_be_bytes(len_buf) as i64;
                if !(0..=MAX_PES_PAYLOAD).contains(&len) {
                    return Err(Error::invalid("mpegps: oversized packet"));
                }
                self.input.seek(SeekFrom::Current(len))?;
                code = 0xFFFF_FFFF;
                continue;
            } else if startcode == PROGRAM_STREAM_MAP {
                let mut len_buf = [0u8; 2];
                self.input.read_exact(&mut len_buf)?;
                let len = u16::from_be_bytes(len_buf) as usize;
                if len > MAX_PES_PAYLOAD as usize {
                    return Err(Error::invalid("mpegps: PSM too large"));
                }
                let mut body = vec![0u8; len];
                self.input.read_exact(&mut body)?;
                if let Some(map) = parse_psm(&body) {
                    self.psm_es_type = vec![0u8; 256];
                    for (es_id, es_type) in map {
                        self.psm_es_type[es_id as usize] = es_type;
                    }
                }
                code = 0xFFFF_FFFF;
                continue;
            }

            if !is_known_stream(startcode) {
                code = 0xFFFF_FFFF;
                continue;
            }

            let mut len_buf = [0u8; 2];
            self.input.read_exact(&mut len_buf)?;
            let mut len = i64::from(u16::from_be_bytes(len_buf));
            let mut pts = None;
            let mut dts = None;

            if startcode != PRIVATE_STREAM_2 {
                // stuffing
                let mut c: u32;
                loop {
                    if len < 1 {
                        // FFmpeg's error_redo: abandon this packet.
                        code = 0xFFFF_FFFF;
                        continue 'pes_scan;
                    }
                    let mut sb = [0u8; 1];
                    self.input.read_exact(&mut sb)?;
                    c = u32::from(sb[0]);
                    len -= 1;
                    if c != 0xFF {
                        break;
                    }
                }
                if (c & 0xC0) == 0x40 {
                    // buffer scale & size
                    let mut bb = [0u8; 2];
                    self.input.read_exact(&mut bb)?;
                    len -= 2;
                    c = u32::from(bb[1]);
                }
                if (c & 0xE0) == 0x20 {
                    // MPEG-1 PTS (c carries the first byte)
                    let mut ts = [0u8; 5];
                    ts[0] = c as u8;
                    self.input.read_exact(&mut ts[1..])?;
                    pts = Some(parse_pes_pts(&ts));
                    dts = pts;
                    len -= 4;
                    if c & 0x10 != 0 {
                        let mut ts2 = [0u8; 5];
                        self.input.read_exact(&mut ts2)?;
                        dts = Some(parse_pes_pts(&ts2));
                        len -= 5;
                    }
                } else if (c & 0xC0) == 0x80 {
                    // MPEG-2 PES
                    let mut fb = [0u8; 2];
                    self.input.read_exact(&mut fb)?;
                    let mut flags = u32::from(fb[0]);
                    let mut header_len = i64::from(fb[1]);
                    len -= 2;
                    if header_len > len {
                        code = 0xFFFF_FFFF;
                        continue;
                    }
                    len -= header_len;
                    if flags & 0x80 != 0 {
                        let mut ts = [0u8; 5];
                        self.input.read_exact(&mut ts)?;
                        dts = Some(parse_pes_pts(&ts));
                        pts = dts;
                        header_len -= 5;
                        if flags & 0x40 != 0 {
                            let mut ts2 = [0u8; 5];
                            self.input.read_exact(&mut ts2)?;
                            dts = Some(parse_pes_pts(&ts2));
                            header_len -= 5;
                        }
                    }
                    if flags & 0x3F != 0 && header_len == 0 {
                        flags &= 0xC0;
                    }
                    if flags & 0x01 != 0 {
                        // PES extension
                        if header_len < 1 {
                            code = 0xFFFF_FFFF;
                            continue;
                        }
                        let mut eb = [0u8; 1];
                        self.input.read_exact(&mut eb)?;
                        let mut pes_ext = eb[0];
                        header_len -= 1;
                        let mut skip = u32::from((pes_ext >> 4) & 0xB);
                        skip += skip & 0x9;
                        if pes_ext & 0x40 != 0 || i64::from(skip) > header_len {
                            pes_ext = 0;
                            skip = 0;
                        }
                        self.input.seek(SeekFrom::Current(i64::from(skip)))?;
                        header_len -= i64::from(skip);
                        if pes_ext & 0x01 != 0 {
                            // PES extension 2
                            if header_len < 2 {
                                code = 0xFFFF_FFFF;
                                continue;
                            }
                            let mut e2 = [0u8; 2];
                            self.input.read_exact(&mut e2)?;
                            let ext2_len = e2[0];
                            header_len -= 1;
                            if (ext2_len & 0x7F) > 0 {
                                let id_ext = e2[1];
                                header_len -= 1;
                                if id_ext & 0x80 == 0 {
                                    // stream-id remap (mpeg.c):
                                    // ((startcode & 0xff) << 8) | id_ext
                                    startcode = ((startcode & 0xFF) << 8) | u32::from(id_ext);
                                }
                            }
                        }
                    }
                    if header_len < 0 {
                        code = 0xFFFF_FFFF;
                        continue;
                    }
                    self.input.seek(SeekFrom::Current(header_len))?;
                } else if c != 0xF {
                    code = 0xFFFF_FFFF;
                    continue;
                }
            }

            // private stream 1: substream id leads the payload
            let mut sub_id: Option<u8> = None;
            let mut prefix: Vec<u8> = Vec::new();
            if startcode == PRIVATE_STREAM_1 {
                let mut sb = [0u8; 2];
                // read first payload byte, then peek at the second for
                // FFmpeg's raw-AC3 detection.
                self.input.read_exact(&mut sb[..1])?;
                if sb[0] == 0x0B {
                    self.input.read_exact(&mut sb[1..])?;
                    if sb[1] == 0x77 {
                        // raw AC-3: no substream header; both bytes are payload
                        prefix.extend_from_slice(&sb);
                        len -= 2;
                        sub_id = Some(0x80);
                    } else {
                        // not AC-3: both bytes are the (sub_id +) payload
                        prefix.extend_from_slice(&sb);
                        len -= 2;
                        sub_id = Some(sb[0]);
                    }
                } else {
                    len -= 1;
                    sub_id = Some(sb[0]);
                }
            }
            // Non-raw-AC3 private-stream-1 audio carries a substream header
            // the decoders do not expect (mpegps_read_packet "found:" path):
            // 0x80..0xCF: 3-byte header; 0xB0..0xBF (MLP): 4 bytes total;
            // 0xA0..0xAF (pcm_dvd): 3 bytes.
            if let Some(sid) = sub_id
                && (0x80..=0xCF).contains(&sid) {
                    let mut hdr = [0u8; 3];
                    self.input.read_exact(&mut hdr)?;
                    len -= 3;
                    if (0xB0..=0xBF).contains(&sid) {
                        let mut b = [0u8; 1];
                        self.input.read_exact(&mut b)?;
                        len -= 1;
                    }
                }
            if len < 0 {
                code = 0xFFFF_FFFF;
                continue;
            }
            return Ok((startcode, sub_id, prefix, len, pts, dts));
        }
    }
}

/// Stream ids mpegps_read_packet recognises.
fn is_known_stream(startcode: u32) -> bool {
    (0x1C0..=0x1DF).contains(&startcode)
        || (0x1E0..=0x1EF).contains(&startcode)
        || startcode == PRIVATE_STREAM_1
        || startcode == PRIVATE_STREAM_2
        || startcode == 0x1FD
        || (0x80..=0xCF).contains(&startcode)
}

pub fn open_mpegps(
    input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(MpegPsDemuxer {
        input,
        streams: Vec::new(),
        states: Vec::new(),
        psm_es_type: vec![0u8; 256],
    }))
}

impl Demuxer for MpegPsDemuxer {
    fn format_name(&self) -> &str {
        "mpeg"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let (startcode, sub_id, prefix, len, pts, dts) = self.read_pes_header()?;
        // Skip data-stream packets we cannot assign a codec to
        // (mpeg.c's `goto skip` path for unknown ids) — everything
        // accepted above maps to a stream.
        let idx = self.stream_for(startcode, sub_id);
        if len > MAX_PES_PAYLOAD {
            return Err(Error::invalid("mpegps: payload too large"));
        }
        let prefix_len = prefix.len();
        let mut data = prefix;
        data.reserve(len as usize);
        (&mut self.input).take(len as u64).read_to_end(&mut data)?;
        if data.len() < prefix_len + len as usize {
            return Err(Error::Eof);
        }
        let tb = self.streams[idx as usize].time_base;
        let mut pkt = Packet::new(idx, tb, data);
        pkt.pts = pts;
        pkt.dts = dts;
        pkt.flags.keyframe = true;
        Ok(pkt)
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
