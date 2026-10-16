//! One native-route playback: the producer's demuxer, the segment plan and
//! the stream configs worked out up front, then the HTTP server AVPlayer
//! reads the playlists and segments from. An audio switch publishes a new
//! generation of the playlists under a new path.

use std::io::{Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxideav_core::{AudioTrim, ChannelLayout, Demuxer, Packet, RuntimeContext, StreamInfo, TimeBase};
use parking_lot::Mutex;

use super::bridge::{self, DecoderFactory};
use super::codec;
use super::fmp4::{self, Track, TrackKind};
use super::hls::{self, Variant};
use super::plan::{self, Plan, TARGET_SECS};
use super::producer::{Bridging, Producer, Streams, AUDIO_TRACK, VIDEO_TIMESCALE, VIDEO_TRACK};
use super::server::{Body, Resources, Server};
use super::types::{AudioConfig, VideoConfig};
use crate::source::{open_source, SourceMonitor};

const PLAYLIST: &str = "application/vnd.apple.mpegurl";
const MP4: &str = "video/mp4";

/// Packets read at most while looking for the first video keyframe and the
/// first audio packet.
const FIRST_PACKETS: usize = 20_000;
/// Time allowed for probing an MP4 sample table for keyframes.
const PROBE_BUDGET: Duration = Duration::from_secs(2);
/// Audio read at the start: the first packet for a copied track's
/// configuration; for a bridged one's, what its decoder needs before it
/// sounds (mid-stream, TrueHD waits up to 128 access units of 0.83 ms for a
/// major sync), up to a second of it.
const FIRST_AUDIO_PACKETS: usize = 256;
const FIRST_AUDIO_SECS: f64 = 1.0;

/// The engine's decoder for an audio stream, for the bridge.
pub(crate) type BridgeProvider = Arc<dyn Fn(&StreamInfo) -> DecoderFactory + Send + Sync>;

/// Everything worked out before the first segment.
pub(crate) struct Prepared {
    demuxer: Box<dyn Demuxer>,
    streams: Streams,
    plan: Plan,
    bandwidth: (u64, Option<u64>),
    bridge: BridgeProvider,
}

impl Prepared {
    pub(crate) fn video_config(&self) -> &VideoConfig {
        &self.streams.video_config
    }
}

/// Opens `url` a second time for the producer and prepares the native route
/// for `video` and `audio`; `Err` says why the playback is not eligible.
/// `keep` receives the new source's monitor (stopping it unblocks any read);
/// `wrap` is the engine's decode-order repair; `bridge` makes the engine's
/// decoder for an audio track AVPlayer cannot take as it is; `stopped`
/// aborts.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare(
    ctx: &RuntimeContext,
    url: &str,
    container: &str,
    video: &StreamInfo,
    audio: Option<&StreamInfo>,
    duration_secs: f64,
    keep: &dyn Fn(SourceMonitor),
    wrap: &dyn Fn(Box<dyn Demuxer>) -> Box<dyn Demuxer>,
    bridge: BridgeProvider,
    stopped: &AtomicBool,
) -> Result<Prepared, String> {
    let pts_only = matches!(container, "matroska" | "webm");
    if !pts_only && !matches!(container, "mp4" | "mov") {
        return Err(format!("{container} has no keyframe index to plan from"));
    }
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return Err("unknown duration".into());
    }
    let mut source = open_source(url).map_err(|e| format!("open: {e}"))?;
    keep(source.monitor());
    let length = source.seek(SeekFrom::End(0)).ok();
    source.seek(SeekFrom::Start(0)).map_err(|e| format!("rewind: {e}"))?;
    let (demuxer, cues, dovi_record) = if pts_only {
        let mkv = oxideav_mkv::demux::open_typed(Box::new(source), &ctx.codecs).map_err(|e| format!("demuxer: {e}"))?;
        let keys = mkv_keyframes(&mkv, video.index);
        let dovi = mkv
            .block_addition_mappings(video.index)
            .iter()
            .find(|m| {
                (m.addid_type == 0x6476_6343
                    || m.addid_type == 0x6476_7643
                    || m.name.as_deref().is_some_and(|n| n.contains("Dolby Vision")))
                    && m.extra_data.as_ref().is_some_and(|d| d.len() >= 24)
            })
            .and_then(|m| m.extra_data.clone())
            .or_else(|| codec::find_dovi_box(&video.params.extradata).map(|b| b.to_vec()));
        (Box::new(mkv) as Box<dyn Demuxer>, Some(keys), dovi)
    } else {
        let dovi = scan_mp4_dovi(&mut source).or_else(|| codec::find_dovi_box(&video.params.extradata).map(|b| b.to_vec()));
        let demuxer = ctx.containers.open_demuxer(container, Box::new(source), &ctx.codecs).map_err(|e| format!("demuxer: {e}"))?;
        (demuxer, None, dovi)
    };
    let mut demuxer = wrap(demuxer);
    let mut active = vec![video.index];
    active.extend(audio.map(|a| a.index));
    demuxer.set_active_streams(&active);
    let tb = video.time_base;
    let keys = match cues {
        Some(keys) => keys,
        None => probe_keyframes(&mut *demuxer, video.index, tb, duration_secs, stopped)?,
    };
    if !plan::trusted(&keys, tb, duration_secs) {
        let last = keys.iter().max().map_or(f64::NAN, |&k| tb.seconds_of(k));
        return Err(format!(
            "the container's index cannot be trusted ({} keyframes, the last at {last:.3} s of {duration_secs:.3} s)",
            keys.len()
        ));
    }
    let first_key = *keys.iter().min().expect("trusted keys are not empty");
    let last_key = *keys.iter().max().expect("trusted keys are not empty");

    // The first keyframe and audio packet: the codec configurations.
    seek(&mut *demuxer, video.index, first_key)?;
    let first = first_packets(&mut *demuxer, video, audio, stopped)?;
    let mut video_config = codec::video_config(&video.params, &first.keyframe.data).ok_or("no usable H.264/HEVC configuration")?;
    codec::apply_dovi(
        &mut video_config,
        dovi_record.as_deref(),
        &first.keyframe.data,
        codec::video_framing(&video.params),
    );
    if video_config.interlaced {
        return Err("interlaced video".into());
    }
    let (audio, bridging) = match audio {
        Some(stream) => {
            let (config, bridging) = audio_track(stream, &first.audio, &bridge)?;
            (Some((stream.clone(), config)), bridging)
        }
        None => (None, None),
    };
    let frame_ticks = video_config
        .frame_rate
        .filter(|fps| (1.0..=500.0).contains(fps))
        .map(|fps| tb.ticks_of(1.0 / fps))
        .filter(|&ticks| ticks > 0)
        .or(first.keyframe.duration.filter(|&d| d > 0))
        .unwrap_or_else(|| tb.ticks_of(0.04).max(1));
    let depth = i64::from(video_config.reorder_depth);
    let origin = if pts_only {
        first.earliest_pts - depth * frame_ticks
    } else {
        first.keyframe.dts.or(first.keyframe.pts).unwrap_or(0)
    };

    // The end: the last picture's, read from the last keyframe on.
    seek(&mut *demuxer, video.index, last_key)?;
    let end = last_picture_end(&mut *demuxer, video.index, frame_ticks, stopped)?;
    let plan = plan::plan(&keys, tb, end).ok_or("no keyframe before the end")?;
    let bandwidth = length.map_or((40_000_000, None), |bytes| {
        let average = (bytes as f64 * 8.0 / duration_secs) as u64;
        ((average * 2).max(2_000_000), Some(average))
    });
    Ok(Prepared {
        demuxer,
        streams: Streams {
            video: video.clone(),
            video_config,
            framing: codec::video_framing(&video.params),
            synthesize_dts: pts_only,
            frame_ticks,
            audio,
            bridging,
            origin,
        },
        plan,
        bandwidth,
        bridge,
    })
}

