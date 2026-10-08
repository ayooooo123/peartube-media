use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use audio_trim::{Pcm, Trimmer};
use parking_lot::{Condvar, Mutex, MutexGuard};

use oxideav_core::{
    CodecParameters, Decoder, Demuxer, Frame, MediaType, Packet, PacketMetadata, ProbeData, RuntimeContext,
    SampleFormat, StreamInfo, TimeBase, PROBE_SCORE_EXTENSION,
};

mod captions;

use crate::backend::{AudioSink, Backend, Clock, SinkError, VideoSink};
use crate::clock::MasterClock;
use crate::headless::find_headless;
use crate::source::{open_source, ReadAheadSource, SourceMonitor};
use crate::subs::{run_subtitle_loop, SubtitlePipeline};

mod transport;
use transport::{Due, Live, Pipe, Preroll, Transport};

#[cfg(test)]
mod subtitle_tests;

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported container: {0}")]
    UnsupportedContainer(String),
    #[error("failed to open demuxer: {0}")]
    Demuxer(String),
    #[error("other error: {0}")]
    Other(String),
}

#[derive(Clone, Debug)]
pub struct PlayerOptions {
    pub audio: Option<u32>,
    pub video: Option<u32>,
    pub subtitle: Option<u32>,
    pub realtime: bool,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        Self {
            audio: None,
            video: None,
            subtitle: None,
            realtime: true,
        }
    }
}

pub enum Event {
    Changed,
    Ended,
    Error(String),
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub position: Duration,
    pub duration: Option<Duration>,
    pub playing: bool,
    pub buffering: bool,
    pub ended: bool,
    pub error: Option<String>,
    pub tracks: Vec<Track>,
    pub audio: Option<u32>,
    pub video: Option<u32>,
    pub subtitle: Option<u32>,
    pub video_size: Option<(u32, u32)>,
    pub video_decoder: Option<String>,
    pub dropped_frames: u64,
    /// Trim work lost to input limits. Playback continues with untrimmed
    /// samples; this is not an audio decoder error.
    pub audio_trim_fallbacks: audio_trim::Fallbacks,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub stream: u32,
    pub kind: TrackKind,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Video,
    Subtitle,
}

/// Queue bounds: packets are held back by both media duration and bytes.
/// (duration, video bytes, audio bytes, subtitle bytes)
const QUEUE_MAX_SECS: f64 = 2.0;
const VIDEO_MAX_BYTES: usize = 32 * 1024 * 1024;
const AUDIO_MAX_BYTES: usize = 8 * 1024 * 1024;
const SUB_MAX_BYTES: usize = 1024 * 1024;

/// Side data travels with its packet, including across equal-PTS laces.
pub(crate) struct QueuedPacket {
    pub(crate) packet: Packet,
    pub(crate) metadata: PacketMetadata,
}

/// One decoder lane. A lane owns its packet queue and wakes the demux loop
/// whenever it drains, so the demuxer never stalls behind a slow sink.
pub(crate) struct Lane {
    pub(crate) queue: Mutex<Vec<QueuedPacket>>,
    pub(crate) cv: Condvar,
    /// Seek generation the queued packets belong to; changed only with
    /// `queue` locked, when the demuxer empties the lane for a seek.
    pub(crate) seek_gen: AtomicU64,
    /// A pipeline thread drains the lane (see `Consumer`); changed only with
    /// `queue` locked. Without one, packets for the lane are dropped:
    /// queued, they would fill it and park the demuxer for good.
    consumed: AtomicBool,
}

/// What a pipeline got from its lane.
enum Pop {
    Packet(QueuedPacket),
    /// The demuxer's end marker.
    Eof,
    /// The lane holds packets of a seek the pipeline has not reset for, or
    /// the caller's `wake` condition turned true while it waited.
    Wake,
}

impl Lane {
    fn new() -> Arc<Lane> {
        Arc::new(Lane {
            queue: Mutex::new(Vec::new()),
            cv: Condvar::new(),
            seek_gen: AtomicU64::new(0),
            consumed: AtomicBool::new(false),
        })
    }

    fn push(&self, packet: QueuedPacket) {
        let mut q = self.queue.lock();
        if !self.consumed.load(Ordering::SeqCst) {
            return;
        }
        q.push(packet);
        drop(q);
        self.cv.notify_one();
    }

    fn push_eof(&self) {
        let mut q = self.queue.lock();
        if !self.consumed.load(Ordering::SeqCst) {
            return;
        }
        q.push_eof_marker();
        drop(q);
        self.cv.notify_all();
    }

    /// Empties the lane for the demuxer's seek `generation`: what it queues
    /// next comes from the seek target.
    fn clear_for_seek(&self, generation: u64) {
        let mut q = self.queue.lock();
        q.clear();
        self.seek_gen.store(generation, Ordering::SeqCst);
        drop(q);
        self.cv.notify_all();
    }

    /// Media span (seconds between the first and last pts) and bytes queued.
    fn queued(&self, time_base: TimeBase) -> (f64, usize) {
        let q = self.queue.lock();
        let mut first: Option<f64> = None;
        let mut last: Option<f64> = None;
        let mut bytes = 0;
        for p in q.iter() {
            // An untimed packet (B-pictures only carry a PTS in some raw
            // streams) says nothing about the queued span.
            if let Some(ticks) = p.packet.pts.or(p.packet.dts) {
                let secs = time_base.seconds_of(ticks);
                first.get_or_insert(secs);
                last = Some(secs);
            }
            let side_bytes = p.metadata.webvtt.as_ref().map_or(0, |m| {
                std::mem::size_of_val(m.as_ref())
                    + m.identifier.capacity() + m.settings.capacity()
            });
            bytes += std::mem::size_of::<QueuedPacket>() + p.packet.data.capacity() + side_bytes;
        }
        let span = match (first, last) {
            (Some(a), Some(b)) => (b - a).max(0.0),
            _ => 0.0,
        };
        (span, bytes)
    }

    /// The next packet or the end marker for seek generation `seen_seek`,
    /// waiting while there is none: packets queued before the demuxer
    /// applied that seek are never handed out, and `Wake` asks a pipeline
    /// that has not reset for the demuxer's newest seek to do so first. An
    /// empty lane short of its end starves the pipeline: `report(true)` when
    /// that starts and `report(false)` once a packet or the end arrives
    /// (`starved` carries the reported state across calls; both reports run
    /// with the lane unlocked). Also returns `Wake` as soon as `wake` holds
    /// while waiting.
    fn pop(
        &self,
        seen_seek: u64,
        demux_cv: &Condvar,
        wake: impl Fn() -> bool,
        starved: &mut bool,
        report: impl Fn(bool),
    ) -> Pop {
        let mut q = self.queue.lock();
        let popped = loop {
            let generation = self.seek_gen.load(Ordering::SeqCst);
            if generation > seen_seek {
                break Pop::Wake;
            }
            let current = generation == seen_seek;
            match q.first() {
                Some(p) if current && p.packet.stream_index == u32::MAX => {
                    q.remove(0);
                    break Pop::Eof;
                }
                Some(_) if current => break Pop::Packet(q.remove(0)),
                _ if wake() => break Pop::Wake,
                _ if !*starved => {
                    *starved = true;
                    MutexGuard::unlocked(&mut q, || report(true));
                }
                _ => {
                    demux_cv.notify_one();
                    self.cv.wait_for(&mut q, Duration::from_millis(100));
                }
            }
        };
        drop(q);
        if *starved && !matches!(popped, Pop::Wake) {
            *starved = false;
            report(false);
        }
        demux_cv.notify_one();
        popped
    }
}

/// Marks a lane as drained by one pipeline thread, from spawn until that
/// thread ends, however it ends: its stream has no decoder, the decoder gave
/// up, a selection switch retired it, it panicked, or it played to the end.
/// The lane then empties and queues nothing more, so the demuxer never waits
/// on a lane that nobody drains.
struct Consumer {
    lane: Arc<Lane>,
    demux_cv: Arc<Condvar>,
}

impl Consumer {
    fn new(lane: &Arc<Lane>, demux_cv: &Arc<Condvar>) -> Consumer {
        let q = lane.queue.lock();
        lane.consumed.store(true, Ordering::SeqCst);
        drop(q);
        Consumer {
            lane: Arc::clone(lane),
            demux_cv: Arc::clone(demux_cv),
        }
    }
}

impl Drop for Consumer {
    fn drop(&mut self) {
        let mut q = self.lane.queue.lock();
        self.lane.consumed.store(false, Ordering::SeqCst);
        q.clear();
        drop(q);
        // The demuxer may be waiting for this lane to make room or drain.
        self.demux_cv.notify_all();
    }
}

/// A running pipeline thread and the flag that retires it. A selection
/// switch retires the old thread, and waits for it, before the new one
/// starts: a lane has one consumer at a time, and nothing the old thread
/// still had in hand reaches a sink after the switch.
struct PipelineThread {
    handle: JoinHandle<()>,
    retired: Arc<AtomicBool>,
}

impl PipelineThread {
    /// Runs `body` on a new thread called `name`, handing it its retire flag.
    fn spawn(name: &str, body: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> PipelineThread {
        let retired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&retired);
        let handle = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || body(flag))
            .unwrap_or_else(|e| panic!("failed to spawn {name} thread: {e}"));
        PipelineThread { handle, retired }
    }

    /// Stops the thread without stopping the player, and waits for it.
    fn retire(self, shared: &SharedState, lane: &Lane) {
        self.retired.store(true, Ordering::SeqCst);
        // Wake it wherever it waits: on its lane or on the clock.
        drop(lane.queue.lock());
        lane.cv.notify_all();
        shared.wake_clock_waiters();
        self.join();
    }

    fn join(self) {
        let _ = self.handle.join();
    }
}

trait PushEof {
    fn push_eof_marker(&mut self);
}
impl PushEof for Vec<QueuedPacket> {
    fn push_eof_marker(&mut self) {
        // EOF marker: a packet with stream_index == u32::MAX.
        self.push(QueuedPacket {
            packet: Packet {
                stream_index: u32::MAX,
                time_base: TimeBase::new(1, 1000),
                pts: None,
                dts: None,
                duration: None,
                flags: Default::default(),
                data: Vec::new(),
            },
            metadata: PacketMetadata::default(),
        });
    }
}

/// End of `p` in seconds (pts, else dts, plus its duration).
fn packet_end_secs(p: &Packet) -> Option<f64> {
    if !p.time_base.is_valid() {
        return None;
    }
    let start = p.pts.or(p.dts)?;
    let end = start.saturating_add(p.duration.unwrap_or(0).max(0));
    Some(p.time_base.seconds_of(end))
}

