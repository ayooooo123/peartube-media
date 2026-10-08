//! A bounded, asynchronous platform-decoder model. It emits one picture
//! per H.264 access unit, reorders by PTS, and can hold decoding at a gate.
//! Pixel accuracy is covered by seek_entry/intra_refresh; these tests check
//! the engine's compressed-output contract against real packet streams.
#[path = "support/seek_preroll.rs"]
mod fixture;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use oxideav_core::{CodecParameters, Packet, VideoFrame};
use parking_lot::{Condvar, Mutex};
use player::backend::{AudioSink, Backend, Clock, PictureReady, SinkError, SubtitleSink, VideoSink};
use player::{Event, Headless, Player, PlayerOptions};

#[derive(Default)]
struct Observed {
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
        Box::new(PlatformVideo { clock, observed: self.observed.clone(), paced: self.paced,
            gate: self.gate, reorder_depth: self.reorder_depth, ready: None, epoch: 0,
            state: Arc::new((Mutex::new(DecodeState::default()), Condvar::new())), worker: None })
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
    fn open_compressed(&mut self, params: &CodecParameters, ready: PictureReady) -> bool {
        assert_eq!(params.codec_id.as_str(), "h264");
        self.ready = Some(ready);
        self.start();
        true
    }
    fn present_from(&mut self, start: Duration) { self.state.0.lock().from = start; }
    fn push_packet(&mut self, _: &Packet, pts: Duration, _: bool) -> Result<(), SinkError> {
        let mut s = self.state.0.lock();
        if s.input.len() == 8 { return Err(SinkError::WouldBlock); }
        s.input.push_back(pts);
        self.state.1.notify_all();
        Ok(())
    }
    fn open_frames(&mut self, _: &CodecParameters) -> Result<(), SinkError> { panic!("unexpected software fallback") }
    fn push_frame(&mut self, _: &VideoFrame, _: Duration) -> Result<(), SinkError> { panic!("unexpected software frame") }
    fn frame_lead(&self) -> Duration { Duration::ZERO }
    fn finish(&mut self) -> Result<(), SinkError> {
        let mut s = self.state.0.lock();
        s.eof = true;
        self.state.1.notify_all();
        if s.done { Ok(()) } else { Err(SinkError::WouldBlock) }
    }
    fn flush(&mut self) {
        self.stop();
        let playing = self.state.0.lock().playing;
        self.state = Arc::new((Mutex::new(DecodeState { playing, ..DecodeState::default() }), Condvar::new()));
        self.epoch += 1;
        self.start();
    }
    fn set_playing(&mut self, playing: bool) {
        self.state.0.lock().playing = playing;
        self.state.1.notify_all();
    }
}

fn open(path: &Path, backend: Arc<Platform>) -> (Player, std::sync::mpsc::Receiver<Event>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let options = PlayerOptions { realtime: backend.paced, ..PlayerOptions::default() };
    let player = Player::open(path.to_str().unwrap(), backend, Arc::new(codecs::context()), options,
        move |event| { let _ = tx.send(event); });
    (player, rx)
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
