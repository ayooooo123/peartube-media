// The `amr` and `qcp` demuxers.
//
// Ported from FFmpeg libavformat/amr.c (the `amr` demuxer), the framing of
// libavcodec/amr_parser.c that FFmpeg applies to its packets
// (AVSTREAM_PARSE_FULL_RAW), and libavformat/qcp.c (commit 2da55bf),
// LGPL-2.1-or-later.

//! - `amr`: the 3GPP storage format (RFC 4867 §5), `#!AMR\n` or
//!   `#!AMR-WB\n`, or their multichannel forms with a channel count. A
//!   packet is one frame per channel, each sized by its mode byte, as
//!   FFmpeg's AMR parser cuts them; at the end of the file it keeps what
//!   is left, as the parser's flush does. 160 (NB) or 320 (WB) samples
//!   per packet.
//! - `qcp`: RIFF `QLCM` files (RFC 3625) of QCELP-13K, EVRC, SMV or 4GV.
//!   Each packet in the `data` chunk is a rate byte, then as many bytes as
//!   the rate map (or the fixed packet size) gives that rate; the packet
//!   is those bytes without the rate byte. Unknown rates are skipped. 160
//!   samples per packet.
//!
//! Neither format has an index. FFmpeg seeks them by reading forward from
//! what it has read (its generic index search); here a seek reads forward
//! from the nearest packet start remembered so far, every 64th one.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecTag, ContainerRegistry, Demuxer, Error, Packet, ProbeData, ReadSeek, Result,
    StreamInfo, TimeBase, MAX_PROBE_SCORE,
};

/// AMR-NB frame sizes with the mode byte, per mode (`amrnb_packed_size`).
const AMRNB_PACKED_SIZE: [u8; 16] = [13, 14, 16, 18, 20, 21, 27, 32, 6, 1, 1, 1, 1, 1, 1, 1];
/// AMR-WB frame sizes with the mode byte (`amrwb_packed_size`).
const AMRWB_PACKED_SIZE: [u8; 16] = [18, 24, 33, 37, 41, 47, 51, 59, 61, 6, 1, 1, 1, 1, 1, 1];

const AMR_HEADER: &[u8] = b"#!AMR\n";
const AMRMC_HEADER: &[u8] = b"#!AMR_MC1.0\n";
const AMRWB_HEADER: &[u8] = b"#!AMR-WB\n";
const AMRWBMC_HEADER: &[u8] = b"#!AMR-WB_MC1.0\n";

/// The most channels a stream may declare.
const MAX_CHANNELS: u32 = 64;
/// Every how many packets a seek point is remembered.
const SEEK_POINT_EVERY: i64 = 64;

/// Up to `size` bytes, fewer at the end of the input.
fn read_up_to(input: &mut Box<dyn ReadSeek>, size: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    input.take(size).read_to_end(&mut data)?;
    Ok(data)
}

/// Exactly `N` bytes, or `None` at the end of the input.
fn read_array<const N: usize>(input: &mut Box<dyn ReadSeek>) -> Result<Option<[u8; N]>> {
    Ok(<[u8; N]>::try_from(read_up_to(input, N as u64)?).ok())
}

/// Packet starts remembered for seeking: (pts, byte position, packet
/// number), one every [`SEEK_POINT_EVERY`] packets, in order.
#[derive(Default)]
struct SeekPoints(Vec<(i64, u64, i64)>);

impl SeekPoints {
    fn note(&mut self, pts: i64, pos: u64, number: i64) {
        if number % SEEK_POINT_EVERY == 0 && self.0.last().is_none_or(|&(_, _, n)| n < number) {
            self.0.push((pts, pos, number));
        }
    }

    /// The last remembered start at or before `pts`.
    fn before(&self, pts: i64) -> Option<(i64, u64, i64)> {
        let at = self.0.partition_point(|&(p, _, _)| p <= pts);
        at.checked_sub(1).map(|i| self.0[i])
    }
}

// ───────────────────────── amr ─────────────────────────

/// `amr_probe`: "#!AMR" also starts the wideband and multichannel headers.
pub fn amr_probe(probe: &ProbeData) -> u8 {
    if probe.buf.starts_with(b"#!AMR") { MAX_PROBE_SCORE } else { 0 }
}