struct SharedState {
    state: Mutex<State>,
    stopped: Arc<AtomicBool>,
    /// The playback failed (`set_error`); set with `state` locked. A stream
    /// error that leaves the rest playing does not count.
    failed: AtomicBool,
    /// Paired with `state` for `Player::wait`.
    condvar: Condvar,
    on_event: Arc<dyn Fn(Event) + Send + Sync>,
    last_changed: Mutex<Instant>,
    /// The demuxer's source, for suspend/resume.
    source: Mutex<Option<SourceMonitor>>,
    /// Monotonic counter bumped by every `seek`. The demux loop applies the
    /// newest request; decoder threads compare their local copy against it to
    /// detect a seek they have not yet honoured.
    seek_gen: AtomicU64,
    /// Target of the latest `seek`; the demux loop applies it once per
    /// generation.
    seek_target: Mutex<Option<Duration>>,
    /// Seek the demux loop has applied (`seek_to` returned): generation and
    /// target. Decoder threads read it to drop pre-target output.
    active_seek: Mutex<Option<Seek>>,
    /// Selection written by `select_audio` / `select_subtitle`; the demux
    /// loop applies it (flush + respawn the pipeline) and mirrors `state`.
    wanted_audio: Mutex<AudioChoice>,
    wanted_video: Mutex<Option<u32>>,
    wanted_subtitle: Mutex<Option<u32>>,
    /// Bumped on every selection change; the demux loop compares to detect it.
    select_gen: AtomicU64,
    /// The playback has video or audio pipelines (playing or played out),
    /// as its selection stands: subtitles beside them end with the screen
    /// clear. Set by the demux thread whenever it starts or retires them.
    beside_media: AtomicBool,
    backend: Arc<dyn Backend>,
    /// The playback's clock (see `MasterClock`); `transport` decides when
    /// it runs.
    master: Arc<MasterClock>,
    /// The playback's audio output while no audio pipeline holds it: made
    /// before the video output (an Apple video layer joins the audio
    /// output's synchronizer) and kept across audio track switches, so the
    /// playback has one audio clock throughout.
    audio_sink: Mutex<Option<Box<dyn AudioSink>>>,
    /// Play/pause intent and the buffering hold; decides when the clock
    /// runs (see `transport`).
    transport: Mutex<Transport>,
    /// Paired with `transport`: notified whenever the clock's run state,
    /// position or the hold changes.
    transport_cv: Condvar,
    /// Lock-free mirror of `Transport::running`.
    running: AtomicBool,
    /// The playback's lanes, woken when the clock starts or stops so idle
    /// pipelines pause or resume their sinks.
    lanes: Mutex<Vec<Arc<Lane>>>,
    ctx: Arc<RuntimeContext>,
}

#[derive(Clone, Copy, Debug)]
struct Seek {
    generation: u64,
    /// Target in seconds.
    target: f64,
}

/// The audio track asked for: the playback's default, or one the caller
/// chose (`None`: no audio). A switch of another kind keeps the default.
#[derive(Clone, Copy, Debug, PartialEq)]
enum AudioChoice {
    Default,
    Chosen(Option<u32>),
}

impl SharedState {
    /// The clock every sink of this playback follows: the audio output's
    /// clock while audio plays, else the free-running clock; either stands
    /// still while buffering or paused.
    fn sink_clock(&self) -> Arc<dyn Clock> {
        self.master.clone()
    }

    /// Moves playback to `to`: the demux loop applies the newest request,
    /// and each pipeline starts over when it sees the generation change.
    fn request_seek(&self, to: Duration) {
        *self.seek_target.lock() = Some(to);
        self.seek_gen.fetch_add(1, Ordering::SeqCst);
        self.seek_clock(to);
        self.state.lock().position = to;
        notify_changed(self);
    }
}

pub struct Player {
    shared: Arc<SharedState>,
    /// Runs the playback: opens the source, owns the demuxer, and spawns and
    /// joins the pipeline threads.
    pipeline: Option<JoinHandle<()>>,
}

impl Player {
    pub fn open(
        url: &str,
        backend: Arc<dyn Backend>,
        ctx: Arc<RuntimeContext>,
        options: PlayerOptions,
        on_event: impl Fn(Event) + Send + Sync + 'static,
    ) -> Player {
        let on_event_arc: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(on_event);

        // Playing is the intent; the clock holds at the start (buffering)
        // until the first audio/video is ready.
        let initial_state = State {
            playing: true,
            buffering: true,
            ..State::default()
        };

        let stopped = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(SharedState {
            state: Mutex::new(initial_state),
            stopped,
            failed: AtomicBool::new(false),
            condvar: Condvar::new(),
            on_event: on_event_arc,
            last_changed: Mutex::new(Instant::now() - Duration::from_secs(1)),
            source: Mutex::new(None),
            seek_gen: AtomicU64::new(0),
            seek_target: Mutex::new(None),
            active_seek: Mutex::new(None),
            wanted_audio: Mutex::new(options.audio.map_or(AudioChoice::Default, |stream| AudioChoice::Chosen(Some(stream)))),
            wanted_video: Mutex::new(options.video),
            wanted_subtitle: Mutex::new(options.subtitle),
            select_gen: AtomicU64::new(1),
            beside_media: AtomicBool::new(false),
            backend,
            master: Arc::new(MasterClock::new()),
            audio_sink: Mutex::new(None),
            transport: Mutex::new(Transport::new()),
            transport_cv: Condvar::new(),
            running: AtomicBool::new(false),
            lanes: Mutex::new(Vec::new()),
            ctx,
        });

        let url_owned = url.to_string();
        let shared_clone = Arc::clone(&shared);

        let init_thread = std::thread::Builder::new()
            .name("peartube-player-pipeline".into())
            .spawn(move || {
                run_player_pipeline(url_owned, options, shared_clone);
            })
            .expect("failed to spawn player pipeline thread");

        Player {
            shared,
            pipeline: Some(init_thread),
        }
    }

    /// The user's intent; the clock also waits for data while buffering.
    pub fn play(&self) {
        self.shared.set_paused(false);
        self.shared.state.lock().playing = true;
        notify_changed(&self.shared);
    }

    /// The user's intent: stays paused when buffering ends.
    pub fn pause(&self) {
        self.shared.set_paused(true);
        self.shared.state.lock().playing = false;
        notify_changed(&self.shared);
    }

    pub fn seek(&self, to: Duration) {
        self.shared.request_seek(to);
    }

    pub fn select_audio(&self, stream: Option<u32>) {
        *self.shared.wanted_audio.lock() = AudioChoice::Chosen(stream);
        self.shared.select_gen.fetch_add(1, Ordering::SeqCst);
        self.shared.condvar.notify_all();
        notify_changed(&self.shared);
    }

    pub fn select_subtitle(&self, stream: Option<u32>) {
        *self.shared.wanted_subtitle.lock() = stream;
        self.shared.select_gen.fetch_add(1, Ordering::SeqCst);
        self.shared.condvar.notify_all();
        notify_changed(&self.shared);
    }

    pub fn suspend(&self) {
        self.pause();
        if let Some(src) = &*self.shared.source.lock() {
            src.suspend();
        }
        self.shared.backend.suspend();
        notify_changed(&self.shared);
    }

    pub fn resume(&self) {
        if let Some(src) = &*self.shared.source.lock() {
            src.resume();
        }
        self.shared.backend.resume();
        self.play();
    }

    pub fn state(&self) -> State {
        let mut st = self.shared.state.lock();
        st.position = self.shared.master.now().unwrap_or(st.position);
        st.clone()
    }

    /// Blocks until the playback ended or failed (a stream error that leaves
    /// the rest playing does not end the wait), or the player is dropped.
    pub fn wait(&self) -> State {
        let mut st = self.shared.state.lock();
        while !st.ended
            && !self.shared.failed.load(Ordering::SeqCst)
            && !self.shared.stopped.load(Ordering::SeqCst)
        {
            self.shared.condvar.wait(&mut st);
        }
        st.position = self.shared.master.now().unwrap_or(st.position);
        st.clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        self.shared.stop();
        // A demuxer waiting on a stalled source gets an error instead of
        // the bytes, and the playback winds down without them.
        let opened = match &*self.shared.source.lock() {
            Some(source) => {
                source.stop();
                true
            }
            None => false,
        };
        self.shared.condvar.notify_all();
        if let Some(pipeline) = self.pipeline.take() {
            // Until the source exists the pipeline thread may sit in the
            // request that opens it, which a stalled peer can hold for as
            // long as it likes: leave it then (it ends as soon as that
            // returns, see `run_player_pipeline`).
            if opened {
                let _ = pipeline.join();
            }
        }
    }
}

/// `Event::Changed`, at most every 100 ms.
fn notify_changed(shared: &SharedState) {
    let mut last = shared.last_changed.lock();
    let now = Instant::now();
    if now.duration_since(*last) >= Duration::from_millis(100) {
        *last = now;
        (shared.on_event)(Event::Changed);
    }
}

/// `Event::Changed` that must not be dropped (buffering started or ended).
fn notify_changed_now(shared: &SharedState) {
    *shared.last_changed.lock() = Instant::now();
    (shared.on_event)(Event::Changed);
}

fn set_error(shared: &SharedState, err: String) {
    {
        let mut st = shared.state.lock();
        if st.error.is_none() {
            st.error = Some(err.clone());
            st.playing = false;
        }
        shared.failed.store(true, Ordering::SeqCst);
    }
    shared.finish();
    (shared.on_event)(Event::Error(err));
    shared.condvar.notify_all();
}

fn set_ended(shared: &SharedState) {
    {
        let mut st = shared.state.lock();
        st.ended = true;
        st.playing = false;
    }
    shared.finish();
    (shared.on_event)(Event::Ended);
    shared.condvar.notify_all();
}

