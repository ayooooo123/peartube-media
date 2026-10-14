//! Real Player/Headless playback through a one-frame preparation adapter.
//! Compare complete decoded contents against ordinary Headless playback; observe
//! decisions and retained tokens without hiding video-thread failures in panics.

#[path = "support/seek_preroll.rs"]
mod fixture;

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::task::Poll;
use std::time::{Duration, Instant};

use oxideav_core::{Packet, VideoFrame};
use parking_lot::Mutex;
use player::backend::{
    AudioSink, Backend, Clock, FrameDecision, PreparedFrameId, ProducerId, SinkError,
    SubtitleSink, VideoError, VideoMode, VideoOutput, VideoRequest, VideoSink,
};
use player::{Event, Headless, Player, PlayerOptions};

#[derive(Clone, Copy)]
enum Mode {
    Prepared,
    Unsupported,
    Late(Duration),
    RetryPresent(Duration),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Record {
    producer: ProducerId,
    token: PreparedFrameId,
    pts: Duration,
    clock: Duration,
}

#[derive(Default)]
struct Trace {
    prepared: Vec<Record>,
    presented: Vec<Record>,
    discarded: Vec<Record>,
    cancelled: Vec<Record>,
    pending: Option<Record>,
    admission_retries: usize,
    discard_retries: usize,
    present_retries: usize,
    finish_present_count: Option<usize>,
    violations: Vec<&'static str>,
}

struct Control {
    mode: Mode,
    trace: Mutex<Trace>,
}

struct PreparedBackend {
    headless: Arc<Headless>,
    control: Arc<Control>,
}

impl PreparedBackend {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            headless: Headless::new(),
            control: Arc::new(Control { mode, trace: Mutex::new(Trace::default()) }),
        })
    }

    fn assert_clean(&self) {
        let trace = self.control.trace.lock();
        assert!(trace.violations.is_empty(), "sink violations: {:?}", trace.violations);
        assert!(trace.pending.is_none(), "ended with an ungranted frame");
    }
}

impl Backend for PreparedBackend {
    fn audio(&self) -> Box<dyn AudioSink> { self.headless.audio() }
    fn subtitles(&self) -> Box<dyn SubtitleSink> { self.headless.subtitles() }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        Box::new(PreparedSink {
            inner: self.headless.video(clock.clone()), clock,
            control: self.control.clone(), next_id: 1, staged: None,
        })
    }
}

struct PreparedSink {
    inner: Box<dyn VideoSink>,
    clock: Arc<dyn Clock>,
    control: Arc<Control>,
    next_id: u64,
    staged: Option<(Record, VideoFrame)>,
}

impl PreparedSink {
    fn violation(&self, message: &'static str) -> VideoError {
        self.control.trace.lock().violations.push(message);
        VideoError::Sink(SinkError::Fatal(message.into()))
    }
}

impl VideoSink for PreparedSink {
    fn output(&self) -> VideoOutput { self.inner.output() }

    fn poll_transition(&mut self, request: &VideoRequest) -> Poll<Result<VideoMode, VideoError>> {
        if self.staged.as_ref().is_some_and(|(record, _)| record.producer != request.producer) {
            if let Some((record, _)) = self.staged.take() {
                let mut trace = self.control.trace.lock();
                trace.cancelled.push(record);
                trace.pending = None;
            }
        }
        self.inner.poll_transition(request)
    }

    fn push_packet(&mut self, producer: ProducerId, packet: &mut Option<Packet>, pts: Duration, random_access: bool) -> Result<(), VideoError> {
        self.inner.push_packet(producer, packet, pts, random_access)
    }

    fn push_frame(&mut self, producer: ProducerId, frame: &mut Option<VideoFrame>, pts: Duration) -> Result<(), VideoError> {
        self.inner.push_frame(producer, frame, pts)
    }

    fn prepare_frame(&mut self, producer: ProducerId, frame: &mut Option<VideoFrame>, pts: Duration) -> Result<PreparedFrameId, VideoError> {
        if matches!(self.control.mode, Mode::Unsupported) {
            return Err(VideoError::Unsupported);
        }
        if let Mode::Late(target) = self.control.mode {
            if pts == target && self.clock.now().unwrap_or_default() <= pts + Duration::from_millis(150) {
                self.control.trace.lock().admission_retries += 1;
                return Err(VideoError::Sink(SinkError::WouldBlock));
            }
        }
        if self.staged.is_some() { return Err(self.violation("second input while a frame is staged")); }
        let Some(payload) = frame.take() else { return Err(self.violation("missing preparation input")); };
        let record = Record { producer, token: PreparedFrameId(self.next_id), pts, clock: self.clock.now().unwrap_or_default() };
        self.next_id += 1;
        self.staged = Some((record, payload));
        let mut trace = self.control.trace.lock();
        trace.prepared.push(record);
        trace.pending = Some(record);
        Ok(record.token)
    }

