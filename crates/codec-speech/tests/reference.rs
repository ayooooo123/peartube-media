//! Reference tests against FFmpeg 2da55bf (`$FFMPEG_SRC/ffmpeg` and
//! `ffprobe`, default ~/projects/ffmpeg-src), on every FATE sample its
//! makefiles name for these codecs and demuxers (amrnb.mak, amrwb.mak,
//! voice.mak, demux.mak), and on a raw AMR-WB remux FFmpeg makes:
//!
//! - decoders: SNR of at least 90 dB against FFmpeg's C path
//!   (`-cpuflags 0`), the sample count within one frame, the layout FFmpeg
//!   outputs;
//! - demuxers: every packet's size, payload MD5, pts and duration equal
//!   ffprobe's; seeking lands on the packet at or before the target and
//!   reads on as a linear read does.

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oxideav_core::{Demuxer, Error, MediaType, RuntimeContext, SampleFormat, TimeBase};
use refcheck::{decode, fate, interleaved_f32, Registrar};
use support::{raw_amr_wb, run};

/// The containers the player registers for these files, in its order.
const REGISTRARS: [Registrar; 4] =
    [codec_speech::register, oxideav_mov::registry::register, oxideav_mp4::__oxideav_entry, demux_misc::register];

fn ffmpeg_src() -> PathBuf {
    std::env::var_os("FFMPEG_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("projects/ffmpeg-src"))
}


/// FFmpeg 2da55bf's decode of the first audio stream through its C code,
/// as interleaved f32.
fn ffmpeg_f32(path: &Path) -> Vec<f32> {
    let args = ["-nostdin", "-cpuflags", "0", "-i", path.to_str().unwrap(), "-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"];
    run(&ffmpeg_src().join("ffmpeg"), &args).chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
}


/// `path` decodes to FFmpeg's output in the layout FFmpeg gives it, with
/// an SNR of at least 90 dB and a sample count within one `frame`.
fn assert_snr(path: &Path, sample_format: SampleFormat, rate: u32, frame: usize) {
    let theirs = ffmpeg_f32(path);
    let decoded = decode(path, &REGISTRARS, MediaType::Audio, 0);
    let format = decoded.audio_format.expect("the decoder reports its layout");
    let ours = interleaved_f32(&decoded);
    let name = path.file_name().unwrap().to_string_lossy();
    assert_eq!((format.sample_format, format.sample_rate, format.channels), (sample_format, rate, 1), "{name}: layout");
    let snr = refcheck::try_snr_db(&theirs, &ours, frame).unwrap_or_else(|e| panic!("{name}: {e}"));
    let equal = ours.iter().zip(&theirs).filter(|(a, b)| a.to_bits() == b.to_bits()).count();
    println!(
        "{name}: {} samples, FFmpeg {}; SNR {snr:.2} dB, {equal} of {} samples equal",
        ours.len(),
        theirs.len(),
        ours.len().min(theirs.len())
    );
    assert!(snr >= 90.0, "{name}: SNR {snr:.2} dB below 90 dB");
}

const AMRNB: [&str; 8] = ["4.75k", "5.15k", "5.9k", "6.7k", "7.4k", "7.95k", "10.2k", "12.2k"];
const AMRWB: [&str; 10] =
    ["seed-6k60", "seed-8k85", "seed-12k65", "seed-14k25", "seed-15k85", "seed-18k25", "seed-19k85", "seed-23k05", "seed-23k85", "deus-23k85"];

/// amrnb.mak: one file per mode, through the `amr` demuxer.
#[test]
fn amr_nb_fate() {
    for mode in AMRNB {
        assert_snr(&fate(&format!("amrnb/{mode}.amr")), SampleFormat::F32P, 8_000, 160);
    }
}

/// amrwb.mak: every 3GP file, remuxed by FFmpeg to the raw `#!AMR-WB`
/// storage format (`amr` demuxer); see `support::raw_amr_wb` for why not
/// through the MP4 demuxer.
#[test]
fn amr_wb_fate() {
    for name in AMRWB {
        assert_snr(&raw_amr_wb(name), SampleFormat::F32P, 16_000, 320);
    }
}

/// voice.mak's `fate-qcelp`, through the `qcp` demuxer.
#[test]
fn qcelp_fate() {
    assert_snr(&fate("qcp/0036580847.QCP"), SampleFormat::F32, 8_000, 160);
}

// ───────────────────────── demuxers ─────────────────────────

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    for register in REGISTRARS {
        register(&mut ctx);
    }
    ctx
}

