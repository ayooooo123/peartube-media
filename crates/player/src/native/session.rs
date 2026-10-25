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
const FIRST_AUDIO_PACKETS: usize = 2_500;
const FIRST_AUDIO_SECS: f64 = 1.0;
/// Content budget scanned for a second IRAP when Cues are missing or untrusted.
const SPACING_SCAN_SECS: f64 = 30.0;
/// Streams larger than this over HTTP skip the startup seek-to-EOF scan so P2P
/// playback never stalls fetching uncached end-credits blocks before frame 0.
const TAIL_SCAN_HTTP_MAX_BYTES: u64 = 32 << 20;
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
    let mp4_family = matches!(container, "mp4" | "mov");
    let ts_family = container == "mpegts";
    if !pts_only && !mp4_family && !ts_family {
        return Err(format!("{container} is not supported on the native route"));
    }
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return Err("unknown duration".into());
    }
    let is_http = url.starts_with("http://") || url.starts_with("https://");
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
    } else if mp4_family {
        let dovi = scan_mp4_dovi(&mut source).or_else(|| codec::find_dovi_box(&video.params.extradata).map(|b| b.to_vec()));
        let demuxer = ctx.containers.open_demuxer(container, Box::new(source), &ctx.codecs).map_err(|e| format!("demuxer: {e}"))?;
        (demuxer, None, dovi)
    } else {
        let dovi = codec::find_dovi_box(&video.params.extradata).map(|b| b.to_vec());
        let demuxer = ctx.containers.open_demuxer(container, Box::new(source), &ctx.codecs).map_err(|e| format!("demuxer: {e}"))?;
        (demuxer, Some(Vec::new()), dovi)
    };
    let mut demuxer = wrap(demuxer);
    let mut active = vec![video.index];
    active.extend(audio.map(|a| a.index));
    demuxer.set_active_streams(&active);
    let tb = video.time_base;
    let keys = match cues {
        Some(keys) => keys,
        None => probe_keyframes(&mut *demuxer, video.index, tb, duration_secs, stopped).unwrap_or_default(),
    };

    let has_cues = plan::usable_cues(&keys, tb, duration_secs);
    let (first, first_key, uniform_stride) = if has_cues {
        let fk = *keys.iter().min().expect("usable cues are not empty");
        seek(&mut *demuxer, video.index, fk)?;
        let first = first_packets(&mut *demuxer, video, audio, false, stopped)?;
        (first, fk, None)
    } else {
        // Missing/partial Cues or MPEG-TS: stay on AVPlayer like AetherEngine by
        // measuring IRAP spacing from the head and laying a uniform grid.
        let _ = seek(&mut *demuxer, video.index, 0);
        let first = first_packets(&mut *demuxer, video, audio, true, stopped)?;
        let fk = first.keyframe.pts.or(first.keyframe.dts).unwrap_or(0);
        let stride = match first.spacing_secs {
            Some(s) if s.is_finite() => s.max(TARGET_SECS),
            Some(_) => duration_secs.max(TARGET_SECS),
            None => TARGET_SECS,
        };
        (first, fk, Some(stride))
    };

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
    let synthesize_dts = pts_only || first.keyframe.dts.is_none();
    let origin = if synthesize_dts {
        first.earliest_pts - depth * frame_ticks
    } else {
        first.keyframe.dts.or(first.keyframe.pts).unwrap_or(0)
    };

    let skip_tail_scan = is_http && length.map_or(true, |n| n > TAIL_SCAN_HTTP_MAX_BYTES);
    let duration_end = if ts_family || tb.seconds_of(first_key) > 5.0 {
        first_key.saturating_add(tb.ticks_of(duration_secs).max(frame_ticks))
    } else {
        tb.ticks_of(duration_secs).max(first_key.saturating_add(frame_ticks))
    };
    let plan = if let Some(stride) = uniform_stride {
        plan::uniform(first_key, duration_end, stride, tb).ok_or("no uniform segment plan")?
    } else {
        let last_key = *keys.iter().max().expect("usable cues are not empty");
        let end = if skip_tail_scan {
            duration_end.max(last_key.saturating_add(frame_ticks))
        } else {
            seek(&mut *demuxer, video.index, last_key)?;
            last_picture_end(&mut *demuxer, video.index, frame_ticks, stopped)?
        };
        plan::plan(&keys, tb, end).ok_or("no keyframe before the end")?
    };
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
            synthesize_dts,
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
    /// Measured distance (seconds) between the first two video keyframes,
    /// when `measure_spacing` is requested for a uniform fallback plan.
    spacing_secs: Option<f64>,
    /// The first audio packets, with their trims.
    audio: Vec<(Packet, Option<AudioTrim>)>,
}