fn run_player_pipeline(
    url: String,
    options: PlayerOptions,
    shared: Arc<SharedState>,
) {
    let ctx = &*shared.ctx;
    // 1. One read-ahead source feeds the probe and then the demuxer. The
    //    engine keeps its monitor: starvation reports drive the buffering
    //    hold, suspend/resume pause the download.
    let mut source = match open_source(&url) {
        Ok(s) => s,
        Err(e) => {
            if !shared.stopped.load(Ordering::SeqCst) {
                set_error(&shared, format!("failed to open source: {e}"));
            }
            return;
        }
    };
    let monitor = source.monitor();
    let weak: Weak<SharedState> = Arc::downgrade(&shared);
    monitor.on_starved(move |starved| {
        if let Some(shared) = weak.upgrade() {
            shared.source_starved(starved);
        }
    });
    *shared.source.lock() = Some(monitor);
    if shared.stopped.load(Ordering::SeqCst) {
        // Dropped while the source opened: `Player::drop` neither waited for
        // this thread nor could stop a source that did not exist yet.
        return;
    }

    // 2. Probe (rule from engine-api.md, same as refcheck), then rewind.
    let container = match probe_container(&url, &mut source, &shared) {
        Ok(c) => c,
        Err(e) => {
            if !shared.stopped.load(Ordering::SeqCst) {
                set_error(&shared, e);
            }
            return;
        }
    };

    // 3. Demuxer. Container codec tags resolve through the registry — the
    //    mpeg4video fork claims Matroska's MPEG-4 Part 2 CodecIDs
    //    (V_MPEG4/ISO/ASP, //SP, //AP) directly.
    let mut demuxer = match ctx
        .containers
        .open_demuxer(&container, Box::new(source), &ctx.codecs)
    {
        Ok(d) => d,
        Err(e) => {
            if !shared.stopped.load(Ordering::SeqCst) {
                set_error(&shared, format!("failed to open demuxer: {e}"));
            }
            return;
        }
    };

    // 4. Streams (cap 64; drop video tracks above the size limits).
    let streams_all = demuxer.streams();
    let count = streams_all.len().min(64);
    let mut streams: Vec<StreamInfo> = streams_all[..count].to_vec();

    let mut tracks = Vec::new();
    let mut first_audio = None;
    let mut first_video = None;
    let mut first_subtitle = None;
    for s in &streams {
        let kind = match s.params.media_type {
            MediaType::Audio => {
                if first_audio.is_none() {
                    first_audio = Some(s.index);
                }
                TrackKind::Audio
            }
            MediaType::Video => {
                let w = s.params.width.unwrap_or(0);
                let h = s.params.height.unwrap_or(0);
                if w > 16384 || h > 16384 || u64::from(w) * u64::from(h) > 8192 * 8192 {
                    continue;
                }
                if first_video.is_none() {
                    first_video = Some(s.index);
                }
                TrackKind::Video
            }
            MediaType::Subtitle => {
                if first_subtitle.is_none() {
                    first_subtitle = Some(s.index);
                }
                TrackKind::Subtitle
            }
            _ => continue,
        };
        tracks.push(Track {
            stream: s.index,
            kind,
            codec: s.params.codec_id.as_str().to_string(),
            language: s.params.language.clone(),
            title: None,
            default: false,
        });
    }

    let duration = demuxer
        .duration_micros()
        .map(|us| Duration::from_micros(us.max(0) as u64))
        .or_else(|| {
            streams
                .iter()
                .filter_map(|s| {
                    s.duration.and_then(|d| {
                        if s.time_base.is_valid() {
                            Some(Duration::from_secs_f64(
                                s.time_base.seconds_of(d).max(0.0),
                            ))
                        } else {
                            None
                        }
                    })
                })
                .max()
        });
    // Captions in the video: selectable streams, tracks once data shows up.
    captions::add_streams(&mut streams, shared.wanted_video.lock().or(first_video));

    // 5. Selection. The pipeline thread owns `current_*`; `select_*` writes
    // `wanted_*` and bumps `select_gen` so the demux loop applies switches.
    // Read the generation first: a switch made while this reads `wanted_*`
    // then still counts as new.
    let select_gen = shared.select_gen.load(Ordering::SeqCst);
    let options_video = *shared.wanted_video.lock();
    let current_video = options_video.or(first_video);
    let current_audio = match *shared.wanted_audio.lock() {
        AudioChoice::Default => first_audio,
        AudioChoice::Chosen(stream) => stream,
    };
    let current_subtitle = *shared.wanted_subtitle.lock();

    {
        let mut st = shared.state.lock();
        st.tracks = tracks;
        st.audio = current_audio;
        st.video = current_video;
        st.subtitle = current_subtitle;
        st.duration = duration;
        st.video_size = current_video.and_then(|idx| {
            streams.iter().find(|s| s.index == idx).and_then(|s| {
                let (w, h) = (s.params.width?, s.params.height?);
                (w > 0 && h > 0).then_some((w, h))
            })
        });
        st.video_decoder = current_video.map(|idx| {
            streams
                .iter()
                .find(|s| s.index == idx)
                .map(|s| s.params.codec_id.as_str().to_string())
                .unwrap_or_default()
        });
    }
    notify_changed(&shared);

    // Tell the headless registry which streams the captures describe.
    if let Some(headless) = find_headless(Arc::as_ptr(&shared.backend) as *const () as usize) {
        let info = |idx: Option<u32>, kind: MediaType| {
            idx.and_then(|i| {
                streams
                    .iter()
                    .find(|s| s.index == i && s.params.media_type == kind)
                    .map(|s| (i, s.params.codec_id.as_str().to_string()))
            })
        };
        headless.set_active_streams(
            info(current_video, MediaType::Video),
            info(current_audio, MediaType::Audio),
            info(current_subtitle, MediaType::Subtitle),
            options.realtime,
        );
        headless.set_clock(shared.sink_clock());
    }

    // 6. Lanes + sinks.
    let video_lane = Lane::new();
    let audio_lane = Lane::new();
    let sub_lane = Lane::new();
    *shared.lanes.lock() = vec![
        Arc::clone(&video_lane),
        Arc::clone(&audio_lane),
        Arc::clone(&sub_lane),
    ];
    let demux_cv = Arc::new(Condvar::new());
    let video_tb = current_video
        .and_then(|i| streams.iter().find(|s| s.index == i))
        .map(|s| s.time_base)
        .unwrap_or_else(|| TimeBase::new(1, 1000));
    let audio_tb = current_audio
        .and_then(|i| streams.iter().find(|s| s.index == i))
        .map(|s| s.time_base)
        .unwrap_or_else(|| TimeBase::new(1, 1000));
    let sub_tb = current_subtitle
        .and_then(|i| streams.iter().find(|s| s.index == i))
        .map(|s| s.time_base)
        .unwrap_or_else(|| TimeBase::new(1, 1000));

    // 7. Pipeline threads. The demux loop replaces the audio and subtitle
    // pipelines on a selection switch; the video pipeline stays fixed (the
    // public API cannot switch it mid-playback).
    let realtime = options.realtime;
    let mut run = Run {
        shared: &shared,
        demuxer: &mut *demuxer,
        streams: &streams,
        options: &options,
        video_lane: &video_lane,
        audio_lane: &audio_lane,
        sub_lane: &sub_lane,
        demux_cv: &demux_cv,
        video_tb,
        audio_tb,
        sub_tb,
        select_gen,
        current_video,
        current_audio,
        current_subtitle,
        video_thread: None,
        audio_thread: None,
        sub_thread: None,
    };
    let video_stream = find_stream(&streams, current_video);
    let audio_stream = find_stream(&streams, current_audio);
    // The audio output comes first: the video output may join its clock
    // (an Apple video layer joins the audio's synchronizer).
    if audio_stream.is_some() {
        *shared.audio_sink.lock() = Some(shared.backend.audio());
    }
    run.video_thread =
        video_stream.map(|stream| spawn_video(&shared, stream, &video_lane, &demux_cv, realtime));
    run.audio_thread =
        audio_stream.map(|stream| spawn_audio(&shared, stream, &audio_lane, &demux_cv, realtime));
    shared.beside_media.store(run.video_thread.is_some() || run.audio_thread.is_some(), Ordering::SeqCst);
    run.sub_thread = find_stream(&streams, current_subtitle)
        .map(|stream| spawn_subtitles(&shared, stream, &sub_lane, &demux_cv, realtime));

    // 8. From here the buffering hold follows the pipelines' data.
    shared.pipelines_started();
    run_demux_loop(&mut run);

    // Subtitles never hold the end of a playback with video or audio: a
    // state still up when those have played (its end can be minutes or
    // hours out) comes down with them. Alone, subtitles play to their last
    // end; without realtime nothing waits on the clock and the pipeline
    // ends once its lane has drained.
    let paced = run.video_thread.is_some() || run.audio_thread.is_some();
    for thread in [run.video_thread.take(), run.audio_thread.take()].into_iter().flatten() {
        thread.join();
    }
    if let Some(subtitles) = run.sub_thread.take() {
        if paced && realtime {
            subtitles.retire(&shared, &sub_lane);
        } else {
            subtitles.join();
        }
    }
    // Everything has played: the idle audio output stops with the clock.
    if let Some(sink) = shared.audio_sink.lock().as_mut() {
        sink.pause();
    }

    // A stream that lost its decoder leaves an error in `State` while the
    // rest plays on; the playback still ends. Only a failure (`set_error`)
    // or a drop ends it otherwise.
    if !shared.stopped.load(Ordering::SeqCst) && !shared.finished() {
        set_ended(&shared);
    }
}

fn find_stream(streams: &[StreamInfo], index: Option<u32>) -> Option<StreamInfo> {
    let index = index?;
    streams.iter().find(|s| s.index == index).cloned()
}

/// The stream's decoder, built from the registry. Seeks build a fresh one
/// rather than calling `Decoder::reset`, which may drop state the codec
/// parameters set up: oxideav-h264's reset forgets the avcC SPS/PPS, after
/// which every slice fails. H.264 whose extradata is Annex B (NUT, AVI, or
/// any stream muxed with global headers outside MP4/Matroska) reaches the
/// decoder in-band, as a packet of its own ahead of the stream: that decoder
/// reads only avcC extradata, while FFmpeg's accepts both forms.
fn make_decoder(ctx: &RuntimeContext, params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
    let built = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if params.codec_id.as_str() != "h264" || !is_annex_b(&params.extradata) {
            return ctx.codecs.first_decoder(params);
        }
        let mut bare = params.clone();
        let parameter_sets = std::mem::take(&mut bare.extradata);
        let mut decoder = ctx.codecs.first_decoder(&bare)?;
        decoder.send_packet(&Packet {
            stream_index: 0,
            time_base: TimeBase::new(1, 1000),
            pts: None,
            dts: None,
            duration: None,
            flags: Default::default(),
            data: parameter_sets,
        })?;
        Ok(decoder)
    }));
    built.unwrap_or_else(|_| Err(oxideav_core::Error::Other("decoder panicked while opening".into())))
}

/// Annex B framing: the bytes start with a start code. (An avcC record
/// starts with its version, 1.)
fn is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

fn spawn_video(
    shared: &Arc<SharedState>,
    stream: StreamInfo,
    lane: &Arc<Lane>,
    demux_cv: &Arc<Condvar>,
    realtime: bool,
) -> PipelineThread {
    let consumer = Consumer::new(lane, demux_cv);
    let live = Live::new(shared, Pipe::Video);
    let sink = shared.backend.video(shared.sink_clock());
    let (shared, lane, demux_cv) = (Arc::clone(shared), Arc::clone(lane), Arc::clone(demux_cv));
    PipelineThread::spawn("peartube-video", move |retired| {
        let _guards = (live, consumer);
        #[cfg(target_os = "macos")]
        if realtime {
            if let Err(error) = crate::clock::timing::initialize_worker() {
                eprintln!("macOS video worker scheduling setup failed: {error}");
            }
        }
        run_video_thread(stream, sink, lane, demux_cv, shared, realtime, retired);
    })
}

