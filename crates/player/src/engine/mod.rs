use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, MutexGuard};

use oxideav_core::{
    Decoder, Demuxer, Frame, MediaType, Packet, ProbeData, RuntimeContext,
    SampleFormat, StreamInfo, TimeBase, PROBE_SCORE_EXTENSION,
};

use crate::backend::{AudioSink, Backend, Clock, SinkError, VideoSink};
use crate::clock::FreeRunningClock;
use crate::headless::find_headless;
use crate::source::{open_source, ReadAheadSource, SourceMonitor};
use crate::subs::run_subtitle_loop;

mod transport;
use transport::{Due, Live, Pipe, Transport};

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
}

/// What a pipeline got from its lane.
enum Pop {
    Packet(Packet),
    /// The demuxer's end marker.
    Eof,
    /// The caller's `wake` condition turned true while the lane was empty.
    Wake,
}

impl Lane {
    fn new() -> Arc<Lane> {
        Arc::new(Lane {
            queue: Mutex::new(Vec::new()),
            cv: Condvar::new(),
        })
    }

    fn push(&self, packet: Packet) {
        self.queue.lock().push(packet);
        self.cv.notify_one();
    }

    fn push_eof(&self) {
        self.queue.lock().push_eof_marker();
        self.cv.notify_all();
    }

    fn clear(&self) {
        self.queue.lock().clear();
    }

    /// Media span (seconds between the first and last pts) and bytes queued.
    fn queued(&self, time_base: TimeBase) -> (f64, usize) {
        let q = self.queue.lock();
        let mut first: Option<f64> = None;
        let mut last: Option<f64> = None;
        let mut bytes = 0;
        for p in q.iter() {
            let secs = time_base.seconds_of(p.pts.unwrap_or(0));
            first.get_or_insert(secs);
            last = Some(secs);
            bytes += p.data.len();
        }
        let span = match (first, last) {
            (Some(a), Some(b)) => (b - a).max(0.0),
            _ => 0.0,
        };
        (span, bytes)
    }