fn first_packets(
    demuxer: &mut dyn Demuxer,
    video: &StreamInfo,
    audio: Option<&StreamInfo>,
    measure_spacing: bool,
    stopped: &AtomicBool,
) -> Result<First, String> {
    let mut keyframe: Option<Packet> = None;
    let mut first_key_pts: Option<i64> = None;
    let mut spacing_secs: Option<f64> = None;
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
    let mut hit_eof = false;
    for _ in 0..FIRST_PACKETS {
        if stopped.load(Ordering::SeqCst) {
            return Err("stopped".into());
        }
        let Some((packet, trim)) = next_trimmed(demuxer)? else {
            hit_eof = true;
            break;
        };
        if packet.stream_index == video.index {
            let pts = packet.pts.or(packet.dts);
            match &keyframe {
                None if packet.flags.keyframe => {
                    let at = pts.unwrap_or(0);
                    earliest = at;
                    first_key_pts = Some(at);
                    keyframe = Some(packet);
                }
                None => {}
                Some(_) => {
                    following += 1;
                    if following <= 16 {
                        earliest = earliest.min(pts.unwrap_or(earliest));
                    }
                    if measure_spacing && spacing_secs.is_none() {
                        if let (Some(fk), Some(at)) = (first_key_pts, pts) {
                            let elapsed = video.time_base.seconds_of(at.saturating_sub(fk));
                            if packet.flags.keyframe && elapsed > 0.0 {
                                spacing_secs = Some(elapsed);
                            } else if elapsed >= SPACING_SCAN_SECS {
                                spacing_secs = Some(SPACING_SCAN_SECS);
                            }
                        }
                    }
                }
            }
        } else if let Some(stream) = audio.filter(|a| a.index == packet.stream_index && !full(&first_audio, copied) && !packet.data.is_empty()) {
            if first_audio.is_empty() {
                copied = codec::audio_config(&stream.params, &packet.data).is_some();
            }
            first_audio.push((packet, trim));
        }
        let spacing_done = !measure_spacing || spacing_secs.is_some();
        if keyframe.is_some() && following >= 16 && spacing_done && (audio.is_none() || full(&first_audio, copied)) {
            break;
        }
    }
    let keyframe = keyframe.ok_or("no video keyframe at the first cue")?;
    if measure_spacing && spacing_secs.is_none() {
        spacing_secs = Some(if hit_eof { f64::INFINITY } else { SPACING_SCAN_SECS });
    }
    Ok(First { keyframe, earliest_pts: earliest, spacing_secs, audio: first_audio })
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

    /// Source time, in seconds, where the last segment ends (refined to the
    /// exact final video picture once the last segment has been produced).
    pub(crate) fn end_secs(&self) -> f64 {
        let ticks = self
            .producer
            .as_ref()
            .and_then(|p| p.actual_end_ticks())
            .unwrap_or(self.plan.end);
        self.video.time_base.seconds_of(ticks)
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
                let first = first_packets(&mut *demuxer, &self.video, Some(stream), false, stopped)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use dolby_vision::rpu::dovi_rpu::DoviRpu;
    use oxideav_core::{Demuxer, Muxer};
    use oxideav_mkv::demux::BlockAdditionMapping;

    const P7_FEL_RPU_HEX: &str = "7c011908090840613650ae20002008020080200802007f801ffc00ffc001fffa0000030100000300d0000008000006800000400000340000030200000301a000001000000d0000030080000068000004000003034000002000000a800000800000e18618800000800000800000800000800000800000b0c30c800000800000800000800000800000800000986186800000800000800000800000800000540000040000040000070c30c4000004000004000004000004000004000005861864000004000004000004000004000004000004c30c34000004000004000004000004012000010010010000030100480000400400400000401200001001001000001c95980000d7a8959be7f3ac7095991328000004000003002000000300200000030070d8890c06182978c238145000000698f96bfffc00000300000300000300018081f73805463008015faa4900c0282183180081872b94210002020000030008050000030002261140120c1f4000641f2e1276ffb6cfda80";

    #[test]
    fn profile7_session_rewrites_init_and_converts_segment_rpus_to_profile81() {
        let p7_rpu: Vec<u8> = (0..P7_FEL_RPU_HEX.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&P7_FEL_RPU_HEX[i..i + 2], 16).unwrap())
            .collect();
        let el_nal: Vec<u8> = vec![63 << 1, 0x01, 0xDE, 0xAD, 0xBE, 0xEF];

        let mut ctx = RuntimeContext::new();
        oxideav_mkv::register(&mut ctx);
        oxideav_mp4::register(&mut ctx);

        // Build a 10 s Profile 7 MKV in a temp file from the HEVC HDR10 fixture's extradata:
        let tmp = std::env::temp_dir().join(format!("peartube_p7_test_{}.mkv", std::process::id()));
        {
            let hvcc = vec![
                0x01, 0x02, 0x20, 0x00, 0x00, 0x00, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0xf0,
                0x00, 0xfc, 0xfd, 0xfa, 0xfa, 0x00, 0x00, 0x0f, 0x03,
                0x20, 0x00, 0x01, 0x00, 0x18, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x02, 0x20, 0x00,
                0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x78, 0x95, 0x94,
                0x09,
                0x21, 0x00, 0x01, 0x00, 0x2e, 0x42, 0x01, 0x01, 0x02, 0x20, 0x00, 0x00, 0x03, 0x00,
                0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x78, 0xa0, 0x03, 0xc0, 0x80, 0x11,
                0x07, 0xca, 0xd9, 0x65, 0x65, 0x4a, 0x4c, 0x2f, 0x01, 0x6a, 0x12, 0x20, 0x12, 0x08,
                0x00, 0x00, 0x1f, 0x48, 0x00, 0x02, 0xee, 0x00, 0x40,
                0x22, 0x00, 0x01, 0x00, 0x06, 0x44, 0x01, 0xc0, 0x73, 0xc1, 0x89,
            ];
            let mut params = oxideav_core::CodecParameters::video(oxideav_core::CodecId::new("h265"));
            params.width = Some(1920);
            params.height = Some(1080);
            params.extradata = hvcc;
            let stream = StreamInfo {
                index: 0,
                time_base: TimeBase::new(1, 1000),
                duration: Some(10_000),
                start_time: Some(0),
                params,
            };
            let file = std::fs::File::create(&tmp).unwrap();
            let mut muxer = oxideav_mkv::mux::MkvMuxer::new_matroska(Box::new(file), &[stream]).unwrap();
            let dovi_p7 = codec::build_dovi_record(7, 6, true, true, true, 6);
            muxer
                .set_block_addition_mappings(
                    0,
                    vec![BlockAdditionMapping {
                        value: Some(4),
                        name: Some("Dolby Vision configuration".into()),
                        addid_type: 0x6476_6343, // 'dvcC'
                        extra_data: Some(dovi_p7.to_vec()),
                    }],
                )
                .unwrap();
            muxer.write_header().unwrap();
            // Write 10 seconds of frames (25 fps = 40 ms per frame), keyframes every 2 s (50 frames):
            for i in 0..250i64 {
                let is_key = i % 50 == 0;
                let vcl = vec![if is_key { 19 << 1 } else { 1 << 1 }, 0x01, 0xAA, (i & 0xFF) as u8];
                let mut payload = Vec::new();
                for n in [&vcl, &p7_rpu, &el_nal] {
                    payload.extend_from_slice(&(n.len() as u32).to_be_bytes());
                    payload.extend_from_slice(n);
                }
                let mut pkt = Packet::new(0, TimeBase::new(1, 1000), payload);
                pkt.pts = Some(i * 40);
                pkt.dts = Some(i * 40);
                pkt.duration = Some(40);
                pkt.flags.keyframe = is_key;
                muxer.write_packet(&pkt).unwrap();
            }
            muxer.write_trailer().unwrap();
        }

        let mkv = oxideav_mkv::demux::open_typed(Box::new(std::fs::File::open(&tmp).unwrap()), &ctx.codecs).unwrap();
        let video_stream = mkv.streams()[0].clone();
        drop(mkv);

        let bridge: BridgeProvider = Arc::new(|_| Arc::new(|| Err("no audio".into())));
        let stopped = AtomicBool::new(false);
        let prepared = prepare(
            &ctx,
            tmp.to_str().unwrap(),
            "matroska",
            &video_stream,
            None,
            10.0,
            &|_| {},
            &|d| d,
            bridge,
            &stopped,
        )
        .expect("prepare Profile 7 MKV");

        let dovi = prepared.video_config().dovi.as_ref().expect("dovi config present");
        assert_eq!(dovi.profile, 8);
        assert_eq!(dovi.compat_id, 1);
        assert!(dovi.convert_p7_to_81);
        assert_eq!(dovi.supplemental_codecs.as_deref(), Some("dvh1.08.06/db1p"));

        let session = Session::start(prepared).expect("start session");
        let master_body = match session.catalog.get("g1/master.m3u8").unwrap().0 {
            Body::Ready(b) => String::from_utf8(b.to_vec()).unwrap(),
            _ => panic!("master not ready"),
        };
        assert!(master_body.contains("SUPPLEMENTAL-CODECS=\"dvh1.08.06/db1p\""), "master: {master_body}");

        let init_bytes = match session.catalog.get("g1/init.mp4").unwrap().0 {
            Body::Ready(b) => b.to_vec(),
            _ => panic!("init not ready"),
        };
        assert!(init_bytes.windows(4).any(|w| w == b"dvvC"), "init.mp4 must carry dvvC for converted P8.1");

        let seg0_bytes = match session.catalog.get("g1/seg0.m4s").unwrap().0 {
            Body::Ready(b) => b.to_vec(),
            Body::Pending(rx) => rx.recv().unwrap().unwrap().to_vec(),
        };
        session.stop();
        let _ = std::fs::remove_file(&tmp);

        // Demux init.mp4 + seg0.m4s through oxideav_mp4 and inspect every video packet's NALs:
        let mut combined = init_bytes;
        combined.extend_from_slice(&seg0_bytes);
        let mut mp4 = oxideav_mp4::demux::open_typed(Box::new(std::io::Cursor::new(combined)), &ctx.codecs).unwrap();
        let mut packet_count = 0;
        while let Ok(pkt) = mp4.next_packet() {
            packet_count += 1;
            let mut pos = 0;
            let mut saw_rpu = false;
            while pos + 4 <= pkt.data.len() {
                let len = u32::from_be_bytes(pkt.data[pos..pos + 4].try_into().unwrap()) as usize;
                pos += 4;
                let nal = &pkt.data[pos..pos + len];
                pos += len;
                let nal_type = (nal[0] >> 1) & 0x3F;
                assert_ne!(nal_type, 63, "UNSPEC63 EL NAL must be stripped from segment samples");
                if nal_type == 62 {
                    saw_rpu = true;
                    let rpu = DoviRpu::parse_unspec62_nalu(nal).expect("segment RPU must be valid");
                    assert_eq!(rpu.dovi_profile, 8, "segment RPU must be converted from P7 to P8.1");
                    assert!(rpu.header.disable_residual_flag);
                    assert!(!rpu.header.el_spatial_resampling_filter_flag);
                    assert!(rpu.el_type.is_none());
                }
            }
            assert!(saw_rpu, "every video sample in seg0 must carry its converted P8.1 RPU");
        }
        assert_eq!(packet_count, 150);
    }
}
