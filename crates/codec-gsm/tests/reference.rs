//! codec-gsm against FFmpeg 2da55bf (`-cpuflags 0`). The FATE samples of
//! tests/fate/voice.mak (fate-gsm-ms: gsm/ciao.wav, fate-gsm-toast:
//! gsm/sample-gsm-8000.mov) and a raw `.gsm` made from the second, through
//! the demuxers the player opens them with: FFmpeg's s16 samples byte for
//! byte, and its sample count. The raw demuxer's packets and seeks equal
//! ffprobe's.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use oxideav_core::{CodecId, CodecParameters, Decoder, Frame, MediaType, Packet, RuntimeContext, TimeBase};
use refcheck::{Registrar, decode, fate, pinned_ffmpeg};

fn ffmpeg(args: &[&str]) -> Vec<u8> {
    let out = Command::new(pinned_ffmpeg()).args(["-v", "error", "-nostdin"]).args(args).output().expect("pinned ffmpeg runs");
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn ffprobe_csv(args: &[&str]) -> Vec<Vec<String>> {
    let ffprobe = pinned_ffmpeg().with_file_name("ffprobe");
    let out = Command::new(ffprobe).args(["-v", "error"]).args(args).output().expect("pinned ffprobe runs");
    assert!(out.status.success(), "ffprobe {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split(',').map(str::to_string).collect())
        .collect()
}

/// FFmpeg's decode of the first audio stream as s16le.
fn ffmpeg_s16(path: &Path) -> Vec<u8> {
    ffmpeg(&["-cpuflags", "0", "-i", path.to_str().unwrap(), "-map", "0:a:0", "-f", "s16le", "-"])
}

fn pcm(frames: &[Frame]) -> Vec<u8> {
    frames
        .iter()
        .filter_map(|f| match f {
            Frame::Audio(a) => Some(a.data[0].as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect()
}

/// `path` through the player's demuxer for it and codec-gsm equals FFmpeg:
/// the samples both have, then the count.
fn assert_matches_ffmpeg(path: &Path, registrars: &[Registrar]) -> usize {
    let decoded = decode(path, registrars, MediaType::Audio, 0);
    let format = decoded.audio_format.expect("output format");
    assert_eq!((format.channels, format.sample_rate), (1, 8000), "{}", path.display());
    let ours = pcm(&decoded.frames);
    let theirs = ffmpeg_s16(path);
    let first = ours.iter().zip(&theirs).position(|(a, b)| a != b);
    assert_eq!(first, None, "{}: first differing byte", path.display());
    assert_eq!(ours.len() / 2, theirs.len() / 2, "{}: samples", path.display());
    ours.len() / 2
}

/// A raw `.gsm` of the MOV sample's packets, made once by FFmpeg's `gsm`
/// muxer.
static RAW_GSM: LazyLock<PathBuf> = LazyLock::new(|| {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("codec-gsm-sample-gsm-8000.gsm");
    ffmpeg(&[
        "-y",
        "-i",
        fate("gsm/sample-gsm-8000.mov").to_str().unwrap(),
        "-map",
        "0:a:0",
        "-c:a",
        "copy",
        "-f",
        "gsm",
        path.to_str().unwrap(),
    ]);
    path
});

fn raw_gsm() -> PathBuf {
    RAW_GSM.clone()
}

#[test]
fn ms_gsm_in_wav_matches_ffmpeg() {
    let n = assert_matches_ffmpeg(&fate("gsm/ciao.wav"), &[codec_gsm::register, oxideav_basic::__oxideav_entry]);
    eprintln!("gsm/ciao.wav: {n} samples, bit-exact");
}

fn mov(ctx: &mut RuntimeContext) {
    oxideav_mov::registry::register(ctx);
}

#[test]
fn gsm_in_mov_matches_ffmpeg() {
    let n = assert_matches_ffmpeg(&fate("gsm/sample-gsm-8000.mov"), &[codec_gsm::register, mov]);
    eprintln!("gsm/sample-gsm-8000.mov: {n} samples, bit-exact");
}

#[test]
fn raw_gsm_matches_ffmpeg() {
    let path = raw_gsm();
    let n = assert_matches_ffmpeg(&path, &[codec_gsm::register]);
    eprintln!("raw .gsm: {n} samples, bit-exact");
}

fn open_raw(path: &Path) -> Box<dyn oxideav_core::Demuxer> {
    let mut ctx = RuntimeContext::new();
    codec_gsm::register(&mut ctx);
    assert_eq!(refcheck::probe_container(&ctx, path).as_deref(), Ok("gsm"));
    let file = std::fs::File::open(path).expect("open");
    ctx.containers.open_demuxer("gsm", Box::new(file), &ctx.codecs).expect("open gsm demuxer")
}

#[test]
fn raw_gsm_packets_equal_ffprobe() {
    let path = raw_gsm();
    let expected = ffprobe_csv(&["-select_streams", "a:0", "-show_entries", "packet=pts,size", "-of", "csv=p=0", path.to_str().unwrap()]);
    let mut demuxer = open_raw(&path);
    assert_eq!(demuxer.streams()[0].params.codec_id, CodecId::new("gsm"));
    let mut ours = Vec::new();
    while let Ok(p) = demuxer.next_packet() {
        ours.push(vec![p.pts.unwrap().to_string(), p.data.len().to_string()]);
    }
    assert_eq!(ours.len(), expected.len(), "packet count");
    assert_eq!(ours, expected);
}

#[test]
fn raw_gsm_seeks_land_where_ffprobe_lands() {
    let path = raw_gsm();
    for seconds in ["3", "10.5", "100000"] {
        let first = ffprobe_csv(&[
            "-select_streams",
            "a:0",
            "-read_intervals",
            &format!("{seconds}%+#1"),
            "-show_entries",
            "packet=pts",
            "-of",
            "csv=p=0",
            path.to_str().unwrap(),
        ]);
        let expected: i64 = first[0][0].parse().unwrap();
        let mut demuxer = open_raw(&path);
        let target = (seconds.parse::<f64>().unwrap() * 50.0) as i64;
        let landed = demuxer.seek_to(0, target).expect("seek");
        let next = demuxer.next_packet().expect("packet after seek");
        assert_eq!((landed, next.pts), (expected, Some(expected)), "seek to {seconds} s");
    }
}

/// The MSN Audio rates: Microsoft GSM blocks of 41 to 62 bytes, the
/// decoder's mode from the WAVE header's block_align. No FATE sample has
/// them, so a WAVE file of random blocks (any bits decode) at each size,
/// decoded by FFmpeg, against codec-gsm given the same block_align.
#[test]
fn msn_rates_match_ffmpeg() {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    for block_align in (41..=62).step_by(3) {
        let blocks = 50;
        let payload: Vec<u8> = (0..blocks * block_align)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-gsm-msn-{block_align}.wav"));
        std::fs::write(&path, wave_file(block_align as u16, &payload)).expect("write fixture");
        let theirs = ffmpeg_s16(&path);

        let mut params = CodecParameters::audio(CodecId::new("gsm_ms"));
        params.sample_rate = Some(8000);
        params.options.insert("block_align", block_align.to_string());
        let mut decoder = codec_gsm::GsmDecoder::new(&params).expect("decoder");
        let mut frames = Vec::new();
        for block in payload.chunks(block_align) {
            decoder.send_packet(&Packet::new(0, TimeBase::new(1, 8000), block.to_vec())).expect("send");
            while let Ok(f) = decoder.receive_frame() {
                frames.push(f);
            }
        }
        let ours = pcm(&frames);
        assert_eq!(ours.len(), theirs.len(), "block_align {block_align}: samples");
        assert!(ours == theirs, "block_align {block_align}: samples differ from FFmpeg's");
    }
}

/// A WAVE file of Microsoft GSM blocks (format 0x0031, 8 kHz mono).
fn wave_file(block_align: u16, payload: &[u8]) -> Vec<u8> {
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&0x0031u16.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&8000u32.to_le_bytes());
    fmt.extend_from_slice(&(u32::from(block_align) * 8000 / 320).to_le_bytes());
    fmt.extend_from_slice(&block_align.to_le_bytes());
    fmt.extend_from_slice(&0u16.to_le_bytes());
    fmt.extend_from_slice(&2u16.to_le_bytes());
    fmt.extend_from_slice(&320u16.to_le_bytes());
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((4 + 8 + fmt.len() + 8 + payload.len()) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    out.extend_from_slice(&fmt);
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}