/// An audio track's configuration: copied when AVPlayer takes it from
/// fMP4 as it is, else made FLAC by the bridge (`Some` bridging).
fn audio_track(
    stream: &StreamInfo,
    packets: &[(Packet, Option<AudioTrim>)],
    bridge: &BridgeProvider,
) -> Result<(AudioConfig, Option<Bridging>), String> {
    let (first, _) = packets.first().ok_or("the audio track has no packets")?;
    if let Some(mut config) = codec::audio_config(&stream.params, &first.data) {
        config.language = stream.params.language.clone();
        return Ok((config, None));
    }
    // Container layouts outside FLAC's channel sets would play on the
    // wrong speakers.
    let odd = matches!(
        stream.params.resolved_layout(),
        Some(ChannelLayout::Stereo21 | ChannelLayout::Surround40 | ChannelLayout::Surround41 | ChannelLayout::Surround60 | ChannelLayout::Surround70)
    );
    if odd {
        return Err(format!("{:?} audio has no FLAC channel order", stream.params.resolved_layout()));
    }
    let factory = bridge(stream);
    let output = bridge_output(&factory, stream, packets)?;
    let streaminfo = bridge::streaminfo(&output);
    let config = codec::flac_config(&streaminfo, stream.params.language.clone()).ok_or("no FLAC configuration")?;
    Ok((config, Some(Bridging { factory, output })))
}