fn spawn_audio(
    shared: &Arc<SharedState>,
    stream: StreamInfo,
    lane: &Arc<Lane>,
    demux_cv: &Arc<Condvar>,
    realtime: bool,
) -> PipelineThread {
    let consumer = Consumer::new(lane, demux_cv);
    let live = Live::new(shared, Pipe::Audio);
    let held = shared.audio_sink.lock().take();
    let sink = held.unwrap_or_else(|| shared.backend.audio());
    let (shared, lane, demux_cv) = (Arc::clone(shared), Arc::clone(lane), Arc::clone(demux_cv));
    PipelineThread::spawn("peartube-audio", move |retired| {
        let _guards = (live, consumer);
        #[cfg(target_os = "macos")]
        if realtime {
            if let Err(error) = crate::clock::timing::initialize_worker() {
                eprintln!("macOS audio worker scheduling setup failed: {error}");
            }
        }
        let mut output = AudioOutput {
            shared: Arc::clone(&shared),
            sink: Some(sink),
        };
        if let Some(sink) = output.sink.as_deref_mut() {
            // However the pipeline fails, the track stops being the audio
            // track, as when its decoder fails: a panic outside the decoder
            // calls included.
            let failed = Arc::clone(&shared);
            let run = std::panic::catch_unwind(AssertUnwindSafe(|| {
                run_audio_thread(stream, sink, lane, demux_cv, shared, realtime, retired)
            }));
            if run.is_err() {
                audio_failed(&failed, "audio pipeline failed".into());
            }
        }
    })
}

/// The playback's audio output while an audio pipeline holds it. However
/// the pipeline ends, the output's clock stops leading (the free clock
/// carries on from where it was) and the output goes back to
/// `SharedState::audio_sink` for the next audio pipeline.
struct AudioOutput {
    shared: Arc<SharedState>,
    sink: Option<Box<dyn AudioSink>>,
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.shared.master.release_audio();
        *self.shared.audio_sink.lock() = self.sink.take();
    }
}

fn spawn_subtitles(
    shared: &Arc<SharedState>,
    stream: StreamInfo,
    lane: &Arc<Lane>,
    demux_cv: &Arc<Condvar>,
    realtime: bool,
) -> PipelineThread {
    let consumer = Consumer::new(lane, demux_cv);
    let sink = shared.backend.subtitles();
    let clock = shared.sink_clock();
    let (shared, lane, demux_cv) = (Arc::clone(shared), Arc::clone(lane), Arc::clone(demux_cv));
    PipelineThread::spawn("peartube-subtitles", move |retired| {
        let _consumer = consumer;
        let mut params = stream.params.clone();
        let (video_size, lanes) = (shared.state.lock().video_size, shared.lanes.lock().clone());
        // DVD, CVD and OGT place regions in video pixels. For a stream that
        // declares no canvas, FFmpeg's is the video's (fftools/ffmpeg_demux.c
        // sub2video); the DVD decoder's own `size:` line still takes
        // precedence, as dvdsubdec's does. The video size is known only from
        // the container at open: nothing publishes a decoded one, so without
        // it the decoders keep their 720x576, and nothing waits for a size.
        let video_pixels = matches!(params.codec_id.as_str(), "dvd_subtitle" | "dvdsub" | "vobsub" | "cvd_subtitle" | "ogt");
        let declared = params.width.unwrap_or(0) > 0 && params.height.unwrap_or(0) > 0;
        if let Some((width, height)) = video_size.filter(|_| video_pixels && !declared) {
            params.width = Some(params.width.unwrap_or(0).max(width));
            params.height = Some(params.height.unwrap_or(0).max(height));
        }
        let (w, h) = video_size.unwrap_or((320, 240));
        let decoder = match shared.ctx.codecs.first_decoder(&params) {
            Ok(d) => d,
            Err(_) => return,
        };
        let ctx = Arc::clone(&shared.ctx);
        let (seeks, members) = (Arc::clone(&shared), Arc::clone(&shared));
        let pipeline = SubtitlePipeline {
            decoder,
            new_decoder: Box::new(move || ctx.codecs.first_decoder(&params)),
            clock,
            time_base: stream.time_base,
            video_width: w,
            video_height: h,
            realtime,
            lane,
            demux_cv,
            seek_generation: Box::new(move || seeks.seek_gen.load(Ordering::SeqCst)),
            // A video or audio pipeline drains its lane: the demuxer's
            // read-ahead is bounded by theirs.
            paced: Box::new(move || lanes.iter().take(2).any(|lane| lane.consumed.load(Ordering::SeqCst))),
            beside_media: Box::new(move || members.beside_media.load(Ordering::SeqCst)),
            stopped: shared.stopped.clone(),
            retired,
        };
        run_subtitle_loop(pipeline, sink);
    })
}

/// Probes the first 256 KiB of `source` and rewinds it to the start (the
/// read-ahead ring still holds those bytes, so the demuxer re-reads them
/// without another request).
fn probe_container(
    url: &str,
    source: &mut ReadAheadSource,
    shared: &SharedState,
) -> Result<String, String> {
    use std::io::{Read, Seek, SeekFrom};
    let ctx = &*shared.ctx;
    // One read returns what has arrived so far, which over a P2P stream can
    // be a few bytes: fill the buffer (or reach the end) before probing.
    let mut probe_buf = vec![0u8; 256 * 1024];
    let mut n = 0;
    while n < probe_buf.len() {
        if shared.stopped.load(Ordering::SeqCst) {
            return Err("stopped".into());
        }
        match source.read(&mut probe_buf[n..]) {
            Ok(0) => break,
            Ok(read) => n += read,
            Err(e) => return Err(format!("failed to read for probe: {e}")),
        }
    }
    source
        .seek(SeekFrom::Start(0))
        .map_err(|e| format!("failed to rewind after probing: {e}"))?;
    probe_buf.truncate(n);

    let ext = url.split(['?', '#']).next().unwrap_or(url);
    let ext = std::path::Path::new(ext)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);

    let probe_data = ProbeData {
        buf: &probe_buf,
        ext: ext.as_deref(),
    };
    let candidates = ctx.containers.probe_candidates(&probe_data);
    let by_extension = ext.as_deref().and_then(|e| ctx.containers.container_for_extension(e));
    match (candidates.first(), by_extension) {
        (Some(c), _) if c.score >= PROBE_SCORE_EXTENSION => Ok(c.name.to_string()),
        (_, Some(name)) => Ok(name.to_string()),
        _ => Err("no container claims this input".into()),
    }
}

/// What the demux loop works with: the demuxer, the lanes, the selection and
/// the pipeline threads it replaces on a selection switch.
struct Run<'a> {
    shared: &'a Arc<SharedState>,
    demuxer: &'a mut dyn Demuxer,
    streams: &'a [StreamInfo],
    options: &'a PlayerOptions,
    video_lane: &'a Arc<Lane>,
    audio_lane: &'a Arc<Lane>,
    sub_lane: &'a Arc<Lane>,
    demux_cv: &'a Arc<Condvar>,
    video_tb: TimeBase,
    audio_tb: TimeBase,
    sub_tb: TimeBase,
    /// The `SharedState::select_gen` the `current_*` selection reflects.
    select_gen: u64,
    current_video: Option<u32>,
    current_audio: Option<u32>,
    current_subtitle: Option<u32>,
    video_thread: Option<PipelineThread>,
    audio_thread: Option<PipelineThread>,
    sub_thread: Option<PipelineThread>,
}

fn run_demux_loop(run: &mut Run<'_>) {
    let shared = run.shared;
    let mut active: Vec<u32> = [run.current_video, run.current_audio, run.current_subtitle]
        .into_iter()
        .flatten()
        .collect();
    let _ = run.demuxer.set_active_streams(&active);
    let mut eof = false;
    let mut full = false;
    let mut captions = captions::Captions::new(run);

    while !shared.stopped.load(Ordering::SeqCst) {
        // Selection switch: replace the changed pipelines.
        let gen_now = shared.select_gen.load(Ordering::SeqCst);
        if gen_now != run.select_gen {
            run.select_gen = gen_now;
            apply_selection_switch(run, &mut active);
            if eof {
                eof = false;
                shared.demux_eof(false);
            }
        }

        // Seek: apply each new request; a request already applied for this
        // generation is skipped so a slow seek_to doesn't re-run.
        let latest = shared.seek_gen.load(Ordering::SeqCst);
        let pending = *shared.seek_target.lock();
        if let Some(target) = pending {
            let applied = (*shared.active_seek.lock()).map(|sk| sk.generation) == Some(latest);
            if !applied {
                do_seek(run, target, latest, &mut eof);
            }
        }

        // Bounded queues: wait while a lane is full. Nothing more can be
        // queued until the clock moves, so a buffering hold lets go as soon
        // as the pipelines have output.
        let now_full = !eof && lanes_full(run);
        if now_full != full {
            full = now_full;
            shared.demux_full(full);
        }
        if full {
            let mut none: Option<()> = None;
            let guard = Mutex::new(&mut none);
            let mut g = guard.lock();
            run.demux_cv.wait_for(&mut g, Duration::from_millis(50));
            continue;
        }

        if eof {
            // Wait until the pipelines drained their lanes, end markers
            // included, so a trailing packet is never cut off, then leave. A
            // lane without a consumer is always empty (see `Consumer`).
            // Leaving ends `run_player_pipeline`, which joins the pipelines
            // and sets Ended. A seek or a selection switch clears lanes and
            // reopens `eof` at the top of this loop. In realtime, subtitles
            // beside video or audio do not hold the end: their last state
            // comes down when those have played (`run_player_pipeline`).
            let paced = run.options.realtime && (run.video_thread.is_some() || run.audio_thread.is_some());
            let subtitles = run.sub_thread.as_ref().filter(|_| !paced);
            let drained = [run.video_thread.as_ref(), run.audio_thread.as_ref(), subtitles]
                .into_iter().flatten().all(|thread| thread.handle.is_finished());
            if drained {
                return;
            }
            // Consumers notify `demux_cv` on every packet they take.
            let mut none: Option<()> = None;
            let guard = Mutex::new(&mut none);
            let mut g = guard.lock();
            run.demux_cv.wait_for(&mut g, Duration::from_millis(10));
            continue;
        }

        let packet_res = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let packet = run.demuxer.next_packet()?;
            let metadata = run.demuxer.packet_metadata();
            Ok::<_, oxideav_core::Error>(QueuedPacket { packet, metadata })
        }));
        match packet_res {
            Ok(Ok(packet)) => {
                let stream_id = packet.packet.stream_index;
                let pipe = if Some(stream_id) == run.current_video {
                    Some(Pipe::Video)
                } else if Some(stream_id) == run.current_audio {
                    Some(Pipe::Audio)
                } else {
                    None
                };
                let end = pipe.and_then(|_| packet_end_secs(&packet.packet));
                if pipe == Some(Pipe::Video) {
                    captions.video_packet(run, &packet.packet);
                }
                match pipe {
                    Some(Pipe::Video) => run.video_lane.push(packet),
                    Some(Pipe::Audio) => run.audio_lane.push(packet),
                    None if Some(stream_id) == run.current_subtitle => run.sub_lane.push(packet),
                    // Inactive streams' packets are dropped.
                    None => {}
                }
                if let (Some(pipe), Some(end)) = (pipe, end) {
                    shared.demuxed(pipe, end);
                }
            }
            Ok(Err(oxideav_core::Error::Eof)) => {
                captions.finish(run);
                eof = true;
                run.video_lane.push_eof();
                run.audio_lane.push_eof();
                run.sub_lane.push_eof();
                shared.demux_eof(true);
            }
            Ok(Err(e)) => {
                // A source stopped by `Player::drop` fails the read: that
                // ends the loop, it is not a playback error.
                if !shared.stopped.load(Ordering::SeqCst) {
                    set_error(shared, format!("demux error: {e}"));
                }
                return;
            }
            Err(_) => {
                set_error(shared, "demuxer panicked".into());
                return;
            }
        }
    }
}

