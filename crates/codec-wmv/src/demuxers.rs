// Ported from FFmpeg libavformat/vc1dec.c, libavformat/vc1test.c, and
// libavcodec/vc1_parser.c (commit 2da55bf).
// License: LGPL-2.1-or-later.

use std::io::{Read, Seek, SeekFrom};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, ProbeData, ProbeScore, ReadSeek, Result, StreamInfo,
    TimeBase, PROBE_SCORE_EXTENSION,
};

/// Codec id of the VC-1 Advanced Profile streams the `vc1` demuxer emits.
pub const CODEC_ID_VC1: &str = "vc1";
/// Codec id of the WMV3 (VC-1 Simple/Main) streams in `.rcv` files.
pub const CODEC_ID_WMV3: &str = "wmv3";

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

        Ok(Self {
            input,
            streams: vec![stream],
            fps,
            pts: 0,
        })
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
        let mut hdr = [0u8; 8];
        match self.input.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::Eof),
            Err(e) => return Err(Error::Io(e)),
        }

        let frame_size = (hdr[0] as usize) | ((hdr[1] as usize) << 8) | ((hdr[2] as usize) << 16);
        let keyframe = (hdr[3] & 0x80) != 0;
        let file_pts = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);

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
        Ok(pkt)
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

pub struct Vc1Demuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    buffer: Vec<u8>,
    eof_reached: bool,
    pts: i64,
}

impl Vc1Demuxer {
    pub fn open(mut input: Box<dyn ReadSeek>) -> Result<Self> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 16384];
        let n = input.read(&mut chunk).map_err(Error::Io)?;
        buffer.extend_from_slice(&chunk[..n]);

        let mut width = None;
        let mut height = None;

        // Try to parse sequence header from initial buffer
        for i in 0..buffer.len().saturating_sub(10) {
            if buffer[i] == 0 && buffer[i + 1] == 0 && buffer[i + 2] == 1 && buffer[i + 3] == 0x0F {
                let b = &buffer[i + 4..];
                if b.len() >= 6 {
                    let w_val = ((b[2] as u32) << 4) | ((b[3] as u32) >> 4);
                    let h_val = (((b[3] as u32) & 0x0F) << 8) | (b[4] as u32);
                    width = Some((w_val + 1) * 2);
                    height = Some((h_val + 1) * 2);
                }
                break;
            }
        }

        let time_base = TimeBase::new(1, 25);
        let mut params = CodecParameters::video(CodecId::new(CODEC_ID_VC1));
        params.width = width;
        params.height = height;

        let stream = StreamInfo {
            index: 0,
            params,
            time_base,
            duration: None,
            start_time: Some(0),
        };

        Ok(Self {
            input,
            streams: vec![stream],
            buffer,
            eof_reached: n == 0,
            pts: 0,

        })
    }

    /// Finds the next start-code boundary that begins a new frame.
    /// FFmpeg parser logic: once `pic_found` is true (we have seen a 0x0D or 0x0C),
    /// any subsequent start code OTHER than FIELD (0x0C), SLICE (0x0B), or ENDOFSEQ (0x0A)
    /// ends the current frame and begins the next.
    fn find_next_frame_boundary(&self) -> Option<usize> {
        let mut i = 0;
        let mut pic_seen = false;

        while i + 4 <= self.buffer.len() {
            if self.buffer[i] == 0 && self.buffer[i + 1] == 0 && self.buffer[i + 2] == 1 {
                let code = self.buffer[i + 3];
                if !pic_seen {
                    if code == 0x0D || code == 0x0C {
                        pic_seen = true;
                    }
                    i += 4;
                } else if code != 0x0C && code != 0x0B && code != 0x0A {
                    return Some(i);
                } else {
                    i += 4;
                }
            } else {
                i += 1;
            }
        }
        None
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
            if let Some(boundary) = self.find_next_frame_boundary() {
                let packet_data = self.buffer.drain(..boundary).collect();
                let pts = self.pts;
                self.pts += 1;
                let mut pkt = Packet {
                    stream_index: 0,
                    time_base: self.streams[0].time_base,
                    pts: Some(pts),
                    dts: Some(pts),
                    duration: Some(1),
                    flags: Default::default(),
                    data: packet_data,
                };
                if pts == 0 {
                    pkt.flags.keyframe = true;
                }
                return Ok(pkt);
            }

            if self.eof_reached {
                if !self.buffer.is_empty() {
                    let packet_data = std::mem::take(&mut self.buffer);
                    let pts = self.pts;
                    self.pts += 1;
                    let mut pkt = Packet {
                        stream_index: 0,
                        time_base: self.streams[0].time_base,
                        pts: Some(pts),
                        dts: Some(pts),
                        duration: Some(1),
                        flags: Default::default(),
                        data: packet_data,
                    };
                    if pts == 0 {
                        pkt.flags.keyframe = true;
                    }
                    return Ok(pkt);
                }
                return Err(Error::Eof);
            }

            let n = match self.input.read(&mut chunk) {
                Ok(0) => {
                    self.eof_reached = true;
                    0
                }
                Ok(n) => n,
                Err(e) => return Err(Error::Io(e)),
            };
            if n > 0 {
                if self.buffer.len() + n > 16 * 1024 * 1024 {
                    return Err(Error::invalid("vc1: frame exceeded 16 MiB"));
                }
                self.buffer.extend_from_slice(&chunk[..n]);
            }
        }
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
