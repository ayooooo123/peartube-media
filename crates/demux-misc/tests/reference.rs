//! Demuxer reference tests for crates/demux-misc against FFmpeg's ffprobe.
//!
//! Inputs: every FATE input that FFmpeg's test makefiles name
//! (tests/fate/*.mak of the FFmpeg tree this crate ports, commit 2da55bf,
//! at `FFMPEG_SRC`, default ~/projects/ffmpeg-src) and that FFmpeg demuxes
//! with one of this crate's formats: candidates by extension, kept when
//! ffprobe names the format. Program streams have their own file
//! (tests/mpegps.rs). A missing sample fails its test.
//!
//! Each input opens through the player's registry and probe rule. Then:
//! - the probe picks the format FFmpeg uses;
//! - the streams at open equal the streams after demuxing and ffprobe's,
//!   in order, type and codec;
//! - the packets equal ffprobe's one for one: stream, size, payload MD5;
//! - containers and raw audio: pts and dts, rescaled to FFmpeg's stream
//!   time base, equal ffprobe's. PVA and NUT compare with FFmpeg's
//!   demuxer output (`-fflags +noparse+nofillin`), excluding parser
//!   reframing and decoder-dependent DTS interpolation.
//! - raw video elementary streams (MPEG-1/2, H.264, HEVC): the access
//!   units and key flags FFmpeg's parsers produce. Key flags use the
//!   port's FFmpeg revision (`FFMPEG_SRC/ffmpeg -dump`): 9.0.2 predates
//!   its H.264 data-partition key detection. Raw MPEG-1/2 also compares
//!   pts, dts (a missing one included) and duration with that revision's
//!   packet table (`FFMPEG_SRC/ffprobe`). H.264 and HEVC timestamps are
//!   not compared: FFmpeg leaves them to its decoder.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use oxideav_core::{Demuxer, TimeBase};

// ───────────────────────── FATE inventory ─────────────────────────

/// A FATE sample's path (FATE_SUITE, default ~/projects/fate-suite),
/// present or not.
fn suite_path(rel: &str) -> PathBuf {
    std::env::var_os("FATE_SUITE")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&std::env::var("HOME").unwrap()).join("projects/fate-suite"))
        .join(rel)
}

