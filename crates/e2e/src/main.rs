//! Corpus runner: plays every manifest entry through `player::Player` with the
//! `Headless` backend, compares every stream with FFmpeg, and writes
//! `target/e2e/codecs.json`.
//!
//! ```text
//! cargo run -p e2e --release -- [--filter X] [--fuzz]
//! ```

mod compare;
mod manifest;
mod oracle;
mod tool;

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use compare::{Compare, Verdict};
use manifest::{Entry, Kind, Policy};
use oxideav_core::{MediaType, RuntimeContext};
use player::{Headless, Player, PlayerOptions};
use serde::Serialize;

/// Yardstick row (a `<kind>:<id>` from corpus/yardstick.toml).
type Row = String;

// ---------------------------------------------------------------- manifest

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
    /// The FFmpeg stream the comparison used (`0:<index>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    ffmpeg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frames: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    samples: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy: Option<String>,
    verdict: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    metric: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Non-accepting measurements (`diag:` tokens).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    diagnostics: Vec<String>,
}

#[derive(Serialize)]
struct EntryResult {
    path: String,
    /// The container the player's probe rule picked.
    #[serde(skip_serializing_if = "Option::is_none")]
    demuxer: Option<String>,
    /// Every track the player offered, and which of them were checked: only
    /// the selected track of each kind plays, so the rest are unchecked.
    tracks: Vec<TrackReport>,
    streams: Vec<StreamResult>,
}

#[derive(Serialize)]
struct TrackReport {
    stream: u32,
    kind: Kind,
    codec: String,
    /// `default` (the player's choice), `explicit` (the manifest's), or
    /// `not selected`.
    selection: &'static str,
    checked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    ffmpeg: Option<String>,
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

/// The registry the player plays with, for discovering streams the way the
/// player does.
fn registry() -> &'static RuntimeContext {
    static CTX: OnceLock<RuntimeContext> = OnceLock::new();
    CTX.get_or_init(codecs::context)
}

/// One track the player offers.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct TrackInfo {
    stream: u32,
    kind: Kind,
    codec: String,
}

/// What the player is offered, found the way the player finds it: its probe
/// rule and registry, and the engine's track filter (audio, video within the
/// size caps, subtitles; at most 64 streams).
struct Discovery {
    demuxer: String,
    tracks: Vec<TrackInfo>,
}

fn discover(path: &Path) -> Result<Discovery, String> {
    let ctx = registry();
    let demuxer = refcheck::probe_container(ctx, path)?;
    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.containers.open_demuxer(&demuxer, Box::new(file), &ctx.codecs)
    }));
    let d = match opened {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return Err(format!("failed to open demuxer: {e}")),
        Err(_) => return Err(format!("the {demuxer} demuxer panicked while opening")),
    };
    let mut tracks = Vec::new();
    for s in d.streams().iter().take(64) {
        let kind = match s.params.media_type {
            MediaType::Audio => Kind::Audio,
            MediaType::Video => {
                let (w, h) = (s.params.width.unwrap_or(0), s.params.height.unwrap_or(0));
                if w > 16384 || h > 16384 || u64::from(w) * u64::from(h) > 8192 * 8192 {
                    continue;
                }
                Kind::Video
            }
            MediaType::Subtitle => Kind::Subtitle,
            _ => continue,
        };
        tracks.push(TrackInfo { stream: s.index, kind, codec: s.params.codec_id.as_str().to_string() });
    }
    Ok(Discovery { demuxer, tracks })
}

fn track_kind(kind: player::TrackKind) -> Kind {
    match kind {
        player::TrackKind::Video => Kind::Video,
        player::TrackKind::Audio => Kind::Audio,
        player::TrackKind::Subtitle => Kind::Subtitle,
    }
}

/// A track the runner plays and checks: the manifest's choice for its kind
/// (`explicit`), else the player's default (`default`: the first video and
/// audio track; for subtitles, which the player leaves off, the runner
/// selects the first).
struct Selected {
    track: TrackInfo,
    how: &'static str,
}