    fn decide_frame(&mut self, producer: ProducerId, token: PreparedFrameId, decision: FrameDecision) -> Result<(), VideoError> {
        let Some((record, _)) = self.staged.as_ref() else { return Err(self.violation("decision without retained input")); };
        let record = *record;
        if (record.producer, record.token) != (producer, token) {
            return Err(self.violation("decision addressed to a stale token"));
        }
        {
            let mut trace = self.control.trace.lock();
            if decision == FrameDecision::Discard && matches!(self.control.mode, Mode::Late(_)) && trace.discard_retries == 0 {
                trace.discard_retries += 1;
                return Err(VideoError::Sink(SinkError::Unavailable));
            }
            if decision == FrameDecision::Present && matches!(self.control.mode, Mode::RetryPresent(pts) if pts == record.pts) && trace.present_retries < 3 {
                trace.present_retries += 1;
                return Err(VideoError::Sink(SinkError::WouldBlock));
            }
        }
        let Some((_, payload)) = self.staged.take() else { return Err(self.violation("retained input disappeared")); };
        if decision == FrameDecision::Present {
            let now = self.clock.now().unwrap_or_default();
            let mut input = Some(payload);
            if let Err(error) = self.inner.push_frame(producer, &mut input, record.pts) {
                let Some(payload) = input else { return Err(self.violation("inner sink consumed refused input")); };
                self.staged = Some((record, payload));
                return Err(error);
            }
            self.control.trace.lock().presented.push(Record { clock: now, ..record });
        } else {
            self.control.trace.lock().discarded.push(record);
        }
        self.control.trace.lock().pending = None;
        Ok(())
    }

    fn present_from(&mut self, producer: ProducerId, start: Duration) -> Result<(), VideoError> { self.inner.present_from(producer, start) }
    fn set_playing(&mut self, producer: ProducerId, playing: bool) -> Result<(), VideoError> { self.inner.set_playing(producer, playing) }
    fn frame_lead(&self) -> Duration { self.inner.frame_lead() }
    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        if self.staged.is_some() { return Poll::Ready(Err(self.violation("EOF sealed over ungranted input"))); }
        let mut trace = self.control.trace.lock();
        let count = trace.presented.len();
        trace.finish_present_count.get_or_insert(count);
        drop(trace);
        self.inner.poll_finish(producer)
    }
}

fn clip(name: &str, seconds: u32, fps: u32) -> PathBuf {
    let path = fixture::tmp(&format!("prepared_{name}_{seconds}_{fps}.mkv"));
    fixture::encode(&[
        "-f", "lavfi", "-i", &format!("testsrc2=size=320x240:rate={fps}:duration={seconds}"),
        "-f", "lavfi", "-i", &format!("sine=frequency=440:sample_rate=48000:duration={seconds}"),
        "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "pcm_s16le", "-f", "matroska",
    ], &path);
    path
}

fn open(path: &Path, backend: Arc<dyn Backend>, realtime: bool) -> (Player, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), backend, Arc::new(codecs::context()),
        PlayerOptions { realtime, ..PlayerOptions::default() }, move |event| { let _ = tx.send(event); });
    (player, rx)
}

fn end(player: &Player, rx: &mpsc::Receiver<Event>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "playback stalled: {:?}", player.state());
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Ended) => { assert!(player.state().error.is_none()); return; }
            Ok(Event::Error(error)) => panic!("playback error: {error}"),
            Ok(Event::Changed) => {},
            Err(error) => panic!("playback event wait: {error}; {:?}", player.state()),
        }
    }
}

fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "expected lifecycle transition did not occur");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn frames(headless: &Headless) -> Vec<(Duration, String)> {
    let capture = headless.capture();
    capture.video.iter().flat_map(|video| {
        assert_eq!(video.pts.len(), video.frame_md5.len());
        video.pts.iter().copied().zip(video.frame_md5.iter().cloned())
    }).collect()
}

fn reference(path: &Path) -> Vec<(Duration, String)> {
    let backend = Headless::new();
    let (player, rx) = open(path, backend.clone(), false);
    end(&player, &rx);
    drop(player);
    let result = frames(&backend);
    assert_eq!(result.iter().map(|(pts, _)| *pts).collect::<Vec<_>>(), fixture::reference_pts(path));
    result
}

#[test]
fn preparation_preserves_complete_frames_and_grants_only_when_due() {
    let path = clip("timing", 2, 10);
    let expected = reference(&path);
    let backend = PreparedBackend::new(Mode::Prepared);
    let (player, rx) = open(&path, backend.clone(), true);
    end(&player, &rx);
    assert_eq!(player.state().dropped_frames, 0);
    drop(player);
    backend.assert_clean();
    assert_eq!(frames(&backend.headless), expected);
    let trace = backend.control.trace.lock();
    assert_eq!(trace.prepared.len(), expected.len());
    assert_eq!(trace.presented.len(), expected.len());
    assert!(trace.prepared.iter().skip(1).any(|record| record.clock < record.pts));
    for (prepared, presented) in trace.prepared.iter().zip(&trace.presented).skip(1) {
        assert_eq!((prepared.producer, prepared.token, prepared.pts), (presented.producer, presented.token, presented.pts));
        assert!(presented.clock >= presented.pts, "early presentation: {presented:?}");
        assert!(presented.clock <= presented.pts + Duration::from_millis(100), "late presentation: {presented:?}");
    }
}

