// Ported from FFmpeg libavformat/ivfdec.c (commit 2da55bf).
// License: LGPL-2.1-or-later
//
// On2 IVF demuxer: 32-byte file header (DKIF magic, version, header size,
// fourcc, width, height, rate, scale, frame count), then per frame a
// 12-byte header (4-byte size, 8-byte pts) followed by the payload. The
// fourcc resolves through the codec resolver (VP80/VP90/AV01); timestamps
// are in the rate/scale time base FFmpeg derives from the header.

use std::io::Read;
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, ProbeContext, ProbeData, ProbeScore, ReadSeek, Result, StreamInfo,
    TimeBase, CodecTag, MAX_PROBE_SCORE,
};

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

    Ok(Box::new(IvfDemuxer {
        input,
        stream,
        left: None,
    }))
}

impl IvfDemuxer {
    fn read_frame(&mut self) -> Result<Packet> {
        if let Some(left) = self.left
            && left == 0 {
                return Err(Error::Eof);
            }
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
        pkt.flags.keyframe = true;
        Ok(pkt)
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
        self.read_frame()
    }
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("ivf", open_ivf);
    reg.register_probe("ivf", probe_ivf);
    reg.register_extension("ivf", "ivf");
}