fn lanes_full(run: &Run<'_>) -> bool {
    let full = |lane: &Lane, tb: TimeBase, max_bytes: usize| {
        let (secs, bytes) = lane.queued(tb);
        secs >= QUEUE_MAX_SECS || bytes >= max_bytes
    };
    full(run.video_lane, run.video_tb, VIDEO_MAX_BYTES)
        || full(run.audio_lane, run.audio_tb, AUDIO_MAX_BYTES)
        || full(run.sub_lane, run.sub_tb, SUB_MAX_BYTES)
}

/// Where the demuxer seeks for a seek to `target`, in seconds, as fftools'
/// `-ss` input seek does (2da55bf ffmpeg_demux.c): 3/23 s early when a
/// stream has a decoding delay (FFmpeg's `codecpar->video_delay`) and the
/// format does not seek by presentation time (`AVFMT_SEEK_TO_PTS`: FFmpeg's
/// mov, mxf, nut, dhav and dvdvideo demuxers). The demuxer starts from the
/// random access point at or before that time, so the pictures FFmpeg
/// shows from the target decode from the same references; frames before
/// the target are still dropped. Starting from the random access point at
/// the target instead loses pictures that reference earlier ones, such as
/// those after a non-IDR I frame without a recovery point.
fn seek_position(format: &str, streams: &[StreamInfo], target: Duration) -> f64 {
    const SEEKS_TO_PTS: [&str; 7] = ["mov", "mp4", "ismv", "mxf", "nut", "dhav", "dvdvideo"];
    let mut micros = i64::try_from(target.as_micros()).unwrap_or(i64::MAX);
    if !SEEKS_TO_PTS.contains(&format) && streams.iter().any(|s| video_delay(&s.params) > 0) {
        micros -= 3 * 1_000_000 / 23;
    }
    micros as f64 / 1e6
}

/// FFmpeg's `codecpar->video_delay` for a stream, when its parameters tell:
/// the demuxer's `video_delay` option (demuxers that port
/// find_stream_info set it), and for H.264 the reorder depth its SPS
/// declares. 0 otherwise.
fn video_delay(params: &CodecParameters) -> u32 {
    match params.codec_id.as_str() {
        "h264" => oxideav_h264::h264_decoder::video_delay(params),
        _ => params.options.get("video_delay").and_then(|v| v.parse().ok()),
    }
    .unwrap_or(0)
}

fn do_seek(run: &mut Run<'_>, target: Duration, generation: u64, eof: &mut bool) {
    let shared = run.shared;
    // Convert to the seek stream's time base. The demuxer seeks the video
    // stream when present, else audio, else stream 0.
    let seek_stream = run.current_video.or(run.current_audio).unwrap_or(0);
    let tb = run
        .streams
        .iter()
        .find(|s| s.index == seek_stream)
        .map(|s| s.time_base)
        .unwrap_or_else(|| TimeBase::new(1, 1000));
    let ticks = tb.ticks_of(seek_position(run.demuxer.format_name(), run.streams, target)).max(0);

    // `seek_target` keeps the newest request: the generation decides what
    // has been applied, so a seek arriving meanwhile is not lost.
    *shared.active_seek.lock() = Some(Seek {
        generation,
        target: target.as_secs_f64(),
    });
    run.video_lane.clear_for_seek(generation);
    run.audio_lane.clear_for_seek(generation);
    run.sub_lane.clear_for_seek(generation);
    *eof = false;
    shared.demux_seeked(generation);

    let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
        run.demuxer.seek_to(seek_stream, ticks)
    }));
    match res {
        Ok(Ok(_)) | Ok(Err(_)) => {}
        Err(_) => {
            set_error(shared, "demuxer panicked during seek".into());
        }
    }
}

/// `select_audio` / `select_subtitle` took effect: retire the replaced
/// pipeline, start the new one, and refresh the headless registry entry. A
/// new audio track resumes where playback is: the demuxer re-reads from the
/// clock's position (a refresh seek) instead of starting the track wherever
/// it has read ahead to. A subtitle switch never seeks: the new track shows
/// from its next cue the demuxer reads.
fn apply_selection_switch(run: &mut Run<'_>, active: &mut Vec<u32>) {
    let shared = run.shared;

    // The default audio track stays whatever else changes.
    let wanted_audio = match *shared.wanted_audio.lock() {
        AudioChoice::Default => run.current_audio,
        AudioChoice::Chosen(stream) => stream,
    };
    let wanted_subtitle = *shared.wanted_subtitle.lock();
    let audio_changed = wanted_audio != run.current_audio;
    let sub_changed = wanted_subtitle != run.current_subtitle;

    // The old thread goes first: a lane has one consumer, and nothing the
    // old pipeline still holds may reach a sink once the new stream is the
    // selected one.
    if audio_changed {
        if let Some(old) = run.audio_thread.take() {
            old.retire(shared, run.audio_lane);
        }
    }
    if sub_changed {
        if let Some(old) = run.sub_thread.take() {
            old.retire(shared, run.sub_lane);
        }
    }

    // The video stream stays as opened: the public API cannot switch it.
    run.current_audio = wanted_audio;
    run.current_subtitle = wanted_subtitle;
    let time_base = |index: Option<u32>| {
        find_stream(run.streams, index)
            .map(|s| s.time_base)
            .unwrap_or_else(|| TimeBase::new(1, 1000))
    };
    run.audio_tb = time_base(wanted_audio);
    run.sub_tb = time_base(wanted_subtitle);

    {
        let mut st = shared.state.lock();
        st.audio = wanted_audio;
        st.subtitle = wanted_subtitle;
    }
    notify_changed(shared);

    if let Some(headless) = find_headless(Arc::as_ptr(&shared.backend) as *const () as usize) {
        let info = |idx: Option<u32>, kind: MediaType| {
            idx.and_then(|i| {
                run.streams
                    .iter()
                    .find(|s| s.index == i && s.params.media_type == kind)
                    .map(|s| (i, s.params.codec_id.as_str().to_string()))
            })
        };
        headless.set_active_streams(
            info(run.current_video, MediaType::Video),
            info(run.current_audio, MediaType::Audio),
            info(run.current_subtitle, MediaType::Subtitle),
            run.options.realtime,
        );
    }

    let realtime = run.options.realtime;
    if audio_changed {
        let stream = find_stream(run.streams, run.current_audio)
            .filter(|s| s.params.media_type == MediaType::Audio);
        if let Some(stream) = stream {
            shared.request_seek(shared.master.now().unwrap_or_default());
            run.audio_thread = Some(spawn_audio(shared, stream, run.audio_lane, run.demux_cv, realtime));
        }
    }
    // Subtitles beside video or audio, running or new, end with them.
    shared.beside_media.store(run.video_thread.is_some() || run.audio_thread.is_some(), Ordering::SeqCst);
    if sub_changed {
        let stream = find_stream(run.streams, run.current_subtitle)
            .filter(|s| s.params.media_type == MediaType::Subtitle);
        if let Some(stream) = stream {
            run.sub_thread = Some(spawn_subtitles(shared, stream, run.sub_lane, run.demux_cv, realtime));
        }
    }

    *active = [run.current_video, run.current_audio, run.current_subtitle]
        .into_iter()
        .flatten()
        .collect();
    let _ = run.demuxer.set_active_streams(active);
}

fn audio_failed(shared: &SharedState, message: String) {
    let mut st = shared.state.lock();
    st.error.get_or_insert(message);
    st.audio = None;
    drop(st);
    notify_changed(shared);
}

fn record_trim_fallbacks(shared: &SharedState, trimmer: &mut Trimmer<Chunk>) {
    let fallbacks = trimmer.take_fallbacks();
    if !fallbacks.is_empty() {
        shared.state.lock().audio_trim_fallbacks.add(fallbacks);
    }
}

