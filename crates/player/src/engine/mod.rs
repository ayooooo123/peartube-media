use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use oxideav_core::{
    Decoder, Demuxer, Frame, MediaType, Packet, ProbeData, RuntimeContext,
    SampleFormat, StreamInfo, TimeBase, PROBE_SCORE_EXTENSION,
};

use crate::backend::{AudioSink, Backend, Clock, SinkError, VideoSink};
use crate::clock::FreeRunningClock;
use crate::headless::find_headless;
use crate::source::{open_source, ReadAheadSource};
use crate::subs::run_subtitle_loop;

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

/// One decoder lane. A lane owns its packet queue and wakes the demux loop
/// whenever it drains, so the demuxer never stalls behind a slow sink.
pub(crate) struct Lane {
    pub(crate) queue: Mutex<Vec<Packet>>,
    pub(crate) cv: Condvar,
    /// Bytes currently queued, tracked alongside the Vec to avoid rescans.
    bytes: AtomicU64,
    /// Set when the demux loop hit EOF or a fatal demux error: the queue ends
    /// with a final `None` the consumer removes before treating a bare empty
    /// queue as "keep waiting".
    eof: AtomicBool,
}

impl Lane {
    fn new() -> Arc<Lane> {
        Arc::new(Lane {
            queue: Mutex::new(Vec::new()),
            cv: Condvar::new(),
            bytes: AtomicU64::new(0),
            eof: AtomicBool::new(false),
        })
    }

    fn push(&self, packet: Packet) {
        self.bytes.fetch_add(packet.data.len() as u64, Ordering::SeqCst);
        self.queue.lock().push(packet);
        self.cv.notify_one();
    }

    fn push_eof(&self) {
        self.eof.store(true, Ordering::SeqCst);
        self.queue.lock().push_eof_marker();
        self.cv.notify_all();
    }

    fn clear(&self) {
        let mut q = self.queue.lock();
        q.clear();
        // A cleared queue may have been past EOF (seek); reopen it.
        self.eof.store(false, Ordering::SeqCst);
        self.bytes.store(0, Ordering::SeqCst);
    }

    fn queued_secs(&self, time_base: TimeBase) -> f64 {
        let q = self.queue.lock();
        let mut last: Option<f64> = None;
        let mut first: Option<f64> = None;
        for p in q.iter() {
            let secs = time_base.seconds_of(p.pts.unwrap_or(0));
            if first.is_none() {
                first = Some(secs);
            }
            last = Some(secs);
        }
        match (first, last) {
            (Some(a), Some(b)) => (b - a).max(0.0),
            _ => 0.0,
        }
    }
}

trait PushEof {
    fn push_eof_marker(&mut self);
}
impl PushEof for Vec<Packet> {
    fn push_eof_marker(&mut self) {
        // EOF marker: a packet with stream_index == u32::MAX.
        self.push(Packet {
            stream_index: u32::MAX,
            time_base: TimeBase::new(1, 1000),
            pts: None,
            dts: None,
            duration: None,
            flags: Default::default(),
            data: Vec::new(),
        });
    }
}

struct SharedState {
    state: Mutex<State>,
    stopped: Arc<AtomicBool>,
    paused: AtomicBool,
    condvar: Condvar,
    on_event: Arc<dyn Fn(Event) + Send + Sync>,
    last_changed: Mutex<Instant>,
    source: Mutex<Option<Arc<ReadAheadSource>>>,
    /// Monotonic counter bumped by every `seek`. The demux loop applies the
    /// newest request; decoder threads compare their local copy against it to
    /// detect a seek they have not yet honoured.
    seek_gen: AtomicU64,
    /// Seek request from the latest `seek`, consumed by the demux loop.
    seek_target: Mutex<Option<Duration>>,
    /// Seek the demux loop has applied (`seek_to` returned): generation and
    /// target. Decoder threads read it to drop pre-target output.
    active_seek: Mutex<Option<Seek>>,
    /// Selection written by `select_audio` / `select_subtitle`; the demux
    /// loop applies it (flush + respawn the pipeline) and mirrors `state`.
    wanted_audio: Mutex<Option<u32>>,
    wanted_video: Mutex<Option<u32>>,
    wanted_subtitle: Mutex<Option<u32>>,
    /// Bumped on every selection change; the demux loop compares to detect it.
    select_gen: AtomicU64,
    backend: Arc<dyn Backend>,
    free_clock: Arc<FreeRunningClock>,
    ctx: Arc<RuntimeContext>,
}

#[derive(Clone, Copy, Debug)]
struct Seek {
    generation: u64,
    /// Target in seconds.
    target: f64,
}

