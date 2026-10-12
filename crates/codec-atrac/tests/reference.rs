//! codec-atrac against the FFmpeg it ports (commit 2da55bf,
//! `refcheck::pinned_ffmpeg`, C code paths via `-cpuflags 0`).
//!
//! The decoders get the packets FFmpeg's demuxers give FFmpeg's decoders
//! and must emit FFmpeg's frames, refuse the packets FFmpeg refuses, and
//! come within 90 dB SNR of FFmpeg's samples. The AEA and OMA demuxers must
//! emit FFmpeg's packets, and the whole path, demuxer to samples, must
//! match FFmpeg's decode.

use check_decoders::{decode_packets, ffmpeg_packets, tool};
use oxideav_core::{CodecId, CodecParameters, Demuxer, Error, Frame, RuntimeContext, SampleFormat};

/// FATE's ATRAC samples (tests/fate/atrac.mak, oma.mak and the aea / oma
/// directories).
const DECODE_SAMPLES: &[&str] = &[
    "atrac1/test_tones_small.aea",
    "atrac1/chirp_tone_10-16000.aea",
    "aea/chirp.aea",
    "atrac3/mc_sich_at3_066_small.wav",
    "atrac3/mc_sich_at3_105_small.wav",
    "atrac3/mc_sich_at3_132_small.wav",
    "oma/01-Untitled-partial.oma",
    "atrac3p/at3p_sample1.oma",
    "atrac3p/sonateno14op27-2-cut.aa3",
];

const DEMUX_SAMPLES: &[(&str, &str)] = &[
    ("atrac1/test_tones_small.aea", "aea"),
    ("atrac1/chirp_tone_10-16000.aea", "aea"),
    ("aea/chirp.aea", "aea"),
    ("oma/01-Untitled-partial.oma", "oma"),
    ("atrac3p/at3p_sample1.oma", "oma"),
    ("atrac3p/sonateno14op27-2-cut.aa3", "oma"),
];

fn ffprobe(args: &[&str]) -> String {
    String::from_utf8(tool(
        refcheck::pinned_ffprobe(),
        &[&["-v", "error", "-cpuflags", "0"][..], args].concat(),
    ))
    .expect("UTF-8")
}

/// The `block_align` FFmpeg's demuxer sets (ffprobe does not print it):
/// the WAV `fmt ` chunk's nBlockAlign, 212 bytes per channel in AEA
/// (aeadec.c), or the frame size in the OMA EA3 header (omadec.c).
fn block_align(path: &str) -> usize {
    let data = std::fs::read(path).unwrap();
    if data.starts_with(b"RIFF") {
        let fmt = data
            .windows(4)
            .position(|w| w == b"fmt ")
            .expect("fmt chunk")
            + 8;
        return usize::from(u16::from_le_bytes([data[fmt + 12], data[fmt + 13]]));
    }
    if data.starts_with(&[0, 8, 0, 0]) {
        return 212 * usize::from(data[264]);
    }
    let ea3 = data
        .windows(6)
        .position(|w| &w[..3] == b"EA3" && w[4] == 0 && w[5] == 96)
        .expect("EA3 header");
    let params = u32::from_be_bytes([0, data[ea3 + 33], data[ea3 + 34], data[ea3 + 35]]);
    let framesize = (params & 0x3FF) as usize * 8;
    if data[ea3 + 32] == 1 {
        framesize + 8
    } else {
        framesize
    }
}

/// FFmpeg's stream parameters: (codec name, sample rate, channels,
/// block_align, extradata).
fn stream_params(path: &str) -> (String, u32, u16, usize, Vec<u8>) {
    let text = ffprobe(&[
        "-select_streams",
        "a:0",
        "-show_streams",
        "-show_data",
        path,
    ]);
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("{path}: no {name}"))
            .to_string()
    };
    // `extradata=` is followed by a hexdump: "00000000: 0100 0010 ...  ascii"
    let mut extradata = Vec::new();
    let mut lines = text.lines().skip_while(|l| *l != "extradata=").skip(1);
    while let Some(line) = lines
        .next()
        .filter(|l| l.len() > 10 && l.as_bytes()[8] == b':')
    {
        let hex = &line[10..line.find("  ").unwrap_or(line.len())];
        for group in hex.split_whitespace() {
            for k in (0..group.len()).step_by(2) {
                extradata.push(u8::from_str_radix(&group[k..k + 2], 16).expect("hex"));
            }
        }
    }
    (
        field("codec_name"),
        field("sample_rate").parse().unwrap(),
        field("channels").parse().unwrap(),
        block_align(path),
        extradata,
    )
}

