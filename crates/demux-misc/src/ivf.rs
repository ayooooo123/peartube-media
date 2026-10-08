// Ported from FFmpeg libavformat/ivfdec.c, the key-frame rules of
// libavcodec/vp8_parser.c, vp9_parser.c and av1_parser.c (over the CBS
// read in av1_cbs.rs), and libavformat/seek.c seek_frame_generic
// (commit 2da55bf).
// License: LGPL-2.1-or-later
//
// On2 IVF demuxer: 32-byte file header (DKIF magic, version, header size,
// fourcc, width, height, rate, scale, frame count), then per frame a
// 12-byte header (4-byte size, 8-byte pts) followed by the payload. The
// fourcc resolves through the codec resolver (VP80/VP90/AV01); timestamps
// are in the rate/scale time base FFmpeg derives from the header. FFmpeg
// runs the codec's parser on each frame (AVSTREAM_PARSE_HEADERS), which
// decides the key flag, and seeks by the generic index of key frames.

use std::io::{Read, Seek, SeekFrom};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, ProbeContext, ProbeData, ProbeScore, ReadSeek, Result, StreamInfo,
    TimeBase, CodecTag, MAX_PROBE_SCORE,
};

use crate::av1_cbs::Av1Parser;
use demux_seek_core::{read_on, Allowance, Index};

const IVF_FILE_HEADER: usize = 32;
const IVF_FRAME_HEADER: usize = 12;
/// Untrusted-input cap: a frame header may claim huge sizes.
const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

pub fn probe_ivf(probe: &ProbeData) -> ProbeScore {
    let p = probe.buf;
    if p.len() < 10 {
        return 0;
    }
    // AV_RL32 == MKTAG('D','K','I','F') little-endian
    if p[0] == b'D' && p[1] == b'K' && p[2] == b'I' && p[3] == b'F'
        && u16::from_le_bytes([p[4], p[5]]) == 0
        && u16::from_le_bytes([p[6], p[7]]) == 32
    {
        // FFmpeg: AVPROBE_SCORE_MAX - 2
        MAX_PROBE_SCORE - 2
    } else {
        0
    }
}

struct IvfDemuxer {
    input: Box<dyn ReadSeek>,
    stream: StreamInfo,
    /// Bytes left in the data file (None = unlimited).
    left: Option<u64>,
    keys: KeyParser,
    /// AVFMT_GENERIC_INDEX: the key frames returned so far.
    index: Index,
    /// What the seek under way may still read.
    allowance: Allowance,
}

/// The parser FFmpeg runs on the stream, as far as its key flag:
/// parse_packet flags key when key_frame is 1, or still -1 (av_parser_init)
/// with pict_type still I. FFmpeg makes a new parser after every seek.
enum KeyParser {
    /// vp8_parser.c and vp9_parser.c: the last frame they parsed decides,
    /// none yet is a key frame (key_frame -1, pict_type I).
    Vpx { vp9: bool, key_frame: Option<bool> },
    /// av1_parser.c over its coded bitstream context.
    Av1(Box<Av1Parser>),
    /// No parser this port knows: every frame is a key frame.
    Other,
}

impl KeyParser {
    fn new(codec: &CodecId) -> Self {
        match codec.as_str() {
            "vp8" => Self::Vpx { vp9: false, key_frame: None },
            "vp9" => Self::Vpx { vp9: true, key_frame: None },
            "av1" => Self::Av1(Box::default()),
            _ => Self::Other,
        }
    }

    fn key(&mut self, frame: &[u8]) -> bool {
        match self {
            Self::Vpx { vp9: false, key_frame } => {
                // vp8_parser.c: frame_type, when profile <= 3 and at least
                // 3 bytes
                if frame.len() >= 3 && (frame[0] >> 1) & 7 <= 3 {
                    *key_frame = Some(frame[0] & 1 == 0);
                }
                key_frame.unwrap_or(true)
            }
            Self::Vpx { vp9: true, key_frame } => {
                // vp9_parser.c: frame marker, profile (a third bit for 3),
                // show_existing_frame, frame_type; past the end reads zeros.
                if !frame.is_empty() {
                    let bit = |n: usize| frame.get(n / 8).map_or(0, |b| (b >> (7 - n % 8)) & 1);
                    let mut profile = bit(2) | (bit(3) << 1);
                    let mut at = 4;
                    if profile == 3 {
                        profile += bit(4);
                        at = 5;
                    }
                    if profile <= 3 {
                        *key_frame = Some(bit(at) == 0 && bit(at + 1) == 0);
                    }
                }
                key_frame.unwrap_or(true)
            }
            Self::Av1(parser) => parser.key(frame),
            Self::Other => true,
        }
    }
}

