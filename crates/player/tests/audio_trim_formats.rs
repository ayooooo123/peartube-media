//! Real Opus/MLP decoder regressions and pinned-FFmpeg gapless counts. The
//! oracle and the fixtures come from the FFmpeg the ports follow (2da55bf,
//! `refcheck::pinned_ffmpeg`), not from PATH.

use std::{path::{Path, PathBuf}, process::Command, sync::Arc, time::Duration};
use oxideav_core::MediaType;
use oxideav_ogg::page::{flags, lace, Page};
use player::{Headless, Player, PlayerOptions};

struct Fixture(PathBuf);
impl Fixture {
    fn new(ext: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("audio-trim-format-{}-{n}.{ext}", std::process::id())))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); }
}

fn ffmpeg(args: &[&str], path: &Path) {
    let out = Command::new(refcheck::pinned_ffmpeg()).args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args(args).arg(path).output().unwrap();
    assert!(out.status.success(), "FFmpeg fixture: {}", String::from_utf8_lossy(&out.stderr));
}

fn play(path: &Path, seek: Option<Duration>) -> (Vec<f32>, usize) {
    let backend = Headless::new();
    let p = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions { realtime: false, ..PlayerOptions::default() }, |_| {});
    if let Some(at) = seek { p.seek(at); }
    let state = p.wait();
    drop(p);
    assert!(state.error.is_none(), "{}: {:?}", path.display(), state.error);
    assert_eq!(state.audio, Some(0));
    let mut capture = backend.capture();
    let mut audio = capture.audio.remove(0);
    let from = audio.flushes.last().map_or(0, |&w| audio.writes.get(w).map_or(audio.pcm.len(), |&(_, at)| at));
    audio.pcm.drain(..from);
    (audio.pcm, audio.channels as usize)
}

fn opus(pre_skip: u16, packet_samples: u32, packets: u32) -> Fixture {
    let path = Fixture::new("opus");
    let mut head = b"OpusHead\x01\x02".to_vec();
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&48000u32.to_le_bytes());
    head.extend_from_slice(&[0, 0, 0]);
    let mut data = Vec::new();
    let mut page = |seq_no, flags, granule_position, packet: Vec<u8>| {
        data.extend(Page { flags, granule_position, serial: 17, seq_no,
            lacing: lace(packet.len()), data: packet }.to_bytes());
    };
    page(0, flags::FIRST_PAGE, 0, head);
    page(1, 0, 0, [b"OpusTags".as_slice(), &[0; 8]].concat());
    for n in 0..packets {
        page(n + 2, if n + 1 == packets { flags::LAST_PAGE } else { 0 },
            i64::from((n + 1) * packet_samples),
            vec![if packet_samples == 120 { 0x80 } else { 0xF8 }, 0xFF, 0xFE]);
    }
    std::fs::write(&path.0, data).unwrap();
    path
}

fn exact_length(path: &Path, expected: usize) {
    let ff = refcheck::ffmpeg_audio_f32(path, 0);
    let (played, channels) = play(path, None);
    let decoded = refcheck::decode(path, &[codecs::register_all], MediaType::Audio, 0);
    let kept = refcheck::interleaved_f32(&decoded);
    eprintln!("{}: played {}, refcheck {}, FFmpeg {} samples/channel", path.display(),
        played.len() / channels, kept.len() / channels, ff.len() / channels);
    assert_eq!(ff.len() / channels, expected, "fixture/oracle count");
    assert_eq!(played.len(), ff.len(), "Player count");
    assert_eq!(played, kept, "Player and refcheck PCM");
}

#[test]
fn opus_preskip_over_one_hundred_tiny_packets_keeps_playing() {
    let input = opus(12000, 120, 160);
    exact_length(&input.0, 7200);
}

fn opus_in_container(ext: &str) {
    let input = opus(312, 960, 12);
    let output = Fixture::new(ext);
    ffmpeg(&["-i", input.0.to_str().unwrap(), "-c", "copy"], &output.0);
    exact_length(&output.0, 11208);
}

/// FATE's chained Ogg Opus: a second link (its own serial, pre-skip and end
/// padding) after the first. FFmpeg goes on with it as the same stream
/// (`ogg_replace_stream`): 4800 samples from each link.
#[test]
fn chained_ogg_opus_plays_both_links() {
    exact_length(&refcheck::fate("ogg-opus/chained-meta.ogg"), 9600);
}