fn ffmpeg_src() -> PathBuf {
    std::env::var_os("FFMPEG_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&std::env::var("HOME").unwrap()).join("projects/ffmpeg-src"))
}

/// The text of a makefile with continuation lines joined.
fn joined(text: &str) -> String {
    text.replace("\\\r\n", " ").replace("\\\n", " ")
}

/// The index just past the parenthesis that closes the one before
/// `from` (which follows a `$(`).
fn closing(text: &str, from: usize) -> Option<usize> {
    let mut depth = 1;
    for (i, c) in text[from..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(from + i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Top-level comma-separated arguments of a make function call.
fn arguments(text: &str) -> Vec<String> {
    let (mut args, mut depth, mut start) = (Vec::new(), 0, 0);
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                args.push(text[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    args.push(text[start..].trim().to_string());
    args
}

struct Makefiles {
    vars: HashMap<String, Vec<String>>,
    defines: HashMap<String, String>,
}

impl Makefiles {
    /// The words of `$(NAME)` references in `value`, expanded through
    /// the list variables; other functions drop out.
    fn words(&self, value: &str, depth: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = value;
        while let Some(at) = rest.find("$(") {
            out.extend(rest[..at].split_whitespace().map(str::to_string));
            let Some(end) = closing(rest, at + 2) else { break };
            let inner = &rest[at + 2..end - 1];
            if depth < 8 {
                if let Some(list) = self.vars.get(inner) {
                    out.extend(list.iter().flat_map(|v| self.words(v, depth + 1)));
                } else if let Some(cond) = inner.strip_prefix("if ") {
                    // $(if COND, LIST): every list counts (FATE runs its
                    // large tests by default).
                    if let Some(list) = arguments(cond).get(1) {
                        out.extend(self.words(list, depth + 1));
                    }
                }
            }
            rest = &rest[end..];
        }
        out.extend(rest.split_whitespace().map(str::to_string));
        out
    }

    /// Every sample path `text` names, with `$(foreach ...)` and
    /// `$(call ...)` expanded.
    fn samples(&self, text: &str, out: &mut Vec<String>, depth: usize) {
        const PREFIX: &str = "$(TARGET_SAMPLES)/";
        let mut rest = text;
        while let Some(at) = rest.find(PREFIX) {
            let tail = &rest[at + PREFIX.len()..];
            let len = tail.find(|c: char| !(c.is_ascii_alphanumeric() || "_./+-".contains(c))).unwrap_or(tail.len());
            if !tail[len..].starts_with('$') && len > 0 && !tail[..len].ends_with('/') {
                out.push(tail[..len].to_string());
            }
            rest = &tail[len..];
        }
        if depth >= 6 {
            return;
        }
        let mut rest = text;
        while let Some(at) = rest.find("$(") {
            let Some(end) = closing(rest, at + 2) else { break };
            let inner = &rest[at + 2..end - 1];
            if let Some(args) = inner.strip_prefix("foreach ") {
                let args = arguments(args);
                if let [var, list, body] = &args[..] {
                    for item in self.words(list, 0) {
                        self.samples(&body.replace(&format!("$({var})"), &item), out, depth + 1);
                    }
                }
            } else if let Some(args) = inner.strip_prefix("call ") {
                let args = arguments(args);
                if let Some(body) = self.defines.get(args[0].as_str()) {
                    let mut body = body.clone();
                    for (k, arg) in args.iter().enumerate().skip(1).rev() {
                        body = body.replace(&format!("$({k})"), arg);
                    }
                    self.samples(&body, out, depth + 1);
                }
            } else if !inner.starts_with("TARGET_SAMPLES") {
                self.samples(inner, out, depth + 1);
            }
            rest = &rest[end..];
        }
    }
}

/// Every FATE input path the makefiles name, relative to the suite.
fn fate_inputs() -> &'static [String] {
    static INPUTS: LazyLock<Vec<String>> = LazyLock::new(|| {
        let dir = ffmpeg_src().join("tests/fate");
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("FATE makefiles at {}: {e}", dir.display()))
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "mak"))
            .collect();
        files.sort();
        let texts: Vec<String> = files.iter().map(|f| joined(&std::fs::read_to_string(f).unwrap())).collect();
        let mut make = Makefiles { vars: HashMap::new(), defines: HashMap::new() };
        for text in &texts {
            let mut lines = text.lines();
            while let Some(line) = lines.next() {
                if let Some(name) = line.strip_prefix("define ") {
                    let body: Vec<&str> = lines.by_ref().take_while(|l| l.trim() != "endef").collect();
                    make.defines.insert(name.trim().to_string(), body.join("\n"));
                    continue;
                }
                let Some(eq) = line.find('=') else { continue };
                let (name, value) = (line[..eq].trim_end_matches([':', '+', '?']).trim(), &line[eq + 1..]);
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    let entry = make.vars.entry(name.to_string()).or_default();
                    if !line[..eq].ends_with('+') {
                        entry.clear();
                    }
                    entry.push(value.trim().to_string());
                }
            }
        }
        let mut out = Vec::new();
        for text in &texts {
            make.samples(text, &mut out, 0);
        }
        // hevc.mak: `fate-hevc-conformance-NAME` runs NAME.bit (NAME.bin
        // for the 4:2:2 10-bit .bin set) from the HEVC_SAMPLES_* lists.
        for (name, values) in &make.vars {
            if let Some(set) = name.strip_prefix("HEVC_SAMPLES_") {
                let ext = if set.contains("10BIN") { "bin" } else { "bit" };
                for word in values.iter().flat_map(|v| make.words(v, 0)) {
                    if !word.contains('$') {
                        out.push(format!("hevc-conformance/{word}.{ext}"));
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out
    });
    &INPUTS
}

/// FFmpeg's demuxer for `path`, as ffprobe names it.
fn ffprobe_format(path: &Path) -> String {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-show_entries", "format=format_name", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe must be on PATH");
    assert!(out.status.success(), "ffprobe format {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The FATE inputs with one of `exts` that FFmpeg demuxes as `format`.
fn inventory(format: &str, exts: &[&str]) -> Vec<String> {
    let mut missing = Vec::new();
    let mut found = Vec::new();
    for rel in fate_inputs() {
        let ext = Path::new(rel).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if !exts.contains(&ext.as_str()) {
            continue;
        }
        let path = suite_path(rel);
        if !path.is_file() {
            missing.push(rel.clone());
        } else if ffprobe_format(&path) == format {
            found.push(rel.clone());
        }
    }
    assert!(missing.is_empty(), "{format}: FATE samples missing from the suite: {missing:?}");
    assert!(!found.is_empty(), "{format}: no FATE input found");
    found
}

// ───────────────────────── tables ─────────────────────────

struct FfStream {
    kind: String,
    codec: String,
    time_base: TimeBase,
}

#[derive(Debug, PartialEq)]
struct Pkt {
    stream: u32,
    size: usize,
    md5: String,
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<i64>,
    key: bool,
}

/// The ffprobe of the port's FFmpeg revision (`FFMPEG_SRC/ffprobe`,
/// 2da55bf), checked once to be that revision.
fn port_ffprobe() -> &'static Path {
    static PORT: LazyLock<PathBuf> = LazyLock::new(|| {
        let bin = ffmpeg_src().join("ffprobe");
        let out = std::process::Command::new(&bin)
            .arg("-version")
            .output()
            .expect("build ffprobe in FFMPEG_SRC (the port's revision): make ffprobe");
        let version = String::from_utf8_lossy(&out.stdout);
        assert!(version.contains("2da55bf"), "packet-table oracle must be FFmpeg 2da55bf: {version}");
        bin
    });
    &PORT
}

/// `ffprobe -f format` on `path`: streams, then packets. `port` takes the
/// table from the port's FFmpeg revision instead of the installed one.
fn ffprobe(path: &Path, format: &str, unparsed: bool, port: bool) -> (Vec<FfStream>, Vec<Pkt>) {
    let mut cmd = std::process::Command::new(if port { port_ffprobe() } else { Path::new("ffprobe") });
    cmd.args(["-v", "quiet", "-f", format]);
    if unparsed {
        cmd.args(["-fflags", "+noparse+nofillin"]);
    }
    let out = cmd
        .args(["-show_data_hash", "md5", "-show_entries"])
        .arg("stream=index,codec_type,codec_name,time_base:packet=stream_index,pts,dts,duration,size,flags,data_hash")
        .args(["-of", "compact"])
        .arg(path)
        .output()
        .expect("ffprobe must be on PATH");
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    let num = |v: Option<&&str>| v.and_then(|v| v.parse::<i64>().ok());
    let (mut streams, mut packets) = (Vec::new(), Vec::new());
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
                let (n, d) = kv["time_base"].split_once('/').unwrap();
                streams.push(FfStream {
                    kind: kv["codec_type"].to_string(),
                    codec: kv["codec_name"].to_string(),
                    time_base: TimeBase::new(n.parse().unwrap(), d.parse().unwrap()),
                });
            }
            _ => {}
        }
    }
    assert!(!streams.is_empty() && !packets.is_empty(), "ffprobe {}: empty oracle", path.display());
    (streams, packets)
}

/// The port's parser flags, not the older system ffprobe's: 2da55bf
/// recognizes H.264 partition-A slices, which 9.0.2 does not. Compare
/// every raw-video packet's size as well, so this oracle cannot silently
/// use a different set of boundaries.
fn raw_video_flags(path: &Path, format: &str) -> Vec<(usize, bool)> {
    let out = std::process::Command::new(ffmpeg_src().join("ffmpeg"))
        .args(["-nostdin", "-v", "info", "-dump", "-f", format, "-i"])
        .arg(path)
        .args(["-map", "0:v", "-c", "copy", "-f", "null", "-"])
        .output()
        .expect("build ffmpeg from FFMPEG_SRC (the port's revision)");
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "ffmpeg packet flags {}: {log}", path.display());
    assert!(log.contains("2da55bf"), "packet-flag oracle must be FFmpeg 2da55bf: {log}");
    let mut key = None;
    let mut packets = Vec::new();
    for line in log.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("keyframe=") {
            key = Some(value.parse::<u8>().unwrap() != 0);
        } else if let Some(value) = line.strip_prefix("size=") {
            packets.push((value.parse().unwrap(), key.take().expect("packet key flag")));
        }
    }
    assert!(!packets.is_empty(), "ffmpeg {}: empty packet-flag oracle", path.display());
    packets
}

