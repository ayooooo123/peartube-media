//! The engine applies container trims (`PacketMetadata::audio_trim`: encoder
//! delay and end padding) once, after decoding, exactly as `refcheck::decode`
//! does: the same samples reach the sink as refcheck keeps. The synthetic
//! fixture's samples carry their own decoder-output index.

use std::sync::Arc;
use std::time::Duration;

use oxideav_core::{MediaType, RuntimeContext};
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

/// Plays `spec` to the end (seeking to `seek` first, when given) and
/// returns the capture.
fn play(spec: &Spec, seek: Option<Duration>) -> Capture {
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
    backend.capture()
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
fn refcheck_keeps(spec: &Spec) -> Vec<(u64, u64)> {
    let path = write_spec(spec);
    let decoded = refcheck::decode(&path, &[trim_fixture::register], MediaType::Audio, 0);
    let _ = std::fs::remove_file(&path);
    trim_fixture::runs(&trim_fixture::indices(&refcheck::interleaved_f32(&decoded), spec.channels as usize))
}

fn range(start: u64, end: u64) -> Vec<(u64, u64)> {
    vec![(start, end)]
}

fn assert_plays(spec: &Spec, expected: Vec<(u64, u64)>) {
    assert_eq!(played(&play(spec, None)), expected, "the engine's output");
    assert_eq!(refcheck_keeps(spec), expected, "refcheck keeps the same samples");
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
fn padding_spans_the_frames_of_one_packet() {
    let mut spec = Spec::new(1, 48000, 1024, 3);
    spec.mode = Mode::Split;
    spec.packets[2].discard = 700;
    assert_plays(&spec, range(0, 3 * 1024 - 700));
}

#[test]
fn a_seek_restarts_the_trims_from_the_landing_packet() {
    // Seeking to zero lands on the packet straddling it, which carries the
    // rest of the priming; the end padding still goes.
    let mut spec = Spec::new(1, 48000, 1024, 40);
    spec.start_pts = -2220;
    spec.skip_after_seek = true;
    spec.packets[0].skip = 2220;
    spec.packets[39].discard = 340;
    let capture = play(&spec, Some(Duration::ZERO));
    assert_eq!(played(&capture), range(2220, 40 * 1024 - 340));
}
