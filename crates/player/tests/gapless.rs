//! Gapless sources through the engine: the encoder delay and end padding
//! their containers declare never reach the sink, so a playback holds
//! exactly FFmpeg's samples, and exactly the ones `refcheck::decode` keeps.
//! FATE samples (tests/fate/gapless.mak, demux.mak): Ogg Opus with end
//! padding past the final granule, AAC in MP4 with iTunSMPB and edit-list
//! priming and padding (HE-AAC v2 too), MP3 with LAME delay and padding
//! (and an iTunes MP3 FFmpeg trims nothing from).

use std::sync::Arc;

use oxideav_core::MediaType;
use player::{Headless, Player, PlayerOptions};

/// Interleaved f32 of a whole playback (realtime off) and its channels.
fn play(path: &std::path::Path) -> (Vec<f32>, usize) {
    let backend = Headless::new();
    let p = Player::open(
        path.to_str().unwrap(),
        backend.clone(),
        Arc::new(codecs::context()),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        |_| {},
    );
    let state = p.wait();
    drop(p);
    assert!(state.error.is_none(), "{}: {:?}", path.display(), state.error);
    let audio = backend.capture().audio.remove(0);
    (audio.pcm, audio.channels as usize)
}

const SAMPLES: &[&str] = &[
    "ogg/intro-partial.opus",
    "audiomatch/tones_opus_48000_stereo.opus",
    "audiomatch/tones_afconvert_44100_stereo_aac_he2.m4a",
    "audiomatch/tones_fdkaac_44100_stereo_aac_lc.m4a",
    "gapless/102400samples_qt-lc-aac.m4a",
    "gapless/gapless.mp3",
    "gapless/gapless-itunes.mp3",
    "audiomatch/square3.mp3",
];

#[test]
fn playback_has_ffmpegs_samples_and_refchecks() {
    let mut failures = Vec::new();
    for rel in SAMPLES {
        let path = refcheck::fate(rel);
        let (played, channels) = play(&path);
        let ff = refcheck::ffmpeg_audio_f32(&path, 0);
        let decoded = refcheck::decode(&path, &[codecs::register_all], MediaType::Audio, 0);
        let kept = refcheck::interleaved_f32(&decoded);
        let snr = refcheck::try_snr_db(&ff, &played, usize::MAX);
        eprintln!(
            "{rel}: played {} x{channels}, refcheck {}, FFmpeg {}, SNR {snr:.2?} dB",
            played.len() / channels,
            kept.len() / channels,
            ff.len() / channels
        );
        if played.len() != ff.len() {
            failures.push(format!("{rel}: played {} samples, FFmpeg {}", played.len(), ff.len()));
        }
        if played != kept {
            failures.push(format!("{rel}: the playback is not the samples refcheck keeps"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
