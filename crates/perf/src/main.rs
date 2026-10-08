//! Decoder speed audit.
//!
//! Decodes one input per codec row of `corpus/manifest.toml` (the longest
//! sample whose stream carries that codec) and the standard inputs that
//! `corpus/perf-inputs.sh` makes, through the player's registry
//! (`codecs::context()`; the decoder `first_decoder` picks, built the way the
//! engine builds it). The file is read into memory first; a run times demux +
//! decode of that one stream, no sink, single-threaded
//! (`ExecutionContext::serial`). ×real-time = media seconds decoded ÷ wall
//! seconds; the table reports the median of `--runs` runs. A run that passes
//! `--max-secs` stops there and counts what it decoded (default 0: no cap).
//!
//! Each input is decoded once more, untimed, and compared with FFmpeg's
//! complete decode of the same stream (video: the MD5 of every frame against
//! `framemd5` with FFmpeg's C IDCT; audio: full PCM bytes for lossless codecs
//! and SNR for float codecs). Failed comparisons invalidate speed verdicts.
//! FFmpeg's own single-threaded decode of the stream is timed for reference.
//!
//! ```text
//! cargo run --release -p perf -- --out target/perf/perf.json
//!     [--runs 3] [--max-secs 0] [--filter <substring>] [--no-check] [--no-ffmpeg]
//! cargo run --release -p perf -- --list                   # every manifest stream
//! cargo run --release -p perf -- --filter <s> --loop 30   # decode in a loop, for a profiler
//! ```

mod journal;
mod pcm;

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxideav_core::{
    CodecId, CodecParameters, Decoder, Error, ExecutionContext, Frame, MediaType, PROBE_SCORE_EXTENSION, Packet,
    PixelFormat, ProbeData, RuntimeContext, TimeBase,
};
use serde::{Deserialize, Serialize};

/// Slowest acceptable decode on this Mac, in ×real-time. A phone is about
/// 3-6× slower, and video falls back to software whenever MediaCodec or
/// VideoToolbox cannot take the stream.
const AUDIO_FLOOR: f64 = 10.0;
/// Video above 576 lines (720p and 1080p).
const HD_FLOOR: f64 = 2.0;
/// Video up to 576 lines.
const SD_FLOOR: f64 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Video,
    Audio,
}

impl Kind {
    fn of(media_type: MediaType) -> Option<Kind> {
        match media_type {
            MediaType::Video => Some(Kind::Video),
            MediaType::Audio => Some(Kind::Audio),
            _ => None,
        }
    }

    /// FFmpeg's stream specifier letter.
    fn letter(self) -> &'static str {
        match self {
            Kind::Video => "v",
            Kind::Audio => "a",
        }
    }
}

/// A standard input: its table label, its file in
/// `$PEARTUBE_CORPUS_DIR/perf` (made by `corpus/perf-inputs.sh`), and the
/// kind of stream measured.
struct Standard {
    label: &'static str,
    file: &'static str,
    kind: Kind,
}

const STANDARD: &[Standard] = &[
    Standard { label: "std:h264 High 1080p30", file: "h264_1080p30_high.mp4", kind: Kind::Video },
    Standard { label: "std:hevc Main 1080p30", file: "hevc_1080p30_main.mkv", kind: Kind::Video },
    Standard { label: "std:hevc Main10 1080p30", file: "hevc_1080p30_main10.mkv", kind: Kind::Video },
    Standard { label: "std:vp9 1080p30", file: "vp9_1080p30.webm", kind: Kind::Video },
    Standard { label: "std:av1 1080p30", file: "av1_1080p30.mkv", kind: Kind::Video },
    Standard { label: "std:mpeg2 720p30", file: "mpeg2_720p30.ts", kind: Kind::Video },
    Standard { label: "std:mpeg2 576i25", file: "mpeg2_576i25.m2v", kind: Kind::Video },
    Standard { label: "std:dv 576i25", file: "dv_576i25.avi", kind: Kind::Video },
    Standard { label: "std:mpeg4 ASP (Xvid) 480p30", file: "mpeg4_asp_480p30.avi", kind: Kind::Video },
    Standard { label: "std:aac LC stereo", file: "aac_lc_stereo.m4a", kind: Kind::Audio },
    Standard { label: "std:aac LC 5.1", file: "aac_lc_51.m4a", kind: Kind::Audio },
    Standard { label: "std:aac HE v1 stereo", file: "he_aac_stereo.m4a", kind: Kind::Audio },
    Standard { label: "std:aac HE v2 stereo", file: "he_aac_v2_stereo.m4a", kind: Kind::Audio },
    Standard { label: "std:ac3 5.1", file: "ac3_51.ac3", kind: Kind::Audio },
    Standard { label: "std:eac3 7.1", file: "eac3_71.eac3", kind: Kind::Audio },
    Standard { label: "std:dts 5.1", file: "dts_51.dts", kind: Kind::Audio },
    Standard { label: "std:truehd 7.1", file: "truehd_71.thd", kind: Kind::Audio },
    Standard { label: "std:flac 24/96", file: "flac_24_96.flac", kind: Kind::Audio },
    Standard { label: "std:opus stereo", file: "opus_stereo.opus", kind: Kind::Audio },
    Standard { label: "std:vorbis stereo", file: "vorbis_stereo.ogg", kind: Kind::Audio },
    Standard { label: "std:mp3 stereo", file: "mp3_stereo.mp3", kind: Kind::Audio },
    Standard { label: "std:wmapro 5.1", file: "wmapro_51.wma", kind: Kind::Audio },
    Standard { label: "extra:mpeg1 raw", file: "mpeg1_480p30.m1v", kind: Kind::Video },
    Standard { label: "extra:wmv1", file: "wmv1_480p30.avi", kind: Kind::Video },
    Standard { label: "extra:wma1", file: "wma1_stereo.wma", kind: Kind::Audio },
    Standard { label: "extra:3ivx tag", file: "mpeg4_3ivx.avi", kind: Kind::Video },
    Standard { label: "extra:svq1 rewrap", file: "svq1_rewrapped.mov", kind: Kind::Video },
    Standard { label: "extra:svq3 rewrap", file: "svq3_rewrapped.mov", kind: Kind::Video },
    Standard { label: "extra:dirac rewrap", file: "dirac_rewrapped.mkv", kind: Kind::Video },
    Standard { label: "extra:wmv3 RCV", file: "fate:vc1/SMM0015.rcv", kind: Kind::Video },
    Standard { label: "extra:rv10", file: "fate:sipr/sipr_5k0.rm", kind: Kind::Video },
    Standard { label: "extra:indeo5", file: "fate:iv50/Educ_Movie_DeadlyForce.avi", kind: Kind::Video },
    Standard { label: "extra:wavpack MKV", file: "fate:wavpack/special/matroska_mode.mka", kind: Kind::Audio },
    Standard { label: "extra:HE-AAC v1 5.1", file: "fate:aac/al_sbr_cm_48_5.1.mp4", kind: Kind::Audio },
    // Decoders added since the first audit (`corpus/perf-inputs.sh new`).
    // The MXF PCM stream times the demuxer; raw .mp2 shows its routing.
    Standard { label: "new:h263 CIF", file: "h263_cif.avi", kind: Kind::Video },
    Standard { label: "new:h263 4CIF", file: "h263_4cif.avi", kind: Kind::Video },
    Standard { label: "new:flv1 480p30", file: "flv1_480p30.flv", kind: Kind::Video },
    Standard { label: "new:dv DVCPRO HD 1080i50", file: "dvcprohd_1080i50.mov", kind: Kind::Video },
    Standard { label: "new:mp2 stereo raw", file: "mp2_stereo.mp2", kind: Kind::Audio },
    Standard { label: "new:mp2 stereo MKA", file: "mp2_stereo.mka", kind: Kind::Audio },
    Standard { label: "new:alac stereo", file: "alac_stereo.m4a", kind: Kind::Audio },
    Standard { label: "new:atrac3 132k OMA", file: "atrac3_132k.oma", kind: Kind::Audio },
    Standard { label: "new:dvaudio Ulead WAV", file: "dvaudio_ulead.wav", kind: Kind::Audio },
    Standard { label: "new:mpeg2 in MXF 576p25", file: "mpeg2_pcm.mxf", kind: Kind::Video },
    Standard { label: "new:pcm in MXF", file: "mpeg2_pcm.mxf", kind: Kind::Audio },
    // Larger FATE streams for decoders whose manifest sample is tiny.
    Standard { label: "extra:vp6f FLV", file: "fate:flash-vp6/clip1024.flv", kind: Kind::Video },
    Standard { label: "extra:vp6a FLV", file: "fate:flash-vp6/300x180-Scr-f8-056alpha.flv", kind: Kind::Video },
    Standard { label: "extra:mpeg2 XDCAM MXF", file: "fate:mxf/omneon_8.3.0.0_xdcam_startc_footer.mxf", kind: Kind::Video },
    Standard { label: "extra:atrac1", file: "fate:atrac1/chirp_tone_10-16000.aea", kind: Kind::Audio },
];