/// FFmpeg's frames: (samples per channel, channels).
fn ffmpeg_frames(path: &str) -> Vec<(u32, u16)> {
    ffprobe(&[
        "-select_streams",
        "a:0",
        "-show_entries",
        "frame=nb_samples,channels",
        "-of",
        "csv=p=0",
        path,
    ])
    .lines()
    .filter(|l| !l.trim().is_empty())
    .map(|l| {
        let mut f = l
            .split(',')
            .map(|v| v.trim().parse::<u32>().expect("number"));
        (f.next().unwrap(), f.next().unwrap() as u16)
    })
    .collect()
}

/// FFmpeg's decode, interleaved f32, and the packets its decoder refused.
fn ffmpeg_pcm(path: &str) -> (Vec<f32>, usize) {
    let out = std::process::Command::new(refcheck::pinned_ffmpeg())
        .args([
            "-v",
            "error",
            "-nostdin",
            "-cpuflags",
            "0",
            "-i",
            path,
            "-map",
            "0:a:0",
            "-f",
            "f32le",
            "-c:a",
            "pcm_f32le",
            "-",
        ])
        .output()
        .expect("pinned ffmpeg runs");
    assert!(
        out.status.success(),
        "{path}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // ffmpeg logs a refused packet as one or the other, by when it saw it
    let log = String::from_utf8_lossy(&out.stderr);
    let refused = log.matches("Error submitting packet to decoder").count()
        + log.matches("] Decoding error").count();
    (
        out.stdout
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect(),
        refused,
    )
}

fn interleave(frames: &[Frame]) -> Vec<f32> {
    let mut pcm = Vec::new();
    for frame in frames {
        let Frame::Audio(a) = frame else {
            panic!("not audio")
        };
        let planes: Vec<Vec<f32>> = a
            .data
            .iter()
            .map(|p| {
                p.chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect()
            })
            .collect();
        for i in 0..a.samples as usize {
            pcm.extend(planes.iter().map(|p| p[i]));
        }
    }
    pcm
}

/// Each decoder on FFmpeg's packets: FFmpeg's frames and refused packets,
/// and its samples within 90 dB.
#[test]
fn decoders_match_ffmpeg_on_its_packets() {
    let mut failures = Vec::new();
    for &sample in DECODE_SAMPLES {
        let path = refcheck::fate(sample);
        let p = path.to_str().unwrap();
        let (codec, rate, channels, block_align, extradata) = stream_params(p);
        let mut params = CodecParameters::audio(CodecId::new(codec.as_str()));
        params.sample_rate = Some(rate);
        params.channels = Some(channels);
        params.extradata = extradata;
        params
            .options
            .insert("block_align", block_align.to_string());
        let packets = ffmpeg_packets(&path, "a:0", None);
        let (decoded, errors) = decode_packets(&[codec_atrac::register], &params, &packets);

        let ours: Vec<(u32, u16)> = decoded
            .frames
            .iter()
            .zip(&decoded.frame_formats)
            .map(|(f, fmt)| match (f, fmt) {
                (Frame::Audio(a), Some(fmt)) if fmt.sample_format == SampleFormat::F32P => {
                    (a.samples, fmt.channels)
                }
                _ => (0, 0),
            })
            .collect();
        let (reference, refused) = ffmpeg_pcm(p);
        let theirs = ffmpeg_frames(p);
        if ours != theirs || errors.len() != refused {
            let first = ours.iter().zip(&theirs).position(|(a, b)| a != b);
            failures.push(format!(
                "{sample}: {} frames vs FFmpeg {}, first differing frame {first:?}, refused {} vs FFmpeg {refused}: {:?}",
                ours.len(),
                theirs.len(),
                errors.len(),
                errors.first()
            ));
            continue;
        }
        let snr = refcheck::snr_db(&reference, &interleave(&decoded.frames), 0);
        eprintln!(
            "{sample}: {} frames, {} refused, SNR {snr:.1} dB",
            ours.len(),
            errors.len()
        );
        if snr < 90.0 {
            failures.push(format!("{sample}: SNR {snr:.1} dB < 90"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    codec_atrac::register(&mut ctx);
    ctx
}

/// One packet as `ffprobe -show_packets -show_data_hash md5` lists it.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<i64>,
    size: usize,
    key: bool,
    md5: String,
}

fn ffprobe_rows(path: &str) -> Vec<Row> {
    let num = |v: &str| v.parse::<i64>().ok();
    ffprobe(&[
        "-show_entries",
        "packet=pts,dts,duration,size,flags,data_hash",
        "-show_data_hash",
        "md5",
        "-of",
        "csv=p=0",
        path,
    ])
    .lines()
    .filter(|l| !l.trim().is_empty())
    .map(|l| {
        let f: Vec<&str> = l.split(',').collect();
        Row {
            pts: num(f[0]),
            dts: num(f[1]),
            duration: num(f[2]),
            size: f[3].parse().unwrap(),
            key: f[4].starts_with('K'),
            md5: f[5].trim_start_matches("MD5:").to_string(),
        }
    })
    .collect()
}

/// The AEA and OMA demuxers: picked by the probe, and FFmpeg's packets,
/// timestamps, durations and payloads, in FFmpeg's stream layout.
#[test]
fn demuxers_emit_ffmpegs_packets() {
    let ctx = context();
    for &(sample, format) in DEMUX_SAMPLES {
        let path = refcheck::fate(sample);
        let p = path.to_str().unwrap();
        assert_eq!(
            refcheck::probe_container(&ctx, &path).as_deref(),
            Ok(format),
            "{sample}"
        );
        let mut demuxer: Box<dyn Demuxer> = ctx
            .containers
            .open_demuxer(
                format,
                Box::new(std::fs::File::open(&path).unwrap()),
                &ctx.codecs,
            )
            .unwrap_or_else(|e| panic!("{sample}: open: {e}"));

        let (codec, rate, channels, block_align, extradata) = stream_params(p);
        let params = &demuxer.streams()[0].params;
        let expect_codec = if codec == "atrac3p" {
            "atrac3plus"
        } else {
            codec.as_str()
        };
        assert_eq!(
            (
                params.codec_id.as_str(),
                params.sample_rate,
                params.channels,
                params.options.get("block_align"),
                &params.extradata
            ),
            (
                expect_codec,
                Some(rate),
                Some(channels),
                Some(block_align.to_string().as_str()),
                &extradata
            ),
            "{sample}: stream parameters"
        );

        let mut ours = Vec::new();
        loop {
            match demuxer.next_packet() {
                Ok(pkt) => ours.push(Row {
                    pts: pkt.pts,
                    dts: pkt.dts,
                    duration: pkt.duration,
                    size: pkt.data.len(),
                    key: pkt.flags.keyframe,
                    md5: refcheck::md5_hex(&pkt.data),
                }),
                Err(Error::Eof) => break,
                Err(e) => panic!("{sample}: demux after {} packets: {e}", ours.len()),
            }
        }
        let theirs = ffprobe_rows(p);
        let first = ours.iter().zip(&theirs).position(|(a, b)| a != b);
        assert!(
            ours.len() == theirs.len() && first.is_none(),
            "{sample}: {} packets vs FFmpeg {}, first difference at {first:?}: {:?} vs {:?}",
            ours.len(),
            theirs.len(),
            first.map(|i| &ours[i]),
            first.map(|i| &theirs[i])
        );
    }
}

/// Demuxer and decoder together, as the player runs them: FFmpeg's decode
/// within 90 dB and exactly its length (a decode error, as on a cut last
/// packet, skips that packet as FFmpeg does).
#[test]
fn aea_and_oma_files_decode_like_ffmpeg() {
    let ctx = context();
    for &(sample, format) in DEMUX_SAMPLES {
        let path = refcheck::fate(sample);
        let mut demuxer = ctx
            .containers
            .open_demuxer(
                format,
                Box::new(std::fs::File::open(&path).unwrap()),
                &ctx.codecs,
            )
            .unwrap();
        let params = demuxer.streams()[0].params.clone();
        let mut packets = Vec::new();
        loop {
            match demuxer.next_packet() {
                Ok(p) => packets.push(p),
                Err(Error::Eof) => break,
                Err(e) => panic!("{sample}: demux: {e}"),
            }
        }
        let (decoded, _) = decode_packets(&[codec_atrac::register], &params, &packets);
        let ours = interleave(&decoded.frames);
        let (reference, _) = ffmpeg_pcm(path.to_str().unwrap());
        let channels = usize::from(params.channels.unwrap());
        let snr = refcheck::snr_db(&reference, &ours, 0);
        eprintln!(
            "{sample}: {} samples, SNR {snr:.1} dB",
            ours.len() / channels
        );
        assert!(snr >= 90.0, "{sample}: SNR {snr:.1} dB");
    }
}

/// A seek lands on the packet at or before the target, as
/// `ff_pcm_read_seek` with AVSEEK_FLAG_BACKWARD does, and reading goes on
/// from there.
#[test]
fn seeks_land_on_the_block_at_or_before_the_target() {
    let ctx = context();
    for (sample, format, target, landed) in [
        // ffmpeg -ss 1.0 lands 43 samples before 44,100 in chirp.aea; the
        // OMA landings are the pts ffprobe lists at those byte positions
        ("aea/chirp.aea", "aea", 44_100i64, 44_057i64),
        ("atrac3p/at3p_sample1.oma", "oma", 100_000, 98_305),
        ("oma/01-Untitled-partial.oma", "oma", 300_000, 299_017),
    ] {
        let path = refcheck::fate(sample);
        let mut demuxer = ctx
            .containers
            .open_demuxer(
                format,
                Box::new(std::fs::File::open(&path).unwrap()),
                &ctx.codecs,
            )
            .unwrap();
        assert_eq!(demuxer.seek_to(0, target).unwrap(), landed, "{sample}");
        assert_eq!(demuxer.next_packet().unwrap().pts, Some(landed), "{sample}");
    }
}