/// `amr_read_header`.
pub fn open_amr(mut input: Box<dyn ReadSeek>, _codecs: &dyn oxideav_core::CodecResolver) -> Result<Box<dyn Demuxer>> {
    // FFmpeg compares the first 19 bytes, zero past the end of a short file.
    let read = read_up_to(&mut input, 19)?;
    let mut header = [0u8; 19];
    header[..read.len()].copy_from_slice(&read);
    let le32 = |at: usize| u32::from_le_bytes(header[at..at + 4].try_into().unwrap());
    let (wideband, channels, header_len) = if header.starts_with(AMR_HEADER) {
        (false, 1, AMR_HEADER.len())
    } else if header.starts_with(AMRWB_HEADER) {
        (true, 1, AMRWB_HEADER.len())
    } else if header.starts_with(AMRMC_HEADER) {
        (false, le32(12), AMRMC_HEADER.len() + 4)
    } else if header.starts_with(AMRWBMC_HEADER) {
        (true, le32(15), AMRWBMC_HEADER.len() + 4)
    } else {
        return Err(Error::invalid("amr: no AMR header"));
    };
    if channels < 1 || channels > MAX_CHANNELS {
        return Err(Error::invalid(format!("amr: {channels} channels")));
    }
    let (codec, tag, sample_rate, frame_samples) =
        if wideband { ("amr_wb", b"sawb", 16_000, 320) } else { ("amr_nb", b"samr", 8_000, 160) };
    let mut params = CodecParameters::audio(CodecId::new(codec));
    params.sample_rate = Some(sample_rate);
    params.channels = Some(channels as u16);
    params.tag = Some(CodecTag::fourcc(tag));
    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::from_rate(sample_rate),
        duration: None,
        start_time: Some(0),
    };
    let data_start = header_len as u64;
    input.seek(SeekFrom::Start(data_start))?;
    Ok(Box::new(AmrDemuxer {
        input,
        stream,
        packed_size: if wideband { &AMRWB_PACKED_SIZE } else { &AMRNB_PACKED_SIZE },
        channels: channels as usize,
        frame_samples,
        data_start,
        pts: 0,
        number: 0,
        seek_points: SeekPoints::default(),
    }))
}

struct AmrDemuxer {
    input: Box<dyn ReadSeek>,
    stream: StreamInfo,
    packed_size: &'static [u8; 16],
    channels: usize,
    frame_samples: i64,
    data_start: u64,
    /// The next packet's pts and number.
    pts: i64,
    number: i64,
    seek_points: SeekPoints,
}

impl AmrDemuxer {
    /// One packet's bytes: a frame per channel, or what is left of them at
    /// the end of the file; `None` there when nothing is left.
    fn read_frames(&mut self) -> Result<Option<Vec<u8>>> {
        let mut data = Vec::new();
        for _ in 0..self.channels {
            let Some([mode]) = read_array::<1>(&mut self.input)? else { break };
            data.push(mode);
            let size = self.packed_size[usize::from(mode >> 3 & 0x0F)];
            let rest = read_up_to(&mut self.input, u64::from(size) - 1)?;
            let short = rest.len() + 1 < usize::from(size);
            data.extend_from_slice(&rest);
            if short {
                break;
            }
        }
        Ok((!data.is_empty()).then_some(data))
    }
}

impl Demuxer for AmrDemuxer {
    fn format_name(&self) -> &str {
        "amr"
    }

    fn streams(&self) -> &[StreamInfo] {
        std::slice::from_ref(&self.stream)
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let pos = self.input.stream_position()?;
        self.seek_points.note(self.pts, pos, self.number);
        let data = self.read_frames()?.ok_or(Error::Eof)?;
        let mut packet = Packet::new(0, self.stream.time_base, data);
        packet.pts = Some(self.pts);
        packet.dts = Some(self.pts);
        packet.duration = Some(self.frame_samples);
        packet.flags.keyframe = true;
        self.pts += self.frame_samples;
        self.number += 1;
        Ok(packet)
    }

    /// The packet starting at or before `pts`: read forward from the
    /// nearest remembered start, frame headers only.
    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        let target = pts.max(0);
        let (mut at_pts, pos, mut number) = self.seek_points.before(target).unwrap_or((0, self.data_start, 0));
        self.input.seek(SeekFrom::Start(pos))?;
        loop {
            let start = self.input.stream_position()?;
            self.seek_points.note(at_pts, start, number);
            if at_pts + self.frame_samples > target || self.read_frames()?.is_none() {
                self.input.seek(SeekFrom::Start(start))?;
                break;
            }
            at_pts += self.frame_samples;
            number += 1;
        }
        self.pts = at_pts;
        self.number = number;
        Ok(at_pts)
    }
}

