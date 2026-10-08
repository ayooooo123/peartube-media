// Ported from FFmpeg libavformat/cafdec.c and caf.c (commit 2da55bf).
// License: LGPL-2.1-or-later
//
// Core Audio Format demuxer: 'caff' header, a 'desc' audio description
// chunk, then 'kuki' (magic cookie / extradata), 'pakt' (packet table),
// 'chan', 'info' and 'data' chunks, the table before or after the data.
// Packets are framed by the packet table: fixed bytes_per_packet/
// frames_per_packet, or a per-packet table of (bytes, frames) lengths when
// variable. Timestamps are in samples. The table's priming frames (first
// packet) and remainder frames (last packet) reach the player as
// `AudioTrim`, FFmpeg's skip-samples side data.

use std::io::{Read, Seek, SeekFrom};
use oxideav_core::{
    AudioTrim, CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, PacketMetadata, ProbeData, ProbeScore, ReadSeek, Result, SampleFormat, StreamInfo,
    TimeBase, CodecTag, MAX_PROBE_SCORE,
};

const CAF_MAX_PKT_SIZE: usize = 4096;
/// Untrusted-input caps: chunks read into memory, packets, packet tables.
const MAX_CHUNK_SIZE: i64 = 256 * 1024 * 1024;
const MAX_PACKET_SIZE: i64 = 256 * 1024 * 1024;
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
    sample_rate: u32,
    bytes_per_packet: i64,
    frames_per_packet: i64,
    data_start: u64,
    /// The data chunk's size without its edit count; negative when it
    /// runs to the end of the file.
    data_size: i64,
    num_packets: i64,
    packet_cnt: i64,
    frame_cnt: i64,
    /// Per-packet (byte_pos, frame_start) from the pakt chunk when sizes
    /// are variable (FFmpeg's index entries); empty for fixed-size packets.
    table: Vec<(i64, i64)>,
    /// `caf->num_bytes`: the data bytes the packet table covers.
    num_bytes: i64,
    /// `st->duration`: the frames after the priming, without the
    /// remainder.
    duration: i64,
    /// Frames to drop from the end of the last packet.
    remainder: u32,
    /// Frames to drop from the start of the first packet.
    priming: u32,
    /// The trims of the packet read last (`AV_PKT_DATA_SKIP_SAMPLES`).
    metadata: PacketMetadata,
}

