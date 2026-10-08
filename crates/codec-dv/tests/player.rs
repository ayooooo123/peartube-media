//! The Player plays DV through the registry (`codecs::context()`): raw DV
//! with its audio, DV in MXF, type-1 DV AVI and QuickTime's DV audio
//! tracks (`dvca`, `vdva`) give every frame and every audio sample FFmpeg
//! decodes (`-idct simple`), and Ulead DV audio in WAV plays timed by its
//! samples past the 1024 blocks the WAV demuxer once timed as one sample
//! each.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxideav_core::PixelFormat;
use player::{Event, Headless, Player, PlayerOptions};
use refcheck::fate;

/// Plays `path` to its end with the default tracks; the capture.
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
            Err(error) => panic!("{}: playback did not end: {error}: {:?}", path.display(), player.state()),
        }
    }
    let state = player.state();
    assert!(state.error.is_none(), "{}: {:?}", path.display(), state.error);
    drop(player);
    backend.capture()
}

fn check(path: &Path) {
    let name = path.display().to_string();
    let capture = play(path);
    let video = capture.video.first().expect("a video stream played");
    let pix_fmt = match video.pixel_format {
        PixelFormat::Yuv420P => "yuv420p",
        PixelFormat::Yuv411P => "yuv411p",
        PixelFormat::Yuv422P => "yuv422p",
        other => panic!("{name}: {other:?}"),
    };
    let want = refcheck::ffmpeg_video_md5s_with(path, 0, pix_fmt, &["-idct", "simple"]);
    assert!(!want.is_empty(), "{name}: FFmpeg's frames");
    assert_eq!(video.frame_md5, want, "{name}: the frames equal FFmpeg's");
    let audio = capture.audio.first().expect("an audio stream played");
    let reference = refcheck::ffmpeg_audio_f32(path, 0);
    assert_eq!(audio.pcm.len(), reference.len(), "{name}: audio samples");
    let snr = refcheck::snr_db(&reference, &audio.pcm, 0);
    assert!(snr.is_infinite(), "{name}: the audio equals FFmpeg's ({snr} dB)");
}

fn made(name: &str, args: &[&str]) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-dv-player-{}-{name}", std::process::id()));
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y"])
        .args(args)
        .arg(&path)
        .output()
        .expect("ffmpeg on PATH");
    assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    path
}

