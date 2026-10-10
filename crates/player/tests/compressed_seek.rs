//! A bounded, asynchronous platform-decoder model. It emits one picture
//! per H.264 access unit, reorders by PTS, and can hold decoding at a gate.
//! Pixel accuracy is covered by seek_entry/intra_refresh; these tests check
//! the engine's compressed-output contract against real packet streams.
#[path = "support/seek_preroll.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "support/scripted.rs"]
mod scripted;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use oxideav_core::{Packet, VideoFrame};
use parking_lot::{Condvar, Mutex};
use std::task::Poll;
use player::backend::{
    AudioSink, Backend, Clock, PictureReady, ProducerId, SinkError, SubtitleSink,
    VideoControl, VideoError, VideoMode, VideoOutput, VideoRequest, VideoSink, VideoTarget,
};
use player::{Event, Headless, Player, PlayerOptions};

#[derive(Default)]
struct Observed {
    started: bool,
    shown: Vec<(u64, Duration, Duration)>,
    hidden: Vec<(u64, Duration)>,
    ready: Vec<(u64, Duration, Duration)>,
    blocked: bool,
    release: bool,
}

struct Platform {
    audio: Arc<Headless>,
    observed: Arc<(Mutex<Observed>, Condvar)>,
    paced: bool,
    gate: bool,
    reorder_depth: usize,
}

impl Platform {
    fn new(paced: bool, gate: bool, reorder_depth: usize) -> Arc<Self> {
        Arc::new(Self { audio: Headless::new(), observed: Arc::new((Mutex::new(Observed::default()), Condvar::new())),
            paced, gate, reorder_depth })
    }
}

impl Backend for Platform {
    fn audio(&self) -> Box<dyn AudioSink> { self.audio.audio() }
    fn subtitles(&self) -> Box<dyn SubtitleSink> { self.audio.subtitles() }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        Box::new(PlatformVideo {
            clock,
            observed: self.observed.clone(),
            paced: self.paced,
            gate: self.gate,
            reorder_depth: self.reorder_depth,
            ready: None,
            epoch: 0,
            producer: None,
            state: Arc::new((Mutex::new(DecodeState::default()), Condvar::new())),
            worker: None,
        })
    }
}

#[derive(Default)]
struct DecodeState {
    input: VecDeque<Duration>,
    from: Duration,
    playing: bool,
    stop: bool,
    eof: bool,
    done: bool,
}

struct PlatformVideo {
    clock: Arc<dyn Clock>,
    observed: Arc<(Mutex<Observed>, Condvar)>,
    state: Arc<(Mutex<DecodeState>, Condvar)>,
    ready: Option<PictureReady>,
    worker: Option<JoinHandle<()>>,
    epoch: u64,
    producer: Option<ProducerId>,
    paced: bool,
    gate: bool,
    reorder_depth: usize,
}
impl PlatformVideo {
    fn start(&mut self) {
        let state = self.state.clone();
        let observed = self.observed.clone();
        let ready = self.ready.clone().unwrap();
        let clock = self.clock.clone();
        let (epoch, paced, gate) = (self.epoch, self.paced, self.gate);
        let reorder_depth = self.reorder_depth;
        self.worker = Some(std::thread::spawn(move || {
            let mut reorder = Vec::new();
            loop {
                let mut s = state.0.lock();
                while !s.stop && s.input.is_empty() && !s.eof {
                    state.1.wait(&mut s);
                }
                if s.stop { return; }
                if let Some(input) = s.input.pop_front() { reorder.push(input); }
                let draining = s.eof && s.input.is_empty();
                if reorder.len() <= reorder_depth && !draining { continue; }
                if reorder.is_empty() {
                    s.done = true;
                    state.1.notify_all();
                    return;
                }
                let first = reorder.iter().enumerate().min_by_key(|(_, pts)| **pts).unwrap().0;
                let pts = reorder.remove(first);
                let hidden = pts < s.from;
                drop(s);
                if hidden {
                    observed.0.lock().hidden.push((epoch, pts));
                    continue;
                }
                if gate && epoch > 0 {
                    let mut seen = observed.0.lock();
                    seen.blocked = true;
                    observed.1.notify_all();
                    while !seen.release && !state.0.lock().stop {
                        observed.1.wait_for(&mut seen, Duration::from_millis(5));
                    }
                }
                if state.0.lock().stop { return; }
                observed.0.lock().ready.push((epoch, pts, clock.now().unwrap_or_default()));
                ready.ready(pts);
                loop {
                    let mut s = state.0.lock();
                    if s.stop { return; }
                    let now = clock.now().unwrap_or_default();
                    if !paced || (s.playing && now >= pts) {
                        observed.0.lock().shown.push((epoch, pts, now));
                        break;
                    }
                    state.1.wait_for(&mut s, Duration::from_millis(1));
                }
            }
        }));
    }

    fn stop(&mut self) {
        self.state.0.lock().stop = true;
        self.state.1.notify_all();
        self.observed.1.notify_all();
        if let Some(worker) = self.worker.take() { worker.join().unwrap(); }
    }
}

impl Drop for PlatformVideo {
    fn drop(&mut self) { self.stop(); }
}

impl VideoSink for PlatformVideo {
    fn output(&self) -> VideoOutput {
        VideoOutput {
            revision: 1,
            available: true,
        }
    }

    fn poll_transition(&mut self, request: &VideoRequest) -> Poll<Result<VideoMode, VideoError>> {
        match &request.target {
            VideoTarget::Compressed { params, ready, present_from } => {
                assert_eq!(params.codec_id.as_str(), "h264");
                let new_producer = self.producer != Some(request.producer);
                let new_seek = self.epoch != request.seek_generation;
                if new_producer || new_seek {
                    self.stop();
                    self.producer = Some(request.producer);
                    self.epoch = request.seek_generation;
                    let playing = self.state.0.lock().playing;
                    self.state = Arc::new((
                        Mutex::new(DecodeState {
                            playing,
                            from: *present_from,
                            ..DecodeState::default()
                        }),
                        Condvar::new(),
                    ));
                    self.ready = Some(ready.clone());
                    self.start();
                }
                Poll::Ready(Ok(VideoMode::Compressed))
            }
            VideoTarget::Frames { .. } => Poll::Ready(Err(VideoError::Unsupported)),
            VideoTarget::Retired => {
                self.stop();
                Poll::Ready(Ok(VideoMode::Retired))
            }
        }
    }

    fn present_from(&mut self, _producer: ProducerId, start: Duration) -> Result<(), VideoError> {
        self.state.0.lock().from = start;
        Ok(())
    }

    fn push_packet(
        &mut self,
        _producer: ProducerId,
        packet: &mut Option<Packet>,
        pts: Duration,
        _random_access: bool,
    ) -> Result<(), VideoError> {
        let mut s = self.state.0.lock();
        if s.input.len() == 8 {
            return Err(VideoError::Sink(SinkError::WouldBlock));
        }
        let _ = packet.take();
        s.input.push_back(pts);
        self.state.1.notify_all();
        Ok(())
    }

    fn push_frame(
        &mut self,
        _producer: ProducerId,
        _frame: &mut Option<VideoFrame>,
        _pts: Duration,
    ) -> Result<(), VideoError> {
        panic!("unexpected software frame")
    }

    fn frame_lead(&self) -> Duration {
        Duration::ZERO
    }