    /// The next packet or the end marker, waiting while the lane is empty.
    /// An empty lane short of its end starves the pipeline: `report(true)`
    /// when that starts and `report(false)` once a packet or the end
    /// arrives (`starved` carries the reported state across calls; both
    /// reports run with the lane unlocked). Returns `Wake` as soon as `wake`
    /// holds while the lane is empty.
    fn pop(
        &self,
        demux_cv: &Condvar,
        wake: impl Fn() -> bool,
        starved: &mut bool,
        report: impl Fn(bool),
    ) -> Pop {
        let mut q = self.queue.lock();
        let popped = loop {
            match q.first() {
                Some(p) if p.stream_index == u32::MAX => {
                    q.remove(0);
                    break Pop::Eof;
                }
                Some(_) => break Pop::Packet(q.remove(0)),
                None if wake() => break Pop::Wake,
                None if !*starved => {
                    *starved = true;
                    MutexGuard::unlocked(&mut q, || report(true));
                }
                None => {
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
    /// Play/pause intent and the buffering hold; decides when `free_clock`
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

impl SharedState {
    /// The clock every sink of this playback follows: the free-running
    /// clock, held while buffering (the audio sinks follow it through
    /// `AudioSink::play`/`pause`).
    fn sink_clock(&self) -> Arc<dyn Clock> {
        self.free_clock.clone()
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

        // Playing is the intent; the clock holds at the start (buffering)
        // until the first audio/video is ready.
        let initial_state = State {
            playing: true,
            buffering: true,
            ..State::default()
        };

        let free = Arc::new(FreeRunningClock::new());
        free.set_position(Duration::ZERO);

        let stopped = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(SharedState {
            state: Mutex::new(initial_state),
            stopped,
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
            threads: Mutex::new(vec![init_thread]),
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
        {
            let mut target = self.shared.seek_target.lock();
            *target = Some(to);
        }
        self.shared.seek_gen.fetch_add(1, Ordering::SeqCst);
        self.shared.seek_clock(to);
        {
            let mut st = self.shared.state.lock();
            st.position = to;
        }
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
        self.shared.stop();
        self.shared.condvar.notify_all();
        let threads = self.threads.lock().drain(..).collect::<Vec<_>>();
        for t in threads {
            let _ = t.join();
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
            set_error(&shared, format!("failed to open source: {e}"));
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
            set_error(&shared, format!("failed to open demuxer: {e}"));
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

    let threads: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

    // 7. Initial pipeline threads. The demux loop re-spawns the audio and
    // subtitle threads on a selection switch; the video pipeline stays fixed
    // (the public API cannot switch it mid-playback).
    if let Some(stream) = current_video.and_then(|i| streams.iter().find(|s| s.index == i).cloned())
    {
        let lane = Arc::clone(&video_lane);
        let demux_cv2 = Arc::clone(&demux_cv);
        let shared2 = Arc::clone(&shared);
        let sink = shared.backend.video(shared.sink_clock());
        let ctx_video = Arc::clone(&shared.ctx);
        let realtime = options.realtime;
        let live = Live::new(&shared, Pipe::Video);
        let handle = std::thread::Builder::new()
            .name("peartube-video".into())
            .spawn(move || {
                let _live = live;
                run_video_thread(stream, sink, lane, demux_cv2, shared2, ctx_video, realtime);
            })
            .expect("failed to spawn video thread");
        threads.lock().push(handle);
    }
    if let Some(stream) = current_audio.and_then(|i| streams.iter().find(|s| s.index == i).cloned())
    {
        let lane = Arc::clone(&audio_lane);
        let demux_cv2 = Arc::clone(&demux_cv);
        let shared2 = Arc::clone(&shared);
        let sink = shared.backend.audio();
        let ctx_audio = Arc::clone(&shared.ctx);
        let realtime = options.realtime;
        let live = Live::new(&shared, Pipe::Audio);
        let handle = std::thread::Builder::new()
            .name("peartube-audio".into())
            .spawn(move || {
                let _live = live;
                run_audio_thread(stream, sink, lane, demux_cv2, shared2, ctx_audio, realtime);
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
    // respawn the affected pipeline thread without ending playback. From
    // here the buffering hold follows the pipelines' data.
    shared.pipelines_started();
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
    let mut full = false;

    while !shared.stopped.load(Ordering::SeqCst) {
        // Selection switch: flush lanes, respawn changed pipelines.
        let gen_now = shared.select_gen.load(Ordering::SeqCst);
        if gen_now != select_gen_seen {
            select_gen_seen = gen_now;
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

        // Bounded queues: wait while a lane is full. The buffering hold
        // lets go then: nothing more can be queued until the clock moves.
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
            // Consumers notify `demux_cv` on every packet they take.
            let mut none: Option<()> = None;
            let guard = Mutex::new(&mut none);
            let mut g = guard.lock();
            run.demux_cv.wait_for(&mut g, Duration::from_millis(10));
            continue;
        }

        let packet_res = std::panic::catch_unwind(AssertUnwindSafe(|| run.demuxer.next_packet()));
        match packet_res {
            Ok(Ok(packet)) => {
                let stream_id = packet.stream_index;
                let pipe = if Some(stream_id) == *run.current_video {
                    Some(Pipe::Video)
                } else if Some(stream_id) == *run.current_audio {
                    Some(Pipe::Audio)
                } else {
                    None
                };
                let end = pipe.and_then(|_| packet_end_secs(&packet));
                match pipe {
                    Some(Pipe::Video) => run.video_lane.push(packet),
                    Some(Pipe::Audio) => run.audio_lane.push(packet),
                    None if Some(stream_id) == *run.current_subtitle => run.sub_lane.push(packet),
                    // Inactive streams' packets are dropped.
                    None => {}
                }
                if let (Some(pipe), Some(end)) = (pipe, end) {
                    shared.demuxed(pipe, end);
                }
            }
            Ok(Err(oxideav_core::Error::Eof)) => {
                eof = true;
                run.video_lane.push_eof();
                run.audio_lane.push_eof();
                run.sub_lane.push_eof();
                shared.demux_eof(true);
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
    let full = |lane: &Lane, tb: TimeBase, max_bytes: usize| {
        let (secs, bytes) = lane.queued(tb);
        secs >= QUEUE_MAX_SECS || bytes >= max_bytes
    };
    full(run.video_lane, run.video_tb, VIDEO_MAX_BYTES)
        || full(run.audio_lane, run.audio_tb, AUDIO_MAX_BYTES)
        || full(run.sub_lane, run.sub_tb, SUB_MAX_BYTES)
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
            let live = Live::new(shared, Pipe::Audio);
            let handle = std::thread::Builder::new()
                .name("peartube-audio".into())
                .spawn(move || {
                    let _live = live;
                    run_audio_thread(stream, sink, lane, demux_cv, shared2, ctx_audio, realtime);
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

/// Packet lanes → decoder → sink, for one audio stream. The sink follows the
/// clock's run state (`play`/`pause`); while the clock stands still, PCM goes
/// out only up to `PREROLL` past it. Reaches Ended with the rest of the
/// pipeline at EOF.
fn run_audio_thread(
    stream: StreamInfo,
    mut sink: Box<dyn AudioSink>,
    lane: Arc<Lane>,
    demux_cv: Arc<Condvar>,
    shared: Arc<SharedState>,
    ctx: Arc<RuntimeContext>,
    realtime: bool,
) {
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
    // The clock run state last applied to the sink (`play`/`pause`).
    let mut sink_running: Option<bool> = None;
    let mut starved = false;
    let mut consecutive_errors = 0;
    let mut seen_seek = shared.seek_gen.load(Ordering::SeqCst);
    let mut seen_seek_target: u64 = 0;
    let mut primed: Option<u64> = None;

    while !shared.stopped.load(Ordering::SeqCst) {
        if !realtime {
            // Nothing waits on the clock: park while paused instead.
            shared.wait_while_paused();
            if shared.stopped.load(Ordering::SeqCst) {
                break;
            }
        }
        sync_audio_sink(&mut *sink, &shared, &mut sink_running);

        // Seek generation: always reset the decoder and the sink, drop
        // pre-target output after the demuxer's seek lands.
        let gen_now = shared.seek_gen.load(Ordering::SeqCst);
        if gen_now != seen_seek {
            seen_seek = gen_now;
            let _ = sink.flush();
            let _ = decoder.reset();
            consecutive_errors = 0;
        }

        // Pull a packet; the EOF marker ends this pipeline.
        let woken = || shared.stopped.load(Ordering::SeqCst) || Some(shared.running()) != sink_running;
        let report = |dry| shared.pipe_starved(Pipe::Audio, dry);
        let packet = match lane.pop(&demux_cv, woken, &mut starved, report) {
            Pop::Packet(p) => p,
            Pop::Wake => continue,
            Pop::Eof => {
                // Drain the decoder's tail into the sink.
                let _ = decoder.flush();
                while !shared.stopped.load(Ordering::SeqCst) {
                    let recv = std::panic::catch_unwind(AssertUnwindSafe(|| decoder.receive_frame()));
                    match recv {
                        Ok(Ok(Frame::Audio(af))) => {
                            let (format, rate, channels) =
                                audio_layout(decoder.as_ref(), &stream.params, &af);
                            let pcm = convert_audio_to_f32(&af, format, channels as usize);
                            if pcm.is_empty() {
                                break;
                            }
                            let ticks = af.pts.unwrap_or(0).max(0);
                            let secs = stream.time_base.seconds_of(ticks).max(0.0);
                            let pts = Duration::from_secs_f64(secs);
                            if !write_pcm(
                                &mut *sink, &shared, &pcm, channels as usize, rate, pts,
                                seen_seek, realtime, &mut sink_running,
                            ) {
                                break;
                            }
                        }
                        Ok(Ok(_)) => {}
                        _ => break,
                    }
                }
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

            let (format, sample_rate, channels) = audio_layout(decoder.as_ref(), &stream.params, &af);
            let channels = channels as usize;

            if !sink_open || sample_rate != current_rate || (channels as u16) != current_channels {
                current_rate = sample_rate;
                current_channels = channels as u16;
                sink_open = sink.open(current_rate, current_channels).is_ok();
                sink_running = None;
            }
            let sink_failed = !sink_open;

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
                if !pcm.is_empty() {
                    seen_seek_target = seen_seek;
                }
            }
            if pcm.is_empty() {
                continue;
            }

            // Decoded audio counts as ready even when the output refused to
            // open: the clock must not wait for a sink that drops it.
            if primed != Some(seen_seek) {
                primed = Some(seen_seek);
                shared.pipe_primed(Pipe::Audio, seen_seek);
            }
            if sink_failed {
                continue;
            }
            let pts = Duration::from_secs_f64(pts_secs);
            if !write_pcm(
                &mut *sink, &shared, &pcm, channels, sample_rate, pts, seen_seek, realtime,
                &mut sink_running,
            ) {
                // Stopped, or a seek: the rest of this packet is stale.
                break;
            }
        }
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

/// Hands interleaved `pcm`, whose first frame plays at `pts`, to the sink.
/// While the clock stands still (buffering or paused), audio goes out only up
/// to `PREROLL` past it, so a paused output never fills up and blocks; what a
/// paused sink did not take is written again once the clock runs, so no
/// audio is skipped across a hold. False when the player stopped or a seek
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
    sink_running: &mut Option<bool>,
) -> bool {
    let channels = channels.max(1);
    let frames = pcm.len() / channels;
    let mut done = 0;
    while done < frames {
        let at = pts + Duration::from_secs_f64(done as f64 / f64::from(rate.max(1)));
        if realtime && !shared.preroll(at, seen_seek) {
            return false;
        }
        sync_audio_sink(sink, shared, sink_running);
        match sink.write(&pcm[done * channels..frames * channels], at) {
            Ok(0) => {
                // A paused output that is full: write the rest once the clock
                // runs. A playing output that takes nothing has nowhere to
                // put it.
                if shared.running() {
                    return true;
                }
                if !shared.wait_running(seen_seek) {
                    return false;
                }
            }
            Ok(n) => done += n,
            Err(_) => {
                // Sink refused (device lost): keep the engine alive; the
                // platform resume path reopens it.
                let _ = sink.open(rate, channels as u16);
                *sink_running = None;
                return true;
            }
        }
    }
    true
}

/// The layout of `af`: what the decoder says it emits, else the container's
/// declaration corrected by the frame's actual plane count and byte length.
/// Containers often declare a different format, rate or channel count than
/// the decoder produces (HE-AAC, parametric stereo, S16 decoders), and
/// reading S16 bytes as f32 yields garbage and NaNs.
fn audio_layout(
    decoder: &dyn oxideav_core::Decoder,
    params: &oxideav_core::CodecParameters,
    af: &oxideav_core::AudioFrame,
) -> (SampleFormat, u32, u16) {
    if let Some(f) = decoder.output_audio_format() {
        return (f.sample_format, f.sample_rate, f.channels);
    }
    let rate = params.sample_rate.unwrap_or(48000);
    let declared = params.sample_format;
    let planar = af.data.len() > 1;
    let channels = if planar { af.data.len() as u16 } else { params.channels.unwrap_or(1).max(1) };
    let per_plane = if planar { 1 } else { channels as usize };
    let samples = (af.samples as usize).max(1);
    let bytes = af.data.first().map_or(0, Vec::len);
    let width = bytes / (samples * per_plane);
    let fits = |f: SampleFormat| f.is_planar() == planar && f.bytes_per_sample() == width;
    let format = match declared {
        Some(f) if fits(f) => f,
        _ => {
            // 4-byte samples are f32 or s32: keep the declared family.
            let float = declared.map_or(true, |f| {
                matches!(f, SampleFormat::F32 | SampleFormat::F32P | SampleFormat::F64 | SampleFormat::F64P)
            });
            match (width, planar, float) {
                (1, false, _) => SampleFormat::U8,
                (1, true, _) => SampleFormat::U8P,
                (2, false, _) => SampleFormat::S16,
                (2, true, _) => SampleFormat::S16P,
                (3, false, _) => SampleFormat::S24,
                (4, false, true) => SampleFormat::F32,
                (4, false, false) => SampleFormat::S32,
                (4, true, true) => SampleFormat::F32P,
                (4, true, false) => SampleFormat::S32P,
                (8, false, _) => SampleFormat::F64,
                (8, true, _) => SampleFormat::F64P,
                _ => declared.unwrap_or(SampleFormat::F32),
            }
        }
    };
    (format, rate, channels)
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
    ctx: Arc<RuntimeContext>,
    realtime: bool,
) {
    let mut compressed = sink.open_compressed(&stream.params);
    let mut sw_decoder: Option<Box<dyn Decoder>> = None;
    let mut need_keyframe = !compressed;
    let mut consecutive_errors = 0;
    let mut seen_seek = shared.seek_gen.load(Ordering::SeqCst);
    let mut seen_seek_target: u64 = 0;
    // The clock run state last applied to the sink (`set_playing`).
    let mut sink_running: Option<bool> = None;
    let mut starved = false;
    let mut primed: Option<u64> = None;

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
        if !realtime {
            // Nothing waits on the clock: park while paused instead.
            shared.wait_while_paused();
            if shared.stopped.load(Ordering::SeqCst) {
                break;
            }
        }
        sync_video_sink(&mut *sink, &shared, &mut sink_running);

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

        let woken = || shared.stopped.load(Ordering::SeqCst) || Some(shared.running()) != sink_running;
        let report = |dry| shared.pipe_starved(Pipe::Video, dry);
        let packet = match lane.pop(&demux_cv, woken, &mut starved, report) {
            Pop::Packet(p) => p,
            Pop::Wake => continue,
            Pop::Eof => {
                // Drain the decoder's delayed frames, on the clock like the
                // rest.
                if let Some(dec) = sw_decoder.as_mut() {
                    let _ = dec.flush();
                    while !shared.stopped.load(Ordering::SeqCst) {
                        let recv = std::panic::catch_unwind(AssertUnwindSafe(|| dec.receive_frame()));
                        match recv {
                            Ok(Ok(Frame::Video(vf))) => {
                                let ticks = vf.pts.unwrap_or(0).max(0);
                                let secs = stream.time_base.seconds_of(ticks).max(0.0);
                                let shown = present_frame(
                                    &mut *sink, &shared, &vf, Duration::from_secs_f64(secs),
                                    seen_seek, realtime, &mut sink_running, &mut primed,
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
                break;
            }
        };

        if need_keyframe && !packet.flags.keyframe {
            continue;
        }
        need_keyframe = false;

        if compressed {
            let ticks = packet.pts.unwrap_or(0).max(0);
            let pts = Duration::from_secs_f64(stream.time_base.seconds_of(ticks).max(0.0));
            // While the clock stands still the platform decoder cannot
            // present anything: feed it only up to `PREROLL` past the clock,
            // so its input queue never fills and blocks `push_packet`.
            if realtime && !shared.preroll(pts, seen_seek) {
                continue;
            }
            sync_video_sink(&mut *sink, &shared, &mut sink_running);
            if primed != Some(seen_seek) {
                primed = Some(seen_seek);
                shared.pipe_primed(Pipe::Video, seen_seek);
            }
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

                let shown = present_frame(
                    &mut *sink, &shared, &vf, Duration::from_secs_f64(frame_pts_secs),
                    seen_seek, realtime, &mut sink_running, &mut primed,
                );
                if !shown {
                    // Stopped, or a seek: the decoder's output is stale.
                    break;
                }
            }
        }
    }
}

/// Applies the clock's run state to a video sink when it changed.
fn sync_video_sink(sink: &mut dyn VideoSink, shared: &SharedState, applied: &mut Option<bool>) {
    let running = shared.running();
    if *applied != Some(running) {
        sink.set_playing(running);
        *applied = Some(running);
    }
}

/// Hands one decoded frame to the sink: in realtime once it is due on the
/// clock (pushed up to 100 ms early; more than 100 ms late it is dropped and
/// counted), immediately otherwise. A frame waiting for its time counts as
/// output ready for the buffering hold. False when the player stopped or a
/// seek superseded the frame.
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
) -> bool {
    if *primed != Some(seen_seek) {
        *primed = Some(seen_seek);
        shared.pipe_primed(Pipe::Video, seen_seek);
    }
    if realtime {
        match shared.wait_due(pts, seen_seek) {
            Due::Now => {}
            Due::Late => {
                shared.state.lock().dropped_frames += 1;
                return true;
            }
            Due::Abort => return false,
        }
    }
    sync_video_sink(sink, shared, sink_running);
    let _ = sink.push_frame(frame, pts);
    true
}