impl SharedState {
    /// The clock every sink of this playback follows: the free-running clock
    /// (the audio sink's headless clock also derives its reads from the audio
    /// writes; the platform sinks' own clock keeps audio as master).
    fn sink_clock(&self) -> Arc<dyn Clock> {
        self.free_clock.clone()
    }

    /// The pipeline stops feeding while paused: every thread parks here
    /// instead of decoding ahead, and resumes on `play`.
    fn wait_while_paused(&self) {
        if !self.paused.load(Ordering::SeqCst) {
            return;
        }
        let gate = Mutex::new(());
        let mut g = gate.lock();
        while self.paused.load(Ordering::SeqCst) && !self.stopped.load(Ordering::SeqCst) {
            self.condvar.wait_for(&mut g, Duration::from_millis(20));
        }
    }
}

pub struct Player {
    shared: Arc<SharedState>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
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

        let initial_state = State {
            playing: true,
            ..State::default()
        };

        let free = Arc::new(FreeRunningClock::new());
        free.play();

        let stopped = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(SharedState {
            state: Mutex::new(initial_state),
            stopped,
            paused: AtomicBool::new(false),
            condvar: Condvar::new(),
            on_event: on_event_arc,
            last_changed: Mutex::new(Instant::now() - Duration::from_secs(1)),
            source: Mutex::new(None),
            seek_gen: AtomicU64::new(0),
            seek_target: Mutex::new(None),
            active_seek: Mutex::new(None),
            wanted_audio: Mutex::new(options.audio),
            wanted_video: Mutex::new(options.video),
            wanted_subtitle: Mutex::new(options.subtitle),
            select_gen: AtomicU64::new(1),
            backend,
            free_clock: Arc::clone(&free),
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
            threads: Mutex::new(vec![init_thread]),
        }
    }

    pub fn play(&self) {
        self.shared.paused.store(false, Ordering::SeqCst);
        self.shared.free_clock.play();
        {
            let mut st = self.shared.state.lock();
            st.playing = true;
        }
        self.shared.condvar.notify_all();
        notify_changed(&self.shared);
    }

    pub fn pause(&self) {
        self.shared.paused.store(true, Ordering::SeqCst);
        self.shared.free_clock.pause();
        {
            let mut st = self.shared.state.lock();
            st.playing = false;
        }
        notify_changed(&self.shared);
    }

    pub fn seek(&self, to: Duration) {
        {
            let mut target = self.shared.seek_target.lock();
            *target = Some(to);
        }
        self.shared.seek_gen.fetch_add(1, Ordering::SeqCst);
        self.shared.free_clock.set_position(to);
        {
            let mut st = self.shared.state.lock();
            st.position = to;
        }
        self.shared.condvar.notify_all();
        notify_changed(&self.shared);
    }

    pub fn select_audio(&self, stream: Option<u32>) {
        *self.shared.wanted_audio.lock() = stream;
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
        st.position = self.shared.free_clock.now().unwrap_or(st.position);
        st.clone()
    }

    pub fn wait(&self) -> State {
        let mut st = self.shared.state.lock();
        while !st.ended && st.error.is_none() && !self.shared.stopped.load(Ordering::SeqCst) {
            self.shared.condvar.wait(&mut st);
        }
        st.position = self.shared.free_clock.now().unwrap_or(st.position);
        st.clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        self.shared.condvar.notify_all();
        let threads = self.threads.lock().drain(..).collect::<Vec<_>>();
        for t in threads {
            let _ = t.join();
        }
    }
}

fn notify_changed(shared: &Arc<SharedState>) {
    let mut last = shared.last_changed.lock();
    let now = Instant::now();
    if now.duration_since(*last) >= Duration::from_millis(100) {
        *last = now;
        (shared.on_event)(Event::Changed);
    }
}

fn set_error(shared: &Arc<SharedState>, err: String) {
    {
        let mut st = shared.state.lock();
        if st.error.is_none() {
            st.error = Some(err.clone());
            st.playing = false;
        }
    }
    (shared.on_event)(Event::Error(err));
    shared.condvar.notify_all();
}

fn set_ended(shared: &Arc<SharedState>) {
    {
        let mut st = shared.state.lock();
        st.ended = true;
        st.playing = false;
    }
    (shared.on_event)(Event::Ended);
    shared.condvar.notify_all();
}

/// A `Read + Seek` handle for the demuxer: one read-ahead source per consumer
/// (probe, demuxer), each over its own connection to the URL.
struct SourceHandle {
    url: String,
    inner: Option<ReadAheadSource>,
}

impl SourceHandle {
    fn new(url: &str) -> std::io::Result<Self> {
        Ok(Self {
            url: url.to_string(),
            inner: Some(open_source(url)?),
        })
    }
}