    fn poll_finish(&mut self, _producer: ProducerId) -> Poll<Result<(), VideoError>> {
        let mut s = self.state.0.lock();
        s.eof = true;
        self.state.1.notify_all();
        if s.done {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn set_playing(&mut self, _producer: ProducerId, playing: bool) -> Result<(), VideoError> {
        self.state.0.lock().playing = playing;
        self.state.1.notify_all();
        self.observed.0.lock().started = true;
        self.observed.1.notify_all();
        Ok(())
    }
}

fn open(path: &Path, backend: Arc<Platform>) -> (Player, std::sync::mpsc::Receiver<Event>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let options = PlayerOptions { realtime: backend.paced, ..PlayerOptions::default() };
    let player = Player::open(path.to_str().unwrap(), backend, Arc::new(codecs::context()), options,
        move |event| { let _ = tx.send(event); });
    (player, rx)
}

// The staged demux helper has one process-wide gate. Keep it released even
// when an assertion fails, before Player's drop joins the demux thread.
static STARTUP_GATE: Mutex<()> = Mutex::new(());

struct GatedStartup {
    player: Player,
    events: std::sync::mpsc::Receiver<Event>,
    backend: Arc<Platform>,
    expected_audio: Vec<f32>,
    opened: Instant,
    _serial: parking_lot::MutexGuard<'static, ()>,
}

impl GatedStartup {
    fn new(realtime: bool) -> Self {
        let serial = STARTUP_GATE.lock();
        let clip = fixture::tmp("compressed_paused_startup.mkv");
        fixture::encode(&["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=8:duration=1",
            "-f", "lavfi", "-i", "sine=sample_rate=8000:duration=1",
            "-c:v", "libx264", "-preset", "ultrafast", "-x264-params", "keyint=8:bframes=0",
            "-c:a", "pcm_s16le", "-f", "matroska"], &clip);
        let expected_audio = refcheck::ffmpeg_audio_f32(&clip, 0);
        let path = fixture::tmp("compressed_paused_startup.ptscript");
        scripted::write_gated(&path, &clip, scripted::Mode::Seekable);
        scripted::arm(&[scripted::Hold::FirstPacket]);
        let backend = Platform::new(realtime, false, 4);
        // Player sees the wrapper, so configure Headless's device mode here.
        backend.audio.set_active_streams(None, None, None, realtime);
        let (tx, events) = std::sync::mpsc::channel();
        let opened = Instant::now();
        let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(scripted::context()),
            PlayerOptions { realtime, ..PlayerOptions::default() }, move |event| { let _ = tx.send(event); });
        let startup = Self { player, events, backend, expected_audio, opened, _serial: serial };
        assert!(scripted::entered(scripted::Hold::FirstPacket, Duration::from_secs(3)), "demux did not reach gate");
        {
            let mut seen = startup.backend.observed.0.lock();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !seen.started {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "video worker did not start");
                startup.backend.observed.1.wait_for(&mut seen, remaining);
            }
        }
        startup
    }
}

impl Drop for GatedStartup {
    fn drop(&mut self) { scripted::arm(&[]); }
}

fn resume_after_startup_pause(realtime: bool) {
    let startup = GatedStartup::new(realtime);
    startup.player.pause();
    let paused_at = Instant::now();
    std::thread::sleep(Duration::from_millis(5500));
    let held = startup.player.state();
    assert!(held.error.is_none(), "timeout during user pause: {held:?}");
    assert!(!held.playing && held.buffering, "{held:?}");
    assert_eq!(held.position, Duration::ZERO);
    assert!(startup.backend.observed.0.lock().ready.is_empty(), "readiness without decoder input");
    // Resume without seeking: neither the decoder generation nor its
    // already-spent active timeout allowance should be reset.
    startup.player.play();
    scripted::release();
    ended(&startup.player, &startup.events, Duration::from_secs(8));
    {
        let seen = startup.backend.observed.0.lock();
        let pts: Vec<_> = seen.shown.iter().map(|s| s.1).collect();
        assert_eq!(pts, (0..8).map(|n| Duration::from_millis(n * 125)).collect::<Vec<_>>());
        assert_eq!(seen.ready[0].2, Duration::ZERO, "clock ran before decoded output");
    }
    if realtime {
        audio_drained(&startup.backend.audio, &startup.expected_audio);
    } else {
        // Unpaced Headless has no device playback runs: every write is final.
        assert_eq!(startup.backend.audio.capture().audio[0].pcm, startup.expected_audio,
            "unpaced output lost or changed audio");
    }
    eprintln!("startup pause: realtime={realtime}, resumed without seeking after at least 5.5s; ended in {:?}", paused_at.elapsed());
}

#[test]
fn user_pause_before_first_packet_resumes_nonrealtime() {
    resume_after_startup_pause(false);
}

#[test]
fn user_pause_before_first_packet_resumes_realtime() {
    resume_after_startup_pause(true);
}

#[test]
fn user_pause_preserves_spent_input_wait_budget() {
    let startup = GatedStartup::new(true);
    // Active starvation counts even though the buffering hold stops media.
    std::thread::sleep(Duration::from_secs(2));
    let paused_at = Instant::now();
    startup.player.pause();
    std::thread::sleep(Duration::from_millis(2750));
    startup.player.pause(); // Duplicate intent must not lose pause credit.
    std::thread::sleep(Duration::from_millis(2750));
    let held = startup.player.state();
    assert!(held.error.is_none(), "timeout during user pause: {held:?}");
    assert!(!held.playing && held.buffering, "{held:?}");
    let resumed_at = Instant::now();
    startup.player.play();
    startup.player.play(); // Duplicate intent must not grant extra credit.
    // Keep input blocked: roughly three seconds remain, not a new five.
    let watchdog = resumed_at + Duration::from_secs(4);
    let error = loop {
        match startup.events.recv_timeout(watchdog.saturating_duration_since(Instant::now())) {
            Ok(Event::Error(error)) => break error,
            Ok(Event::Ended) => panic!("stalled input reached a successful end"),
            Ok(Event::Changed) => {}
            Err(error) => panic!("active input deadline was reset or disabled: {error}: {:?}", startup.player.state()),
        }
    };
    let active = startup.opened.elapsed().saturating_sub(resumed_at - paused_at);
    assert!(active >= Duration::from_secs(5), "early deadline after {active:?}: {error}");
    assert_eq!(startup.player.state().position, Duration::ZERO);
    assert!(startup.backend.observed.0.lock().ready.is_empty());
    eprintln!("input deadline retained: active={active:?}, after resume={:?}: {error}", resumed_at.elapsed());
}

fn ended(player: &Player, rx: &std::sync::mpsc::Receiver<Event>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Ended) => return,
            Ok(Event::Error(error)) => panic!("{error}"),
            Ok(Event::Changed) => {}
            Err(error) => panic!("{error}: {:?}", player.state()),
        }
    }
}

#[test]
fn clock_waits_for_decoded_target_not_for_compressed_input() {
    let path = fixture::tmp("compressed_seek_audio.mkv");
    fixture::encode(&["-f", "lavfi", "-i", "testsrc2=size=160x96:rate=25:duration=8",
        "-f", "lavfi", "-i", "sine=sample_rate=48000:duration=8", "-c:v", "libx264", "-preset", "medium",
        "-x264-params", "keyint=50:min-keyint=50:scenecut=0:open-gop=1:ref=3:bframes=2",
        "-bsf:v", "filter_units=remove_types=6", "-c:a", "pcm_s16le", "-f", "matroska"], &path);
    let backend = Platform::new(true, true, 4);
    let (player, rx) = open(&path, backend.clone());
    let deadline = Instant::now() + Duration::from_secs(15);
    while player.state().position < Duration::from_millis(400) {
        assert!(Instant::now() < deadline, "startup: {:?}", player.state());
        assert!(player.state().error.is_none(), "{:?}", player.state());
        std::thread::sleep(Duration::from_millis(5));
    }
    player.pause();
    player.seek(Duration::from_secs(4));
    player.play();
    {
        let mut seen = backend.observed.0.lock();
        while !seen.blocked {
            assert!(Instant::now() < deadline, "decoder did not reach target: {:?}", player.state());
            backend.observed.1.wait_for(&mut seen, Duration::from_millis(10));
        }
    }
    // Input already reached the target, but decoding has not completed.
    // Audio must remain on the seek target throughout this controlled wait.
    std::thread::sleep(Duration::from_millis(250));
    let held = player.state();
    assert!(held.buffering, "clock released before decoder output: {held:?}");
    assert_eq!(held.position, Duration::from_secs(4));
    backend.observed.0.lock().release = true;
    backend.observed.1.notify_all();
    ended(&player, &rx, Duration::from_secs(30));
    drop(player);
    let seen = backend.observed.0.lock();
    let epoch = seen.shown.last().unwrap().0;
    let first = seen.shown.iter().find(|s| s.0 == epoch).unwrap();
    assert_eq!(first.1, Duration::from_secs(4));
    assert!(first.2.abs_diff(first.1) <= Duration::from_millis(40), "video trailed audio: {first:?}");
    assert!(seen.hidden.iter().any(|&(g, pts)| g == epoch && pts < Duration::from_secs(1)), "seek did not decode earlier input");
    assert!(seen.shown.iter().filter(|s| s.0 == epoch).all(|s| s.1 >= Duration::from_secs(4)));
    eprintln!("compressed seek: held at {:?}; first pts {:?}, clock {:?}", held.position, first.1, first.2);
}

