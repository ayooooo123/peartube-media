// Ported from FFmpeg (commit 2da55bf): libavformat/dv.c (dv_extract_pack,
// dv_extract_audio, dv_extract_audio_info, dv_extract_video_info,
// dv_init_demux, avpriv_dv_get_packet, avpriv_dv_produce_packet,
// dv_frame_offset, ff_dv_ts_reset, dv_read_header, dv_read_packet,
// dv_read_seek, dv_probe; dv_audio_12to16 is in audio.rs) and dv.h (pack
// types, DV_TIMESCALE_VIDEO/AUDIO).
// License: LGPL-2.1-or-later

//! The raw DV demuxer `dv`: a stream of whole DIF frames. Each frame comes
//! out as a `dvvideo` packet, followed by one packet of 16-bit PCM per
//! stereo pair carried in its audio DIF blocks (12-bit nonlinear samples
//! widened as FFmpeg does), timed as FFmpeg times them.
//!
//! FFmpeg adds audio streams as frames reveal them while it analyses the
//! input; here they are taken from the first frame that carries audio
//! (within FFmpeg's 5,000,000-byte analysis window), read at open.

use std::io::{BufReader, Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, MediaType, Packet, ProbeData, ProbeScore, ReadSeek,
    Rational, Result, SampleFormat, StreamInfo, TimeBase,
};

use crate::audio::audio_12to16;
use crate::profile::{frame_profile, DvProfile, DV_MAX_FRAME_SIZE, DV_PROFILE_BYTES};

/// DV_TIMESCALE_VIDEO: the LCM of video framerate numerators.
const DV_TIMESCALE_VIDEO: i64 = 60_000;
/// DV_TIMESCALE_AUDIO: the LCM of audio sample rates.
const DV_TIMESCALE_AUDIO: i64 = 14_112_000;
const VIDEO_TB: (i64, i64) = (1, DV_TIMESCALE_VIDEO);
const AUDIO_TB: (i64, i64) = (1, DV_TIMESCALE_AUDIO);
/// The bytes avformat_find_stream_info reads (probesize).
const ANALYSIS_BYTES: u64 = 5_000_000;
/// DVPackType.
const DV_TIMECODE: u8 = 0x13;
const DV_AUDIO_SOURCE: u8 = 0x50;
const DV_AUDIO_CONTROL: u8 = 0x51;
const DV_VIDEO_CONTROL: u8 = 0x61;
const DV_AUDIO_FREQUENCY: [u32; 3] = [48_000, 44_100, 32_000];
/// The size of each audio buffer.
const AUDIO_BUF: usize = 8192;