/// The container the player's probe rule picks (engine-api.md): the best
/// content probe that scores at least an extension match on the first
/// 256 KiB, else the extension's container.
fn probe(ctx: &oxideav_core::RuntimeContext, path: &Path) -> Option<String> {
    use std::io::Read;
    let mut head = Vec::new();
    std::fs::File::open(path).unwrap().take(256 * 1024).read_to_end(&mut head).unwrap();
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let data = oxideav_core::ProbeData { buf: &head, ext: ext.as_deref() };
    match ctx.containers.probe_candidates(&data).first() {
        Some(c) if c.score >= oxideav_core::PROBE_SCORE_EXTENSION => Some(c.name.to_string()),
        _ => ext.as_deref().and_then(|e| ctx.containers.container_for_extension(e)).map(str::to_string),
    }
}

fn identity(streams: &[oxideav_core::StreamInfo]) -> Vec<(String, String)> {
    streams
        .iter()
        .map(|s| (format!("{:?}", s.params.media_type).to_lowercase(), s.params.codec_id.as_str().to_string()))
        .collect()
}

/// av_rescale_q, rounding to nearest with ties away from zero.
fn rescale(ts: i64, from: TimeBase, to: TimeBase) -> i64 {
    let num = i128::from(ts) * i128::from(from.num()) * i128::from(to.den());
    let den = i128::from(from.den()) * i128::from(to.num());
    let q = (num.abs() + den / 2) / den;
    (if num < 0 { -q } else { q }) as i64
}

