//! Linear PCM in QuickTime, CAF, AIFF and OMA plays through the registry
//! (`codecs::context()`) sample for sample as FFmpeg decodes it: the
//! big-endian and signed 8-bit decoders, the MOV demuxer's choice of PCM
//! codec (`twos`, `in24`, `in32`, `fl32`, `fl64`) and AIFF's by sample size.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use player::{Event, Headless, Player, PlayerOptions};
use refcheck::fate;

/// The audio `path` plays to its end with the default track.
fn played_audio(path: &Path) -> Vec<f32> {
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
    let deadline = Instant::now() + Duration::from_secs(60);
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
    let capture = backend.capture();
    capture.audio.first().unwrap_or_else(|| panic!("{}: no audio played", path.display())).pcm.clone()
}

fn plays_as_ffmpeg(path: &Path) {
    let audio = played_audio(path);
    let reference = refcheck::ffmpeg_audio_f32(path, 0);
    assert_eq!(audio.len(), reference.len(), "{}: samples", path.display());
    let snr = refcheck::snr_db(&reference, &audio, 0);
    assert!(snr.is_infinite(), "{}: the audio equals FFmpeg's ({snr} dB)", path.display());
}

fn tmp(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("player-lpcm-{}-{name}", std::process::id()))
}

/// 1 s of a stereo 440 Hz tone at 44.1 kHz as `codec`, by FFmpeg, in the
/// container of `name`'s extension.
fn tone(name: &str, codec: &str) -> PathBuf {
    let path = tmp(name);
    let out = Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100:duration=1", "-ac", "2", "-c:a", codec])
        .arg(&path)
        .output()
        .expect("the system FFmpeg runs");
    assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    path
}

#[test]
fn lpcm_in_quicktime() {
    plays_as_ffmpeg(&fate("qt-surge-suite/surge-2-16-B-twos.mov"));
    for codec in ["pcm_s24be", "pcm_s32be", "pcm_f32be", "pcm_f64be"] {
        let path = tone(&format!("{codec}.mov"), codec);
        plays_as_ffmpeg(&path);
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn lpcm_in_caf() {
    plays_as_ffmpeg(&fate("caf/caf-pcm16.caf"));
    for codec in ["pcm_s16be", "pcm_s24be", "pcm_s32be", "pcm_f32be", "pcm_f64be", "pcm_s8"] {
        let path = tone(&format!("{codec}.caf"), codec);
        plays_as_ffmpeg(&path);
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn lpcm_in_aiff() {
    for codec in ["pcm_s16be", "pcm_s24be", "pcm_s32be", "pcm_f32be"] {
        let path = tone(&format!("{codec}.aiff"), codec);
        plays_as_ffmpeg(&path);
        let _ = std::fs::remove_file(&path);
    }
}

/// OMA's LPCM (EA3 codec id 4: 16-bit big-endian stereo at 44.1 kHz),
/// which FFmpeg's OMA muxer does not write: the EA3 tag and header by hand.
#[test]
fn lpcm_in_oma() {
    let raw = tmp("oma.s16be");
    let out = Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100:duration=1", "-ac", "2", "-f", "s16be"])
        .arg(&raw)
        .output()
        .expect("the system FFmpeg runs");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut file = b"ea3\x03\x00\x00\x00\x00\x00\x00".to_vec();
    let mut ea3 = [0u8; 96];
    ea3[..3].copy_from_slice(b"EA3");
    ea3[3] = 1;
    ea3[5] = 96;
    ea3[6..8].copy_from_slice(&[0xff, 0xff]);
    ea3[32] = 4;
    file.extend_from_slice(&ea3);
    file.extend_from_slice(&std::fs::read(&raw).unwrap());
    let path = tmp("lpcm.oma");
    std::fs::write(&path, file).unwrap();
    plays_as_ffmpeg(&path);
    let _ = std::fs::remove_file(&raw);
    let _ = std::fs::remove_file(&path);
}