/// Codec ids (as the demuxers report them) that carry a manifest row's codec.
/// A row not listed here is its own codec id (`audio:aac` → `aac`).
const ROW_CODECS: &[(&str, &[&str])] = &[
    ("video:mpeg1", &["mpeg1video"]),
    ("video:mpeg2", &["mpeg2video"]),
    ("video:mpeg4", &["mpeg4video", "mpeg4"]),
    ("video:xvid", &["mpeg4video", "mpeg4"]),
    ("video:3ivx", &["mpeg4video", "mpeg4"]),
    ("video:divx", &["msmpeg4v3", "div3"]),
    ("video:hevc", &["hevc", "h265"]),
    ("video:iv32", &["indeo3"]),
    ("video:dv", &["dvvideo"]),
    ("video:vp6", &["vp6", "vp6f", "vp6a"]),
    ("audio:wma1", &["wma1", "wmav1"]),
    ("audio:wma2", &["wma2", "wmav2"]),
    ("audio:ra144", &["ra_144"]),
    ("audio:ra288", &["ra_288"]),
    ("audio:alaw", &["pcm_alaw", "alaw"]),
    ("audio:ulaw", &["pcm_mulaw", "ulaw"]),
    ("audio:lpcm", &["pcm_u8", "pcm_s16le", "pcm_s24le", "pcm_s32le", "pcm_f32le", "pcm_f64le"]),
    ("audio:adpcm", &["adpcm_ms", "adpcm_ima_wav", "adpcm_ima_qt"]),
    ("audio:amrnb", &["amr_nb", "amrnb"]),
    ("audio:amrwb", &["amr_wb", "amrwb"]),
    ("audio:atrac3p", &["atrac3p", "atrac3plus"]),
];

fn row_codecs(row: &str) -> Vec<&str> {
    match ROW_CODECS.iter().find(|(r, _)| *r == row) {
        Some((_, ids)) => ids.to_vec(),
        None => vec![row.split_once(':').map_or(row, |(_, codec)| codec)],
    }
}

// ---------------------------------------------------------------- inputs

#[derive(serde::Deserialize)]
struct Manifest {
    #[serde(default)]
    entry: Vec<ManifestEntry>,
}

#[derive(serde::Deserialize)]
struct ManifestEntry {
    path: String,
    rows: Vec<String>,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn corpus_dir() -> PathBuf {
    std::env::var_os("PEARTUBE_CORPUS_DIR").map(PathBuf::from).unwrap_or_else(|| home().join("projects/peartube-media-corpus"))
}

/// Where a manifest `path` lives on disk (the e2e runner's rule).
fn resolve(path: &str) -> PathBuf {
    match path.split_once(':') {
        Some(("fate", rel)) => std::env::var_os("FATE_SUITE")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("projects/fate-suite"))
            .join(rel),
        Some(("gen", rel)) => corpus_dir().join(rel),
        _ => PathBuf::from(path),
    }
}

/// One stream of a probed file.
#[derive(Clone, Debug, Serialize)]
struct StreamProbe {
    index: u32,
    kind: Option<Kind>,
    /// Position among the streams of its kind: FFmpeg's `0:v:<nth>`.
    nth: usize,
    codec: String,
    /// Implementation `first_decoder` picks, `None` when nothing decodes it.
    decoder: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    channels: Option<u16>,
    sample_rate: Option<u32>,
    /// Duration the container declares, in seconds.
    declared_secs: Option<f64>,
    /// FFprobe's metadata and packet span, independent of codec output.
    reference: Option<FfprobeStream>,
}

struct Probed {
    container: String,
    streams: Vec<StreamProbe>,
}

/// The container the engine would pick (`player::engine`'s `probe_container`).
fn container_for(ctx: &RuntimeContext, path: &Path, head: &[u8]) -> Result<String, String> {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let probe = ProbeData { buf: head, ext: ext.as_deref() };
    let candidates = ctx.containers.probe_candidates(&probe);
    let by_extension = ext.as_deref().and_then(|e| ctx.containers.container_for_extension(e));
    match (candidates.first(), by_extension) {
        (Some(c), _) if c.score >= PROBE_SCORE_EXTENSION => Ok(c.name.to_string()),
        (_, Some(name)) => Ok(name.to_string()),
        _ => Err("no container claims it".into()),
    }
}

fn decoder_name(ctx: &RuntimeContext, codec: &CodecId) -> Option<String> {
    ctx.codecs.implementations(codec).iter().find(|i| i.make_decoder.is_some()).map(|i| i.caps.implementation.clone())
}

fn probe(ctx: &RuntimeContext, path: &Path) -> Result<Probed, String> {
    let mut head = vec![0; 256 * 1024];
    let mut file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let mut n = 0;
    while n < head.len() {
        match file.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    head.truncate(n);
    let container = container_for(ctx, path, &head)?;
    let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let opened = catch_unwind(AssertUnwindSafe(|| ctx.containers.open_demuxer(&container, Box::new(file), &ctx.codecs)));
    let demuxer = match opened {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return Err(format!("{container} demuxer: {e}")),
        Err(p) => return Err(format!("{container} demuxer panicked: {}", panic_text(&p))),
    };
    let file_secs = demuxer.duration_micros().map(|us| us as f64 / 1e6);
    let mut counts: HashMap<Option<Kind>, usize> = HashMap::new();
    let streams = demuxer
        .streams()
        .iter()
        .map(|s| {
            let kind = Kind::of(s.params.media_type);
            let nth = counts.entry(kind).or_default();
            let reference = kind.and_then(|k| ffprobe_stream(path, k, *nth));
            let probe = StreamProbe {
                index: s.index,
                kind,
                nth: *nth,
                codec: s.params.codec_id.as_str().to_string(),
                decoder: decoder_name(ctx, &s.params.codec_id),
                width: s.params.width.filter(|w| *w > 0).or_else(|| reference.as_ref().and_then(|p| p.width)),
                height: s.params.height.filter(|h| *h > 0).or_else(|| reference.as_ref().and_then(|p| p.height)),
                channels: s.params.channels.filter(|c| *c > 0).or_else(|| reference.as_ref().and_then(|p| p.channels)),
                sample_rate: s.params.sample_rate.filter(|r| *r > 0).or_else(|| reference.as_ref().and_then(|p| p.sample_rate)),
                declared_secs: s
                    .duration
                    .filter(|&d| d > 0 && s.time_base.0.num > 0 && s.time_base.0.den > 0)
                    .map(|d| s.time_base.seconds_of(d))
                    .or(file_secs),
                reference,
            };
            *nth += 1;
            probe
        })
        .collect();
    Ok(Probed { container, streams })
}

/// One thing to measure: a stream of a file, and the rows it stands for.
struct Input {
    rows: Vec<String>,
    /// Manifest path (`fate:…`, `gen:…`) or `perf/<file>`.
    source: String,
    path: PathBuf,
    kind: Kind,
    container: Option<String>,
    stream: Option<StreamProbe>,
    note: Option<String>,
    /// Why it cannot be measured.
    problem: Option<String>,
}

fn probe_cached<'a>(
    ctx: &RuntimeContext,
    cache: &'a mut HashMap<PathBuf, Result<Probed, String>>,
    path: &Path,
) -> &'a Result<Probed, String> {
    cache.entry(path.to_path_buf()).or_insert_with(|| {
        if path.is_file() { probe(ctx, path) } else { Err(format!("missing file {}", path.display())) }
    })
}

