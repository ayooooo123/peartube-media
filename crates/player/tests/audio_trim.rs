//! The engine applies container trims (`PacketMetadata::audio_trim`: encoder
//! delay and end padding) once, after decoding, exactly as `refcheck::decode`
//! does: the same samples reach the sink as refcheck keeps. The synthetic
//! fixture's samples carry their own decoder-output index.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use oxideav_core::{MediaType, RuntimeContext};
use parking_lot::Mutex;
use player::backend::{AudioSink, Backend, Clock, SinkError, SubtitleSink, VideoSink};
use player::{Capture, Headless, Player, PlayerOptions};
use refcheck::trim_fixture::{self, Mode, Spec};

fn write_spec(spec: &Spec) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let name = format!("player-trim-{}-{n}.{}", std::process::id(), trim_fixture::EXTENSION);
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, spec.to_bytes()).unwrap();
    path
}

fn context() -> Arc<RuntimeContext> {
    let mut ctx = RuntimeContext::new();
    trim_fixture::register(&mut ctx);
    Arc::new(ctx)
}

/// Plays `spec` to the end, optionally seeking first.
fn play(spec: &Spec, seek: Option<Duration>) -> (Capture, audio_trim::Fallbacks) {
    let path = write_spec(spec);
    let backend = Headless::new();
    let p = Player::open(
        path.to_str().unwrap(),
        backend.clone(),
        context(),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        |_| {},
    );
    if let Some(to) = seek {
        p.seek(to);
    }
    let state = p.wait();
    drop(p);
    let _ = std::fs::remove_file(&path);
    assert!(state.error.is_none(), "playback error: {:?}", state.error);
    assert_eq!(state.audio, Some(0), "audio track was disabled");
    (backend.capture(), state.audio_trim_fallbacks)
}

/// The decoder-output samples the sink took after its last flush (a seek
/// flushes), as runs `[start, end)`.
fn played(capture: &Capture) -> Vec<(u64, u64)> {
    let audio = &capture.audio[0];
    let from = match audio.flushes.last() {
        Some(&w) => audio.writes.get(w).map_or(audio.pcm.len(), |&(_, at)| at),
        None => 0,
    };
    trim_fixture::runs(&trim_fixture::indices(&audio.pcm[from..], audio.channels as usize))
}

/// What `refcheck::decode` keeps of the same stream.
fn refcheck_keeps(spec: &Spec) -> (Vec<(u64, u64)>, audio_trim::Fallbacks) {
    let path = write_spec(spec);
    let decoded = refcheck::decode(&path, &[trim_fixture::register], MediaType::Audio, 0);
    let _ = std::fs::remove_file(&path);
    (
        trim_fixture::runs(&trim_fixture::indices(&refcheck::interleaved_f32(&decoded), spec.channels as usize)),
        decoded.trim_fallbacks,
    )
}

fn range(start: u64, end: u64) -> Vec<(u64, u64)> {
    vec![(start, end)]
}

fn assert_plays(spec: &Spec, expected: Vec<(u64, u64)>) -> (audio_trim::Fallbacks, audio_trim::Fallbacks) {
    let (capture, engine_fallbacks) = play(spec, None);
    let (kept, reference_fallbacks) = refcheck_keeps(spec);
    assert_eq!(played(&capture), expected, "the engine's output");
    assert_eq!(kept, expected, "refcheck keeps the same samples");
    (engine_fallbacks, reference_fallbacks)
}

#[test]
fn priming_stamped_before_zero_is_skipped_once_and_padding_dropped() {
    // MP4 edit-list priming: the first 2220 samples (more than two frames)
    // are stamped before zero. Trimming them must not also drop the
    // negative-pts samples a second time.
    let mut spec = Spec::new(1, 48000, 1024, 6);
    spec.start_pts = -2220;
    spec.packets[0].skip = 2220;
    spec.packets[5].discard = 340;
    assert_plays(&spec, range(2220, 6 * 1024 - 340));
}

#[test]
fn priming_stamped_from_zero_is_skipped() {
    // Matroska CodecDelay: the stream starts at zero, the decoder's first
    // 1024 samples are priming.
    let mut spec = Spec::new(2, 48000, 1024, 5);
    spec.packets[0].skip = 1024;
    assert_plays(&spec, range(1024, 5 * 1024));
}

#[test]
fn trims_follow_the_decoders_output_rate() {
    let mut spec = Spec::new(2, 24000, 1024, 5);
    spec.output_rate = 48000;
    spec.packets[0].skip = 1100;
    spec.packets[4].discard = 100;
    assert_plays(&spec, range(2200, 5 * 2048 - 200));
}

#[test]
fn padding_reaches_the_tail_a_delayed_decoder_drains() {
    let mut spec = Spec::new(1, 48000, 1024, 4);
    spec.mode = Mode::Delayed;
    spec.packets[0].skip = 1500;
    spec.packets[3].discard = 300;
    assert_plays(&spec, range(1500, 4 * 1024 - 300));
}

#[test]
fn mid_stream_padding_stays_with_the_packet_a_delayed_decoder_outputs_late() {
    let mut spec = Spec::new(1, 48000, 4, 3);
    spec.mode = Mode::Delayed;
    spec.packets[1].discard = 1;
    assert_plays(&spec, vec![(0, 7), (8, 12)]);
}

