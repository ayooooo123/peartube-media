//! These tests need PEARTUBE_TEST_SF2 (GeneralUser GS 2.0.3) and FluidSynth
//! 2.6.1 on PATH. Neither the bank nor rendered audio is shipped.
use oxideav_core::{
    CodecId, CodecParameters, Decoder, Error, Frame, Packet, RuntimeContext, TimeBase,
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, LazyLock},
};

static FONT: LazyLock<PathBuf> = LazyLock::new(|| {
    let path = PathBuf::from(
        std::env::var_os("PEARTUBE_TEST_SF2")
            .expect("set PEARTUBE_TEST_SF2 to GeneralUser GS 2.0.3"),
    );
    let hash = format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap()));
    assert_eq!(
        hash, "9575028c7a1f589f5770fccc8cff2734566af40cd26ed836944e9a5152688cfe",
        "wrong GeneralUser GS fixture"
    );
    path
});
fn decoder() -> Box<dyn Decoder> {
    let mut params = CodecParameters::audio(CodecId::new("midi"));
    params.options.insert("soundfont", FONT.to_str().unwrap());
    codec_midi::make_decoder(&params).unwrap()
}
fn vlq(mut n: u32, out: &mut Vec<u8>) {
    let mut bytes = [0u8; 4];
    let mut pos = 3;
    bytes[pos] = (n & 127) as u8;
    while {
        n >>= 7;
        n != 0
    } {
        pos -= 1;
        bytes[pos] = (n as u8 & 127) | 128;
    }
    out.extend_from_slice(&bytes[pos..]);
}
fn track(events: &[(u32, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut previous = 0;
    for &(tick, bytes) in events {
        vlq(tick - previous, &mut out);
        out.extend_from_slice(bytes);
        previous = tick;
    }
    out
}
fn song(format: u16, division: u16, tracks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"MThd\0\0\0\x06".to_vec();
    out.extend_from_slice(&format.to_be_bytes());
    out.extend_from_slice(&(tracks.len() as u16).to_be_bytes());
    out.extend_from_slice(&division.to_be_bytes());
    for track in tracks {
        out.extend_from_slice(b"MTrk");
        out.extend_from_slice(&(track.len() as u32).to_be_bytes());
        out.extend_from_slice(track);
    }
    out
}
fn piano() -> Vec<u8> {
    track(&[
        (0, &[0xc0, 0]),
        (0, &[0x90, 60, 100]),
        (480, &[0x80, 60, 0]),
        (960, &[0xff, 0x2f, 0]),
    ])
}
fn render(d: &mut dyn Decoder, midi: &[u8]) -> Vec<f32> {
    d.send_packet(&Packet::new(0, TimeBase::new(1, 44100), midi.to_vec()))
        .unwrap();
    d.flush().unwrap();
    let mut out = Vec::new();
    loop {
        match d.receive_frame() {
            Ok(Frame::Audio(a)) => {
                assert!(a.samples <= 1024, "unbounded output frame");
                assert_eq!(a.pts, Some((out.len() / 2) as i64));
                out.extend(
                    a.data[0]
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes(b.try_into().unwrap())),
                );
            }
            Err(Error::Eof) => break,
            other => panic!("unexpected decoder result: {other:?}"),
        }
    }
    out
}
fn snr(a: &[f32], b: &[f32]) -> f64 {
    let (mut signal, mut noise) = (0.0, 0.0);
    for (&x, &y) in a.iter().zip(b) {
        assert!(x.is_finite() && y.is_finite());
        signal += f64::from(y).powi(2);
        noise += (f64::from(x) - f64::from(y)).powi(2);
    }
    assert!(signal > 1e-6, "silent oracle");
    10.0 * (signal / noise).log10()
}
fn oracle(dir: &Path, name: &str, midi: &[u8]) -> Vec<f32> {
    let input = dir.join(format!("{name}.mid"));
    let output = dir.join(format!("{name}.f32"));
    std::fs::write(&input, midi).unwrap();
    let result = Command::new("fluidsynth")
        .args([
            "-ni",
            "-q",
            "-T",
            "raw",
            "-O",
            "float",
            "-r",
            "44100",
            "-z",
            "64",
            "-o",
            "synth.cpu-cores=1",
            "-F",
        ])
        .arg(&output)
        .arg(&*FONT)
        .arg(&input)
        .output()
        .expect("install fluid-synth 2.6.1");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::read(output)
        .unwrap()
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect()
}
fn evidence_dir() -> PathBuf {
    let dir = std::env::var_os("MIDI_EVIDENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("codec-midi-{}", std::process::id())));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn general_midi_matches_fluidsynth() {
    let version = Command::new("fluidsynth")
        .arg("--version")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&version.stdout).contains("2.6.1"),
        "reference must be FluidSynth 2.6.1"
    );
    let dir = evidence_dir();
    let controls = track(&[
        (0, &[0xf0, 5, 0x7e, 0x7f, 9, 1, 0xf7]),
        (0, &[0xc0, 48]),
        (0, &[0xc1, 73]),
        (0, &[0xb0, 91, 90]),
        (0, &[0xb0, 93, 70]),
        (0, &[0x90, 60, 100]),
        (0, &[0x91, 67, 90]),
        (0, &[0x99, 42, 110]),
        (120, &[0xb0, 64, 127]),
        (240, &[0xe1, 0, 80]),
        (360, &[0xb1, 1, 80]),
        (480, &[0x80, 60, 0]),
        (480, &[0x81, 67, 0]),
        (720, &[0xb0, 64, 0]),
        (720, &[0x99, 46, 100]),
        (840, &[0x99, 42, 100]),
        (960, &[0xb1, 121, 0]),
        (960, &[0xc1, 16]),
        (960, &[0x91, 72, 95]),
        (1440, &[0x81, 72, 0]),
        (1600, &[0xff, 0x2f, 0]),
    ]);
    let tuning = track(&[
        (0, &[0xc0, 81]),
        (0, &[0xb0, 101, 0]),
        (0, &[0xb0, 100, 0]),
        (0, &[0xb0, 6, 12]),
        (0, &[0x90, 60, 100]),
        (240, &[0xe0, 0, 70]),
        (360, &[0xa0, 60, 90]),
        (480, &[0xd0, 80]),
        (600, &[0xb0, 7, 60]),
        (720, &[0xb0, 11, 80]),
        (840, &[0xb0, 10, 16]),
        (960, &[0xb0, 66, 127]),
        (1080, &[0x80, 60, 0]),
        (1200, &[0xb0, 66, 0]),
        (1440, &[0xff, 0x2f, 0]),
    ]);
    let tempos = track(&[
        (0, &[0xff, 0x51, 3, 7, 0xa1, 0x20]),
        (240, &[0xff, 0x51, 3, 6, 0x1a, 0x80]),
        (720, &[0xff, 0x51, 3, 9, 0x27, 0xc0]),
        (960, &[0xff, 0x2f, 0]),
    ]);
    let running = track(&[
        (0, &[0xc0, 0]),
        (0, &[0xff, 5, 5, b'h', b'e', b'l', b'l', b'o']),
        (0, &[0x90, 60, 100]),
        (120, &[64, 90]),
        (240, &[60, 0]),
        (360, &[64, 0]),
        (480, &[0xff, 0x2f, 0]),
    ]);
    let short = track(&[
        (0, &[0x90, 60, 100]),
        (1, &[0x80, 60, 0]),
        (480, &[0xff, 0x2f, 0]),
    ]);
    let missing_off = track(&[
        (0, &[0xc0, 48]),
        (0, &[0x90, 60, 100]),
        (480, &[0xff, 0x2f, 0]),
    ]);
    let mut poly = Vec::new();
    for c in 0..4u8 {
        for k in 32..112u8 {
            vlq(0, &mut poly);
            poly.extend_from_slice(&[0x90 | c, k, 75]);
        }
    }
    vlq(480, &mut poly);
    poly.extend_from_slice(&[0xff, 0x2f, 0]);
    let cases = [
        ("piano", song(0, 480, &[piano()])),
        ("controls-drums-effects", song(0, 480, &[controls])),
        ("tuning-pressure", song(0, 480, &[tuning])),
        ("format1-tempo", song(1, 480, &[tempos, piano()])),
        ("karaoke-running-status", song(0, 480, &[running])),
        ("short-note", song(0, 480, &[short])),
        ("missing-noteoff", song(0, 480, &[missing_off])),
        ("voice-stealing", song(0, 480, &[poly])),
    ];
    let mut report = String::from("case\tframes\toracle_frames\tsnr_db\n");
    let mut failures = Vec::new();
    let mut d = decoder();
    for (name, midi) in cases {
        d.reset().unwrap();
        let ours = render(d.as_mut(), &midi);
        let reference = oracle(&dir, name, &midi);
        let value = snr(&ours, &reference);
        eprintln!(
            "MIDI {name}: {} / {} frames, SNR {value:.3} dB",
            ours.len() / 2,
            reference.len() / 2
        );
        report.push_str(&format!(
            "{name}\t{}\t{}\t{value:.6}\n",
            ours.len() / 2,
            reference.len() / 2
        ));
        if std::env::var_os("MIDI_EVIDENCE_DIR").is_some() {
            let bytes: Vec<u8> = ours.iter().flat_map(|v| v.to_le_bytes()).collect();
            std::fs::write(dir.join(format!("{name}-rust.f32")), bytes).unwrap();
        }
        if ours.len() != reference.len() || value < 90.0 {
            failures.push(name);
        }
    }
    std::fs::write(dir.join("reference.tsv"), report).unwrap();
    assert!(failures.is_empty(), "MIDI oracle mismatches: {failures:?}");
}