/// av_rescale_q: `a` from time base `b` to `c`, rounded to nearest, halves
/// away from zero.
fn rescale(a: i64, b: (i64, i64), c: (i64, i64)) -> i64 {
    let num = i128::from(b.0) * i128::from(c.1);
    let den = i128::from(c.0) * i128::from(b.1);
    let v = i128::from(a) * num;
    let r = if v < 0 { -((-v + den / 2) / den) } else { (v + den / 2) / den };
    r.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// dv_extract_pack: the offset of the pack of type `t` in `frame`.
fn extract_pack(frame: &[u8], t: u8) -> Option<usize> {
    let mut offs = 0;
    for c in 0..10usize {
        offs = match t {
            DV_AUDIO_SOURCE if c & 1 == 1 => 80 * 6 + 3 + c * 12000,
            DV_AUDIO_SOURCE => 80 * 6 + 80 * 16 * 3 + 3 + c * 12000,
            DV_AUDIO_CONTROL if c & 1 == 1 => 80 * 6 + 80 * 16 + 3 + c * 12000,
            DV_AUDIO_CONTROL => 80 * 6 + 80 * 16 * 4 + 3 + c * 12000,
            DV_VIDEO_CONTROL if c & 1 == 1 => 80 * 3 + 8 + c * 12000,
            DV_VIDEO_CONTROL => 80 * 5 + 48 + 5 + c * 12000,
            DV_TIMECODE => 80 + 3 + 3,
            _ => return None,
        };
        if frame.get(offs) == Some(&t) {
            break;
        }
    }
    (frame.get(offs) == Some(&t)).then_some(offs)
}

/// dv_extract_audio: the PCM of each stereo pair whose buffer is given
/// (`ppcm[k]` names the audio buffer of PCM pair k); the bytes per pair.
fn extract_audio(frame: &[u8], ppcm: &[Option<usize>; 5], bufs: &mut [Vec<u8>; 4], sys: &DvProfile) -> Option<usize> {
    let as_pack = extract_pack(frame, DV_AUDIO_SOURCE)?;
    let smpls = usize::from(frame[as_pack + 1] & 0x3f);
    let freq = usize::from(frame[as_pack + 4] >> 3 & 0x07);
    let quant = frame[as_pack + 4] & 0x07;
    if quant > 1 || freq >= DV_AUDIO_FREQUENCY.len() {
        return None;
    }
    let size = (sys.audio_min_samples[freq] + smpls) * 4;
    let half_ch = sys.difseg_size / 2;
    // 720p frames come in halves: the first carries pairs 2 and 3
    let mut ipcm = if sys.height == 720 && frame[1] & 0x0C == 0 { 2 } else { 0 };
    if ipcm + sys.n_difchan > if quant == 1 { 2 } else { 4 } {
        return None;
    }
    let mut at = 0usize;
    let byte = |k: usize| frame.get(k).copied().unwrap_or(0);
    let mut put = |pcm: usize, of: usize, value: u16| {
        if let Some(b) = bufs[pcm].get_mut(of * 2..of * 2 + 2) {
            b.copy_from_slice(&value.to_le_bytes());
        }
    };
    for _ in 0..sys.n_difchan {
        let next = ppcm.get(ipcm).copied().flatten();
        ipcm += 1;
        let Some(mut pcm) = next else { break };
        for i in 0..sys.difseg_size {
            at += 6 * 80; // the DIF segment header
            if quant == 1 && i == half_ch {
                // the next stereo pair (12-bit only)
                let next = ppcm.get(ipcm).copied().flatten();
                ipcm += 1;
                let Some(next) = next else { break };
                pcm = next;
            }
            for j in 0..9 {
                let mut d = 8;
                while d < 80 {
                    let at_d = at + d;
                    if quant == 0 {
                        // 16-bit linear
                        let of = usize::from(sys.audio_shuffle[i][j]) + (d - 8) / 2 * sys.audio_stride;
                        if of * 2 >= size {
                            d += 2;
                            continue;
                        }
                        // erroneous 0x8000 samples are silenced
                        let (hi, lo) = (byte(at_d), byte(at_d + 1));
                        let hi = if hi == 0x80 && lo == 0x00 { 0 } else { hi };
                        put(pcm, of, u16::from_le_bytes([lo, hi]));
                    } else {
                        // 12-bit nonlinear
                        let lc = (u16::from(byte(at_d)) << 4) | (u16::from(byte(at_d + 2)) >> 4);
                        let rc = (u16::from(byte(at_d + 1)) << 4) | (u16::from(byte(at_d + 2)) & 0x0f);
                        let lc = if lc == 0x800 { 0 } else { audio_12to16(lc) };
                        let rc = if rc == 0x800 { 0 } else { audio_12to16(rc) };
                        let of = usize::from(sys.audio_shuffle[i % half_ch][j]) + (d - 8) / 3 * sys.audio_stride;
                        // FFmpeg's continue skips the third byte's step too
                        if of * 2 >= size {
                            d += 2;
                            continue;
                        }
                        put(pcm, of, lc);
                        let of = usize::from(sys.audio_shuffle[i % half_ch + half_ch][j]) + (d - 8) / 3 * sys.audio_stride;
                        put(pcm, of, rc);
                        d += 1;
                    }
                    d += 2;
                }
                at += 16 * 80; // 15 video DIFs + 1 audio DIF
            }
        }
    }
    Some(size)
}

/// DVPacket: an audio packet waiting to be returned.
#[derive(Clone, Copy, Default)]
struct AudioPkt {
    pts: i64,
    size: usize,
    duration: i64,
    sample_rate: u32,
}

/// DVDemuxContext.
struct DvContext {
    sys: Option<&'static DvProfile>,
    /// Which of FFmpeg's four audio streams exist.
    created: [bool; 4],
    audio_pkt: [AudioPkt; 4],
    audio_buf: [Vec<u8>; 4],
    ach: usize,
    frames: i64,
    next_pts_video: i64,
    /// None for AV_NOPTS_VALUE.
    next_pts_audio: Option<i64>,
}

impl DvContext {
    fn new(sys: &'static DvProfile) -> Self {
        Self {
            sys: Some(sys),
            created: [false; 4],
            audio_pkt: [AudioPkt::default(); 4],
            audio_buf: std::array::from_fn(|_| vec![0; AUDIO_BUF]),
            ach: 0,
            frames: 0,
            next_pts_video: 0,
            next_pts_audio: Some(0),
        }
    }

    /// dv_extract_audio_info: the audio streams of `frame`, made where new;
    /// the bytes of each pair's PCM.
    fn extract_audio_info(&mut self, frame: &[u8], sys: &DvProfile) -> usize {
        let Some(as_pack) = extract_pack(frame, DV_AUDIO_SOURCE) else {
            self.ach = 0;
            return 0;
        };
        let smpls = usize::from(frame[as_pack + 1] & 0x3f);
        let freq = usize::from(frame[as_pack + 4] >> 3 & 0x07);
        let stype = frame[as_pack + 3] & 0x1f;
        let quant = frame[as_pack + 4] & 0x07;
        // An unknown rate keeps the last frame's pairs.
        let Some(&sr) = DV_AUDIO_FREQUENCY.get(freq) else { return 0 };
        if stype > 3 {
            self.ach = 0;
            return 0;
        }
        // ach counts stereo pairs
        let mut ach = [1, 0, 2, 4][usize::from(stype)];
        if ach == 1 && quant != 0 && freq == 2 {
            ach = 2;
        }
        self.ach = 0;
        for i in 0..ach {
            if !self.created[i] {
                self.created[i] = true;
                self.audio_pkt[i] = AudioPkt { pts: 0, size: 0, duration: 0, sample_rate: sr };
            }
            self.audio_pkt[i].sample_rate = sr;
        }
        self.ach = ach;
        (sys.audio_min_samples[freq] + smpls) * 4
    }

    /// avpriv_dv_produce_packet: queues the audio of the frame in `buf`
    /// (`buf_size` bytes taken as the frame); its video packet's (pts,
    /// duration).
    fn produce(&mut self, buf: &[u8], buf_size: usize) -> Result<(i64, i64, usize)> {
        if buf_size < DV_PROFILE_BYTES {
            return Err(Error::invalid("dv: frame shorter than its header"));
        }
        self.sys = frame_profile(self.sys, buf, buf_size, false);
        let sys = self.sys.filter(|s| buf_size >= s.frame_size).ok_or_else(|| Error::invalid("dv: no DV frame profile"))?;
        // queueing audio packets
        let size = self.extract_audio_info(buf, sys);
        let (mut pts, mut duration) = (0, 0);
        if self.ach > 0 {
            let next_pts_video = rescale(self.next_pts_video, VIDEO_TB, AUDIO_TB);
            duration = rescale((size / 4) as i64, (1, i64::from(self.audio_pkt[0].sample_rate)), AUDIO_TB);
            // more than a frame from the video: resynchronise
            pts = match self.next_pts_audio {
                Some(next) if (i128::from(next_pts_video) - i128::from(next)).abs() < i128::from(duration) => next,
                _ => next_pts_video,
            };
            self.next_pts_audio = Some(pts + duration);
        }
        let mut ppcm = [None; 5];
        for (i, slot) in ppcm.iter_mut().enumerate().take(self.ach) {
            self.audio_pkt[i].size = size;
            self.audio_pkt[i].pts = pts;
            self.audio_pkt[i].duration = duration;
            *slot = Some(i);
        }
        if self.ach > 0 {
            extract_audio(buf, &ppcm, &mut self.audio_buf, sys);
        }
        if sys.height == 720 {
            let half = if buf[1] & 0x0C != 0 { 2 } else { 0 };
            self.audio_pkt[half].size = 0;
            self.audio_pkt[half + 1].size = 0;
        }
        // the video packet (dv_extract_video_info)
        let pts = self.next_pts_video;
        let duration = rescale(1, sys.time_base, VIDEO_TB);
        self.next_pts_video += duration;
        self.frames += 1;
        Ok((pts, duration, sys.frame_size))
    }

    /// ff_dv_ts_reset. `audio_known`: FFmpeg made the first audio stream
    /// while it analysed the input.
    fn ts_reset(&mut self, ts: i64, audio_known: bool) {
        self.frames = self.sys.map_or(0, |s| rescale(ts, VIDEO_TB, s.time_base));
        self.next_pts_video = ts;
        self.next_pts_audio = (self.sys.is_some() && (self.created[0] || audio_known)).then(|| rescale(ts, VIDEO_TB, AUDIO_TB));
        for p in &mut self.audio_pkt {
            p.size = 0;
        }
    }
}

/// avio over the input: the byte position and end-of-file flag kept.
struct Io {
    inner: BufReader<Box<dyn ReadSeek>>,
    pos: u64,
    eof: bool,
}

impl Io {
    /// avio_r8: 0 at the end.
    fn r8(&mut self) -> u8 {
        let mut b = [0u8];
        match self.inner.read(&mut b) {
            Ok(1) => {
                self.pos += 1;
                b[0]
            }
            _ => {
                self.eof = true;
                0
            }
        }
    }

    fn rb32(&mut self) -> u32 {
        (0..4).fold(0, |v, _| (v << 8) | u32::from(self.r8()))
    }

    /// avio_read: up to `dst.len()` bytes.
    fn read(&mut self, dst: &mut [u8]) -> Result<usize> {
        let mut n = 0;
        while n < dst.len() {
            match self.inner.read(&mut dst[n..]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.pos += n as u64;
        Ok(n)
    }

    fn seek(&mut self, pos: u64) -> Result<()> {
        self.inner.seek(SeekFrom::Start(pos))?;
        self.pos = pos;
        self.eof = false;
        Ok(())
    }
}

/// The raw DV demuxer.
struct DvDemuxer {
    io: Io,
    streams: Vec<StreamInfo>,
    /// Where the first frame starts.
    data_offset: u64,
    file_size: Option<u64>,
    duration_micros: Option<i64>,
    c: DvContext,
    buf: Vec<u8>,
}

/// dv_read_header's search for the first frame: where it starts.
fn find_header(io: &mut Io) -> Result<u64> {
    let mut marker_pos = 0u64;
    let mut state = io.rb32();
    while state & 0xffffff7f != 0x1f07003f {
        if io.eof {
            return Err(Error::invalid("dv: cannot find DV header"));
        }
        if state == 0x003f0700 || state == 0xff3f0700 {
            marker_pos = io.pos;
        }
        if state == 0xff3f0701 && io.pos.wrapping_sub(marker_pos) == 80 {
            return io.pos.checked_sub(163).ok_or_else(|| Error::invalid("dv: header before the input"));
        }
        state = (state << 8) | u32::from(io.r8());
    }
    Ok(io.pos - 4)
}

fn open(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut inner = BufReader::with_capacity(64 * 1024, input);
    let file_size = inner.seek(SeekFrom::End(0)).ok();
    inner.seek(SeekFrom::Start(0))?;
    let mut io = Io { inner, pos: 0, eof: false };
    let data_offset = find_header(&mut io)?;
    io.seek(data_offset)?;
    let mut buf = vec![0u8; DV_MAX_FRAME_SIZE];
    if io.read(&mut buf[..DV_PROFILE_BYTES])? < DV_PROFILE_BYTES {
        return Err(Error::invalid("dv: input ends in the first frame's header"));
    }
    let sys = frame_profile(None, &buf, DV_PROFILE_BYTES, false)
        .ok_or_else(|| Error::invalid("dv: can't determine profile of DV input stream"))?;
    // The audio streams: those of the first frame with audio in FFmpeg's
    // analysis window.
    io.seek(data_offset)?;
    let mut scan = DvContext::new(sys);
    let mut analysed = 0u64;
    let mut audio: Option<(usize, u32)> = None;
    while analysed < ANALYSIS_BYTES && audio.is_none() {
        let Some(frame_sys) = scan.sys else { break };
        let size = frame_sys.frame_size;
        let n = io.read(&mut buf[..size])?;
        if n == 0 || scan.produce(&buf, size).is_err() {
            break;
        }
        analysed += size as u64;
        if scan.ach > 0 {
            audio = Some((scan.ach, scan.audio_pkt[0].sample_rate));
        }
    }
    io.seek(data_offset)?;

    // s->bit_rate, and the durations estimate_timings_from_bit_rate gives.
    let bit_rate = rescale(sys.frame_size as i64, (8, 1), sys.time_base);
    let data_size = file_size.map(|s| s.saturating_sub(data_offset) as i64);
    let estimate = |tb: (i64, i64)| data_size.filter(|_| bit_rate > 0).map(|size| rescale(size, (8, bit_rate), tb));
    let mut video = CodecParameters::video(CodecId::new("dvvideo"));
    video.width = Some(sys.width as u32);
    video.height = Some(sys.height as u32);
    video.pixel_format = Some(sys.pix_fmt);
    video.frame_rate = Some(Rational::new(sys.time_base.1, sys.time_base.0));
    video.bit_rate = Some(bit_rate as u64);
    let mut streams = vec![StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, DV_TIMESCALE_VIDEO),
        duration: estimate(VIDEO_TB),
        start_time: Some(0),
        params: video,
    }];
    if let Some((pairs, rate)) = audio {
        for i in 0..pairs {
            let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
            params.sample_rate = Some(rate);
            params.channels = Some(2);
            params.sample_format = Some(SampleFormat::S16);
            params.bit_rate = Some(2 * u64::from(rate) * 16);
            streams.push(StreamInfo {
                index: 1 + i as u32,
                time_base: TimeBase::new(1, DV_TIMESCALE_AUDIO),
                duration: estimate(AUDIO_TB),
                start_time: Some(0),
                params,
            });
        }
    }
    debug_assert!(streams.iter().all(|s| s.params.media_type == MediaType::Video || s.params.media_type == MediaType::Audio));
    Ok(Box::new(DvDemuxer {
        io,
        streams,
        data_offset,
        file_size,
        duration_micros: estimate((1, 1_000_000)),
        c: DvContext::new(sys),
        buf,
    }))
}

impl DvDemuxer {
    /// avpriv_dv_get_packet: the first queued audio packet (of a stream
    /// this demuxer has).
    fn audio_packet(&mut self) -> Option<Packet> {
        for i in 0..self.c.ach {
            let p = self.c.audio_pkt[i];
            if !self.c.created[i] || p.size == 0 {
                continue;
            }
            self.c.audio_pkt[i].size = 0;
            let Some(stream) = self.streams.get(1 + i) else { continue };
            let mut packet = Packet::new(stream.index, stream.time_base, self.c.audio_buf[i][..p.size.min(AUDIO_BUF)].to_vec());
            packet.pts = Some(p.pts);
            packet.dts = Some(p.pts);
            packet.duration = Some(p.duration);
            packet.flags.keyframe = true;
            return Some(packet);
        }
        None
    }
}

impl Demuxer for DvDemuxer {
    fn format_name(&self) -> &str {
        "dv"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    /// dv_read_packet: the frame's video packet, then its audio packets.
    fn next_packet(&mut self) -> Result<Packet> {
        if let Some(packet) = self.audio_packet() {
            return Ok(packet);
        }
        let sys = self.c.sys.ok_or_else(|| Error::invalid("dv: no DV frame profile"))?;
        let size = sys.frame_size;
        let n = self.io.read(&mut self.buf[..size])?;
        if n == 0 {
            return Err(Error::Eof);
        }
        // A short last frame keeps the bytes of the one before it.
        let (pts, duration, size) = self.c.produce(&self.buf, size)?;
        let stream = &self.streams[0];
        let mut packet = Packet::new(0, stream.time_base, self.buf[..size].to_vec());
        packet.pts = Some(pts);
        packet.dts = Some(pts);
        packet.duration = Some(duration);
        packet.flags.keyframe = true;
        Ok(packet)
    }

    /// dv_read_seek: to the frame nearest `pts`, by its offset.
    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        let stream = self.streams.get(stream_index as usize).ok_or_else(|| Error::invalid("dv: no such stream"))?;
        let tb = stream.time_base.as_rational();
        let tb = (i64::from(tb.num), i64::from(tb.den));
        let sys = self.c.sys.ok_or_else(|| Error::invalid("dv: no DV frame profile"))?;
        let mut timestamp = if stream_index == 0 { pts } else { rescale(pts, tb, VIDEO_TB) };
        // dv_frame_offset
        let frame_size = sys.frame_size as i64;
        let frame_count = rescale(timestamp, VIDEO_TB, sys.time_base);
        let size = self.file_size.map_or(-1, |s| s as i64 - self.data_offset as i64);
        let max_offset = ((size - 1) / frame_size) * frame_size;
        let mut offset = frame_size.saturating_mul(frame_count);
        if size >= 0 && offset > max_offset {
            offset = max_offset;
        } else if offset < 0 {
            offset = 0;
        }
        timestamp = rescale(offset / frame_size, sys.time_base, VIDEO_TB);
        self.io.seek(self.data_offset + offset as u64)?;
        self.c.ts_reset(timestamp, self.streams.len() > 1);
        Ok(if stream_index == 0 { timestamp } else { rescale(timestamp, VIDEO_TB, tb) })
    }

    fn duration_micros(&self) -> Option<i64> {
        self.duration_micros
    }
}

/// dv_probe.
fn probe(p: &ProbeData) -> ProbeScore {
    let buf = p.buf;
    if buf.len() < 5 {
        return 0;
    }
    let mut marker_pos = 0usize;
    let (mut matches, mut firstmatch, mut secondary_matches) = (0usize, false, 0usize);
    for i in 0..buf.len() - 4 {
        let state = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
        if state & 0x0007f840 == 0x00070000 {
            // any section header, also with seq/chan num != 0, should
            // appear around every 12000 bytes, at least 10 per frame
            if state & 0xff07ff7f == 0x1f07003f {
                secondary_matches += 1;
                if state & 0xffffff7f == 0x1f07003f {
                    matches += 1;
                    if i == 0 {
                        firstmatch = true;
                    }
                }
            }
            if state == 0x003f0700 || state == 0xff3f0700 {
                marker_pos = i;
            }
            if state == 0xff3f0701 && i.wrapping_sub(marker_pos) == 80 {
                matches += 1;
            }
        }
    }
    if matches > 0 && buf.len() / matches < 1024 * 1024 {
        if matches > 4 || firstmatch || (secondary_matches >= 10 && buf.len() / secondary_matches < 24000) {
            // AVPROBE_SCORE_MAX * 3 / 4, not max to avoid dv in mov to match
            return 75;
        }
        // AVPROBE_SCORE_MAX / 4
        return 25;
    }
    0
}

/// Registers the `dv` demuxer, its probe and the .dv/.dif extensions.
pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("dv", open);
    reg.register_probe("dv", probe);
    reg.register_extension("dv", "dv");
    reg.register_extension("dif", "dv");
}