/// Read one chunk header: (tag, size); `None` at the end of the input
/// (fewer than 12 bytes left, FFmpeg's avio_feof after reading them).
fn read_chunk_header(input: &mut Box<dyn ReadSeek>) -> Result<Option<([u8; 4], i64)>> {
    let hdr = read_up_to(input, 12)?;
    let Ok(hdr) = <[u8; 12]>::try_from(hdr) else { return Ok(None) };
    let size = i64::from_be_bytes([
        hdr[4], hdr[5], hdr[6], hdr[7], hdr[8], hdr[9], hdr[10], hdr[11],
    ]);
    Ok(Some(([hdr[0], hdr[1], hdr[2], hdr[3]], size)))
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

/// Up to `size` bytes from the reader, which may end sooner. Memory grows
/// with the bytes that are there, not with what a header claims.
fn read_up_to(input: &mut Box<dyn ReadSeek>, size: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    input.take(size).read_to_end(&mut data)?;
    Ok(data)
}

/// What read_pakt_chunk sets.
#[derive(Default)]
struct PacketTable {
    num_packets: i64,
    priming: u32,
    remainder: u32,
    /// `st->duration`
    duration: i64,
    /// `caf->num_bytes`
    num_bytes: i64,
    /// FFmpeg's index entries: (byte_pos, frame_start), variable sizes only.
    entries: Vec<(i64, i64)>,
}

/// read_pakt_chunk: the `size`-byte table at the reader's position, for
/// packets of `bytes_per_packet` and `frames_per_packet` (0 where they
/// vary), in a data chunk of `data_size` bytes (0 while the data chunk
/// is still ahead, as in FFmpeg's zeroed context).
fn read_pakt(
    input: &mut Box<dyn ReadSeek>,
    size: i64,
    bytes_per_packet: i64,
    frames_per_packet: i64,
    data_size: i64,
) -> Result<PacketTable> {
    let invalid = || Error::invalid("caf: error reading packet table");
    let start = input.stream_position()?;
    if size < 0 {
        return Err(invalid());
    }
    let end = start.checked_add(size as u64).ok_or_else(invalid)?;
    let mut cnt = [0u8; 24];
    input.read_exact(&mut cnt)?;
    let mut num_packets = i64::from_be_bytes(cnt[0..8].try_into().expect("8 bytes"));
    if !(0..=MAX_PACKETS).contains(&num_packets) {
        return Err(Error::invalid("caf: packet table too large"));
    }
    // cnt[8..16], the valid frames, only adds up FFmpeg's nb_frames.
    let mut table = PacketTable {
        priming: u32::from_be_bytes(cnt[16..20].try_into().expect("4 bytes")),
        remainder: u32::from_be_bytes(cnt[20..24].try_into().expect("4 bytes")),
        ..PacketTable::default()
    };
    let priming = i64::from(table.priming);
    if bytes_per_packet > 0 && frames_per_packet > 0 {
        if num_packets == 0 {
            if data_size < 0 {
                return Err(invalid());
            }
            num_packets = data_size / bytes_per_packet;
        }
        table.duration = frames_per_packet.checked_mul(num_packets).ok_or_else(invalid)? - priming;
        table.num_bytes = bytes_per_packet.checked_mul(num_packets).ok_or_else(invalid)?;
    } else {
        // Each entry takes at least one byte of the chunk.
        table.entries.reserve(num_packets.min(size) as usize);
        let (mut pos, mut duration) = (0i64, -priming);
        for _ in 0..num_packets {
            table.entries.push((pos, duration));
            pos += if bytes_per_packet != 0 { bytes_per_packet } else { read_descr_len(input)? };
            duration += if frames_per_packet != 0 { frames_per_packet } else { read_descr_len(input)? };
            if input.stream_position()? > end {
                return Err(invalid());
            }
        }
        table.duration = duration;
        table.num_bytes = pos;
    }
    table.duration -= i64::from(table.remainder);
    if table.duration < 0 || input.stream_position()? > end {
        return Err(invalid());
    }
    input.seek(SeekFrom::Start(end))?;
    table.num_packets = num_packets;
    Ok(table)
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
    let (tag, size) = read_chunk_header(&mut input)?.ok_or_else(|| Error::invalid("caf: desc chunk not present"))?;
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

    // read_header's chunk walk. Past a data chunk of known size it skips
    // the audio and reads on, so a packet table written after the data
    // (where FFmpeg's muxer puts it) is found; a data chunk that runs to
    // the end of the file ends the walk.
    let mut found_data = false;
    let mut data_start = 0u64;
    let mut data_size = 0i64;
    let mut extradata: Vec<u8> = Vec::new();
    let mut pakt: Option<PacketTable> = None;

    loop {
        if found_data && data_size < 0 {
            break;
        }
        let Some((tag, size)) = read_chunk_header(&mut input)? else { break };
        let pos = input.stream_position()?;
        match &tag {
            b"data" => {
                input.seek(SeekFrom::Current(4))?; // edit count
                data_start = input.stream_position()?;
                data_size = if size < 0 { -1 } else { size - 4 };
                if data_start > i64::MAX as u64 || data_size > i64::MAX - data_start as i64 {
                    return Err(Error::invalid("caf: data chunk overflow"));
                }
                if data_size > 0 {
                    input.seek(SeekFrom::Start(data_start + data_size as u64))?;
                }
                found_data = true;
            }
            b"kuki" => {
                if !(0..=MAX_CHUNK_SIZE).contains(&size) {
                    return Err(Error::invalid("caf: oversized or negative magic cookie"));
                }
                extradata = read_up_to(&mut input, size as u64)?;
                if extradata.len() as i64 != size {
                    return Err(Error::invalid("caf: truncated magic cookie"));
                }
            }
            b"pakt" => {
                pakt = Some(read_pakt(&mut input, size, bytes_per_packet, frames_per_packet, data_size)?);
            }
            // 'free', 'chan', 'info' and unknown chunks are skipped.
            _ => {
                if size < 0 {
                    if found_data {
                        break;
                    }
                    return Err(Error::invalid("caf: chunk of unknown size before the data"));
                }
            }
        }
        if size > 0 {
            let next = pos.checked_add(size as u64).ok_or_else(|| Error::invalid("caf: chunk overflow"))?;
            input.seek(SeekFrom::Start(next))?;
        }
    }

    if !found_data {
        return Err(Error::invalid("caf: data chunk not found"));
    }
    let constant = bytes_per_packet > 0 && frames_per_packet > 0;
    // FFmpeg's st->duration from the packet table; without one, constant
    // packets give nb_frames, when that fits an i64 (cafdec.c's check).
    let duration = match &pakt {
        Some(table) => Some(table.duration),
        None if constant && data_size > 0 && data_size / bytes_per_packet < i64::MAX / frames_per_packet => {
            Some((data_size / bytes_per_packet) * frames_per_packet)
        }
        None => None,
    };
    let pakt = pakt.unwrap_or_default();
    if !constant && (pakt.entries.is_empty() || pakt.duration <= 0) {
        return Err(Error::invalid("caf: missing packet table, required when block or frame size varies"));
    }
    params.extradata = extradata;
    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::from_rate(sample_rate.max(1)),
        duration,
        start_time: Some(0),
    };
    input.seek(SeekFrom::Start(data_start))?;

    Ok(Box::new(CafDemuxer {
        input,
        stream,
        sample_rate,
        bytes_per_packet,
        frames_per_packet,
        data_start,
        data_size,
        num_packets: pakt.num_packets,
        packet_cnt: 0,
        frame_cnt: -i64::from(pakt.priming),
        table: pakt.entries,
        num_bytes: pakt.num_bytes,
        duration: pakt.duration,
        remainder: pakt.remainder,
        priming: pakt.priming,
        metadata: PacketMetadata::default(),
    }))
}

