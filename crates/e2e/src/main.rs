//! Corpus runner: plays every manifest entry through `player::Player` with the
//! `Headless` backend, compares every stream with FFmpeg, and writes
//! `target/e2e/codecs.json`.
//!
//! ```text
//! cargo run -p e2e --release -- [--filter X] [--fuzz]
//! ```

mod compare;
mod coverage;
mod http;
mod manifest;
mod oracle;
mod tap;
mod tool;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use compare::{Compare, Verdict};
use manifest::{Entry, Kind, Policy};
use oxideav_core::{Demuxer, MediaType, RuntimeContext, StreamInfo};
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
    /// That stream's container tag as FFmpeg reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frames: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    samples: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cues: Option<usize>,
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
    /// The HTTP pass for this stream: PASS when playing the sample over HTTP
    /// captured exactly what the file playback did.
    #[serde(skip_serializing_if = "Option::is_none")]
    http: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_error: Option<String>,
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
    /// How the entry stands on each row it claims.
    claims: Vec<ClaimResult>,
}

#[derive(Serialize)]
struct ClaimResult {
    row: Row,
    /// `VERIFIED`, `UNVERIFIED` (FFmpeg cannot decode the format) or `FAIL`.
    standing: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
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
    /// `VERIFIED` (an entry passed against FFmpeg), `UNVERIFIED` (only
    /// decodes-only entries, for formats FFmpeg cannot decode), `FAIL`
    /// (claimed, nothing passed), `UNCOVERED` (no entry claims it) or
    /// `NOT RUN` (claimed only by entries a --filter left out).
    status: &'static str,
    verified_entries: Vec<String>,
    unverified_entries: Vec<String>,
    /// `<entry>: <reason>`.
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
    /// No mutation failed; set by [`FuzzReport::finish`].
    passed: bool,
    failures: Vec<String>,
}

impl FuzzReport {
    /// The verdict, from the failures collected.
    fn finish(&mut self) {
        self.passed = self.mutations > 0 && self.failures.is_empty();
    }
}

/// What an entry's results establish for row attribution: the demuxer and
/// codecs that actually ran, their verdicts, and the failures that withdraw
/// rows (any entry-level failure withdraws container rows; a tracks,
/// selection or outright HTTP failure withdraws codec rows too, since the
/// comparisons can no longer be trusted to describe the stream).
fn entry_facts(result: &EntryResult, path: &Path) -> coverage::EntryFacts {
    let failure = |kinds: &[&str]| {
        result
            .streams
            .iter()
            .find(|s| kinds.contains(&s.kind.as_str()) && s.verdict == "FAIL")
            .map(|s| format!("{}: {}", s.kind, s.error.as_deref().unwrap_or("failed")))
    };
    coverage::EntryFacts {
        demuxer: result.demuxer.clone(),
        ogm: result.demuxer.as_deref() == Some("ogg") && coverage::is_ogm(path),
        entry_failure: failure(&["open", "engine", "tracks", "selection", "http"]),
        stream_failure: failure(&["tracks", "selection", "http"]),
        streams: result
            .streams
            .iter()
            .filter_map(|s| {
                let kind = match s.kind.as_str() {
                    "video" => Kind::Video,
                    "audio" => Kind::Audio,
                    "subtitle" => Kind::Subtitle,
                    _ => return None,
                };
                Some(coverage::StreamFacts {
                    kind,
                    codec: s.codec.clone(),
                    tag: s.tag.clone(),
                    verdict: s.verdict.clone(),
                    http_ok: s.http != Some("FAIL"),
                })
            })
            .collect(),
    }
}

/// The process exit code: 1 when a judged row has no passing entry (neither
/// verified nor, for formats FFmpeg cannot decode, decodes-only) or the fuzz
/// pass ran and failed.
fn exit_code(rows_without_pass: usize, fuzz: Option<&FuzzReport>) -> i32 {
    i32::from(rows_without_pass > 0 || fuzz.is_some_and(|f| !f.passed))
}

// ---------------------------------------------------------------- playback