#[test]
fn compressed_start_and_seek_hide_partial_recovery_pictures() {
    let (_, cut) = fixture::intra_refresh("compressed_intra_refresh");
    let expected = fixture::reference_pts(&cut);
    let (stream, packets) = fixture::video_stream(&cut);
    let start = Duration::from_secs_f64(stream.time_base.seconds_of(packets[0].pts.unwrap()));
    assert!(expected[0] > start + Duration::from_millis(500));
    // Both native sinks filter decoded output. Include a seek with no
    // earlier recovery point available, as well as playback from the start.
    for seek in [None, Some(start + Duration::from_millis(120))] {
        let backend = Platform::new(false, false, 4);
        let (player, rx) = open(&cut, backend.clone());
        if let Some(target) = seek {
            player.pause();
            player.seek(target);
            player.play();
        }
        ended(&player, &rx, Duration::from_secs(30));
        drop(player);
        let seen = backend.observed.0.lock();
        let epoch = seen.shown.last().unwrap().0;
        let shown: Vec<_> = seen.shown.iter().filter(|s| s.0 == epoch).map(|s| s.1).collect();
        assert_eq!(shown.len(), expected.len(), "seek={seek:?}");
        for (ours, theirs) in shown.iter().zip(&expected) {
            assert!(ours.abs_diff(*theirs) < Duration::from_micros(2), "{ours:?} != {theirs:?}");
        }
        assert!(seen.hidden.iter().any(|&(g, pts)| g == epoch && pts < expected[0]));
        assert!(seen.ready.iter().filter(|r| r.0 == epoch).all(|r| r.1 >= expected[0]), "partial picture released the clock");
        eprintln!("compressed recovery: seek={seek:?}, {} frames from {:?}", shown.len(), shown[0]);
    }
}

fn audio_drained(backend: &Headless, expected: &[f32]) {
    let capture = backend.capture();
    let audio = &capture.audio[0];
    assert_eq!(audio.pcm, expected, "queued audio was lost or changed");
    let mut heard = 0;
    for &(from, to, _) in &audio.played {
        assert_eq!(from, heard, "audio playback skipped or repeated samples");
        heard = to;
    }
    // The engine's audio-end tolerance is 5 ms, including timestamp rounding.
    let slack = (audio.sample_rate as usize / 200 + 1) * usize::from(audio.channels);
    assert!(heard + slack >= expected.len(), "audio ended early: {heard}/{}", expected.len());
    eprintln!("audio: {} exact samples, {heard} heard", expected.len());
}

#[test]
fn priming_low_fps_retains_audio_while_feeding_reordered_video() {
    let path = fixture::tmp("compressed_low_fps.mkv");
    fixture::encode(&["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=1:duration=18",
        "-f", "lavfi", "-i", "sine=sample_rate=8000:duration=18",
        "-c:v", "libx264", "-preset", "ultrafast", "-x264-params", "keyint=30:bframes=0",
        "-c:a", "pcm_s16le", "-f", "matroska"], &path);
    let expected = refcheck::ffmpeg_audio_f32(&path, 0);
    let backend = Platform::new(true, false, 16);
    backend.audio.set_audio_speed(4.0);
    let (player, rx) = open(&path, backend.clone());
    ended(&player, &rx, Duration::from_secs(12));
    drop(player);
    let seen = backend.observed.0.lock();
    let pts: Vec<_> = seen.shown.iter().map(|s| s.1).collect();
    assert_eq!(pts, (0..18).map(Duration::from_secs).collect::<Vec<_>>());
    assert_eq!(seen.ready[0].2, Duration::ZERO, "audio ran before decoded output");
    drop(seen);
    audio_drained(&backend.audio, &expected);
}

#[test]
fn priming_byte_limit_still_reaches_the_input_wait_deadline() {
    // Finite 9.2 MB PCM exceeds the 8 MiB audio lane bound before the
    // decoder's seventeenth picture. The empty video lane must time out.
    let path = fixture::tmp("compressed_priming_byte_limit.mkv");
    fixture::encode(&["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=1/4:duration=24",
        "-f", "lavfi", "-i", "sine=sample_rate=192000:duration=24",
        "-c:v", "libx264", "-preset", "ultrafast", "-x264-params", "keyint=30:bframes=0",
        "-c:a", "pcm_s16le", "-f", "matroska"], &path);
    let backend = Platform::new(true, false, 16);
    let (player, rx) = open(&path, backend.clone());
    let start = Instant::now();
    let watchdog = start + Duration::from_secs(8);
    let error = loop {
        match rx.recv_timeout(watchdog.saturating_duration_since(Instant::now())) {
            Ok(Event::Error(error)) => break error,
            Ok(Event::Ended) => panic!("unprimed decoder reached a successful end"),
            Ok(Event::Changed) => {}
            Err(error) => panic!("input wait missed its deadline: {error}: {:?}", player.state()),
        }
    };
    assert!(start.elapsed() >= Duration::from_secs(5), "unexpected early error: {error}");
    assert_eq!(player.state().position, Duration::ZERO);
    assert!(backend.observed.0.lock().ready.is_empty());
    eprintln!("bounded priming error after {:?}: {error}", start.elapsed());
}

#[test]
fn eof_before_recovery_retires_empty_video_and_drains_audio() {
    let (_, cut) = fixture::intra_refresh("short_compressed_recovery");
    let path = fixture::tmp("short_compressed_recovery.mkv");
    fixture::encode(&["-i", cut.to_str().unwrap(),
        "-f", "lavfi", "-i", "sine=sample_rate=48000:duration=0.32",
        "-map", "0:v:0", "-map", "1:a:0", "-t", "0.32",
        "-c:v", "copy", "-c:a", "pcm_s16le", "-f", "matroska"], &path);
    let (_, packets) = fixture::video_stream(&path);
    assert!(!packets.is_empty(), "fixture must contain incomplete recovery pictures");
    assert!(fixture::reference_pts(&path).is_empty(), "fixture must end before recovery");
    let expected = refcheck::ffmpeg_audio_f32(&path, 0);
    assert_eq!(expected.len(), 15360);
    let backend = Platform::new(true, false, 4);
    let (player, rx) = open(&path, backend.clone());
    ended(&player, &rx, Duration::from_secs(8));
    drop(player);
    let seen = backend.observed.0.lock();
    assert!(seen.shown.is_empty(), "partial recovery picture was displayed");
    assert!(seen.ready.is_empty(), "empty video reported a decoded picture");
    assert_eq!(seen.hidden.len(), packets.len(), "compressed decoder did not drain");
    drop(seen);
    audio_drained(&backend.audio, &expected);

    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions::default(), move |event| { let _ = tx.send(event); });
    ended(&player, &rx, Duration::from_secs(8));
    drop(player);
    assert!(backend.capture().video.iter().all(|video| video.frame_md5.is_empty()));
    audio_drained(&backend, &expected);
}

// --- L2 lifecycle regressions (controlled sinks; gates released on every path) ---

#[derive(Clone, Copy, Debug)]
enum WaitingTransition { Pending, WouldBlock, Unavailable }

impl VideoSink for WaitingTransition {
    fn output(&self) -> VideoOutput { VideoOutput { revision: 0, available: true } }
    fn poll_transition(&mut self, _: &VideoRequest) -> Poll<Result<VideoMode, VideoError>> {
        match self {
            Self::Pending => Poll::Pending,
            Self::WouldBlock => Poll::Ready(Err(VideoError::Sink(SinkError::WouldBlock))),
            Self::Unavailable => Poll::Ready(Err(VideoError::Sink(SinkError::Unavailable))),
        }
    }
    fn push_packet(&mut self, _: ProducerId, _: &mut Option<Packet>, _: Duration, _: bool) -> Result<(), VideoError> {
        panic!("input transferred before the transition completed")
    }
    fn push_frame(&mut self, _: ProducerId, _: &mut Option<VideoFrame>, _: Duration) -> Result<(), VideoError> {
        panic!("input transferred before the transition completed")
    }
    fn present_from(&mut self, _: ProducerId, _: Duration) -> Result<(), VideoError> { Ok(()) }
    fn set_playing(&mut self, _: ProducerId, _: bool) -> Result<(), VideoError> { Ok(()) }
    fn frame_lead(&self) -> Duration { Duration::ZERO }
    fn poll_finish(&mut self, _: ProducerId) -> Poll<Result<(), VideoError>> { Poll::Pending }
}