/// Packet lanes → decoder → sink, for one audio stream. The sink follows the
/// clock's run state (`play`/`pause`); while the clock stands still, PCM goes
/// out only up to `PREROLL` past it. From its first samples of each seek the
/// output's clock leads the playback. At EOF the pipeline ends once its last
/// samples are heard; it ends early when the player stops or a selection
/// switch sets `retired`. The encoder delay and end padding the container
/// declares (`PacketMetadata::audio_trim`) never reach the sink: they come
/// off the decoder's output through `audio_trim`, as `refcheck` removes
/// them, so the reference tests check what plays. A decoder's own start
/// delay comes off there too, once, unless the container's skip replaces it.
fn run_audio_thread(
    stream: StreamInfo,
    sink: &mut dyn AudioSink,
    lane: Arc<Lane>,
    demux_cv: Arc<Condvar>,
    shared: Arc<SharedState>,
    realtime: bool,
    retired: Arc<AtomicBool>,
) {
    let mut decoder_params = stream.params.clone();
    let decoder_delay = audio_trim::take_decoder_delay(&mut decoder_params);
    let mut decoder = match make_decoder(&shared.ctx, &decoder_params) {
        Ok(d) => d,
        Err(e) => {
            // No decoder: the track is skipped (still listed in `tracks`)
            // and the rest plays on.
            audio_failed(&shared, format!("no audio decoder found: {e}"));
            return;
        }
    };

    let mut out = AudioOut {
        rate: stream.params.sample_rate.unwrap_or(48000),
        channels: stream.params.channels.unwrap_or(2),
        open: false,
        written: Written::default(),
        seen_seek_target: 0,
        primed: None,
    };
    // The output may come from the previous track: none of that plays on.
    sink.flush();
    out.open = sink.open(out.rate, out.channels).is_ok();
    let mut starved = false;
    let mut consecutive_errors = 0;
    let mut seen_seek = shared.seek_gen.load(Ordering::SeqCst);
    let mut trimmer: Trimmer<Chunk> = Trimmer::with_decoder_delay(decoder_delay);
    let mut kept: Vec<Chunk> = Vec::new();
    // Where the decoder's output so far ends: where a frame without a pts
    // of its own starts.
    let mut decoded_end: Option<f64> = None;
    let quit = || shared.stopped.load(Ordering::SeqCst) || retired.load(Ordering::SeqCst);

    while !quit() {
        if !realtime {
            // Nothing waits on the clock: park while paused instead.
            shared.wait_while_paused(&retired);
            if quit() {
                break;
            }
        }
        sync_audio_sink(sink, &shared, &mut out.written.running);

        // Seek generation: start the decoder, the trims and the sink over,
        // drop pre-target output after the demuxer's seek lands.
        let gen_now = shared.seek_gen.load(Ordering::SeqCst);
        if gen_now != seen_seek {
            seen_seek = gen_now;
            sink.flush();
            out.written.running = None;
            out.written.end = None;
            trimmer.reset();
            decoded_end = None;
            decoder = match make_decoder(&shared.ctx, &decoder_params) {
                Ok(d) => d,
                Err(e) => {
                    audio_failed(&shared, format!("audio decoder failed to restart: {e}"));
                    return;
                }
            };
            consecutive_errors = 0;
        }

        // Pull a packet; the EOF marker ends this pipeline.
        let applied = out.written.running;
        let woken = || quit() || Some(shared.running()) != applied;
        let report = |dry| shared.pipe_starved(Pipe::Audio, dry);
        let QueuedPacket { packet, metadata } = match lane.pop(seen_seek, &demux_cv, woken, &mut starved, report) {
            Pop::Packet(p) => p,
            Pop::Wake => continue,
            Pop::Eof => {
                // Drain the decoder's tail into the sink. What the trimmer
                // still holds after that is the stream's end padding.
                let _ = decoder.flush();
                let mut packet_pts = None;
                while !quit() {
                    let recv = std::panic::catch_unwind(AssertUnwindSafe(|| decoder.receive_frame()));
                    let af = match recv {
                        Ok(Ok(Frame::Audio(af))) => af,
                        Ok(Ok(_)) => continue,
                        _ => break,
                    };
                    // An empty frame ends the tail: a decoder may return
                    // them forever.
                    if af.samples == 0 {
                        break;
                    }
                    let chunk = decoded_chunk(decoder.as_ref(), &stream, &af, &mut packet_pts, &mut decoded_end);
                    trimmer.frame(chunk, af.pts, &mut kept);
                    record_trim_fallbacks(&shared, &mut trimmer);
                    if !present_kept(sink, &shared, &mut out, &mut kept, seen_seek, realtime, &retired) {
                        break;
                    }
                }
                // A tail the trimmer held as padding but finds too short for
                // it plays (FFmpeg ignores such padding).
                trimmer.finish(&mut kept);
                record_trim_fallbacks(&shared, &mut trimmer);
                if !quit() {
                    let _ = present_kept(sink, &shared, &mut out, &mut kept, seen_seek, realtime, &retired);
                }
                // The output plays what it holds before the pipeline ends:
                // the audio leads the clock up to its last sample, and the
                // playback ends after that sample is heard.
                if let (true, Some(end), Some(leads)) = (realtime, out.written.end, out.written.leads) {
                    if leads == seen_seek {
                        shared.wait_heard(end, seen_seek, &retired, |running| {
                            if out.written.running != Some(running) {
                                if running { sink.play(); } else { sink.pause(); }
                                out.written.running = Some(running);
                            }
                        });
                    }
                }
                if shared.seek_gen.load(Ordering::SeqCst) != seen_seek { continue; }
                break;
            }
        };

        // Decode one packet under catch_unwind. A decoder that panics or
        // errors 3 times in a row on this stream disables it: the track goes
        // silent but playback continues.
        let send_res = std::panic::catch_unwind(AssertUnwindSafe(|| decoder.send_packet(&packet)));
        match send_res {
            Ok(Ok(())) => {
                consecutive_errors = 0;
                trimmer.packet(&packet, metadata.audio_trim);
                record_trim_fallbacks(&shared, &mut trimmer);
            }
            Ok(Err(_)) | Err(_) => {
                consecutive_errors += 1;
                if consecutive_errors >= 3 {
                    audio_failed(&shared, format!("audio decoder failed 3 times on stream {}", stream.index));
                    return;
                }
                continue;
            }
        }

        // A decoder that does not stamp its frames gets the packet's time
        // for the first one; the rest continue where the previous ended.
        let mut packet_pts = packet.pts;
        loop {
            if quit() {
                return;
            }
            let recv_res =
                std::panic::catch_unwind(AssertUnwindSafe(|| decoder.receive_frame()));
            let frame = match recv_res {
                Ok(Ok(f)) => {
                    consecutive_errors = 0;
                    f
                }
                Ok(Err(oxideav_core::Error::NeedMore))
                | Ok(Err(oxideav_core::Error::Eof)) => break,
                Ok(Err(_)) | Err(_) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= 3 {
                        audio_failed(&shared, format!("audio decoder failed 3 times on stream {}", stream.index));
                        return;
                    }
                    break;
                }
            };
            let Frame::Audio(af) = frame else { continue };
            let chunk = decoded_chunk(decoder.as_ref(), &stream, &af, &mut packet_pts, &mut decoded_end);
            trimmer.frame(chunk, af.pts, &mut kept);
            record_trim_fallbacks(&shared, &mut trimmer);
            if !present_kept(sink, &shared, &mut out, &mut kept, seen_seek, realtime, &retired) {
                // Stopped, retired or a seek: the rest of this packet is stale.
                break;
            }
        }
    }
}

/// Decoded audio on its way to the sink: interleaved f32 in its own layout
/// and the time its first sample plays.
struct Chunk {
    pcm: Vec<f32>,
    channels: usize,
    rate: u32,
    pts: f64,
    /// The first sample after a declared start skip (`Pcm::begins_presentation`).
    begins: bool,
}

impl Pcm for Chunk {
    fn samples(&self) -> usize {
        self.pcm.len() / self.channels.max(1)
    }

    fn rate(&self) -> u32 {
        self.rate
    }

    fn retained_bytes(&self) -> usize {
        self.pcm.capacity() * std::mem::size_of::<f32>()
    }

    fn drop_front(&mut self, n: usize) {
        self.pcm.drain(..n.saturating_mul(self.channels).min(self.pcm.len()));
        self.pts += n as f64 / f64::from(self.rate.max(1));
    }

    fn split_off(&mut self, n: usize) -> Self {
        let rest = self.pcm.split_off(n.saturating_mul(self.channels).min(self.pcm.len()));
        let pts = self.pts + n as f64 / f64::from(self.rate.max(1));
        Chunk { pcm: rest, channels: self.channels, rate: self.rate, pts, begins: false }
    }

    fn begins_presentation(&mut self) {
        self.begins = true;
    }
}

/// A decoded frame as a `Chunk`, stamped where it starts: its own pts, else
/// the packet's for the first frame after a send, else where the decoder's
/// previous frame ended.
fn decoded_chunk(
    decoder: &dyn Decoder,
    stream: &StreamInfo,
    af: &oxideav_core::AudioFrame,
    packet_pts: &mut Option<i64>,
    decoded_end: &mut Option<f64>,
) -> Chunk {
    let layout = audio_trim::frame_layout(decoder.output_audio_format(), &stream.params, af);
    let (format, rate, channels) = (layout.sample_format, layout.sample_rate, layout.channels as usize);
    let pcm = convert_audio_to_f32(af, format, channels);
    let pts = match (af.pts.or(packet_pts.take()), *decoded_end) {
        (Some(ticks), _) => stream.time_base.seconds_of(ticks),
        (None, Some(end)) => end,
        (None, None) => 0.0,
    };
    let frames = pcm.len() / channels.max(1);
    *decoded_end = Some(pts + frames as f64 / f64::from(rate.max(1)));
    Chunk { pcm, channels, rate, pts, begins: false }
}

/// What an audio pipeline's output is set up for and has taken.
struct AudioOut {
    /// The layout the sink was last opened with, and whether that worked.
    rate: u32,
    channels: u16,
    open: bool,
    written: Written,
    /// The newest seek generation whose target the output has reached.
    seen_seek_target: u64,
    /// The seek generation a failed output last let the playback go for.
    primed: Option<u64>,
}

/// Hands what the trimmer released to the sink, in order. False when the
/// player stopped, the pipeline was retired, or a seek superseded the
/// audio; the rest is dropped then.
fn present_kept(
    sink: &mut dyn AudioSink,
    shared: &SharedState,
    out: &mut AudioOut,
    kept: &mut Vec<Chunk>,
    seen_seek: u64,
    realtime: bool,
    retired: &AtomicBool,
) -> bool {
    for chunk in kept.drain(..) {
        if !present_audio(sink, shared, out, chunk, seen_seek, realtime, retired) {
            return false;
        }
    }
    true
}

/// One chunk of decoded audio to the sink, which is (re)opened for its
/// layout. Samples stamped before zero precede the presentation (codec
/// priming no container trim covered), unless a declared start skip just
/// came off: those begin the presentation at zero, as FFmpeg plays them.
/// After a seek, those before its target never play.
fn present_audio(
    sink: &mut dyn AudioSink,
    shared: &SharedState,
    out: &mut AudioOut,
    chunk: Chunk,
    seen_seek: u64,
    realtime: bool,
    retired: &AtomicBool,
) -> bool {
    let Chunk { mut pcm, channels, rate: sample_rate, pts, begins } = chunk;
    if !out.open || sample_rate != out.rate || channels as u16 != out.channels {
        out.rate = sample_rate;
        out.channels = channels as u16;
        out.written.reopen(shared);
        out.open = sink.open(out.rate, out.channels).is_ok();
    }
    let sink_failed = !out.open;

    // Clamping undeclared priming stamped before zero to zero would overlap
    // the first real samples on the output's timeline. Declared priming is
    // gone already; what follows it starts the presentation even when the
    // container's timestamps disagree with its skip.
    let mut pts_secs = pts;
    if pts_secs < 0.0 {
        if !begins {
            let before = (-pts_secs * f64::from(sample_rate)).round() as usize;
            pcm.drain(..(before.saturating_mul(channels)).min(pcm.len()));
        }
        pts_secs = 0.0;
    }

    // Drop pre-target output after a seek: audio before the target never
    // reaches the sink. The chunk that holds the target loses its samples
    // before it, so the clock restarts at the target with the target's own
    // sample.
    if let Some(seek) = *shared.active_seek.lock() {
        if seek.generation > out.seen_seek_target && pts_secs < seek.target {
            let frames = pcm.len() / channels.max(1);
            let before = ((seek.target - pts_secs) * f64::from(sample_rate)).round() as usize;
            if before < frames {
                pcm.drain(..before * channels);
                pts_secs = seek.target;
            } else {
                pcm.clear();
            }
        }
        if !pcm.is_empty() {
            out.seen_seek_target = seen_seek;
        }
    }
    if pcm.is_empty() {
        return true;
    }

    // A failed output cannot prime; let the remaining streams run. A
    // working output primes only after it has actually taken PCM.
    if sink_failed {
        if out.primed != Some(seen_seek) {
            out.primed = Some(seen_seek);
            shared.pipe_primed(Pipe::Audio, seen_seek);
        }
        return true;
    }
    let pts = Duration::from_secs_f64(pts_secs);
    write_pcm(sink, shared, &pcm, channels, sample_rate, pts, seen_seek, realtime, &mut out.written, retired)
}

/// What an audio pipeline knows about its output across writes.
#[derive(Default)]
struct Written {
    /// The clock run state last applied to the sink (`play`/`pause`).
    running: Option<bool>,
    /// The seek generation whose audio the output has taken: from its first
    /// samples on, the output's clock leads the playback.
    leads: Option<u64>,
    /// Where the audio written since the last flush ends.
    end: Option<Duration>,
}