/// Resolve an IVF fourcc to a codec id the same way FFmpeg's
/// `ff_codec_get_id(ff_codec_bmp_tags, tag)` does for the IVF subset:
/// ask the codec resolver for the FourCC, with known raw-IVF fallbacks.
fn ivf_codec_id(codecs: &dyn CodecResolver, tag: [u8; 4]) -> Option<CodecId> {
    let fourcc = CodecTag::fourcc(&tag);
    let ctx = ProbeContext::new(&fourcc);
    if let Some(id) = codecs.resolve_tag(&ctx) {
        return Some(id);
    }
    // FFmpeg's bmp table maps VP80→vp8, VP90→vp9, AV01→av1.
    match &tag {
        b"VP80" => Some(CodecId::new("vp8")),
        b"VP90" => Some(CodecId::new("vp9")),
        b"AV01" => Some(CodecId::new("av1")),
        _ => None,
    }
}

pub fn open_ivf(
    mut input: Box<dyn ReadSeek>,
    codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let mut head = [0u8; IVF_FILE_HEADER];
    input.read_exact(&mut head)?;
    if &head[0..4] != b"DKIF" {
        return Err(Error::invalid("ivf: bad magic"));
    }
    let tag = [head[8], head[9], head[10], head[11]];
    let width = u16::from_le_bytes([head[12], head[13]]) as u32;
    let height = u16::from_le_bytes([head[14], head[15]]) as u32;
    let rate = u32::from_le_bytes([head[16], head[17], head[18], head[19]]);
    let scale = u32::from_le_bytes([head[20], head[21], head[22], head[23]]);
    let nb_frames = u32::from_le_bytes([head[24], head[25], head[26], head[27]]) as i64;

    if rate == 0 || scale == 0 {
        return Err(Error::invalid("ivf: invalid frame rate"));
    }

    let codec_id = ivf_codec_id(codecs, tag)
        .ok_or_else(|| Error::codec_not_found(format!("ivf: unknown codec tag {tag:?}")))?;

    let mut params = CodecParameters::video(codec_id);
    params.width = Some(width);
    params.height = Some(height);
    params.tag = Some(CodecTag::fourcc(&tag));
    // Cap declared dimensions like other demuxers in this crate do.
    if width > 16384 || height > 16384 || u64::from(width) * u64::from(height) > 8192 * 8192 {
        return Err(Error::invalid("ivf: dimensions exceed limits"));
    }

    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::new(i64::from(scale), i64::from(rate)),
        duration: Some(nb_frames),
        start_time: Some(0),
    };

    let keys = KeyParser::new(&stream.params.codec_id);
    let allowance = Allowance::default();
    Ok(Box::new(IvfDemuxer {
        input: Box::new(allowance.meter(input)),
        stream,
        left: None,
        keys,
        index: Index::default(),
        allowance,
    }))
}