struct WaitingTransitionBackend {
    audio: Arc<Headless>,
    transition: WaitingTransition,
}

impl Backend for WaitingTransitionBackend {
    fn audio(&self) -> Box<dyn AudioSink> { self.audio.audio() }
    fn subtitles(&self) -> Box<dyn SubtitleSink> { self.audio.subtitles() }
    fn video(&self, _: Arc<dyn Clock>) -> Box<dyn VideoSink> { Box::new(self.transition) }
}

#[test]
fn every_transient_startup_transition_expires_without_admitting_media() {
    let clip = fixture::tmp("transient_transition_deadline.m2v");
    fixture::encode(&["-f", "lavfi", "-i", "color=size=16x16:rate=25:duration=0.04",
        "-an", "-c:v", "mpeg2video", "-g", "1", "-bf", "0", "-frames:v", "1", "-f", "mpeg2video"], &clip);
    for transition in [WaitingTransition::Pending, WaitingTransition::WouldBlock, WaitingTransition::Unavailable] {
        let (tx, rx) = std::sync::mpsc::channel();
        let start = Instant::now();
        let player = Player::open(
            clip.to_str().unwrap(),
            Arc::new(WaitingTransitionBackend { audio: Headless::new(), transition }),
            Arc::new(codecs::context()),
            PlayerOptions { realtime: false, ..PlayerOptions::default() },
            move |event| { let _ = tx.send(event); },
        );
        let watchdog = start + Duration::from_secs(8);
        loop {
            match rx.recv_timeout(watchdog.saturating_duration_since(Instant::now())) {
                Ok(Event::Error(_)) => break,
                Ok(Event::Changed) => {}
                Ok(Event::Ended) => panic!("{transition:?} ended without a completed video transition"),
                Err(error) => panic!("{transition:?} escaped the startup budget: {error}"),
            }
        }
        assert!(start.elapsed() >= Duration::from_secs(5), "{transition:?} failed before its active allowance");
        assert_eq!(player.state().position, Duration::ZERO, "{transition:?} advanced the held clock");
    }
}

struct Gate {
    release: Arc<AtomicBool>,
    done: Arc<(Mutex<bool>, Condvar)>,
}

impl Gate {
    fn new() -> Self {
        Self {
            release: Arc::new(AtomicBool::new(false)),
            done: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }
    fn release_all(&self) {
        self.release.store(true, Ordering::SeqCst);
        let (lock, cv) = &*self.done;
        *lock.lock() = true;
        cv.notify_all();
    }
}

struct TransitionPendingSink {
    poll_count: Arc<AtomicUsize>,
    cancelled_observed: Arc<AtomicBool>,
    captured_request: Arc<Mutex<Option<VideoRequest>>>,
    gate: Gate,
    inner: Box<dyn VideoSink>,
}

impl VideoSink for TransitionPendingSink {
    fn output(&self) -> VideoOutput {
        self.inner.output()
    }
    fn poll_transition(&mut self, request: &VideoRequest) -> Poll<Result<VideoMode, VideoError>> {
        self.poll_count.fetch_add(1, Ordering::SeqCst);
        {
            let mut cap = self.captured_request.lock();
            if cap.is_none() {
                *cap = Some(request.clone());
            }
        }
        if request.control.cancelled(request.producer, request.seek_generation)
            || self.gate.release.load(Ordering::SeqCst)
        {
            self.cancelled_observed.store(true, Ordering::SeqCst);
            return self.inner.poll_transition(request);
        }
        Poll::Pending
    }
    fn push_packet(
        &mut self,
        producer: ProducerId,
        packet: &mut Option<Packet>,
        pts: Duration,
        random_access: bool,
    ) -> Result<(), VideoError> {
        self.inner.push_packet(producer, packet, pts, random_access)
    }
    fn push_frame(
        &mut self,
        producer: ProducerId,
        frame: &mut Option<VideoFrame>,
        pts: Duration,
    ) -> Result<(), VideoError> {
        self.inner.push_frame(producer, frame, pts)
    }
    fn present_from(&mut self, producer: ProducerId, start: Duration) -> Result<(), VideoError> {
        self.inner.present_from(producer, start)
    }
    fn set_playing(&mut self, producer: ProducerId, playing: bool) -> Result<(), VideoError> {
        self.inner.set_playing(producer, playing)
    }
    fn frame_lead(&self) -> Duration {
        self.inner.frame_lead()
    }
    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        self.inner.poll_finish(producer)
    }
}

struct TransitionPendingBackend {
    audio: Arc<Headless>,
    poll_count: Arc<AtomicUsize>,
    cancelled_observed: Arc<AtomicBool>,
    captured_request: Arc<Mutex<Option<VideoRequest>>>,
    gate: Gate,
}

impl Backend for TransitionPendingBackend {
    fn audio(&self) -> Box<dyn AudioSink> {
        self.audio.audio()
    }
    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        self.audio.subtitles()
    }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        Box::new(TransitionPendingSink {
            poll_count: self.poll_count.clone(),
            cancelled_observed: self.cancelled_observed.clone(),
            captured_request: self.captured_request.clone(),
            gate: Gate {
                release: self.gate.release.clone(),
                done: self.gate.done.clone(),
            },
            inner: self.audio.video(clock),
        })
    }
}

#[test]
fn blocked_native_transition_releases_on_player_drop() {
    let clip = fixture::tmp("transition_pending_test.mkv");
    fixture::encode(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x48:rate=8:duration=1",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-f",
            "matroska",
        ],
        &clip,
    );
    let poll_count = Arc::new(AtomicUsize::new(0));
    let cancelled_observed = Arc::new(AtomicBool::new(false));
    let captured_request = Arc::new(Mutex::new(None));
    let gate = Gate::new();
    let backend = Arc::new(TransitionPendingBackend {
        audio: Headless::new(),
        poll_count: poll_count.clone(),
        cancelled_observed: cancelled_observed.clone(),
        captured_request: captured_request.clone(),
        gate: Gate {
            release: gate.release.clone(),
            done: gate.done.clone(),
        },
    });
    let player = Player::open(
        clip.to_str().unwrap(),
        backend,
        Arc::new(codecs::context()),
        PlayerOptions::default(),
        |_| {},
    );
    let wait_poll = Instant::now() + Duration::from_secs(3);
    while poll_count.load(Ordering::SeqCst) == 0 && Instant::now() < wait_poll {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        poll_count.load(Ordering::SeqCst) > 0,
        "poll_transition was never called"
    );

    // Drop on a separate thread with an independent completion deadline so a
    // stuck join cannot hang the test thread indefinitely.
    let drop_deadline = Instant::now() + Duration::from_secs(3);
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        drop(player);
        let _ = tx.send(());
    });
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(()) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Release every controlled gate so unresolved native work cannot hold drop.
                gate.release_all();
                if Instant::now() >= drop_deadline {
                    gate.release_all();
                    let _ = handle.join();
                    panic!("player drop exceeded independent completion deadline");
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    gate.release_all();
    let _ = handle.join();
    assert!(
        cancelled_observed.load(Ordering::SeqCst)
            || captured_request
                .lock()
                .as_ref()
                .is_some_and(|r| r.control.cancelled(r.producer, r.seek_generation)),
        "captured request was never cancelled on drop"
    );
}

struct PendingIdentitySink {
    requests: Arc<Mutex<Vec<VideoRequest>>>,
    early_ready: Arc<Mutex<Option<PictureReady>>>,
    polls_before_ready: usize,
    poll_count: usize,
    admitted: bool,
    inner: Box<dyn VideoSink>,
}

