//! Decodes through the production demuxers compared with FFmpeg
//! 2da55bf's decode of the same file (`$FFMPEG_SRC/ffmpeg`, default
//! ~/projects/ffmpeg-src, run with `-cpuflags 0`: its C code paths).
//!
//! ALAC and MACE are integer decoders: every sample must be equal,
//! compared as signed 32-bit (`-f s32le`, which FFmpeg fills from 16-bit
//! output by shifting 16 bits up), with the same count. QDM2 and QDMC
//! synthesize in floating point before their 16-bit output: SNR of at
//! least 90 dB, counts within one frame.
//!
//! ALAC inputs are the ones FATE uses: `lossless-audio/inside.m4a`
//! (`fate-lossless-alac`), the same packets remuxed to Matroska `A_ALAC`
//! and CAF (`fate-matroska-alac-remux`, `fate-caf-alac-remux`), and FFmpeg's
//! own encodes of the 16-bit and 24-bit reference WAVs at compression
//! levels 0 to 2 and with LPC orders 1 to 30 (`fate-alac-*`), plus mono and
//! 6-channel encodes for the single-channel elements and the channel
//! order table. MACE, QDM2 and QDMC inputs are listed at their tests; FATE
//! has a single QDM2 sample and no QDMC one, so those come from FFmpeg's
//! sample archive too (`tests/data/ffmpeg-samples/README.md`).

mod support;

use std::path::{Path, PathBuf};

use oxideav_core::{Frame, MediaType, SampleFormat};
use refcheck::{decode, fate};
use support::{archive, caf_remux, ffmpeg, generated, pinned, run, track_caf, REGISTRARS};

