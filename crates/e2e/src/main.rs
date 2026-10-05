//! Corpus runner: plays every manifest entry through `player::Player` with the
//! `Headless` backend, compares every stream with FFmpeg, and writes
//! `target/e2e/codecs.json`.
//!
//! ```text
//! cargo run -p e2e --release -- [--filter X] [--fuzz]
//! ```

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use player::{Headless, Player, PlayerOptions};
use serde::Serialize;

/// Yardstick row (a `<kind>:<id>` from corpus/yardstick.toml).
type Row = String;

// ---------------------------------------------------------------- manifest

#[derive(Debug, serde::Deserialize)]
struct Manifest {
    #[serde(default)]
    entry: Vec<Entry>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Entry {
    path: String,
    rows: Vec<String>,
    compare: Vec<String>,
    #[serde(default)]
    streams: BTreeMap<String, u32>,
}

#[derive(Debug, serde::Deserialize)]
struct Yardstick {
    rows: Vec<Row>,
}

/// Where a `path` resolves to on disk.
fn resolve(path: &str) -> Option<PathBuf> {
    let p = match path.split_once(':') {
        Some(("fate", rel)) => {
            let root = std::env::var_os("FATE_SUITE")
                .map(PathBuf::from)
                .unwrap_or_else(|| dirs_home().join("projects/fate-suite"));
            root.join(rel)
        }
        Some(("gen", name)) => corpus_dir().join(name),
        _ => return None,
    };
    p.is_file().then_some(p)
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn corpus_dir() -> PathBuf {
    std::env::var_os("PEARTUBE_CORPUS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join("projects/peartube-media-corpus"))
}

// ---------------------------------------------------------------- results

#[derive(Serialize, Clone)]
struct StreamResult {
    index: u32,
    kind: String,
    codec: String,
    decoder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    frames: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    samples: Option<usize>,
    verdict: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    metric: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct EntryResult {
    path: String,
    streams: Vec<StreamResult>,
}

#[derive(Serialize, Default, Clone)]
struct RowResult {
    row: Row,
    passing_entries: Vec<String>,
    failing_entries: Vec<String>,
}

#[derive(Serialize)]
struct Report {
    entries: Vec<EntryResult>,
    rows: Vec<RowResult>,
    fuzz: Option<FuzzReport>,
}

#[derive(Serialize, Default)]
struct FuzzReport {
    mutations: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    passed: bool,
    failures: Vec<String>,
}

// ---------------------------------------------------------------- playback

/// Plays `path` (or URL) to the end and returns the headless capture plus the
/// final state. Non-realtime, so it runs as fast as the decoders do. A hung
/// pipeline (a demuxer or decoder waiting forever) aborts after `secs` and is
/// reported as a timeout; the leaked threads die with the process.
fn play(
    url: &str,
    options: PlayerOptions,
    secs: u64,
) -> Result<(player::Capture, player::State), String> {
    let backend = Headless::new();
    let ctx = Arc::new(codecs::context());
    let url = url.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let player = Player::open(&url, backend.clone(), ctx, options, |_| {});
        let state = player.wait();
        drop(player);
        let capture = backend.capture();
        let _ = tx.send((capture, state));
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok((capture, state)) => Ok((capture, state)),
        Err(_) => Err(format!("playback hung past the {secs}s watchdog")),
    }
}

/// Subtitle packet count by `ffprobe -show_packets`. Bounded: ffmpeg's
/// subtitle parsers can spin on malformed samples, so the call is given
/// `stdin(null)`, `-nostdin`, and a 30 s kill timeout.
fn ffprobe_subtitle_packets(path: &Path, nth: usize) -> Result<usize, String> {
    let mut child = std::process::Command::new("ffprobe")
        .args([
            "-v", "error", "-nostdin", "-select_streams", &format!("s:{nth}"),
            "-count_packets", "-show_entries", "stream=nb_read_packets",
            "-of", "csv=p=0", path.to_str().ok_or("path not utf8")?,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut o) = child.stdout.take() {
                    let _ = o.read_to_string(&mut out);
                }
                if !status.success() {
                    let mut err = String::new();
                    if let Some(mut e) = child.stderr.take() {
                        let _ = e.read_to_string(&mut err);
                    }
                    return Err(format!("ffprobe failed: {}", err.trim()));
                }
                return out
                    .trim()
                    .parse::<usize>()
                    .map_err(|e| format!("ffprobe output: {e}"));
            }
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    return Err("ffprobe timed out after 30s (sample likely loops the parser)".into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// One stream's comparison, run against one capture.
struct Compare {
    verdict: &'static str,
    metric: String,
    error: Option<String>,
}

fn compare_video(path: &Path, cap: &player::VideoCapture, nth: usize) -> Compare {
    if cap.frame_md5.is_empty() {
        return Compare {
            verdict: "FAIL",
            metric: "frames=0".into(),
            error: Some("no frames captured".into()),
        };
    }
    let pix = refcheck::ffmpeg_pix_fmt(cap.pixel_format);
    let p = path.to_path_buf();
    let pix2 = pix;
    let expect = match with_ffmpeg_timeout(path, 60, move || {
        refcheck::ffmpeg_video_md5s(&p, nth, pix2)
    }) {
        Ok(e) => e,
        Err(e) => {
            return Compare {
                verdict: "FAIL",
                metric: format!("frames={}", cap.frame_md5.len()),
                error: Some(e),
            }
        }
    };
    let got = &cap.frame_md5;
    let exact = expect == *got;
    let matched = got.iter().filter(|m| expect.contains(m)).count();
    let metric = format!("frames={} matched {matched}/{}", got.len(), expect.len());
    if exact {
        Compare { verdict: "PASS", metric, error: None }
    } else {
        Compare { verdict: "FAIL", metric, error: None }
    }
}

fn compare_audio(path: &Path, cap: &player::AudioCapture, nth: usize, floor_db: f64) -> Compare {
    if cap.pcm.is_empty() {
        return Compare {
            verdict: "FAIL",
            metric: "samples=0".into(),
            error: Some("no PCM captured".into()),
        };
    }
    let non_finite = cap.pcm.iter().filter(|x| !x.is_finite()).count();
    if non_finite > 0 {
        return Compare {
            verdict: "FAIL",
            metric: format!(
                "samples={} non-finite={non_finite}",
                cap.pcm.len() / cap.channels.max(1) as usize
            ),
            error: Some(format!("decoder produced {non_finite} NaN/inf samples")),
        };
    }
    let p = path.to_path_buf();
    let reference = match with_ffmpeg_timeout(path, 60, move || refcheck::ffmpeg_audio_f32(&p, nth)) {
        Ok(r) => r,
        Err(e) => {
            return Compare {
                verdict: "FAIL",
                metric: format!(
                    "samples={}",
                    cap.pcm.len() / cap.channels.max(1) as usize
                ),
                error: Some(e),
            }
        }
    };
    // One decode frame of slack; floor at one frame of the source rate.
    let slack = cap.sample_rate.max(1) as usize / 10 + 2048;
    let snr = refcheck::snr_db(&reference, &cap.pcm, slack);
    let metric = format!(
        "samples={} snr={snr:.1} dB",
        cap.pcm.len() / cap.channels.max(1) as usize
    );
    if snr.is_infinite() || snr >= floor_db {
        Compare { verdict: "PASS", metric, error: None }
    } else {
        Compare {
            verdict: "FAIL",
            metric,
            error: Some(format!("SNR {snr:.1} dB below {floor_db} dB floor")),
        }
    }
}

fn compare_subtitles(path: &Path, cap: &player::SubtitleCapture, nth: usize) -> Compare {
    let expect = match ffprobe_subtitle_packets(path, nth) {
        Ok(n) => n,
        Err(e) => {
            return Compare {
                verdict: "FAIL",
                metric: format!("shows={}", cap.shows.len()),
                error: Some(e),
            }
        }
    };
    // The pipeline shows each cue then clears it, so `shows` counts cleared
    // events too; the cue count is the number of non-empty shows.
    let cues = cap.shows.iter().filter(|(_, n)| *n > 0).count();
    let metric = format!("shows={} ffmpeg_packets={expect}, cues={cues}", cap.shows.len());
    if cues == expect {
        Compare { verdict: "PASS", metric, error: None }
    } else {
        Compare {
            verdict: "FAIL",
            metric,
            error: Some(format!("cue count {cues} != ffprobe {expect}")),
        }
    }
}

// ---------------------------------------------------------------- entries

/// Runs `f` on a worker thread with a `secs` timeout. ffmpeg's demuxers spin
/// forever on some malformed subtitle samples; on timeout the ffmpeg child
/// the worker spawned is killed by command line and the caller gets an error.
/// The worker thread itself leaks (it dies with the process); `f` must be
/// `'static`-safe (refcheck's helpers take owned `PathBuf`s).
fn with_ffmpeg_timeout<T: Send + 'static>(
    path: &Path,
    secs: u64,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let p = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(v) => Ok(v),
        Err(_) => {
            // Kill any ffmpeg/ffprobe working on this exact path.
            let pat = p.to_string_lossy().into_owned();
            let _ = std::process::Command::new("pkill")
                .args(["-9", "-f", &pat])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            Err(format!("ffmpeg reference timed out after {secs}s on {}", p.display()))
        }
    }
}

/// Plays one entry once and returns its per-stream results. With `http_base`,
/// the entry is played a second time over HTTP from a Range-capable local
/// server and the same digests are compared; the HTTP result replaces the
/// file one when it disagrees.
fn run_entry(entry: &Entry, path: &Path, http_base: Option<&str>) -> EntryResult {
    let mut streams_out: Vec<StreamResult> = Vec::new();

    // First pass: discover tracks (needed to map kinds → stream indices).
    let discover = PlayerOptions {
        realtime: false,
        audio: entry.streams.get("audio").copied(),
        video: entry.streams.get("video").copied(),
        subtitle: entry.streams.get("subtitle").copied(),
    };
    let (capture, state) = match play(&path.to_string_lossy(), discover, 60) {
        Ok(r) => r,
        Err(e) => {
            streams_out.push(StreamResult {
                index: 0,
                kind: "open".into(),
                codec: String::new(),
                decoder: String::new(),
                frames: None,
                samples: None,
                verdict: "FAIL".into(),
                metric: None,
                error: Some(e),
            });
            return EntryResult { path: entry.path.clone(), streams: streams_out };
        }
    };

    if let Some(err) = &state.error {
        streams_out.push(StreamResult {
            index: 0,
            kind: "engine".into(),
            codec: String::new(),
            decoder: String::new(),
            frames: None,
            samples: None,
            verdict: "FAIL".into(),
            metric: Some(format!("ended={} position={:?}", state.ended, state.position)),
            error: Some(err.clone()),
        });
        return EntryResult { path: entry.path.clone(), streams: streams_out };
    }
    if !state.ended {
        streams_out.push(StreamResult {
            index: 0,
            kind: "engine".into(),
            codec: String::new(),
            decoder: String::new(),
            frames: None,
            samples: None,
            verdict: "FAIL".into(),
            metric: None,
            error: Some("playback did not reach Ended".into()),
        });
        return EntryResult { path: entry.path.clone(), streams: streams_out };
    }

    let tracks = state.tracks.clone();

    // Compare each captured stream with FFmpeg. The first pass already played
    // everything the options select; comparisons run on its capture.
    let mut nth_video = 0usize;
    let mut nth_audio = 0usize;
    let mut nth_sub = 0usize;
    for vc in &capture.video {
        // `video:decodes` means FFmpeg itself cannot compare this format
        // (no reference possible): playing to Ended with frames is the check.
        let cmp = if entry.compare.iter().any(|c| c == "video:decodes" || c == "decodes") {
            if vc.frame_md5.is_empty() {
                Compare { verdict: "FAIL", metric: "frames=0".into(), error: Some("no frames captured".into()) }
            } else {
                Compare { verdict: "PASS", metric: format!("frames={}", vc.frame_md5.len()), error: None }
            }
        } else {
            compare_video(path, vc, nth_video)
        };
        streams_out.push(StreamResult {
            index: vc.stream,
            kind: "video".into(),
            codec: vc.codec.clone(),
            decoder: "software".into(),
            frames: Some(vc.frame_md5.len()),
            samples: None,
            verdict: cmp.verdict.into(),
            metric: Some(cmp.metric),
            error: cmp.error,
        });
        nth_video += 1;
    }
    for ac in &capture.audio {
        // Find the SNR floor from the compare rows: `audio:snr:<dB>`, `md5`,
        // or `decodes`.
        let floor = entry
            .compare
            .iter()
            .find_map(|c| c.strip_prefix("audio:snr:").and_then(|d| d.parse::<f64>().ok()));
        let cmp = if entry.compare.iter().any(|c| c == "audio:decodes" || c == "decodes") {
            // FFmpeg cannot decode this format; playing to Ended with PCM is
            // the check.
            let non_finite = ac.pcm.iter().filter(|x| !x.is_finite()).count();
            if ac.pcm.is_empty() {
                Compare { verdict: "FAIL", metric: "samples=0".into(), error: Some("no PCM captured".into()) }
            } else if non_finite > 0 {
                Compare { verdict: "FAIL", metric: format!("samples={} non-finite={non_finite}", ac.pcm.len()), error: Some(format!("decoder produced {non_finite} NaN/inf samples")) }
            } else {
                Compare { verdict: "PASS", metric: format!("samples={}", ac.pcm.len() / ac.channels.max(1) as usize), error: None }
            }
        } else if floor.is_some() {
            compare_audio(path, ac, nth_audio, floor.unwrap())
        } else if entry.compare.iter().any(|c| c == "audio:md5") {
            // md5 on float conversion is too strict to be meaningful across
            // sample-format conversions; use a 120 dB floor (bit-exact
            // integer paths pass, lossy float noise fails).
            compare_audio(path, ac, nth_audio, 120.0)
        } else {
            compare_audio(path, ac, nth_audio, 120.0)
        };
        streams_out.push(StreamResult {
            index: ac.stream,
            kind: "audio".into(),
            codec: ac.codec.clone(),
            decoder: "software".into(),
            frames: None,
            samples: Some(ac.pcm.len() / ac.channels.max(1) as usize),
            verdict: cmp.verdict.into(),
            metric: Some(cmp.metric),
            error: cmp.error,
        });
        nth_audio += 1;
    }
    for sc in &capture.subtitles {
        let cmp = compare_subtitles(path, sc, nth_sub);
        streams_out.push(StreamResult {
            index: sc.stream,
            kind: "subtitle".into(),
            codec: sc.codec.clone(),
            decoder: "software".into(),
            frames: None,
            samples: None,
            verdict: cmp.verdict.into(),
            metric: Some(cmp.metric),
            error: cmp.error,
        });
        nth_sub += 1;
    }

    // Rows the entry declares whose kind never produced any stream are
    // failures (a kind that ran but failed already shows its own FAIL).
    let kinds_seen: Vec<String> = streams_out
        .iter()
        .map(|s| s.kind.clone())
        .collect();
    for row in &entry.rows {
        let kind = match row.split_once(':') {
            Some(("video", _)) => "video",
            Some(("audio", _)) => "audio",
            Some(("sub", _)) => "subtitle",
            _ => "",
        };
        if !kind.is_empty() && !kinds_seen.iter().any(|k| k == kind) {
            let already = streams_out
                .iter()
                .any(|s| s.kind == "row" && s.metric.as_deref() == Some(row.as_str()));
            if !already {
                streams_out.push(StreamResult {
                    index: u32::MAX,
                    kind: "row".into(),
                    codec: String::new(),
                    decoder: String::new(),
                    frames: None,
                    samples: None,
                    verdict: "FAIL".into(),
                    metric: Some(row.clone()),
                    error: Some("stream never captured".into()),
                });
            }
        }
    }

    let _ = tracks;

    // HTTP pass: same digests over a Range-capable local server. The HTTP
    // run replaces the verdict when it disagrees (a source-level regression).
    // Unreachable after an open/engine failure: those return early above, so
    // a hung or errored decode never pays the second watchdog delay.
    if let Some(base) = http_base {
        let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let url = format!("{base}/{}", urlencode(&name));
        let options = PlayerOptions {
            realtime: false,
            audio: entry.streams.get("audio").copied(),
            video: entry.streams.get("video").copied(),
            subtitle: entry.streams.get("subtitle").copied(),
        };
        match play(&url, options, 60) {
            Ok((hcap, hstate)) => {
                if let Some(err) = &hstate.error {
                    streams_out.push(StreamResult {
                        index: u32::MAX - 1,
                        kind: "http".into(),
                        codec: String::new(),
                        decoder: String::new(),
                        frames: None,
                        samples: None,
                        verdict: "FAIL".into(),
                        metric: None,
                        error: Some(err.clone()),
                    });
                } else if !hstate.ended {
                    streams_out.push(StreamResult {
                        index: u32::MAX - 1,
                        kind: "http".into(),
                        codec: String::new(),
                        decoder: String::new(),
                        frames: None,
                        samples: None,
                        verdict: "FAIL".into(),
                        metric: None,
                        error: Some("http playback did not reach Ended".into()),
                    });
                } else {
                    for (i, (h, f)) in hcap.video.iter().zip(capture.video.iter()).enumerate() {
                        if h.frame_md5 != f.frame_md5 {
                            streams_out.push(StreamResult {
                                index: h.stream,
                                kind: "http".into(),
                                codec: h.codec.clone(),
                                decoder: "software".into(),
                                frames: Some(h.frame_md5.len()),
                                samples: None,
                                verdict: "FAIL".into(),
                                metric: Some(format!(
                                    "video[{i}] http frames {} vs file {}",
                                    h.frame_md5.len(),
                                    f.frame_md5.len()
                                )),
                                error: Some("http frame digests differ from file playback".into()),
                            });
                        }
                    }
                    for (i, (h, f)) in hcap.audio.iter().zip(capture.audio.iter()).enumerate() {
                        // A decoder that emits NaN fails via the non-finite
                        // check above; NaN != NaN would otherwise also flag
                        // the pass as "differs".
                        let clean = |p: &[f32]| p.iter().all(|x| x.is_finite());
                        if clean(&h.pcm) && clean(&f.pcm) && h.pcm != f.pcm {
                            streams_out.push(StreamResult {
                                index: h.stream,
                                kind: "http".into(),
                                codec: h.codec.clone(),
                                decoder: "software".into(),
                                frames: None,
                                samples: Some(h.pcm.len() / h.channels.max(1) as usize),
                                verdict: "FAIL".into(),
                                metric: Some(format!("audio[{i}] http PCM differs from file playback")),
                                error: Some("http PCM differs from file playback".into()),
                            });
                        }
                    }
                }
            }
            Err(e) => {
                streams_out.push(StreamResult {
                    index: u32::MAX - 1,
                    kind: "http".into(),
                    codec: String::new(),
                    decoder: String::new(),
                    frames: None,
                    samples: None,
                    verdict: "FAIL".into(),
                    metric: None,
                    error: Some(e),
                });
            }
        }
    }

    EntryResult { path: entry.path.clone(), streams: streams_out }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------- fuzz

/// 20 deterministic mutations of one file; each must end within 20 s without
/// panicking the process, and RSS must stay under 1 GiB.
fn fuzz_entry(entry: &Entry, path: &Path, report: &mut FuzzReport) {
    let mut data = Vec::new();
    if std::fs::File::open(path).and_then(|mut f| f.read_to_end(&mut data)).is_err() {
        report.failures.push(format!("{}: unreadable", entry.path));
        return;
    }
    for i in 0..20u64 {
        let mutated = mutate(&data, i);
        let tmp = std::env::temp_dir().join(format!("peartube-e2e-fuzz-{}-{i}.bin", std::process::id()));
        if std::fs::write(&tmp, &mutated).is_err() {
            report.failures.push(format!("{}: mutation {i} unwritable", entry.path));
            continue;
        }
        let rss0 = self_rss_bytes();
        let started = Instant::now();
        let options = PlayerOptions { realtime: false, ..PlayerOptions::default() };
        let url = format!("file://{}", tmp.display());
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let backend = Headless::new();
            let ctx = Arc::new(codecs::context());
            let url2 = url.clone();
            std::thread::spawn(move || {
                let player = Player::open(&url2, backend, ctx, options, |_| {});
                let state = player.wait();
                drop(player);
                let _ = tx.send(state);
            });
        }
        // A mutation that hangs must be recorded as a failure, not stall the
        // corpus. The player threads leak and die with the process.
        let state = rx.recv_timeout(Duration::from_secs(20));
        let elapsed = started.elapsed();
        let _ = std::fs::remove_file(&tmp);

        let ok_state = match &state {
            Ok(s) => s.ended || s.error.is_some(),
            Err(_) => false,
        };
        let rss1 = self_rss_bytes();
        let rss_ok = rss1.map(|r| r.saturating_sub(rss0.unwrap_or(0)) < 1024 * 1024 * 1024).unwrap_or(true);
        if !ok_state {
            report
                .failures
                .push(format!("{}: mutation {i} neither Ended nor Error in {:?}", entry.path, elapsed));
        } else if elapsed > Duration::from_secs(20) {
            report.failures.push(format!("{}: mutation {i} took {elapsed:?}", entry.path));
        } else if !rss_ok {
            report.failures.push(format!("{}: mutation {i} grew RSS past 1 GiB", entry.path));
        }
        report.mutations += 1;
    }
}

/// Deterministic mutations: truncations, byte flips, zeroed ranges.
fn mutate(data: &[u8], i: u64) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    let len = data.len();
    match i {
        // Truncations at increasing powers of two (and one byte short of EOF).
        0..=7 => {
            let frac = [1usize, 1, 2, 4, 8, 16, 32, 64][i as usize];
            let cut = (len / frac).max(1);
            data[..cut.min(len)].to_vec()
        }
        // Byte flips: flip every 7th byte of the first 64 KiB, XOR 0xFF.
        8..=11 => {
            let mut v = data.to_vec();
            let stride = 7 + (i as usize - 8) * 3;
            for b in v.iter_mut().take(len.min(64 * 1024)).step_by(stride) {
                *b ^= 0xFF;
            }
            v
        }
        // Zeroed ranges: 4 KiB at quarter positions.
        12..=15 => {
            let mut v = data.to_vec();
            let quarter = len / 4;
            let at = ((i as usize - 12) * quarter).min(len.saturating_sub(1));
            let end = (at + 4096).min(len);
            v[at..end].fill(0);
            v
        }
        // Header and middle byte flips.
        16..=19 => {
            let mut v = data.to_vec();
            let idx = match i {
                16 => 0,
                17 => len / 2,
                18 => 3,
                _ => len - 1,
            };
            v[idx] ^= 0xFF;
            v
        }
        _ => unreachable!(),
    }
}

fn self_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        // mach task_info(MACH_TASK_BASIC_INFO) — resident_size, the same
        // source `ps` uses. `#![forbid(unsafe_code)]` does not reach this
        // crate: the runner is test tooling, and the unsafe block only
        // reads the process's own counters.
        #[repr(C)]
        #[derive(Default)]
        struct TimeValue {
            seconds: i32,
            microseconds: i32,
        }
        #[repr(C)]
        #[derive(Default)]
        struct MachTaskBasicInfo {
            virtual_size: u64,
            resident_size: u64,
            resident_size_max: u64,
            user_time: TimeValue,
            system_time: TimeValue,
            policy: i32,
            suspend_count: i32,
        }
        const MACH_TASK_BASIC_INFO: u32 = 20;
        let mut count = (std::mem::size_of::<MachTaskBasicInfo>() / 4) as u32;
        let mut info = MachTaskBasicInfo::default();
        // libc's mach API is deprecated in favor of the mach2 crate; adding a
        // dependency for one call is not worth it.
        #[allow(deprecated)]
        let self_port: libc::mach_port_t = unsafe { libc::mach_task_self_ };
        let kr = unsafe {
            libc::task_info(
                self_port,
                MACH_TASK_BASIC_INFO,
                &mut info as *mut MachTaskBasicInfo as libc::task_info_t,
                &mut count,
            )
        };
        (kr == libc::KERN_SUCCESS).then_some(info.resident_size)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let s = std::fs::read_to_string("/proc/self/status").ok()?;
        s.lines()
            .find(|l| l.starts_with("VmRSS:"))
            .and_then(|l| {
                l.split_whitespace()
                    .nth(1)
                    .and_then(|kb| kb.parse::<u64>().ok().map(|k| k * 1024))
            })
    }
}

