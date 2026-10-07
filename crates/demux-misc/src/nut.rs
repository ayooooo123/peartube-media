// Ported from FFmpeg libavformat/nutdec.c, nut.c and nut.h (commit 2da55bf).
// License: LGPL-2.1-or-later
//
// NUT container demuxer: main header (version, time bases, the 256-entry
// frame_code table, optional elision headers), stream headers (fourcc →
// codec via the codec resolver, time base id, msb_pts_shift, extradata,
// video dimensions or audio format), info headers (skipped), then frames
// introduced by syncpoints. Frames without FLAG_CODED_PTS derive their PTS
// from the previous PTS plus the frame_code's pts_delta (lsb2full unwind
// otherwise). Checksummed with CRC-32 (poly 0x04C11DB7) over startcode +
// payload, like FFmpeg's get_packetheader.

use std::io::{Read, Seek};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    MediaType, Packet, ProbeContext, ProbeData, ProbeScore, ReadSeek, Result,
    StreamInfo, TimeBase, CodecTag, MAX_PROBE_SCORE,
};

use demux_seek_core::{gen_search, Allowance, Bounds, Index, Reduce};

const MAIN_STARTCODE: u64 = 0x4E4D7A561F5F04AD; // 'N''M' | 0x7A561F5F04AD
const STREAM_STARTCODE: u64 = 0x4E5311405BF2F9DB; // 'N''S'
const SYNCPOINT_STARTCODE: u64 = 0x4E4BE4ADEECA4569; // 'N''K'
const INDEX_STARTCODE: u64 = 0x4E58DD672F23E64E; // 'N''X'
const INFO_STARTCODE: u64 = 0x4E49AB68B596BA78; // 'N''I'

const NUT_MAX_STREAMS: usize = 256;
const NUT_VERSION_MIN: u64 = 2;
const NUT_VERSION_MAX: u64 = 4;

const FLAG_KEY: i64 = 1;
const FLAG_EOR: i64 = 2;
const FLAG_CODED_PTS: i64 = 8;
const FLAG_STREAM_ID: i64 = 16;
const FLAG_SIZE_MSB: i64 = 32;
const FLAG_CHECKSUM: i64 = 64;
const FLAG_RESERVED: i64 = 128;
const FLAG_SM_DATA: i64 = 256;
const FLAG_HEADER_IDX: i64 = 1024;
const FLAG_MATCH_TIME: i64 = 2048;
const FLAG_CODED: i64 = 4096;
const FLAG_INVALID: i64 = 8192;

/// Caps for untrusted input.
const MAX_HEADER_BYTES: usize = 8 * 1024 * 1024;
const MAX_FRAME_SIZE: i64 = 64 * 1024 * 1024;
/// The index read from the end of the file.
const MAX_INDEX_BYTES: i64 = 64 * 1024 * 1024;
/// Syncpoints remembered for seeking a file without an index.
const MAX_SYNCPOINTS: usize = 1 << 20;

/// ff_crc04C11DB7 / AV_CRC_32_IEEE, bit-at-a-time (poly 0x04C11DB7,
/// non-reflected, init/final 0).
fn crc32_mpeg(data: &[u8], mut crc: u32) -> u32 {
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[derive(Clone, Copy, Default)]
struct FrameCode {
    flags: i64,
    pts_delta: i64,
    stream_id: usize,
    size_mul: i64,
    size_lsb: i64,
    reserved_count: i64,
    header_idx: usize,
}

#[derive(Clone)]
struct NutStream {
    time_base: TimeBase,
    msb_pts_shift: i64,
    max_pts_distance: i64,
    last_pts: i64,
    last_flags: i64,
}

/// A syncpoint FFmpeg's syncpoint tree keeps: where it starts, where
/// decoding every stream can start from (back_ptr), its time in
/// microseconds.
#[derive(Clone, Copy)]
struct Syncpoint {
    pos: i64,
    back_ptr: i64,
    ts: i64,
}

struct NutDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    states: Vec<NutStream>,
    time_bases: Vec<TimeBase>,
    frame_code: Vec<FrameCode>,
    header: Vec<Vec<u8>>,
    max_distance: i64,
    /// Bytes consumed while headers were being read, so frame decoding
    /// starts exactly at the first syncpoint.
    header_end: u64,
    /// Where the first syncpoint starts (FFmpeg's data_offset).
    data_offset: i64,
    /// Per stream: the key frames of the index at the end of the file.
    index: Vec<Index>,
    /// Every syncpoint read, by position (FFmpeg's syncpoint tree).
    syncpoints: Vec<Syncpoint>,
    /// Per stream: frames are dropped after a seek until a key frame
    /// (skip_until_key_frame).
    skip_until_key: Vec<bool>,
    /// What the seek under way may still read.
    allowance: Allowance,
}

/// ffio_read_varlen: little-endian base-128 with continuation MSB,
/// big-endian accumulation ((val << 7) + (tmp & 127)).
fn read_varlen(input: &mut Box<dyn ReadSeek>) -> Result<u64> {
    let mut val: u64 = 0;
    loop {
        let mut b = [0u8; 1];
        input.read_exact(&mut b)?;
        val = (val << 7) | u64::from(b[0] & 0x7F);
        if b[0] & 0x80 == 0 {
            break;
        }
        if val > u64::MAX >> 7 {
            return Err(Error::invalid("nut: varlen overflow"));
        }
    }
    Ok(val)
}

/// get_s: signed varlen — v+1 coded, odd = negative.
fn read_signed_varlen(input: &mut Box<dyn ReadSeek>) -> Result<i64> {
    let v = read_varlen(input)? as i64;
    let v = v.checked_add(1).ok_or_else(|| Error::invalid("nut: varlen overflow"))?;
    if v & 1 != 0 {
        Ok(-(v >> 1))
    } else {
        Ok(v >> 1)
    }
}