impl VideoSink for PendingIdentitySink {
    fn output(&self) -> VideoOutput {
        self.inner.output()
    }
    fn poll_transition(&mut self, request: &VideoRequest) -> Poll<Result<VideoMode, VideoError>> {
        self.requests.lock().push(request.clone());
        if self.early_ready.lock().is_none() {
            if let VideoTarget::Compressed { ready, .. } | VideoTarget::Frames { ready, .. } =
                &request.target
            {
                *self.early_ready.lock() = Some(ready.clone());
            }
        }
        self.poll_count += 1;
        if self.poll_count < self.polls_before_ready {
            return Poll::Pending;
        }
        let result = self.inner.poll_transition(request);
        if matches!(result, Poll::Ready(Ok(_))) {
            self.admitted = true;
        }
        result
    }
    fn push_packet(
        &mut self,
        producer: ProducerId,
        packet: &mut Option<Packet>,
        pts: Duration,
        random_access: bool,
    ) -> Result<(), VideoError> {
        self.inner.push_packet(producer, packet, pts, random_access)
    }
    fn push_frame(
        &mut self,
        producer: ProducerId,
        frame: &mut Option<VideoFrame>,
        pts: Duration,
    ) -> Result<(), VideoError> {
        self.inner.push_frame(producer, frame, pts)
    }
    fn present_from(&mut self, producer: ProducerId, start: Duration) -> Result<(), VideoError> {
        self.inner.present_from(producer, start)
    }
    fn set_playing(&mut self, producer: ProducerId, playing: bool) -> Result<(), VideoError> {
        self.inner.set_playing(producer, playing)
    }
    fn frame_lead(&self) -> Duration {
        self.inner.frame_lead()
    }
    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        self.inner.poll_finish(producer)
    }
}

struct PendingIdentityBackend {
    audio: Arc<Headless>,
    requests: Arc<Mutex<Vec<VideoRequest>>>,
    early_ready: Arc<Mutex<Option<PictureReady>>>,
    polls_before_ready: usize,
}

impl Backend for PendingIdentityBackend {
    fn audio(&self) -> Box<dyn AudioSink> {
        self.audio.audio()
    }
    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        self.audio.subtitles()
    }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        Box::new(PendingIdentitySink {
            requests: self.requests.clone(),
            early_ready: self.early_ready.clone(),
            polls_before_ready: self.polls_before_ready,
            poll_count: 0,
            admitted: false,
            inner: self.audio.video(clock),
        })
    }
}

#[test]
fn repeated_pending_request_identity() {
    let clip = fixture::tmp("pending_identity.mkv");
    fixture::encode(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x48:rate=8:duration=1",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-f",
            "matroska",
        ],
        &clip,
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let early_ready = Arc::new(Mutex::new(None));
    let backend = Arc::new(PendingIdentityBackend {
        audio: Headless::new(),
        requests: requests.clone(),
        early_ready: early_ready.clone(),
        polls_before_ready: 5,
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(
        clip.to_str().unwrap(),
        backend,
        Arc::new(codecs::context()),
        PlayerOptions {
            realtime: false,
            ..PlayerOptions::default()
        },
        move |ev| {
            let _ = tx.send(ev);
        },
    );
    // Retain an early callback and exercise it after admission begins.
    let wait_ready = Instant::now() + Duration::from_secs(3);
    while early_ready.lock().is_none() && Instant::now() < wait_ready {
        std::thread::sleep(Duration::from_millis(5));
    }
    if let Some(cb) = early_ready.lock().clone() {
        cb.ready(Duration::from_millis(40));
    }
    ended(&player, &rx, Duration::from_secs(10));
    drop(player);

    let recorded = requests.lock();
    assert!(
        recorded.len() >= 5,
        "expected at least 5 polls, got {}",
        recorded.len()
    );
    let first = &recorded[0];
    for (i, req) in recorded.iter().take(5).enumerate() {
        assert_eq!(req.producer, first.producer, "poll {i} producer differed");
        assert_eq!(
            req.seek_generation, first.seek_generation,
            "poll {i} seek_generation differed"
        );
        assert_eq!(
            req.output_revision, first.output_revision,
            "poll {i} output_revision differed"
        );
        assert_eq!(req.deadline, first.deadline, "poll {i} deadline differed");
        assert!(
            Arc::ptr_eq(&req.control, &first.control),
            "poll {i} control Arc identity differed"
        );
        match (&req.target, &first.target) {
            (
                VideoTarget::Compressed {
                    params: a,
                    present_from: pa,
                    ..
                },
                VideoTarget::Compressed {
                    params: b,
                    present_from: pb,
                    ..
                },
            ) => {
                assert!(Arc::ptr_eq(a, b), "poll {i} params Arc identity differed");
                assert_eq!(pa, pb, "poll {i} present_from differed");
            }
            (
                VideoTarget::Frames {
                    params: a,
                    reset: ra,
                    ..
                },
                VideoTarget::Frames {
                    params: b,
                    reset: rb,
                    ..
                },
            ) => {
                assert!(Arc::ptr_eq(a, b), "poll {i} params Arc identity differed");
                assert_eq!(ra, rb, "poll {i} reset differed");
            }
            _ => panic!("poll {i} target kind mismatch between repeated pending polls"),
        }
    }
}

/// Same-seek replacement: only the live producer's picture may release buffering.
struct SameSeekSink {
    state: Arc<Mutex<SameSeekState>>,
    inner: Box<dyn VideoSink>,
}

struct SameSeekState {
    first_compressed: Option<(ProducerId, u64, PictureReady, Arc<dyn VideoControl>)>,
    second_frames: Option<(ProducerId, u64)>,
    hold_second: bool,
    replacement_polls: usize,
}

impl VideoSink for SameSeekSink {
    fn output(&self) -> VideoOutput {
        VideoOutput {
            revision: 0,
            available: true,
        }
    }
    fn poll_transition(&mut self, request: &VideoRequest) -> Poll<Result<VideoMode, VideoError>> {
        let mut s = self.state.lock();
        match &request.target {
            VideoTarget::Compressed { ready, .. } => {
                if s.first_compressed.is_none() {
                    s.first_compressed = Some((
                        request.producer,
                        request.seek_generation,
                        ready.clone(),
                        Arc::clone(&request.control),
                    ));
                }
                // Accept compressed then force software replacement via Fallback on push.
                Poll::Ready(Ok(VideoMode::Compressed))
            }
            VideoTarget::Frames { .. } => {
                s.replacement_polls += 1;
                if s.second_frames.is_none() {
                    s.second_frames = Some((request.producer, request.seek_generation));
                }
                if s.hold_second {
                    return Poll::Pending;
                }
                drop(s);
                self.inner.poll_transition(request)
            }
            VideoTarget::Retired => self.inner.poll_transition(request),
        }
    }
    fn push_packet(
        &mut self,
        producer: ProducerId,
        _packet: &mut Option<Packet>,
        _pts: Duration,
        _random_access: bool,
    ) -> Result<(), VideoError> {
        let s = self.state.lock();
        if s.first_compressed
            .as_ref()
            .is_some_and(|(p, _, _, _)| *p == producer)
        {
            drop(s);
            // Trigger same-seek software replacement while leaving input owned.
            return Err(VideoError::Sink(SinkError::Fallback(
                "force same-seek software".into(),
            )));
        }
        Err(VideoError::Superseded)
    }
    fn push_frame(
        &mut self,
        producer: ProducerId,
        frame: &mut Option<VideoFrame>,
        pts: Duration,
    ) -> Result<(), VideoError> {
        if self.state.lock().hold_second {
            return Err(VideoError::Sink(SinkError::WouldBlock));
        }
        self.inner.push_frame(producer, frame, pts)
    }
    fn present_from(&mut self, producer: ProducerId, start: Duration) -> Result<(), VideoError> {
        if self
            .state
            .lock()
            .first_compressed
            .as_ref()
            .is_some_and(|(p, _, _, _)| *p == producer)
        {
            return Ok(());
        }
        self.inner.present_from(producer, start)
    }
    fn set_playing(&mut self, producer: ProducerId, playing: bool) -> Result<(), VideoError> {
        if self
            .state
            .lock()
            .first_compressed
            .as_ref()
            .is_some_and(|(p, _, _, _)| *p == producer)
        {
            return Ok(());
        }
        self.inner.set_playing(producer, playing)
    }
    fn frame_lead(&self) -> Duration {
        self.inner.frame_lead()
    }
    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        self.inner.poll_finish(producer)
    }
}

struct SameSeekBackend {
    audio: Arc<Headless>,
    state: Arc<Mutex<SameSeekState>>,
}

impl Backend for SameSeekBackend {
    fn audio(&self) -> Box<dyn AudioSink> {
        self.audio.audio()
    }
    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        self.audio.subtitles()
    }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        Box::new(SameSeekSink {
            state: self.state.clone(),
            inner: self.audio.video(clock),
        })
    }
}