#[test]
fn raw_dv_with_audio_plays_as_ffmpeg() {
    let path = made(
        "ntsc.dv",
        &[
            "-f", "lavfi", "-i", "testsrc=size=720x480:rate=30000/1001:duration=2", "-f", "lavfi", "-i",
            "sine=frequency=1000:sample_rate=48000:duration=2", "-c:v", "dvvideo", "-pix_fmt", "yuv411p", "-c:a", "pcm_s16le",
            "-ac", "2", "-f", "dv",
        ],
    );
    check(&path);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn dv_in_mxf_plays_as_ffmpeg() {
    check(&fate("mxf/Avid-00005.mxf"));
}

/// A Ulead DV-audio WAV (WAVE tag 0x0216) of 45 s: per DV frame, the nine
/// audio DIF blocks of each of its 12 DIF sequences.
fn ulead_wav_45s() -> PathBuf {
    let dv = made(
        "ulead-src.dv",
        &[
            "-f", "lavfi", "-i", "testsrc=size=720x576:rate=25:duration=45", "-f", "lavfi", "-i",
            "sine=frequency=1000:sample_rate=48000:duration=45", "-c:v", "dvvideo", "-pix_fmt", "yuv420p", "-c:a", "pcm_s16le",
            "-ac", "2", "-f", "dv",
        ],
    );
    let frames = std::fs::read(&dv).unwrap();
    let _ = std::fs::remove_file(&dv);
    let mut data = Vec::new();
    for frame in frames.chunks_exact(144_000) {
        for seq in 0..12 {
            for blk in 0..9 {
                let at = seq * 150 * 80 + (6 + 16 * blk) * 80;
                data.extend_from_slice(&frame[at..at + 80]);
            }
        }
    }
    let block_align = 12 * 9 * 80u32;
    let mut wav = b"RIFF".to_vec();
    wav.extend_from_slice(&(4 + 8 + 16 + 8 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    for v in [0x0216u16, 2] {
        wav.extend_from_slice(&v.to_le_bytes());
    }
    wav.extend_from_slice(&48_000u32.to_le_bytes());
    wav.extend_from_slice(&(block_align * 25).to_le_bytes());
    wav.extend_from_slice(&(block_align as u16).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-dv-player-{}-ulead45.wav", std::process::id()));
    std::fs::write(&path, wav).unwrap();
    path
}

/// 45 s of Ulead DV audio (1125 blocks): every write the engine makes is
/// stamped at the samples before it, and the audio equals FFmpeg's.
#[test]
fn ulead_dv_audio_in_wav_plays_timed_by_its_samples() {
    let path = ulead_wav_45s();
    let capture = play(&path);
    let audio = capture.audio.first().expect("an audio stream played");
    assert_eq!((audio.sample_rate, audio.channels), (48_000, 2));
    let late: Vec<_> = audio
        .writes
        .iter()
        .filter(|(pts, start)| (pts.as_secs_f64() - *start as f64 / 2.0 / 48_000.0).abs() > 0.001)
        .collect();
    assert!(audio.writes.len() > 100, "{} writes", audio.writes.len());
    assert!(late.is_empty(), "writes off their samples' time: {:?}", &late[..late.len().min(4)]);
    let reference = refcheck::ffmpeg_audio_f32(&path, 0);
    assert_eq!(audio.pcm.len(), reference.len(), "audio samples");
    let snr = refcheck::snr_db(&reference, &audio.pcm, 0);
    assert!(snr.is_infinite(), "the audio equals FFmpeg's ({snr} dB)");
    let _ = std::fs::remove_file(&path);
}

/// The whole DIF frames of a DV file `made` (as `<test>-src.dv`, one per
/// test as they run in parallel) from 2 s of `testsrc` and a 1 kHz tone:
/// 525/60 (720x480, 120000-byte frames) or 625/50.
fn dv_frames(test: &str, ntsc: bool) -> Vec<u8> {
    let video = if ntsc {
        "testsrc=size=720x480:rate=30000/1001:duration=2"
    } else {
        "testsrc=size=720x576:rate=25:duration=2"
    };
    let pix_fmt = if ntsc { "yuv411p" } else { "yuv420p" };
    let dv = made(
        &format!("{test}-src.dv"),
        &[
            "-f", "lavfi", "-i", video, "-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000:duration=2", "-c:v", "dvvideo",
            "-pix_fmt", pix_fmt, "-c:a", "pcm_s16le", "-ac", "2", "-f", "dv",
        ],
    );
    let bytes = std::fs::read(&dv).unwrap();
    let _ = std::fs::remove_file(&dv);
    bytes
}

fn riff_chunk(fcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let pad = if body.len() % 2 == 1 { vec![0] } else { Vec::new() };
    [fcc.to_vec(), (body.len() as u32).to_le_bytes().to_vec(), body.to_vec(), pad].concat()
}

fn riff_list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    riff_chunk(b"LIST", &[kind.as_slice(), body].concat())
}

fn le32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// A type-1 DV AVI: one `iavs` stream (handler `dvsd`) of the frames of
/// `dv` (`frame` bytes each) as `00__` chunks, `rate` frames a second.
fn type1_avi(dv: &[u8], frame: usize, rate: (u32, u32), (w, h): (u32, u32)) -> Vec<u8> {
    let frames: Vec<&[u8]> = dv.chunks_exact(frame).collect();
    let n = frames.len() as u32;
    let frame_us = (1_000_000 * u64::from(rate.1) / u64::from(rate.0)) as u32;
    let avih = le32(&[frame_us, frame as u32 * 30, 0, 0x110, n, 0, 1, frame as u32 + 8, w, h, 0, 0, 0, 0]);
    let mut strh = b"iavsdvsd".to_vec();
    strh.extend(le32(&[0, 0, 0, rate.1, rate.0, 0, n, frame as u32, u32::MAX, 0]));
    strh.extend([0i16, 0, w as i16, h as i16].iter().flat_map(|v| v.to_le_bytes()));
    let strl = [riff_chunk(b"strh", &strh), riff_chunk(b"strf", &[0; 32])].concat();
    let hdrl = riff_list(b"hdrl", &[riff_chunk(b"avih", &avih), riff_list(b"strl", &strl)].concat());
    let (mut movi, mut idx1) = (Vec::new(), Vec::new());
    for f in frames {
        idx1.extend(b"00__");
        idx1.extend(le32(&[0x10, 4 + movi.len() as u32, f.len() as u32]));
        movi.extend(riff_chunk(b"00__", f));
    }
    let body = [b"AVI ".to_vec(), hdrl, riff_list(b"movi", &movi), riff_chunk(b"idx1", &idx1)].concat();
    [b"RIFF".to_vec(), (body.len() as u32).to_le_bytes().to_vec(), body].concat()
}

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    [(8 + body.len() as u32).to_be_bytes().to_vec(), kind.to_vec(), body.to_vec()].concat()
}

fn full_atom(kind: &[u8; 4], flags: u32, body: &[u8]) -> Vec<u8> {
    atom(kind, &[flags.to_be_bytes().as_slice(), body].concat())
}

fn be32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// A QuickTime file of the frames of `dv`: a DV video track and a sound
/// track of sample entry `audio` (`dvca`, `vdva`) whose samples are the
/// same frames, `samples[i]` audio samples each at `sample_rate`.
fn dv_mov(dv: &[u8], frame: usize, rate: (u32, u32), (w, h): (u16, u16), audio: &[u8; 4], sample_rate: u32, samples: &[u32]) -> Vec<u8> {
    let n = dv.len() / frame;
    let ftyp = atom(b"ftyp", &[b"qt  ".as_slice(), &0x200u32.to_be_bytes(), b"qt  "].concat());
    let first = ftyp.len() + 8;
    let mdat = atom(b"mdat", &dv[..n * frame]);
    let offsets: Vec<u32> = (0..n).map(|i| (first + i * frame) as u32).collect();
    let stbl = |entry: Vec<u8>, stts: &[(u32, u32)]| {
        let stts: Vec<u8> = [be32(&[stts.len() as u32]), stts.iter().flat_map(|&(c, d)| be32(&[c, d])).collect()].concat();
        atom(
            b"stbl",
            &[
                full_atom(b"stsd", 0, &[be32(&[1]), entry].concat()),
                full_atom(b"stts", 0, &stts),
                full_atom(b"stsc", 0, &be32(&[1, 1, 1, 1])),
                full_atom(b"stsz", 0, &be32(&[frame as u32, n as u32])),
                full_atom(b"stco", 0, &[be32(&[n as u32]), be32(&offsets)].concat()),
            ]
            .concat(),
        )
    };
    let dinf = atom(b"dinf", &full_atom(b"dref", 0, &[be32(&[1]), full_atom(b"url ", 1, &[])].concat()));
    let movie_duration = (n as u64 * 600 * u64::from(rate.1) / u64::from(rate.0)) as u32;
    let matrix = be32(&[0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000]);
    let tkhd = |id: u32, volume: u16, tw: u32, th: u32| {
        let layer: Vec<u8> = [0u16, 0, volume, 0].iter().flat_map(|v| v.to_be_bytes()).collect();
        full_atom(b"tkhd", 0xf, &[be32(&[0, 0, id, 0, movie_duration]), vec![0; 8], layer, matrix.clone(), be32(&[tw << 16, th << 16])].concat())
    };
    let mdhd = |timescale: u32, duration: u32| full_atom(b"mdhd", 0, &[be32(&[0, 0, timescale, duration]), vec![0; 4]].concat());
    let hdlr = |kind: &[u8; 4]| full_atom(b"hdlr", 0, &[b"mhlr".as_slice(), kind, &[0; 13]].concat());
    let mut video_entry = [vec![0; 6], vec![0, 1, 0, 0, 0, 0], b"appl".to_vec(), be32(&[0, 0x200])].concat();
    video_entry.extend([w, h].iter().flat_map(|v| v.to_be_bytes()));
    video_entry.extend(be32(&[72 << 16, 72 << 16, 0]));
    video_entry.extend([0, 1]);
    video_entry.extend([0; 32]);
    video_entry.extend([0, 24, 0xff, 0xff]);
    let video_kind = if h == 480 { b"dvc " } else { b"dvcp" };
    let video = atom(
        b"trak",
        &[
            tkhd(1, 0, u32::from(w), u32::from(h)),
            atom(
                b"mdia",
                &[
                    mdhd(rate.0, n as u32 * rate.1),
                    hdlr(b"vide"),
                    atom(b"minf", &[full_atom(b"vmhd", 1, &[0; 8]), dinf.clone(), stbl(atom(video_kind, &video_entry), &[(n as u32, rate.1)])].concat()),
                ]
                .concat(),
            ),
        ]
        .concat(),
    );
    let audio_entry = [vec![0; 6], vec![0, 1, 0, 0, 0, 0], vec![0; 4], vec![0, 2, 0, 16, 0, 0, 0, 0], (sample_rate << 16).to_be_bytes().to_vec()].concat();
    let mut stts: Vec<(u32, u32)> = Vec::new();
    for &d in &samples[..n] {
        match stts.last_mut() {
            Some((count, delta)) if *delta == d => *count += 1,
            _ => stts.push((1, d)),
        }
    }
    let sound = atom(
        b"trak",
        &[
            tkhd(2, 0x100, 0, 0),
            atom(
                b"mdia",
                &[
                    mdhd(sample_rate, samples[..n].iter().sum()),
                    hdlr(b"soun"),
                    atom(b"minf", &[full_atom(b"smhd", 0, &[0; 4]), dinf, stbl(atom(audio, &audio_entry), &stts)].concat()),
                ]
                .concat(),
            ),
        ]
        .concat(),
    );
    let mvhd = full_atom(
        b"mvhd",
        0,
        &[be32(&[0, 0, 600, movie_duration, 0x10000]), vec![1, 0], vec![0; 10], matrix, vec![0; 24], be32(&[3])].concat(),
    );
    [ftyp, mdat, atom(b"moov", &[mvhd, video, sound].concat())].concat()
}

fn written(name: &str, bytes: &[u8]) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-dv-player-{}-{name}", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    path
}

/// 525/60 with 48 kHz audio: frames of 1600 and 1602 samples, which the
/// AVI's frame-counted time base cannot place, played back to back.
#[test]
fn type1_dv_avi_plays_as_ffmpeg() {
    let path = written("type1.avi", &type1_avi(&dv_frames("type1", true), 120_000, (30_000, 1001), (720, 480)));
    check(&path);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn dvca_track_in_mov_plays_as_ffmpeg() {
    let mov = dv_mov(&dv_frames("dvca", false), 144_000, (25, 1), (720, 576), b"dvca", 48_000, &[1920; 50]);
    let path = written("dvca.mov", &mov);
    check(&path);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn vdva_track_in_mov_plays_as_ffmpeg() {
    let samples: Vec<u32> = [1600, 1602, 1602, 1602, 1602].repeat(12);
    let mov = dv_mov(&dv_frames("vdva", true), 120_000, (30_000, 1001), (720, 480), b"vdva", 48_000, &samples);
    let path = written("vdva.mov", &mov);
    check(&path);
    let _ = std::fs::remove_file(&path);
}