/// get_fourcc: 2-byte or 4-byte little-endian value.
fn read_fourcc(input: &mut Box<dyn ReadSeek>) -> Result<Option<CodecTag>> {
    let len = read_varlen(input)?;
    match len {
        2 => {
            let mut b = [0u8; 2];
            input.read_exact(&mut b)?;
            // FFmpeg returns avio_rl16 zero-extended into a 4-byte tag.
            Ok(Some(CodecTag::fourcc(&[b[0], b[1], 0, 0])))
        }
        4 => {
            let mut b = [0u8; 4];
            input.read_exact(&mut b)?;
            Ok(Some(CodecTag::fourcc(&b)))
        }
        _ => Err(Error::invalid("nut: unsupported fourcc length")),
    }
}

/// get_packetheader: the payload size, a varlen, and for a payload over
/// 4096 bytes the checksum after it. The running checksum starts from the
/// start code's and runs over the size and the stored checksum, which
/// make it 0 (nutdec.c:97-107). Returns (size, checksum holds).
fn read_packet_size(input: &mut Box<dyn ReadSeek>, startcode: u64) -> Result<(i64, bool)> {
    let mut crc = crc32_mpeg(&startcode.to_be_bytes(), 0);
    let mut size: u64 = 0;
    loop {
        let mut b = [0u8; 1];
        input.read_exact(&mut b)?;
        crc = crc32_mpeg(&b, crc);
        size = (size << 7) | u64::from(b[0] & 0x7F);
        if b[0] & 0x80 == 0 {
            break;
        }
        if size > u64::MAX >> 7 {
            return Err(Error::invalid("nut: varlen overflow"));
        }
    }
    if size > u64::from(u32::MAX) {
        return Err(Error::invalid("nut: header size overflow"));
    }
    if size > 4096 {
        let mut stored = [0u8; 4];
        input.read_exact(&mut stored)?;
        return Ok((size as i64, crc32_mpeg(&stored, crc) == 0));
    }
    Ok((size as i64, true))
}

/// get_packetheader where a bad checksum is an error. Returns (size,
/// checksummed).
fn read_packet_header(
    input: &mut Box<dyn ReadSeek>,
    startcode: u64,
) -> Result<(i64, bool)> {
    let (size, holds) = read_packet_size(input, startcode)?;
    if !holds {
        return Err(Error::invalid("nut: header checksum mismatch"));
    }
    Ok((size, size > 4096))
}

