//! Seeking lands where FFmpeg's seek lands. For every format, two or more
//! targets (mid-block or mid-GOP among them): after `seek_to`, the first
//! packets equal those `ffprobe -read_intervals TARGET%+#N` of the port's
//! FFmpeg revision (FFMPEG_SRC/ffprobe, 2da55bf) returns. ffprobe seeks
//! like `ffmpeg -ss`: avformat_seek_file on the default stream with
//! AVSEEK_FLAG_BACKWARD, the target rescaled from microseconds with
//! av_rescale. The seek gets the same stream and timestamp here.
//!
//! FFmpeg's paths (libavformat, 2da55bf): CAF and VOC have read_seek
//! (cafdec.c read_seek, vocdec.c voc_read_seek, which falls back to
//! seek.c seek_frame_generic); NUT read_seek (nutdec.c); MPEG-PS and PVA
//! bisect with their read_timestamp (seek.c ff_seek_frame_binary /
//! ff_gen_search over mpeg.c mpegps_read_dts, pva.c pva_read_timestamp);
//! raw AC-3/E-AC-3, raw MPEG video and IVF are AVFMT_GENERIC_INDEX
//! (seek.c seek_frame_generic over the key packets read so far).
//!
//! Packets compare as the reference tests compare them: stream, size,
//! payload MD5, pts and dts rescaled to FFmpeg's time base; durations and
//! key flags where the packet tables do. PVA and NUT compare with
//! FFmpeg's demuxer output (`-fflags +noparse+nofillin`); MPEG-PS with
//! the parsed table for the streams it re-frames and the PES table for
//! the others.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use oxideav_core::{Demuxer, Error, MediaType, StreamInfo, TimeBase};

fn ffmpeg_src() -> PathBuf {
    std::env::var_os("FFMPEG_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&std::env::var("HOME").unwrap()).join("projects/ffmpeg-src"))
}

/// The port's ffprobe, checked once to be revision 2da55bf.
fn port_ffprobe() -> &'static Path {
    static PORT: LazyLock<PathBuf> = LazyLock::new(|| {
        let bin = ffmpeg_src().join("ffprobe");
        let out = Command::new(&bin).arg("-version").output().expect("build ffprobe in FFMPEG_SRC: make ffprobe");
        let version = String::from_utf8_lossy(&out.stdout);
        assert!(version.contains("2da55bf"), "seek oracle must be FFmpeg 2da55bf: {version}");
        bin
    });
    &PORT
}