/// What the bridge encodes: the decoder's output for the first packets.
fn bridge_output(factory: &DecoderFactory, stream: &StreamInfo, packets: &[(Packet, Option<AudioTrim>)]) -> Result<bridge::Output, String> {
    let mut decoder = factory()?;
    let mut out = Vec::new();
    for (packet, trim) in packets {
        let _ = decoder.packet(packet, *trim, &mut out);
        if let Some(pcm) = out.iter().find(|pcm| !pcm.samples.is_empty()) {
            if !(1..=8).contains(&pcm.channels) {
                return Err(format!("{} audio channels: more than FLAC carries", pcm.channels));
            }
            let codec_id = stream.params.codec_id.as_str().to_string();
            return Ok(bridge::Output { rate: pcm.rate, channels: pcm.channels, bits: bridge::bits_for(&codec_id), codec_id });
        }
    }
    Err("the audio decoder gave no sound from the first packets".into())
}

fn seek(demuxer: &mut dyn Demuxer, stream: u32, ticks: i64) -> Result<i64, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| demuxer.seek_to(stream, ticks))) {
        Ok(Ok(at)) => Ok(at),
        Ok(Err(e)) => Err(format!("seek: {e}")),
        Err(_) => Err("seek panicked".into()),
    }
}

/// The next packet and its audio trim.
fn next_trimmed(demuxer: &mut dyn Demuxer) -> Result<Option<(Packet, Option<AudioTrim>)>, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        demuxer.next_packet().map(|packet| {
            let trim = demuxer.packet_metadata().audio_trim;
            (packet, trim)
        })
    })) {
        Ok(Ok(read)) => Ok(Some(read)),
        Ok(Err(oxideav_core::Error::Eof)) => Ok(None),
        Ok(Err(e)) => Err(format!("demux: {e}")),
        Err(_) => Err("demuxer panicked".into()),
    }
}

fn next(demuxer: &mut dyn Demuxer) -> Result<Option<Packet>, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| demuxer.next_packet())) {
        Ok(Ok(packet)) => Ok(Some(packet)),
        Ok(Err(oxideav_core::Error::Eof)) => Ok(None),
        Ok(Err(e)) => Err(format!("demux: {e}")),
        Err(_) => Err("demuxer panicked".into()),
    }
}

/// The cue times of the video track. Cue track numbers are Matroska track
/// numbers, which the demuxer does not expose: with cues for one track only
/// that is the video's; otherwise the track numbered after the video
/// stream's position (mkvmerge and FFmpeg number tracks from 1 in order),
/// else the track with the most cues. The first production seek checks the
/// landing.
fn mkv_keyframes(mkv: &oxideav_mkv::demux::MkvDemuxer, video_index: u32) -> Vec<i64> {
    let mut by_track: std::collections::BTreeMap<u64, Vec<i64>> = Default::default();
    for cue in mkv.cue_points() {
        for position in &cue.track_positions {
            by_track.entry(position.track).or_default().push(cue.time as i64);
        }
    }
    let track = if by_track.len() == 1 {
        by_track.keys().next().copied()
    } else if by_track.contains_key(&(u64::from(video_index) + 1)) {
        Some(u64::from(video_index) + 1)
    } else {
        by_track.iter().max_by_key(|(_, times)| times.len()).map(|(&track, _)| track)
    };
    track.and_then(|track| by_track.remove(&track)).unwrap_or_default()
}