impl Written {
    /// The sink is about to be (re)opened, which restarts its clock: the
    /// free clock carries on from where the output was until it plays the
    /// next write.
    fn reopen(&mut self, shared: &SharedState) {
        shared.master.release_audio();
        self.leads = None;
        self.end = None;
        self.running = None;
    }
}

/// Applies the clock's run state to an audio sink when it changed.
fn sync_audio_sink(sink: &mut dyn AudioSink, shared: &SharedState, applied: &mut Option<bool>) {
    let running = shared.running();
    if *applied != Some(running) {
        if running {
            sink.play();
        } else {
            sink.pause();
        }
        *applied = Some(running);
    }
}

/// Hands interleaved `pcm`, whose first frame plays at `pts`, to the sink,
/// in writes of at most 20 ms so the output follows a change of the clock's
/// run state within one of them. While the clock stands still (buffering or
/// paused), audio goes out only up to `PREROLL` past it, so a paused output
/// never fills up and blocks; what a paused sink did not take is written
/// again once the clock runs, so no audio is skipped across a hold. The
/// first samples the output takes after a seek put its clock in the lead.
/// False when the player stopped, the pipeline was retired, or a seek
/// superseded this audio.
#[allow(clippy::too_many_arguments)]
fn write_pcm(
    sink: &mut dyn AudioSink,
    shared: &SharedState,
    pcm: &[f32],
    channels: usize,
    rate: u32,
    pts: Duration,
    seen_seek: u64,
    realtime: bool,
    written: &mut Written,
    retired: &AtomicBool,
) -> bool {
    let channels = channels.max(1);
    let rate = rate.max(1);
    let chunk = (rate as usize / 50).max(1);
    let frames = pcm.len() / channels;
    let mut done = 0;
    while done < frames {
        let at = pts + Duration::from_secs_f64(done as f64 / f64::from(rate));
        sync_audio_sink(sink, shared, &mut written.running);
        let result = {
            #[cfg(target_os = "macos")]
            let _timing = if realtime { crate::clock::timing::Guard::enter() } else { None };
            if realtime {
                match shared.preroll(at, written.running, None, seen_seek, retired) {
                    Preroll::Go => {}
                    Preroll::Resync => continue,
                    Preroll::Abort => return false,
                }
            }
            let until = frames.min(done + chunk);
            sink.write(&pcm[done * channels..until * channels], at)
        };
        let until = frames.min(done + chunk);
        match result {
            Ok(0) => {
                // Full (or paused): retain these samples and retry once the
                // output can drain.
                if !shared.wait_output(seen_seek, retired) { return false; }
            }
            Ok(n) => {
                done += n.min(until - done);
                written.end = Some(pts + Duration::from_secs_f64(done as f64 / f64::from(rate)));
                if written.leads != Some(seen_seek) {
                    written.leads = Some(seen_seek);
                    shared.master.follow_audio(sink.clock(), seen_seek);
                    shared.pipe_primed(Pipe::Audio, seen_seek);
                }
            }
            Err(SinkError::Unavailable) => {
                // Suspended: the platform's resume recreates the output.
                // Reopening here would restart it in the background. Keep
                // the samples; the free clock leads until audio plays again.
                if written.leads.is_some() {
                    written.reopen(shared);
                }
                if !shared.wait_resumed(seen_seek, retired) { return false; }
            }
            Err(_) => {
                // Sink refused (device lost): keep the engine alive and
                // reopen it for the next samples.
                written.reopen(shared);
                let _ = sink.open(rate, channels as u16);
                return true;
            }
        }
    }
    true
}

fn convert_audio_to_f32(
    af: &oxideav_core::AudioFrame,
    format: SampleFormat,
    channels: usize,
) -> Vec<f32> {
    let n = af.samples as usize;
    let mut out = Vec::with_capacity(n * channels);
    for i in 0..n {
        for c in 0..channels {
            out.push(sample_f32(format, &af.data, channels, c, i));
        }
    }
    out
}

fn sample_f32(
    format: SampleFormat,
    data: &[Vec<u8>],
    channels: usize,
    c: usize,
    i: usize,
) -> f32 {
    let (plane, index) = if format.is_planar() {
        if c < data.len() {
            (&data[c], i)
        } else {
            (&data[0], i)
        }
    } else {
        (&data[0], i * channels + c)
    };
    let w = format.bytes_per_sample();
    let start = index * w;
    if start + w > plane.len() {
        return 0.0;
    }
    let b = &plane[start..start + w];
    match format {
        SampleFormat::U8 | SampleFormat::U8P => (b[0] as f32 - 128.0) / 128.0,
        SampleFormat::S8 => (b[0] as i8 as f32) / 128.0,
        SampleFormat::S16 | SampleFormat::S16P => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
        SampleFormat::S24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8388608.0,
        SampleFormat::S32 | SampleFormat::S32P => {
            i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2147483648.0
        }
        SampleFormat::F32 | SampleFormat::F32P => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        SampleFormat::F64 | SampleFormat::F64P => {
            f64::from_le_bytes(b.try_into().unwrap_or([0; 8])) as f32
        }
        // Non-exhaustive upstream: treat anything new as silence rather than
        // panicking on untrusted input.
        _ => 0.0,
    }
}

/// Video lane → platform decoder or registry software decoder → sink, paced
/// against the clock in realtime, pushed immediately otherwise. The sink
/// follows the clock's run state (`set_playing`).
fn run_video_thread(
    stream: StreamInfo,
    mut sink: Box<dyn VideoSink>,
    lane: Arc<Lane>,
    demux_cv: Arc<Condvar>,
    shared: Arc<SharedState>,
    realtime: bool,
    retired: Arc<AtomicBool>,
) {
    let mut compressed = sink.open_compressed(&stream.params);
    let mut sw_decoder: Option<Box<dyn Decoder>> = None;
    // What the frame sink was last opened with (see `sync_frame_format`).
    let mut frame_format: Option<CodecParameters> = None;
    let mut need_keyframe = !compressed;
    let mut consecutive_errors = 0;
    let mut seen_seek = shared.seek_gen.load(Ordering::SeqCst);
    let mut shown_seek: u64 = 0;
    // The clock run state last applied to the sink (`set_playing`).
    let mut sink_running: Option<bool> = None;
    let mut starved = false;
    let mut primed: Option<u64> = None;
    let mut last_end = Duration::ZERO;
    let mut frame_clock = FrameClock::default();
    let quit = || shared.stopped.load(Ordering::SeqCst) || retired.load(Ordering::SeqCst);

    if !compressed {
        match make_decoder(&shared.ctx, &stream.params) {
            Ok(d) => {
                let _ = sink.open_frames(&stream.params);
                frame_format = Some(stream.params.clone());
                sw_decoder = Some(d);
            }
            Err(e) => {
                // No software decoder either: the track is skipped (still
                // listed in `tracks`) and the rest plays on (audio-only
                // file, or a codec neither backend knows).
                let mut st = shared.state.lock();
                let _ = st
                    .error
                    .get_or_insert_with(|| format!("no video decoder found: {e}"));
                st.video = None;
                drop(st);
                notify_changed(&shared);
                return;
            }
        }
    }

    while !quit() {
        if !realtime {
            // Nothing waits on the clock: park while paused instead.
            shared.wait_while_paused(&retired);
            if quit() {
                break;
            }
        }
        sync_video_sink(&mut *sink, &shared, &mut sink_running);

        // Seek generation: start the decoder over; drop pre-target frames;
        // resume from the next keyframe.
        let gen_now = shared.seek_gen.load(Ordering::SeqCst);
        if gen_now != seen_seek {
            seen_seek = gen_now;
            sink.flush();
            last_end = Duration::ZERO;
            frame_clock = FrameClock::default();
            if sw_decoder.is_some() {
                match make_decoder(&shared.ctx, &stream.params) {
                    Ok(d) => sw_decoder = Some(d),
                    Err(e) => {
                        let mut st = shared.state.lock();
                        let _ = st
                            .error
                            .get_or_insert_with(|| format!("video decoder failed to restart: {e}"));
                        st.video = None;
                        drop(st);
                        notify_changed(&shared);
                        return;
                    }
                }
            }
            need_keyframe = true;
            consecutive_errors = 0;
        }

        let woken = || quit() || Some(shared.running()) != sink_running;
        let report = |dry| shared.pipe_starved(Pipe::Video, dry);
        let QueuedPacket { packet, metadata } = match lane.pop(seen_seek, &demux_cv, woken, &mut starved, report) {
            Pop::Packet(p) => p,
            Pop::Wake => continue,
            Pop::Eof => {
                // Drain the decoder's delayed frames, on the clock like the
                // rest.
                if let Some(dec) = sw_decoder.as_mut() {
                    let _ = dec.flush();
                    while !quit() {
                        let recv = std::panic::catch_unwind(AssertUnwindSafe(|| dec.receive_frame()));
                        match recv {
                            Ok(Ok(Frame::Video(vf))) => {
                                let ticks = frame_clock.time(vf.pts, None);
                                let secs = stream.time_base.seconds_of(ticks).max(0.0);
                                if before_seek_target(&shared, secs, seen_seek, &mut shown_seek) {
                                    continue;
                                }
                                let shown = present_frame(
                                    &mut *sink, &shared, &vf, Duration::from_secs_f64(secs),
                                    seen_seek, realtime, &mut sink_running, &mut primed, &retired,
                                );
                                if !shown {
                                    break;
                                }
                            }
                            Ok(Ok(_)) => {}
                            _ => break,
                        }
                    }
                }
                if compressed {
                    while !quit() && shared.seek_gen.load(Ordering::SeqCst) == seen_seek {
                        sync_video_sink(&mut *sink, &shared, &mut sink_running);
                        if !matches!(sink.finish(), Err(SinkError::WouldBlock)) { break; }
                        shared.wait_retry();
                    }
                }
                if realtime {
                    // Timestamped renderers still own queued frames at EOF.
                    // Keep the sink alive through the final presentation.
                    loop {
                        sync_video_sink(&mut *sink, &shared, &mut sink_running);
                        match shared.preroll(last_end, sink_running, Some(Duration::ZERO), seen_seek, &retired) {
                            Preroll::Resync => continue,
                            Preroll::Go | Preroll::Abort => break,
                        }
                    }
                }
                if shared.seek_gen.load(Ordering::SeqCst) != seen_seek { continue; }
                break;
            }
        };
        if let Some(end) = packet_end_secs(&packet) {
            last_end = last_end.max(Duration::from_secs_f64(end.max(0.0)));
        }
        frame_clock.note_duration(packet.duration);

        let random_access = packet.flags.keyframe || metadata.container_keyframe;
        if need_keyframe && !random_access {
            continue;
        }
        need_keyframe = false;

        if compressed && packet.pts.is_none() {
            // A platform decoder presents each picture at its input's
            // timestamp. Raw elementary streams leave reference pictures
            // untimed (only their decode order is known), so only the
            // software decoder can time them: switch before pushing.
            sink.flush();
            compressed = false;
            sw_decoder = match software_fallback(&shared, &stream, &mut *sink) {
                Some(d) => Some(d),
                None => return,
            };
            frame_format = Some(stream.params.clone());
            if !random_access {
                need_keyframe = true;
                continue;
            }
        }

        if compressed {
            let ticks = packet.pts.unwrap_or(0).max(0);
            let pts = Duration::from_secs_f64(stream.time_base.seconds_of(ticks).max(0.0));
            if primed != Some(seen_seek) {
                primed = Some(seen_seek);
                shared.pipe_primed(Pipe::Video, seen_seek);
            }
            // While the clock stands still the platform decoder cannot
            // present anything: feed it only up to `PREROLL` past the clock,
            // so its input queue never fills and blocks `push_packet`.
            if realtime && !preroll_video(&mut *sink, &shared, pts, &mut sink_running, seen_seek, &retired) {
                continue;
            }
            sync_video_sink(&mut *sink, &shared, &mut sink_running);
            let result = loop {
                if quit() || shared.seek_gen.load(Ordering::SeqCst) != seen_seek { break Ok(()); }
                sync_video_sink(&mut *sink, &shared, &mut sink_running);
                match sink.push_packet(&packet, pts, random_access) {
                    Err(SinkError::WouldBlock) => shared.wait_retry(),
                    result => break result,
                }
            };
            match result {
                Ok(()) => {
                    consecutive_errors = 0;
                }
                Err(SinkError::Fallback(_)) => {
                    // Platform decoder cannot continue: software from the
                    // next keyframe.
                    compressed = false;
                    need_keyframe = true;
                    sw_decoder = match software_fallback(&shared, &stream, &mut *sink) {
                        Some(d) => Some(d),
                        None => return,
                    };
                    frame_format = Some(stream.params.clone());
                }
                Err(SinkError::Fatal(f)) => {
                    set_error(&shared, format!("video fatal error: {f}"));
                    return;
                }
                Err(_) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= 3 {
                        let mut st = shared.state.lock();
                        let _ = st.error.get_or_insert_with(|| {
                            format!("video sink failed 3 times on stream {}", stream.index)
                        });
                        st.video = None;
                        drop(st);
                        notify_changed(&shared);
                        return;
                    }
                }
            }
        } else if let Some(decoder) = sw_decoder.as_mut() {
            let send_res =
                std::panic::catch_unwind(AssertUnwindSafe(|| decoder.send_packet(&packet)));
            match send_res {
                Ok(Ok(())) => {
                    consecutive_errors = 0;
                }
                Ok(Err(_)) | Err(_) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= 3 {
                        let mut st = shared.state.lock();
                        let _ = st.error.get_or_insert_with(|| {
                            format!("video decoder failed 3 times on stream {}", stream.index)
                        });
                        st.video = None;
                        drop(st);
                        notify_changed(&shared);
                        return;
                    }
                    continue;
                }
            }

            loop {
                if quit() {
                    return;
                }
                let recv_res =
                    std::panic::catch_unwind(AssertUnwindSafe(|| decoder.receive_frame()));
                let frame = match recv_res {
                    Ok(Ok(f)) => {
                        consecutive_errors = 0;
                        f
                    }
                    Ok(Err(oxideav_core::Error::NeedMore))
                    | Ok(Err(oxideav_core::Error::Eof)) => break,
                    Ok(Err(_)) | Err(_) => {
                        consecutive_errors += 1;
                        if consecutive_errors >= 3 {
                            let mut st = shared.state.lock();
                            let _ = st.error.get_or_insert_with(|| {
                                format!(
                                    "video decoder failed 3 times on stream {}",
                                    stream.index
                                )
                            });
                            st.video = None;
                            drop(st);
                            notify_changed(&shared);
                            return;
                        }
                        break;
                    }
                };
                let Frame::Video(vf) = frame else { continue };
                sync_frame_format(&mut *sink, &shared, &stream.params, &**decoder, &mut frame_format);

                let frame_ticks = frame_clock.time(vf.pts, packet.pts);
                let frame_pts_secs = stream.time_base.seconds_of(frame_ticks).max(0.0);

                // Drop everything before the seek target, for seeks this
                // thread has not shown a frame for yet.
                if before_seek_target(&shared, frame_pts_secs, seen_seek, &mut shown_seek) {
                    continue;
                }

                let shown = present_frame(
                    &mut *sink, &shared, &vf, Duration::from_secs_f64(frame_pts_secs),
                    seen_seek, realtime, &mut sink_running, &mut primed, &retired,
                );
                if !shown {
                    // Stopped, retired or a seek: the decoder's output is stale.
                    break;
                }
            }
        }
    }
}