/// FFmpeg 2da55bf's decode of `path` from `at` on. `-ss` before `-i`
/// seeks, decodes from where it lands and drops what precedes `at`;
/// `-seek_timestamp 1` makes `at` a media time, as `Player::seek` takes
/// it, instead of an offset from the input's start time.
fn ffmpeg_from(path: &Path, at: Duration) -> Vec<f32> {
    let out = Command::new(refcheck::pinned_ffmpeg())
        .args(["-nostdin", "-v", "error", "-seek_timestamp", "1", "-ss", &format!("{:.6}", at.as_secs_f64()), "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
}

/// SNR of `played` against `reference` from sample `from` (per channel)
/// on, with `played` read `lag` samples later.
fn snr_at(reference: &[f32], played: &[f32], channels: usize, from: usize, lag: i64) -> f64 {
    let shift = lag.unsigned_abs() as usize;
    let (r, p) = if lag >= 0 { (from, from + shift) } else { (from + shift, from) };
    let r = reference.get(r * channels..).unwrap_or_default();
    let p = played.get(p * channels..).unwrap_or_default();
    let n = r.len().min(p.len());
    refcheck::try_snr_db(&r[..n], &p[..n], 0).unwrap_or(f64::NEG_INFINITY)
}

/// After a seek the Player plays FFmpeg's `-ss` samples: the same count
/// and the same audio, once both decoders are past the 80 ms Opus pre-roll
/// (RFC 9559 SeekPreRoll). Until then their states differ: the demuxers
/// may land on different packets (FFmpeg's Matroska index has every block,
/// ours its Cues). `ticks`: how far FFmpeg's Matroska output may be off.
/// After a seek FFmpeg injects the CodecDelay skip into the packet it
/// lands on and advances that frame by the skip rounded to the 1 ms
/// packet time base, so its cut can be off by up to one tick. A start
/// delay dropped again after the seek shifts everything by the pre-skip.
fn seek_matches_ffmpeg(path: &Path, at: Duration, ticks: i64) {
    let ff = ffmpeg_from(path, at);
    let (played, channels) = play(path, Some(at));
    let (ff_len, played_len) = (ff.len() / channels, played.len() / channels);
    assert!(ff_len > 3840, "{}: too short to compare after the pre-roll", path.display());
    let (lag, snr) = (-ticks..=ticks)
        .map(|lag| (lag, snr_at(&ff, &played, channels, 3840, lag)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap();
    eprintln!("{} from {at:?}: played {played_len} FFmpeg {ff_len} samples/channel, SNR {snr:.2} dB at lag {lag} after 80 ms",
        path.display());
    assert!(played_len.abs_diff(ff_len) as i64 <= ticks, "post-seek samples/channel: {played_len} vs FFmpeg {ff_len}");
    assert!(snr >= 40.0, "post-seek SNR after the pre-roll {snr:.2} dB (best lag {lag})");
}

#[test]
fn opus_seeks_play_ffmpegs_samples_in_matroska_and_ogg() {
    // RFC 6716 test vector 1 (29.5 s stereo, CELT and SILK, no pre-skip),
    // as FFmpeg's FATE muxed it, and FFmpeg's own encoder's output (a
    // 120-sample pre-skip, CodecDelay and an 80 ms SeekPreRoll), each also
    // in Ogg. The 1.5 s seek lands on the first Cluster, which starts with
    // the CodecDelay; the 7.5 s one on the second, with the SeekPreRoll.
    let vector = refcheck::fate("opus/testvector01.mka");
    let generated = Fixture::new("mka");
    ffmpeg(&["-f", "lavfi", "-i", "aevalsrc=0.4*sin(2*PI*(220+300*t)*t)|0.4*sin(2*PI*(330+150*t)*t):s=48000:d=12",
        "-c:a", "opus", "-strict", "-2", "-b:a", "128k"], &generated.0);
    let cases: [(&Path, &[u64]); 2] = [(&vector, &[5000, 12345]), (&generated.0, &[1500, 7500])];
    for (mkv, targets) in cases {
        let ogg = Fixture::new("opus");
        ffmpeg(&["-i", mkv.to_str().unwrap(), "-c", "copy"], &ogg.0);
        for &ms in targets {
            seek_matches_ffmpeg(mkv, Duration::from_millis(ms), 48);
            seek_matches_ffmpeg(&ogg.0, Duration::from_millis(ms), 0);
        }
    }
}

#[test]
fn opus_preskip_is_applied_once_in_mp4() { opus_in_container("mp4"); }

#[test]
fn opus_preskip_is_applied_once_in_matroska() { opus_in_container("mkv"); }

/// FFmpeg's Vorbis decoder outputs the first packet's frame and drops it
/// as its own delay (`vorbisdec.c`); FFmpeg's encoders declare that frame
/// as the WebM CodecDelay, whose skip replaces the delay. OxideAV's decoder
/// never outputs that frame, so the skip must not come off its output too.
#[test]
fn vorbis_in_webm_plays_ffmpegs_samples() {
    let output = Fixture::new("webm");
    ffmpeg(&["-f", "lavfi", "-i", "aevalsrc=0.5*sin(2*PI*(220+400*t)*t):s=48000:d=3", "-ac", "2",
        "-c:a", "vorbis", "-strict", "-2"], &output.0);
    exact_length(&output.0, 144_000);
    let (played, channels) = play(&output.0, None);
    let snr = snr_at(&refcheck::ffmpeg_audio_f32(&output.0, 0), &played, channels, 0, 0);
    assert!(snr >= 90.0, "Vorbis in WebM is {snr:.1} dB from FFmpeg's samples");
}

fn long_major_sync_seek(codec: &str) {
        let input = Fixture::new(if codec == "truehd" { "thd" } else { "mlp" });
        ffmpeg(&["-f", "lavfi", "-i", "sine=frequency=997:sample_rate=48000:duration=0.4",
            "-ac", "2", "-c:a", codec, "-strict", "-2", "-max_interval", "128", "-f", codec], &input.0);
        let bytes = std::fs::read(&input.0).unwrap();
        let mut at = 0;
        let mut au = 0;
        let mut majors = Vec::new();
        while at + 8 <= bytes.len() {
            if bytes[at + 4..at + 7] == [0xF8, 0x72, 0x6F] { majors.push(au); }
            let length = usize::from(u16::from_be_bytes([bytes[at], bytes[at + 1]]) & 0xFFF) * 2;
            assert!(length >= 4 && at + length <= bytes.len());
            at += length;
            au += 1;
        }
        assert_eq!(&majors[..2], &[0, 128], "major syncs must be 128 access units apart");
        // The long major-sync interval deliberately defeats raw probing.
        let oracle = Command::new(refcheck::pinned_ffmpeg())
            .args(["-nostdin", "-v", "error", "-f", codec, "-i", input.0.to_str().unwrap(),
                "-f", "f32le", "-c:a", "pcm_f32le", "-"]).output().unwrap();
        assert!(oracle.status.success(), "{}", String::from_utf8_lossy(&oracle.stderr));
        let ff: Vec<f32> = oracle.stdout.chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        // The seek lands on the major sync at or before the target, as
        // FFmpeg's does (only major-sync units are random-access points), so
        // playback resumes at the target instead of waiting 128 units for
        // the next one; audio before the target is dropped by whole frames.
        let (played, channels) = play(&input.0, Some(Duration::from_micros(834)));
        let unit = 40 * channels;
        eprintln!("{codec}: post-seek {} samples/channel of {}", played.len() / channels, ff.len() / channels);
        assert!(played.len() % unit == 0 && played.len() + 2 * unit >= ff.len(),
            "playback must resume within one unit of the target: {} of {} samples", played.len(), ff.len());
        assert_eq!(played[..], ff[ff.len() - played.len()..], "post-seek PCM must equal FFmpeg's, bit-exact");
}

#[test]
fn truehd_seek_across_long_major_sync_intervals() { long_major_sync_seek("truehd"); }

#[test]
fn mlp_seek_across_long_major_sync_intervals() { long_major_sync_seek("mlp"); }

#[test]
fn itunes_mp3_has_the_pinned_ffmpeg_presented_length() {
    exact_length(&refcheck::fate("gapless/gapless-itunes.mp3"), 418950);
}

/// `(pts, skip, discard)` of the packets in ffprobe `-show_packets` compact
/// lines with side data, `pts` rescaled by `scale` (packets without side
/// data are kept when `all`, with zero trims).
fn side_data(text: &str, scale: (i64, i64), all: bool) -> Vec<(i64, u32, u32)> {
    text.lines()
        .filter(|line| line.starts_with("packet|"))
        .filter_map(|line| {
            let field = |name: &str| line.split('|').find_map(|f| f.strip_prefix(name)).map(|v| v.parse::<i64>().unwrap());
            let pts = field("pts=")? * scale.0 / scale.1;
            match (field("side_datum/skip_samples:skip_samples="), field("side_datum/skip_samples:discard_padding=")) {
                (Some(skip), Some(discard)) => Some((pts, skip as u32, discard as u32)),
                _ => all.then_some((pts, 0, 0)),
            }
        })
        .collect()
}

/// The packets with a trim in an FFmpeg FATE side-data reference of a
/// 44.1 kHz MP3 (the mp3 demuxer's 1/14112000 time base), in samples.
fn fate_side_data(reference: &str) -> Vec<(i64, u32, u32)> {
    let text = std::fs::read_to_string(refcheck::ffmpeg_src().join("tests/ref/fate").join(reference)).unwrap();
    side_data(&text, (44100, 14_112_000), false)
}

/// `(pts, skip, discard)` from our demuxer, through the Player's registry:
/// every packet when `all`, else those with a trim.
fn demuxed_trims(path: &Path, all: bool) -> Vec<(i64, u32, u32)> {
    let ctx = codecs::context();
    let name = refcheck::probe_container(&ctx, path).unwrap();
    let file = std::fs::File::open(path).unwrap();
    let mut demuxer = ctx.containers.open_demuxer(&name, Box::new(file), &ctx.codecs).unwrap();
    let mut out = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(packet) => match demuxer.packet_metadata().audio_trim {
                Some(t) => out.push((packet.pts.unwrap(), t.skip_samples, t.discard_padding)),
                None if all => out.push((packet.pts.unwrap(), 0, 0)),
                None => {}
            },
            Err(oxideav_core::Error::Eof) => return out,
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
}

#[test]
fn mp3_packet_trims_are_ffmpegs_fate_side_data() {
    for (sample, reference) in [
        ("gapless/gapless.mp3", "gapless-mp3-side-data"),
        ("gapless/gapless-itunes.mp3", "gapless-mp3-itunes-side-data"),
    ] {
        let expected = fate_side_data(reference);
        assert!(!expected.is_empty(), "{reference}");
        assert_eq!(demuxed_trims(&refcheck::fate(sample), false), expected, "{sample}");
    }
}

#[test]
fn ogg_opus_packets_have_ffprobes_pts_and_trims() {
    // Every packet: pts (the start less the pre-skip), the first packet's
    // pre-skip and the last one's end padding, all in 1/48000.
    let path = refcheck::fate("ogg/intro-partial.opus");
    let out = Command::new(refcheck::pinned_ffprobe())
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "packet=pts:packet_side_data", "-of", "compact"])
        .arg(&path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let expected = side_data(&String::from_utf8(out.stdout).unwrap(), (1, 1), true);
    assert!(expected.len() > 2 && expected[0].1 > 0, "{expected:?}");
    assert_eq!(demuxed_trims(&path, true), expected);
}

/// The playback's audio writes as `pts - position`, in samples: 0
/// throughout on FFmpeg's timeline, whose first frame of these files is at
/// 0 and whose frames follow on.
fn timeline_offsets(path: &Path) -> Vec<i64> {
    let backend = Headless::new();
    let p = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions { realtime: false, ..PlayerOptions::default() }, |_| {});
    assert!(p.wait().error.is_none());
    drop(p);
    let capture = backend.capture();
    let audio = &capture.audio[0];
    let ch = audio.channels as usize;
    audio.writes.iter()
        .map(|&(pts, at)| (pts.as_secs_f64() * f64::from(audio.sample_rate)).round() as i64 - (at / ch) as i64)
        .collect()
}

#[test]
fn opus_plays_on_ffmpegs_timeline_in_ogg_mp4_and_webm() {
    // Matroska stamps whole milliseconds and moves them back by the
    // CodecDelay rounded to one: within one tick (48 samples).
    let ogg = refcheck::fate("ogg/intro-partial.opus");
    let mp4 = Fixture::new("mp4");
    let webm = Fixture::new("webm");
    for out in [&mp4, &webm] {
        ffmpeg(&["-i", ogg.to_str().unwrap(), "-c", "copy"], &out.0);
    }
    for (path, tolerance) in [(ogg.as_path(), 0), (mp4.0.as_path(), 0), (webm.0.as_path(), 48)] {
        let offsets = timeline_offsets(path);
        assert!(!offsets.is_empty() && offsets.iter().all(|&o| o.abs() <= tolerance), "{}: {offsets:?}", path.display());
    }
}

#[test]
fn declared_priming_is_not_applied_again_from_negative_timestamps() {
    use refcheck::trim_fixture::{self, Spec};
    let mut spec = Spec::new(1, 24000, 512, 2);
    spec.output_rate = 48000;
    spec.start_pts = -100;
    spec.packets[0].skip = 100;
    spec.packets[0].trim_rate = 48000;
    let input = Fixture::new(trim_fixture::EXTENSION);
    std::fs::write(&input.0, spec.to_bytes()).unwrap();
    let backend = Headless::new();
    let mut ctx = oxideav_core::RuntimeContext::new();
    trim_fixture::register(&mut ctx);
    let p = Player::open(input.0.to_str().unwrap(), backend.clone(), Arc::new(ctx),
        PlayerOptions { realtime: false, ..PlayerOptions::default() }, |_| {});
    assert!(p.wait().error.is_none());
    drop(p);
    let capture = backend.capture();
    let pcm = &capture.audio[0].pcm;
    assert_eq!(trim_fixture::runs(&trim_fixture::indices(pcm, 1)), [(100, 2048)]);
    assert_eq!(capture.audio[0].writes[0].0, Duration::ZERO);
}