#[test]
fn same_seek_replacement_ignores_old_readiness_until_live_picture() {
    let clip = fixture::tmp("stale_readiness.mkv");
    fixture::encode(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x48:rate=8:duration=1",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-f",
            "matroska",
        ],
        &clip,
    );
    let state = Arc::new(Mutex::new(SameSeekState {
        first_compressed: None,
        second_frames: None,
        hold_second: true,
        replacement_polls: 0,
    }));
    let headless = Headless::new();
    let backend = Arc::new(SameSeekBackend {
        audio: Arc::clone(&headless),
        state: state.clone(),
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(
        clip.to_str().unwrap(),
        backend,
        Arc::new(codecs::context()),
        PlayerOptions {
            realtime: false,
            ..PlayerOptions::default()
        },
        move |ev| {
            let _ = tx.send(ev);
        },
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let s = state.lock();
        if s.first_compressed.is_some() && s.second_frames.is_some() {
            break;
        }
        drop(s);
        std::thread::sleep(Duration::from_millis(10));
    }
    {
        let s = state.lock();
        assert!(s.first_compressed.is_some(), "first compressed producer missing");
        assert!(s.second_frames.is_some(), "same-seek frames replacement missing");
        let (p1, g1, _, _) = s.first_compressed.as_ref().unwrap();
        let (p2, g2) = s.second_frames.as_ref().unwrap();
        assert_ne!(p1, p2, "replacement must use a distinct producer");
        assert_eq!(g1, g2, "replacement must stay on the same seek generation");
    }

    let (old_ready, control, polls) = {
        let s = state.lock();
        let (_, _, ready, control) = s.first_compressed.as_ref().unwrap();
        (ready.clone(), Arc::clone(control), s.replacement_polls)
    };
    assert!(player.state().buffering);
    assert_eq!(player.state().position, Duration::ZERO);
    old_ready.ready(Duration::from_millis(80));
    let watchdog = Instant::now() + Duration::from_secs(2);
    while state.lock().replacement_polls < polls + 2 {
        assert!(Instant::now() < watchdog, "the woken replacement was not pumped");
        std::thread::sleep(Duration::from_millis(5));
    }
    let held = player.state();
    assert!(held.buffering, "the cancelled producer released the replacement hold");
    assert_eq!(held.position, Duration::ZERO, "stale readiness advanced the clock");
    assert!(held.error.is_none(), "{:?}", held.error);
    state.lock().hold_second = false;
    control.wake();
    ended(&player, &rx, Duration::from_secs(10));
    drop(player);
    let capture = headless.capture();
    assert_eq!(capture.video.len(), 1);
    assert_eq!(capture.video[0].frame_md5, refcheck::ffmpeg_video_md5s(&clip, 0, "yuv420p"));
    assert_eq!(capture.video[0].pts, fixture::reference_pts(&clip));
}

struct RetentionSink {
    block_remaining: usize,
    refused_would_block: Arc<AtomicBool>,
    accepted_packets: Arc<Mutex<Vec<Vec<u8>>>>,
    sealed: bool,
    inner: Box<dyn VideoSink>,
}

impl RetentionSink {
    fn admit(&mut self) -> Result<(), VideoError> {
        assert!(!self.sealed, "engine submitted media after sealing input");
        if self.block_remaining > 0 {
            self.block_remaining -= 1;
            self.refused_would_block.store(true, Ordering::SeqCst);
            return Err(SinkError::WouldBlock.into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum RetentionMode { CompressedPackets, SoftwareFrames }

impl VideoSink for RetentionSink {
    fn output(&self) -> VideoOutput { self.inner.output() }
    fn poll_transition(&mut self, request: &VideoRequest) -> Poll<Result<VideoMode, VideoError>> {
        self.inner.poll_transition(request)
    }
    fn push_packet(&mut self, producer: ProducerId, packet: &mut Option<Packet>, pts: Duration, random_access: bool) -> Result<(), VideoError> {
        self.admit()?;
        let bytes = packet.as_ref().expect("missing retained packet").data.clone();
        self.inner.push_packet(producer, packet, pts, random_access)?;
        assert!(packet.is_none(), "successful packet admission retained ownership");
        self.accepted_packets.lock().push(bytes);
        Ok(())
    }
    fn push_frame(&mut self, producer: ProducerId, frame: &mut Option<VideoFrame>, pts: Duration) -> Result<(), VideoError> {
        self.admit()?;
        self.inner.push_frame(producer, frame, pts)
    }
    fn present_from(&mut self, producer: ProducerId, start: Duration) -> Result<(), VideoError> {
        self.inner.present_from(producer, start)
    }
    fn set_playing(&mut self, producer: ProducerId, playing: bool) -> Result<(), VideoError> {
        self.inner.set_playing(producer, playing)
    }
    fn frame_lead(&self) -> Duration { self.inner.frame_lead() }
    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        self.sealed = true;
        self.inner.poll_finish(producer)
    }
}

struct RetentionBackend {
    audio: Arc<Headless>,
    mode: RetentionMode,
    refused_would_block: Arc<AtomicBool>,
    accepted_packets: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Backend for RetentionBackend {
    fn audio(&self) -> Box<dyn AudioSink> { self.audio.audio() }
    fn subtitles(&self) -> Box<dyn SubtitleSink> { self.audio.subtitles() }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        let inner = match self.mode {
            RetentionMode::CompressedPackets => Platform::new(false, false, 0).video(clock),
            RetentionMode::SoftwareFrames => self.audio.video(clock),
        };
        Box::new(RetentionSink {
            block_remaining: 3, refused_would_block: Arc::clone(&self.refused_would_block),
            accepted_packets: Arc::clone(&self.accepted_packets), sealed: false, inner,
        })
    }
}

#[test]
fn packet_and_frame_retained_across_backpressure_and_eof() {
    let clip = fixture::tmp("backpressure_retained.mkv");
    fixture::encode(&["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=8:duration=1",
        "-c:v", "libx264", "-preset", "ultrafast", "-g", "8", "-bf", "0", "-f", "matroska"], &clip);
    let (_, packets) = fixture::video_stream(&clip);
    let expected_packets: Vec<_> = packets.into_iter().map(|packet| packet.data).collect();
    let expected_frames = refcheck::ffmpeg_video_md5s(&clip, 0, "yuv420p");
    let expected_pts = fixture::reference_pts(&clip);
    for mode in [RetentionMode::CompressedPackets, RetentionMode::SoftwareFrames] {
        let refused_would_block = Arc::new(AtomicBool::new(false));
        let accepted_packets = Arc::new(Mutex::new(Vec::new()));
        let headless = Headless::new();
        let backend = Arc::new(RetentionBackend {
            audio: Arc::clone(&headless), mode,
            refused_would_block: Arc::clone(&refused_would_block),
            accepted_packets: Arc::clone(&accepted_packets),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let player = Player::open(clip.to_str().unwrap(), backend, Arc::new(codecs::context()),
            PlayerOptions { realtime: false, ..PlayerOptions::default() },
            move |event| { let _ = tx.send(event); });
        ended(&player, &rx, Duration::from_secs(10));
        drop(player);
        assert!(refused_would_block.load(Ordering::SeqCst));
        match mode {
            RetentionMode::CompressedPackets => {
                assert_eq!(*accepted_packets.lock(), expected_packets, "packet bytes lost, duplicated, or reordered");
            }
            RetentionMode::SoftwareFrames => {
                let capture = headless.capture();
                assert_eq!(capture.video.len(), 1);
                assert_eq!(capture.video[0].frame_md5, expected_frames);
                assert_eq!(capture.video[0].pts, expected_pts);
            }
        }
    }
}

struct LateFaultSink {
    inner: Box<dyn VideoSink>,
    finish_pending_polls: usize,
    finished_ok: bool,
    fault_published: bool,
    control: Option<Arc<dyn player::backend::VideoControl>>,
    fault: Arc<Mutex<Option<String>>>,
}

impl VideoSink for LateFaultSink {
    fn output(&self) -> VideoOutput {
        self.inner.output()
    }
    fn poll_transition(&mut self, request: &VideoRequest) -> Poll<Result<VideoMode, VideoError>> {
        self.control = Some(Arc::clone(&request.control));
        if self.fault_published {
            return Poll::Ready(Err(VideoError::Sink(SinkError::Fatal(
                "late asynchronous decoder fault".into(),
            ))));
        }
        self.inner.poll_transition(request)
    }
    fn push_packet(
        &mut self,
        producer: ProducerId,
        packet: &mut Option<Packet>,
        pts: Duration,
        random_access: bool,
    ) -> Result<(), VideoError> {
        self.inner.push_packet(producer, packet, pts, random_access)
    }
    fn push_frame(
        &mut self,
        producer: ProducerId,
        frame: &mut Option<VideoFrame>,
        pts: Duration,
    ) -> Result<(), VideoError> {
        self.inner.push_frame(producer, frame, pts)
    }
    fn present_from(&mut self, producer: ProducerId, start: Duration) -> Result<(), VideoError> {
        self.inner.present_from(producer, start)
    }
    fn set_playing(&mut self, producer: ProducerId, playing: bool) -> Result<(), VideoError> {
        self.inner.set_playing(producer, playing)
    }
    fn frame_lead(&self) -> Duration {
        self.inner.frame_lead()
    }
    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        if !self.finished_ok {
            if self.finish_pending_polls > 0 {
                self.finish_pending_polls -= 1;
                return Poll::Pending;
            }
            // Finish succeeds first; fault is published afterward with wake.
            self.finished_ok = true;
            let _ = self.inner.poll_finish(producer);
            self.fault_published = true;
            *self.fault.lock() = Some("late asynchronous decoder fault".into());
            if let Some(ctrl) = &self.control {
                ctrl.wake();
            }
            return Poll::Ready(Ok(()));
        }
        if self.fault_published {
            return Poll::Ready(Err(VideoError::Sink(SinkError::Fatal(
                "late asynchronous decoder fault".into(),
            ))));
        }
        self.inner.poll_finish(producer)
    }
}

struct LateFaultBackend {
    audio: Arc<Headless>,
    fault: Arc<Mutex<Option<String>>>,
}

impl Backend for LateFaultBackend {
    fn audio(&self) -> Box<dyn AudioSink> {
        self.audio.audio()
    }
    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        self.audio.subtitles()
    }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        Box::new(LateFaultSink {
            inner: self.audio.video(clock),
            finish_pending_polls: 2,
            finished_ok: false,
            fault_published: false,
            control: None,
            fault: self.fault.clone(),
        })
    }
}

#[test]
fn late_async_error_after_last_input_wakes_and_reports() {
    let clip = fixture::tmp("late_async_error.mkv");
    fixture::encode(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x48:rate=8:duration=1",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-f",
            "matroska",
        ],
        &clip,
    );
    let fault = Arc::new(Mutex::new(None));
    let backend = Arc::new(LateFaultBackend {
        audio: Headless::new(),
        fault: fault.clone(),
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(
        clip.to_str().unwrap(),
        backend,
        Arc::new(codecs::context()),
        PlayerOptions {
            realtime: false,
            ..PlayerOptions::default()
        },
        move |ev| {
            let _ = tx.send(ev);
        },
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut err_msg = None;
    let mut ended_seen = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Error(msg)) => {
                err_msg = Some(msg);
                break;
            }
            Ok(Event::Ended) => {
                ended_seen = true;
                // Fault must be observed before Ended wins the race permanently.
            }
            Ok(Event::Changed) => {
                if player.state().error.as_ref().is_some_and(|e| e.contains("late asynchronous decoder fault"))
                {
                    err_msg = player.state().error.clone();
                    break;
                }
            }
            Err(_) => {
                if let Some(e) = player.state().error.clone() {
                    if e.contains("late asynchronous decoder fault") {
                        err_msg = Some(e);
                        break;
                    }
                }
            }
        }
    }
    let state = player.state();
    drop(player);
    let msg = err_msg
        .or(state.error.clone())
        .expect("late async error was never received");
    assert!(
        msg.contains("late asynchronous decoder fault"),
        "error did not contain fault text: {msg}"
    );
    assert!(
        !ended_seen || state.error.is_some(),
        "Ended without the exact late fault"
    );
    let _ = fault;
}

#[derive(Default)]
struct PauseReplacement {
    revision: u64,
    accepted: usize,
    gated: bool,
    initial: Option<(ProducerId, u64)>,
    replacement: Option<(ProducerId, u64)>,
    playing: Option<(ProducerId, bool)>,
    control: Option<Arc<dyn VideoControl>>,
}

struct PauseReplaceSink {
    inner: Box<dyn VideoSink>,
    state: Arc<(Mutex<PauseReplacement>, Condvar)>,
    translated: Option<VideoRequest>,
    admitted: Option<ProducerId>,
}

impl VideoSink for PauseReplaceSink {
    fn output(&self) -> VideoOutput {
        VideoOutput { revision: self.state.0.lock().revision, available: true }
    }
    fn poll_transition(&mut self, request: &VideoRequest) -> Poll<Result<VideoMode, VideoError>> {
        {
            let mut state = self.state.0.lock();
            state.control = Some(Arc::clone(&request.control));
            if request.output_revision != state.revision {
                return Poll::Ready(Err(VideoError::Superseded));
            }
        }
        // The outer surface revision is independent of Headless's fixed output.
        // Translate once per immutable producer, without changing its identity.
        if self.translated.as_ref().is_none_or(|r| r.producer != request.producer) {
            let mut translated = request.clone();
            translated.output_revision = 0;
            self.translated = Some(translated);
        }
        let result = self.inner.poll_transition(self.translated.as_ref().unwrap());
        if matches!(&result, Poll::Ready(Ok(VideoMode::Frames))) && self.admitted != Some(request.producer) {
            self.admitted = Some(request.producer);
            let mut state = self.state.0.lock();
            let identity = Some((request.producer, request.seek_generation));
            if request.output_revision == 0 { state.initial = identity; }
            else { state.replacement = identity; }
            self.state.1.notify_all();
        }
        result
    }
    fn push_packet(&mut self, producer: ProducerId, packet: &mut Option<Packet>, pts: Duration, random_access: bool) -> Result<(), VideoError> {
        self.inner.push_packet(producer, packet, pts, random_access)
    }
    fn push_frame(&mut self, producer: ProducerId, frame: &mut Option<VideoFrame>, pts: Duration) -> Result<(), VideoError> {
        {
            let mut state = self.state.0.lock();
            if state.revision == 0 && state.accepted == 4 {
                state.gated = true;
                self.state.1.notify_all();
                return Err(SinkError::WouldBlock.into());
            }
        }
        self.inner.push_frame(producer, frame, pts)?;
        self.state.0.lock().accepted += 1;
        Ok(())
    }
    fn present_from(&mut self, producer: ProducerId, start: Duration) -> Result<(), VideoError> {
        self.inner.present_from(producer, start)
    }
    fn set_playing(&mut self, producer: ProducerId, playing: bool) -> Result<(), VideoError> {
        self.inner.set_playing(producer, playing)?;
        self.state.0.lock().playing = Some((producer, playing));
        self.state.1.notify_all();
        Ok(())
    }
    fn frame_lead(&self) -> Duration { self.inner.frame_lead() }
    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        self.inner.poll_finish(producer)
    }
}

struct PauseReplaceBackend {
    audio: Arc<Headless>,
    state: Arc<(Mutex<PauseReplacement>, Condvar)>,
}

impl Backend for PauseReplaceBackend {
    fn audio(&self) -> Box<dyn AudioSink> { self.audio.audio() }
    fn subtitles(&self) -> Box<dyn SubtitleSink> { self.audio.subtitles() }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        Box::new(PauseReplaceSink {
            inner: self.audio.video(clock), state: Arc::clone(&self.state),
            translated: None, admitted: None,
        })
    }
}