/// One input per codec row. Search every manifest entry by actual stream
/// codec, not just its labels (some labels name a codec the file lacks).
/// Prefer the longest physically present packet span, not the duration in a
/// truncated FATE file's header. Never substitute another codec.
fn manifest_inputs(
    ctx: &RuntimeContext,
    manifest: &Manifest,
    cache: &mut HashMap<PathBuf, Result<Probed, String>>,
) -> Vec<Input> {
    let mut rows: Vec<&str> = Vec::new();
    for entry in &manifest.entry {
        for row in &entry.rows {
            if (row.starts_with("video:") || row.starts_with("audio:")) && !rows.contains(&row.as_str()) {
                rows.push(row);
            }
        }
    }
    let mut inputs: Vec<Input> = Vec::new();
    for row in rows {
        let kind = if row.starts_with("video:") { Kind::Video } else { Kind::Audio };
        let ids = row_codecs(row);
        let is_row = |stream: &StreamProbe| {
            ids.contains(&stream.codec.as_str()) && stream.reference.as_ref().and_then(|r| r.codec.as_deref())
                .is_none_or(|codec| ids.contains(&codec))
        };
        let mut best: Option<(f64, &str, StreamProbe, String)> = None;
        let mut problems: Vec<String> = Vec::new();
        for entry in &manifest.entry {
            let claims_row = entry.rows.iter().any(|r| r == row);
            // These rows name specific encoder tags, not the entire family.
            if matches!(row, "video:xvid" | "video:3ivx") && !claims_row {
                continue;
            }
            let path = resolve(&entry.path);
            match probe_cached(ctx, cache, &path) {
                Err(e) if claims_row => problems.push(format!("{}: {e}", entry.path)),
                Err(_) => {}
                Ok(probed) => {
                    let of_kind = probed.streams.iter().filter(|s| s.kind == Some(kind));
                    for s in of_kind.clone() {
                        let secs = s.reference.as_ref().and_then(|r| r.media_secs).or(s.declared_secs).unwrap_or(0.0);
                        if is_row(s) && best.as_ref().is_none_or(|b| secs > b.0) {
                            best = Some((secs, &entry.path, s.clone(), probed.container.clone()));
                        }
                    }
                    if claims_row && !of_kind.clone().any(is_row) {
                        problems.push(format!("{}: no {} stream (found {})", entry.path, ids.join("/"),
                            of_kind.map(|s| match s.reference.as_ref().and_then(|r| r.codec.as_deref()) {
                                Some(codec) if codec != s.codec => format!("{} (FFmpeg: {codec})", s.codec),
                                _ => s.codec.clone(),
                            }).collect::<Vec<_>>().join(", ")));
                    }
                }
            }
        }
        let (source, stream, container) = match best {
            Some((_, source, stream, container)) => (source.to_string(), Some(stream), Some(container)),
            None => (String::new(), None, None),
        };
        if let Some(existing) = inputs
            .iter_mut()
            .find(|i| i.stream.is_some() && i.source == source && i.stream.as_ref().map(|s| s.index) == stream.as_ref().map(|s| s.index))
        {
            existing.rows.push(row.to_string());
            continue;
        }
        let problem = stream.is_none().then(|| if problems.is_empty() { "no manifest entry".to_string() } else { problems.join("; ") });
        inputs.push(Input {
            rows: vec![row.to_string()],
            path: if source.is_empty() { PathBuf::new() } else { resolve(&source) },
            source,
            kind,
            container,
            stream,
            note: None,
            problem,
        });
    }
    inputs
}

fn standard_inputs(ctx: &RuntimeContext, cache: &mut HashMap<PathBuf, Result<Probed, String>>) -> Vec<Input> {
    STANDARD
        .iter()
        .map(|s| {
            let path = if s.file.starts_with("fate:") { resolve(s.file) } else { corpus_dir().join("perf").join(s.file) };
            let (container, stream, problem) = match probe_cached(ctx, cache, &path) {
                Err(e) => (None, None, Some(format!("{e} (run corpus/perf-inputs.sh)"))),
                Ok(probed) => match probed.streams.iter().find(|p| p.kind == Some(s.kind)) {
                    Some(stream) => (Some(probed.container.clone()), Some(stream.clone()), None),
                    None => (Some(probed.container.clone()), None, Some(format!("no {:?} stream", s.kind))),
                },
            };
            Input {
                rows: vec![s.label.to_string()],
                source: if s.file.starts_with("fate:") { s.file.to_string() } else { format!("perf/{}", s.file) },
                path,
                kind: s.kind,
                container,
                stream,
                note: None,
                problem,
            }
        })
        .collect()
}

// ---------------------------------------------------------------- decoding

/// The stream's decoder, built as `player::engine`'s `make_decoder` builds
/// it: H.264 whose extradata is Annex B gets its parameter sets as a packet
/// of their own ahead of the stream. Serial, as the player runs it.
fn make_decoder(ctx: &RuntimeContext, params: &CodecParameters) -> Result<Box<dyn Decoder>, String> {
    let annex_b = params.extradata.starts_with(&[0, 0, 1]) || params.extradata.starts_with(&[0, 0, 0, 1]);
    let mut decoder = if params.codec_id.as_str() != "h264" || !annex_b {
        ctx.codecs.first_decoder(params).map_err(|e| e.to_string())?
    } else {
        let mut bare = params.clone();
        let parameter_sets = std::mem::take(&mut bare.extradata);
        let mut decoder = ctx.codecs.first_decoder(&bare).map_err(|e| e.to_string())?;
        decoder
            .send_packet(&Packet {
                stream_index: 0,
                time_base: TimeBase::new(1, 1000),
                pts: None,
                dts: None,
                duration: None,
                flags: Default::default(),
                data: parameter_sets,
            })
            .map_err(|e| e.to_string())?;
        decoder
    };
    decoder.set_execution_context(&ExecutionContext::serial());
    Ok(decoder)
}

fn panic_text(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "panic".into())
}