fn open(path: &Path, format: &str) -> Box<dyn Demuxer> {
    let ctx = context();
    assert_eq!(refcheck::probe_container(&ctx, path).as_deref(), Ok(format), "{}: probe", path.display());
    ctx.containers.open_demuxer(format, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap()
}

#[derive(Debug, PartialEq)]
struct Pkt {
    size: usize,
    md5: String,
    pts: Option<i64>,
    duration: Option<i64>,
}

/// av_rescale_q, rounding to nearest with ties away from zero.
fn rescale(ts: i64, from: TimeBase, to: TimeBase) -> i64 {
    let num = i128::from(ts) * i128::from(from.num()) * i128::from(to.den());
    let den = i128::from(from.den()) * i128::from(to.num());
    let q = (num.abs() + den / 2) / den;
    (if num < 0 { -q } else { q }) as i64
}

/// Every packet of the demuxer, timed in `to`.
fn ours(demuxer: &mut dyn Demuxer, to: TimeBase) -> Vec<Pkt> {
    let mut packets = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) => packets.push(Pkt {
                size: p.data.len(),
                md5: refcheck::md5_hex(&p.data),
                pts: p.pts.map(|t| rescale(t, p.time_base, to)),
                duration: p.duration.map(|t| rescale(t, p.time_base, to)),
            }),
            Err(Error::Eof) => return packets,
            Err(e) => panic!("demux after {} packets: {e}", packets.len()),
        }
    }
}

/// FFmpeg 2da55bf's ffprobe: the codec, the stream time base and every
/// packet.
fn ffprobe(path: &Path) -> (String, TimeBase, Vec<Pkt>) {
    let out = run(&ffmpeg_src().join("ffprobe"), &[
        "-show_data_hash",
        "md5",
        "-show_entries",
        "stream=codec_name,time_base:packet=pts,duration,size,data_hash",
        "-of",
        "compact",
        path.to_str().unwrap(),
    ]);
    let (mut codec, mut time_base, mut packets) = (String::new(), TimeBase::new(1, 1), Vec::new());
    for line in String::from_utf8(out).unwrap().lines() {
        let mut fields = line.split('|');
        let section = fields.next().unwrap_or("");
        let kv: HashMap<&str, &str> = fields.filter_map(|f| f.split_once('=')).collect();
        let num = |key: &str| kv.get(key).and_then(|v| v.parse::<i64>().ok());
        match section {
            "packet" => packets.push(Pkt {
                size: kv["size"].parse().unwrap(),
                md5: kv["data_hash"].trim_start_matches("MD5:").to_string(),
                pts: num("pts"),
                duration: num("duration"),
            }),
            "stream" => {
                codec = kv["codec_name"].to_string();
                let (n, d) = kv["time_base"].split_once('/').unwrap();
                time_base = TimeBase::new(n.parse().unwrap(), d.parse().unwrap());
            }
            _ => {}
        }
    }
    (codec, time_base, packets)
}

/// `path`'s packets through our `format` demuxer equal ffprobe's, and its
/// codec id is the one OxideAV and FFmpeg's demuxers use for ffprobe's
/// codec name.
fn assert_packets(path: &Path, format: &str) {
    let (codec, time_base, theirs) = ffprobe(path);
    let mut demuxer = open(path, format);
    let id = demuxer.streams()[0].params.codec_id.as_str().to_string();
    assert_eq!(id, codec, "{}: codec id", path.display());
    let ours = ours(demuxer.as_mut(), time_base);
    assert_eq!(ours.len(), theirs.len(), "{}: packet count", path.display());
    if let Some(n) = ours.iter().zip(&theirs).position(|(a, b)| a != b) {
        panic!("{}: packet {n}: ours {:?}, ffprobe {:?}", path.display(), ours[n], theirs[n]);
    }
}

/// amrnb.mak's inputs and `fate-amrnb-remux`'s demuxer; the raw AMR-WB
/// remuxes; demux.mak's `fate-qcp-demux` input and the EVRC file next to it.
#[test]
fn demuxers_match_ffprobe() {
    for mode in AMRNB {
        assert_packets(&fate(&format!("amrnb/{mode}.amr")), "amr");
    }
    for name in AMRWB {
        assert_packets(&raw_amr_wb(name), "amr");
    }
    assert_packets(&fate("qcp/0036580847.QCP"), "qcp");
    assert_packets(&fate("qcp/evrc.qcp"), "qcp");
}

/// Seeking to a sample lands on the packet that contains it, and the
/// packets from there equal a linear read's.
#[test]
fn seeking_lands_on_the_packet_containing_the_target() {
    for (path, format) in [(fate("amrnb/12.2k.amr"), "amr"), (fate("qcp/0036580847.QCP"), "qcp")] {
        let mut demuxer = open(&path, format);
        let tb = demuxer.streams()[0].time_base;
        let all = ours(demuxer.as_mut(), tb);
        for target in [0, 1, 159, 160, 12_345, 80_000, i64::MAX / 2] {
            let landed = demuxer.seek_to(0, target).unwrap();
            let rest = ours(demuxer.as_mut(), tb);
            let index = all.iter().rposition(|p| p.pts.unwrap() <= target).unwrap();
            let index = if target >= all.last().unwrap().pts.unwrap() + 160 { all.len() } else { index };
            assert_eq!(rest.as_slice(), &all[index..], "{}: seek to {target}", path.display());
            assert_eq!(landed, all.get(index).map_or(160 * all.len() as i64, |p| p.pts.unwrap()), "{}: seek to {target}", path.display());
        }
    }
}