#[test]
fn window_replacement_while_paused_triggers_recovery_at_held_position() {
    let clip = fixture::tmp("pause_recovery.mkv");
    fixture::encode(&["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=8:duration=4",
        "-c:v", "libx264", "-preset", "ultrafast", "-x264-params", "keyint=32:bframes=0",
        "-f", "matroska"], &clip);
    let expected = refcheck::ffmpeg_video_md5s(&clip, 0, "yuv420p");
    let expected_pts = fixture::reference_pts(&clip);
    let state = Arc::new((Mutex::new(PauseReplacement::default()), Condvar::new()));
    let headless = Headless::new();
    let backend = Arc::new(PauseReplaceBackend { audio: Arc::clone(&headless), state: Arc::clone(&state) });
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(clip.to_str().unwrap(), backend, Arc::new(codecs::context()),
        PlayerOptions::default(), move |event| { let _ = tx.send(event); });
    let deadline = Instant::now() + Duration::from_secs(8);
    {
        let mut observed = state.0.lock();
        while !observed.gated {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "initial playback never reached its gate: {:?}", player.state());
            state.1.wait_for(&mut observed, remaining);
        }
    }
    player.pause();
    let held = player.state().position;
    assert!(held > Duration::ZERO && held < *expected_pts.last().unwrap());
    let (initial, wake) = {
        let mut observed = state.0.lock();
        observed.revision = 1;
        (observed.initial.unwrap(), Arc::clone(observed.control.as_ref().unwrap()))
    };
    wake.wake();
    {
        let mut observed = state.0.lock();
        while !observed.replacement.is_some_and(|(producer, _)| observed.playing == Some((producer, false))) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "paused replacement did not configure with paused intent: {:?}", player.state());
            state.1.wait_for(&mut observed, remaining);
        }
        let replacement = observed.replacement.unwrap();
        assert_ne!(replacement.0, initial.0);
        assert_eq!(replacement.1, initial.1 + 1, "replacement must initiate recovery without a user seek");
    }
    assert!(!player.state().playing, "recovery changed the user's paused intent");
    assert_eq!(player.state().position, held, "recovery moved the held clock");
    player.play();
    ended(&player, &rx, Duration::from_secs(10));
    drop(player);
    let capture = headless.capture();
    assert_eq!(capture.video.len(), 1);
    let video = &capture.video[0];
    assert_eq!(video.flushes, [4], "recovery must reset once after the gated prefix");
    let suffix = expected_pts.partition_point(|pts| *pts < held);
    let hashes: Vec<_> = expected[..4].iter().chain(&expected[suffix..]).cloned().collect();
    let pts: Vec<_> = expected_pts[..4].iter().chain(&expected_pts[suffix..]).copied().collect();
    assert_eq!(video.frame_md5, hashes, "recovery lost, duplicated, or reordered pictures");
    assert_eq!(video.pts, pts, "recovery did not resume at the held position");
}

