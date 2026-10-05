use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use oxideav_core::{
    Decoder, Demuxer, Frame, MediaType, Packet, ProbeData, RuntimeContext, SampleFormat,
    StreamInfo, PROBE_SCORE_EXTENSION,
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

struct SharedState {
    state: Mutex<State>,
    clock: Arc<dyn Clock>,
    free_clock: Option<FreeRunningClock>,
    stopped: Arc<AtomicBool>,
    paused: AtomicBool,
    condvar: Condvar,
    on_event: Arc<dyn Fn(Event) + Send + Sync>,
    last_changed: Mutex<Instant>,
    source: Mutex<Option<Arc<ReadAheadSource>>>,
    seek_target: Mutex<Option<Duration>>,
    active_audio_stream: Mutex<Option<u32>>,
    active_subtitle_stream: Mutex<Option<u32>>,
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
            position: Duration::ZERO,
            duration: None,
            playing: true,
            buffering: false,
            ended: false,
            error: None,
            tracks: Vec::new(),
            audio: None,
            video: None,
            subtitle: None,
            video_size: None,
            video_decoder: None,
            dropped_frames: 0,
        };

        let free_clock = FreeRunningClock::new();
        let master_clock: Arc<dyn Clock> = Arc::new(free_clock.clone());

        let shared = Arc::new(SharedState {
            state: Mutex::new(initial_state),
            clock: master_clock,
            free_clock: Some(free_clock),
            stopped: Arc::new(AtomicBool::new(false)),
            paused: AtomicBool::new(false),
            condvar: Condvar::new(),
            on_event: on_event_arc,
            last_changed: Mutex::new(Instant::now() - Duration::from_secs(1)),
            source: Mutex::new(None),
            seek_target: Mutex::new(None),
            active_audio_stream: Mutex::new(None),
            active_subtitle_stream: Mutex::new(None),
        });

        let url_owned = url.to_string();
        let shared_clone = Arc::clone(&shared);

        let init_thread = std::thread::Builder::new()
            .name("peartube-player-init".into())
            .spawn(move || {
                run_player_pipeline(url_owned, backend, ctx, options, shared_clone);
            })
            .expect("failed to spawn player pipeline thread");

        Player {
            shared,
            threads: Mutex::new(vec![init_thread]),
        }
    }

    pub fn play(&self) {
        self.shared.paused.store(false, Ordering::SeqCst);
        if let Some(fc) = &self.shared.free_clock {
            fc.play();
        }
        {
            let mut st = self.shared.state.lock();
            st.playing = true;
        }
        notify_changed(&self.shared);
    }

    pub fn pause(&self) {
        self.shared.paused.store(true, Ordering::SeqCst);
        if let Some(fc) = &self.shared.free_clock {
            fc.pause();
        }
        {
            let mut st = self.shared.state.lock();
            st.playing = false;
        }
        notify_changed(&self.shared);
    }

    pub fn seek(&self, to: Duration) {
        *self.shared.seek_target.lock() = Some(to);
        if let Some(fc) = &self.shared.free_clock {
            fc.seek(to);
        }
        {
            let mut st = self.shared.state.lock();
            st.position = to;
        }
        self.shared.condvar.notify_all();
        notify_changed(&self.shared);
    }

    pub fn select_audio(&self, stream: Option<u32>) {
        *self.shared.active_audio_stream.lock() = stream;
        {
            let mut st = self.shared.state.lock();
            st.audio = stream;
        }
        self.shared.condvar.notify_all();
        notify_changed(&self.shared);
    }

    pub fn select_subtitle(&self, stream: Option<u32>) {
        *self.shared.active_subtitle_stream.lock() = stream;
        {
            let mut st = self.shared.state.lock();
            st.subtitle = stream;
        }
        self.shared.condvar.notify_all();
        notify_changed(&self.shared);
    }

    pub fn suspend(&self) {
        self.pause();
        if let Some(src) = &*self.shared.source.lock() {
            src.suspend();
        }
    }

    pub fn resume(&self) {
        if let Some(src) = &*self.shared.source.lock() {
            src.resume();
        }
        self.play();
    }

    pub fn state(&self) -> State {
        let mut st = self.shared.state.lock();
        if let Some(now) = self.shared.clock.now() {
            st.position = now;
        }
        st.clone()
    }

    pub fn wait(&self) -> State {
        let mut st = self.shared.state.lock();
        while !st.ended && st.error.is_none() && !self.shared.stopped.load(Ordering::SeqCst) {
            self.shared.condvar.wait(&mut st);
        }
        if let Some(now) = self.shared.clock.now() {
            st.position = now;
        }
        st.clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        self.shared.condvar.notify_all();
        let mut threads = self.threads.lock();
        for t in threads.drain(..) {
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

fn notify_ended(shared: &Arc<SharedState>) {
    {
        let mut st = shared.state.lock();
        st.ended = true;
        st.playing = false;
    }
    (shared.on_event)(Event::Ended);
    shared.condvar.notify_all();
}

fn notify_error(shared: &Arc<SharedState>, err: String) {
    {
        let mut st = shared.state.lock();
        st.error = Some(err.clone());
        st.playing = false;
    }
    (shared.on_event)(Event::Error(err));
    shared.condvar.notify_all();
}

fn run_player_pipeline(
    url: String,
    backend: Arc<dyn Backend>,
    ctx: Arc<RuntimeContext>,
    options: PlayerOptions,
    shared: Arc<SharedState>,
) {
    // 1. Open Source
    let mut source = match open_source(&url) {
        Ok(s) => s,
        Err(e) => {
            notify_error(&shared, format!("failed to open source: {e}"));
            return;
        }
    };

    // 2. Probe rule
    let mut probe_buf = vec![0u8; 256 * 1024];
    let n = match std::io::Read::read(&mut source, &mut probe_buf) {
        Ok(n) => n,
        Err(e) => {
            notify_error(&shared, format!("failed to read for probe: {e}"));
            return;
        }
    };
    if let Err(e) = std::io::Seek::seek(&mut source, std::io::SeekFrom::Start(0)) {
        notify_error(&shared, format!("failed to rewind source: {e}"));
        return;
    }

    let ext = url.split('?').next().unwrap_or(&url);
    let ext = std::path::Path::new(ext)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);

    let probe_data = ProbeData {
        buf: &probe_buf[..n],
        ext: ext.as_deref(),
    };
    let candidates = ctx.containers.probe_candidates(&probe_data);
    let by_extension = ext
        .as_deref()
        .and_then(|e| ctx.containers.container_for_extension(e));

    let container_format = match (candidates.first(), by_extension) {
        (Some(c), _) if c.score >= PROBE_SCORE_EXTENSION => c.name.to_string(),
        (_, Some(name)) => name.to_string(),
        _ => {
            notify_error(&shared, "no container claims this input".into());
            return;
        }
    };

    // 3. Open Demuxer
    let mut demuxer = match ctx
        .containers
        .open_demuxer(&container_format, Box::new(source), &ctx.codecs)
    {
        Ok(d) => d,
        Err(e) => {
            notify_error(&shared, format!("failed to open demuxer: {e}"));
            return;
        }
    };

    // 4. Inspect Streams (Limit to at most 64 streams)
    let streams_all = demuxer.streams();
    let streams_count = streams_all.len().min(64);
    let streams: Vec<StreamInfo> = streams_all[..streams_count].to_vec();

    let mut tracks = Vec::new();
    let mut first_audio: Option<u32> = None;
    let mut first_video: Option<u32> = None;
    let mut first_subtitle: Option<u32> = None;

    for s in &streams {
        let kind = match s.params.media_type {
            MediaType::Audio => {
                if first_audio.is_none() {
                    first_audio = Some(s.index);
                }
                TrackKind::Audio
            }
            MediaType::Video => {
                // Untrusted input limit: video dimensions <= 16384 and <= 8192*8192
                let w = s.params.width.unwrap_or(0);
                let h = s.params.height.unwrap_or(0);
                if w > 16384 || h > 16384 || (w as u64 * h as u64 > 8192 * 8192) {
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

    let active_audio = options.audio.or(first_audio);
    let active_video = options.video.or(first_video);
    let active_subtitle = options.subtitle.or(first_subtitle);

    *shared.active_audio_stream.lock() = active_audio;
    *shared.active_subtitle_stream.lock() = active_subtitle;

    let duration_micros = demuxer.duration_micros();
    let duration = duration_micros.map(|us| Duration::from_micros(us.max(0) as u64));

    let (video_w, video_h) = if let Some(v_idx) = active_video {
        if let Some(s) = streams.iter().find(|s| s.index == v_idx) {
            (s.params.width.unwrap_or(0), s.params.height.unwrap_or(0))
        } else {
            (0, 0)
        }
    } else {
        (0, 0)
    };

    let video_decoder_name = if let Some(v_idx) = active_video {
        if let Some(s) = streams.iter().find(|s| s.index == v_idx) {
            Some(s.params.codec_id.as_str().to_string())
        } else {
            None
        }
    } else {
        None
    };

    {
        let mut st = shared.state.lock();
        st.tracks = tracks;
        st.audio = active_audio;
        st.video = active_video;
        st.subtitle = active_subtitle;
        st.duration = duration;
        st.video_size = if video_w > 0 && video_h > 0 {
            Some((video_w, video_h))
        } else {
            None
        };
        st.video_decoder = video_decoder_name.clone();
    }
    notify_changed(&shared);

    // 5. Connect to Headless backend if applicable
    let backend_ptr = Arc::as_ptr(&backend) as *const () as usize;
    if let Some(headless) = find_headless(backend_ptr) {
        let v_info = active_video.and_then(|idx| {
            streams
                .iter()
                .find(|s| s.index == idx)
                .map(|s| (idx, s.params.codec_id.as_str().to_string()))
        });
        let a_info = active_audio.and_then(|idx| {
            streams
                .iter()
                .find(|s| s.index == idx)
                .map(|s| (idx, s.params.codec_id.as_str().to_string()))
        });
        let sub_info = active_subtitle.and_then(|idx| {
            streams
                .iter()
                .find(|s| s.index == idx)
                .map(|s| (idx, s.params.codec_id.as_str().to_string()))
        });
        headless.set_active_streams(v_info, a_info, sub_info, options.realtime);
    }

    // 6. Create Sinks
    let mut audio_sink = backend.audio();
    let master_clock = audio_sink.clock();
    let mut video_sink = backend.video(Arc::clone(&master_clock));
    let subtitle_sink = backend.subtitles();

    // 7. Bounded Packet Queues
    // video 32 MiB, audio 8 MiB, subtitles 1 MiB
    let audio_queue = Arc::new(Mutex::new(Vec::<Option<Packet>>::new()));
    let audio_cv = Arc::new(Condvar::new());

    let video_queue = Arc::new(Mutex::new(Vec::<Option<Packet>>::new()));
    let video_cv = Arc::new(Condvar::new());

    let sub_queue = Arc::new(Mutex::new(Vec::<Option<Packet>>::new()));
    let sub_cv = Arc::new(Condvar::new());

    let demux_cv = Arc::new(Condvar::new());

    // 8. Spawn Audio Thread
    let audio_handle = if let Some(audio_idx) = active_audio {
        let a_stream = streams.iter().find(|s| s.index == audio_idx).cloned();
        let a_queue = Arc::clone(&audio_queue);
        let a_cv = Arc::clone(&audio_cv);
        let a_demux_cv = Arc::clone(&demux_cv);
        let a_shared = Arc::clone(&shared);
        let a_ctx = Arc::clone(&ctx);
        let a_realtime = options.realtime;

        Some(
            std::thread::Builder::new()
                .name("peartube-audio".into())
                .spawn(move || {
                    let Some(stream_info) = a_stream else { return };
                    run_audio_thread(
                        stream_info,
                        &mut *audio_sink,
                        a_queue,
                        a_cv,
                        a_demux_cv,
                        a_shared,
                        a_ctx,
                        a_realtime,
                    );
                })
                .expect("failed to spawn audio thread"),
        )
    } else {
        None
    };

    // 9. Spawn Video Thread
    let video_handle = if let Some(video_idx) = active_video {
        let v_stream = streams.iter().find(|s| s.index == video_idx).cloned();
        let v_queue = Arc::clone(&video_queue);
        let v_cv = Arc::clone(&video_cv);
        let v_demux_cv = Arc::clone(&demux_cv);
        let v_shared = Arc::clone(&shared);
        let v_ctx = Arc::clone(&ctx);
        let v_clock = Arc::clone(&master_clock);
        let v_realtime = options.realtime;

        Some(
            std::thread::Builder::new()
                .name("peartube-video".into())
                .spawn(move || {
                    let Some(stream_info) = v_stream else { return };
                    run_video_thread(
                        stream_info,
                        &mut *video_sink,
                        v_clock,
                        v_queue,
                        v_cv,
                        v_demux_cv,
                        v_shared,
                        v_ctx,
                        v_realtime,
                    );
                })
                .expect("failed to spawn video thread"),
        )
    } else {
        None
    };

    // 10. Spawn Subtitle Thread
    let sub_handle = if let Some(sub_idx) = active_subtitle {
        let s_stream = streams.iter().find(|s| s.index == sub_idx).cloned();
        let s_queue = Arc::clone(&sub_queue);
        let s_cv = Arc::clone(&sub_cv);
        let s_shared = Arc::clone(&shared);
        let s_ctx = Arc::clone(&ctx);
        let s_clock = Arc::clone(&master_clock);
        let s_realtime = options.realtime;
        let s_stopped = Arc::clone(&s_shared.stopped);

        Some(
            std::thread::Builder::new()
                .name("peartube-subtitles".into())
                .spawn(move || {
                    let Some(stream_info) = s_stream else { return };
                    let decoder = match s_ctx.codecs.first_decoder(&stream_info.params) {
                        Ok(d) => d,
                        Err(_) => return,
                    };
                    run_subtitle_loop(
                        decoder,
                        subtitle_sink,
                        s_clock,
                        stream_info.time_base,
                        video_w,
                        video_h,
                        s_realtime,
                        s_queue,
                        s_cv,
                        s_stopped,
                    );
                })
                .expect("failed to spawn subtitle thread"),
        )
    } else {
        None
    };

    // 11. Demux Loop (runs in this thread)
    run_demux_loop(
        &mut *demuxer,
        active_video,
        active_audio,
        active_subtitle,
        video_queue,
        audio_queue,
        sub_queue,
        video_cv,
        audio_cv,
        sub_cv,
        demux_cv,
        shared.clone(),
    );

    // Join decoding threads
    if let Some(h) = video_handle {
        let _ = h.join();
    }
    if let Some(h) = audio_handle {
        let _ = h.join();
    }
    if let Some(h) = sub_handle {
        let _ = h.join();
    }

    if !shared.stopped.load(Ordering::SeqCst) && shared.state.lock().error.is_none() {
        notify_ended(&shared);
    }
}

fn run_demux_loop(
    demuxer: &mut dyn Demuxer,
    video_idx: Option<u32>,
    audio_idx: Option<u32>,
    sub_idx: Option<u32>,
    video_queue: Arc<Mutex<Vec<Option<Packet>>>>,
    audio_queue: Arc<Mutex<Vec<Option<Packet>>>>,
    sub_queue: Arc<Mutex<Vec<Option<Packet>>>>,
    video_cv: Arc<Condvar>,
    audio_cv: Arc<Condvar>,
    sub_cv: Arc<Condvar>,
    demux_cv: Arc<Condvar>,
    shared: Arc<SharedState>,
) {
    let mut active = Vec::new();
    if let Some(i) = video_idx {
        active.push(i);
    }
    if let Some(i) = audio_idx {
        active.push(i);
    }
    if let Some(i) = sub_idx {
        active.push(i);
    }
    demuxer.set_active_streams(&active);

    const VIDEO_MAX_BYTES: usize = 32 * 1024 * 1024;
    const AUDIO_MAX_BYTES: usize = 8 * 1024 * 1024;
    const SUB_MAX_BYTES: usize = 1024 * 1024;

    while !shared.stopped.load(Ordering::SeqCst) {
        // Handle Seek
        let seek_req = shared.seek_target.lock().take();
        if let Some(target) = seek_req {
            // Flush queues
            {
                let mut vq = video_queue.lock();
                vq.clear();
            }
            {
                let mut aq = audio_queue.lock();
                aq.clear();
            }
            {
                let mut sq = sub_queue.lock();
                sq.clear();
            }
            video_cv.notify_all();
            audio_cv.notify_all();
            sub_cv.notify_all();

            // Demuxer seek
            let stream_for_seek = video_idx.or(audio_idx).unwrap_or(0);
            let pts_ticks = (target.as_secs_f64() * 1000.0) as i64;
            let _ = demuxer.seek_to(stream_for_seek, pts_ticks);
        }

        // Bounded queue wait
        {
            let v_bytes: usize = video_queue
                .lock()
                .iter()
                .flatten()
                .map(|p| p.data.len())
                .sum();
            let a_bytes: usize = audio_queue
                .lock()
                .iter()
                .flatten()
                .map(|p| p.data.len())
                .sum();
            let s_bytes: usize = sub_queue
                .lock()
                .iter()
                .flatten()
                .map(|p| p.data.len())
                .sum();

            if v_bytes >= VIDEO_MAX_BYTES || a_bytes >= AUDIO_MAX_BYTES || s_bytes >= SUB_MAX_BYTES
            {
                let mut dummy = false;
                let dummy_lock = Mutex::new(&mut dummy);
                let mut guard = dummy_lock.lock();
                demux_cv.wait_for(&mut guard, Duration::from_millis(50));
                continue;
            }
        }

        // Next packet under catch_unwind
        let packet_res = std::panic::catch_unwind(AssertUnwindSafe(|| demuxer.next_packet()));

        match packet_res {
            Ok(Ok(packet)) => {
                let stream_id = packet.stream_index;
                if Some(stream_id) == video_idx {
                    video_queue.lock().push(Some(packet));
                    video_cv.notify_one();
                } else if Some(stream_id) == audio_idx {
                    audio_queue.lock().push(Some(packet));
                    audio_cv.notify_one();
                } else if Some(stream_id) == sub_idx {
                    sub_queue.lock().push(Some(packet));
                    sub_cv.notify_one();
                }
                // Other streams dropped
            }
            Ok(Err(oxideav_core::Error::Eof)) => {
                // EOF on input
                video_queue.lock().push(None);
                audio_queue.lock().push(None);
                sub_queue.lock().push(None);
                video_cv.notify_all();
                audio_cv.notify_all();
                sub_cv.notify_all();
                break;
            }
            Ok(Err(e)) => {
                // Demux error
                video_queue.lock().push(None);
                audio_queue.lock().push(None);
                sub_queue.lock().push(None);
                video_cv.notify_all();
                audio_cv.notify_all();
                sub_cv.notify_all();
                notify_error(&shared, format!("demux error: {e}"));
                break;
            }
            Err(_) => {
                // Demux panic
                video_queue.lock().push(None);
                audio_queue.lock().push(None);
                sub_queue.lock().push(None);
                video_cv.notify_all();
                audio_cv.notify_all();
                sub_cv.notify_all();
                notify_error(&shared, "demuxer panicked".into());
                break;
            }
        }
    }
}

fn run_audio_thread(
    stream: StreamInfo,
    sink: &mut dyn AudioSink,
    queue: Arc<Mutex<Vec<Option<Packet>>>>,
    cv: Arc<Condvar>,
    demux_cv: Arc<Condvar>,
    shared: Arc<SharedState>,
    ctx: Arc<RuntimeContext>,
    _realtime: bool,
) {
    let mut decoder = match ctx.codecs.first_decoder(&stream.params) {
        Ok(d) => d,
        Err(e) => {
            notify_error(&shared, format!("no audio decoder found: {e}"));
            return;
        }
    };

    let mut current_rate = stream.params.sample_rate.unwrap_or(48000);
    let mut current_channels = stream.params.channels.unwrap_or(2);
    let _ = sink.open(current_rate, current_channels);

    let mut consecutive_errors = 0;
    let mut pending_seek_target: Option<Duration> = None;

    while !shared.stopped.load(Ordering::SeqCst) {
        let packet = {
            let mut q = queue.lock();
            while q.is_empty() && !shared.stopped.load(Ordering::SeqCst) {
                cv.wait(&mut q);
            }
            if shared.stopped.load(Ordering::SeqCst) {
                break;
            }
            if q.is_empty() {
                continue;
            }
            q.remove(0)
        };
        demux_cv.notify_one();

        let Some(packet) = packet else {
            // EOF
            break;
        };

        if let Some(target) = *shared.seek_target.lock() {
            pending_seek_target = Some(target);
            sink.flush();
            let _ = decoder.reset();
        }

        let send_res =
            std::panic::catch_unwind(AssertUnwindSafe(|| decoder.send_packet(&packet)));
        match send_res {
            Ok(Ok(())) => {
                consecutive_errors = 0;
            }
            Ok(Err(_)) | Err(_) => {
                consecutive_errors += 1;
                if consecutive_errors >= 3 {
                    // Disable stream
                    let mut st = shared.state.lock();
                    st.error = Some("audio decoder failed 3 times".into());
                    st.audio = None;
                    break;
                }
                continue;
            }
        }

        loop {
            if shared.stopped.load(Ordering::SeqCst) {
                break;
            }
            let recv_res =
                std::panic::catch_unwind(AssertUnwindSafe(|| decoder.receive_frame()));

            let frame = match recv_res {
                Ok(Ok(f)) => {
                    consecutive_errors = 0;
                    f
                }
                Ok(Err(oxideav_core::Error::NeedMore)) => break,
                Ok(Err(oxideav_core::Error::Eof)) => break,
                Ok(Err(_)) | Err(_) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= 3 {
                        let mut st = shared.state.lock();
                        st.error = Some("audio decoder failed 3 times".into());
                        st.audio = None;
                        return;
                    }
                    break;
                }
            };

            let Frame::Audio(af) = frame else { continue };

            let format = stream
                .params
                .sample_format
                .unwrap_or(SampleFormat::F32);
            let channels = stream.params.channels.unwrap_or(1) as usize;
            let sample_rate = stream.params.sample_rate.unwrap_or(48000);

            if sample_rate != current_rate || (channels as u16) != current_channels {
                current_rate = sample_rate;
                current_channels = channels as u16;
                let _ = sink.open(current_rate, current_channels);
            }

            let pcm = convert_audio_to_f32(&af, format, channels);
            let ticks = af.pts.or(packet.pts).unwrap_or(0).max(0);
            let secs = stream.time_base.seconds_of(ticks);
            let mut pts = Duration::from_secs_f64(secs.max(0.0));

            // If seek was performed, ensure first audio pts is within one frame of seek target
            if let Some(target) = pending_seek_target.take() {
                if pts < target {
                    let frame_dur = Duration::from_secs_f64(af.samples as f64 / sample_rate as f64);
                    if target - pts < frame_dur {
                        pts = target;
                    }
                }
            }

            let _ = sink.write(&pcm, pts);
        }
    }
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
        SampleFormat::S16 | SampleFormat::S16P => {
            i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0
        }
        SampleFormat::S24 => {
            (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8388608.0
        }
        SampleFormat::S32 | SampleFormat::S32P => {
            i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2147483648.0
        }
        SampleFormat::F32 | SampleFormat::F32P => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        SampleFormat::F64 | SampleFormat::F64P => {
            f64::from_le_bytes(b.try_into().unwrap_or([0; 8])) as f32
        }
        // Non-exhaustive upstream: treat anything new as silence rather
        // than panicking on untrusted input.
        _ => 0.0,
    }
}

fn run_video_thread(
    stream: StreamInfo,
    sink: &mut dyn VideoSink,
    clock: Arc<dyn Clock>,
    queue: Arc<Mutex<Vec<Option<Packet>>>>,
    cv: Arc<Condvar>,
    demux_cv: Arc<Condvar>,
    shared: Arc<SharedState>,
    ctx: Arc<RuntimeContext>,
    realtime: bool,
) {
    let mut compressed = sink.open_compressed(&stream.params);
    let mut sw_decoder: Option<Box<dyn Decoder>> = None;
    let mut need_keyframe = false;
    let mut consecutive_errors = 0;
    let mut pending_seek_target: Option<Duration> = None;

    if !compressed {
        match ctx.codecs.first_decoder(&stream.params) {
            Ok(d) => {
                let _ = sink.open_frames(&stream.params);
                sw_decoder = Some(d);
            }
            Err(e) => {
                notify_error(&shared, format!("no video decoder found: {e}"));
                return;
            }
        }
    }

    while !shared.stopped.load(Ordering::SeqCst) {
        let packet = {
            let mut q = queue.lock();
            while q.is_empty() && !shared.stopped.load(Ordering::SeqCst) {
                cv.wait(&mut q);
            }
            if shared.stopped.load(Ordering::SeqCst) {
                break;
            }
            if q.is_empty() {
                continue;
            }
            q.remove(0)
        };
        demux_cv.notify_one();

        let Some(packet) = packet else {
            // EOF
            break;
        };

        if let Some(target) = *shared.seek_target.lock() {
            pending_seek_target = Some(target);
            sink.flush();
            if let Some(dec) = sw_decoder.as_mut() {
                let _ = dec.reset();
            }
            need_keyframe = true;
        }

        if need_keyframe && !packet.flags.keyframe {
            continue;
        }
        need_keyframe = false;

        let ticks = packet.pts.unwrap_or(0).max(0);
        let secs = stream.time_base.seconds_of(ticks);
        let pts = Duration::from_secs_f64(secs.max(0.0));

        if compressed {
            match sink.push_packet(&packet, pts) {
                Ok(()) => {
                    consecutive_errors = 0;
                }
                Err(SinkError::Fallback(_)) => {
                    // Fall back to software decoder
                    compressed = false;
                    need_keyframe = true;
                    match ctx.codecs.first_decoder(&stream.params) {
                        Ok(d) => {
                            let _ = sink.open_frames(&stream.params);
                            sw_decoder = Some(d);
                        }
                        Err(e) => {
                            notify_error(&shared, format!("video fallback failed: {e}"));
                            break;
                        }
                    }
                    continue;
                }
                Err(SinkError::Fatal(f)) => {
                    notify_error(&shared, format!("video fatal error: {f}"));
                    break;
                }
                Err(_) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= 3 {
                        let mut st = shared.state.lock();
                        st.error = Some("video sink failed 3 times".into());
                        st.video = None;
                        break;
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
                        st.error = Some("video decoder failed 3 times".into());
                        st.video = None;
                        break;
                    }
                    continue;
                }
            }

            loop {
                if shared.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let recv_res =
                    std::panic::catch_unwind(AssertUnwindSafe(|| decoder.receive_frame()));

                let frame = match recv_res {
                    Ok(Ok(f)) => {
                        consecutive_errors = 0;
                        f
                    }
                    Ok(Err(oxideav_core::Error::NeedMore)) => break,
                    Ok(Err(oxideav_core::Error::Eof)) => break,
                    Ok(Err(_)) | Err(_) => {
                        consecutive_errors += 1;
                        if consecutive_errors >= 3 {
                            let mut st = shared.state.lock();
                            st.error = Some("video decoder failed 3 times".into());
                            st.video = None;
                            return;
                        }
                        break;
                    }
                };

                let Frame::Video(vf) = frame else { continue };

                let frame_ticks = vf.pts.or(packet.pts).unwrap_or(0).max(0);
                let frame_secs = stream.time_base.seconds_of(frame_ticks);
                let frame_pts = Duration::from_secs_f64(frame_secs.max(0.0));

                // If seeking, discard frames before seek target so first displayed frame >= seek target
                if let Some(target) = pending_seek_target {
                    if frame_pts < target {
                        continue;
                    } else {
                        pending_seek_target = None;
                    }
                }

                if !realtime {
                    let _ = sink.push_frame(&vf, frame_pts);
                } else {
                    // Realtime pacing
                    // push_frame up to 100 ms before pts on the clock; drop frames more than 100 ms late
                    let mut dropped = false;
                    while !shared.stopped.load(Ordering::SeqCst) {
                        if let Some(now) = clock.now() {
                            if now > frame_pts + Duration::from_millis(100) {
                                // Frame is more than 100 ms late: drop it!
                                shared.state.lock().dropped_frames += 1;
                                dropped = true;
                                break;
                            }
                            if frame_pts <= now + Duration::from_millis(100) {
                                // Within 100 ms before pts: ready to push!
                                break;
                            }
                            // Sleep until 100 ms before pts
                            let lead = frame_pts - now;
                            let sleep_dur = (lead - Duration::from_millis(100)).min(Duration::from_millis(10));
                            std::thread::sleep(sleep_dur);
                        } else {
                            // Clock not started yet; push initial frame
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