impl std::io::Read for SourceHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.inner.is_none() {
            self.inner = Some(open_source(&self.url)?);
        }
        self.inner.as_mut().unwrap().read(buf)
    }
}

impl std::io::Seek for SourceHandle {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        if self.inner.is_none() {
            self.inner = Some(open_source(&self.url)?);
        }
        self.inner.as_mut().unwrap().seek(pos)
    }
}

impl Drop for SourceHandle {
    fn drop(&mut self) {
        self.inner = None;
    }
}

fn run_player_pipeline(
    url: String,
    options: PlayerOptions,
    shared: Arc<SharedState>,
) {
    let ctx = &*shared.ctx;
    // 1. Open the source, keep it for suspend/resume.
    let source = match open_source(&url) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            set_error(&shared, format!("failed to open source: {e}"));
            return;
        }
    };
    *shared.source.lock() = Some(Arc::clone(&source));

    // 2. Probe (rule from engine-api.md, same as refcheck).
    let container = match probe_container(&url, &ctx) {
        Ok(c) => c,
        Err(e) => {
            set_error(&shared, e);
            return;
        }
    };

    // 3. Demuxer. Container codec tags resolve through the registry — the
    //    mpeg4video fork claims Matroska's MPEG-4 Part 2 CodecIDs
    //    (V_MPEG4/ISO/ASP, //SP, //AP) directly.
    let demuxer_source = SourceHandle::new(&url);
    let mut demuxer = match demuxer_source {
        Ok(src) => match ctx
            .containers
            .open_demuxer(&container, Box::new(src), &ctx.codecs)
        {
            Ok(d) => d,
            Err(e) => {
                set_error(&shared, format!("failed to open demuxer: {e}"));
                return;
            }
        },
        Err(e) => {
            set_error(&shared, format!("failed to open source: {e}"));
            return;
        }
    };

    // 4. Streams (cap 64; drop video tracks above the size limits).
    let streams_all = demuxer.streams();
    let count = streams_all.len().min(64);
    let streams: Vec<StreamInfo> = streams_all[..count].to_vec();

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

    // 5. Selection. The pipeline thread owns `current_*`; `select_*` writes
    // `wanted_*` and bumps `select_gen` so this loop applies switches.
    let options_video = *shared.wanted_video.lock();
    let mut current_video = options_video.or(first_video);
    let mut current_audio = (*shared.wanted_audio.lock()).or(first_audio);
    let mut current_subtitle = *shared.wanted_subtitle.lock();

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
    }

    // 6. Lanes + sinks.
    let video_lane = Lane::new();
    let audio_lane = Lane::new();
    let sub_lane = Lane::new();
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

    let threads: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

    // 7. Initial pipeline threads. The demux loop re-spawns the audio and
    // subtitle threads on a selection switch; the video pipeline stays fixed
    // (the public API cannot switch it mid-playback).
    if let Some(stream) = current_video.and_then(|i| streams.iter().find(|s| s.index == i).cloned())
    {
        let lane = Arc::clone(&video_lane);
        let demux_cv2 = Arc::clone(&demux_cv);
        let shared2 = Arc::clone(&shared);
        let clock = shared.sink_clock();
        let sink = shared.backend.video(Arc::clone(&clock));
        let ctx_video = Arc::clone(&shared.ctx);
        let realtime = options.realtime;
        let handle = std::thread::Builder::new()
            .name("peartube-video".into())
            .spawn(move || {
                run_video_thread(
                    stream, sink, clock, lane, demux_cv2, shared2, ctx_video, realtime,
                );
            })
            .expect("failed to spawn video thread");
        threads.lock().push(handle);
    }
    if let Some(stream) = current_audio.and_then(|i| streams.iter().find(|s| s.index == i).cloned())
    {
        let lane = Arc::clone(&audio_lane);
        let demux_cv2 = Arc::clone(&demux_cv);
        let shared2 = Arc::clone(&shared);
        let clock = shared.sink_clock();
        let sink = shared.backend.audio();
        let ctx_audio = Arc::clone(&shared.ctx);
        let realtime = options.realtime;
        let handle = std::thread::Builder::new()
            .name("peartube-audio".into())
            .spawn(move || {
                run_audio_thread(
                    stream, sink, clock, lane, demux_cv2, shared2, ctx_audio, realtime,
                );
            })
            .expect("failed to spawn audio thread");
        threads.lock().push(handle);
    }
    if let Some(stream) =
        current_subtitle.and_then(|i| streams.iter().find(|s| s.index == i).cloned())
    {
        let lane = Arc::clone(&sub_lane);
        let demux_cv2 = Arc::clone(&demux_cv);
        let shared2 = Arc::clone(&shared);
        let ctx_sub = Arc::clone(&shared.ctx);
        let clock = shared.sink_clock();
        let sink = shared.backend.subtitles();
        let realtime = options.realtime;
        let (w, h) = shared.state.lock().video_size.unwrap_or((320, 240));
        let handle = std::thread::Builder::new()
            .name("peartube-subtitles".into())
            .spawn(move || {
                let decoder = match ctx_sub.codecs.first_decoder(&stream.params) {
                    Ok(d) => d,
                    Err(_) => return,
                };
                run_subtitle_loop(
                    decoder,
                    sink,
                    clock,
                    stream.time_base,
                    w,
                    h,
                    realtime,
                    lane,
                    demux_cv2,
                    shared2.stopped.clone(),
                );
            })
            .expect("failed to spawn subtitle thread");
        threads.lock().push(handle);
    }

    // 8. Demux loop owns spawn/join so a selection switch can drain lanes and
    // respawn the affected pipeline thread without ending playback.
    run_demux_loop(&mut Run {
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
        threads: &threads,
        current_video: &mut current_video,
        current_audio: &mut current_audio,
        current_subtitle: &mut current_subtitle,
    });

    for t in threads.lock().drain(..) {
        let _ = t.join();
    }

    if !shared.stopped.load(Ordering::SeqCst) && shared.state.lock().error.is_none() {
        set_ended(&shared);
    }
}