struct FormatChangeSink {
    transitions: Arc<Mutex<Vec<(ProducerId, bool, u32, u32)>>>,
    inner: Box<dyn VideoSink>,
    initial_deadline: Option<Instant>,
    accepted_frames: usize,
}

impl VideoSink for FormatChangeSink {
    fn output(&self) -> VideoOutput {
        self.inner.output()
    }
    fn poll_transition(&mut self, request: &VideoRequest) -> Poll<Result<VideoMode, VideoError>> {
        let result = self.inner.poll_transition(request);
        if matches!(&result, Poll::Ready(Ok(VideoMode::Frames))) {
            self.initial_deadline.get_or_insert(request.deadline);
            if let VideoTarget::Frames { reset, params, .. } = &request.target {
                let mut transitions = self.transitions.lock();
                if transitions.last().is_none_or(|(producer, _, _, _)| *producer != request.producer) {
                    transitions.push((request.producer, *reset, params.width.unwrap_or(0), params.height.unwrap_or(0)));
                }
            }
        }
        result
    }
    fn push_packet(
        &mut self,
        producer: ProducerId,
        packet: &mut Option<Packet>,
        pts: Duration,
        random_access: bool,
    ) -> Result<(), VideoError> {
        self.inner.push_packet(producer, packet, pts, random_access)
    }
    fn push_frame(
        &mut self,
        producer: ProducerId,
        frame: &mut Option<VideoFrame>,
        pts: Duration,
    ) -> Result<(), VideoError> {
        // Keep the last old-format frame owned until healthy playback has
        // outlived its startup budget. The subsequent format needs a new budget.
        if self.accepted_frames == 7
            && Instant::now() < self.initial_deadline.unwrap() + Duration::from_millis(100)
        {
            return Err(SinkError::WouldBlock.into());
        }
        self.inner.push_frame(producer, frame, pts)?;
        self.accepted_frames += 1;
        Ok(())
    }
    fn present_from(&mut self, producer: ProducerId, start: Duration) -> Result<(), VideoError> {
        self.inner.present_from(producer, start)
    }
    fn set_playing(&mut self, producer: ProducerId, playing: bool) -> Result<(), VideoError> {
        self.inner.set_playing(producer, playing)
    }
    fn frame_lead(&self) -> Duration {
        self.inner.frame_lead()
    }
    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        self.inner.poll_finish(producer)
    }
}

struct FormatChangeBackend {
    audio: Arc<Headless>,
    transitions: Arc<Mutex<Vec<(ProducerId, bool, u32, u32)>>>,
}

impl Backend for FormatChangeBackend {
    fn audio(&self) -> Box<dyn AudioSink> {
        self.audio.audio()
    }
    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        self.audio.subtitles()
    }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        Box::new(FormatChangeSink {
            transitions: self.transitions.clone(),
            inner: self.audio.video(clock),
            initial_deadline: None,
            accepted_frames: 0,
        })
    }
}

#[test]
fn software_format_change_issues_fresh_producer_and_preserves_queued_frames() {
    // Concatenated encoded sequences at two resolutions force a midstream
    // decoded format change through the real software path.
    let part_a = fixture::tmp("format_change_a.mkv");
    let part_b = fixture::tmp("format_change_b.mkv");
    let clip = fixture::tmp("format_change_concat.mkv");
    fixture::encode(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x48:rate=8:duration=1",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-bf",
            "0",
            "-f",
            "matroska",
        ],
        &part_a,
    );
    fixture::encode(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=96x64:rate=8:duration=1",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-bf",
            "0",
            "-f",
            "matroska",
        ],
        &part_b,
    );
    // Concat demuxer list.
    let list = fixture::tmp("format_change_list.txt");
    std::fs::write(
        &list,
        format!(
            "file '{}'\nfile '{}'\n",
            part_a.display(),
            part_b.display()
        ),
    )
    .unwrap();
    fixture::encode(
        &[
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
            list.to_str().unwrap(),
            "-c",
            "copy",
            "-f",
            "matroska",
        ],
        &clip,
    );

    let transitions = Arc::new(Mutex::new(Vec::new()));
    let headless = Headless::new();
    let backend = Arc::new(FormatChangeBackend {
        audio: headless.clone(),
        transitions: transitions.clone(),
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(
        clip.to_str().unwrap(),
        backend,
        Arc::new(codecs::context()),
        PlayerOptions {
            realtime: false,
            ..PlayerOptions::default()
        },
        move |ev| {
            let _ = tx.send(ev);
        },
    );
    ended(&player, &rx, Duration::from_secs(15));
    let state = player.state();
    drop(player);

    let recorded = transitions.lock().clone();
    assert!(
        recorded.len() >= 2,
        "expected initial + format-change producers, got {recorded:?}"
    );
    let (p0, reset0, w0, h0) = recorded[0];
    assert!(reset0, "initial transition should have reset=true");
    assert_eq!((w0, h0), (64, 48), "initial decoded dimensions");
    let (p1, _, _, _) = recorded.iter().skip(1)
        .find(|(_, reset, w, h)| !*reset && (*w, *h) == (96, 64))
        .expect("the second sequence must configure 96x64 without resetting accepted output");
    assert_ne!(p0, *p1, "format change must issue a fresh producer");
    assert!(state.error.is_none(), "format change failed: {state:?}");
    let capture = headless.capture();
    let mut expected = refcheck::ffmpeg_video_md5s(&part_a, 0, "yuv420p");
    expected.extend(refcheck::ffmpeg_video_md5s(&part_b, 0, "yuv420p"));
    assert_eq!(expected.len(), 16, "both complete eight-frame sequences");
    assert_eq!(capture.video.len(), 1);
    assert_eq!(capture.video[0].frame_md5, expected, "format change must preserve every frame in order");
}