/// Keyframes of an MP4 by seeking the video every `TARGET_SECS` (every
/// 600th of the title when that is longer): the sample table is in memory,
/// so each seek is a lookup, not a read.
fn probe_keyframes(demuxer: &mut dyn Demuxer, video: u32, tb: TimeBase, duration: f64, stopped: &AtomicBool) -> Result<Vec<i64>, String> {
    let step = TARGET_SECS.max(duration / 600.0);
    let started = Instant::now();
    let mut keys = Vec::new();
    let mut at = 0.0;
    while at < duration {
        if stopped.load(Ordering::SeqCst) {
            return Err("stopped".into());
        }
        if started.elapsed() > PROBE_BUDGET {
            return Err("probing the sample table for keyframes took too long".into());
        }
        keys.push(seek(demuxer, video, tb.ticks_of(at))?);
        at += step;
    }
    Ok(keys)
}

struct First {
    keyframe: Packet,
    /// The earliest presentation time among the keyframe and the packets
    /// that follow it within the reorder window.
    earliest_pts: i64,
    /// The first audio packets, with their trims.
    audio: Vec<(Packet, Option<AudioTrim>)>,
}

fn first_packets(demuxer: &mut dyn Demuxer, video: &StreamInfo, audio: Option<&StreamInfo>, stopped: &AtomicBool) -> Result<First, String> {
    let mut keyframe: Option<Packet> = None;
    let mut earliest = i64::MAX;
    let mut following = 0;
    let mut first_audio: Vec<(Packet, Option<AudioTrim>)> = Vec::new();
    // A copied track needs its first packet only.
    let mut copied = false;
    let full = |audio: &[(Packet, Option<AudioTrim>)], copied: bool| {
        let secs = |packet: &Packet| packet.pts.or(packet.dts).map(|ticks| packet.time_base.seconds_of(ticks));
        let span = match (audio.first(), audio.last()) {
            (Some((first, _)), Some((last, _))) => secs(last).zip(secs(first)).map_or(0.0, |(l, f)| l - f),
            _ => 0.0,
        };
        (copied && !audio.is_empty()) || audio.len() >= FIRST_AUDIO_PACKETS || span >= FIRST_AUDIO_SECS
    };
    for _ in 0..FIRST_PACKETS {
        if stopped.load(Ordering::SeqCst) {
            return Err("stopped".into());
        }
        let Some((packet, trim)) = next_trimmed(demuxer)? else { break };
        if packet.stream_index == video.index {
            let pts = packet.pts.or(packet.dts);
            match &keyframe {
                None if packet.flags.keyframe => {
                    earliest = pts.unwrap_or(0);
                    keyframe = Some(packet);
                }
                None => {}
                Some(_) => {
                    following += 1;
                    if following <= 16 {
                        earliest = earliest.min(pts.unwrap_or(earliest));
                    }
                }
            }
        } else if let Some(stream) = audio.filter(|a| a.index == packet.stream_index && !full(&first_audio, copied) && !packet.data.is_empty()) {
            if first_audio.is_empty() {
                copied = codec::audio_config(&stream.params, &packet.data).is_some();
            }
            first_audio.push((packet, trim));
        }
        if keyframe.is_some() && following >= 16 && (audio.is_none() || full(&first_audio, copied)) {
            break;
        }
    }
    let keyframe = keyframe.ok_or("no video keyframe at the first cue")?;
    Ok(First { keyframe, earliest_pts: earliest, audio: first_audio })
}

/// End of the last video picture, reading from the last keyframe to the end.
fn last_picture_end(demuxer: &mut dyn Demuxer, video: u32, frame: i64, stopped: &AtomicBool) -> Result<i64, String> {
    let mut end: Option<i64> = None;
    while let Some(packet) = next(demuxer)? {
        if stopped.load(Ordering::SeqCst) {
            return Err("stopped".into());
        }
        if packet.stream_index != video {
            continue;
        }
        if let Some(pts) = packet.pts.or(packet.dts) {
            let this = pts + packet.duration.filter(|&d| d > 0).unwrap_or(frame);
            end = Some(end.map_or(this, |e| e.max(this)));
        }
    }
    end.ok_or_else(|| "no video after the last keyframe".into())
}

/// What the server hands out for the current generation.
struct Published {
    generation: u32,
    master: Arc<[u8]>,
    media: Arc<[u8]>,
    init: Arc<[u8]>,
    producer: Arc<Producer>,
}

#[derive(Default)]
struct Catalog {
    published: Mutex<Option<Published>>,
}