/// What one pass over a stream produced.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Tally {
    /// Video frames or audio frames out of the decoder.
    frames: u64,
    /// Audio samples per channel.
    samples: u64,
    /// Packets sent to the decoder.
    packets: u64,
    /// `send_packet` / `receive_frame` errors (the player skips the packet).
    errors: u64,
    first_error: Option<String>,
    /// Packet pts span in seconds (first, last), for a frame rate fallback.
    #[serde(skip)]
    pts_span: Option<(f64, f64)>,
}

impl Tally {
    fn error(&mut self, e: impl std::fmt::Display) {
        self.errors += 1;
        if self.first_error.is_none() {
            self.first_error = Some(e.to_string());
        }
    }

    fn packet(&mut self, p: &Packet) {
        self.packets += 1;
        if let Some(pts) = p.pts.filter(|_| p.time_base.0.num > 0 && p.time_base.0.den > 0) {
            let t = p.time_base.seconds_of(pts);
            self.pts_span = Some(self.pts_span.map_or((t, t), |(a, b)| (a.min(t), b.max(t))));
        }
    }
}

/// Opens `bytes` with `container`, decodes stream `index` in full (or until
/// `deadline`), and hands every frame to `sink` with the decoder (for its
/// reported output format) and the stream's parameters, which it also
/// returns.
fn decode_stream(
    ctx: &RuntimeContext,
    container: &str,
    bytes: &Arc<[u8]>,
    index: u32,
    deadline: Option<Instant>,
    demux_time: &mut Duration,
    sink: &mut dyn FnMut(Frame, &dyn Decoder, &CodecParameters),
) -> Result<(Tally, Box<dyn Decoder>, CodecParameters, bool), String> {
    let mut demuxer = ctx
        .containers
        .open_demuxer(container, Box::new(Cursor::new(Arc::clone(bytes))), &ctx.codecs)
        .map_err(|e| format!("{container} demuxer: {e}"))?;
    let stream = demuxer.streams().iter().find(|s| s.index == index).cloned().ok_or("stream vanished")?;
    let _ = demuxer.set_active_streams(&[index]);
    let mut decoder = make_decoder(ctx, &stream.params)?;
    let mut tally = Tally::default();
    let mut drain = |decoder: &mut Box<dyn Decoder>, tally: &mut Tally| loop {
        match decoder.receive_frame() {
            Ok(frame) => {
                tally.frames += 1;
                if let Frame::Audio(a) = &frame {
                    tally.samples += a.samples as u64;
                }
                sink(frame, decoder.as_ref(), &stream.params);
            }
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => {
                tally.error(e);
                break;
            }
        }
    };
    let mut capped = false;
    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            capped = true;
            break;
        }
        let t = Instant::now();
        let packet = demuxer.next_packet();
        *demux_time += t.elapsed();
        match packet {
            Ok(p) if p.stream_index == index => {
                tally.packet(&p);
                if let Err(e) = decoder.send_packet(&p) {
                    tally.error(e);
                }
                drain(&mut decoder, &mut tally);
            }
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => {
                tally.error(format!("demux: {e}"));
                break;
            }
        }
    }
    if !capped {
        if let Err(e) = decoder.flush() {
            tally.error(e);
        }
        drain(&mut decoder, &mut tally);
    }
    Ok((tally, decoder, stream.params, capped))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Run {
    wall_secs: f64,
    demux_secs: f64,
    media_secs: f64,
    xrt: f64,
    /// User + system CPU seconds of the process during the run: on a shared
    /// machine the wall clock also counts time spent waiting for a core.
    #[serde(default)]
    cpu_secs: f64,
    /// `media_secs / cpu_secs`.
    #[serde(default)]
    cpu_xrt: f64,
    /// Instructions retired and cycles during the run (macOS; 0 elsewhere).
    #[serde(default)]
    instructions: u64,
    #[serde(default)]
    cycles: u64,
    capped: bool,
    sample_rate: Option<u32>,
    channels: Option<u16>,
    #[serde(flatten)]
    tally: Tally,
}

/// The process's CPU seconds, instructions and cycles so far.
#[derive(Clone, Copy)]
struct Counters {
    cpu_secs: f64,
    instructions: u64,
    cycles: u64,
}

impl Counters {
    fn now() -> Counters {
        // SAFETY: getrusage fills the zeroed struct it is given.
        let cpu_secs = unsafe {
            let mut ru: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut ru);
            let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
            tv(ru.ru_utime) + tv(ru.ru_stime)
        };
        #[cfg(target_os = "macos")]
        // SAFETY: proc_pid_rusage writes one rusage_info_v4 into the buffer.
        let (instructions, cycles) = unsafe {
            let mut info: libc::rusage_info_v4 = std::mem::zeroed();
            let ok = libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, (&raw mut info).cast());
            if ok == 0 { (info.ri_instructions, info.ri_cycles) } else { (0, 0) }
        };
        #[cfg(not(target_os = "macos"))]
        let (instructions, cycles) = (0, 0);
        Counters { cpu_secs, instructions, cycles }
    }
}

/// The media clock of a decode: audio samples over the output rate, video
/// frames over the frame rate (FFmpeg's, else the container's, else the
/// packets').
fn media_secs(kind: Kind, tally: &Tally, rate: Option<f64>) -> Option<f64> {
    match kind {
        Kind::Audio => rate.filter(|r| *r > 0.0).map(|r| tally.samples as f64 / r),
        Kind::Video => {
            let fps = rate.filter(|r| *r > 0.0).or_else(|| {
                let (a, b) = tally.pts_span?;
                (tally.packets > 1 && b > a).then(|| (tally.packets - 1) as f64 / (b - a))
            })?;
            Some(tally.frames as f64 / fps)
        }
    }
}

fn timed_run(ctx: &RuntimeContext, input: &Input, bytes: &Arc<[u8]>, fps: Option<f64>, max: Duration) -> Result<Run, String> {
    let (container, stream) = (input.container.as_deref().unwrap(), input.stream.as_ref().unwrap());
    let mut demux = Duration::ZERO;
    let mut output = None;
    let start = Instant::now();
    let before = Counters::now();
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        decode_stream(ctx, container, bytes, stream.index, (!max.is_zero()).then(|| start + max), &mut demux, &mut |frame, decoder, params| {
            if let Frame::Audio(audio) = &frame {
                output = Some(pcm::layout(decoder, params, audio));
            }
            std::hint::black_box(frame);
        })
    }));
    let wall = start.elapsed().as_secs_f64();
    let after = Counters::now();
    let cpu = after.cpu_secs - before.cpu_secs;
    let (tally, decoder, params, capped) = match outcome {
        Ok(r) => r?,
        Err(p) => return Err(format!("panicked: {}", panic_text(&p))),
    };
    let output = output.or_else(|| decoder.output_audio_format());
    let rate = match input.kind {
        Kind::Audio => output.map(|f| f64::from(f.sample_rate)),
        Kind::Video => fps,
    };
    let media = media_secs(input.kind, &tally, rate).ok_or_else(|| {
        tally.first_error.clone().unwrap_or_else(|| "no media clock: unknown sample or frame rate".into())
    })?;
    Ok(Run { wall_secs: wall, demux_secs: demux.as_secs_f64(), media_secs: media, xrt: media / wall, capped,
        cpu_secs: cpu, cpu_xrt: if cpu > 0.0 { media / cpu } else { 0.0 },
        instructions: after.instructions - before.instructions, cycles: after.cycles - before.cycles,
        sample_rate: output.map(|f| f.sample_rate).or(params.sample_rate),
        channels: output.map(|f| f.channels).or(params.channels), tally })
}

// ---------------------------------------------------------------- FFmpeg