/// find_any_startcode from the current position; returns (startcode, pos
/// of the startcode's first byte).
fn find_any_startcode(input: &mut Box<dyn ReadSeek>) -> Result<Option<(u64, u64)>> {
    let mut state: u64 = 0;
    let start: u64 = input.stream_position()?;
    let mut i: u64 = 0;
    loop {
        let mut b = [0u8; 1];
        match input.read(&mut b) {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
        state = (state << 8) | u64::from(b[0]);
        i += 1;
        if (state >> 56) != u64::from(b'N') {
            continue;
        }
        if matches!(
            state,
            MAIN_STARTCODE | STREAM_STARTCODE | SYNCPOINT_STARTCODE | INFO_STARTCODE | INDEX_STARTCODE
        ) {
            return Ok(Some((state, start + i - 8)));
        }
    }
}

fn nut_probe(p: &ProbeData) -> ProbeScore {
    let buf = p.buf;
    let target = (MAIN_STARTCODE >> 32) as u32;
    let target_lo = (MAIN_STARTCODE & 0xFFFF_FFFF) as u32;
    if buf.len() < 8 {
        return 0;
    }
    for w in buf[..buf.len() - 7].windows(8) {
        if u32::from_be_bytes(w[0..4].try_into().unwrap()) == target
            && u32::from_be_bytes(w[4..8].try_into().unwrap()) == target_lo
        {
            return MAX_PROBE_SCORE;
        }
    }
    0
}

fn nut_codec_id(
    codecs: &dyn CodecResolver,
    class: u64,
    tag: &CodecTag,
    sample_rate: u32,
    channels: u16,
    width: u32,
    height: u32,
) -> Option<(CodecId, MediaType)> {
    let media_type = match class {
        0 => MediaType::Video,
        1 => MediaType::Audio,
        2 => MediaType::Subtitle,
        _ => MediaType::Data,
    };
    let mut ctx = ProbeContext::new(tag);
    ctx.channels = Some(channels);
    ctx.sample_rate = Some(sample_rate);
    ctx.width = Some(width);
    ctx.height = Some(height);
    if let Some(id) = codecs.resolve_tag(&ctx) {
        return Some((id, media_type));
    }
    // NUT audio tags reuse WAVEFORMAT ids (ff_codec_wav_tags): the 32-bit
    // tag value's low half is the wFormatTag (0x50 = mp2, 0x55 = mp3, …).
    if let CodecTag::Fourcc(raw) = tag {
        let wav = u16::from_le_bytes([raw[0], raw[1]]);
        if wav != 0 {
            // FFmpeg's ff_codec_wav_tags mapping for the tags the player
            // decodes in software (ff_codec_wav_tags maps 0x50 → MP2).
            let direct = match wav {
                0x0050 => Some("mp2"),
                0x0055 => Some("mp3"),
                0x0001 => Some("pcm_u8"),
                _ => None,
            };
            if let Some(name) = direct {
                return Some((CodecId::new(name), media_type));
            }
        }
        if wav != 0 {
            let wt = CodecTag::WaveFormat(wav);
            let mut ctx = ProbeContext::new(&wt);
            ctx.channels = Some(channels);
            ctx.sample_rate = Some(sample_rate);
            if let Some(id) = codecs.resolve_tag(&ctx) {
                return Some((id, media_type));
            }
        }
    }
    codecs.resolve_tag(&ctx).map(|id| (id, media_type))
}

impl NutDemuxer {
    /// decode_main_header. Returns Ok(false) when the header did not
    /// validate (caller resyncs, like FFmpeg's do/while), Ok(true) on
    /// success.
    fn decode_main_header(&mut self) -> Result<bool> {
        let (size, _ck) = read_packet_header(&mut self.input, MAIN_STARTCODE)?;
        let end = self.input.stream_position()? + size as u64;

        let version = read_varlen(&mut self.input)?;
        if !(NUT_VERSION_MIN..=NUT_VERSION_MAX).contains(&version) {
            return Err(Error::unsupported(format!("nut: version {version} not supported")));
        }
        if version > 3 {
            read_varlen(&mut self.input)?; // minor version
        }
        let stream_count = read_varlen(&mut self.input)? as usize;
        if stream_count == 0 || stream_count > NUT_MAX_STREAMS {
            return Err(Error::invalid("nut: illegal stream count"));
        }
        let max_distance = read_varlen(&mut self.input)? as i64;
        self.max_distance = max_distance.min(65536);

        let time_base_count = read_varlen(&mut self.input)? as usize;
        if time_base_count == 0 || time_base_count >= size as usize / 2 {
            return Err(Error::invalid("nut: illegal time base count"));
        }
        self.time_bases.clear();
        for _ in 0..time_base_count {
            let num = read_varlen(&mut self.input)? as i64;
            let den = read_varlen(&mut self.input)? as i64;
            if num <= 0 || num >= 1 << 31 || den <= 0 || den >= 1 << 31 {
                return Err(Error::invalid("nut: invalid time base"));
            }
            let tb = TimeBase::new(num, den);
            if tb.0.num != num || tb.0.den != den {
                return Err(Error::invalid("nut: time base not reduced"));
            }
            self.time_bases.push(tb);
        }

        self.frame_code = vec![FrameCode::default(); 256];
        #[allow(unused_assignments)]
        let (mut tmp_pts, mut tmp_mul, mut tmp_stream, mut tmp_size, mut tmp_res, mut tmp_head_idx) =
            (0i64, 1i64, 0usize, 0i64, 0i64, 0usize);
        let mut i = 0usize;
        while i < 256 {
            let tmp_flags = read_varlen(&mut self.input)? as i64;
            let tmp_fields = read_varlen(&mut self.input)? as usize;
            if tmp_fields > 0 {
                tmp_pts = read_signed_varlen(&mut self.input)?;
            }
            if tmp_fields > 1 {
                tmp_mul = read_varlen(&mut self.input)? as i64;
            }
            if tmp_fields > 2 {
                tmp_stream = read_varlen(&mut self.input)? as usize;
            }
            if tmp_fields > 3 {
                tmp_size = read_varlen(&mut self.input)? as i64;
            } else {
                tmp_size = 0;
            }
            if tmp_fields > 4 {
                tmp_res = read_varlen(&mut self.input)? as i64;
            } else {
                tmp_res = 0;
            }
            let count = if tmp_fields > 5 {
                read_varlen(&mut self.input)? as usize
            } else {
                tmp_mul.saturating_sub(tmp_size) as usize
            };
            if tmp_fields > 6 {
                read_signed_varlen(&mut self.input)?;
            }
            if tmp_fields > 7 {
                tmp_head_idx = read_varlen(&mut self.input)? as usize;
            }
            for _ in (8..tmp_fields).rev() {
                read_varlen(&mut self.input)?;
            }

            if count == 0 || count > 256 - usize::from(i <= b'N' as usize) - i {
                return Err(Error::invalid("nut: illegal frame code count"));
            }
            if tmp_stream >= stream_count {
                return Err(Error::invalid("nut: illegal stream number"));
            }
            if tmp_size < 0 || tmp_size > i32::MAX as i64 - count as i64 {
                return Err(Error::invalid("nut: illegal size"));
            }

            let mut j = 0usize;
            while j < count {
                if i == b'N' as usize {
                    // FFmpeg: frame_code['N'] is invalid and does not consume
                    // one of the entry's count slots (j-- cancels the loop's
                    // j++), but i still advances.
                    self.frame_code[i].flags = FLAG_INVALID;
                    i += 1;
                    continue;
                }
                self.frame_code[i] = FrameCode {
                    flags: tmp_flags,
                    pts_delta: tmp_pts,
                    stream_id: tmp_stream,
                    size_mul: tmp_mul,
                    size_lsb: tmp_size + j as i64,
                    reserved_count: tmp_res,
                    header_idx: tmp_head_idx,
                };
                j += 1;
                i += 1;
            }
        }

        // optional elision headers
        self.header = vec![Vec::new()];
        if end > self.input.stream_position()? + 4 {
            let header_count = read_varlen(&mut self.input)? as usize;
            if header_count >= 128 {
                return Err(Error::invalid("nut: too many elision headers"));
            }
            let mut rem = 1024usize;
            for _ in 1..=header_count {
                let len = read_varlen(&mut self.input)? as usize;
                if len == 0 || len > rem {
                    return Err(Error::invalid("nut: invalid elision header"));
                }
                rem -= len;
                let mut hdr = vec![0u8; len];
                self.input.read_exact(&mut hdr)?;
                self.header.push(hdr);
            }
        }

        if self.input.stream_position()? > end {
            return Err(Error::invalid("nut: main header overlong"));
        }
        self.input.seek(std::io::SeekFrom::Start(end))?;

        self.states = (0..stream_count).map(|_| NutStream { time_base: TimeBase::new(1, 90000), msb_pts_shift: 0, max_pts_distance: 0, last_pts: 0, last_flags: 0 }).collect();
        Ok(true)
    }

    /// decode_stream_header.
    fn decode_stream_header(&mut self, codecs: &dyn CodecResolver) -> Result<bool> {
        let (size, _ck) = read_packet_header(&mut self.input, STREAM_STARTCODE)?;
        let end = self.input.stream_position()? + size as u64;

        let stream_id = read_varlen(&mut self.input)? as usize;
        if stream_id >= self.states.len() {
            return Err(Error::invalid("nut: stream id out of range"));
        }
        let class = read_varlen(&mut self.input)?;
        if class > 3 {
            return Err(Error::unsupported(format!("nut: unknown stream class {class}")));
        }
        let tag = read_fourcc(&mut self.input)?;

        let time_base_id = read_varlen(&mut self.input)? as usize;
        if time_base_id >= self.time_bases.len() {
            return Err(Error::invalid("nut: time base id out of range"));
        }
        let msb_pts_shift = read_varlen(&mut self.input)? as i64;
        if msb_pts_shift >= 16 {
            return Err(Error::invalid("nut: msb_pts_shift out of range"));
        }
        let max_pts_distance = read_varlen(&mut self.input)? as i64;
        let decode_delay = read_varlen(&mut self.input)? as i64;
        if decode_delay >= 1000 {
            return Err(Error::invalid("nut: decode_delay out of range"));
        }
        read_varlen(&mut self.input)?; // stream flags
        let extradata_size = read_varlen(&mut self.input)? as usize;
        if extradata_size > MAX_HEADER_BYTES {
            return Err(Error::invalid("nut: extradata too large"));
        }
        let mut extradata = vec![0u8; extradata_size];
        self.input.read_exact(&mut extradata)?;

        let mut width = 0u32;
        let mut height = 0u32;
        let mut sample_rate = 0u32;
        let mut channels = 0u16;
        match class {
            0 => {
                width = read_varlen(&mut self.input)? as u32;
                height = read_varlen(&mut self.input)? as u32;
                if width == 0 || height == 0 || width > 16384 || height > 16384 {
                    return Err(Error::invalid("nut: invalid video dimensions"));
                }
                read_varlen(&mut self.input)?; // sample aspect ratio num
                read_varlen(&mut self.input)?; // sample aspect ratio den
                read_varlen(&mut self.input)?; // colorspace type
            }
            1 => {
                sample_rate = read_varlen(&mut self.input)? as u32;
                read_varlen(&mut self.input)?; // samplerate_den
                channels = read_varlen(&mut self.input)? as u16;
                if sample_rate == 0 || channels == 0 || channels > 64 {
                    return Err(Error::invalid("nut: invalid audio format"));
                }
            }
            _ => {}
        }

        if self.input.stream_position()? > end {
            return Err(Error::invalid("nut: stream header overlong"));
        }
        self.input.seek(std::io::SeekFrom::Start(end))?;

        let default_tag = CodecTag::fourcc(&[0, 0, 0, 0]);
        let tag = tag.unwrap_or(default_tag);
        let (codec_id, media_type) =
            match nut_codec_id(codecs, class, &tag, sample_rate, channels, width, height) {
                Some(v) => v,
                None => {
                    return Err(Error::codec_not_found(format!(
                        "nut: unknown codec tag for class {class}"
                    )))
                }
            };

        let mut params = match media_type {
            MediaType::Video => CodecParameters::video(codec_id),
            MediaType::Audio => CodecParameters::audio(codec_id),
            MediaType::Subtitle => CodecParameters::subtitle(codec_id),
            MediaType::Data | MediaType::Unknown => CodecParameters::data(codec_id),
        };
        params.extradata = extradata;
        params.tag = Some(tag);
        if media_type == MediaType::Video {
            params.width = Some(width);
            params.height = Some(height);
        } else if media_type == MediaType::Audio {
            params.sample_rate = Some(sample_rate);
            params.channels = Some(channels);
        }

        let tb = self.time_bases[time_base_id];
        self.states[stream_id] = NutStream {
            time_base: tb,
            msb_pts_shift,
            max_pts_distance,
            last_pts: 0,
            last_flags: 0,
        };
        self.streams.push(StreamInfo {
            index: self.streams.len() as u32,
            params,
            time_base: tb,
            duration: None,
            start_time: Some(0),
        });
        Ok(true)
    }

    /// decode_syncpoint, its start code just read: the global PTS in the
    /// syncpoint's time base resets every stream's (ff_nut_reset_ts), and
    /// the syncpoint, with its back pointer and its time in microseconds,
    /// joins the syncpoint tree (ff_nut_add_sp).
    fn decode_syncpoint(&mut self) -> Result<Syncpoint> {
        self.allowance.spend(1, 0)?;
        let pos = self.input.stream_position()? as i64 - 8;
        let (size, _ck) = read_packet_header(&mut self.input, SYNCPOINT_STARTCODE)?;
        let end = self.input.stream_position()? + size as u64;

        let tmp = read_varlen(&mut self.input)?;
        let back_ptr = read_varlen(&mut self.input)?
            .checked_mul(16)
            .and_then(|b| i64::try_from(b).ok())
            .and_then(|b| pos.checked_sub(b))
            .filter(|&b| b >= 0)
            .ok_or_else(|| Error::invalid("nut: syncpoint back pointer before the file"))?;
        let sp_tb = self.time_bases[(tmp % self.time_bases.len() as u64) as usize];
        let sp_val = (tmp / self.time_bases.len() as u64) as i64;
        for st in &mut self.states {
            // av_rescale_rnd(val, sp.num * st.den, sp.den * st.num, DOWN)
            st.last_pts = rescale_down(
                sp_val,
                sp_tb.0.num,
                sp_tb.0.den,
                st.time_base.0.num,
                st.time_base.0.den,
            );
        }
        if self.input.stream_position()? > end {
            return Err(Error::invalid("nut: syncpoint overlong"));
        }
        self.input.seek(std::io::SeekFrom::Start(end))?;
        // tmp / time_base_count * av_q2d(time_base) * AV_TIME_BASE, in
        // double precision as FFmpeg computes it.
        let ts = (sp_val as f64 * (sp_tb.0.num as f64 / sp_tb.0.den as f64) * 1_000_000f64) as i64;
        let sp = Syncpoint { pos, back_ptr, ts };
        if let Err(at) = self.syncpoints.binary_search_by_key(&pos, |s| s.pos) {
            if self.syncpoints.len() < MAX_SYNCPOINTS {
                self.syncpoints.insert(at, sp);
            }
        }
        Ok(sp)
    }

    /// find_startcode: the position of the next `code` from `from` on.
    fn find_startcode(&mut self, code: u64, from: i64) -> Result<Option<i64>> {
        self.input.seek(std::io::SeekFrom::Start(from.max(0) as u64))?;
        loop {
            match find_any_startcode(&mut self.input)? {
                Some((found, at)) if found == code => return Ok(Some(at as i64)),
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }

    /// nut_read_timestamp for stream -1: the time of the first syncpoint
    /// that decodes from `*pos` on, `*pos` moved to it.
    fn read_timestamp(&mut self, pos: &mut i64) -> Result<Option<i64>> {
        let mut from = *pos;
        loop {
            let Some(at) = self.find_startcode(SYNCPOINT_STARTCODE, from)? else { return Ok(None) };
            from = at + 1;
            if let Ok(sp) = self.decode_syncpoint() {
                *pos = at;
                return Ok(Some(sp.ts));
            }
        }
    }

    /// find_and_decode_index up to the payload: the index FFmpeg's muxer
    /// writes at the end of the file, None without one. FFmpeg ignores
    /// get_packetheader's result here and reads the index on from after
    /// its header, to the end of the file (up to MAX_INDEX_BYTES here).
    fn read_index(&mut self) -> Result<Option<Vec<u8>>> {
        let file_size = self.input.seek(std::io::SeekFrom::End(0))? as i64;
        if file_size < 12 {
            return Ok(None);
        }
        self.input.seek(std::io::SeekFrom::Start((file_size - 12) as u64))?;
        let mut ptr = [0u8; 8];
        self.input.read_exact(&mut ptr)?;
        let Some(at) = file_size.checked_sub(i64::from_be_bytes(ptr)).filter(|&at| at >= 0) else { return Ok(None) };
        self.input.seek(std::io::SeekFrom::Start(at as u64))?;
        let mut code = [0u8; 8];
        self.input.read_exact(&mut code)?;
        if u64::from_be_bytes(code) != INDEX_STARTCODE {
            return Ok(None);
        }
        read_packet_size(&mut self.input, INDEX_STARTCODE)?;
        let left = file_size as u64 - self.input.stream_position()?.min(file_size as u64);
        let len = left.min(MAX_INDEX_BYTES as u64);
        let mut data = vec![0u8; len as usize];
        self.input.read_exact(&mut data)?;
        Ok(Some(data))
    }

    /// FFmpeg's read_seek index search for `stream` over the whole index,
    /// decoded again from the file: (position, pts) of the last key frame
    /// at or before `pts`, else the first after it. The index keeps, per
    /// timestamp, the last entry added with it.
    fn search_file_index(&mut self, stream: usize, pts: i64) -> Result<Option<(i64, i64)>> {
        let Some(data) = self.read_index()? else { return Ok(None) };
        let (mut before, mut first): (Option<(i64, i64)>, Option<(i64, i64)>) = (None, None);
        let _ = decode_index(&data, self.streams.len(), |s, pos, ts| {
            if s != stream {
                return;
            }
            if ts <= pts && before.is_none_or(|(b, _)| ts >= b) {
                before = Some((ts, pos));
            }
            if first.is_none_or(|(f, _)| ts <= f) {
                first = Some((ts, pos));
            }
        });
        Ok(before.or(first).map(|(ts, pos)| (pos, ts)))
    }
}

/// find_and_decode_index on the index payload `data` (zeros past its
/// end, as ffio_read_varlen reads past the end of a file): each key frame
/// it lists to `entry`, as (stream, position of the syncpoint before it,
/// pts). Like FFmpeg, entries before an error stand.
fn decode_index(data: &[u8], streams: usize, mut entry: impl FnMut(usize, i64, i64)) -> Result<()> {
    let mut p = 0;
    // ffio_read_varlen: zeros past the end
    let mut v = || -> u64 {
        let mut val = 0u64;
        loop {
            let b = data.get(p).copied().unwrap_or(0);
            p += 1;
            val = (val << 7).wrapping_add(u64::from(b & 127));
            if b & 128 == 0 {
                return val;
            }
        }
    };
    v(); // max_pts
    let count = v();
    if count == 0 || count >= i32::MAX as u64 / 8 || count > data.len() as u64 {
        return Err(Error::invalid("nut: index syncpoint count"));
    }
    let count = count as usize;
    let mut syncpoints = vec![0i64; count];
    for i in 0..count {
        let pos = i64::try_from(v()).ok().filter(|&pos| pos > 0);
        let Some(pos) = pos.and_then(|pos| if i > 0 { pos.checked_add(syncpoints[i - 1]) } else { Some(pos) }) else {
            return Err(Error::invalid("nut: index syncpoint position"));
        };
        syncpoints[i] = pos;
    }
    let mut has_keyframe = vec![false; count + 1];
    for stream in 0..streams {
        let mut last_pts: i64 = -1;
        let mut j = 0;
        while j < count {
            let mut x = v();
            let mut n = j;
            let run = x & 1 != 0;
            x >>= 1;
            if run {
                let flag = x & 1 != 0;
                x >>= 1;
                if x.checked_add(n as u64).is_none_or(|end| end >= count as u64 + 1) {
                    return Err(Error::invalid("nut: index overflow"));
                }
                for _ in 0..x {
                    has_keyframe[n] = flag;
                    n += 1;
                }
                has_keyframe[n] = !flag;
                n += 1;
            } else {
                if x <= 1 {
                    return Err(Error::invalid("nut: index keyframe bits"));
                }
                while x != 1 {
                    if n >= count + 1 {
                        return Err(Error::invalid("nut: index overflow"));
                    }
                    has_keyframe[n] = x & 1 != 0;
                    n += 1;
                    x >>= 1;
                }
            }
            if has_keyframe[0] {
                return Err(Error::invalid("nut: keyframe before the first syncpoint in the index"));
            }
            while j < n && j < count {
                if has_keyframe[j] {
                    let mut a = v();
                    let b = if a == 0 {
                        a = v();
                        v()
                    } else {
                        0
                    };
                    let pts = last_pts.wrapping_add(a as i64);
                    if let Some(pos) = j.checked_sub(1).and_then(|k| syncpoints[k].checked_mul(16)) {
                        entry(stream, pos, pts);
                    }
                    last_pts = pts.wrapping_add(b as i64);
                }
                j += 1;
            }
        }
    }
    Ok(())
}

impl NutDemuxer {
    /// decode_frame_header + decode_frame: one packet or None when the
    /// frame was a header/index/info block that must be skipped.
    fn decode_frame(&mut self, frame_code: u8) -> Result<Option<Packet>> {
        let fc = self.frame_code[frame_code as usize];
        if fc.flags & FLAG_INVALID != 0 {
            return Err(Error::invalid("nut: invalid frame code"));
        }
        let mut flags = fc.flags;
        if flags & FLAG_CODED != 0 {
            flags ^= read_varlen(&mut self.input)? as i64;
        }
        let mut stream_id = fc.stream_id;
        if flags & FLAG_STREAM_ID != 0 {
            stream_id = read_varlen(&mut self.input)? as usize;
            if stream_id >= self.states.len() {
                return Err(Error::invalid("nut: stream id out of range"));
            }
        }
        let mut header_idx = fc.header_idx;
        let pts;
        {
            let st = &self.states[stream_id];
            if flags & FLAG_CODED_PTS != 0 {
                let coded_pts = read_varlen(&mut self.input)? as i64;
                let mask = (1i64 << st.msb_pts_shift) - 1;
                if coded_pts < mask + 1 {
                    // ff_lsb2full
                    let delta = st.last_pts - mask / 2;
                    pts = ((coded_pts - delta) & mask) + delta;
                } else {
                    pts = coded_pts - (mask + 1);
                }
            } else {
                pts = st.last_pts.wrapping_add(fc.pts_delta);
            }
        }
        let mut size = fc.size_lsb;
        if flags & FLAG_SIZE_MSB != 0 {
            size += fc.size_mul
                * read_varlen(&mut self.input)? as i64;
        }
        if flags & FLAG_MATCH_TIME != 0 {
            read_signed_varlen(&mut self.input)?;
        }
        if flags & FLAG_HEADER_IDX != 0 {
            header_idx = read_varlen(&mut self.input)? as usize;
            if header_idx >= self.header.len() {
                return Err(Error::invalid("nut: header_idx invalid"));
            }
        }
        let mut reserved_count = fc.reserved_count;
        if flags & FLAG_RESERVED != 0 {
            reserved_count = read_varlen(&mut self.input)? as i64;
        }
        for _ in 0..reserved_count {
            read_varlen(&mut self.input)?;
        }
        if header_idx >= self.header.len() {
            return Err(Error::invalid("nut: header_idx invalid"));
        }
        if size > 4096 {
            header_idx = 0;
        }
        size -= self.header[header_idx].len() as i64;
        if flags & FLAG_CHECKSUM != 0 {
            let mut cks = [0u8; 4];
            self.input.read_exact(&mut cks)?;
        } else if size > 2 * self.max_distance
            || (self.states[stream_id].last_pts - pts).abs() > self.states[stream_id].max_pts_distance
        {
            return Err(Error::invalid("nut: frame size or pts distance exceeds limits without checksum"));
        }
        if !(0..=MAX_FRAME_SIZE).contains(&size) {
            return Err(Error::invalid("nut: frame size out of range"));
        }

        {
            let st = &mut self.states[stream_id];
            st.last_pts = pts;
            st.last_flags = flags;
        }

        let mut data = self.header[header_idx].clone();
        let header_len = data.len();
        let mut payload = vec![0u8; size as usize];
        self.input.read_exact(&mut payload)?;
        data.extend_from_slice(&payload);
        let _ = header_len;

        if flags & FLAG_SM_DATA != 0 {
            // skip sm data (two sub-blocks) like FFmpeg but without side
            // data extraction: it lives before the payload per the spec —
            // FFmpeg reads it from the buffer; re-scan not needed for the
            // FATE corpus. Keep it simple: ignore.
        }

        let st = &self.states[stream_id];
        let mut pkt = Packet {
            stream_index: stream_id as u32,
            time_base: st.time_base,
            pts: Some(pts),
            dts: None,
            duration: None,
            flags: Default::default(),
            data,
        };
        pkt.flags.keyframe = flags & FLAG_KEY != 0;
        if flags & FLAG_EOR != 0 {
            pkt.flags.discard = true;
        }
        Ok(Some(pkt))
    }
}

/// av_rescale_rnd(AV_ROUND_DOWN): a * b / c without overflow by u128.
fn rescale_down(a: i64, b1: i64, c1: i64, b2: i64, c2: i64) -> i64 {
    // a in sp time base (num1/den1) → stream time base (num2/den2):
    // result = a * num1 * den2 / (den1 * num2), rounded down.
    let num = i128::from(a) * i128::from(b1) * i128::from(c2);
    let den = i128::from(c1) * i128::from(b2);
    if den == 0 {
        return 0;
    }
    (num / den) as i64
}

fn open_nut(
    input: Box<dyn ReadSeek>,
    codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    // nut_read_header: find MAIN_STARTCODE and parse the main header,
    // then stream headers, then skip info headers until a syncpoint.
    let allowance = Allowance::default();
    let mut nut = NutDemuxer {
        input: Box::new(allowance.meter(input)),
        streams: Vec::new(),
        states: Vec::new(),
        time_bases: Vec::new(),
        frame_code: Vec::new(),
        header: Vec::new(),
        max_distance: 0,
        header_end: 0,
        data_offset: 0,
        index: Vec::new(),
        syncpoints: Vec::new(),
        skip_until_key: Vec::new(),
        allowance,
    };

    // main header: FFmpeg loops find+decode until decode succeeds; errors
    // during decode are resyncs.
    let mut found_main = false;
    let mut first_err = None;
    for _ in 0..64 {
        match find_any_startcode(&mut nut.input)? {
            Some((code, _)) if code == MAIN_STARTCODE => {
                match nut.decode_main_header() {
                    Ok(true) => {
                        found_main = true;
                        break;
                    }
                    Ok(false) => continue,
                    Err(e) => {
                        if first_err.is_none() {
                            first_err = Some(format!("{e}"));
                        }
                    }
                }
            }
            Some(_) => continue,
            None => break,
        }
    }
    if !found_main {
        return Err(Error::invalid(format!(
            "nut: no decodable main header (first decode error: {first_err:?})"
        )));
    }

    // stream headers
    let mut last_err = None;
    while nut.streams.len() < nut.states.len() {
        match find_any_startcode(&mut nut.input)? {
            Some((code, _)) if code == STREAM_STARTCODE => {
                match nut.decode_stream_header(codecs) {
                    Ok(true) => continue,
                    Ok(false) => continue,
                    Err(e) => {
                        eprintln!("nut: stream header decode: {e} (streams {}/{} )", nut.streams.len(), nut.states.len());
                        last_err = Some(format!("{e}"));
                    }
                }
            }
            Some(_) => continue,
            None => {
                return Err(Error::invalid(format!(
                    "nut: not all stream headers found (last decode error: {last_err:?})"
                )))
            }
        }
    }

    // info headers / skip to syncpoint
    let mut sync = false;
    loop {
        match find_any_startcode(&mut nut.input)? {
            Some((code, at)) if code == SYNCPOINT_STARTCODE => {
                nut.data_offset = at as i64;
                sync = true;
                break;
            }
            Some((code, _)) if code == INFO_STARTCODE => {
                nut.skip_info_header()?;
            }
            Some(_) => continue,
            None => break,
        }
    }
    if !sync {
        return Err(Error::invalid("nut: EOF before video frames"));
    }

    // Decode the first syncpoint to prime stream PTS bases.
    nut.decode_syncpoint()?;
    nut.header_end = nut.input.stream_position()?;

    // nut_read_header on a seekable input: the index at the end, if any;
    // a broken one only ends early.
    nut.index = (0..nut.streams.len()).map(|_| Index::new(Reduce::Lossy)).collect();
    nut.skip_until_key = vec![false; nut.streams.len()];
    if let Ok(Some(data)) = nut.read_index() {
        let index = &mut nut.index;
        let _ = decode_index(&data, index.len(), |stream, pos, pts| {
            index[stream].add(pos, pts, 0, 0, true);
        });
    }
    nut.input.seek(std::io::SeekFrom::Start(nut.header_end))?;

    Ok(Box::new(nut))
}

impl NutDemuxer {
    /// decode_info_header: parse and discard (metadata is optional).
    fn skip_info_header(&mut self) -> Result<()> {
        let (size, _ck) = read_packet_header(&mut self.input, INFO_STARTCODE)?;
        let end = self.input.stream_position()? + size as u64;
        self.input.seek(std::io::SeekFrom::Start(end))?;
        Ok(())
    }

    /// nut_read_packet's skip_until_key_frame: after a seek a stream's
    /// frames are dropped until its first key frame.
    fn kept(&mut self, packet: Packet) -> Option<Packet> {
        let skip = self.skip_until_key.get_mut(packet.stream_index as usize)?;
        if packet.flags.keyframe {
            *skip = false;
        }
        (!*skip).then_some(packet)
    }
}

impl Demuxer for NutDemuxer {
    fn format_name(&self) -> &str {
        "nut"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            // read one byte; if 'N' read the full 8-byte startcode
            let mut b = [0u8; 1];
            match self.input.read(&mut b) {
                Ok(0) => return Err(Error::Eof),
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }
            if b[0] == b'N' {
                // might be a startcode; read the remaining 7 bytes
                let mut rest = [0u8; 7];
                self.input.read_exact(&mut rest)?;
                let mut code = u64::from(b'N');
                for r in rest {
                    code = (code << 8) | u64::from(r);
                }
                match code {
                    MAIN_STARTCODE | STREAM_STARTCODE | INDEX_STARTCODE => {
                        let (size, _ck) = read_packet_header(&mut self.input, code)?;
                        self.input
                            .seek(std::io::SeekFrom::Current(i64::from(u32::try_from(size).unwrap_or(0))))?;
                    }
                    INFO_STARTCODE => {
                        self.skip_info_header()?;
                    }
                    SYNCPOINT_STARTCODE => {
                        self.decode_syncpoint()?;
                        // frame_code byte follows
                        let mut fb = [0u8; 1];
                        self.input.read_exact(&mut fb)?;
                        if let Some(pkt) = self.decode_frame(fb[0])?.and_then(|p| self.kept(p)) {
                            return Ok(pkt);
                        }
                    }
                    _ => {
                        // not a startcode: re-interpret as a frame code
                        if let Some(pkt) = self.decode_frame(b'N')?.and_then(|p| self.kept(p)) {
                            return Ok(pkt);
                        }
                    }
                }
            } else if let Some(pkt) = self.decode_frame(b[0])?.and_then(|p| self.kept(p)) {
                return Ok(pkt);
            }
        }
    }

    /// nutdec.c read_seek with AVSEEK_FLAG_BACKWARD. With the index FFmpeg's
    /// muxer writes: the stream's last key frame at or before the target
    /// (else the first after it), from the syncpoint before it. Without
    /// one: ff_gen_search over the syncpoint times (nut_read_timestamp),
    /// within the syncpoints read so far, then the landing syncpoint's
    /// back pointer. Reading resumes at a syncpoint and drops each
    /// stream's frames until its first key frame. The seek reads within
    /// its allowance; one that fails, its reposition to the landing
    /// included, leaves reading where it was, with the stream timestamps
    /// the syncpoints it decoded reset.
    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        let stream = stream_index as usize;
        let Some(info) = self.streams.get(stream) else {
            return Err(Error::invalid("nut: no such stream to seek"));
        };
        let tb = info.time_base;
        let resume = self.input.stream_position()?;
        let states = self.states.clone();
        self.allowance.start();
        let landed = self.land(stream, tb, pts);
        let landed = match self.allowance.finish(landed) {
            Ok((pos, landed)) => self.input.seek(std::io::SeekFrom::Start(pos)).map(|_| landed).map_err(Error::from),
            Err(e) => Err(e),
        };
        match landed {
            Ok(landed) => {
                self.skip_until_key.iter_mut().for_each(|skip| *skip = true);
                Ok(landed)
            }
            Err(e) => {
                self.input.seek(std::io::SeekFrom::Start(resume))?;
                self.states = states;
                Err(e)
            }
        }
    }
}

impl NutDemuxer {
    /// read_seek's search: the syncpoint reading resumes at, and the pts
    /// landed on.
    fn land(&mut self, stream: usize, tb: TimeBase, pts: i64) -> Result<(u64, i64)> {
        let (pos2, landed) = if !self.index[stream].entries().is_empty() {
            // An index over MAX_INDEX_ENTRIES lost entries FFmpeg keeps.
            let found = if self.index[stream].lossy() {
                self.search_file_index(stream, pts)?
            } else {
                let index = &self.index[stream];
                index.search(pts, true).or_else(|| index.search(pts, false)).map(|i| {
                    let e = index.entries()[i];
                    (e.pos, e.timestamp)
                })
            };
            found.ok_or_else(|| Error::invalid("nut: no key frame to seek to"))?
        } else {
            // pts * av_q2d(time_base) * AV_TIME_BASE
            let target = (pts as f64 * (tb.0.num as f64 / tb.0.den as f64) * 1_000_000f64) as i64;
            let before = self.syncpoints.iter().rev().find(|s| s.ts < target).copied();
            let after = self.syncpoints.iter().find(|s| s.ts > target).copied();
            let bounds = Bounds {
                pos_min: before.map_or(0, |s| s.pos),
                pos_max: after.map_or(0, |s| s.pos),
                pos_limit: after.map_or(0, |s| s.pos),
                ts_min: before.map(|s| s.ts),
                ts_max: after.map(|s| s.ts),
            };
            let file_size = self.input.seek(std::io::SeekFrom::End(0))? as i64;
            let data_offset = self.data_offset;
            let found = gen_search(target, bounds, data_offset, file_size, &mut |pos, _| self.read_timestamp(pos))?;
            let Some((pos, ts)) = found else {
                return Err(Error::invalid("nut: no syncpoint to seek to"));
            };
            let Ok(at) = self.syncpoints.binary_search_by_key(&pos, |s| s.pos) else {
                return Err(Error::invalid("nut: the search landed off the syncpoints"));
            };
            let landed = (ts as f64 / (tb.0.num as f64 / tb.0.den as f64) / 1_000_000f64) as i64;
            (self.syncpoints[at].back_ptr - 15, landed)
        };
        let Some(pos) = self.find_startcode(SYNCPOINT_STARTCODE, pos2)? else {
            return Err(Error::invalid("nut: no syncpoint at the seek position"));
        };
        Ok((pos as u64, landed))
    }
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("nut", open_nut);
    reg.register_probe("nut", nut_probe);
    reg.register_extension("nut", "nut");
}
