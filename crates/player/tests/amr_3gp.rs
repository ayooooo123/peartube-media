//! AMR phone recordings play as FFmpeg decodes them: AMR-WB in FATE's 3GPP
//! files (`amrwb/*.awb`, brand `3gp4`, read by the MP4 demuxer), and AMR-NB
//! and AMR-WB that FFmpeg remuxes into 3GPP (`.3gp`) and QuickTime (`.mov`)
//! files. 3GPP sample entries say 2 channels (TS 26.244 fixes the field)
//! and FFmpeg forces mono and the AMR rate (`mov_finalize_stsd_codec`); its
//! own muxer writes 1, so the remuxes get the 2 phones write. Sound must be
//! FFmpeg 2da55bf's C path, sample for sample.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use player::{Event, Headless, Player, PlayerOptions};

/// The sound the Player plays from `path`: samples, rate, channels.
fn played(path: &Path) -> (Vec<f32>, u32, u16) {
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        move |event| { let _ = tx.send(event); });
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(Event::Ended) => break,
            Ok(Event::Error(error)) => panic!("{}: {error}", path.display()),
            Ok(Event::Changed) => {}
            Err(error) => panic!("{}: player did not end: {error}: {:?}", path.display(), player.state()),
        }
    }
    let state = player.state();
    drop(player);
    assert!(state.error.is_none(), "{}: {:?}", path.display(), state.error);
    let capture = backend.capture();
    let audio = capture.audio.first().unwrap_or_else(|| panic!("{}: no sound played", path.display()));
    (audio.pcm.clone(), audio.sample_rate, audio.channels)
}

/// FFmpeg 2da55bf's decode through its C code, as interleaved f32.
fn ffmpeg_f32(path: &Path) -> Vec<f32> {
    let out = Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
}

/// FFmpeg's remux of FATE's `source` into `format`, its AMR sample entry
/// then saying 2 channels, as 3GPP writers set it.
fn phone_remux(source: &str, format: &str) -> PathBuf {
    let stem = Path::new(source).file_stem().unwrap().to_str().unwrap();
    let path = std::env::temp_dir().join(format!("amr-{}-{stem}.{format}", std::process::id()));
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-i"])
        .arg(refcheck::fate(source))
        .args(["-c", "copy", "-f", format])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg could not remux {source}");
    let mut bytes = std::fs::read(&path).unwrap();
    let stsd = bytes.windows(4).position(|w| w == b"stsd").expect("an stsd atom");
    let entry = stsd + bytes[stsd..].windows(4).position(|w| w == b"samr" || w == b"sawb").expect("an AMR entry");
    // Sound sample description (QTFF): the format at 4, channels at 24.
    bytes[entry + 20..entry + 22].copy_from_slice(&2u16.to_be_bytes());
    std::fs::write(&path, bytes).unwrap();
    path
}

fn plays_like_ffmpeg(path: &Path, rate: u32) {
    let name = path.display();
    let reference = ffmpeg_f32(path);
    assert!(!reference.is_empty(), "FFmpeg decodes nothing from {name}");
    let (pcm, sample_rate, channels) = played(path);
    assert_eq!((sample_rate, channels), (rate, 1), "{name}: layout");
    assert_eq!(pcm.len(), reference.len(), "{name}: samples");
    let first_wrong = pcm.iter().zip(&reference).position(|(a, b)| a.to_bits() != b.to_bits());
    assert_eq!(first_wrong, None, "{name}: first sample unlike FFmpeg's");
}

/// FATE's AMR-WB files: 3GPP, `sawb` entries saying 2 channels.
#[test]
fn amr_wb_in_fates_3gpp_files() {
    for name in [
        "seed-6k60", "seed-8k85", "seed-12k65", "seed-14k25", "seed-15k85", "seed-18k25", "seed-19k85", "seed-23k05",
        "seed-23k85", "deus-23k85",
    ] {
        plays_like_ffmpeg(&refcheck::fate(&format!("amrwb/{name}.awb")), 16_000);
    }
}

#[test]
fn amr_nb_in_3gp() {
    for mode in ["4.75k", "12.2k"] {
        let path = phone_remux(&format!("amrnb/{mode}.amr"), "3gp");
        plays_like_ffmpeg(&path, 8_000);
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn amr_nb_and_wb_in_mov() {
    for (source, rate) in [("amrnb/7.95k.amr", 8_000), ("amrwb/seed-23k85.awb", 16_000)] {
        let path = phone_remux(source, "mov");
        plays_like_ffmpeg(&path, rate);
        let _ = std::fs::remove_file(path);
    }
}
