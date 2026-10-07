// Ported from FFmpeg libavformat/cafdec.c and caf.c (commit 2da55bf).
// License: LGPL-2.1-or-later
//
// Core Audio Format demuxer: 'caff' header, a 'desc' audio description
// chunk, then 'kuki' (magic cookie / extradata), 'pakt' (packet table),
// 'chan', 'info' and 'data' chunks. Packets are framed by the packet
// table: fixed bytes_per_packet/frames_per_packet, or a per-packet table
// of (bytes, frames) lengths when variable. Timestamps are in samples.

use std::io::{Read, Seek, SeekFrom};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, ProbeData, ProbeScore, ReadSeek, Result, SampleFormat, StreamInfo,
    TimeBase, CodecTag, MAX_PROBE_SCORE,
};

const CAF_MAX_PKT_SIZE: usize = 4096;
/// Untrusted-input caps.
const MAX_CHUNK_SIZE: i64 = 256 * 1024 * 1024;
const MAX_PACKETS: i64 = 16 * 1024 * 1024;

/// ff_codec_caf_tags (caf.c): tags the player can resolve, keyed by the
/// bytes as they appear in the file (avio_rl32 order).
fn caf_codec_tag(tag: [u8; 4]) -> Option<&'static str> {
    match &tag {
        b"aac " | b"aacl" => Some("aac"),
        b"ac-3" => Some("ac3"),
        b"ima4" => Some("adpcm_ima_qt"),
        b"alac" => Some("alac"),
        b"samr" => Some("amr_nb"),
        b"flac" => Some("flac"),
        b"MAC3" => Some("mace3"),
        b"MAC6" => Some("mace6"),
        b".mp1" => Some("mp1"),
        b".mp2" => Some("mp2"),
        b".mp3" | b"ms\0U" => Some("mp3"),
        b"opus" => Some("opus"),
        b"alaw" => Some("pcm_alaw"),
        b"ulaw" => Some("pcm_mulaw"),
        b"Qclp" => Some("qcelp"),
        b"QDM2" => Some("qdm2"),
        b"QDMC" => Some("qdmc"),
        b"agsm" => Some("gsm"),
        b"ms\0\x01" => Some("gsm_ms"),
        b"ilbc" => Some("ilbc"),
        _ => None,
    }
}

/// The two WAVE-style CAF tags contain NUL bytes; matched by hand.
fn caf_codec_tag_raw(tag: [u8; 4]) -> Option<&'static str> {
    if tag[0] == b'm' && tag[1] == b's' && tag[2] == 0 {
        return match tag[3] {
            2 => Some("adpcm_ms"),
            17 => Some("adpcm_ima_wav"),
            b'1' => Some("gsm_ms"),
            _ => None,
        };
    }
    None
}

/// ff_get_pcm_codec_id for the 'lpcm' tag. FFmpeg passes
/// `(flags ^ 0x2) | 0x4` where the CAF flags are 0x1 float, 0x2
/// little-endian, 0x4 signed integers — so FFmpeg's `flt` is CAF's float
/// bit and FFmpeg's `be` is `!(CAF little-endian bit)`. Integers are
/// always signed.
fn lpcm_codec_id(bps: u32, caf_flags: u32) -> Option<&'static str> {
    let passed = (caf_flags ^ 0x2) | 0x4;
    let flt = passed & 0x1 != 0;
    let be = passed & 0x2 != 0;
    if bps == 0 || bps > 64 {
        return None;
    }
    if flt {
        return match bps {
            32 => Some(if be { "pcm_f32be" } else { "pcm_f32le" }),
            64 => Some(if be { "pcm_f64be" } else { "pcm_f64le" }),
            _ => None,
        };
    }
    match bps.div_ceil(8) {
        1 => Some("pcm_s8"),
        2 => Some(if be { "pcm_s16be" } else { "pcm_s16le" }),
        3 => Some(if be { "pcm_s24be" } else { "pcm_s24le" }),
        4 => Some(if be { "pcm_s32be" } else { "pcm_s32le" }),
        8 => Some(if be { "pcm_s64be" } else { "pcm_s64le" }),
        _ => None,
    }
}

fn caf_sample_format(codec: &str) -> SampleFormat {
    match codec {
        "pcm_u8" | "pcm_s8" => SampleFormat::U8,
        "pcm_s16be" | "pcm_s16le" => SampleFormat::S16,
        "pcm_s24be" | "pcm_s24le" => SampleFormat::S24,
        "pcm_s32be" | "pcm_s32le" => SampleFormat::S32,
        "pcm_f32be" | "pcm_f32le" => SampleFormat::F32,
        "pcm_f64be" | "pcm_f64le" => SampleFormat::F64,
        _ => SampleFormat::S16,
    }
}