/// Plays `path` (or URL) to the end and returns the headless capture plus the
/// final state. Non-realtime, so it runs as fast as the decoders do. A hung
/// pipeline (a demuxer or decoder waiting forever) aborts after `secs` and is
/// reported as a timeout; the leaked threads die with the process.
/// One playback's outcome: the headless capture, the final state, and the
/// cues the subtitle decoder handed the pipeline.
struct Played {
    capture: player::Capture,
    state: player::State,
    cues: Vec<tap::Cue>,
    /// Codec ids of the subtitle decoders the player built.
    subtitle_decoders: Vec<String>,
}

/// The player's registry behind decoder taps ([`tap::context`]), shared by
/// every playback.
static TAPPED: LazyLock<Arc<RuntimeContext>> = LazyLock::new(|| Arc::new(tap::context()));

/// Plays `url` to the end. A hung pipeline (a demuxer or decoder waiting
/// forever) aborts after `secs` and is reported as a timeout; the leaked
/// threads die with the process.
fn play(url: &str, options: PlayerOptions, secs: u64) -> Result<Played, String> {
    let backend = Headless::new();
    let ctx = Arc::clone(&TAPPED);
    let recorder = Arc::new(tap::Recorder::default());
    tap::record_into(Arc::clone(&recorder));
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
        Ok((capture, state)) => {
            Ok(Played { capture, state, cues: recorder.cues(), subtitle_decoders: recorder.decoders() })
        }
        Err(_) => Err(format!("playback hung past the {secs}s watchdog")),
    }
}

/// One track the player offers.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct TrackInfo {
    stream: u32,
    kind: Kind,
    codec: String,
}

/// What the player is offered, found the way the player finds it: its probe
/// rule and registry (a VobSub index opens with its program stream, as the
/// engine opens it), the engine's track filter (audio, video within the
/// size caps, subtitles; at most 64 streams), and the closed-caption tracks
/// of the video it plays (`wanted_video`, else the first).
struct Discovery {
    demuxer: String,
    tracks: Vec<TrackInfo>,
}

fn discover(path: &Path, wanted_video: Option<u32>) -> Result<Discovery, String> {
    let ctx = &*tap::PLAIN;
    let vobsub = player::source::vobsub_stream_url(&path.to_string_lossy());
    type Opened = Result<(String, Box<dyn Demuxer>), String>;
    let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Opened {
        let file = |p: &Path| std::fs::File::open(p).map_err(|e| format!("open {}: {e}", p.display()));
        let failed = |e: oxideav_core::Error| format!("failed to open demuxer: {e}");
        match &vobsub {
            Some(sub) => {
                let d = subs_bitmap::open_vobsub(Box::new(file(path)?), Box::new(file(Path::new(sub))?)).map_err(failed)?;
                Ok((d.format_name().to_string(), d))
            }
            None => {
                let name = refcheck::probe_container(ctx, path)?;
                let d = ctx.containers.open_demuxer(&name, Box::new(file(path)?), &ctx.codecs).map_err(failed)?;
                Ok((name, d))
            }
        }
    }));
    let (demuxer, mut d) = match opened {
        Ok(Ok(opened)) => opened,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("the demuxer panicked while opening".into()),
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
    let video = wanted_video.or_else(|| tracks.iter().find(|t| t.kind == Kind::Video).map(|t| t.stream));
    if let Some(info) = video.and_then(|v| d.streams().iter().find(|s| s.index == v)).cloned() {
        let scan = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| caption_tracks(&mut *d, &info)));
        tracks.extend(scan.unwrap_or_default());
    }
    Ok(Discovery { demuxer, tracks })
}