#[test]
fn format2_and_smpte_match_equivalent_format0() {
    let mut a = piano();
    let b = piano();
    let format2 = song(2, 480, &[a.clone(), b.clone()]);
    // Retain the first track's final delta; replace only EOT with a no-op text event.
    let end = a.len();
    a[end - 2] = 1;
    a.extend_from_slice(&b);
    let equivalent = song(0, 480, &[a]);
    let mut d = decoder();
    let first = render(d.as_mut(), &format2);
    d.reset().unwrap();
    assert_eq!(first, render(d.as_mut(), &equivalent));
    let mut ppq = vec![0, 0xff, 0x51, 3, 0x0f, 0x42, 0x40];
    ppq.extend(piano());
    let smpte = song(0, 0xe728, &[piano()]); // 25 fps * 40 ticks = 1000 ticks/sec.
    d.reset().unwrap();
    let first = render(d.as_mut(), &smpte);
    d.reset().unwrap();
    assert_eq!(first, render(d.as_mut(), &song(0, 1000, &[ppq])));
}

#[test]
fn reset_discards_sounding_voices_and_effect_tails() {
    let midi = song(0, 480, &[piano()]);
    let mut d = decoder();
    let expected = render(d.as_mut(), &midi);
    d.reset().unwrap();
    d.send_packet(&Packet::new(0, TimeBase::new(1, 44100), midi.clone()))
        .unwrap();
    for _ in 0..10 {
        d.receive_frame().unwrap();
    }
    d.reset().unwrap();
    assert_eq!(render(d.as_mut(), &midi), expected);
}