#[test]
fn pause_holds_the_prepared_token_and_resumes_its_exact_picture() {
    let path = clip("pause", 2, 2);
    let expected = reference(&path);
    let backend = PreparedBackend::new(Mode::Prepared);
    let (player, rx) = open(&path, backend.clone(), true);
    wait_for(|| backend.control.trace.lock().pending.is_some_and(|record| record.pts == Duration::from_millis(500)));
    player.pause();
    std::thread::sleep(Duration::from_millis(100));
    let held = backend.control.trace.lock().pending.expect("prepared token across pause");
    let before = frames(&backend.headless);
    let position = player.state().position;
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(player.state().position, position);
    assert_eq!(frames(&backend.headless), before);
    assert_eq!(backend.control.trace.lock().pending, Some(held));
    player.play();
    end(&player, &rx);
    assert_eq!(player.state().dropped_frames, 0);
    drop(player);
    backend.assert_clean();
    assert_eq!(frames(&backend.headless), expected);
    assert!(backend.control.trace.lock().presented.iter().any(|record| record.token == held.token && record.producer == held.producer && record.pts == held.pts));
}

#[test]
fn a_retried_discard_drops_exactly_the_late_picture_once() {
    let path = clip("discard", 2, 2);
    let target = Duration::from_millis(500);
    let expected = reference(&path).into_iter().filter(|(pts, _)| *pts != target).collect::<Vec<_>>();
    let backend = PreparedBackend::new(Mode::Late(target));
    let (player, rx) = open(&path, backend.clone(), true);
    end(&player, &rx);
    assert_eq!(player.state().dropped_frames, 1);
    drop(player);
    backend.assert_clean();
    assert_eq!(frames(&backend.headless), expected);
    let trace = backend.control.trace.lock();
    assert!(trace.admission_retries > 0);
    assert_eq!(trace.discard_retries, 1);
    assert_eq!(trace.discarded.iter().map(|record| record.pts).collect::<Vec<_>>(), [target]);
}

#[test]
fn seek_cancels_the_held_token_and_presents_the_exact_new_segment() {
    let path = clip("seek", 3, 2);
    let target = Duration::from_millis(1500);
    let expected = reference(&path).into_iter().filter(|(pts, _)| *pts >= target).collect::<Vec<_>>();
    let backend = PreparedBackend::new(Mode::Prepared);
    let (player, rx) = open(&path, backend.clone(), true);
    wait_for(|| backend.control.trace.lock().pending.is_some_and(|record| record.pts == Duration::from_millis(500)));
    let held = backend.control.trace.lock().pending.expect("pre-seek prepared input");
    let before = frames(&backend.headless).len();
    player.seek(target);
    end(&player, &rx);
    drop(player);
    backend.assert_clean();
    assert_eq!(&frames(&backend.headless)[before..], expected);
    let trace = backend.control.trace.lock();
    assert!(trace.cancelled.contains(&held));
    assert!(!trace.presented.iter().any(|record| record.token == held.token && record.producer == held.producer));
    assert_eq!(trace.presented.iter().filter(|record| record.producer != held.producer).map(|record| record.pts).collect::<Vec<_>>(), expected.iter().map(|(pts, _)| *pts).collect::<Vec<_>>());
}

#[test]
fn unsupported_preparation_keeps_complete_inline_playback() {
    let path = clip("unsupported", 1, 10);
    let expected = reference(&path);
    let backend = PreparedBackend::new(Mode::Unsupported);
    let (player, rx) = open(&path, backend.clone(), true);
    end(&player, &rx);
    assert_eq!(player.state().dropped_frames, 0);
    drop(player);
    backend.assert_clean();
    assert_eq!(frames(&backend.headless), expected);
}

#[test]
fn final_present_retry_retains_content_and_defers_eof_sealing() {
    let path = clip("finish", 2, 2);
    let expected = reference(&path);
    let target = expected.last().unwrap().0;
    let backend = PreparedBackend::new(Mode::RetryPresent(target));
    let (player, rx) = open(&path, backend.clone(), true);
    end(&player, &rx);
    assert_eq!(player.state().dropped_frames, 0);
    drop(player);
    backend.assert_clean();
    assert_eq!(frames(&backend.headless), expected);
    let trace = backend.control.trace.lock();
    assert_eq!(trace.present_retries, 3);
    assert_eq!(trace.finish_present_count, Some(expected.len()));
}