impl IvfDemuxer {
    /// One frame and where its frame header starts (the packet position
    /// FFmpeg indexes).
    fn read_frame(&mut self) -> Result<(Packet, i64)> {
        if let Some(left) = self.left
            && left == 0 {
                return Err(Error::Eof);
            }
        let pos = self.input.stream_position()? as i64;
        let mut fh = [0u8; IVF_FRAME_HEADER];
        match self.input.read(&mut fh) {
            Ok(0) => return Err(Error::Eof),
            Ok(n) if n < IVF_FRAME_HEADER => {
                // header itself truncated: nothing decodable follows
                return Err(Error::Eof);
            }
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
        let size = u32::from_le_bytes([fh[0], fh[1], fh[2], fh[3]]) as usize;
        let pts = i64::from_le_bytes([
            fh[4], fh[5], fh[6], fh[7], fh[8], fh[9], fh[10], fh[11],
        ]);
        if size > MAX_FRAME_SIZE {
            return Err(Error::invalid("ivf: frame size exceeds maximum"));
        }
        self.allowance.spend(1, 0)?;
        if let Some(left) = &mut self.left {
            *left = left.saturating_sub((IVF_FRAME_HEADER + size) as u64);
        }
        let mut data = vec![0u8; size];
        // FFmpeg's av_get_packet tolerates a short final read and still
        // returns the partial packet; mirror that (FATE files are cut).
        let mut got = 0usize;
        while got < size {
            let n = self.input.read(&mut data[got..])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        data.truncate(got);
        if data.is_empty() {
            return Err(Error::Eof);
        }
        let mut pkt = Packet {
            stream_index: 0,
            time_base: self.stream.time_base,
            pts: Some(pts),
            dts: Some(pts),
            duration: None,
            flags: Default::default(),
            data,
        };
        pkt.flags.keyframe = self.keys.key(&pkt.data);
        Ok((pkt, pos))
    }

    /// Reads on from `pos` with a new parser (ff_read_frame_flush).
    fn restart(&mut self, pos: i64) -> Result<()> {
        self.input.seek(SeekFrom::Start(pos as u64))?;
        self.keys = KeyParser::new(&self.stream.params.codec_id);
        Ok(())
    }

    /// seek_frame_generic from the index search's result `found`.
    fn land(&mut self, timestamp: i64, mut found: Option<usize>) -> Result<i64> {
        if found.is_none() || found == Some(self.index.entries().len() - 1) {
            let from = self.index.entries().last().map_or(IVF_FILE_HEADER as i64, |e| e.pos);
            self.restart(from)?;
            read_on(timestamp, || self.next_packet().map(|p| (p.flags.keyframe, p.dts)))?;
            found = self.index.search(timestamp, true);
        }
        let Some(i) = found else {
            return Err(Error::invalid("ivf: no key frame to seek to"));
        };
        let e = self.index.entries()[i];
        self.restart(e.pos)?;
        Ok(e.timestamp)
    }
}

impl Demuxer for IvfDemuxer {
    fn format_name(&self) -> &str {
        "ivf"
    }

    fn streams(&self) -> &[StreamInfo] {
        std::slice::from_ref(&self.stream)
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let (packet, pos) = self.read_frame()?;
        // av_read_frame indexes every key packet it returns.
        if packet.flags.keyframe {
            self.index.add(pos, pts_of(&packet), 0, 0, true);
        }
        Ok(packet)
    }

    /// seek.c seek_frame_generic with AVSEEK_FLAG_BACKWARD (ivfdec.c:
    /// AVFMT_GENERIC_INDEX): the last key frame at or before the target
    /// among those returned so far; past the last of them frames are read
    /// on, within the seek's allowance, until a key frame starts after
    /// the target or, as FFmpeg gives up, more than 1000 others did. A
    /// seek that fails leaves reading where it was.
    fn seek_to(&mut self, _stream_index: u32, timestamp: i64) -> Result<i64> {
        let found = self.index.search(timestamp, true);
        if found.is_none() && self.index.entries().first().is_some_and(|e| timestamp < e.timestamp) {
            return Err(Error::invalid("ivf: seek before the first key frame"));
        }
        let at = self.input.stream_position()?;
        let keys = std::mem::replace(&mut self.keys, KeyParser::new(&self.stream.params.codec_id));
        let left = self.left;
        self.allowance.start();
        let landed = self.land(timestamp, found);
        let landed = self.allowance.finish(landed);
        if landed.is_err() {
            self.input.seek(SeekFrom::Start(at))?;
            (self.keys, self.left) = (keys, left);
        }
        landed
    }
}

/// Every frame header carries its pts, the packet's dts too.
fn pts_of(packet: &Packet) -> i64 {
    packet.dts.unwrap_or(i64::MIN)
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("ivf", open_ivf);
    reg.register_probe("ivf", probe_ivf);
    reg.register_extension("ivf", "ivf");
}