/// The closed-caption tracks the player lists for `video`, the video stream
/// it plays: one per service that shows up in its pictures' caption data,
/// taken in presentation order as the engine takes it, listed in the order
/// the engine lists them. Reads `demuxer` to its end.
fn caption_tracks(demuxer: &mut dyn Demuxer, video: &StreamInfo) -> Vec<TrackInfo> {
    fn list(released: Vec<subs_cc::timeline::Timed>, tracks: &mut Vec<TrackInfo>) {
        for (_, triplets) in released {
            let services = subs_cc::Services::of(&triplets);
            for (present, stream, codec) in [
                (services.eia608, player::CAPTIONS_608, subs_cc::eia608::CODEC_ID),
                (services.cea708, player::CAPTIONS_708, subs_cc::cea708::CODEC_ID),
            ] {
                if present && !tracks.iter().any(|t| t.stream == stream) {
                    tracks.push(TrackInfo { stream, kind: Kind::Subtitle, codec: codec.to_string() });
                }
            }
        }
    }
    let Some(mut extractor) = subs_cc::CcExtractor::new(video.params.codec_id.as_str(), &video.params.extradata) else {
        return Vec::new();
    };
    let mut timeline = subs_cc::CaptionTimeline::new();
    let mut tracks = Vec::new();
    while let Ok(packet) = demuxer.next_packet() {
        if packet.stream_index == video.index {
            let triplets = extractor.extract(&packet.data);
            list(timeline.push(packet.pts, packet.dts, triplets), &mut tracks);
        }
    }
    list(timeline.finish(), &mut tracks);
    tracks
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
/// Closed captions are none of the file's streams: FFmpeg reads EIA-608
/// from the video (its `subcc` output) and decodes no CEA-708.
fn map_to_ffmpeg(track: &TrackInfo, disc: &Discovery, ff: &[oracle::FfStream]) -> Result<oracle::FfStream, String> {
    let caption = |stream: u32| stream == player::CAPTIONS_608 || stream == player::CAPTIONS_708;
    match track.stream {
        player::CAPTIONS_608 => return Ok(oracle::FfStream::subcc()),
        player::CAPTIONS_708 => return Err("FFmpeg decodes no CEA-708: the caption track has no FFmpeg counterpart".into()),
        _ => {}
    }
    let ours: Vec<u32> = disc.tracks.iter().filter(|t| t.kind == track.kind && !caption(t.stream)).map(|t| t.stream).collect();
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

/// An empty capture matches FFmpeg only when FFmpeg's demuxer reads no
/// packet of the stream either: it then decodes nothing and its `-map`
/// output is empty (FATE's dvbsubtest_filter.ts declares MPEG-2 video and
/// MPEG audio that never carry a packet).
fn empty_capture(path: &Path, ff: &oracle::FfStream, output: String, what: &str) -> Compare {
    match oracle::packet_count(path, ff.index) {
        Ok(0) => Compare::pass(format!("{output}, FFmpeg demuxes no packets")),
        Ok(n) => Compare::fail(output, format!("no {what} captured; FFmpeg demuxes {n} packets")),
        Err(e) => Compare::fail(output, format!("no {what} captured; {e}")),
    }
}

fn compare_video(path: &Path, cap: &player::VideoCapture, ff: &oracle::FfStream) -> Compare {
    let output = format!("frames={}", cap.frame_md5.len());
    if cap.frame_md5.is_empty() {
        return empty_capture(path, ff, output, "frames");
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
            match oracle::audio_pcm(path, ff, pcm)
                .and_then(|reference| compare::exact_pcm(&cap.pcm, &reference, pcm, cap.channels as usize))
            {
                Ok(m) => Compare::pass(m),
                Err(e) => Compare::fail(metric, e),
            }
        }
        Policy::AudioSnr(floor) => {
            let slack = oracle::audio_frames(path, ff).and_then(|frames| compare::lossy_slack(&frames, cap.channels));
            let reference = oracle::audio_f32(path, ff);
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
        let slack = compare::lossy_slack(&oracle::audio_frames(path, ff)?, cap.channels)?;
        refcheck::try_snr_db(&oracle::audio_f32(path, ff)?, &cap.pcm, slack)
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

/// The decoded cues of one subtitle stream against FFmpeg's decode of `ff`,
/// under `policy`; `shown` is how many images the pipeline put on screen.
fn compare_subtitles(
    path: &Path,
    played: &Played,
    codec: &str,
    shown: usize,
    ff: &oracle::FfStream,
    policy: Policy,
) -> Compare {
    let cues = &played.cues;
    let output = format!("cues={}", cues.len());
    if played.subtitle_decoders.is_empty() {
        return Compare::fail(output, format!("the player built no subtitle decoder for codec {codec}"));
    }
    if cues.is_empty() {
        return Compare::fail(output, format!("the {} decoder emitted no cue", played.subtitle_decoders.join("+")));
    }
    let verdict = match policy {
        Policy::SubText => {
            oracle::subtitle_srt(path, ff).and_then(|reference| compare::text_cues(cues, shown, &reference))
        }
        Policy::SubBitmap => oracle::subtitle_events(path, ff.index).and_then(|events| {
            let (dims, canvases) = oracle::subtitle_canvases(path, ff.index)?;
            Ok((compare::bitmap_cues(cues, shown, &events, dims, &canvases)?, Vec::new()))
        }),
        other => return misapplied(other, Kind::Subtitle),
    };
    match verdict {
        Ok((metric, diagnostics)) => Compare { diagnostics, ..Compare::pass(metric) },
        Err(e) => Compare::fail(output, e),
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
        (Policy::Decodes(_), Ok(ff)) if oracle::decodes(path, ff) => Compare::fail(
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
            tag: None,
            frames: None,
            samples: None,
            cues: None,
            policy: None,
            verdict: Verdict::Fail.as_str().into(),
            metric,
            error: Some(error.into()),
            diagnostics: Vec::new(),
            http: None,
            http_error: None,
        }
    }

    fn judged(index: u32, kind: Kind, codec: &str, policy: Option<Policy>, cmp: Compare) -> Self {
        StreamResult {
            index,
            kind: kind.name().into(),
            codec: codec.into(),
            decoder: "software".into(),
            ffmpeg: None,
            tag: None,
            frames: None,
            samples: None,
            cues: None,
            policy: policy.map(Policy::token),
            verdict: cmp.verdict.as_str().into(),
            metric: Some(cmp.metric),
            error: cmp.error,
            diagnostics: cmp.diagnostics,
            http: None,
            http_error: None,
        }
    }
}

/// Judges one selected track against its capture.
fn judge_track(
    entry: &Entry,
    path: &Path,
    sel: &Selected,
    ff: &Result<oracle::FfStream, String>,
    played: &Played,
) -> StreamResult {
    let (capture, state, cues) = (&played.capture, &played.state, &played.cues[..]);
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
                    Err(_) if cap.is_some_and(|c| c.pcm.is_empty()) => empty_capture(path, ff, output.clone(), "PCM"),
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
                r.diagnostics.extend(audio_diagnostics(path, cap, ff, &entry.diagnostics));
            }
            r
        }
        Kind::Subtitle => {
            // The capture of a subtitle stream exists from its first show.
            let cap = capture.subtitles.iter().find(|c| c.stream == t.stream);
            let shown = cap.map_or(0, |c| c.shows.iter().filter(|(_, n)| *n > 0).count());
            let output = format!("cues={}", cues.len());
            let (policy, cmp) = judge(
                entry,
                path,
                Kind::Subtitle,
                ff,
                &output,
                |policy, ff| match policy {
                    Policy::SubText | Policy::SubBitmap => compare_subtitles(path, played, &t.codec, shown, ff, policy),
                    other => misapplied(other, Kind::Subtitle),
                },
                || {
                    match compare::shown_all(cues, shown) {
                        Ok(()) if shown > 0 => Compare::decodes(output.clone()),
                        Ok(()) => Compare::fail(output.clone(), missing()),
                        Err(e) => Compare::fail(output.clone(), e),
                    }
                },
            );
            let mut r = StreamResult::judged(t.stream, Kind::Subtitle, &t.codec, policy, cmp);
            r.cues = Some(cues.len());
            r
        }
    };
    r.ffmpeg = ff.as_ref().ok().map(oracle::FfStream::map);
    r.tag = ff.as_ref().ok().map(|ff| ff.codec_tag.clone());
    r
}

/// Whether two playbacks captured the same output for `track`: equal frame
/// digests, bit-identical PCM in the same layout, equal decoded cues shown
/// as often.
fn same_capture(file: &Played, http: &Played, track: &TrackInfo) -> Result<(), String> {
    let s = track.stream;
    match track.kind {
        Kind::Video => {
            let (f, h) = (file.capture.video.iter().find(|c| c.stream == s), http.capture.video.iter().find(|c| c.stream == s));
            let (f, h) = (f.map(|c| &c.frame_md5[..]).unwrap_or(&[]), h.map(|c| &c.frame_md5[..]).unwrap_or(&[]));
            match f.iter().zip(h).position(|(a, b)| a != b) {
                Some(i) => Err(format!("frame {i} differs from the file playback's")),
                None if f.len() != h.len() => Err(format!("{} frames vs file {}", h.len(), f.len())),
                None => Ok(()),
            }
        }
        Kind::Audio => {
            let (f, h) = (file.capture.audio.iter().find(|c| c.stream == s), http.capture.audio.iter().find(|c| c.stream == s));
            let layout = |c: Option<&player::AudioCapture>| c.map(|c| (c.channels, c.sample_rate));
            if layout(f) != layout(h) {
                return Err(format!("layout {:?} vs file {:?}", layout(h), layout(f)));
            }
            let (f, h) = (f.map(|c| &c.pcm[..]).unwrap_or(&[]), h.map(|c| &c.pcm[..]).unwrap_or(&[]));
            match f.iter().zip(h).position(|(a, b)| a.to_bits() != b.to_bits()) {
                Some(i) => Err(format!("PCM sample {i} differs from the file playback's")),
                None if f.len() != h.len() => Err(format!("{} PCM samples vs file {}", h.len(), f.len())),
                None => Ok(()),
            }
        }
        Kind::Subtitle => {
            let shown = |p: &Played| {
                p.capture.subtitles.iter().find(|c| c.stream == s).map_or(0, |c| c.shows.iter().filter(|(_, n)| *n > 0).count())
            };
            if http.cues != file.cues {
                return Err(format!("{} decoded cues vs file {}, or their content differs", http.cues.len(), file.cues.len()));
            }
            if shown(http) != shown(file) {
                return Err(format!("{} cues shown vs file {}", shown(http), shown(file)));
            }
            Ok(())
        }
    }
}

/// Plays one entry and returns its per-stream results. The tracks to check
/// are discovered as the player discovers them, selected per the manifest
/// (else the player's defaults), and each is compared with FFmpeg's stream at
/// the same position among the streams of its kind. With `http_base`, the
/// entry is played a second time over HTTP from a Range-capable local server
/// and the same digests are compared.
fn run_entry(entry: &Entry, path: &Path, http_base: Option<&str>) -> EntryResult {
    let mut result =
        EntryResult { path: entry.path.clone(), demuxer: None, tracks: Vec::new(), streams: Vec::new(), claims: Vec::new() };

    let disc = match discover(path, entry.selection.video) {
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

    let played = match play(&path.to_string_lossy(), options.clone(), 300) {
        Ok(p) => p,
        Err(e) => {
            result.streams.push(StreamResult::entry_level("open", 0, None, e));
            return result;
        }
    };
    let (capture, state) = (&played.capture, &played.state);
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
        // A stream the player dropped (no decoder) fails on its own as never
        // captured; playing another stream than selected discredits the
        // mapping of every comparison.
        if current.is_some_and(|c| c != sel.track.stream) {
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
        result.streams.push(judge_track(entry, path, sel, &ff, &played));
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

    // HTTP pass: the server must serve the very bytes the file run read,
    // and playing them over HTTP must capture exactly what the file run did,
    // stream by stream. Any disagreement fails the stream (or, for the bytes,
    // the open and the end of playback, the whole entry). Unreachable after a
    // fatal open/engine failure: those return early above, so a hung or
    // errored decode never pays the second watchdog delay.
    if let Some(base) = http_base {
        let url = format!("{base}/{}", http::url_path(&entry.path));
        let outcome = http::verify_bytes(&url, path).and_then(|()| play(&url, options, 300));
        match outcome {
            Err(e) => result.streams.push(StreamResult::entry_level("http", u32::MAX - 1, None, e)),
            Ok(h) if !h.state.ended => result.streams.push(StreamResult::entry_level(
                "http",
                u32::MAX - 1,
                None,
                format!("http playback did not reach Ended: {:?}", h.state.error),
            )),
            Ok(h) => {
                if h.state.error != state.error {
                    let error = format!("http playback error {:?} vs file {:?}", h.state.error, state.error);
                    result.streams.push(StreamResult::entry_level("http", u32::MAX - 1, None, error));
                }
                for sel in &selected {
                    let verdict = same_capture(&played, &h, &sel.track);
                    if let Some(r) =
                        result.streams.iter_mut().find(|r| r.index == sel.track.stream && r.kind == sel.track.kind.name())
                    {
                        r.http = Some(if verdict.is_ok() { "PASS" } else { "FAIL" });
                        r.http_error = verdict.err();
                    }
                }
            }
        }
    }

    result
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
    let missing_rules = coverage::missing_rules(&yardstick.rows);
    if !missing_rules.is_empty() {
        eprintln!("yardstick rows without an attribution rule: {}", missing_rules.join(", "));
        std::process::exit(2);
    }
    let all_entries = match manifest::parse(&manifest_text, &yardstick.rows) {
        Ok(entries) => entries,
        Err(errors) => {
            for e in &errors {
                eprintln!("corpus/manifest.toml: {e}");
            }
            std::process::exit(2);
        }
    };
    let manifest_rows: Vec<String> = all_entries.iter().flat_map(|e| e.rows.iter().cloned()).collect();
    let entries: Vec<Entry> = all_entries
        .into_iter()
        .filter(|e| filter.as_ref().map(|f| e.path.contains(f)).unwrap_or(true))
        .collect();

    let http_base = http.then(|| http::start(resolve));

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
                claims: entry
                    .rows
                    .iter()
                    .map(|row| ClaimResult { row: row.clone(), standing: "FAIL", reason: Some("sample missing".into()) })
                    .collect(),
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

        let mut result = run_entry(entry, &path, http_base.as_deref());
        let facts = entry_facts(&result, &path);
        result.claims = entry
            .rows
            .iter()
            .map(|row| {
                let (standing, reason) = match coverage::claim(row, &facts) {
                    coverage::Claim::Verified => ("VERIFIED", None),
                    coverage::Claim::Unverified => ("UNVERIFIED", None),
                    coverage::Claim::Failed(why) => ("FAIL", Some(why)),
                };
                ClaimResult { row: row.clone(), standing, reason }
            })
            .collect();
        let verdict = |kind: &str| {
            result.streams.iter().find(|s| s.kind == kind).map_or("-", |s| match (s.verdict.as_str(), s.http) {
                (_, Some("FAIL")) => "HTTP",
                ("PASS", _) => "ok",
                ("DECODES", _) => "dec",
                _ => "FAIL",
            })
        };
        let checked = result.tracks.iter().filter(|t| t.checked).count();
        let entry_fail = result.streams.iter().find(|s| !matches!(s.kind.as_str(), "video" | "audio" | "subtitle"));
        println!(
            "{:<44} {:>6} {:>6} {:>6} {:>4}/{:<3} {}",
            entry.path,
            verdict("video"),
            verdict("audio"),
            verdict("subtitle"),
            checked,
            result.tracks.len(),
            entry_fail.map(|s| s.kind.as_str()).unwrap_or("")
        );
        entry_results.push(result);
    }

    // Rows: verified by an FFmpeg-checked pass, unverified by decodes-only
    // passes (formats FFmpeg cannot decode), else failing or uncovered.
    for result in &entry_results {
        for c in &result.claims {
            let Some(rr) = row_map.get_mut(&c.row) else { continue };
            match c.standing {
                "VERIFIED" => rr.verified_entries.push(result.path.clone()),
                "UNVERIFIED" => rr.unverified_entries.push(result.path.clone()),
                _ => rr.failing_entries.push(format!("{}: {}", result.path, c.reason.as_deref().unwrap_or(""))),
            }
        }
    }
    let claimed_anywhere = |row: &str| manifest_rows.iter().any(|r| r == row);
    for rr in row_map.values_mut() {
        rr.status = if !rr.verified_entries.is_empty() {
            "VERIFIED"
        } else if !rr.unverified_entries.is_empty() {
            "UNVERIFIED"
        } else if !rr.failing_entries.is_empty() {
            "FAIL"
        } else if claimed_anywhere(&rr.row) {
            "NOT RUN"
        } else {
            "UNCOVERED"
        };
    }

    println!("{}", "-".repeat(74));
    println!("{:<22} {:<10} {:>4} {:>4} {:>4}  {}", "row", "status", "ver", "unv", "fail", "first failing entry");
    println!("{}", "-".repeat(74));
    let rows: Vec<RowResult> = row_map.values().cloned().collect();
    for rr in &rows {
        println!(
            "{:<22} {:<10} {:>4} {:>4} {:>4}  {}",
            rr.row,
            rr.status,
            rr.verified_entries.len(),
            rr.unverified_entries.len(),
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
        fr.finish();
        let verdict = if fr.passed { "ok" } else { "FAILED" };
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
        .filter(|r| match r.status {
            "FAIL" => true,
            // Without --filter every row must be claimed and run.
            "UNCOVERED" | "NOT RUN" => filter.is_none(),
            _ => false,
        })
        .map(|r| r.row.clone())
        .collect();
    if !empty_rows.is_empty() {
        eprintln!(
            "{} yardstick rows have no passing entry: {}",
            empty_rows.len(),
            empty_rows.join(", ")
        );
    }
    if let Some(fr) = report.fuzz.as_ref().filter(|f| !f.passed) {
        eprintln!("fuzz: {} of {} mutations failed", fr.failures.len(), fr.mutations);
    }
    std::process::exit(exit_code(empty_rows.len(), report.fuzz.as_ref()));
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
            subcc: false,
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
    fn fuzz_failures_fail_the_run_and_set_the_verdict() {
        let mut fr = FuzzReport { mutations: 20, ..Default::default() };
        fr.finish();
        assert!(fr.passed);
        assert_eq!(exit_code(0, Some(&fr)), 0);
        fr.failures.push("x: mutation 3 neither Ended nor Error".into());
        fr.finish();
        assert!(!fr.passed);
        assert_eq!(exit_code(0, Some(&fr)), 1, "rows all pass, the fuzz pass does not");
        assert_eq!(exit_code(0, None), 0);
        assert_eq!(exit_code(2, None), 1);
        let json = serde_json::to_value(&fr).unwrap();
        assert_eq!(json["passed"], false, "the verdict is always serialized");
    }

    #[test]
    fn an_explicit_selection_must_name_a_track_of_its_kind() {
        let (disc, _) = two_audio_tracks();
        let wrong = manifest::Selection { audio: Some(0), ..Default::default() };
        assert!(select(&entry(wrong), &disc).is_err(), "stream 0 is video");
    }

    #[test]
    fn an_empty_capture_passes_only_when_ffmpeg_demuxes_no_packets() {
        let empty = player::VideoCapture {
            stream: 0,
            codec: String::new(),
            pixel_format: oxideav_core::PixelFormat::Yuv420P,
            width: 0,
            height: 0,
            frame_md5: Vec::new(),
            pts: Vec::new(),
            shown_at: Vec::new(),
            flushes: Vec::new(),
        };
        // The MPEG-2 video dvbsubtest_filter.ts declares never carries a
        // packet; h264small.ts's video carries 74.
        let none = refcheck::fate("sub/dvbsubtest_filter.ts");
        let cmp = compare_video(&none, &empty, &ff(0, "video", "mpeg2video", false));
        assert!(matches!(cmp.verdict, Verdict::Pass), "{:?}", cmp.error);
        let some = refcheck::fate("mpegts/h264small.ts");
        let cmp = compare_video(&some, &empty, &ff(0, "video", "h264", false));
        assert!(matches!(cmp.verdict, Verdict::Fail));
    }

}