impl CafDemuxer {
    fn read_packet(&mut self) -> Result<Packet> {
        self.metadata = PacketMetadata::default();
        let left = if self.data_size > 0 {
            let end = self.data_start + self.data_size as u64;
            match end.checked_sub(self.input.stream_position()?) {
                Some(0) => return Err(Error::Eof),
                Some(left) => left as i64,
                None => return Err(Error::invalid("caf: read past the data chunk")),
            }
        } else {
            CAF_MAX_PKT_SIZE as i64
        };

        let mut pkt_size = self.bytes_per_packet;
        let mut pkt_frames = self.frames_per_packet;
        let mut remainder = 0u32;

        if pkt_size > 0 && pkt_frames == 1 {
            // Aggregate whole frames into CAF_MAX_PKT_SIZE-sized packets.
            let bpb = self.bytes_per_packet;
            pkt_size = ((CAF_MAX_PKT_SIZE as i64 / bpb) * bpb).min(left);
            pkt_frames = pkt_size / bpb;
        } else if !self.table.is_empty() {
            let n = self.table.len() as i64;
            let i = self.packet_cnt as usize;
            if self.packet_cnt < n - 1 {
                pkt_size = self.table[i + 1].0 - self.table[i].0;
                pkt_frames = self.table[i + 1].1 - self.table[i].1;
            } else if self.packet_cnt == n - 1 {
                pkt_size = self.num_bytes - self.table[i].0;
                pkt_frames = self.duration - self.table[i].1;
                remainder = self.remainder;
            } else {
                return Err(Error::Eof);
            }
        } else if self.packet_cnt + 1 == self.num_packets {
            pkt_frames -= i64::from(self.remainder);
            remainder = self.remainder;
        }

        // FFmpeg refuses only a zero frame count: a remainder larger than
        // the last packet leaves it negative, and the decoder then ignores
        // the padding as larger than the frame.
        if pkt_size <= 0 || pkt_frames == 0 || pkt_size > left || pkt_size > MAX_PACKET_SIZE {
            return Err(Error::invalid("caf: invalid packet size"));
        }

        // av_get_packet: a packet cut short by the end of the file keeps
        // what is there.
        let data = read_up_to(&mut self.input, pkt_size as u64)?;
        if data.is_empty() {
            return Err(Error::Eof);
        }

        let priming = if self.packet_cnt == 0 { self.priming } else { 0 };
        if priming > 0 || remainder > 0 {
            self.metadata.audio_trim =
                Some(AudioTrim { skip_samples: priming, discard_padding: remainder, sample_rate: self.sample_rate });
        }

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

        self.packet_cnt += 1;
        // Hostile frame counts can sum past i64 over a long stream; C's
        // signed overflow has no defined result, and a timestamp is all
        // this feeds.
        self.frame_cnt = self.frame_cnt.wrapping_add(pkt_frames);
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

    /// The priming on the first packet and the remainder on the last, as
    /// FFmpeg's `AV_PKT_DATA_SKIP_SAMPLES`.
    fn packet_metadata(&self) -> PacketMetadata {
        self.metadata.clone()
    }

    /// cafdec.c read_seek with AVSEEK_FLAG_BACKWARD. Constant-size packets
    /// go by arithmetic, so PCM resumes at the target sample itself;
    /// a packet table by av_index_search_timestamp over its entries (the
    /// last packet starting at or before the target). Without either,
    /// FFmpeg's fallback (seek_frame_generic) has no index and fails too.
    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        self.metadata = PacketMetadata::default();
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