fn select(entry: &Entry, disc: &Discovery) -> Result<Vec<Selected>, String> {
    let mut out = Vec::new();
    for (kind, explicit) in [
        (Kind::Video, entry.selection.video),
        (Kind::Audio, entry.selection.audio),
        (Kind::Subtitle, entry.selection.subtitle),
    ] {
        let mut of_kind = disc.tracks.iter().filter(|t| t.kind == kind);
        match explicit {
            Some(index) => {
                let track = of_kind.find(|t| t.stream == index).ok_or_else(|| {
                    format!("streams.{} = {index} is not one of this file's {} tracks", kind.name(), kind.name())
                })?;
                out.push(Selected { track: track.clone(), how: "explicit" });
            }
            None => {
                if let Some(track) = of_kind.next() {
                    out.push(Selected { track: track.clone(), how: "default" });
                }
            }
        }
    }
    Ok(out)
}

/// FFmpeg's stream for a selected track: the same position among FFmpeg's
/// streams of the kind (cover art excluded) as among the player's tracks of
/// the kind. Both lists must be equally long, or the position means nothing.
fn map_to_ffmpeg(track: &TrackInfo, disc: &Discovery, ff: &[oracle::FfStream]) -> Result<oracle::FfStream, String> {
    let ours: Vec<u32> = disc.tracks.iter().filter(|t| t.kind == track.kind).map(|t| t.stream).collect();
    let theirs = oracle::of_type(ff, track.kind.ffmpeg_type());
    if ours.len() != theirs.len() {
        return Err(format!(
            "the player offers {} {} track(s), FFmpeg {}: stream {} has no FFmpeg counterpart",
            ours.len(),
            track.kind.name(),
            theirs.len(),
            track.stream
        ));
    }
    let nth = ours.iter().position(|&s| s == track.stream).ok_or(format!("stream {} not offered", track.stream))?;
    Ok(theirs[nth].clone())
}