/// What a comparison covers beyond streams, sizes and payloads.
#[derive(Clone, Copy)]
struct Mode {
    /// Compare with FFmpeg's PES output instead of its parsed packets.
    unparsed: bool,
    /// Compare rescaled timestamps.
    times: bool,
    /// Compare rescaled durations too.
    durations: bool,
    /// Compare key flags.
    keys: bool,
    /// Take the packet table from the port's FFmpeg revision.
    port: bool,
}

const CONTAINER: Mode = Mode { unparsed: false, times: true, durations: false, keys: false, port: false };
/// Raw H.264 and HEVC: access units, key flags, and FFmpeg 2da55bf's
/// timing of them: no pts or dts (FFmpeg does not interpolate H.264 or
/// HEVC timestamps), and the duration compute_frame_duration gives. Where
/// the stream has no frame rate FFmpeg times the packets it reads while
/// analysing the stream by the raw demuxer's 25 fps and later ones by
/// one tick (its r_frame_rate fallback, the time base); the port keeps
/// 25 fps, so those one-tick durations are not compared.
const RAW_VIDEO: Mode = Mode { unparsed: false, times: true, durations: true, keys: true, port: true };
/// Raw MPEG-1/2 video: access units, key flags, and the pts, dts (a
/// missing one included) and duration FFmpeg 2da55bf's demuxer layer
/// gives each.
const RAW_MPEG: Mode = Mode { unparsed: false, times: true, durations: true, keys: true, port: true };

/// `rel` through the player's registry against ffprobe's table for
/// `format`; the first difference, if any.
fn compare(path: &Path, rel: &str, format: &str, mode: Mode) -> Result<(), String> {
    let ctx = codecs::context();
    let picked = probe(&ctx, path);
    if picked.as_deref() != Some(format) {
        return Err(format!("{rel}: probe picked {picked:?}, FFmpeg demuxes it as {format}"));
    }
    let file = std::fs::File::open(path).unwrap();
    let mut demuxer: Box<dyn Demuxer> = ctx
        .containers
        .open_demuxer(format, Box::new(file), &ctx.codecs)
        .map_err(|e| format!("{rel}: open: {e}"))?;
    let at_open = identity(demuxer.streams());
    let time_bases: Vec<TimeBase> = demuxer.streams().iter().map(|s| s.time_base).collect();
    let mut ours = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) => ours.push((p.stream_index, p.time_base, p.data.len(), refcheck::md5_hex(&p.data), p.pts, p.dts, p.duration, p.flags.keyframe)),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => return Err(format!("{rel}: demux after {} packets: {e}", ours.len())),
        }
    }
    let at_end = identity(demuxer.streams());
    if at_open != at_end {
        return Err(format!("{rel}: streams at open {at_open:?}, after demuxing {at_end:?}"));
    }

    let (ff_streams, mut ff_packets) = ffprobe(path, format, mode.unparsed, mode.port);
    if mode.keys {
        let flags = raw_video_flags(path, format);
        assert_eq!(flags.len(), ff_packets.len(), "{rel}: FFmpeg revisions disagree on packet count");
        for ((size, key), packet) in flags.into_iter().zip(&mut ff_packets) {
            assert_eq!(size, packet.size, "{rel}: FFmpeg revisions disagree on packet size");
            packet.key = key;
        }
    }
    let theirs: Vec<(String, String)> = ff_streams.iter().map(|s| (s.kind.clone(), s.codec.clone())).collect();
    if at_open != theirs {
        return Err(format!("{rel}: streams {at_open:?}, ffprobe {theirs:?}"));
    }
    if ours.len() != ff_packets.len() {
        return Err(format!("{rel}: {} packets, ffprobe {}", ours.len(), ff_packets.len()));
    }
    for (n, (&(stream, tb, size, ref md5, pts, dts, duration, key), want)) in ours.iter().zip(&ff_packets).enumerate() {
        let to = ff_streams[want.stream as usize].time_base;
        let tb = if tb.den() == 0 { time_bases[stream as usize] } else { tb };
        let got = Pkt {
            stream,
            size,
            md5: md5.clone(),
            pts: pts.map(|t| rescale(t, tb, to)),
            dts: dts.map(|t| rescale(t, tb, to)),
            duration: duration.map(|t| rescale(t, tb, to)),
            key,
        };
        // FFmpeg's r_frame_rate fallback for a raw stream without a frame
        // rate: one tick of 1/1200000 per packet after stream analysis.
        let one_tick = want.duration == Some(1) && to.as_rational().num == 1 && to.as_rational().den == 1_200_000;
        if got.stream != want.stream || got.size != want.size || got.md5 != want.md5
            || (mode.times && (got.pts != want.pts || got.dts != want.dts))
            || (mode.durations && !one_tick && got.duration != want.duration)
            || (mode.keys && got.key != want.key)
        {
            return Err(format!("{rel}: packet {n}: ours {got:?}, ffprobe {want:?}"));
        }
    }
    Ok(())
}