impl Resources for Catalog {
    fn get(&self, path: &str) -> Option<(Body, &'static str)> {
        let (generation, name) = path.split_once('/')?;
        let generation: u32 = generation.strip_prefix('g')?.parse().ok()?;
        let published = self.published.lock();
        let current = published.as_ref().filter(|p| p.generation == generation)?;
        match name {
            "master.m3u8" => Some((Body::Ready(Arc::clone(&current.master)), PLAYLIST)),
            "media.m3u8" => Some((Body::Ready(Arc::clone(&current.media)), PLAYLIST)),
            "init.mp4" => Some((Body::Ready(Arc::clone(&current.init)), MP4)),
            _ => {
                let index: usize = name.strip_prefix("seg")?.strip_suffix(".m4s")?.parse().ok()?;
                let producer = Arc::clone(&current.producer);
                drop(published);
                producer.request(index).map(|body| (body, MP4))
            }
        }
    }
}

/// A running native-route playback.
pub(crate) struct Session {
    server: Server,
    catalog: Arc<Catalog>,
    generation: u32,
    /// `None` only while an audio switch has taken its demuxer back.
    producer: Option<Arc<Producer>>,
    video: StreamInfo,
    video_config: VideoConfig,
    framing: codec::Framing,
    synthesize_dts: bool,
    frame_ticks: i64,
    audio: Option<(StreamInfo, AudioConfig)>,
    /// The audio is bridged: how.
    bridging: Option<Bridging>,
    bridge: BridgeProvider,
    origin: i64,
    plan: Plan,
    bandwidth: (u64, Option<u64>),
}

impl Session {
    pub(crate) fn start(prepared: Prepared) -> Result<Session, String> {
        let catalog = Arc::new(Catalog::default());
        let server = Server::start(Arc::clone(&catalog) as Arc<dyn Resources>).map_err(|e| format!("server: {e}"))?;
        let Prepared { demuxer, streams, plan, bandwidth, bridge } = prepared;
        let Streams { video, video_config, framing, synthesize_dts, frame_ticks, audio, bridging, origin } = streams;
        let mut session = Session {
            server,
            catalog,
            generation: 0,
            producer: None,
            video,
            video_config,
            framing,
            synthesize_dts,
            frame_ticks,
            audio,
            bridging,
            bridge,
            origin,
            plan,
            bandwidth,
        };
        session.publish(demuxer);
        Ok(session)
    }

    /// Starts a producer on `demuxer` for the current streams and publishes
    /// a new generation of playlists for it.
    fn publish(&mut self, demuxer: Box<dyn Demuxer>) {
        self.generation += 1;
        let producer = Arc::new(Producer::start(
            demuxer,
            Streams {
                video: self.video.clone(),
                video_config: self.video_config.clone(),
                framing: self.framing,
                synthesize_dts: self.synthesize_dts,
                frame_ticks: self.frame_ticks,
                audio: self.audio.clone(),
                bridging: self.bridging.clone(),
                origin: self.origin,
            },
            self.plan.clone(),
        ));
        let mut tracks = vec![Track { id: VIDEO_TRACK, timescale: VIDEO_TIMESCALE, kind: TrackKind::Video(&self.video_config) }];
        if let Some((_, config)) = &self.audio {
            tracks.push(Track { id: AUDIO_TRACK, timescale: config.sample_rate, kind: TrackKind::Audio(config) });
        }
        let init = fmp4::init_segment(&tracks);
        let mut codecs = vec![self.video_config.codec_string.as_str()];
        codecs.extend(self.audio.as_ref().map(|(_, c)| c.codec_string.as_str()));
        let master = hls::master_playlist(
            &Variant {
                bandwidth: self.bandwidth.0,
                average_bandwidth: self.bandwidth.1,
                codecs: &codecs,
                resolution: Some((self.video_config.width, self.video_config.height)),
                frame_rate: self.video_config.frame_rate,
                range: self.video_config.range,
                supplemental_codecs: self.video_config.dovi.as_ref().and_then(|d| d.supplemental_codecs.as_deref()),
            },
            "media.m3u8",
        );
        let positions = plan::positions_ms(&self.plan, self.video.time_base, self.origin);
        let media = hls::media_playlist(&positions, "init.mp4", &|i| format!("seg{i}.m4s"));
        *self.catalog.published.lock() = Some(Published {
            generation: self.generation,
            master: Arc::from(master.into_bytes()),
            media: Arc::from(media.into_bytes()),
            init: Arc::from(init),
            producer: Arc::clone(&producer),
        });
        self.producer = Some(producer);
    }