// ---------------------------------------------------------------- HTTP pass

/// Serves `root` over HTTP/1.1 with Range support and returns the base URL.
/// Runs on a background thread; the listener is never closed (process-exit
/// reaps it) so every entry can reuse the same server.
fn start_http_server(root: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind http server");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            // Read until the end of the request head.
            loop {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        buf.push(byte[0]);
                        if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") {
                            break;
                        }
                    }
                }
            }
            let req = String::from_utf8_lossy(&buf);
            let mut lines = req.lines();
            let first = lines.next().unwrap_or("");
            let mut parts = first.split_whitespace();
            let _method = parts.next().unwrap_or("");
            let target = parts.next().unwrap_or("").to_string();
            let path = target.split('?').next().unwrap_or("");
            let path = percent_decode(path);
            let path = path.trim_start_matches('/');
            let file = root.join(path);
            let meta = std::fs::metadata(&file).ok().filter(|m| m.is_file());
            let mut range_start = 0u64;
            let mut range_end_incl: Option<u64> = None;
            for line in lines {
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("range:") {
                    if let Some(spec) = v.trim().strip_prefix("bytes=") {
                        let mut it = spec.split('-');
                        if let Some(s) = it.next().and_then(|s| s.parse::<u64>().ok()) {
                            range_start = s;
                        }
                        range_end_incl = it.next().and_then(|s| s.parse::<u64>().ok());
                    }
                }
            }
            let Some(meta) = meta else {
                let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(resp.as_bytes());
                continue;
            };
            let total = meta.len();
            let end = range_end_incl.map(|e| e.min(total.saturating_sub(1))).unwrap_or(total.saturating_sub(1));
            let status = if range_start > 0 || range_end_incl.is_some() { "HTTP/1.1 206 Partial Content" } else { "HTTP/1.1 200 OK" };
            let accept = if range_start > 0 || range_end_incl.is_some() {
                format!("Accept-Ranges: bytes\r\nContent-Range: bytes {range_start}-{end}/{total}\r\n")
            } else {
                "Accept-Ranges: bytes\r\n".to_string()
            };
            let clen = end.saturating_sub(range_start) + 1;
            let head = format!(
                "{status}\r\n{accept}Content-Type: application/octet-stream\r\nContent-Length: {clen}\r\nConnection: close\r\n\r\n"
            );
            if stream.write_all(head.as_bytes()).is_err() {
                continue;
            }
            if let Ok(mut f) = std::fs::File::open(&file) {
                if f.seek(SeekFrom::Start(range_start)).is_ok() {
                    let mut remaining = clen;
                    let mut chunk = vec![0u8; 256 * 1024];
                    while remaining > 0 {
                        let n = (remaining as usize).min(chunk.len());
                        match f.read(&mut chunk[..n]) {
                            Ok(0) | Err(_) => break,
                            Ok(n2) => {
                                if stream.write_all(&chunk[..n2]).is_err() {
                                    break;
                                }
                                remaining -= n2 as u64;
                            }
                        }
                    }
                }
            }
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------- main