pub fn probe_caf(probe: &ProbeData) -> ProbeScore {
    let p = probe.buf;
    if p.len() < 16 {
        return 0;
    }
    if p[0..4] == *b"caff"
        && u16::from_be_bytes([p[4], p[5]]) == 1
        && p[8..12] == *b"desc"
        && u32::from_be_bytes([p[12], p[13], p[14], p[15]]) == 0
        && p.len() >= 20
        && u64::from_be_bytes([p[12], p[13], p[14], p[15], p[16], p[17], p[18], p[19]]) == 32
    {
        MAX_PROBE_SCORE
    } else {
        0
    }
}

struct CafDemuxer {
    input: Box<dyn ReadSeek>,
    stream: StreamInfo,
    bytes_per_packet: i64,
    frames_per_packet: i64,
    data_start: u64,
    data_size: i64,
    num_packets: i64,
    packet_cnt: i64,
    frame_cnt: i64,
    /// Per-packet (byte_pos, frame_start) from the pakt chunk when sizes
    /// are variable; empty for fixed-size packets.
    table: Vec<(i64, i64)>,
    /// Total data bytes and total frames after the packet table.
    total_bytes: i64,
    total_frames: i64,
    remainder: u32,
    priming: u32,
}

/// Read one chunk header: (tag, size).
fn read_chunk_header(input: &mut Box<dyn ReadSeek>) -> Result<([u8; 4], i64)> {
    let mut hdr = [0u8; 12];
    input.read_exact(&mut hdr)?;
    let size = i64::from_be_bytes([
        hdr[4], hdr[5], hdr[6], hdr[7], hdr[8], hdr[9], hdr[10], hdr[11],
    ]);
    Ok(([hdr[0], hdr[1], hdr[2], hdr[3]], size))
}

/// ff_mp4_read_descr_len (isom.c), used for variable-size packet tables.
fn read_descr_len(input: &mut Box<dyn ReadSeek>) -> Result<i64> {
    let mut len: i64 = 0;
    for _ in 0..4 {
        let mut b = [0u8; 1];
        input.read_exact(&mut b)?;
        len = (len << 7) | i64::from(b[0] & 0x7F);
        if b[0] & 0x80 == 0 {
            break;
        }
    }
    Ok(len)
}