/// Every inventory input of `format`: all must match.
fn check_inventory(format: &str, exts: &[&str], mode: Mode) {
    let inputs = inventory(format, exts);
    println!("{format}: {} required FATE inputs: {}", inputs.len(), inputs.join(", "));
    let failures: Vec<String> = inputs.iter().filter_map(|rel| compare(&suite_path(rel), rel, format, mode).err()).collect();
    assert!(
        failures.is_empty(),
        "{format}: {} of {} FATE inputs differ from FFmpeg:\n{}",
        failures.len(),
        inputs.len(),
        failures.join("\n")
    );
}

// ───────────────────────── formats ─────────────────────────

/// ac3.mak and spdif.mak: one packet per frame as FFmpeg's ac3 parser
/// cuts them (an E-AC-3 frame with its dependent substreams), FFmpeg's
/// timestamps for a raw stream.
#[test]
fn ac3() {
    check_inventory("ac3", &["ac3"], CONTAINER);
}

#[test]
fn eac3() {
    check_inventory("eac3", &["eac3", "ec3"], CONTAINER);
}

/// A directory for one test's generated inputs, in the scratch directory
/// Cargo gives integration tests; the test removes it after use.
fn scratch_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("demux-misc-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `codec` from FFmpeg's encoder into `dir/name`, raw, from lavfi
/// `source`.
fn encode(dir: &Path, name: &str, codec: &str, source: &str, args: &[&str]) -> PathBuf {
    let path = dir.join(name);
    let out = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", source, "-c:v", codec])
        .args(args)
        .args(["-f", codec])
        .arg(&path)
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(out.status.success(), "{name}: ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    path
}

/// Each sequence header of a raw MPEG-1/2 stream: its offset, its
/// frame_rate_code, and its sequence extension's low_delay,
/// frame_rate_extension_n and frame_rate_extension_d.
fn sequence_headers(bytes: &[u8]) -> Vec<(usize, u8, Option<(u8, u8, u8)>)> {
    let starts: Vec<usize> = (0..bytes.len().saturating_sub(10)).filter(|&i| bytes[i..i + 3] == [0, 0, 1]).collect();
    let extension = |e: usize| (bytes[e + 3] == 0xB5 && bytes[e + 4] >> 4 == 1).then(|| (bytes[e + 9] >> 7, (bytes[e + 9] >> 5) & 3, bytes[e + 9] & 0x1F));
    starts
        .iter()
        .enumerate()
        .filter(|&(_, &at)| bytes[at + 3] == 0xB3)
        .map(|(k, &at)| (at, bytes[at + 7] & 0x0F, starts.get(k + 1).and_then(|&e| extension(e))))
        .collect()
}

/// The offset of each picture header of a raw MPEG-1/2 stream.
fn picture_headers(bytes: &[u8]) -> Vec<usize> {
    (0..bytes.len().saturating_sub(4)).filter(|&i| bytes[i..i + 4] == [0, 0, 1, 0]).collect()
}

/// Raw MPEG-1/2 video: the frames FFmpeg's mpegvideo parser cuts, timed
/// as FFmpeg 2da55bf's demuxer layer times them. Without B-frame delay a
/// frame's pts is its dts; with it, I- and P-frames have no pts and B-frames
/// pts = dts, dts following the previous I/P frame's duration. MPEG-2
/// declares the delay in its sequence extension; for MPEG-1 FFmpeg learns
/// it from its decoder once the first picture is read, or from the first
/// B-frame. Durations count fields, repeated ones included.
///
/// The FATE inventory is MPEG-2 only, so FFmpeg also encodes MPEG-1 with
/// and without B-frames, MPEG-1 small enough that the 1024-byte read that
/// ends the first picture ends the next one too (stamped before FFmpeg's
/// decoder has seen a picture), and MPEG-2 without B-frames, with and
/// without the low-delay flag.
#[test]
fn mpegvideo() {
    let inputs = inventory("mpegvideo", &["m1v", "m2v", "mpv", "bs", "bits", "mpg", "mpeg"]);
    println!("mpegvideo: {} required FATE inputs: {}", inputs.len(), inputs.join(", "));
    let mut failures: Vec<String> =
        inputs.iter().filter_map(|rel| compare(&suite_path(rel), rel, "mpegvideo", RAW_MPEG).err()).collect();
    let dir = scratch_dir("mpegvideo");
    let generated = [
        ("mpeg1-bframes.m1v", "mpeg1video", "176x144", &["-bf", "2"][..]),
        ("mpeg1-ippp.m1v", "mpeg1video", "176x144", &["-bf", "0"][..]),
        ("mpeg1-small.m1v", "mpeg1video", "32x32", &["-bf", "2"][..]),
        ("mpeg2-ippp.m2v", "mpeg2video", "176x144", &["-bf", "0"][..]),
        ("mpeg2-low-delay.m2v", "mpeg2video", "176x144", &["-bf", "0", "-flags", "+low_delay"][..]),
    ];
    for (name, codec, size, args) in generated {
        let path = encode(&dir, name, codec, &format!("testsrc=duration=0.6:size={size}:rate=25"), args);
        failures.extend(compare(&path, &format!("generated {name}"), "mpegvideo", RAW_MPEG).err());
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "mpegvideo: {} of {} inputs differ from FFmpeg:\n{}",
        failures.len(),
        inputs.len() + generated.len(),
        failures.join("\n")
    );
}

/// A sequence change inside the first 1024-byte read. FFmpeg's decoder
/// sees the first picture only once that read is parsed, and the frame
/// rate and B-frame delay it takes from that picture's sequence hold
/// from the next read on, over those of the later sequence the read
/// already parsed. Two MPEG-2 sequences: three pictures of the first,
/// then the second's I-picture, completed in the first read, and its
/// P-pictures, with no sequence header of their own, in that read and
/// two more. 25 fps with B-frame delay then 50 fps low-delay, and the
/// other way round.
#[test]
fn mpegvideo_sequence_change_in_the_first_read() {
    let dir = scratch_dir("mpegvideo-sequence-change");
    // frame rate, its frame_rate_code, low_delay
    let delayed = ("25", 3u8, 0u8);
    let low_delay = ("50", 6u8, 1u8);
    let mut failures = Vec::new();
    for (first, second) in [(delayed, low_delay), (low_delay, delayed)] {
        let name = format!("mpeg2-{}fps-low-delay-{}-then-{}fps-low-delay-{}.m2v", first.0, first.2, second.0, second.2);
        let mut bytes = Vec::new();
        for (part, (rate, _, low_delay), frames) in [("a", first, "3"), ("b", second, "80")] {
            let mut args = vec!["-frames:v", frames, "-g", "1000", "-bf", "0", "-q:v", "31"];
            if low_delay == 1 {
                args.extend(["-flags", "+low_delay"]);
            }
            let source = format!("testsrc=duration=4:size=16x16:rate={rate}");
            bytes.extend(std::fs::read(encode(&dir, &format!("{part}-{name}"), "mpeg2video", &source, &args)).unwrap());
        }
        let sequences = sequence_headers(&bytes);
        let declared: Vec<_> = sequences.iter().map(|&(_, rate, ext)| (rate, ext)).collect();
        assert_eq!(declared, [(first.1, Some((first.2, 0, 0))), (second.1, Some((second.2, 0, 0)))], "{name}: two sequences");
        let pictures: Vec<usize> = picture_headers(&bytes).into_iter().filter(|&p| p > sequences[1].0).collect();
        assert!(pictures[1] + 4 <= 1024, "{name}: the first read ends the second sequence's first picture");
        assert!(bytes.len() > 2 * 1024, "{name}: the second sequence's P-pictures run on through two more reads");
        let path = dir.join(&name);
        std::fs::write(&path, &bytes).unwrap();
        failures.extend(compare(&path, &format!("generated {name}"), "mpegvideo", RAW_MPEG).err());
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(failures.is_empty(), "{} of 2 sequence changes differ from FFmpeg:\n{}", failures.len(), failures.join("\n"));
}

/// What FFmpeg's decoder sets after the first read is what it reads from
/// the first picture's sequence. A frame_rate_code it rejects (15,
/// reserved) reads as 24000/1001, where FFmpeg's parser has no rate and
/// the 25 fps fallback until then. A picture before any sequence header
/// does not count: a stream cut after its first sequence header, low
/// delay, its next one a read later; the pictures before it are not
/// delayed.
#[test]
fn mpegvideo_decoder_reads_the_first_pictures_sequence() {
    let dir = scratch_dir("mpegvideo-decoder");
    let low_delay = ["-bf", "0", "-q:v", "31", "-flags", "+low_delay"];
    let mut failures = Vec::new();

    let name = "mpeg2-reserved-frame-rate-code.m2v";
    let source = "testsrc=duration=4:size=16x16:rate=25";
    let mut bytes = std::fs::read(encode(&dir, "reserved.m2v", "mpeg2video", source, &[&["-frames:v", "80", "-g", "1000"][..], &low_delay].concat())).unwrap();
    for (at, _, _) in sequence_headers(&bytes) {
        bytes[at + 7] |= 0x0F;
    }
    assert!(bytes.len() > 2 * 1024, "{name}: pictures in two more reads");
    std::fs::write(dir.join(name), &bytes).unwrap();
    failures.extend(compare(&dir.join(name), &format!("generated {name}"), "mpegvideo", RAW_MPEG).err());

    let name = "mpeg2-cut-after-the-first-sequence-header.m2v";
    let source = "testsrc=duration=4:size=16x16:rate=50";
    let whole = std::fs::read(encode(&dir, "gop60.m2v", "mpeg2video", source, &[&["-frames:v", "160", "-g", "60"][..], &low_delay].concat())).unwrap();
    let bytes = &whole[picture_headers(&whole)[1]..];
    let first_sequence = sequence_headers(bytes)[0].0;
    let headerless = picture_headers(bytes).into_iter().filter(|&p| p > 1024 && p < first_sequence).count();
    assert!(headerless >= 2, "{name}: pictures with no sequence header before them in the second read");
    std::fs::write(dir.join(name), bytes).unwrap();
    failures.extend(compare(&dir.join(name), &format!("generated {name}"), "mpegvideo", RAW_MPEG).err());

    let _ = std::fs::remove_dir_all(&dir);
    assert!(failures.is_empty(), "{} of 2 inputs differ from FFmpeg:\n{}", failures.len(), failures.join("\n"));
}

/// 36000/1001 fps (frame_rate_code 1, 24000/1001, extended by 3/2): a
/// frame lasts 33366⅔ ticks of 1/1200000, and av_add_stable moves the
/// dts on by 33367, 33367 and 33366 ticks in turn. Which frame gets the
/// short step depends on the timestamp the rounding starts from: FFmpeg's
/// relative origin (RELATIVE_TS_BASE), not 0. Low-delay, so every frame's
/// pts and dts come from that rounding; 22 frames, every phase several
/// times.
#[test]
fn mpegvideo_frames_of_a_fractional_tick_count() {
    let dir = scratch_dir("mpegvideo-fractional-ticks");
    let name = "mpeg2-36000-1001-low-delay.m2v";
    // -force_fps: the frame rate as given, not the nearest standard one.
    let args = ["-force_fps", "-bf", "0", "-flags", "+low_delay", "-q:v", "31"];
    let path = encode(&dir, name, "mpeg2video", "testsrc=duration=0.6:size=16x16:rate=36000/1001", &args);
    let sequences = sequence_headers(&std::fs::read(&path).unwrap());
    assert!(
        !sequences.is_empty() && sequences.iter().all(|&(_, rate, ext)| (rate, ext) == (1, Some((1, 2, 1)))),
        "{name}: frame_rate_code 1, extension n 2 d 1, low_delay: {sequences:?}"
    );
    let result = compare(&path, &format!("generated {name}"), "mpegvideo", RAW_MPEG);
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = result {
        panic!("{e}");
    }
}

/// h264.mak (the conformance suite) and every other raw H.264 input:
/// the access units FFmpeg's h264 parser cuts, untimed, with the duration
/// of their frame rate and picture structure.
#[test]
fn h264() {
    check_inventory("h264", &["264", "26l", "avc", "h264", "jsv", "jvt"], RAW_VIDEO);
}

/// hevc.mak (the conformance suite) and every other raw HEVC input: the
/// access units FFmpeg's hevc parser cuts, untimed, with the duration of
/// their frame rate.
#[test]
fn hevc() {
    check_inventory("hevc", &["bit", "bin", "hevc", "h265", "265"], RAW_VIDEO);
}

/// `x264`/`x265` output with B-frames, as JD's check made it
/// (testsrc 176x144 at 25 fps for 2 s, -bf 2 -g 25), and at 30000/1001:
/// untimed packets lasting a frame (48000 and 40040 of 1/1200000; H.264
/// counts fields, r_frame_rate 50/1).
#[test]
fn x264_and_x265_streams_are_untimed_with_their_frame_durations() {
    let dir = scratch_dir("raw-es-timing");
    let mut failures = Vec::new();
    for (name, codec, format, rate) in [
        ("x25.h264", "libx264", "h264", "25"),
        ("x30.h264", "libx264", "h264", "30000/1001"),
        ("x25.hevc", "libx265", "hevc", "25"),
        ("x30.hevc", "libx265", "hevc", "30000/1001"),
    ] {
        let path = dir.join(name);
        let out = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=size=176x144:rate={rate}:duration=2"))
            .args(["-c:v", codec, "-bf", "2", "-g", "25", "-x265-params", "log-level=error", "-f", format])
            .arg(&path)
            .output()
            .expect("ffmpeg must be on PATH");
        assert!(out.status.success(), "{name}: ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
        if let Err(e) = compare(&path, &format!("generated {name}"), format, RAW_VIDEO) {
            failures.push(e);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// PES payloads with the PTS each PES carries.
#[test]
fn pva() {
    check_inventory("pva", &["pva"], Mode { unparsed: true, ..CONTAINER });
}

/// adpcm.mak: one packet per block (at most 2048 bytes), timed in
/// samples.
#[test]
fn voc() {
    check_inventory("voc", &["voc"], CONTAINER);
}

/// caf.mak's PCM input and the previously covered AAC/Opus fixtures:
/// complete packet-table framing and timing, not only the first 3 PTS.
#[test]
fn caf() {
    check_inventory("caf", &["caf"], CONTAINER);
    for rel in ["caf/aac.caf", "caf/opus.caf"] {
        compare(&suite_path(rel), rel, "caf", CONTAINER).unwrap();
    }
}

/// cbs.mak, av1.mak, vpx.mak: frame headers carry size and pts; key
/// flags are those of FFmpeg's VP8 / VP9 / AV1 parsers, which seeking
/// depends on.
#[test]
fn ivf() {
    check_inventory("ivf", &["ivf"], Mode { keys: true, ..CONTAINER });
}

/// NUT: FATE muxes its NUT inputs (lavf.mak); mux one here with FFmpeg
/// and compare demuxing it.
#[test]
fn nut() {
    let path = std::env::temp_dir().join(format!("demux-misc-nut-{}.nut", std::process::id()));
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=1000:duration=0.5",
            "-f", "lavfi", "-i", "testsrc=duration=0.5:size=64x64:rate=10",
            "-c:a", "mp2", "-c:v", "mpeg2video", "-shortest",
        ])
        .arg(&path)
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(out.status.success(), "ffmpeg mux failed: {}", String::from_utf8_lossy(&out.stderr));
    // NUT carries PTS only. Disable libavformat's decoder-dependent
    // DTS fill-in, as for the unparsed PVA contract.
    let result = compare(&path, "generated NUT", "nut", Mode { unparsed: true, ..CONTAINER });
    let _ = std::fs::remove_file(&path);
    result.unwrap();
}

/// The inventory reads the makefiles' lists and macros: hevc.mak's
/// conformance names, vpx.mak's VP8 suite, cbs.mak's AV1 vectors.
#[test]
fn inventory_expands_fate_macros() {
    let inputs = fate_inputs();
    for rel in [
        "ac3/monsters_inc_5.1_448_small.ac3",
        "eac3/the_great_wall_7.1.eac3",
        "hevc-conformance/AMP_A_Samsung_4.bit",
        "hevc-conformance/Main_422_10_A_RExt_Sony_1.bin",
        "vp8-test-vectors-r1/vp80-00-comprehensive-017.ivf",
        "av1-test-vectors/av1-1-b8-02-allintra.ivf",
        "h264-conformance/FRext/FRExt_MMCO4_Sony_B.264",
    ] {
        assert!(inputs.iter().any(|i| i == rel), "{rel} missing from the FATE inventory");
    }
}

/// probe.mak has two extensionless MPEG-PS regressions. These inputs
/// contract probing only, unlike the complete packet fixtures above.
#[test]
fn extensionless_program_stream_probes() {
    let text = std::fs::read_to_string(ffmpeg_src().join("tests/fate/probe.mak")).unwrap();
    let names: Vec<&str> = text.lines().filter_map(|line| {
        let (name, value) = line.split_once(':')?;
        let (_, format) = value.split_once('=')?;
        (value.trim_start().starts_with("REF") && format.trim() == "mpeg")
            .then(|| name.trim().strip_prefix("fate-probe-format-")).flatten()
    }).collect();
    assert!(!names.is_empty(), "probe.mak: MPEG-PS inventory missing");
    let ctx = codecs::context();
    for name in names {
        let path = suite_path(&format!("probe-format/{name}"));
        assert_eq!(probe(&ctx, &path).as_deref(), Some("mpeg"), "{name}: production probe");
        assert_eq!(ffprobe_format(&path), "mpeg", "{name}: FFmpeg probe");
    }
}