#[test]
fn padding_spans_the_frames_of_one_packet() {
    let mut spec = Spec::new(1, 48000, 1024, 3);
    spec.mode = Mode::Split;
    spec.packets[2].discard = 700;
    assert_plays(&spec, range(0, 3 * 1024 - 700));
}

#[test]
fn a_seek_restarts_the_trims_from_the_landing_packet() {
    // The seek lands at positive PTS, so the before-zero gate cannot
    // hide a missing 1196-sample priming trim on the landing packet.
    let mut spec = Spec::new(1, 48000, 1024, 40);
    spec.skip_after_seek = true;
    spec.packets[0].skip = 2220;
    spec.packets[39].discard = 340;
    let (capture, _) = play(&spec, Some(Duration::from_secs_f64(1024.0 / 48000.0)));
    assert_eq!(played(&capture), range(2220, 40 * 1024 - 340));
}

/// Headless output whose first audio write waits for the test, so the test
/// seeks while the engine is inside a known packet.
struct FirstWriteGate {
    inner: Arc<Headless>,
    gate: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

impl Backend for FirstWriteGate {
    fn audio(&self) -> Box<dyn AudioSink> {
        Box::new(GatedAudio { sink: self.inner.audio(), gate: self.gate.lock().take() })
    }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        self.inner.video(clock)
    }
    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        self.inner.subtitles()
    }
}

struct GatedAudio {
    sink: Box<dyn AudioSink>,
    gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
}

impl AudioSink for GatedAudio {
    fn open(&mut self, rate: u32, layout: oxideav_core::ChannelLayout) -> Result<(), SinkError> {
        self.sink.open(rate, layout)
    }
    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError> {
        if let Some((entered, release)) = self.gate.take() {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(10)).expect("the test never released the first write");
        }
        self.sink.write(pcm, pts)
    }
    fn play(&mut self) {
        self.sink.play();
    }
    fn pause(&mut self) {
        self.sink.pause();
    }
    fn flush(&mut self) {
        self.sink.flush();
    }
    fn clock(&self) -> Arc<dyn Clock> {
        self.sink.clock()
    }
}

#[test]
fn a_seek_forgets_the_trims_of_packets_sent_before_it() {
    // A delayed decoder without timestamps: when packet 0's output is
    // written, packet 1 and its 2000-sample skip are queued. The seek lands
    // on packet 20, which has no trim; none of packet 1's skip comes off it.
    let mut spec = Spec::new(1, 48000, 1024, 40);
    spec.mode = Mode::Delayed;
    spec.packets[1].skip = 2000;
    let path = write_spec(&spec);
    let (entered, first_write) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let headless = Headless::new();
    let backend = Arc::new(FirstWriteGate { inner: headless.clone(), gate: Mutex::new(Some((entered, gate))) });
    let p = Player::open(
        path.to_str().unwrap(),
        backend,
        context(),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        |_| {},
    );
    first_write.recv_timeout(Duration::from_secs(10)).expect("no audio was written");
    p.seek(Duration::from_secs_f64(20.0 * 1024.0 / 48000.0));
    release.send(()).unwrap();
    let state = p.wait();
    drop(p);
    let _ = std::fs::remove_file(&path);
    assert!(state.error.is_none(), "playback error: {:?}", state.error);
    assert_eq!(played(&headless.capture()), range(20 * 1024, 40 * 1024));
}

#[test]
fn a_failed_audio_pipeline_gives_up_its_track() {
    // The decoder fails outside the calls the engine guards one by one.
    let mut spec = Spec::new(1, 48000, 1024, 4);
    spec.panic_on_layout = true;
    let path = write_spec(&spec);
    let p = Player::open(
        path.to_str().unwrap(),
        Headless::new(),
        context(),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        |_| {},
    );
    let state = p.wait();
    drop(p);
    let _ = std::fs::remove_file(&path);
    assert_eq!(state.audio, None, "the failed pipeline still holds the audio track");
    assert!(state.error.is_some(), "the failure was not reported");
}

#[test]
fn excessive_padding_falls_back_to_audio_without_disabling_the_track() {
    // One stereo f32 frame owns 32 MiB before retained-object overhead.
    let mut spec = Spec::new(2, 48000, 1 << 22, 1);
    spec.packets[0].discard = u32::MAX;
    let (engine, reference) = assert_plays(&spec, range(0, 1 << 22));
    assert_eq!(engine.released_padding_spans, 1);
    assert_eq!(reference, engine);
}

#[test]
fn long_decoder_silence_keeps_playing_and_stamped_padding_stays_local() {
    for stamp in [false, true] {
        let mut spec = Spec::new(1, 48000, 4, 104);
        spec.silent_packets = 100;
        spec.stamp = stamp;
        spec.packets[102].discard = 1;
        let expected = if stamp { vec![(400, 411), (412, 416)] } else { range(400, 416) };
        let (engine, reference) = assert_plays(&spec, expected);
        assert!(engine.lost_packets > 0, "lost association was not reported");
        assert_eq!(reference, engine);
    }
}

#[test]
fn unrelated_frame_timestamps_and_hostile_duration_do_not_stop_audio() {
    let mut spec = Spec::new(1, 48000, 4, 140);
    spec.stamp = true;
    spec.frame_pts_offset = 1;
    spec.duration_override = Some(i64::MAX);
    spec.silent_packets = 2;
    let (engine, reference) = assert_plays(&spec, range(8, 560));
    assert!(engine.lost_packets > 0, "hostile duration was not reported");
    assert_eq!(reference, engine);
}