pub fn open_caf(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let mut magic = [0u8; 8];
    input.read_exact(&mut magic)?;
    if &magic[0..4] != b"caff" {
        return Err(Error::invalid("caf: bad magic"));
    }

    // audio description chunk
    let (tag, size) = read_chunk_header(&mut input)?;
    if &tag != b"desc" {
        return Err(Error::invalid("caf: desc chunk not present"));
    }
    if size != 32 {
        return Err(Error::invalid("caf: desc size must be 32"));
    }

    let mut desc = [0u8; 32];
    input.read_exact(&mut desc)?;
    // sample_rate: IEEE double big-endian, clamped like FFmpeg's
    // av_clipd(av_int2double(..), 0, INT_MAX).
    let rate_bits = u64::from_be_bytes([
        desc[0], desc[1], desc[2], desc[3], desc[4], desc[5], desc[6], desc[7],
    ]);
    let rate_f = f64::from_bits(rate_bits);
    let sample_rate = if rate_f.is_finite() && rate_f > 0.0 {
        rate_f.min(i32::MAX as f64) as u32
    } else {
        0
    };
    let codec_tag = [desc[8], desc[9], desc[10], desc[11]];
    let flags = u32::from_be_bytes([desc[12], desc[13], desc[14], desc[15]]);
    let bytes_per_packet = u32::from_be_bytes([desc[16], desc[17], desc[18], desc[19]]) as i64;
    let frames_per_packet = u32::from_be_bytes([desc[20], desc[21], desc[22], desc[23]]) as i64;
    let channels = u32::from_be_bytes([desc[24], desc[25], desc[26], desc[27]]);
    let bits_per_coded_sample = u32::from_be_bytes([desc[28], desc[29], desc[30], desc[31]]);

    if channels > 64 {
        return Err(Error::invalid("caf: channel count exceeds maximum"));
    }

    let codec = if &codec_tag == b"lpcm" {
        lpcm_codec_id(bits_per_coded_sample, flags)
    } else {
        caf_codec_tag(codec_tag).or_else(|| caf_codec_tag_raw(codec_tag))
    };
    let codec = codec
        .ok_or_else(|| {
            Error::codec_not_found(format!(
                "caf: unknown codec tag {:?}",
                std::str::from_utf8(&codec_tag).unwrap_or("?")
            ))
        })?
        .to_string();

    let mut params = CodecParameters::audio(CodecId::new(codec.clone()));
    params.sample_rate = Some(sample_rate);
    params.channels = Some(channels as u16);
    params.tag = Some(CodecTag::fourcc(&codec_tag));
    if &codec_tag == b"lpcm" {
        params.sample_format = Some(caf_sample_format(&codec));
    }

    // chunk walk until 'data'
    let mut found_data = false;
    let mut data_start = 0u64;
    let mut data_size = -1i64;
    let mut extradata: Vec<u8> = Vec::new();
    let mut table: Vec<(i64, i64)> = Vec::new();
    let mut num_packets = 0i64;
    let mut total_bytes = 0i64;
    let mut total_frames = 0i64;
    let mut priming = 0u32;
    let mut remainder = 0u32;

    while !found_data {
        let (tag, size) = match read_chunk_header(&mut input) {
            Ok(v) => v,
            Err(Error::Eof) => break,
            Err(e) => return Err(e),
        };
        if !(0..=MAX_CHUNK_SIZE).contains(&size) {
            return Err(Error::invalid("caf: oversized or negative chunk"));
        }
        let pos = input.stream_position()?;
        match &tag {
            b"data" => {
                let mut edit = [0u8; 4];
                input.read_exact(&mut edit)?; // edit count
                data_start = input.stream_position()?;
                data_size = size - 4;
                found_data = true;
            }
            b"kuki" => {
                extradata = vec![0u8; size as usize];
                input.read_exact(&mut extradata)?;
            }
            b"pakt" => {
                let end = pos.checked_add_signed(size).ok_or_else(|| Error::invalid("caf: chunk overflow"))?;
                let mut cnt = [0u8; 24];
                input.read_exact(&mut cnt)?;
                num_packets = i64::from_be_bytes(cnt[0..8].try_into().expect("8 bytes"));
                let _valid_frames = i64::from_be_bytes(cnt[8..16].try_into().expect("8 bytes"));
                priming = u32::from_be_bytes(cnt[16..20].try_into().expect("4 bytes"));
                remainder = u32::from_be_bytes(cnt[20..24].try_into().expect("4 bytes"));
                if !(0..=MAX_PACKETS).contains(&num_packets) {
                    return Err(Error::invalid("caf: packet table too large"));
                }
                let variable = !(bytes_per_packet > 0 && frames_per_packet > 0);
                if variable && num_packets > 0 && bytes_per_packet <= 0 && frames_per_packet <= 0 {
                    return Err(Error::invalid("caf: missing packet table sizes"));
                }
                if !variable {
                    if num_packets == 0 {
                        num_packets = if data_size > 0 { data_size / bytes_per_packet } else { 0 };
                    }
                    total_bytes = bytes_per_packet
                        .checked_mul(num_packets)
                        .ok_or_else(|| Error::invalid("caf: packet table overflow"))?;
                    total_frames = frames_per_packet
                        .checked_mul(num_packets)
                        .ok_or_else(|| Error::invalid("caf: packet table overflow"))?;
                } else {
                    let mut pkt_pos = 0i64;
                    let mut frame = -i64::from(priming);
                    table.reserve(num_packets as usize);
                    for _ in 0..num_packets {
                        table.push((pkt_pos, frame));
                        pkt_pos += if bytes_per_packet > 0 {
                            bytes_per_packet
                        } else {
                            read_descr_len(&mut input)?
                        };
                        frame += if frames_per_packet > 0 {
                            frames_per_packet
                        } else {
                            read_descr_len(&mut input)?
                        };
                        if input.stream_position()? > end || pkt_pos > MAX_CHUNK_SIZE {
                            return Err(Error::invalid("caf: error reading packet table"));
                        }
                    }
                    total_bytes = pkt_pos;
                    total_frames = frame;
                }
                if input.stream_position()? > end {
                    return Err(Error::invalid("caf: error reading packet table"));
                }
                input.seek(SeekFrom::Start(end))?;
            }
            _ => {
                // skip unknown chunk ('free', 'chan', 'info', ...)
                input.seek(SeekFrom::Current(size))?;
            }
        }
    }

    if !found_data {
        return Err(Error::invalid("caf: data chunk not found"));
    }
    params.extradata = extradata;

    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::from_rate(sample_rate.max(1)),
        duration: Some(total_frames - i64::from(priming) - i64::from(remainder)),
        start_time: Some(0),
    };

    Ok(Box::new(CafDemuxer {
        input,
        stream,
        bytes_per_packet,
        frames_per_packet,
        data_start,
        data_size,
        num_packets,
        packet_cnt: 0,
        frame_cnt: -i64::from(priming),
        table,
        total_bytes,
        total_frames,
        remainder,
        priming,
    }))
}