fn probe_container(url: &str, ctx: &RuntimeContext) -> Result<String, String> {
    // The probe re-opens the URL through its own read-ahead source; the
    // demuxer's read position is untouched.
    let mut probe_reader = SourceHandle::new(url).map_err(|e| format!("failed to open source: {e}"))?;
    let mut probe_buf = vec![0u8; 256 * 1024];
    let n = std::io::Read::read(&mut probe_reader, &mut probe_buf)
        .map_err(|e| format!("failed to read for probe: {e}"))?;
    let _ = std::io::Seek::seek(&mut probe_reader, std::io::SeekFrom::Start(0));
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

#[allow(clippy::too_many_arguments)]
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
    threads: &'a Mutex<Vec<std::thread::JoinHandle<()>>>,
    current_video: &'a mut Option<u32>,
    current_audio: &'a mut Option<u32>,
    current_subtitle: &'a mut Option<u32>,
}

fn run_demux_loop(run: &mut Run<'_>) {
    let shared = run.shared;
    let mut active: Vec<u32> = [
        *run.current_video,
        *run.current_audio,
        *run.current_subtitle,
    ]
    .into_iter()
    .flatten()
    .collect();
    let _ = run.demuxer.set_active_streams(&active);
    let mut select_gen_seen = shared.select_gen.load(Ordering::SeqCst);
    let mut eof = false;

    while !shared.stopped.load(Ordering::SeqCst) {
        // Selection switch: flush lanes, respawn changed pipelines.
        let gen_now = shared.select_gen.load(Ordering::SeqCst);
        if gen_now != select_gen_seen {
            select_gen_seen = gen_now;
            apply_selection_switch(run, &mut active);
            eof = false;
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

        // Bounded queues: wait while every lane is full.
        if lanes_full(run) && !eof {
            let mut none: Option<()> = None;
            let guard = Mutex::new(&mut none);
            let mut g = guard.lock();
            run.demux_cv.wait_for(&mut g, Duration::from_millis(50));
            continue;
        }

        if eof {
            // Wait until the lanes with consumers drained their tail (the
            // EOF marker) so a trailing packet is never dropped, then leave.
            // A lane without a pipeline thread (no such stream, or the stream
            // was disabled) has no consumer, so only lanes whose stream is
            // currently selected are waited on. Leaving ends
            // `run_player_pipeline`, which joins the pipelines and sets
            // Ended. A seek or a selection switch clears lanes and reopens
            // `eof` at the top of this loop.
            let tail = |lane: &Arc<Lane>, selected: bool| {
                if !selected {
                    return 0;
                }
                lane.queue.lock().len()
            };
            let drained = tail(&run.video_lane, run.current_video.is_some())
                + tail(&run.audio_lane, run.current_audio.is_some())
                + tail(&run.sub_lane, run.current_subtitle.is_some())
                == 0;
            if drained {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }

        let packet_res = std::panic::catch_unwind(AssertUnwindSafe(|| run.demuxer.next_packet()));
        match packet_res {
            Ok(Ok(packet)) => {
                let stream_id = packet.stream_index;
                if Some(stream_id) == *run.current_video {
                    run.video_lane.push(packet);
                } else if Some(stream_id) == *run.current_audio {
                    run.audio_lane.push(packet);
                } else if Some(stream_id) == *run.current_subtitle {
                    run.sub_lane.push(packet);
                }
                // Inactive streams' packets are dropped.
            }
            Ok(Err(oxideav_core::Error::Eof)) => {
                eof = true;
                run.video_lane.push_eof();
                run.audio_lane.push_eof();
                run.sub_lane.push_eof();
            }
            Ok(Err(e)) => {
                // Transient demux errors are retried; a demuxer that keeps
                // failing still ends playback through the 30 s read timeout
                // upstream, so surface immediately otherwise.
                set_error(shared, format!("demux error: {e}"));
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
    let v_full = run.video_lane.bytes.load(Ordering::SeqCst) >= VIDEO_MAX_BYTES as u64
        || run.video_lane.queued_secs(run.video_tb) >= QUEUE_MAX_SECS;
    let a_full = run.audio_lane.bytes.load(Ordering::SeqCst) >= AUDIO_MAX_BYTES as u64
        || run.audio_lane.queued_secs(run.audio_tb) >= QUEUE_MAX_SECS;
    let s_full = run.sub_lane.bytes.load(Ordering::SeqCst) >= SUB_MAX_BYTES as u64
        || run.sub_lane.queued_secs(run.sub_tb) >= QUEUE_MAX_SECS;
    v_full || a_full || s_full
}

fn do_seek(run: &mut Run<'_>, target: Duration, generation: u64, eof: &mut bool) {
    let shared = run.shared;
    // Convert to the seek stream's time base. The demuxer seeks the video
    // stream when present, else audio, else stream 0.
    let seek_stream = (*run.current_video).or(*run.current_audio).unwrap_or(0);
    let tb = run
        .streams
        .iter()
        .find(|s| s.index == seek_stream)
        .map(|s| s.time_base)
        .unwrap_or_else(|| TimeBase::new(1, 1000));
    let ticks = tb.ticks_of(target.as_secs_f64());

    *shared.seek_target.lock() = None;
    *shared.active_seek.lock() = Some(Seek {
        generation,
        target: target.as_secs_f64(),
    });
    run.video_lane.clear();
    run.audio_lane.clear();
    run.sub_lane.clear();
    *eof = false;

    let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
        run.demuxer.seek_to(seek_stream, ticks)
    }));
    match res {
        Ok(Ok(_)) | Ok(Err(_)) => {}
        Err(_) => {
            set_error(shared, "demuxer panicked during seek".into());
        }
    }
    shared.condvar.notify_all();
}

/// `select_audio` / `select_subtitle` took effect: flush the affected lane,
/// respawn its thread (a switch = flush that pipeline and resume at the
/// current position), and refresh the headless registry entry.
fn apply_selection_switch(run: &mut Run<'_>, active: &mut Vec<u32>) {
    let shared = run.shared;

    let wanted_audio = *shared.wanted_audio.lock();
    let wanted_subtitle = *shared.wanted_subtitle.lock();
    let audio_changed = wanted_audio != *run.current_audio;
    let sub_changed = wanted_subtitle != *run.current_subtitle;

    if audio_changed {
        run.audio_lane.clear();
    }
    if sub_changed {
        run.sub_lane.clear();
    }

    // Video selection is not switchable through the public API (options only),
    // but keep the state consistent.
    let wanted_video = *shared.wanted_video.lock();

    *run.current_audio = wanted_audio;
    *run.current_subtitle = wanted_subtitle;
    *run.current_video = wanted_video;

    {
        let mut st = shared.state.lock();
        st.audio = wanted_audio;
        st.subtitle = wanted_subtitle;
        st.video = wanted_video;
        st.video_size = wanted_video.and_then(|idx| {
            run.streams.iter().find(|s| s.index == idx).and_then(|s| {
                let (w, h) = (s.params.width?, s.params.height?);
                (w > 0 && h > 0).then_some((w, h))
            })
        });
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
            info(*run.current_video, MediaType::Video),
            info(*run.current_audio, MediaType::Audio),
            info(*run.current_subtitle, MediaType::Subtitle),
            run.options.realtime,
        );
    }

    // Respawn the audio pipeline if the selection changed. The old thread
    // exits on its own: its lane was cleared and never refilled for the old
    // stream, and the demux loop now feeds the new one.
    if audio_changed {
        let audio_idx = *run.current_audio;
        let stream = audio_idx.and_then(|i| run.streams.iter().find(|s| s.index == i).cloned());
        if let Some(stream) = stream {
            let lane = Arc::clone(run.audio_lane);
            let demux_cv = Arc::clone(run.demux_cv);
            let shared2 = Arc::clone(shared);
            let ctx_audio = Arc::clone(&shared.ctx);
            let realtime = run.options.realtime;
            let sink = shared.backend.audio();
            let clock = shared.sink_clock();
            let handle = std::thread::Builder::new()
                .name("peartube-audio".into())
                .spawn(move || {
                    run_audio_thread(stream, sink, clock, lane, demux_cv, shared2, ctx_audio, realtime);
                })
                .expect("failed to spawn audio thread");
            run.threads.lock().push(handle);
        }
    }
    if sub_changed {
        let sub_idx = *run.current_subtitle;
        let stream = sub_idx.and_then(|i| run.streams.iter().find(|s| s.index == i).cloned());
        if let Some(stream) = stream {
            let lane = Arc::clone(run.sub_lane);
            let demux_cv = Arc::clone(run.demux_cv);
            let shared2 = Arc::clone(shared);
            let ctx_sub = Arc::clone(&shared.ctx);
            let realtime = run.options.realtime;
            let sink = shared.backend.subtitles();
            let clock = shared.sink_clock();
            let (w, h) = shared.state.lock().video_size.unwrap_or((320, 240));
            let handle = std::thread::Builder::new()
                .name("peartube-subtitles".into())
                .spawn(move || {
                    let decoder = match ctx_sub.codecs.first_decoder(&stream.params) {
                        Ok(d) => d,
                        Err(_) => return,
                    };
                    run_subtitle_loop(
                        decoder,
                        sink,
                        clock,
                        stream.time_base,
                        w,
                        h,
                        realtime,
                        lane,
                        demux_cv,
                        shared2.stopped.clone(),
                    );
                })
                .expect("failed to spawn subtitle thread");
            run.threads.lock().push(handle);
        }
    }

    *active = [
        *run.current_video,
        *run.current_audio,
        *run.current_subtitle,
    ]
    .into_iter()
    .flatten()
    .collect();
    let _ = run.demuxer.set_active_streams(active);
}

/// Packet lanes → decoder → sink, for one audio stream. The sink's clock is
/// the master. Reaches Ended with the rest of the pipeline at EOF.
#[allow(clippy::too_many_arguments)]
fn run_audio_thread(
    stream: StreamInfo,
    mut sink: Box<dyn AudioSink>,
    clock: Arc<dyn Clock>,
    lane: Arc<Lane>,
    demux_cv: Arc<Condvar>,
    shared: Arc<SharedState>,
    ctx: Arc<RuntimeContext>,
    realtime: bool,
) {
    let _ = realtime;
    let mut decoder = match ctx.codecs.first_decoder(&stream.params) {
        Ok(d) => d,
        Err(e) => {
            // No decoder: the stream stays silent, playback continues.
            let mut st = shared.state.lock();
            st.error.get_or_insert_with(|| format!("no audio decoder found: {e}"));
            drop(st);
            notify_changed(&shared);
            return;
        }
    };

    let mut current_rate = stream.params.sample_rate.unwrap_or(48000);
    let mut current_channels = stream.params.channels.unwrap_or(2);
    let mut sink_open = sink.open(current_rate, current_channels).is_ok();
    let mut consecutive_errors = 0;
    let mut seen_seek = shared.seek_gen.load(Ordering::SeqCst);
    let mut seen_seek_target: u64 = 0;
    let mut eof_seen = false;

    while !shared.stopped.load(Ordering::SeqCst) {
        // Pause gate: no feeding while paused.
        shared.wait_while_paused();
        if shared.stopped.load(Ordering::SeqCst) {
            break;
        }

        // Seek generation: always reset the decoder and the sink, drop
        // pre-target output after the demuxer's seek lands.
        let gen_now = shared.seek_gen.load(Ordering::SeqCst);
        if gen_now != seen_seek {
            seen_seek = gen_now;
            let _ = sink.flush();
            let _ = decoder.reset();
            consecutive_errors = 0;
        }

        // Pull a packet: None (EOF marker) ends this pipeline.
        let packet = {
            let mut q = lane.queue.lock();
            loop {
                match q.first() {
                    Some(p) if p.stream_index == u32::MAX => {
                        q.remove(0);
                        break None;
                    }
                    Some(_) => break Some(q.remove(0)),
                    None => {
                        if shared.stopped.load(Ordering::SeqCst) {
                            break None;
                        }
                        demux_cv.notify_one();
                        lane.cv.wait_for(&mut q, Duration::from_millis(100));
                    }
                }
            }
        };
        let Some(packet) = packet else {
            // EOF marker: drain the decoder's tail into the sink.
            let _ = decoder.flush();
            while !shared.stopped.load(Ordering::SeqCst) {
                let recv = std::panic::catch_unwind(AssertUnwindSafe(|| decoder.receive_frame()));
                match recv {
                    Ok(Ok(Frame::Audio(af))) => {
                        let channels = stream.params.channels.unwrap_or(1) as usize;
                        let format = stream.params.sample_format.unwrap_or(SampleFormat::F32);
                        let pcm = convert_audio_to_f32(&af, format, channels);
                        if pcm.is_empty() {
                            break;
                        }
                        let ticks = af.pts.unwrap_or(0).max(0);
                        let secs = stream.time_base.seconds_of(ticks).max(0.0);
                        let _ = sink.write(&pcm, Duration::from_secs_f64(secs));
                    }
                    Ok(Ok(_)) => {}
                    _ => break,
                }
            }
            eof_seen = true;
            break;
        };
        demux_cv.notify_one();

        // Decode one packet under catch_unwind. A decoder that panics or
        // errors 3 times in a row on this stream disables it: the track goes
        // silent but playback continues.
        let send_res = std::panic::catch_unwind(AssertUnwindSafe(|| decoder.send_packet(&packet)));
        match send_res {
            Ok(Ok(())) => {
                consecutive_errors = 0;
            }
            Ok(Err(_)) | Err(_) => {
                consecutive_errors += 1;
                if consecutive_errors >= 3 {
                    let mut st = shared.state.lock();
                    let _ = st.error.get_or_insert_with(|| {
                        format!("audio decoder failed 3 times on stream {}", stream.index)
                    });
                    st.audio = None;
                    drop(st);
                    notify_changed(&shared);
                    return;
                }
                continue;
            }
        }

        loop {
            if shared.stopped.load(Ordering::SeqCst) {
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
                            format!("audio decoder failed 3 times on stream {}", stream.index)
                        });
                        st.audio = None;
                        drop(st);
                        notify_changed(&shared);
                        return;
                    }
                    break;
                }
            };
            let Frame::Audio(af) = frame else { continue };

            let channels = stream.params.channels.unwrap_or(1) as usize;
            let sample_rate = stream.params.sample_rate.unwrap_or(48000);
            let format = stream.params.sample_format.unwrap_or(SampleFormat::F32);

            if !sink_open || sample_rate != current_rate || (channels as u16) != current_channels {
                current_rate = sample_rate;
                current_channels = channels as u16;
                sink_open = sink.open(current_rate, current_channels).is_ok();
            }
            if !sink_open {
                continue;
            }

            let mut pcm = convert_audio_to_f32(&af, format, channels);
            let ticks = af.pts.or(packet.pts).unwrap_or(0).max(0);
            let mut pts_secs = stream.time_base.seconds_of(ticks).max(0.0);

            // Drop pre-target output after a seek: anything that still
            // decodes before the target never reaches the sink. The first
            // frame at/after the target is clamped to it so the clock
            // restarts exactly at the seek point.
            if let Some(seek) = *shared.active_seek.lock() {
                if seek.generation > seen_seek_target && pts_secs < seek.target {
                    let frame_dur = af.samples as f64 / sample_rate as f64;
                    if seek.target - pts_secs <= frame_dur {
                        pts_secs = seek.target;
                    } else {
                        pcm.clear();
                    }
                }
                seen_seek_target = seen_seek;
            }
            if pcm.is_empty() {
                continue;
            }

            if sink.write(&pcm, Duration::from_secs_f64(pts_secs)).is_err() {
                // Sink refused (device lost): keep the engine alive; the
                // platform resume path reopens it.
                let _ = sink.open(current_rate, current_channels);
            }
            let _ = &clock;
        }
    }
    let _ = eof_seen;
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
/// against the master clock in realtime, pushed immediately otherwise.
#[allow(clippy::too_many_arguments)]
fn run_video_thread(
    stream: StreamInfo,
    mut sink: Box<dyn VideoSink>,
    clock: Arc<dyn Clock>,
    lane: Arc<Lane>,
    demux_cv: Arc<Condvar>,
    shared: Arc<SharedState>,
    ctx: Arc<RuntimeContext>,
    realtime: bool,
) {
    let mut compressed = sink.open_compressed(&stream.params);
    let mut sw_decoder: Option<Box<dyn Decoder>> = None;
    let mut need_keyframe = !compressed;
    let mut consecutive_errors = 0;
    let mut seen_seek = shared.seek_gen.load(Ordering::SeqCst);
    let mut seen_seek_target: u64 = 0;

    if !compressed {
        match ctx.codecs.first_decoder(&stream.params) {
            Ok(d) => {
                let _ = sink.open_frames(&stream.params);
                sw_decoder = Some(d);
            }
            Err(e) => {
                // No software decoder either: no video, playback continues
                // (audio-only file, or a codec neither backend knows).
                let mut st = shared.state.lock();
                let _ = st
                    .error
                    .get_or_insert_with(|| format!("no video decoder found: {e}"));
                drop(st);
                notify_changed(&shared);
                return;
            }
        }
    }

    while !shared.stopped.load(Ordering::SeqCst) {
        // Pause gate.
        shared.wait_while_paused();
        if shared.stopped.load(Ordering::SeqCst) {
            break;
        }

        // Seek generation: always reset decoder state; drop pre-target
        // frames; resume from the next keyframe.
        let gen_now = shared.seek_gen.load(Ordering::SeqCst);
        if gen_now != seen_seek {
            seen_seek = gen_now;
            sink.flush();
            if let Some(dec) = sw_decoder.as_mut() {
                let _ = dec.reset();
            }
            need_keyframe = true;
            consecutive_errors = 0;
        }

        let packet = {
            let mut q = lane.queue.lock();
            loop {
                match q.first() {
                    Some(p) if p.stream_index == u32::MAX => {
                        q.remove(0);
                        break None;
                    }
                    Some(_) => break Some(q.remove(0)),
                    None => {
                        if shared.stopped.load(Ordering::SeqCst) {
                            break None;
                        }
                        demux_cv.notify_one();
                        lane.cv.wait_for(&mut q, Duration::from_millis(100));
                    }
                }
            }
        };
        let Some(packet) = packet else {
            // EOF marker: drain the decoder (delayed frames) into the sink.
            if let Some(dec) = sw_decoder.as_mut() {
                let _ = dec.flush();
                while !shared.stopped.load(Ordering::SeqCst) {
                    let recv = std::panic::catch_unwind(AssertUnwindSafe(|| dec.receive_frame()));
                    match recv {
                        Ok(Ok(Frame::Video(vf))) => {
                            let ticks = vf.pts.unwrap_or(0).max(0);
                            let secs = stream.time_base.seconds_of(ticks).max(0.0);
                            let _ = sink.push_frame(&vf, Duration::from_secs_f64(secs));
                        }
                        Ok(Ok(_)) => {}
                        _ => break,
                    }
                }
            }
            break;
        };
        demux_cv.notify_one();

        if need_keyframe && !packet.flags.keyframe {
            continue;
        }
        need_keyframe = false;

        if compressed {
            let ticks = packet.pts.unwrap_or(0).max(0);
            let pts = Duration::from_secs_f64(stream.time_base.seconds_of(ticks).max(0.0));
            match sink.push_packet(&packet, pts) {
                Ok(()) => {
                    consecutive_errors = 0;
                }
                Err(SinkError::Fallback(_)) => {
                    // Platform decoder cannot continue: software from the
                    // next keyframe.
                    compressed = false;
                    need_keyframe = true;
                    match ctx.codecs.first_decoder(&stream.params) {
                        Ok(d) => {
                            let _ = sink.open_frames(&stream.params);
                            sw_decoder = Some(d);
                        }
                        Err(e) => {
                            let mut st = shared.state.lock();
                            let _ = st
                                .error
                                .get_or_insert_with(|| format!("video fallback failed: {e}"));
                            drop(st);
                            notify_changed(&shared);
                            return;
                        }
                    }
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
                if shared.stopped.load(Ordering::SeqCst) {
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

                let frame_ticks = vf.pts.or(packet.pts).unwrap_or(0).max(0);
                let frame_pts_secs = stream.time_base.seconds_of(frame_ticks).max(0.0);

                // Drop everything before the seek target, for seeks this
                // thread has not yet consumed. The first frame at or after
                // the target clears the window.
                if let Some(seek) = *shared.active_seek.lock() {
                    if seek.generation > seen_seek_target && frame_pts_secs < seek.target {
                        continue;
                    }
                    seen_seek_target = seen_seek;
                }

                let frame_pts = Duration::from_secs_f64(frame_pts_secs);
                if !realtime {
                    let _ = sink.push_frame(&vf, frame_pts);
                } else {
                    // Realtime pacing: push up to 100 ms before pts; drop
                    // frames more than 100 ms late (counted in State).
                    let mut dropped = false;
                    while !shared.stopped.load(Ordering::SeqCst) {
                        if let Some(now) = clock.now() {
                            if now > frame_pts + Duration::from_millis(100) {
                                shared.state.lock().dropped_frames += 1;
                                dropped = true;
                                break;
                            }
                            if frame_pts <= now + Duration::from_millis(100) {
                                break;
                            }
                            let lead = frame_pts - now;
                            let sleep = (lead - Duration::from_millis(100))
                                .min(Duration::from_millis(10));
                            std::thread::sleep(sleep);
                        } else {
                            break;
                        }
                    }
                    if !dropped && !shared.stopped.load(Ordering::SeqCst) {
                        let _ = sink.push_frame(&vf, frame_pts);
                    }
                }
            }
        }
    }
}