/// Subtitle packet count of FFmpeg's stream `index` by `ffprobe
/// -count_packets`, bounded by [`tool::ffprobe`] (stdin closed, 30 s kill
/// timeout: ffmpeg's subtitle parsers can spin on malformed samples).
fn ffprobe_subtitle_packets(path: &Path, index: u32) -> Result<usize, String> {
    let args: Vec<String> = [
        "-select_streams", &index.to_string(), "-count_packets", "-show_entries", "stream=nb_read_packets",
        "-of", "csv=p=0", path.to_str().ok_or("path not utf8")?,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let out = tool::ffprobe(&args, Duration::from_secs(30))?;
    String::from_utf8_lossy(&out)
        .trim()
        .parse::<usize>()
        .map_err(|e| format!("ffprobe output: {e}"))
}

fn compare_video(path: &Path, cap: &player::VideoCapture, ff: &oracle::FfStream) -> Compare {
    let output = format!("frames={}", cap.frame_md5.len());
    if cap.frame_md5.is_empty() {
        return Compare::fail(output, "no frames captured");
    }
    let Some(pix) = refcheck::ffmpeg_pix_fmt_name(cap.pixel_format) else {
        return Compare::fail(output, format!("FFmpeg has no pixel format for {:?}", cap.pixel_format));
    };
    let expect = match oracle::video_md5s(path, &ff.map(), pix) {
        Ok(e) => e,
        Err(e) => return Compare::fail(output, e),
    };
    let got = &cap.frame_md5;
    let matched = got.iter().filter(|m| expect.contains(m)).count();
    let metric = format!("frames={} matched {matched}/{}", got.len(), expect.len());
    if expect == *got {
        Compare::pass(metric)
    } else {
        Compare::fail(metric, "frame digests differ from FFmpeg's")
    }
}

/// FFmpeg's stream must decode to the capture's channel count and rate: no
/// sample comparison means anything until those agree.
fn check_audio_layout(cap: &player::AudioCapture, ff: &oracle::FfStream) -> Result<(), String> {
    if ff.channels != Some(cap.channels) || ff.sample_rate != Some(cap.sample_rate) {
        return Err(format!(
            "{} ch {} Hz vs FFmpeg {:?} ch {:?} Hz",
            cap.channels, cap.sample_rate, ff.channels, ff.sample_rate
        ));
    }
    Ok(())
}

/// The player's PCM for one audio stream against FFmpeg's decode of `ff`,
/// under `policy`.
fn compare_audio(path: &Path, cap: &player::AudioCapture, ff: &oracle::FfStream, policy: Policy) -> Compare {
    let samples = cap.pcm.len() / cap.channels.max(1) as usize;
    let metric = format!("samples={samples}");
    if let Err(e) = check_audio_layout(cap, ff) {
        return Compare::fail(metric, e);
    }
    match policy {
        Policy::AudioMd5 => {
            let Some(pcm) = ff.sample_fmt.as_deref().and_then(oracle::Pcm::of_sample_fmt) else {
                return Compare::fail(metric, format!("no canonical PCM for FFmpeg's {:?}", ff.sample_fmt));
            };
            match oracle::audio_pcm(path, &ff.map(), pcm)
                .and_then(|reference| compare::exact_pcm(&cap.pcm, &reference, pcm, cap.channels as usize))
            {
                Ok(m) => Compare::pass(m),
                Err(e) => Compare::fail(metric, e),
            }
        }
        Policy::AudioSnr(floor) => {
            let slack = oracle::audio_frames(path, ff.index).and_then(|frames| compare::lossy_slack(&frames, cap.channels));
            let reference = oracle::audio_f32(path, &ff.map());
            match (slack, reference) {
                (Ok(slack), Ok(reference)) => {
                    let mut c = compare::snr_pcm(&cap.pcm, &reference, slack, floor);
                    c.metric = format!("{metric} {}", c.metric);
                    c
                }
                (Err(e), _) | (_, Err(e)) => Compare::fail(metric, e),
            }
        }
        other => misapplied(other, Kind::Audio),
    }
}

/// `diag:audio:snr:<dB>`: the SNR against each diagnostic floor, reported
/// for formats whose decoders do not reach the contract yet. Never a pass.
fn audio_diagnostics(path: &Path, cap: &player::AudioCapture, ff: &oracle::FfStream, floors: &[f64]) -> Vec<String> {
    if floors.is_empty() || cap.pcm.is_empty() {
        return Vec::new();
    }
    let snr = check_audio_layout(cap, ff).and_then(|()| {
        let slack = compare::lossy_slack(&oracle::audio_frames(path, ff.index)?, cap.channels)?;
        refcheck::try_snr_db(&oracle::audio_f32(path, &ff.map())?, &cap.pcm, slack)
    });
    floors
        .iter()
        .map(|floor| match &snr {
            Ok(snr) if *snr >= *floor => format!("snr {snr:.1} dB meets the diagnostic {floor} dB (non-accepting)"),
            Ok(snr) => format!("snr {snr:.1} dB below the diagnostic {floor} dB (non-accepting)"),
            Err(e) => format!("snr for the diagnostic {floor} dB: {e}"),
        })
        .collect()
}

fn compare_subtitles(path: &Path, cap: &player::SubtitleCapture, ff: &oracle::FfStream) -> Compare {
    let expect = match ffprobe_subtitle_packets(path, ff.index) {
        Ok(n) => n,
        Err(e) => return Compare::fail(format!("shows={}", cap.shows.len()), e),
    };
    // The pipeline shows each cue then clears it, so `shows` counts cleared
    // events too; the cue count is the number of non-empty shows.
    let cues = cap.shows.iter().filter(|(_, n)| *n > 0).count();
    let metric = format!("shows={} ffmpeg_packets={expect}, cues={cues}", cap.shows.len());
    if cues == expect {
        Compare::pass(metric)
    } else {
        Compare::fail(metric, format!("cue count {cues} != ffprobe {expect}"))
    }
}

// ---------------------------------------------------------------- entries

/// The manifest's policy for one selected stream's kind, judged: a missing
/// policy fails, and a `decodes` policy fails when FFmpeg does decode the
/// stream (it is only for formats FFmpeg cannot produce a reference for).
/// Otherwise `compare` runs under the policy with FFmpeg's stream, or fails
/// with why there is none.
fn judge(
    entry: &Entry,
    path: &Path,
    kind: Kind,
    ff: &Result<oracle::FfStream, String>,
    output: &str,
    compare: impl FnOnce(Policy, &oracle::FfStream) -> Compare,
    decodes: impl FnOnce() -> Compare,
) -> (Option<Policy>, Compare) {
    let Some(&policy) = entry.policies.get(&kind) else {
        return (None, Compare::fail(output, format!("the manifest declares no {} policy for this entry", kind.name())));
    };
    let cmp = match (policy, ff) {
        (Policy::Decodes(_), Ok(ff)) if oracle::decodes(path, &ff.map()) => Compare::fail(
            output,
            format!("FFmpeg decodes {}: declare an oracle policy, not {}", ff.map(), policy.token()),
        ),
        (Policy::Decodes(_), _) => decodes(),
        (_, Ok(ff)) => compare(policy, ff),
        (_, Err(e)) => Compare::fail(output, e.clone()),
    };
    (Some(policy), cmp)
}

/// The policy does not apply to the stream's kind (the manifest assigns
/// policies per kind, so this is a runner bug).
fn misapplied(policy: Policy, kind: Kind) -> Compare {
    Compare::fail("", format!("{} applied to a {} stream", policy.token(), kind.name()))
}

impl StreamResult {
    /// A failure of the entry as a whole (`kind` = open, engine, tracks,
    /// selection, http, row).
    fn entry_level(kind: &str, index: u32, metric: Option<String>, error: impl Into<String>) -> Self {
        StreamResult {
            index,
            kind: kind.into(),
            codec: String::new(),
            decoder: String::new(),
            ffmpeg: None,
            frames: None,
            samples: None,
            policy: None,
            verdict: Verdict::Fail.as_str().into(),
            metric,
            error: Some(error.into()),
            diagnostics: Vec::new(),
        }
    }

    fn judged(index: u32, kind: Kind, codec: &str, policy: Option<Policy>, cmp: Compare) -> Self {
        StreamResult {
            index,
            kind: kind.name().into(),
            codec: codec.into(),
            decoder: "software".into(),
            ffmpeg: None,
            frames: None,
            samples: None,
            policy: policy.map(Policy::token),
            verdict: cmp.verdict.as_str().into(),
            metric: Some(cmp.metric),
            error: cmp.error,
            diagnostics: Vec::new(),
        }
    }
}

/// Judges one selected track against its capture.
fn judge_track(
    entry: &Entry,
    path: &Path,
    sel: &Selected,
    ff: &Result<oracle::FfStream, String>,
    capture: &player::Capture,
    state: &player::State,
) -> StreamResult {
    let t = &sel.track;
    let missing = || {
        let why = state.error.as_deref().map(|e| format!(" (engine: {e})")).unwrap_or_default();
        format!("selected stream {} never captured{why}", t.stream)
    };
    let mut r = match t.kind {
        Kind::Video => {
            let cap = capture.video.iter().find(|c| c.stream == t.stream);
            let frames = cap.map_or(0, |c| c.frame_md5.len());
            let output = format!("frames={frames}");
            let (policy, cmp) = judge(
                entry,
                path,
                Kind::Video,
                ff,
                &output,
                |policy, ff| match (policy, cap) {
                    (Policy::VideoMd5, Some(cap)) => compare_video(path, cap, ff),
                    (Policy::VideoMd5, None) => Compare::fail(output.clone(), missing()),
                    (other, _) => misapplied(other, Kind::Video),
                },
                || match cap {
                    Some(cap) if !cap.frame_md5.is_empty() => Compare::decodes(output.clone()),
                    _ => Compare::fail(output.clone(), missing()),
                },
            );
            let mut r = StreamResult::judged(t.stream, Kind::Video, &t.codec, policy, cmp);
            r.frames = Some(frames);
            r
        }
        Kind::Audio => {
            let cap = capture.audio.iter().find(|c| c.stream == t.stream);
            let samples = cap.map_or(0, |c| c.pcm.len() / c.channels.max(1) as usize);
            let output = format!("samples={samples}");
            let non_finite = cap.map_or(0, |c| c.pcm.iter().filter(|x| !x.is_finite()).count());
            let usable = || match cap {
                None => Err(Compare::fail(output.clone(), missing())),
                Some(c) if c.pcm.is_empty() => Err(Compare::fail(output.clone(), "no PCM captured")),
                Some(_) if non_finite > 0 => Err(Compare::fail(
                    format!("{output} non-finite={non_finite}"),
                    format!("decoder produced {non_finite} NaN/inf samples"),
                )),
                Some(c) => Ok(c),
            };
            let (policy, cmp) = judge(
                entry,
                path,
                Kind::Audio,
                ff,
                &output,
                |policy, ff| match usable() {
                    Ok(cap) => compare_audio(path, cap, ff, policy),
                    Err(fail) => fail,
                },
                || match usable() {
                    Ok(_) => Compare::decodes(output.clone()),
                    Err(fail) => fail,
                },
            );
            let mut r = StreamResult::judged(t.stream, Kind::Audio, &t.codec, policy, cmp);
            r.samples = Some(samples);
            if let (Some(cap), Ok(ff)) = (cap, ff) {
                r.diagnostics = audio_diagnostics(path, cap, ff, &entry.diagnostics);
            }
            r
        }
        Kind::Subtitle => {
            let cap = capture.subtitles.iter().find(|c| c.stream == t.stream);
            let shown = cap.map_or(0, |c| c.shows.iter().filter(|(_, n)| *n > 0).count());
            let output = format!("cues={shown}");
            let (policy, cmp) = judge(
                entry,
                path,
                Kind::Subtitle,
                ff,
                &output,
                |policy, ff| match (policy, cap) {
                    (Policy::SubCount, Some(cap)) => compare_subtitles(path, cap, ff),
                    (Policy::SubCount, None) => Compare::fail(output.clone(), missing()),
                    (other, _) => misapplied(other, Kind::Subtitle),
                },
                || {
                    if shown > 0 { Compare::decodes(output.clone()) } else { Compare::fail(output.clone(), missing()) }
                },
            );
            StreamResult::judged(t.stream, Kind::Subtitle, &t.codec, policy, cmp)
        }
    };
    r.ffmpeg = ff.as_ref().ok().map(oracle::FfStream::map);
    r
}

/// Plays one entry and returns its per-stream results. The tracks to check
/// are discovered as the player discovers them, selected per the manifest
/// (else the player's defaults), and each is compared with FFmpeg's stream at
/// the same position among the streams of its kind. With `http_base`, the
/// entry is played a second time over HTTP from a Range-capable local server
/// and the same digests are compared.
fn run_entry(entry: &Entry, path: &Path, http_base: Option<&str>) -> EntryResult {
    let mut result = EntryResult { path: entry.path.clone(), demuxer: None, tracks: Vec::new(), streams: Vec::new() };

    let disc = match discover(path) {
        Ok(d) => d,
        Err(e) => {
            result.streams.push(StreamResult::entry_level("open", 0, None, e));
            return result;
        }
    };
    result.demuxer = Some(disc.demuxer.clone());
    let selected = match select(entry, &disc) {
        Ok(s) => s,
        Err(e) => {
            result.streams.push(StreamResult::entry_level("selection", 0, None, e));
            return result;
        }
    };
    let options = PlayerOptions {
        realtime: false,
        video: entry.selection.video,
        audio: entry.selection.audio,
        subtitle: selected.iter().find(|s| s.track.kind == Kind::Subtitle).map(|s| s.track.stream),
    };
    let ff_streams = oracle::streams(path);

    let (capture, state) = match play(&path.to_string_lossy(), options.clone(), 300) {
        Ok(r) => r,
        Err(e) => {
            result.streams.push(StreamResult::entry_level("open", 0, None, e));
            return result;
        }
    };
    if !state.ended {
        let error = state.error.clone().unwrap_or_else(|| "playback did not reach Ended".into());
        let metric = Some(format!("ended=false position={:?}", state.position));
        result.streams.push(StreamResult::entry_level("engine", 0, metric, error));
        return result;
    }
    // Playback ended but reported an error (e.g. a stream without a
    // decoder): the entry fails, the captured streams are still judged.
    if let Some(err) = &state.error {
        let metric = Some(format!("ended=true position={:?}", state.position));
        result.streams.push(StreamResult::entry_level("engine", 0, metric, err.clone()));
    }

    let offered: Vec<TrackInfo> = state
        .tracks
        .iter()
        .map(|t| TrackInfo { stream: t.stream, kind: track_kind(t.kind), codec: t.codec.clone() })
        .collect();
    if offered != disc.tracks {
        result.streams.push(StreamResult::entry_level(
            "tracks",
            u32::MAX,
            Some(format!("player {offered:?} vs discovered {:?}", disc.tracks)),
            "the player offered other tracks than discovery found",
        ));
    }
    for sel in &selected {
        let current = match sel.track.kind {
            Kind::Video => state.video,
            Kind::Audio => state.audio,
            Kind::Subtitle => state.subtitle,
        };
        if current != Some(sel.track.stream) {
            result.streams.push(StreamResult::entry_level(
                "selection",
                sel.track.stream,
                Some(format!("player {} = {current:?}", sel.track.kind.name())),
                format!("the player did not play {} stream {}", sel.track.kind.name(), sel.track.stream),
            ));
        }
    }

    for sel in &selected {
        let ff = ff_streams.as_ref().map_err(Clone::clone).and_then(|ff| map_to_ffmpeg(&sel.track, &disc, ff));
        result.streams.push(judge_track(entry, path, sel, &ff, &capture, &state));
    }
    let selected_stream = |s: u32| selected.iter().any(|sel| sel.track.stream == s);
    for s in capture
        .video
        .iter()
        .map(|c| c.stream)
        .chain(capture.audio.iter().map(|c| c.stream))
        .chain(capture.subtitles.iter().map(|c| c.stream))
        .filter(|&s| !selected_stream(s))
    {
        result.streams.push(StreamResult::entry_level("selection", s, None, format!("stream {s} captured unselected")));
    }
    result.tracks = disc
        .tracks
        .iter()
        .map(|t| {
            let sel = selected.iter().find(|s| s.track.stream == t.stream);
            TrackReport {
                stream: t.stream,
                kind: t.kind,
                codec: t.codec.clone(),
                selection: sel.map_or("not selected", |s| s.how),
                checked: sel.is_some(),
                ffmpeg: result.streams.iter().find(|r| sel.is_some() && r.index == t.stream).and_then(|r| r.ffmpeg.clone()),
            }
        })
        .collect();

    // Rows the entry declares whose kind never produced any stream are
    // failures (a kind that ran but failed already shows its own FAIL).
    for row in &entry.rows {
        let kind = match row.split_once(':') {
            Some(("video", _)) => "video",
            Some(("audio", _)) => "audio",
            Some(("sub", _)) => "subtitle",
            _ => "",
        };
        if !kind.is_empty() && !result.streams.iter().any(|s| s.kind == kind) {
            let already = result.streams.iter().any(|s| s.kind == "row" && s.metric.as_deref() == Some(row.as_str()));
            if !already {
                result.streams.push(StreamResult::entry_level("row", u32::MAX, Some(row.clone()), "stream never captured"));
            }
        }
    }

    // HTTP pass: same digests over a Range-capable local server. The HTTP
    // run replaces the verdict when it disagrees (a source-level regression).
    // Unreachable after an open/engine failure: those return early above, so
    // a hung or errored decode never pays the second watchdog delay.
    if let Some(base) = http_base {
        let url = format!("{base}/{}", http_path(&entry.path));
        match play(&url, options, 300) {
            Ok((hcap, hstate)) => {
                if hstate.error != state.error {
                    let error = format!("http playback error {:?} vs file {:?}", hstate.error, state.error);
                    result.streams.push(StreamResult::entry_level("http", u32::MAX - 1, None, error));
                } else if !hstate.ended {
                    result.streams.push(StreamResult::entry_level(
                        "http",
                        u32::MAX - 1,
                        None,
                        "http playback did not reach Ended",
                    ));
                } else {
                    for (i, (h, f)) in hcap.video.iter().zip(capture.video.iter()).enumerate() {
                        if h.frame_md5 != f.frame_md5 {
                            let metric =
                                format!("video[{i}] http frames {} vs file {}", h.frame_md5.len(), f.frame_md5.len());
                            let mut r = StreamResult::entry_level(
                                "http",
                                h.stream,
                                Some(metric),
                                "http frame digests differ from file playback",
                            );
                            r.codec = h.codec.clone();
                            r.frames = Some(h.frame_md5.len());
                            result.streams.push(r);
                        }
                    }
                    for (i, (h, f)) in hcap.audio.iter().zip(capture.audio.iter()).enumerate() {
                        // A decoder that emits NaN fails via the non-finite
                        // check above; NaN != NaN would otherwise also flag
                        // the pass as "differs".
                        let clean = |p: &[f32]| p.iter().all(|x| x.is_finite());
                        if clean(&h.pcm) && clean(&f.pcm) && h.pcm != f.pcm {
                            let metric = format!("audio[{i}] http PCM differs from file playback");
                            let mut r = StreamResult::entry_level(
                                "http",
                                h.stream,
                                Some(metric),
                                "http PCM differs from file playback",
                            );
                            r.codec = h.codec.clone();
                            r.samples = Some(h.pcm.len() / h.channels.max(1) as usize);
                            result.streams.push(r);
                        }
                    }
                }
            }
            Err(e) => result.streams.push(StreamResult::entry_level("http", u32::MAX - 1, None, e)),
        }
    }

    result
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

/// Serves every manifest sample over HTTP/1.1 with Range support, at
/// `/<kind>/<path>` for the manifest path `<kind>:<path>` (FATE samples and
/// generated files alike), one thread per connection, and returns the base
/// URL. The listener is never closed (process exit reaps it), so every entry
/// reuses the same server.
fn start_http_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind http server");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            // A response the client stopped reading (a seek dropped it)
            // must not hold up the next request.
            std::thread::spawn(move || serve_http(stream));
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// The URL path the HTTP pass serves a manifest path at.
fn http_path(manifest_path: &str) -> String {
    let (kind, rel) = manifest_path.split_once(':').unwrap_or(("", manifest_path));
    let rel: Vec<String> = rel.split('/').map(urlencode).collect();
    format!("{kind}/{}", rel.join("/"))
}

/// Answers one request: the sample at the URL path, or a byte range of it.
fn serve_http(mut stream: std::net::TcpStream) {
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
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("").to_string();
    let path = target.split('?').next().unwrap_or("");
    let path = percent_decode(path);
    let path = path.trim_start_matches('/');
    let file = path
        .split_once('/')
        .and_then(|(kind, rel)| resolve(&format!("{kind}:{rel}")));
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
    let Some((file, meta)) = file.and_then(|f| std::fs::metadata(&f).ok().map(|m| (f, m))) else {
        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(resp.as_bytes());
        return;
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
    if stream.write_all(head.as_bytes()).is_err() || method == "HEAD" {
        return;
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
    let yardstick: Yardstick = toml::from_str(
        &std::fs::read_to_string(manifest_dir.join("yardstick.toml"))
            .expect("read corpus/yardstick.toml"),
    )
    .expect("parse corpus/yardstick.toml");
    let manifest_text = std::fs::read_to_string(manifest_dir.join("manifest.toml")).expect("read corpus/manifest.toml");
    // Records naming the same sample and stream selection merge per kind;
    // contradictory policies, unknown tokens and sub-contract floors stop
    // the run before anything plays.
    let entries: Vec<Entry> = match manifest::parse(&manifest_text, &yardstick.rows) {
        Ok(entries) => entries
            .into_iter()
            .filter(|e| filter.as_ref().map(|f| e.path.contains(f)).unwrap_or(true))
            .collect(),
        Err(errors) => {
            for e in &errors {
                eprintln!("corpus/manifest.toml: {e}");
            }
            std::process::exit(2);
        }
    };

    let http_base = http.then(start_http_server);

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
                demuxer: None,
                tracks: Vec::new(),
                streams: vec![StreamResult::entry_level(
                    "open",
                    0,
                    None,
                    "sample missing (rsync may still be filling)",
                )],
            });
            println!("{:<44} {:>9}", entry.path, "MISSING");
            continue;
        };

        let result = run_entry(entry, &path, http_base.as_deref());
        let pass = |kind: &str| {
            result
                .streams
                .iter()
                .any(|s| s.kind == kind && (s.verdict == "PASS" || s.verdict == "DECODES"))
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

    // Exit non-zero when a row has no passing entry. Under --filter only the
    // rows the selected entries cover are judged.
    let empty_rows: Vec<String> = report
        .rows
        .iter()
        .filter(|r| r.passing_entries.is_empty())
        .filter(|r| filter.is_none() || !r.failing_entries.is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn track(stream: u32, kind: Kind, codec: &str) -> TrackInfo {
        TrackInfo { stream, kind, codec: codec.into() }
    }

    fn ff(index: u32, codec_type: &str, codec_name: &str, attached_pic: bool) -> oracle::FfStream {
        oracle::FfStream {
            index,
            codec_type: codec_type.into(),
            codec_name: codec_name.into(),
            codec_tag: String::new(),
            sample_fmt: None,
            sample_rate: None,
            channels: None,
            attached_pic,
        }
    }

    fn entry(selection: manifest::Selection) -> Entry {
        Entry {
            path: "fate:h264/h264_intra_first-small.ts".into(),
            rows: Vec::new(),
            policies: BTreeMap::new(),
            diagnostics: Vec::new(),
            selection,
        }
    }

    /// h264_intra_first-small.ts: H.264 and two MP2 tracks of different
    /// content, in the same order in OxideAV's demuxer and FFmpeg's.
    fn two_audio_tracks() -> (Discovery, Vec<oracle::FfStream>) {
        let disc = Discovery {
            demuxer: "mpegts".into(),
            tracks: vec![track(0, Kind::Video, "h264"), track(1, Kind::Audio, "mp2"), track(2, Kind::Audio, "mp2")],
        };
        let ff = vec![ff(0, "video", "h264", false), ff(1, "audio", "mp2", false), ff(2, "audio", "mp2", false)];
        (disc, ff)
    }

    #[test]
    fn a_selected_second_track_maps_to_ffmpegs_second_stream_of_its_kind() {
        let (disc, ff) = two_audio_tracks();
        let explicit = manifest::Selection { audio: Some(2), ..Default::default() };
        let selected = select(&entry(explicit), &disc).unwrap();
        let audio = selected.iter().find(|s| s.track.kind == Kind::Audio).unwrap();
        assert_eq!((audio.track.stream, audio.how), (2, "explicit"));
        // The old runner counted captured streams of the kind from zero and
        // compared this track with FFmpeg's first audio stream, 0:a:0 = 0:1.
        assert_eq!(map_to_ffmpeg(&audio.track, &disc, &ff).unwrap().map(), "0:2");
        let default = select(&entry(manifest::Selection::default()), &disc).unwrap();
        let audio = default.iter().find(|s| s.track.kind == Kind::Audio).unwrap();
        assert_eq!((audio.track.stream, audio.how), (1, "default"));
        assert_eq!(map_to_ffmpeg(&audio.track, &disc, &ff).unwrap().map(), "0:1");
    }

    #[test]
    fn cover_art_is_not_a_video_track_and_unequal_lists_do_not_map() {
        let disc = Discovery { demuxer: "mp3".into(), tracks: vec![track(0, Kind::Audio, "mp3")] };
        let with_cover = vec![ff(0, "audio", "mp3", false), ff(1, "video", "mjpeg", true)];
        assert_eq!(map_to_ffmpeg(&disc.tracks[0], &disc, &with_cover).unwrap().map(), "0:0");
        let (disc, mut ff) = two_audio_tracks();
        ff.pop();
        assert!(map_to_ffmpeg(&disc.tracks[2], &disc, &ff).is_err(), "FFmpeg lists one audio stream, the player two");
    }

    #[test]
    fn an_explicit_selection_must_name_a_track_of_its_kind() {
        let (disc, _) = two_audio_tracks();
        let wrong = manifest::Selection { audio: Some(0), ..Default::default() };
        assert!(select(&entry(wrong), &disc).is_err(), "stream 0 is video");
    }

    #[test]
    fn ffprobe_subtitle_packets_counts_a_fate_sample() {
        // ffprobe -select_streams s:0 -count_packets on the SubRip tester: 37.
        let path = refcheck::fate("sub/SubRip_capability_tester.srt");
        assert_eq!(ffprobe_subtitle_packets(&path, 0), Ok(37));
    }
}
