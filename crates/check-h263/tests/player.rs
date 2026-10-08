//! The Player plays the archive's Intel H.263 captures as FFmpeg 2da55bf
//! decodes them: every video frame (`-idct simple`) and every audio
//! sample. Their audio headers are what old capture software wrote: MP3
//! stored by the byte (`strh.dwSampleSize` 1) in i263.avi, 16-bit PCM in
//! i263_2.avi.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use player::{Event, Headless, Player, PlayerOptions};
use support::*;

fn play(path: &Path) -> player::Capture {
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(
        path.to_str().unwrap(),
        backend.clone(),
        Arc::new(codecs::context()),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        move |event| {
            let _ = tx.send(event);
        },
    );
    player.play();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Ended) => break,
            Ok(Event::Error(error)) => panic!("{}: {error}", path.display()),
            Ok(Event::Changed) => {}
            Err(error) => panic!("{}: playback did not end: {error}", path.display()),
        }
    }
    assert!(player.state().error.is_none(), "{}: {:?}", path.display(), player.state().error);
    drop(player);
    backend.capture()
}

/// Video frame for frame; audio sample for sample, within `snr_floor`
/// (infinite: equal).
fn plays_as_ffmpeg(file: &str, snr_floor: f64) {
    let path = archive(file);
    let capture = play(&path);
    let video = capture.video.first().unwrap_or_else(|| panic!("{file}: no video played"));
    assert_frames_equal(file, &video.frame_md5, &ffmpeg_md5s(&path, &[]));
    let audio = capture.audio.first().unwrap_or_else(|| panic!("{file}: no audio played"));
    let reference = refcheck::ffmpeg_src_audio_f32(&path, 0);
    assert_eq!(audio.pcm.len(), reference.len(), "{file}: audio samples");
    let snr = refcheck::snr_db(&reference, &audio.pcm, 0);
    assert!(snr >= snr_floor, "{file}: audio {snr} dB from FFmpeg's");
}

/// 352x240 Intel H.263 with MPEG-2 layer III audio at 22050 Hz, stored
/// by the byte in chunks that cut through its frames. The floor is
/// oxideav-mp3's accuracy on this stream, not the demuxer's: FFmpeg's own
/// copy of these MP3 frames decodes to the same 85.4 dB. Lost, repeated or
/// shifted frames fall far below it and change the sample count.
#[test]
fn i263_with_mp3_stored_by_the_byte() {
    plays_as_ffmpeg("V-codecs/I263/i263.avi", 85.0);
}

/// 320x240 Intel H.263 (PB frames) with 16-bit stereo PCM.
#[test]
fn i263_2_with_pcm() {
    plays_as_ffmpeg("V-codecs/I263/i263_2.avi", f64::INFINITY);
}