/// What `ffprobe` says about stream `0:<kind>:<nth>`.
#[derive(Clone, Debug, Default, Serialize)]
struct FfprobeStream {
    codec: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    channels: Option<u16>,
    sample_rate: Option<u32>,
    fps: Option<f64>,
    media_secs: Option<f64>,
    pixel_format: Option<String>,
}

fn ffprobe_stream(path: &Path, kind: Kind, nth: usize) -> Option<FfprobeStream> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", &format!("{}:{nth}", kind.letter())])
        .args(["-show_packets", "-show_entries", "stream=codec_name,pix_fmt,width,height,channels,sample_rate,avg_frame_rate,r_frame_rate:packet=pts_time,duration_time"])
        .args(["-of", "json"])
        .arg(path)
        .output()
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let s = json.get("streams")?.get(0)?;
    let rate = |key: &str| {
        let (n, d) = s.get(key)?.as_str()?.split_once('/')?;
        let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
        (n > 0.0 && d > 0.0 && n / d <= 400.0).then(|| n / d)
    };
    Some(FfprobeStream {
        codec: s.get("codec_name").and_then(|v| v.as_str()).map(String::from),
        pixel_format: s.get("pix_fmt").and_then(|v| v.as_str()).map(String::from),
        width: s.get("width").and_then(|v| v.as_u64()).map(|v| v as u32),
        height: s.get("height").and_then(|v| v.as_u64()).map(|v| v as u32),
        channels: s.get("channels").and_then(|v| v.as_u64()).map(|v| v as u16),
        sample_rate: s.get("sample_rate").and_then(|v| v.as_str()).and_then(|v| v.parse().ok()),
        fps: rate("avg_frame_rate").or_else(|| rate("r_frame_rate")),
        media_secs: json.get("packets").and_then(|v| v.as_array()).and_then(|packets| {
            let mut lo = f64::INFINITY;
            let mut hi = f64::NEG_INFINITY;
            for p in packets {
                let number = |key| p.get(key).and_then(|v| v.as_str()).and_then(|v| v.parse::<f64>().ok());
                if let Some(t) = number("pts_time") {
                    lo = lo.min(t);
                    hi = hi.max(t + number("duration_time").unwrap_or(0.0));
                }
            }
            (hi > lo).then_some(hi - lo)
        }),
    })
}

/// FFmpeg's own decode of the stream, single-threaded: its `-benchmark`
/// real time in seconds.
fn ffmpeg_decode_secs(path: &Path, kind: Kind, nth: usize) -> Result<f64, String> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-nostats", "-benchmark", "-threads", "1", "-i"])
        .arg(path)
        .args(["-map", &format!("0:{}:{nth}", kind.letter()), "-f", "null", "-"])
        .output()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(format!("ffmpeg failed: {}", stderr.lines().last().unwrap_or("")));
    }
    stderr
        .lines()
        .filter_map(|l| l.split("rtime=").nth(1))
        .find_map(|r| r.trim().trim_end_matches('s').parse::<f64>().ok())
        .ok_or_else(|| "no rtime in ffmpeg -benchmark output".into())
}

/// The untimed decode compared with FFmpeg's complete decode.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Check {
    method: String,
    /// Our frames (video) or samples per channel (audio).
    ours: u64,
    /// FFmpeg's.
    ffmpeg: u64,
    /// Video: frames whose MD5 equals FFmpeg's frame at the same position.
    matched: Option<u64>,
    /// Audio: SNR in dB over the common length (infinite when bit-exact).
    snr_db: Option<f64>,
    pcm_exact: Option<bool>,
    snr_text: Option<String>,
    /// Every frame/sample present; video bit-exact or audio SNR ≥ 90 dB.
    complete: bool,
    error: Option<String>,
}

/// Plane (bytes per row, rows) of a `format` picture, FFmpeg's packed layout.
fn plane_dims(format: PixelFormat, width: u32, height: u32) -> Option<Vec<(usize, usize)>> {
    (0..format.plane_count())
        .map(|i| Some((format.plane_row_bytes(i, width)?, format.plane_dimensions(i, width, height)?.1 as usize)))
        .collect()
}

fn check(ctx: &RuntimeContext, input: &Input, bytes: &Arc<[u8]>) -> Check {
    let outcome = catch_unwind(AssertUnwindSafe(|| match input.kind {
        Kind::Video => check_video(ctx, input, bytes),
        Kind::Audio => check_audio(input),
    }));
    match outcome {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => Check { error: Some(e), ..Check::default() },
        Err(p) => Check { error: Some(format!("panicked: {}", panic_text(&p))), ..Check::default() },
    }
}

fn check_video(ctx: &RuntimeContext, input: &Input, bytes: &Arc<[u8]>) -> Result<Check, String> {
    let (container, stream) = (input.container.as_deref().unwrap(), input.stream.as_ref().unwrap());
    let mut layout: Option<(PixelFormat, u32, u32, Vec<(usize, usize)>)> = None;
    let mut ours: Vec<String> = Vec::new();
    let mut failure: Option<String> = None;
    let mut demux = Duration::ZERO;
    let (tally, _, _, _) = decode_stream(ctx, container, bytes, stream.index, None, &mut demux, &mut |frame, decoder, p| {
        let Frame::Video(v) = frame else { return };
        if layout.is_none() && failure.is_none() {
            let reference_format = stream.reference.as_ref().and_then(|r| r.pixel_format.as_deref()).and_then(|name| {
                [PixelFormat::Yuv420P, PixelFormat::Yuv422P, PixelFormat::Yuv444P, PixelFormat::Yuv411P,
                 PixelFormat::Yuv420P10Le, PixelFormat::Yuv422P10Le, PixelFormat::Yuv444P10Le,
                 PixelFormat::Rgb24, PixelFormat::Bgr24, PixelFormat::Pal8, PixelFormat::Gray8]
                    .into_iter().find(|p| refcheck::ffmpeg_pix_fmt_name(*p) == Some(name))
            });
            let format = decoder.output_pixel_format().or(p.pixel_format).or(reference_format).unwrap_or(PixelFormat::Yuv420P);
            match (stream.width.or(p.width), stream.height.or(p.height)) {
                (Some(w), Some(h)) => match plane_dims(format, w, h) {
                    Some(dims) => layout = Some((format, w, h, dims)),
                    None => failure = Some(format!("no plane layout for {format:?}")),
                },
                _ => failure = Some("stream has no dimensions".into()),
            }
        }
        if let Some((_, _, _, dims)) = &layout {
            ours.push(refcheck::md5_hex(&refcheck::pack(&v, dims)));
        }
    })?;
    if let Some(e) = failure {
        return Err(e);
    }
    let (format, w, h, _) = layout.ok_or("no frames decoded")?;
    let pix = refcheck::ffmpeg_pix_fmt(format);
    let expect = refcheck::ffmpeg_video_md5s_with(&input.path, stream.nth, pix, &["-idct", "simple"]);
    let matched = ours.iter().zip(&expect).filter(|(a, b)| a == b).count() as u64;
    Ok(Check {
        method: format!("md5 per frame, {pix} {w}x{h}, FFmpeg -idct simple"),
        ours: tally.frames,
        ffmpeg: expect.len() as u64,
        matched: Some(matched),
        snr_db: None,
        pcm_exact: None,
        snr_text: None,
        complete: tally.errors == 0 && tally.frames > 0 && tally.frames == expect.len() as u64 && matched == expect.len() as u64,
        error: tally.first_error.map(|e| format!("{} decode errors, first: {e}", tally.errors)),
    })
}