/// FFmpeg 2da55bf's decode of the first audio stream through its C code
/// paths, as signed 32-bit samples.
fn ffmpeg_s32(path: &Path) -> Vec<i32> {
    let args = ["-cpuflags", "0", "-i", path.to_str().unwrap(), "-map", "0:a:0", "-f", "s32le", "-c:a", "pcm_s32le", "-"];
    run(&pinned("ffmpeg"), &args)
        .chunks_exact(4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// Our decode as signed 32-bit samples, interleaved, in the layout the
/// decoder reported for each frame (16-bit samples shifted 16 bits up).
fn ours_s32(path: &Path) -> (Vec<i32>, oxideav_core::AudioFormat) {
    let decoded = decode(path, &REGISTRARS, MediaType::Audio, 0);
    let format = decoded.audio_format.expect("the decoder reports its layout");
    let channels = format.channels as usize;
    let width = format.sample_format.bytes_per_sample();
    let mut out = Vec::new();
    for (frame, reported) in decoded.frames.iter().zip(&decoded.frame_formats) {
        let Frame::Audio(a) = frame else { continue };
        assert_eq!(*reported, Some(format), "{}: the layout changed", path.display());
        let planar = format.sample_format.is_planar();
        assert_eq!(a.data.len(), if planar { channels } else { 1 }, "{}: planes", path.display());
        for i in 0..a.samples as usize {
            for c in 0..channels {
                let (plane, index) = if planar { (&a.data[c], i) } else { (&a.data[0], i * channels + c) };
                let b = &plane[index * width..index * width + width];
                out.push(match format.sample_format {
                    SampleFormat::S16P | SampleFormat::S16 => i32::from(i16::from_le_bytes([b[0], b[1]])) << 16,
                    SampleFormat::S32P | SampleFormat::S32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                    other => panic!("{}: unexpected {other:?}", path.display()),
                });
            }
        }
    }
    (out, format)
}

/// `path` decodes to FFmpeg's output in a layout of `sample_format`,
/// `rate` and `channels`, with an SNR of at least 90 dB and a sample count
/// within one frame (`frame` samples per channel) of FFmpeg's.
fn assert_snr(path: &Path, sample_format: SampleFormat, rate: u32, channels: u16, frame: usize) {
    let theirs = ffmpeg_s32(path);
    let (ours, format) = ours_s32(path);
    let name = path.file_name().unwrap().to_string_lossy();
    assert_eq!((format.sample_format, format.sample_rate, format.channels), (sample_format, rate, channels), "{name}: layout");
    let f32s = |v: &[i32]| v.iter().map(|&s| s as f32 / 2_147_483_648.0).collect::<Vec<f32>>();
    let snr = refcheck::try_snr_db(&f32s(&theirs), &f32s(&ours), frame * channels as usize)
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    let equal = ours.iter().zip(&theirs).filter(|(a, b)| a == b).count();
    println!(
        "{name}: {format:?}, {} samples per channel, FFmpeg {}; SNR {snr:.2} dB, {equal} of {} samples equal",
        ours.len() / channels as usize,
        theirs.len() / channels as usize,
        ours.len().min(theirs.len())
    );
    assert!(snr >= 90.0, "{name}: SNR {snr:.2} dB below 90 dB");
}

/// Every sample of `path` equals FFmpeg's, in a layout of `sample_format`,
/// `rate` and `channels`, and the counts are equal (or, by `slack` samples
/// per channel, ours is longer: see the CAF ALAC test).
fn assert_bit_exact(path: &Path, sample_format: SampleFormat, rate: u32, channels: u16, slack: usize) {
    let theirs = ffmpeg_s32(path);
    let (ours, format) = ours_s32(path);
    let name = path.file_name().unwrap().to_string_lossy();
    println!("{name}: {format:?}, {} samples per channel, FFmpeg {}", ours.len() / channels as usize, theirs.len() / channels as usize);
    assert_eq!((format.sample_format, format.sample_rate, format.channels), (sample_format, rate, channels), "{name}: layout");
    assert!(!theirs.is_empty(), "{name}: FFmpeg decoded nothing");
    if let Some(i) = ours.iter().zip(&theirs).position(|(a, b)| a != b) {
        panic!("{name}: sample {i} (channel {}) is {}, FFmpeg {}", i % channels as usize, ours[i], theirs[i]);
    }
    assert!(
        ours.len() >= theirs.len() && ours.len() - theirs.len() <= slack * channels as usize,
        "{name}: {} samples, FFmpeg {} (slack {slack} per channel)",
        ours.len(),
        theirs.len()
    );
}

/// An ALAC encode by FFmpeg of `source` (input arguments), as FATE's
/// `enc_dec_pcm mov` makes them.
fn alac_encode(name: &str, input: &[&str], options: &[&str]) -> PathBuf {
    generated(name, |out| {
        let mut args: Vec<&str> = input.to_vec();
        args.extend_from_slice(&["-c:a", "alac"]);
        args.extend_from_slice(options);
        args.extend_from_slice(&["-f", "mov", out.to_str().unwrap()]);
        ffmpeg(&args);
    })
}

fn wav(name: &str) -> String {
    fate(&format!("audio-reference/{name}")).to_str().unwrap().to_string()
}

#[test]
fn alac_inside_m4a() {
    assert_bit_exact(&fate("lossless-audio/inside.m4a"), SampleFormat::S16P, 44100, 2, 0);
}

#[test]
fn alac_remuxed_to_matroska_and_caf() {
    let source = fate("lossless-audio/inside.m4a");
    let mkv = generated("inside.mkv", |out| {
        ffmpeg(&["-i", source.to_str().unwrap(), "-map", "0:a", "-c", "copy", "-f", "matroska", out.to_str().unwrap()]);
    });
    assert_bit_exact(&mkv, SampleFormat::S16P, 44100, 2, 0);
    // FFmpeg's CAF demuxer drops the packet table's remainder frames (11)
    // from the last packet, which the ALAC frame already leaves out: ours
    // keeps the 524277 samples of the MP4, FFmpeg gives 11 fewer, within
    // one frame (4096); every sample both give is equal.
    assert_bit_exact(&caf_remux("inside.caf", &source, &["-map", "0:a"]), SampleFormat::S16P, 44100, 2, 4096);
}

#[test]
fn alac_16_bit_levels_and_lpc_orders() {
    let source = wav("luckynight_2ch_44kHz_s16.wav");
    for (name, options) in [
        ("alac-16-level-0.mov", &["-compression_level", "0"][..]),
        ("alac-16-level-1.mov", &["-compression_level", "1"][..]),
        ("alac-16-level-2.mov", &["-compression_level", "2"][..]),
        ("alac-16-lpc-orders.mov", &["-min_prediction_order", "1", "-max_prediction_order", "30"][..]),
    ] {
        let path = alac_encode(name, &["-i", &source], options);
        assert_bit_exact(&path, SampleFormat::S16P, 44100, 2, 0);
    }
}

/// `fate-alac-24-*`: the reference WAV is 192 kHz, whatever its name says.
#[test]
fn alac_24_bit_levels_and_lpc_orders() {
    let source = wav("divertimenti_2ch_96kHz_s24.wav");
    for (name, options) in [
        ("alac-24-level-0.mov", &["-compression_level", "0"][..]),
        ("alac-24-level-1.mov", &["-compression_level", "1"][..]),
        ("alac-24-level-2.mov", &["-compression_level", "2"][..]),
        ("alac-24-lpc-orders.mov", &["-min_prediction_order", "1", "-max_prediction_order", "30"][..]),
    ] {
        let path = alac_encode(name, &["-i", &source], options);
        assert_bit_exact(&path, SampleFormat::S32P, 192000, 2, 0);
    }
}

#[test]
fn alac_mono_and_six_channels() {
    let source = wav("luckynight_2ch_44kHz_s16.wav");
    let mono = alac_encode("alac-mono.mov", &["-i", &source, "-ac", "1"], &[]);
    assert_bit_exact(&mono, SampleFormat::S16P, 44100, 1, 0);
    // Six different signals in ALAC's 6-channel order (5.1, back).
    let six = alac_encode(
        "alac-5.1.mov",
        &[
            "-f", "lavfi", "-i",
            "aevalsrc=0.5*sin(2*PI*220*t)|0.4*sin(2*PI*(100+400*t)*t)|0.3*sin(2*PI*440*t)*sin(2*PI*3*t)|0.2*sin(2*PI*55*t)|0.5*sin(2*PI*660*t)|0.3*sin(2*PI*(900-100*t)*t):c=5.1:s=44100:d=6",
            "-sample_fmt", "s16p",
        ],
        &[],
    );
    assert_bit_exact(&six, SampleFormat::S16P, 44100, 6, 0);
}

/// MACE: FATE's `fate-qt-mac3-*` and `fate-qt-mac6-*` samples and the
/// MACE 6:1 track of `qtrle/Animation-16Greys.mov`, each remuxed by FFmpeg
/// to CAF ([`track_caf`]) and to AIFF-C.
#[test]
fn mace_3_and_6_mono_and_stereo() {
    for (sample, rate, channels) in [
        ("qt-surge-suite/surge-1-8-MAC3.mov", 44100, 1),
        ("qt-surge-suite/surge-2-8-MAC3.mov", 44100, 2),
        ("qt-surge-suite/surge-1-8-MAC6.mov", 44100, 1),
        ("qt-surge-suite/surge-2-8-MAC6.mov", 44100, 2),
        ("qtrle/Animation-16Greys.mov", 22050, 1),
    ] {
        let source = fate(sample);
        let stem = source.file_stem().unwrap().to_str().unwrap().to_string();
        let caf = track_caf(&source);
        assert_bit_exact(&caf, SampleFormat::S16P, rate, channels, 0);
        let aiff = generated(&format!("{stem}.aiff"), |out| {
            ffmpeg(&["-i", source.to_str().unwrap(), "-map", "0:a", "-c", "copy", "-f", "aiff", out.to_str().unwrap()]);
        });
        assert_bit_exact(&aiff, SampleFormat::S16P, rate, channels, 0);
    }
}

/// The largest frame QDM2 and QDMC emit per packet: 16 frames of at most
/// 512 samples (QDM2), 1 << 13 samples (QDMC).
const FLOAT_FRAME: usize = 8192;

/// QDM2: FATE's only sample (`fate-qdm2`, `fate-caf-qdm2-remux`), and from
/// FFmpeg's sample archive a 0 to 22050 Hz sweep at every bitrate from 8
/// to 64 kb/s (each a different coding configuration), QuickTime's own
/// encode of it, and the `fft8` trailer audio.
#[test]
fn qdm2_fate_and_archive() {
    let surge = track_caf(&fate("qt-surge-suite/surge-2-16-B-QDM2.mov"));
    assert_snr(&surge, SampleFormat::S16, 44100, 2, FLOAT_FRAME);
    for kbps in [8, 10, 12, 16, 20, 24, 32, 40, 48, 64] {
        let sweep = track_caf(&archive(&format!("A-codecs/QDM2/sweep/0-22050HzSweep{kbps}kb.mov")));
        assert_snr(&sweep, SampleFormat::S16, 44100, 1, FLOAT_FRAME);
    }
    let quicktime = track_caf(&archive("A-codecs/QDM2/sweep/0-2222050HzSweep24kbQT.mov"));
    assert_snr(&quicktime, SampleFormat::S16, 44100, 1, FLOAT_FRAME);
    let fft8 = track_caf(&archive("A-codecs/QDM2/fft8/resurrection.mov"));
    assert_snr(&fft8, SampleFormat::S16, 22050, 1, FLOAT_FRAME);
}

/// QDMC: FATE has no sample; FFmpeg's sample archive has three.
#[test]
fn qdmc_archive() {
    for (file, rate, channels) in [
        ("A-codecs/QDMC/rumcoke.mov", 44100, 2),
        ("A-codecs/QDMC/slick.mov", 44100, 2),
        ("A-codecs/QDMC/tidemo1-24bit-rle.mov", 22050, 1),
    ] {
        assert_snr(&track_caf(&archive(file)), SampleFormat::S16, rate, channels, FLOAT_FRAME);
    }
}