// ───────────────────────── qcp ─────────────────────────

/// Bytes 1-15 of the QCELP-13K GUID; byte 0 is 0x41 or 0x42.
const GUID_QCELP_13K_PART: [u8; 15] =
    [0x6d, 0x7f, 0x5e, 0x15, 0xb1, 0xd0, 0x11, 0xba, 0x91, 0x00, 0x80, 0x5f, 0xb4, 0xb9, 0x7e];
const GUID_EVRC: [u8; 16] =
    [0x8d, 0xd4, 0x89, 0xe6, 0x76, 0x90, 0xb5, 0x46, 0x91, 0xef, 0x73, 0x6a, 0x51, 0x00, 0xce, 0xb4];
const GUID_4GV: [u8; 16] =
    [0xca, 0x29, 0xfd, 0x3c, 0x53, 0xf6, 0xf5, 0x4e, 0x90, 0xe9, 0xf4, 0x23, 0x6d, 0x59, 0x9b, 0x61];
const GUID_SMV: [u8; 16] =
    [0x75, 0x2b, 0x7c, 0x8d, 0x97, 0xa7, 0x49, 0xed, 0x98, 0x5e, 0xd5, 0x3c, 0x8c, 0xc7, 0x5f, 0x84];

const QCP_MAX_MODE: usize = 4;
/// Samples per QCP packet.
const QCP_FRAME_SAMPLES: i64 = 160;

/// `qcp_probe`
pub fn qcp_probe(probe: &ProbeData) -> u8 {
    let b = probe.buf;
    if b.len() >= 16 && &b[0..4] == b"RIFF" && &b[8..16] == b"QLCMfmt " { MAX_PROBE_SCORE } else { 0 }
}

/// `qcp_read_header`
pub fn open_qcp(mut input: Box<dyn ReadSeek>, _codecs: &dyn oxideav_core::CodecResolver) -> Result<Box<dyn Demuxer>> {
    let truncated = || Error::invalid("qcp: truncated header");
    // "RIFF", file size, "QLCMfmt ", chunk size, major and minor version,
    // then the codec GUID.
    let head = read_array::<38>(&mut input)?.ok_or_else(truncated)?;
    let guid: [u8; 16] = head[22..38].try_into().unwrap();
    let codec = if (guid[0] == 0x41 || guid[0] == 0x42) && guid[1..] == GUID_QCELP_13K_PART {
        "qcelp"
    } else if guid == GUID_EVRC {
        "evrc"
    } else if guid == GUID_SMV {
        "smv"
    } else if guid == GUID_4GV {
        "4gv"
    } else {
        return Err(Error::invalid("qcp: unknown codec GUID"));
    };
    // Codec version and name (82 bytes), average bit rate, packet size,
    // block size, sample rate, sample size, then the rate map's count.
    let fmt = read_array::<96>(&mut input)?.ok_or_else(truncated)?;
    let le16 = |at: usize| u16::from_le_bytes([fmt[at], fmt[at + 1]]);
    let packet_size = le16(84);
    let sample_rate = u32::from(le16(88));
    let nb_rates = u32::from_le_bytes(fmt[92..96].try_into().unwrap()).min(8) as usize;
    let mut rates_per_mode = [-1i16; QCP_MAX_MODE + 1];
    // The 8-entry rate map, then 20 reserved bytes.
    let map = read_array::<36>(&mut input)?.ok_or_else(truncated)?;
    for entry in map[..2 * nb_rates].chunks_exact(2) {
        let (size, mode) = (entry[0], usize::from(entry[1]));
        if mode <= QCP_MAX_MODE {
            rates_per_mode[mode] = i16::from(size);
        }
    }
    if sample_rate == 0 {
        return Err(Error::invalid("qcp: sample rate 0"));
    }
    let mut params = CodecParameters::audio(CodecId::new(codec));
    params.sample_rate = Some(sample_rate);
    params.channels = Some(1);
    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::from_rate(sample_rate),
        duration: None,
        start_time: Some(0),
    };
    let data_start = input.stream_position()?;
    Ok(Box::new(QcpDemuxer {
        input,
        stream,
        rates_per_mode,
        header_packet_size: packet_size,
        packet_size,
        data_size: 0,
        data_start,
        pts: 0,
        number: 0,
        seek_points: SeekPoints::default(),
    }))
}