    /// The master playlist's URL for the current generation.
    pub(crate) fn url(&self) -> String {
        self.server.url(&format!("g{}/master.m3u8", self.generation))
    }

    /// The media playlist's URL for the current generation: what AVPlayer
    /// gets when it refuses the master (an HDR `VIDEO-RANGE` on a display
    /// without HDR fails with -11868).
    pub(crate) fn media_url(&self) -> String {
        self.server.url(&format!("g{}/media.m3u8", self.generation))
    }

    /// Source time, in seconds, of item time 0.
    pub(crate) fn origin_secs(&self) -> f64 {
        self.video.time_base.seconds_of(self.origin)
    }

    /// Source time, in seconds, where the last segment ends.
    pub(crate) fn end_secs(&self) -> f64 {
        self.video.time_base.seconds_of(self.plan.end)
    }

    pub(crate) fn served_any(&self) -> bool {
        self.producer.as_ref().is_some_and(|p| p.served_any())
    }

    pub(crate) fn requested_any(&self) -> bool {
        self.producer.as_ref().is_some_and(|p| p.requested_any())
    }

    pub(crate) fn served_last(&self) -> bool {
        self.producer.as_ref().is_some_and(|p| p.served_last())
    }

    /// Switches the audio to `audio` (`None`: none), reading its first
    /// packet near `at` (source seconds) for its configuration, and
    /// publishes a new generation. `Err`: the track cannot go natively;
    /// the producer is stopped then, and its demuxer is gone.
    pub(crate) fn switch_audio(&mut self, audio: Option<&StreamInfo>, at: f64, stopped: &AtomicBool) -> Result<(), String> {
        let producer = self.producer.take().ok_or("no segment thread")?;
        *self.catalog.published.lock() = None;
        let mut demuxer = producer.stop().ok_or("the segment thread failed")?;
        let mut active = vec![self.video.index];
        active.extend(audio.map(|a| a.index));
        demuxer.set_active_streams(&active);
        (self.audio, self.bridging) = match audio {
            None => (None, None),
            Some(stream) => {
                let start = self.plan.starts[self.plan.segment_at(self.video.time_base.ticks_of(at))];
                seek(&mut *demuxer, self.video.index, start)?;
                let first = first_packets(&mut *demuxer, &self.video, Some(stream), stopped)?;
                let (config, bridging) = audio_track(stream, &first.audio, &self.bridge)?;
                (Some((stream.clone(), config)), bridging)
            }
        };
        self.publish(demuxer);
        Ok(())
    }

    /// Stops serving and producing. `source` must be stopped first if the
    /// producer may sit in a read.
    pub(crate) fn stop(self) {
        self.server.stop();
        *self.catalog.published.lock() = None;
        if let Some(producer) = self.producer {
            producer.stop();
        }
    }
}

/// Scans an MP4/MOV source for a `dvcC` or `dvvC` box body of >= 24 bytes in the `moov` box.
fn scan_mp4_dovi<R: std::io::Read + Seek>(source: &mut R) -> Option<Vec<u8>> {
    let _ = source.seek(SeekFrom::Start(0));
    let mut header = [0u8; 8];
    while source.read_exact(&mut header).is_ok() {
        let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
        let fourcc = &header[4..8];
        let (box_size, header_len) = if size == 1 {
            let mut large = [0u8; 8];
            if source.read_exact(&mut large).is_err() {
                break;
            }
            (u64::from_be_bytes(large), 16)
        } else if size == 0 {
            break;
        } else {
            (size, 8)
        };
        if box_size < header_len {
            break;
        }
        if fourcc == b"moov" {
            let body_len = (box_size - header_len).min(32 * 1024 * 1024) as usize;
            let mut moov_data = vec![0u8; body_len];
            if source.read_exact(&mut moov_data).is_ok() {
                let rec = codec::find_dovi_box(&moov_data).map(|b| b.to_vec());
                let _ = source.seek(SeekFrom::Start(0));
                return rec;
            }
            break;
        } else {
            let skip = box_size - header_len;
            if source.seek(SeekFrom::Current(skip as i64)).is_err() {
                break;
            }
        }
    }
    let _ = source.seek(SeekFrom::Start(0));
    None
}