/// The player's audio, compared with FFmpeg's. It comes from
/// `refcheck::decode`: the production registry with the encoder delay and end
/// padding the container declares and the decoder's start delay removed, as
/// the engine removes them and as FFmpeg's decode does.
fn check_audio(input: &Input) -> Result<Check, String> {
    let stream = input.stream.as_ref().unwrap();
    let decoded = refcheck::decode(&input.path, &[codecs::register_all], MediaType::Audio, stream.nth);
    let max_frame_samples = decoded
        .frames
        .iter()
        .filter_map(|f| if let Frame::Audio(a) = f { Some(a.samples as u64) } else { None })
        .max()
        .unwrap_or(0);
    let channels = decoded.frame_formats.iter().flatten().last().map(|f| f.channels)
        .or(decoded.audio_format.map(|f| f.channels))
        .or(decoded.params.channels).unwrap_or(1).max(1) as usize;
    let ours = refcheck::interleaved_f32(&decoded);
    let reference = pcm::reference_f32(&input.path, stream.nth)?;
    let ffmpeg_channels = stream.reference.as_ref().and_then(|s| s.channels).unwrap_or(channels as u16);
    let snr_result = if ffmpeg_channels as usize == channels {
        refcheck::try_snr_db(&reference, &ours, usize::MAX)
    } else {
        Err(format!("{channels} channels vs FFmpeg's {ffmpeg_channels}"))
    };
    let snr = snr_result.as_ref().ok().copied();
    let (ours_n, ffmpeg_n) = ((ours.len() / channels) as u64, (reference.len() / ffmpeg_channels.max(1) as usize) as u64);
    // At most one decoder frame, as the shared codec contract requires.
    let slack = max_frame_samples;
    let mut error = (!decoded.trim_fallbacks.is_empty()).then(|| format!("audio trims not applied: {:?}", decoded.trim_fallbacks));
    if let Err(e) = snr_result {
        error = Some(e);
    }
    if let Some(rate) = stream.reference.as_ref().and_then(|s| s.sample_rate) {
        if decoded.frame_formats.iter().flatten().any(|f| f.sample_rate != rate) {
            error = Some(format!("player output sample rate differs from FFmpeg's {rate} Hz"));
        }
    }
    let lossless = matches!(stream.codec.as_str(), "flac" | "alac" | "ape" | "tta" | "wavpack" | "ralf" |
        "wmalossless" | "mlp" | "truehd" | "ra_144") || stream.codec.starts_with("pcm_");
    let pcm_exact = if lossless { Some(pcm::exact(&decoded, &input.path, stream.nth)?) } else { None };
    Ok(Check {
        method: if lossless { "full PCM bytes vs FFmpeg -cpuflags 0" } else { "SNR vs FFmpeg f32 -cpuflags 0" }.into(),
        ours: ours_n,
        ffmpeg: ffmpeg_n,
        matched: None,
        snr_db: snr.filter(|s| s.is_finite()),
        pcm_exact,
        snr_text: snr.map(|s| format!("{s:.2}")),
        complete: error.is_none() && ours_n > 0 && if lossless {
            pcm_exact == Some(true)
        } else {
            ours_n.abs_diff(ffmpeg_n) <= slack && snr.is_some_and(|s| s >= 90.0)
        },
        error,
    })
}

// ---------------------------------------------------------------- report

#[derive(Serialize, Deserialize)]
struct Measured {
    rows: Vec<String>,
    input: String,
    path: String,
    kind: Kind,
    container: Option<String>,
    codec: Option<String>,
    decoder: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    channels: Option<u16>,
    sample_rate: Option<u32>,
    /// Media seconds the median run decoded.
    media_secs: Option<f64>,
    /// Wall seconds of the median run.
    decode_secs: Option<f64>,
    /// Median ×real-time on the wall clock.
    xrt: Option<f64>,
    /// Median ×real-time on the process's CPU time. Verdicts use it: on a
    /// shared machine the wall clock also counts waits for a core.
    #[serde(default)]
    cpu_xrt: Option<f64>,
    /// Instructions retired by the run with the median CPU time.
    #[serde(default)]
    instructions: Option<u64>,
    floor: f64,
    /// Speed and independent output validity: ok / SLOW / FAIL / SLOW! / ERROR / MISSING.
    verdict: Cow<'static, str>,
    /// FFmpeg's single-threaded ×real-time on the same stream.
    ffmpeg_xrt: Option<f64>,
    check: Option<Check>,
    runs: Vec<Run>,
    load_avg: Option<f64>,
    note: Option<String>,
    error: Option<String>,
}

impl Measured {
    fn shape(&self) -> String {
        match self.kind {
            Kind::Video => match (self.width, self.height) {
                (Some(w), Some(h)) => format!("{w}x{h}"),
                _ => "?".into(),
            },
            Kind::Audio => format!(
                "{}ch {}k",
                self.channels.map_or("?".into(), |c| c.to_string()),
                self.sample_rate.map_or("?".into(), |r| format!("{}", r as f64 / 1000.0))
            ),
        }
    }

    fn headroom(&self) -> f64 {
        self.cpu_xrt.or(self.xrt).map_or(f64::INFINITY, |x| x / self.floor)
    }
}