struct QcpDemuxer {
    input: Box<dyn ReadSeek>,
    stream: StreamInfo,
    rates_per_mode: [i16; QCP_MAX_MODE + 1],
    /// The header's fixed packet size (0: variable rate).
    header_packet_size: u16,
    /// `s->packet_size`: 0 once a `vrat` chunk says the rate varies.
    packet_size: u16,
    /// Bytes left in the `data` chunk.
    data_size: u32,
    /// Where the chunks after the header start.
    data_start: u64,
    pts: i64,
    number: i64,
    seek_points: SeekPoints,
}

impl QcpDemuxer {
    /// `qcp_read_packet`'s loop: the next packet's bytes, with where its
    /// rate byte starts; `None` at the end of the input.
    fn read_payload(&mut self) -> Result<Option<(u64, Vec<u8>)>> {
        loop {
            if self.data_size > 0 {
                let start = self.input.stream_position()?;
                let Some([mode]) = read_array::<1>(&mut self.input)? else { return Ok(None) };
                let mut pkt_size = if self.packet_size != 0 {
                    i64::from(self.packet_size) - 1
                } else {
                    match self.rates_per_mode.get(usize::from(mode)).copied().filter(|&s| s >= 0) {
                        Some(size) => i64::from(size),
                        None => {
                            self.data_size -= 1;
                            continue;
                        }
                    }
                };
                if i64::from(self.data_size) <= pkt_size {
                    pkt_size = i64::from(self.data_size) - 1;
                }
                let data = read_up_to(&mut self.input, pkt_size.max(0) as u64)?;
                if data.is_empty() && pkt_size > 0 {
                    return Ok(None);
                }
                self.data_size = self.data_size.saturating_sub(pkt_size.max(0) as u32 + 1);
                return Ok(Some((start, data)));
            }
            // A chunk starts on an even offset.
            if self.input.stream_position()? & 1 == 1 && read_array::<1>(&mut self.input)?.is_none() {
                return Ok(None);
            }
            let Some(chunk) = read_array::<8>(&mut self.input)? else { return Ok(None) };
            let size = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
            match &chunk[0..4] {
                b"vrat" => {
                    let Some(vrat) = read_array::<8>(&mut self.input)? else { return Ok(None) };
                    if vrat[0..4] != [0; 4] {
                        self.packet_size = 0;
                    }
                }
                b"data" => self.data_size = size,
                _ => {
                    self.input.seek(SeekFrom::Current(i64::from(size)))?;
                }
            }
        }
    }

    /// Back to the first chunk after the header, as when the file opened.
    fn rewind(&mut self) -> Result<()> {
        self.input.seek(SeekFrom::Start(self.data_start))?;
        self.packet_size = self.header_packet_size;
        self.data_size = 0;
        self.pts = 0;
        self.number = 0;
        Ok(())
    }
}

impl Demuxer for QcpDemuxer {
    fn format_name(&self) -> &str {
        "qcp"
    }

    fn streams(&self) -> &[StreamInfo] {
        std::slice::from_ref(&self.stream)
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let (start, data) = self.read_payload()?.ok_or(Error::Eof)?;
        self.seek_points.note(self.pts, start, self.number);
        let mut packet = Packet::new(0, self.stream.time_base, data);
        packet.pts = Some(self.pts);
        packet.dts = Some(self.pts);
        packet.duration = Some(QCP_FRAME_SAMPLES);
        packet.flags.keyframe = true;
        self.pts += QCP_FRAME_SAMPLES;
        self.number += 1;
        Ok(packet)
    }

    /// The packet starting at or before `pts`, read forward from the start
    /// of the file (a remembered packet start only saves reading the
    /// chunk headers again when it lies in the same `data` chunk, so the
    /// chunk walk restarts from the header).
    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        let target = pts.max(0);
        self.rewind()?;
        loop {
            let state = (self.input.stream_position()?, self.packet_size, self.data_size);
            if self.pts + QCP_FRAME_SAMPLES > target || self.read_payload()?.is_none() {
                self.input.seek(SeekFrom::Start(state.0))?;
                (self.packet_size, self.data_size) = (state.1, state.2);
                break;
            }
            self.pts += QCP_FRAME_SAMPLES;
            self.number += 1;
        }
        Ok(self.pts)
    }
}

/// Registers both demuxers.
pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("amr", open_amr);
    reg.register_extension("amr", "amr");
    reg.register_probe("amr", amr_probe);

    reg.register_demuxer("qcp", open_qcp);
    reg.register_extension("qcp", "qcp");
    reg.register_probe("qcp", qcp_probe);
}