/// `name` in the scratch directory Cargo gives integration tests, made by
/// FFmpeg's `ffmpeg` (on PATH) from `args` once per test binary.
fn generated(name: &str, args: &[&str]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("demux-misc-seek-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    if !path.exists() {
        let out = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y"])
            .args(args)
            .arg(&path)
            .output()
            .expect("ffmpeg must be on PATH");
        assert!(out.status.success(), "{name}: ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    }
    path
}

#[derive(Clone, Debug, PartialEq)]
struct Pkt {
    stream: u32,
    size: usize,
    md5: String,
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<i64>,
    key: bool,
}

/// What a comparison covers beyond stream, size, payload and timestamps.
#[derive(Clone, Copy, Default)]
struct Mode {
    /// FFmpeg's demuxer output (`-fflags +noparse+nofillin`).
    unparsed: bool,
    durations: bool,
    keys: bool,
    /// FFmpeg reads as little ahead at open as it can (`-probesize 32
    /// -analyzeduration 0`): its index then holds what this demuxer's
    /// does, which decides a search over unordered timestamps.
    no_read_ahead: bool,
}

/// The time bases of `path`'s streams and the first `n` packets after
/// FFmpeg seeks it to `target` seconds; `Err` when FFmpeg cannot seek.
fn ffprobe_after(path: &Path, format: &str, target: &str, n: usize, mode: Mode) -> Result<(Vec<TimeBase>, Vec<Pkt>), String> {
    let mut cmd = Command::new(port_ffprobe());
    cmd.args(["-v", "error", "-f", format]);
    if mode.unparsed {
        cmd.args(["-fflags", "+noparse+nofillin"]);
    }
    if mode.no_read_ahead {
        cmd.args(["-probesize", "32", "-analyzeduration", "0"]);
    }
    let out = cmd
        .args(["-read_intervals", &format!("{target}%+#{n}"), "-show_data_hash", "md5", "-show_entries"])
        .arg("stream=time_base:packet=stream_index,pts,dts,duration,size,flags,data_hash")
        .args(["-of", "compact"])
        .arg(path)
        .output()
        .expect("port ffprobe");
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("Could not seek") {
        return Err(stderr.trim().to_string());
    }
    let num = |v: Option<&&str>| v.and_then(|v| v.parse::<i64>().ok());
    let (mut time_bases, mut packets) = (Vec::new(), Vec::new());
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut fields = line.split('|');
        let section = fields.next().unwrap_or("");
        let kv: HashMap<&str, &str> = fields.filter_map(|f| f.split_once('=')).collect();
        match section {
            "packet" => packets.push(Pkt {
                stream: kv["stream_index"].parse().unwrap(),
                size: kv["size"].parse().unwrap(),
                md5: kv["data_hash"].trim_start_matches("MD5:").to_string(),
                pts: num(kv.get("pts")),
                dts: num(kv.get("dts")),
                duration: num(kv.get("duration")),
                key: kv["flags"].starts_with('K'),
            }),
            "stream" => {
                let (num, den) = kv["time_base"].split_once('/').unwrap();
                time_bases.push(TimeBase::new(num.parse().unwrap(), den.parse().unwrap()));
            }
            _ => {}
        }
    }
    assert!(out.status.success() && !packets.is_empty(), "ffprobe {} @ {target}: {stderr}", path.display());
    Ok((time_bases, packets))
}

/// av_rescale_q, rounding to nearest with ties away from zero.
fn rescale(ts: i64, from: TimeBase, to: TimeBase) -> i64 {
    let num = i128::from(ts) * i128::from(from.num()) * i128::from(to.den());
    let den = i128::from(from.den()) * i128::from(to.num());
    let q = (num.abs() + den / 2) / den;
    (if num < 0 { -q } else { q }) as i64
}

/// av_parse_time for the plain decimal seconds the tests use.
fn micros(target: &str) -> i64 {
    let (whole, frac) = target.split_once('.').unwrap_or((target, ""));
    let frac = format!("{frac:0<6}");
    whole.parse::<i64>().unwrap() * 1_000_000 + frac[..6].parse::<i64>().unwrap()
}

/// The stream FFmpeg seeks (av_find_default_stream_index): video over
/// audio, the first of the best. FFmpeg scores video higher once
/// avformat_find_stream_info knows its dimensions, which it does for
/// every file here (PVA's, unlike this demuxer, from the parsed video);
/// the player seeks its video stream the same way.
fn default_stream(streams: &[StreamInfo]) -> &StreamInfo {
    let score = |s: &StreamInfo| match s.params.media_type {
        MediaType::Video => 2,
        MediaType::Audio => 1,
        _ => 0,
    };
    streams.iter().fold(&streams[0], |best, s| if score(s) > score(best) { s } else { best })
}

/// Opens `path` as `format` through the player's registry, seeks it to
/// `target` the way FFmpeg does, and returns the stream infos, the landed
/// timestamp and the next `take` packets (fewer at the end).
fn ours_after(path: &Path, format: &str, target: &str, take: usize) -> Result<(Vec<StreamInfo>, i64, Vec<Pkt>), String> {
    let ctx = codecs::context();
    let file = std::fs::File::open(path).unwrap();
    let mut demuxer: Box<dyn Demuxer> =
        ctx.containers.open_demuxer(format, Box::new(file), &ctx.codecs).map_err(|e| format!("open: {e}"))?;
    let streams = demuxer.streams().to_vec();
    let seek = default_stream(&streams).clone();
    let tb = seek.time_base;
    let ticks = rescale(micros(target), TimeBase::new(1, 1_000_000), tb);
    let landed = demuxer.seek_to(seek.index, ticks).map_err(|e| format!("seek_to({}, {ticks}): {e}", seek.index))?;
    let mut packets = Vec::new();
    while packets.len() < take {
        match demuxer.next_packet() {
            Ok(p) => packets.push(Pkt {
                stream: p.stream_index,
                size: p.data.len(),
                md5: refcheck::md5_hex(&p.data),
                pts: p.pts,
                dts: p.dts,
                duration: p.duration,
                key: p.flags.keyframe,
            }),
            Err(Error::Eof) => break,
            Err(e) => return Err(format!("after the seek, packet {}: {e}", packets.len())),
        }
    }
    Ok((streams, landed, packets))
}

/// Our packets in FFmpeg's time bases, reduced to what `mode` compares.
fn comparable(packets: &[Pkt], ours: &[StreamInfo], theirs: &[TimeBase], mode: Mode) -> Vec<Pkt> {
    packets
        .iter()
        .map(|p| {
            let (from, to) = (ours[p.stream as usize].time_base, theirs[p.stream as usize]);
            Pkt {
                pts: p.pts.map(|t| rescale(t, from, to)),
                dts: p.dts.map(|t| rescale(t, from, to)),
                duration: if mode.durations { p.duration.map(|t| rescale(t, from, to)) } else { None },
                key: mode.keys && p.key,
                ..p.clone()
            }
        })
        .collect()
}

fn without(packets: Vec<Pkt>, mode: Mode) -> Vec<Pkt> {
    packets
        .into_iter()
        .map(|p| Pkt { duration: if mode.durations { p.duration } else { None }, key: mode.keys && p.key, ..p })
        .collect()
}

/// After seeking `path` to each target, our first `n` packets equal
/// FFmpeg's. Collects every difference.
fn check(path: &Path, rel: &str, format: &str, targets: &[&str], n: usize, mode: Mode) {
    let mut failures = Vec::new();
    for target in targets {
        let (time_bases, want) = ffprobe_after(path, format, target, n, mode)
            .unwrap_or_else(|e| panic!("{rel} @ {target}: FFmpeg cannot seek: {e}"));
        let want = without(want, mode);
        match ours_after(path, format, target, want.len()) {
            Ok((streams, landed, got)) => {
                let got = comparable(&got, &streams, &time_bases, mode);
                if got != want {
                    failures.push(format!("{rel} @ {target}: landed {landed}\n  ours   {got:?}\n  ffmpeg {want:?}"));
                }
            }
            Err(e) => failures.push(format!("{rel} @ {target}: {e}")),
        }
    }
    assert!(failures.is_empty(), "{} of {} seeks differ from FFmpeg:\n{}", failures.len(), targets.len(), failures.join("\n"));
}

const CONTAINER: Mode = Mode { unparsed: false, durations: false, keys: false, no_read_ahead: false };

/// cafdec.c read_seek: constant-size packets by arithmetic (PCM lands on
/// the target sample, mid-block), variable ones by the pakt index.
#[test]
fn caf() {
    let all = Mode { durations: true, ..CONTAINER };
    check(&refcheck::fate("caf/aac.caf"), "caf/aac.caf", "caf", &["1.0", "2.51"], 5, all);
    check(&refcheck::fate("caf/caf-pcm16.caf"), "caf/caf-pcm16.caf", "caf", &["0.5", "1.3"], 3, all);
    check(&refcheck::fate("caf/opus.caf"), "caf/opus.caf", "caf", &["0.02", "3.33"], 5, all);
}

/// vocdec.c voc_read_seek over the index ff_voc_get_packet builds (mid
/// block included), seek_frame_generic past it. SBPro ADPCM has no
/// timestamps after its first packet, so FFmpeg lands on that one.
#[test]
fn voc() {
    let sine = "sine=frequency=500:duration=3";
    let u8 = generated("u8.voc", &["-f", "lavfi", "-i", &format!("{sine}:sample_rate=22050"), "-c:a", "pcm_u8"]);
    let s16 = generated("s16.voc", &["-f", "lavfi", "-i", &format!("{sine}:sample_rate=44100"), "-ac", "2", "-c:a", "pcm_s16le"]);
    check(&u8, "pcm_u8 voc", "voc", &["0.5", "1.2345", "2.9"], 4, CONTAINER);
    check(&s16, "pcm_s16le voc", "voc", &["0.5", "1.2345"], 4, CONTAINER);
    check(&refcheck::fate("creative/BBC_4BIT.VOC"), "creative/BBC_4BIT.VOC", "voc", &["0.5", "2.0"], 3, CONTAINER);
}

/// Raw AC-3 / E-AC-3 (AVFMT_GENERIC_INDEX): the last frame at or before
/// the target, the frames after it timed as before.
#[test]
fn ac3_and_eac3() {
    let all = Mode { durations: true, ..CONTAINER };
    check(&refcheck::fate("ac3/monsters_inc_5.1_448_small.ac3"), "monsters_inc_5.1", "ac3", &["0.5", "1.0"], 4, all);
    check(&refcheck::fate("ac3/monsters_inc_2.0_192_small.ac3"), "monsters_inc_2.0", "ac3", &["0.77"], 4, all);
    check(&refcheck::fate("eac3/csi_miami_5.1_256_spx_small.eac3"), "csi_miami_5.1", "eac3", &["0.4", "1.1"], 4, all);
    check(&refcheck::fate("eac3/the_great_wall_7.1.eac3"), "the_great_wall_7.1", "eac3", &["0.3", "1.7"], 4, all);
}

/// IVF (AVFMT_GENERIC_INDEX): the last key frame at or before the
/// target, key frames as FFmpeg's VP8 / VP9 / AV1 parsers flag them.
#[test]
fn ivf() {
    let keys = Mode { keys: true, ..CONTAINER };
    let src = ["-f", "lavfi", "-i", "testsrc=duration=4:size=96x64:rate=25", "-g", "10", "-keyint_min", "10"];
    let vp8 = generated("vp8.ivf", &[&src[..], &["-c:v", "libvpx", "-b:v", "200k", "-f", "ivf"]].concat());
    let vp9 = generated("vp9.ivf", &[&src[..], &["-c:v", "libvpx-vp9", "-b:v", "200k", "-f", "ivf"]].concat());
    let av1 = generated("av1.ivf", &[&src[..], &["-t", "2", "-c:v", "libaom-av1", "-cpu-used", "8", "-b:v", "100k", "-f", "ivf"]].concat());
    for (path, name) in [(&vp8, "vp8"), (&vp9, "vp9"), (&av1, "av1")] {
        check(path, name, "ivf", &["1.0", "1.53"], 4, keys);
    }
    let rel = "av1/seq_hdr_op_param_info.ivf";
    check(&refcheck::fate(rel), rel, "ivf", &["0.9", "1.5"], 3, keys);
    let rel = "vp8-test-vectors-r1/vp80-00-comprehensive-001.ivf";
    check(&refcheck::fate(rel), rel, "ivf", &["0.5"], 3, keys);
}

/// Raw MPEG-1/2 video (AVFMT_GENERIC_INDEX): the last I-frame at or
/// before the target, and FFmpeg's timing of the frames after it.
#[test]
fn mpegvideo() {
    let all = Mode { keys: true, durations: true, ..CONTAINER };
    let src = ["-f", "lavfi", "-i", "testsrc=duration=4:size=176x144:rate=25", "-c:v"];
    let bframes = generated("bframes.m2v", &[&src[..], &["mpeg2video", "-g", "12", "-bf", "2", "-f", "mpeg2video"]].concat());
    let mpeg1 = generated("ippp.m1v", &[&src[..], &["mpeg1video", "-g", "15", "-bf", "0", "-f", "mpeg1video"]].concat());
    check(&bframes, "mpeg2 with B-frames", "mpegvideo", &["1.3", "2.0"], 6, all);
    check(&mpeg1, "mpeg1 IPPP", "mpegvideo", &["0.7", "2.21"], 6, all);
}

/// NUT read_seek: the index of key frames FFmpeg's muxer writes, from the
/// syncpoint before the key frame on, packets before each stream's first
/// key frame skipped.
#[test]
fn nut() {
    let path = generated(
        "av.nut",
        &[
            "-f", "lavfi", "-i", "sine=frequency=1000:duration=4", "-f", "lavfi", "-i",
            "testsrc=duration=4:size=64x64:rate=10", "-c:a", "mp2", "-c:v", "mpeg2video", "-g", "8", "-bf", "0",
            "-shortest",
        ],
    );
    check(&path, "generated NUT", "nut", &["1.0", "2.35"], 8, Mode { unparsed: true, keys: true, ..CONTAINER });
}

/// PVA bisects the PES timestamps (pva_read_timestamp, which looks at
/// most 8 PVA payloads ahead). Video timestamps are in display order, so
/// the landing depends on every step of ff_gen_search, and through its
/// bounds on the index: on what avformat_find_stream_info read ahead,
/// which this demuxer does not model (FFmpeg reads 59 packets of this
/// file, and lands 0.08 s later at 18979.75 than without them). FFmpeg
/// therefore runs without read-ahead here.
#[test]
fn pva() {
    let rel = "pva/PVA_test-partial.pva";
    let mode = Mode { unparsed: true, no_read_ahead: true, ..CONTAINER };
    check(&refcheck::fate(rel), rel, "pva", &["0.5", "18979.3", "18979.75", "18980.1"], 6, mode);
}

/// The streams the MPEG-PS demuxer re-frames with FFmpeg's parsers.
fn reframed(stream: &StreamInfo) -> bool {
    matches!(stream.params.codec_id.as_str(), "dvd_subtitle" | "mp1" | "mp2" | "mp3" | "ac3" | "eac3")
}

/// MPEG-PS bisects the PES dts of the default stream (mpegps_read_dts):
/// the landing PES may be mid-GOP, as FFmpeg's is. After it, every
/// stream's first packets: the parsed table for the streams this demuxer
/// re-frames, the PES table and its interleaving for the others.
fn check_ps(path: &Path, rel: &str, targets: &[&str], n: usize) {
    let mut failures = Vec::new();
    for target in targets {
        let (time_bases, unparsed) =
            ffprobe_after(path, "mpeg", target, n, Mode { unparsed: true, ..CONTAINER }).unwrap_or_else(|e| panic!("{rel} @ {target}: {e}"));
        let (_, parsed) = ffprobe_after(path, "mpeg", target, n, CONTAINER).unwrap_or_else(|e| panic!("{rel} @ {target}: {e}"));
        let (streams, landed, ours) = match ours_after(path, "mpeg", target, 4 * n) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{rel} @ {target}: {e}"));
                continue;
            }
        };
        let ours = comparable(&ours, &streams, &time_bases, CONTAINER);
        let (unparsed, parsed) = (without(unparsed, CONTAINER), without(parsed, CONTAINER));
        for s in &streams {
            let oracle = if reframed(s) { &parsed } else { &unparsed };
            let want: Vec<&Pkt> = oracle.iter().filter(|p| p.stream == s.index).collect();
            let got: Vec<&Pkt> = ours.iter().filter(|p| p.stream == s.index).take(want.len()).collect();
            if got != want {
                failures.push(format!("{rel} @ {target}: landed {landed}, stream {}\n  ours   {got:?}\n  ffmpeg {want:?}", s.index));
            }
        }
        let pes: Vec<u32> = streams.iter().filter(|s| !reframed(s)).map(|s| s.index).collect();
        let order = |pkts: &[Pkt]| pkts.iter().filter(|p| pes.contains(&p.stream)).map(|p| (p.stream, p.md5.clone())).collect::<Vec<_>>();
        let want = order(&unparsed);
        let got: Vec<_> = order(&ours).into_iter().take(want.len()).collect();
        if got != want {
            failures.push(format!("{rel} @ {target}: PES interleaving\n  ours   {got:?}\n  ffmpeg {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{} differences from FFmpeg:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn mpegps() {
    let path = generated(
        "av.mpg",
        &[
            "-f", "lavfi", "-i", "testsrc=duration=6:size=176x144:rate=25", "-f", "lavfi", "-i",
            "sine=frequency=440:duration=6", "-c:v", "mpeg2video", "-g", "25", "-bf", "2", "-c:a", "mp2", "-f", "mpeg",
        ],
    );
    check_ps(&path, "generated MPEG-2/MP2 program stream", &["2.5", "2.7", "3.6", "5.9"], 12);
    let rel = "mpeg2/matrixbench_mpeg2.lq1.mpg";
    check_ps(&refcheck::fate(rel), rel, &["1.0", "3.3"], 12);
}

/// libavformat has no SMF demuxer, so there is no FFmpeg landing to
/// match: the song is one packet, the only random access point, and a
/// seek anywhere hands it out again from its start.
#[test]
fn smf_seeks_back_to_its_single_packet() {
    let mut song = b"MThd\0\0\0\x06\0\0\0\x01\x01\xE0".to_vec();
    let track = [0x00, 0x90, 0x3C, 0x40, 0x83, 0x60, 0x80, 0x3C, 0x40, 0x00, 0xFF, 0x2F, 0x00];
    song.extend_from_slice(b"MTrk");
    song.extend_from_slice(&(track.len() as u32).to_be_bytes());
    song.extend_from_slice(&track);
    let ctx = codecs::context();
    let mut demuxer = ctx.containers.open_demuxer("smf", Box::new(std::io::Cursor::new(song.clone())), &ctx.codecs).unwrap();
    let first = demuxer.next_packet().unwrap();
    assert_eq!(first.data, song);
    assert!(matches!(demuxer.next_packet(), Err(Error::Eof)));
    for target in [44_100, 0] {
        assert_eq!(demuxer.seek_to(0, target).unwrap(), 0, "seek to {target} lands on the song's start");
        let again = demuxer.next_packet().unwrap();
        assert_eq!((again.data, again.pts, again.flags.keyframe), (song.clone(), Some(0), true));
        assert!(matches!(demuxer.next_packet(), Err(Error::Eof)));
    }
}