/// Timestamps for decoded pictures that carry none. FFmpeg leaves many
/// pictures untimed (raw and MPEG-PS H.264 time only some access units);
/// its consumers continue the timeline from the previous picture's time
/// plus its duration (fftools `ffmpeg_dec.c` video_frame_process). A
/// timed picture always keeps its own time.
#[derive(Default)]
struct FrameClock {
    /// Where the next untimed picture goes: the last picture plus `duration`.
    next: Option<i64>,
    /// The latest positive packet duration, in stream ticks.
    duration: Option<i64>,
}

impl FrameClock {
    fn note_duration(&mut self, duration: Option<i64>) {
        if let Some(d) = duration.filter(|&d| d > 0) {
            self.duration = Some(d);
        }
    }

    /// The picture's time in stream ticks: its own, else the continued
    /// timeline, else (first picture) the packet's, else zero.
    fn time(&mut self, frame_pts: Option<i64>, packet_pts: Option<i64>) -> i64 {
        let ticks = frame_pts.or(self.next).or(packet_pts).unwrap_or(0).max(0);
        self.next = self.duration.map(|d| ticks.saturating_add(d));
        ticks
    }
}

/// Whether a frame at `pts_secs` precedes the target of the latest seek
/// while this pipeline has shown nothing since it (`shown_seek` is the
/// newest seek generation it has shown a frame for). Such frames are
/// dropped; the first frame at or after the target closes the window.
fn before_seek_target(shared: &SharedState, pts_secs: f64, seen_seek: u64, shown_seek: &mut u64) -> bool {
    let Some(seek) = *shared.active_seek.lock() else {
        return false;
    };
    if seek.generation > *shown_seek && pts_secs < seek.target {
        return true;
    }
    *shown_seek = seen_seek;
    false
}

/// Applies the clock's run state to a video sink when it changed.
fn sync_video_sink(sink: &mut dyn VideoSink, shared: &SharedState, applied: &mut Option<bool>) {
    let running = shared.running();
    if *applied != Some(running) {
        sink.set_playing(running);
        *applied = Some(running);
    }
}

/// (Re)opens the frame sink at the size and pixel layout the decoder reports
/// for the frame it just returned, when those differ from what the sink was
/// opened with. The container's values stand in only where the decoder
/// reports none: a raw elementary stream declares no size, and a stream may
/// change size mid-way. The size is published as `State::video_size`.
fn sync_frame_format(
    sink: &mut dyn VideoSink,
    shared: &SharedState,
    params: &CodecParameters,
    decoder: &dyn Decoder,
    opened: &mut Option<CodecParameters>,
) {
    let mut want = params.clone();
    if let Some((w, h)) = decoder.output_video_dimensions() {
        want.width = Some(w);
        want.height = Some(h);
    }
    if let Some(format) = decoder.output_pixel_format() {
        want.pixel_format = Some(format);
    }
    let unchanged = opened.as_ref().is_some_and(|o| {
        (o.width, o.height, o.pixel_format) == (want.width, want.height, want.pixel_format)
    });
    if unchanged {
        return;
    }
    let _ = sink.open_frames(&want);
    if let (Some(w), Some(h)) = (want.width, want.height) {
        if w > 0 && h > 0 {
            let changed = std::mem::replace(&mut shared.state.lock().video_size, Some((w, h))) != Some((w, h));
            if changed {
                notify_changed(shared);
            }
        }
    }
    *opened = Some(want);
}

/// `SharedState::preroll` for a video sink: applies the clock's run state
/// to the sink whenever it changes while waiting. False when the player
/// stopped, the pipeline was retired, or a seek superseded the packet.
fn preroll_video(
    sink: &mut dyn VideoSink,
    shared: &SharedState,
    pts: Duration,
    applied: &mut Option<bool>,
    seen_seek: u64,
    retired: &AtomicBool,
) -> bool {
    #[cfg(target_os = "macos")]
    let _timing = crate::clock::timing::Guard::enter();
    loop {
        sync_video_sink(sink, shared, applied);
        match shared.preroll(pts, *applied, Some(Duration::from_millis(500)), seen_seek, retired) {
            Preroll::Go => return true,
            Preroll::Resync => {}
            Preroll::Abort => return false,
        }
    }
}

/// Hands one decoded frame to the sink: in realtime once it is due on the
/// clock, `VideoSink::frame_lead` before its `pts` (more than 100 ms late it
/// is dropped and counted), immediately otherwise. A frame waiting for its
/// time counts as output ready for the buffering hold. False when the player
/// stopped, the pipeline was retired, or a seek superseded the frame.
#[allow(clippy::too_many_arguments)]
fn present_frame(
    sink: &mut dyn VideoSink,
    shared: &SharedState,
    frame: &oxideav_core::VideoFrame,
    pts: Duration,
    seen_seek: u64,
    realtime: bool,
    sink_running: &mut Option<bool>,
    primed: &mut Option<u64>,
    retired: &AtomicBool,
) -> bool {
    if *primed != Some(seen_seek) {
        *primed = Some(seen_seek);
        shared.pipe_primed(Pipe::Video, seen_seek);
    }
    if realtime {
        loop {
            sync_video_sink(sink, shared, sink_running);
            match shared.wait_due(pts, sink.frame_lead(), *sink_running, seen_seek, retired) {
                Due::Now => break,
                Due::Resync => continue,
                Due::Late => {
                    shared.state.lock().dropped_frames += 1;
                    return true;
                }
                Due::Abort => return false,
            }
        }
    }
    sync_video_sink(sink, shared, sink_running);
    let _ = sink.push_frame(frame, pts);
    true
}

/// The software decoder for `stream` after its platform decoder gave up,
/// with the sink switched to frames. `None` (error recorded) when there is
/// no software decoder.
fn software_fallback(
    shared: &SharedState,
    stream: &StreamInfo,
    sink: &mut dyn VideoSink,
) -> Option<Box<dyn oxideav_core::Decoder>> {
    match make_decoder(&shared.ctx, &stream.params) {
        Ok(d) => {
            let _ = sink.open_frames(&stream.params);
            Some(d)
        }
        Err(e) => {
            let mut st = shared.state.lock();
            let _ = st.error.get_or_insert_with(|| format!("video fallback failed: {e}"));
            drop(st);
            notify_changed(shared);
            None
        }
    }
}