#[test]
fn invalid_midi_fails_without_panicking() {
    let mut d = decoder();
    for data in [
        song(0, 0, &[piano()]),
        song(0, 480, &[track(&[(0, &[0x90, 128, 127])])]),
        song(0, 480, &[track(&[(0, &[0xff, 0x51, 3, 0, 0, 0])])]),
        b"MThd".to_vec(),
    ] {
        d.reset().unwrap();
        assert!(
            d.send_packet(&Packet::new(0, TimeBase::new(1, 44100), data))
                .is_err()
        );
    }
}

#[test]
fn player_requires_a_bank_and_plays_all_smf_extensions() {
    let dir = evidence_dir();
    let midi = song(0, 480, &[piano()]);
    let mut ctx = RuntimeContext::new();
    codec_midi::register(&mut ctx);
    demux_misc::register(&mut ctx);
    let ctx = Arc::new(ctx);
    let path = dir.join("player-no-font.mid");
    std::fs::write(&path, &midi).unwrap();
    let p = player::Player::open(
        path.to_str().unwrap(),
        player::Headless::new(),
        ctx.clone(),
        player::PlayerOptions {
            realtime: false,
            ..Default::default()
        },
        |_| {},
    );
    assert!(
        p.wait()
            .error
            .as_deref()
            .is_some_and(|e| e.contains("needs a SoundFont"))
    );
    drop(p);
    let reference = render(decoder().as_mut(), &midi);
    for extension in ["mid", "midi", "kar"] {
        let path = dir.join(format!("player.{extension}"));
        std::fs::write(&path, &midi).unwrap();
        let backend = player::Headless::new();
        let options = player::PlayerOptions {
            realtime: false,
            soundfont: Some(FONT.clone()),
            ..Default::default()
        };
        let p = player::Player::open(
            path.to_str().unwrap(),
            backend.clone(),
            ctx.clone(),
            options,
            |_| {},
        );
        let state = p.wait();
        assert!(state.ended && state.error.is_none(), "{state:?}");
        drop(p);
        let capture = backend.capture();
        assert_eq!(capture.audio.len(), 1);
        assert_eq!(capture.audio[0].pcm, reference);
    }
}