fn floor_for(kind: Kind, height: Option<u32>) -> f64 {
    match kind {
        Kind::Audio => AUDIO_FLOOR,
        Kind::Video if height.is_some_and(|h| h > 576) => HD_FLOOR,
        Kind::Video => SD_FLOOR,
    }
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

fn load_average() -> Option<f64> {
    let mut load = [0f64; 3];
    // SAFETY: getloadavg writes at most `nelem` doubles into the buffer.
    let n = unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
    (n >= 1).then_some(load[0])
}

/// Asks the scheduler for a performance core: a busy machine otherwise parks
/// default-QoS threads on efficiency cores, which would halve the numbers.
fn prefer_performance_cores() {
    #[cfg(target_os = "macos")]
    // SAFETY: changes the calling thread's QoS class; no memory involved.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

fn sysctl(name: &str) -> String {
    Command::new("sysctl")
        .args(["-n", name])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

struct Options {
    out: PathBuf,
    runs: usize,
    max_secs: f64,
    filters: Vec<String>,
    check: bool,
    ffmpeg: bool,
    list: bool,
    loop_secs: Option<f64>,
    resume: bool,
}

fn parse_args() -> Options {
    let mut o = Options {
        out: PathBuf::from("target/perf/perf.json"),
        runs: 3,
        max_secs: 0.0,
        filters: Vec::new(),
        check: true,
        ffmpeg: true,
        list: false,
        loop_secs: None,
        resume: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().unwrap_or_else(|| panic!("{name} needs a value"));
        match arg.as_str() {
            "--out" => o.out = PathBuf::from(value("--out")),
            "--runs" => o.runs = value("--runs").parse().expect("--runs <n>"),
            "--max-secs" => o.max_secs = value("--max-secs").parse().expect("--max-secs <seconds>"),
            "--filter" => o.filters.push(value("--filter").to_ascii_lowercase()),
            "--loop" => o.loop_secs = Some(value("--loop").parse().expect("--loop <seconds>")),
            "--no-check" => o.check = false,
            "--no-ffmpeg" => o.ffmpeg = false,
            "--list" => o.list = true,
            "--resume" => o.resume = true,
            "-h" | "--help" => {
                eprintln!(
                    "perf [--out target/perf/perf.json] [--runs 3] [--max-secs 0 (unlimited)] [--filter <s> (repeatable, any match)] [--no-check] [--no-ffmpeg] [--list] [--loop <secs>] [--resume]"
                );
                std::process::exit(0);
            }
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(o.runs >= 1, "--runs must be at least 1");
    o
}

fn matches(input: &Input, filters: &[String]) -> bool {
    filters.is_empty() || input.rows.iter().map(String::as_str)
        .chain(std::iter::once(input.source.as_str()))
        .chain(input.stream.as_ref().map(|s| s.codec.as_str()))
        .any(|text| {
            let text = text.to_ascii_lowercase();
            filters.iter().any(|f| text.contains(f))
        })
}

fn measure(ctx: &RuntimeContext, input: &Input, opts: &Options) -> Measured {
    let stream = input.stream.as_ref();
    let mut m = Measured {
        rows: input.rows.clone(),
        input: input.source.clone(),
        path: input.path.display().to_string(),
        kind: input.kind,
        container: input.container.clone(),
        codec: stream.map(|s| s.codec.clone()),
        decoder: stream.and_then(|s| s.decoder.clone()),
        width: stream.and_then(|s| s.width),
        height: stream.and_then(|s| s.height),
        channels: stream.and_then(|s| s.channels),
        sample_rate: stream.and_then(|s| s.sample_rate),
        media_secs: None,
        decode_secs: None,
        xrt: None,
        cpu_xrt: None,
        instructions: None,
        floor: floor_for(input.kind, stream.and_then(|s| s.height)),
        verdict: "ERROR".into(),
        ffmpeg_xrt: None,
        check: None,
        runs: Vec::new(),
        load_avg: None,
        note: input.note.clone(),
        error: input.problem.clone(),
    };
    let Some(stream) = stream else {
        m.verdict = if input.problem.as_deref().is_some_and(|p| p.contains("missing file")) { "MISSING" } else { "ERROR" }.into();
        return m;
    };
    if stream.decoder.is_none() {
        m.error = Some(format!("no decoder for {}", stream.codec));
        return m;
    }
    let bytes: Arc<[u8]> = match std::fs::read(&input.path) {
        Ok(b) => b.into(),
        Err(e) => {
            m.error = Some(format!("read: {e}"));
            return m;
        }
    };
    let probe = &stream.reference;
    if let Some(p) = &probe {
        m.width = m.width.or(p.width);
        m.height = m.height.or(p.height);
        m.channels = m.channels.or(p.channels);
        m.sample_rate = m.sample_rate.or(p.sample_rate);
        m.floor = floor_for(input.kind, m.height);
    }
    // FFmpeg's frame rate first: containers that carry none (elementary
    // streams, PS/TS) would otherwise fall back to packet timestamps.
    let fps = stream.reference.as_ref().and_then(|p| p.fps);
    m.load_avg = load_average();
    let max = Duration::from_secs_f64(opts.max_secs);
    for _ in 0..opts.runs {
        match timed_run(ctx, input, &bytes, fps, max) {
            Ok(run) => m.runs.push(run),
            Err(e) => {
                m.error = Some(e);
                return m;
            }
        }
    }
    let xrt = median(m.runs.iter().map(|r| r.xrt).collect()).unwrap();
    let mid = m.runs.iter().find(|r| r.xrt == xrt).unwrap();
    m.xrt = Some(xrt);
    let cpu_xrt = median(m.runs.iter().map(|r| r.cpu_xrt).collect()).unwrap();
    m.cpu_xrt = Some(cpu_xrt).filter(|x| *x > 0.0);
    m.instructions = m.runs.iter().find(|r| r.cpu_xrt == cpu_xrt).map(|r| r.instructions).filter(|n| *n > 0);
    // Verdicts on CPU time when the platform reports it.
    let speed = m.cpu_xrt.unwrap_or(xrt);
    m.media_secs = Some(mid.media_secs);
    m.decode_secs = Some(mid.wall_secs);
    m.channels = mid.channels.filter(|c| *c > 0).or(m.channels);
    m.sample_rate = mid.sample_rate.filter(|r| *r > 0).or(m.sample_rate);
    m.verdict = if speed < m.floor { "SLOW" } else { "ok" }.into();
    if opts.check {
        m.check = Some(check(ctx, input, &bytes));
        if !m.check.as_ref().unwrap().complete {
            m.verdict = if speed < m.floor { "SLOW!" } else { "FAIL" }.into();
        }
    } else {
        m.verdict = "UNCHECKED".into();
    }
    if mid.tally.frames == 0 {
        m.verdict = "ERROR".into();
        m.xrt = None;
        m.cpu_xrt = None;
        m.error = Some(mid.tally.first_error.clone().unwrap_or("no frames decoded".into()));
    } else if mid.tally.errors > 0 {
        m.error = Some(format!("{} decode errors: {}", mid.tally.errors, mid.tally.first_error.as_deref().unwrap_or("")));
    }
    if opts.ffmpeg {
        let secs: Vec<f64> = (0..opts.runs).filter_map(|_| ffmpeg_decode_secs(&input.path, input.kind, stream.nth).ok()).collect();
        // FFmpeg's media length: its own frame / sample count when the check
        // ran, else ours.
        let media = match (&m.check, input.kind) {
            (Some(c), Kind::Video) if c.ffmpeg > 0 => fps.map(|f| c.ffmpeg as f64 / f),
            (Some(c), Kind::Audio) if c.ffmpeg > 0 => stream.reference.as_ref().and_then(|p| p.sample_rate).map(|r| c.ffmpeg as f64 / r as f64),
            _ => None,
        };
        let full = m.runs.iter().find(|r| !r.capped).map(|r| r.media_secs);
        if let (Some(media), Some(s)) = (media.or(full), median(secs)) {
            m.ffmpeg_xrt = Some(media / s);
        }
    }
    if m.xrt.is_some() && m.runs.iter().any(|r| r.capped) {
        m.verdict = "CAPPED".into();
    }
    m
}

fn print_table(rows: &[&Measured]) {
    println!(
        "{:<3} {:<7} {:>8} {:>8} {:>5} {:>9}  {:<26} {:<44} {:<10} {:>7} {:>8} {:>8}  {:<22} rows",
        "#", "verdict", "cpu×RT", "wall×RT", "floor", "ffmpeg×RT", "codec (decoder)", "input", "shape", "media s", "decode s", "Ginstr", "check"
    );
    for (i, m) in rows.iter().enumerate() {
        let codec = match (&m.codec, &m.decoder) {
            (Some(c), Some(d)) if d != c => format!("{c} ({d})"),
            (Some(c), _) => c.clone(),
            _ => "-".into(),
        };
        let check = match &m.check {
            None => "-".into(),
            Some(c) if c.ours == 0 && c.ffmpeg == 0 => format!("err: {}", c.error.as_deref().unwrap_or("?")),
            Some(c) => match (c.matched, c.snr_db) {
                (Some(k), _) => format!("md5 {k}/{} ours {}", c.ffmpeg, c.ours),
                (_, _) if c.pcm_exact.is_some() => format!("PCM {} {}/{}", if c.pcm_exact == Some(true) { "exact" } else { "DIFF" }, c.ours, c.ffmpeg),
                (_, _) if c.snr_text.is_some() => format!("{} dB {}/{}", c.snr_text.as_deref().unwrap(), c.ours, c.ffmpeg),
                _ => format!("{}/{} {}", c.ours, c.ffmpeg, c.error.as_deref().unwrap_or("")),
            },
        };
        let capped = m.runs.iter().any(|r| r.capped);
        println!(
            "{:<3} {:<7} {:>8} {:>8} {:>5} {:>9}  {:<26} {:<44} {:<10} {:>7} {:>8} {:>8}  {:<22} {}",
            i + 1,
            m.verdict,
            m.cpu_xrt.map_or("-".into(), |x| format!("{x:.2}{}", if capped { "*" } else { "" })),
            m.xrt.map_or("-".into(), |x| format!("{x:.2}{}", if capped { "*" } else { "" })),
            m.floor,
            m.ffmpeg_xrt.map_or("-".into(), |x| format!("{x:.1}")),
            truncate(&codec, 26),
            truncate(&m.input, 44),
            m.shape(),
            m.media_secs.map_or("-".into(), |x| format!("{x:.2}")),
            m.decode_secs.map_or("-".into(), |x| format!("{x:.3}")),
            m.instructions.map_or("-".into(), |n| format!("{:.2}", n as f64 / 1e9)),
            truncate(&check, 22),
            m.rows.join(" ")
        );
        if let Some(e) = &m.error {
            println!("{:<12}error: {e}", "");
        }
        if let Some(n) = &m.note {
            println!("{:<12}note: {n}", "");
        }
    }
    println!("cpu×RT = media seconds ÷ CPU seconds (verdicts), wall×RT = media seconds ÷ wall seconds; median runs; * = runs stopped at --max-secs. floor: audio {AUDIO_FLOOR}, video >576 lines {HD_FLOOR}, SD {SD_FLOOR}.");
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n - 1).chain(['…']).collect() }
}

fn list(ctx: &RuntimeContext, manifest: &Manifest, cache: &mut HashMap<PathBuf, Result<Probed, String>>) {
    for entry in &manifest.entry {
        let path = resolve(&entry.path);
        match probe_cached(ctx, cache, &path) {
            Err(e) => println!("{}  [{}]  {e}", entry.path, entry.rows.join(" ")),
            Ok(p) => {
                println!("{}  [{}]  {}", entry.path, entry.rows.join(" "), p.container);
                for s in &p.streams {
                    println!(
                        "    #{} {:?}:{} {} -> {}  {}x{} {}ch {}Hz  {:.2}s",
                        s.index,
                        s.kind,
                        s.nth,
                        s.codec,
                        s.decoder.as_deref().unwrap_or("NO DECODER"),
                        s.width.unwrap_or(0),
                        s.height.unwrap_or(0),
                        s.channels.unwrap_or(0),
                        s.sample_rate.unwrap_or(0),
                        s.declared_secs.unwrap_or(0.0)
                    );
                }
            }
        }
    }
    println!("REGISTERED DECODERS (player's first implementation):");
    for id in ctx.codecs.decoder_ids() {
        if let Some(imp) = ctx.codecs.implementations(id).iter().find(|i| i.make_decoder.is_some()) {
            println!("    {} {:?} {}", id.as_str(), imp.caps.media_type, imp.caps.implementation);
        }
    }
}

#[derive(Serialize)]
struct RegistryCoverage {
    codec: String,
    implementation: String,
    measured_inputs: Vec<String>,
}

#[derive(Serialize)]
struct Report<'a> {
    machine: String,
    runs: usize,
    max_secs: f64,
    floors: HashMap<&'static str, f64>,
    results: Vec<&'a Measured>,
    workspace_commit: &'a str,
    harness_md5: &'a str,
    release: bool,
    /// All first-choice audio/video factories, including those for which
    /// the manifest/standard set provides no reachable input.
    registry: Vec<RegistryCoverage>,
}

fn main() {
    let opts = parse_args();
    prefer_performance_cores();
    let ctx = codecs::context();
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/manifest.toml");
    let manifest: Manifest = toml::from_str(&std::fs::read_to_string(&manifest_path).expect("read corpus/manifest.toml"))
        .expect("parse corpus/manifest.toml");
    let mut cache = HashMap::new();
    if opts.list {
        list(&ctx, &manifest, &mut cache);
        return;
    }
    let mut inputs = manifest_inputs(&ctx, &manifest, &mut cache);
    inputs.extend(standard_inputs(&ctx, &mut cache));
    inputs.retain(|i| matches(i, &opts.filters));

    if let Some(secs) = opts.loop_secs {
        eprintln!("pid {}", std::process::id());
        let until = Instant::now() + Duration::from_secs_f64(secs);
        for input in inputs.iter().filter(|i| i.stream.is_some()) {
            let bytes: Arc<[u8]> = std::fs::read(&input.path).expect("read input").into();
            let fps = ffprobe_stream(&input.path, input.kind, input.stream.as_ref().unwrap().nth).and_then(|p| p.fps);
            while Instant::now() < until {
                match timed_run(&ctx, input, &bytes, fps, Duration::from_secs_f64(opts.max_secs)) {
                    Ok(r) => eprintln!("{}: {:.2}×RT cpu, {:.2}×RT wall ({:.2} s in {:.3} s, {:.2} G instr)", input.source, r.cpu_xrt, r.xrt, r.media_secs, r.wall_secs, r.instructions as f64 / 1e9),
                    Err(e) => {
                        eprintln!("{}: {e}", input.source);
                        break;
                    }
                }
            }
        }
        return;
    }

    let (mut results, mut journal) = journal::Journal::open(&opts, &inputs);
    for (i, input) in inputs.iter().enumerate() {
        if results.iter().any(|done| done.rows == input.rows && done.input == input.source && done.path == input.path.to_string_lossy()) {
            continue;
        }
        eprint!("[{}/{}] {} {} … ", i + 1, inputs.len(), input.rows.join(" "), input.source);
        let m = measure(&ctx, input, &opts);
        journal.append(&m);
        eprintln!(
            "{} {}",
            m.verdict,
            m.cpu_xrt.or(m.xrt).map_or_else(|| m.error.clone().unwrap_or_default(), |x| format!("{x:.2}×RT cpu, load {:.0}", m.load_avg.unwrap_or(0.0)))
        );
        results.push(m);
    }
    assert_eq!(results.len(), inputs.len(), "incomplete selected input set");
    let mut ranked: Vec<&Measured> = results.iter().collect();
    ranked.sort_by(|a, b| a.headroom().total_cmp(&b.headroom()));
    print_table(&ranked);

    let machine = format!(
        "{} ({} performance cores), load {:.1}",
        sysctl("machdep.cpu.brand_string"),
        sysctl("hw.perflevel0.physicalcpu"),
        load_average().unwrap_or(0.0)
    );
    let registry = ctx.codecs.decoder_ids().filter_map(|id| {
        let imp = ctx.codecs.implementations(id).iter().find(|i| i.make_decoder.is_some())?;
        if !matches!(imp.caps.media_type, MediaType::Audio | MediaType::Video) {
            return None;
        }
        Some(RegistryCoverage {
            codec: id.as_str().to_string(),
            implementation: imp.caps.implementation.clone(),
            measured_inputs: results.iter().filter(|m| m.codec.as_deref() == Some(id.as_str()) && m.decode_secs.is_some())
                .map(|m| m.input.clone()).collect(),
        })
    }).collect();
    let report = Report {
        machine,
        workspace_commit: journal.workspace_commit(),
        harness_md5: journal.harness_md5(),
        release: !cfg!(debug_assertions),
        registry,
        runs: opts.runs,
        max_secs: opts.max_secs,
        floors: HashMap::from([("audio", AUDIO_FLOOR), ("video_above_576", HD_FLOOR), ("video_sd", SD_FLOOR)]),
        results: ranked,
    };
    if let Some(dir) = opts.out.parent() {
        std::fs::create_dir_all(dir).expect("create the --out directory");
    }
    std::fs::write(&opts.out, serde_json::to_string_pretty(&report).unwrap()).expect("write --out");
    eprintln!("wrote {}", opts.out.display());
}