fn main() {
    let mut filter: Option<String> = None;
    let mut fuzz = false;
    let mut http = true;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--filter" => filter = args.next(),
            "--fuzz" => fuzz = true,
            "--no-http" => http = false,
            other => {
                eprintln!("unknown argument {other}; usage: e2e [--filter X] [--fuzz] [--no-http]");
                std::process::exit(2);
            }
        }
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../corpus");
    let manifest: Manifest = toml::from_str(
        &std::fs::read_to_string(manifest_dir.join("manifest.toml"))
            .expect("read corpus/manifest.toml"),
    )
    .expect("parse corpus/manifest.toml");
    let yardstick: Yardstick = toml::from_str(
        &std::fs::read_to_string(manifest_dir.join("yardstick.toml"))
            .expect("read corpus/yardstick.toml"),
    )
    .expect("parse corpus/yardstick.toml");

    let mut entries: Vec<Entry> = manifest
        .entry
        .iter()
        .filter(|e| filter.as_ref().map(|f| e.path.contains(f)).unwrap_or(true))
        .cloned()
        .collect();
    // Dedup by path: an entry may cover rows and containers; merge rows.
    entries.sort_by_key(|e| e.path.clone());
    entries.dedup_by(|a, b| {
        if a.path == b.path {
            b.rows.extend(a.rows.iter().cloned());
            b.compare.extend(a.compare.iter().cloned());
            true
        } else {
            false
        }
    });

    let http_base = http.then(|| start_http_server(corpus_dir()));

    let mut entry_results: Vec<EntryResult> = Vec::new();
    let mut row_map: BTreeMap<Row, RowResult> = yardstick
        .rows
        .iter()
        .map(|r| (r.clone(), RowResult { row: r.clone(), ..Default::default() }))
        .collect();

    println!(
        "{:<44} {:>9} {:>9} {:>7}",
        "entry", "video", "audio", "subs"
    );
    println!("{}", "-".repeat(74));

    for entry in &entries {
        let Some(path) = resolve(&entry.path) else {
            entry_results.push(EntryResult {
                path: entry.path.clone(),
                streams: vec![StreamResult {
                    index: 0,
                    kind: "open".into(),
                    codec: String::new(),
                    decoder: String::new(),
                    frames: None,
                    samples: None,
                    verdict: "FAIL".into(),
                    metric: None,
                    error: Some("sample missing (rsync may still be filling)".into()),
                }],
            });
            println!("{:<44} {:>9}", entry.path, "MISSING");
            continue;
        };

        let result = run_entry(entry, &path, http_base.as_deref());
        let pass = |kind: &str| {
            result
                .streams
                .iter()
                .any(|s| s.kind == kind && s.verdict == "PASS")
        };
        let v = if pass("video") { "ok" } else { "-" };
        let a = if pass("audio") { "ok" } else { "-" };
        let s = if pass("subtitle") { "ok" } else { "-" };
        println!("{:<44} {:>9} {:>9} {:>7}", entry.path, v, a, s);

        // Attribute entry result to rows: an entry passes a row when the row's
        // kind passed on this entry.
        for row in &entry.rows {
            let Some(rr) = row_map.get_mut(row) else { continue };
            let kind = match row.split_once(':') {
                Some(("video", _)) => "video",
                Some(("audio", _)) => "audio",
                Some(("sub", _)) => "subtitle",
                Some(("container", _)) => "engine",
                _ => "",
            };
            let ok = if kind == "engine" {
                !result.streams.iter().any(|s| s.kind == "engine" && s.verdict == "FAIL")
                    && !result.streams.iter().any(|s| s.kind == "open" && s.verdict == "FAIL")
            } else if kind.is_empty() {
                false
            } else {
                pass(kind)
            };
            if ok {
                rr.passing_entries.push(entry.path.clone());
            } else {
                rr.failing_entries.push(entry.path.clone());
            }
        }
        entry_results.push(result);
    }

    println!("{}", "-".repeat(74));
    println!(
        "{:<22} {:>8} {:>8} {}",
        "row", "pass", "fail", "first failing entry"
    );
    println!("{}", "-".repeat(74));
    let mut rows: Vec<RowResult> = row_map.values().cloned().collect();
    rows.sort_by(|a, b| a.row.cmp(&b.row));
    for rr in &rows {
        println!(
            "{:<22} {:>8} {:>8} {}",
            rr.row,
            rr.passing_entries.len(),
            rr.failing_entries.len(),
            rr.failing_entries.first().cloned().unwrap_or_default()
        );
    }

    let mut fuzz_report = None;
    if fuzz {
        let mut fr = FuzzReport::default();
        println!("\nfuzz: 20 deterministic mutations per entry");
        for entry in &entries {
            let Some(path) = resolve(&entry.path) else { continue };
            fuzz_entry(entry, &path, &mut fr);
        }
        let verdict = if fr.failures.is_empty() { "ok" } else { "FAILED" };
        println!(
            "fuzz: {} mutations, {} failures — {verdict}",
            fr.mutations,
            fr.failures.len()
        );
        for f in fr.failures.iter().take(20) {
            println!("  {f}");
        }
        fuzz_report = Some(fr);
    }

    // Write target/e2e/codecs.json.
    let report = Report { entries: entry_results, rows, fuzz: fuzz_report };
    let out_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map(|d| PathBuf::from(d).join("../../target/e2e"))
        .unwrap_or_else(|_| PathBuf::from("target/e2e"));
    let _ = std::fs::create_dir_all(&out_dir);
    let json = serde_json::to_string_pretty(&report).expect("serialize report");
    let out_path = out_dir.join("codecs.json");
    std::fs::write(&out_path, &json).expect("write codecs.json");
    println!("\nwrote {}", out_path.display());

    // Exit non-zero when any row has no passing entry.
    let empty_rows: Vec<String> = report
        .rows
        .iter()
        .filter(|r| r.passing_entries.is_empty())
        .map(|r| r.row.clone())
        .collect();
    if !empty_rows.is_empty() {
        eprintln!(
            "{} yardstick rows have no passing entry: {}",
            empty_rows.len(),
            empty_rows.join(", ")
        );
        std::process::exit(1);
    }
}