impl CafDemuxer {
    fn read_packet(&mut self) -> Result<Packet> {
        let left = if self.data_size > 0 {
            let end = self.data_start + self.data_size as u64;
            let cur = self.input.stream_position()?;
            if cur >= end {
                return Err(Error::Eof);
            }
            i64::try_from(end - cur).map_err(|_| Error::invalid("caf: data offset overflow"))?
        } else {
            CAF_MAX_PKT_SIZE as i64
        };

        let pkt_size;
        let mut pkt_frames;
        let mut remainder = 0u32;

        if self.bytes_per_packet > 0 && self.frames_per_packet == 1 {
            // Aggregate whole frames into CAF_MAX_PKT_SIZE-sized packets.
            let bpb = self.bytes_per_packet;
            pkt_size = ((CAF_MAX_PKT_SIZE / bpb as usize) as i64 * bpb).min(left);
            pkt_frames = pkt_size / bpb;
        } else if !self.table.is_empty() {
            let n = self.table.len() as i64;
            if self.packet_cnt < n - 1 {
                let i = self.packet_cnt as usize;
                pkt_size = self.table[i + 1].0 - self.table[i].0;
                pkt_frames = self.table[i + 1].1 - self.table[i].1;
            } else if self.packet_cnt == n - 1 {
                pkt_size = self.total_bytes - self.table[(n - 1) as usize].0;
                pkt_frames = self.total_frames - self.table[(n - 1) as usize].1;
                remainder = self.remainder;
            } else {
                return Err(Error::Eof);
            }
        } else {
            pkt_size = self.bytes_per_packet;
            pkt_frames = self.frames_per_packet;
            if self.packet_cnt + 1 == self.num_packets {
                pkt_frames -= i64::from(self.remainder);
                remainder = self.remainder;
            }
        }

        if pkt_size <= 0 || pkt_frames <= 0 || pkt_size > left {
            return Err(Error::invalid("caf: invalid packet size"));
        }

        let mut data = vec![0u8; pkt_size as usize];
        self.input.read_exact(&mut data)?;

        let pts = self.frame_cnt;
        let mut pkt = Packet {
            stream_index: 0,
            time_base: self.stream.time_base,
            pts: Some(pts),
            dts: Some(pts),
            duration: Some(pkt_frames),
            flags: Default::default(),
            data,
        };
        pkt.flags.keyframe = true;
        // FFmpeg attaches priming/remainder as skip-samples side data; this
        // crate has no side-data channel, so the first packet is marked
        // discard for its priming frames (opus/aac) and the last packet's
        // duration already excludes the remainder via pkt_frames above.
        if self.packet_cnt == 0 && self.priming > 0 {
            pkt.flags.discard = true;
        }
        let _ = remainder;

        self.packet_cnt += 1;
        self.frame_cnt += pkt_frames;
        Ok(pkt)
    }
}

impl Demuxer for CafDemuxer {
    fn format_name(&self) -> &str {
        "caf"
    }

    fn streams(&self) -> &[StreamInfo] {
        std::slice::from_ref(&self.stream)
    }

    fn next_packet(&mut self) -> Result<Packet> {
        self.read_packet()
    }

    /// cafdec.c read_seek with AVSEEK_FLAG_BACKWARD. Constant-size packets
    /// go by arithmetic, so PCM resumes at the target sample itself;
    /// a packet table by av_index_search_timestamp over its entries (the
    /// last packet starting at or before the target). Without either,
    /// FFmpeg's fallback (seek_frame_generic) has no index and fails too.
    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        let timestamp = pts.max(0);
        let priming = i64::from(self.priming);
        let (pos, packet_cnt, frame_cnt) = if self.frames_per_packet > 0 && self.bytes_per_packet > 0 {
            let mut pos = self.bytes_per_packet.saturating_mul(timestamp / self.frames_per_packet);
            if self.data_size > 0 {
                pos = pos.min(self.data_size);
            }
            let packet_cnt = pos / self.bytes_per_packet;
            (pos, packet_cnt, self.frames_per_packet.saturating_mul(packet_cnt) - priming)
        } else {
            let at = self.table.partition_point(|&(_, frame)| frame <= timestamp);
            let Some(&(pos, frame)) = at.checked_sub(1).and_then(|i| self.table.get(i)) else {
                return Err(Error::unsupported("caf: no packet table entry to seek to"));
            };
            (pos, (at - 1) as i64, frame)
        };
        let start = self.data_start.checked_add(pos as u64).ok_or_else(|| Error::invalid("caf: seek offset overflow"))?;
        self.input.seek(SeekFrom::Start(start))?;
        self.packet_cnt = packet_cnt;
        self.frame_cnt = frame_cnt;
        Ok(frame_cnt)
    }
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("caf", open_caf);
    reg.register_probe("caf", probe_caf);
    reg.register_extension("caf", "caf");
}
