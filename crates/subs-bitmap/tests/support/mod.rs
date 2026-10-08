//! FFmpeg oracle for bitmap subtitle streams, shared by the test files.
//!
//! FFmpeg decodes bitmap subtitle packets into `AVSubtitle`s: a start
//! (`pts` + `start_display_time`), an end (`end_display_time`, where
//! `UINT32_MAX` means "until the next subtitle") and palette-indexed
//! rectangles. Two FFmpeg views together pin all of it down:
//!
//! * `ffprobe -show_frames` lists every `AVSubtitle` the decoder returns,
//!   with its times and rectangle count;
//! * `ffmpeg`'s sub2video paints each `AVSubtitle` on a canvas the size the
//!   decoder reports, copying every rectangle's palette-resolved pixels to
//!   its position (`fftools/ffmpeg_filter.c: sub2video_copy_rect`). That
//!   canvas is the picture a player overlays: positions, bitmaps and
//!   palettes are all in it, transparent pixels included.
//!
//! sub2video also emits heartbeat frames: ahead of every packet FFmpeg's
//! demuxer reads, a blank canvas once the shown subtitle has ended,
//! otherwise a repeat of the shown canvas (a blank canvas while nothing is
//! shown). FFmpeg 2da55bf sends that heartbeat for packets of streams the
//! run does not use too, which libavformat still returns when its probe
//! buffered them, so the run's own `-debug_ts` report says which packets
//! were read. [`ffmpeg_reference`] picks the canvas of every subtitle out
//! of that stream and checks that every other frame is such a heartbeat.
//!
//! Decoders here follow `oxideav-sub-image`'s model: one RGBA canvas
//! `VideoFrame` per `AVSubtitle`, at its start, with a display duration when
//! the subtitle has an end. [`reference_cues`] and [`decoded_cues`] put both
//! sides in that form and [`cue_diffs`] compares them.

#![allow(dead_code)]

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use oxideav_core::{Error, Frame, MediaType, PROBE_SCORE_EXTENSION, ProbeData, RuntimeContext, StreamInfo, TimeBase};
use refcheck::Registrar;

/// One `AVSubtitle` as `ffprobe -show_frames` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FfSubtitle {
    /// `AVSubtitle.pts`, microseconds.
    pub pts_us: i64,
    pub start_display_ms: u32,
    pub end_display_ms: u32,
    pub num_rects: u32,
}

impl FfSubtitle {
    pub fn start_us(&self) -> i64 {
        self.pts_us + i64::from(self.start_display_ms) * 1000
    }

    /// `None` for `UINT32_MAX`: shown until the next subtitle.
    pub fn end_us(&self) -> Option<i64> {
        (self.end_display_ms != u32::MAX).then(|| self.pts_us + i64::from(self.end_display_ms) * 1000)
    }
}

/// One FFmpeg subtitle with the canvas sub2video painted for it.
pub struct RefCue {
    pub sub: FfSubtitle,
    /// RGBA, `width * 4` bytes per row.
    pub canvas: Vec<u8>,
}

/// Everything FFmpeg decodes from one subtitle stream.
pub struct Reference {
    pub width: usize,
    pub height: usize,
    pub cues: Vec<RefCue>,
}

/// One subtitle as a player shows it: the canvas from `start_us`, for
/// `duration_us` when it has an end (else until the next one).
#[derive(Clone)]
pub struct Cue {
    pub start_us: i64,
    pub duration_us: Option<i64>,
    pub canvas: Vec<u8>,
}

fn run(program: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> Vec<u8> {
    run_logged(program, args).0
}

/// [`run`], with what the program wrote to stderr.
fn run_logged(program: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> (Vec<u8>, String) {
    let program = program.as_ref();
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {}: {e}", program.to_string_lossy()));
    assert!(out.status.success(), "{} {args:?}: {}", program.to_string_lossy(), String::from_utf8_lossy(&out.stderr));
    (out.stdout, String::from_utf8_lossy(&out.stderr).into_owned())
}

/// Every `AVSubtitle` FFmpeg's decoder returns for stream `s:nth`.
pub fn ffprobe_subtitles(path: &Path, nth: usize) -> Vec<FfSubtitle> {
    let stream = format!("s:{nth}");
    let text = run(
        refcheck::pinned_ffprobe(),
        &[
            "-v",
            "error",
            "-select_streams",
            &stream,
            "-show_frames",
            "-show_entries",
            "subtitle=pts,start_display_time,end_display_time,num_rects",
            "-of",
            "csv=p=0",
            path.to_str().unwrap(),
        ],
    );
    String::from_utf8(text)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.trim().split(',').collect();
            assert_eq!(f.len(), 4, "ffprobe subtitle line {line:?}");
            let num = |s: &str| s.parse::<i64>().unwrap_or_else(|_| panic!("ffprobe field {s:?} in {line:?}"));
            FfSubtitle {
                pts_us: num(f[0]),
                start_display_ms: num(f[1]) as u32,
                end_display_ms: num(f[2]) as u32,
                num_rects: num(f[3]) as u32,
            }
        })
        .collect()
}

/// One packet of a stream as FFmpeg's demuxer returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FfPacket {
    /// In the stream's time base; `None` for `AV_NOPTS_VALUE`.
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: Option<i64>,
    pub size: usize,
    /// Lowercase hex MD5 of the packet data.
    pub md5: String,
}

/// The time base of stream `s:nth`.
pub fn ffprobe_time_base(path: &Path, nth: usize) -> TimeBase {
    let stream = format!("s:{nth}");
    let text = run(
        refcheck::pinned_ffprobe(),
        &["-v", "error", "-select_streams", &stream, "-show_entries", "stream=time_base", "-of", "csv=p=0", path.to_str().unwrap()],
    );
    let text = String::from_utf8(text).unwrap();
    // MPEG-TS lists the stream again under its program: take the first.
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).expect("time_base");
    let (num, den) = line.split_once('/').expect("time_base");
    TimeBase::new(num.parse().unwrap(), den.parse().unwrap())
}

/// Every packet of stream `s:nth`, in demux order.
pub fn ffprobe_packets(path: &Path, nth: usize) -> Vec<FfPacket> {
    let stream = format!("s:{nth}");
    let text = run(
        refcheck::pinned_ffprobe(),
        &[
            "-v",
            "error",
            "-select_streams",
            &stream,
            "-show_entries",
            "packet=pts,dts,duration,size,data_hash:packet_side_data=",
            "-show_data_hash",
            "md5",
            "-of",
            "compact=p=0",
            path.to_str().unwrap(),
        ],
    );
    String::from_utf8(text)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            // MPEG-TS side-data sections add empty CSV fields even when
            // their entries are excluded. Named fields avoid positional
            // ambiguity without ignoring any requested packet metadata.
            let field = |name| {
                line.split('|')
                    .filter_map(|part| part.split_once('='))
                    .find_map(|(key, value)| (key == name).then_some(value))
                    .unwrap_or_else(|| panic!("missing {name} in ffprobe packet {line:?}"))
            };
            let opt = |name| {
                let value = field(name);
                (value != "N/A").then(|| value.parse::<i64>().unwrap_or_else(|_| panic!("{value:?} in {line:?}")))
            };
            FfPacket {
                pts: opt("pts"),
                dts: opt("dts"),
                duration: opt("duration"),
                size: field("size").parse().unwrap(),
                md5: field("data_hash").strip_prefix("MD5:").expect("md5").to_string(),
            }
        })
        .collect()
}

/// The sub2video run's arguments; with `report_packets`, it also logs
/// every packet its demuxer reads (`-debug_ts`, at the info level).
fn sub2video_args(path: &Path, nth: usize, format: &str, report_packets: bool) -> Vec<String> {
    let log: &[&str] = if report_packets { &["-v", "info", "-debug_ts"] } else { &["-v", "error"] };
    // sub2video's canvas is AV_PIX_FMT_RGB32 (bgra in memory on little
    // endian); asking for bgra keeps FFmpeg from converting it.
    log.iter()
        .copied()
        .chain([
            "-copyts",
            "-i",
            path.to_str().unwrap(),
            "-filter_complex",
            &format!("[0:s:{nth}]null[canvas]"),
            "-map",
            "[canvas]",
            "-fps_mode",
            "passthrough",
            "-pix_fmt",
            "bgra",
            "-c:v",
            "rawvideo",
            "-f",
            format,
            "-",
        ])
        .map(str::to_string)
        .collect()
}

/// What the sub2video run produced and read.
struct Sub2VideoRun {
    width: usize,
    height: usize,
    frames: usize,
    /// Every packet its demuxer read, in order: the stream index and the
    /// pts as the heartbeat gets it (`fftools/ffmpeg_demux.c:
    /// input_packet_process` logs it after its timestamp fixes).
    read: Vec<(usize, Option<i64>)>,
}

/// FFmpeg's sub2video output for stream `s:nth`, with its packet report.
fn sub2video_run(path: &Path, nth: usize) -> Sub2VideoRun {
    let args = sub2video_args(path, nth, "framecrc", true);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (out, log) = run_logged(refcheck::pinned_ffmpeg(), &args);
    let text = String::from_utf8(out).unwrap();
    let mut dims = None;
    let mut frames = 0;
    for line in text.lines() {
        if let Some(d) = line.strip_prefix("#dimensions 0:") {
            let (w, h) = d.trim().split_once('x').expect("dimensions");
            dims = Some((w.parse().unwrap(), h.parse().unwrap()));
        } else if !line.starts_with('#') {
            frames += 1;
        }
    }
    let (width, height) = dims.expect("framecrc dimensions");
    let field = |line: &str, name: &str| -> String {
        let at = line.find(name).unwrap_or_else(|| panic!("no {name} in {line:?}")) + name.len();
        line[at..].split_whitespace().next().unwrap_or_default().to_string()
    };
    let read = log
        .lines()
        .filter(|line| line.contains("demuxer+ffmpeg -> "))
        .map(|line| {
            let index = field(line, "ist_index:");
            let stream = index.split_once(':').and_then(|(_, s)| s.parse().ok()).unwrap_or_else(|| panic!("ist_index in {line:?}"));
            let pts = field(line, "pkt_pts:");
            (stream, (pts != "NOPTS").then(|| pts.parse().unwrap_or_else(|_| panic!("pkt_pts in {line:?}"))))
        })
        .collect();
    Sub2VideoRun { width, height, frames, read }
}

/// Every stream's index and time base, and the index of stream `s:nth`.
fn stream_time_bases(path: &Path, nth: usize) -> (Vec<(usize, TimeBase)>, usize) {
    let rows = |select: &[&str]| -> Vec<(usize, TimeBase)> {
        let mut args = vec!["-v", "error"];
        args.extend_from_slice(select);
        args.extend(["-show_entries", "stream=index,time_base", "-of", "csv=p=0", path.to_str().unwrap()]);
        String::from_utf8(run(refcheck::pinned_ffprobe(), &args))
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| {
                // An MPEG-2 video stream's line ends in an empty field (its
                // side data list), so only the first two fields count.
                let mut fields = line.trim().split(',');
                let (Some(index), Some(tb)) = (fields.next(), fields.next()) else { panic!("ffprobe stream line {line:?}") };
                let (num, den) = tb.split_once('/').unwrap_or_else(|| panic!("time base {tb:?}"));
                (index.parse().unwrap(), TimeBase::new(num.parse().unwrap(), den.parse().unwrap()))
            })
            .collect()
    };
    let stream = format!("s:{nth}");
    let selected = rows(&["-select_streams", &stream]);
    assert_eq!(selected.len(), 1, "{stream} of {}", path.display());
    (rows(&[]), selected[0].0)
}

/// What ffprobe sees on stream `s:nth`, in processing order: each packet's
/// pts, followed by the subtitles decoding it returned.
#[derive(Debug)]
enum Probed {
    Packet(Option<i64>),
    Subtitle(FfSubtitle),
}

fn ffprobe_events(path: &Path, nth: usize) -> Vec<Probed> {
    let stream = format!("s:{nth}");
    let text = run(
        refcheck::pinned_ffprobe(),
        &[
            "-v",
            "error",
            "-select_streams",
            &stream,
            "-show_packets",
            "-show_frames",
            "-show_entries",
            "packet=pts:subtitle=pts,start_display_time,end_display_time,num_rects",
            "-of",
            "csv",
            path.to_str().unwrap(),
        ],
    );
    String::from_utf8(text)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.trim().split(',').collect();
            let num = |s: &str| s.parse::<i64>().unwrap_or_else(|_| panic!("ffprobe field {s:?} in {line:?}"));
            match f[0] {
                "packet" => Probed::Packet((f[1] != "N/A").then(|| num(f[1]))),
                "subtitle" => Probed::Subtitle(FfSubtitle {
                    pts_us: num(f[1]),
                    start_display_ms: num(f[2]) as u32,
                    end_display_ms: num(f[3]) as u32,
                    num_rects: num(f[4]) as u32,
                }),
                _ => panic!("ffprobe line {line:?}"),
            }
        })
        .collect()
}

/// What sub2video receives, in order.
#[derive(Debug)]
enum Event {
    /// The heartbeat ahead of a packet the demuxer read: its pts in
    /// microseconds (`AV_TIME_BASE_Q`, sub2video's time base).
    Heartbeat(i64),
    /// A subtitle the decoder returned.
    Subtitle(FfSubtitle),
}

/// The run's packets (`read`, each stream's time base in `time_bases`) with
/// the subtitles decoding stream `stream` returned (`probed`): a heartbeat
/// ahead of every packet that has a pts (`fftools/ffmpeg_demux.c:
/// demux_send`), then, for a packet of `stream`, the subtitles decoding it
/// returned; last, those the decoder returns at the end. The run must read
/// the packets ffprobe lists for `stream`, with the same pts.
fn sub2video_events(read: &[(usize, Option<i64>)], time_bases: &[(usize, TimeBase)], stream: usize, probed: Vec<Probed>) -> Vec<Event> {
    let time_base = |s: usize| time_bases.iter().find(|(i, _)| *i == s).map(|(_, tb)| *tb).unwrap_or_else(|| panic!("no stream {s}"));
    let mut probed = probed.into_iter().peekable();
    let mut events = Vec::new();
    for (n, &(s, pts)) in read.iter().enumerate() {
        if let Some(pts) = pts {
            events.push(Event::Heartbeat(to_us(pts, time_base(s))));
        }
        if s != stream {
            continue;
        }
        match probed.next() {
            Some(Probed::Packet(p)) => assert_eq!(p, pts, "packet {n} the run read: ffprobe's pts differs"),
            other => panic!("packet {n} the run read is not ffprobe's next packet of stream {stream}: {other:?}"),
        }
        while let Some(Probed::Subtitle(_)) = probed.peek() {
            let Some(Probed::Subtitle(sub)) = probed.next() else { unreachable!() };
            events.push(Event::Subtitle(sub));
        }
    }
    for rest in probed {
        match rest {
            Probed::Subtitle(sub) => events.push(Event::Subtitle(sub)),
            Probed::Packet(pts) => panic!("the run did not read ffprobe's packet at pts {pts:?} of stream {stream}"),
        }
    }
    events
}

/// One frame sub2video pushes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sub2VideoFrame {
    /// `sub2video_update(NULL)`: a blank canvas.
    Blank,
    /// `sub2video_push_ref` from a heartbeat: the current canvas again.
    Repeat,
    /// `sub2video_update(sub)` for the n-th subtitle.
    Subtitle(usize),
}

/// The frames sub2video pushes for these events (`fftools/ffmpeg_filter.c:
/// sub2video_heartbeat` / `sub2video_update`). The output timestamps are
/// not modelled: the muxer clamps them to be monotonic, so frames are
/// matched by position.
fn sub2video_frames(events: &[Event]) -> Vec<Sub2VideoFrame> {
    let mut out = Vec::new();
    let (mut last_pts, mut end_pts, mut initialize) = (i64::MIN, i64::MIN, true);
    let mut subs = 0;
    for event in events {
        match event {
            Event::Heartbeat(us) => {
                let pts2 = us - 1;
                if pts2 <= last_pts {
                    continue;
                }
                if pts2 >= end_pts || initialize {
                    last_pts = if initialize { pts2 + 1 } else { end_pts };
                    end_pts = i64::MAX;
                    initialize = false;
                    out.push(Sub2VideoFrame::Blank);
                } else {
                    last_pts = pts2;
                    out.push(Sub2VideoFrame::Repeat);
                }
            }
            Event::Subtitle(sub) => {
                last_pts = sub.start_us();
                end_pts = sub.pts_us + i64::from(sub.end_display_ms) * 1000;
                initialize = false;
                out.push(Sub2VideoFrame::Subtitle(subs));
                subs += 1;
            }
        }
    }
    if end_pts < i64::MAX {
        out.push(Sub2VideoFrame::Blank);
    }
    out
}

/// FFmpeg's decode of subtitle stream `s:nth`: every `AVSubtitle` with the
/// canvas sub2video paints for it.
pub fn ffmpeg_reference(path: &Path, nth: usize) -> Reference {
    let probed = ffprobe_events(path, nth);
    let subs: Vec<FfSubtitle> = probed
        .iter()
        .filter_map(|e| match e {
            Probed::Subtitle(s) => Some(s.clone()),
            Probed::Packet(_) => None,
        })
        .collect();
    let run = sub2video_run(path, nth);
    let (time_bases, stream) = stream_time_bases(path, nth);
    let expected = sub2video_frames(&sub2video_events(&run.read, &time_bases, stream, probed));
    let (width, height) = (run.width, run.height);
    assert_eq!(run.frames, expected.len(), "sub2video frames: {expected:?}");

    let args = sub2video_args(path, nth, "rawvideo", false);
    let mut child = Command::new(refcheck::pinned_ffmpeg()).args(&args).stdout(Stdio::piped()).spawn().expect("run ffmpeg");
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let size = width * height * 4;
    let mut canvases = Vec::with_capacity(subs.len());
    let mut shown = vec![0u8; size];
    let mut frame = vec![0u8; size];
    for (i, kind) in expected.iter().enumerate() {
        out.read_exact(&mut frame).unwrap_or_else(|e| panic!("sub2video frame {i}: {e}"));
        bgra_to_rgba(&mut frame);
        match kind {
            Sub2VideoFrame::Subtitle(_) => canvases.push(frame.clone()),
            Sub2VideoFrame::Blank => assert!(frame.iter().all(|&b| b == 0), "sub2video frame {i} is not blank"),
            Sub2VideoFrame::Repeat => assert!(frame == shown, "sub2video frame {i} does not repeat the shown canvas"),
        }
        shown.copy_from_slice(&frame);
    }
    let mut rest = Vec::new();
    out.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty(), "sub2video wrote {} bytes past its last frame", rest.len());
    assert!(child.wait().unwrap().success(), "ffmpeg sub2video failed");
    Reference {
        width,
        height,
        cues: subs.into_iter().zip(canvases).map(|(sub, canvas)| RefCue { sub, canvas }).collect(),
    }
}

fn bgra_to_rgba(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
}

/// FFmpeg's subtitles as cues.
pub fn reference_cues(reference: &Reference) -> Vec<Cue> {
    reference
        .cues
        .iter()
        .map(|c| Cue {
            start_us: c.sub.start_us(),
            duration_us: c.sub.end_us().map(|e| (e - c.sub.start_us()).max(0)),
            canvas: c.canvas.clone(),
        })
        .collect()
}

/// `pts` in `time_base` to microseconds, rounding like FFmpeg's
/// `av_rescale_q` (to nearest, halves away from zero).
pub fn to_us(pts: i64, time_base: TimeBase) -> i64 {
    let r = time_base.as_rational();
    let num = i128::from(pts) * i128::from(r.num) * 1_000_000;
    let den = i128::from(r.den);
    let half = den / 2;
    (if num >= 0 { (num + half) / den } else { (num - half) / den }) as i64
}

/// Every decoded frame as a cue: its pts and display duration in
/// microseconds and its RGBA canvas packed to `width * 4` bytes per row.
/// Panics when a frame is not a `width x height` RGBA canvas.
pub fn decoded_cues(frames: &[Frame], time_base: TimeBase, width: usize, height: usize) -> Vec<Cue> {
    frames
        .iter()
        .enumerate()
        .map(|(i, frame)| {
            let Frame::Video(v) = frame else { panic!("frame {i} is not a bitmap canvas") };
            let planes = v.image_planes();
            assert_eq!(planes.len(), 1, "frame {i}: one RGBA plane");
            let plane = &planes[0];
            assert_eq!(plane.stride, width * 4, "frame {i}: stride for a {width}-pixel canvas");
            assert_eq!(plane.data.len(), width * height * 4, "frame {i}: {width}x{height} canvas");
            Cue {
                start_us: to_us(v.pts.expect("frame pts"), time_base),
                duration_us: v.display_duration().map(|d| d.as_micros() as i64),
                canvas: plane.data.clone(),
            }
        })
        .collect()
}

/// How two canvases must agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    /// Byte for byte, colour of fully transparent pixels included (what
    /// sub2video writes).
    Exact,
    /// Same alpha everywhere, same colour wherever alpha is not 0: what a
    /// straight-alpha overlay shows.
    Visible,
}

/// Differences between two canvases, `None` when they agree under `rule`.
pub fn canvas_diff(want: &[u8], got: &[u8], width: usize, rule: Match) -> Option<String> {
    assert_eq!(want.len(), got.len(), "canvas sizes differ");
    let mut count = 0usize;
    let mut bbox = (usize::MAX, usize::MAX, 0usize, 0usize);
    let mut samples = Vec::new();
    for (i, (w, g)) in want.chunks_exact(4).zip(got.chunks_exact(4)).enumerate() {
        let same = match rule {
            Match::Exact => w == g,
            Match::Visible => w[3] == g[3] && (w[3] == 0 || w == g),
        };
        if !same {
            let (x, y) = (i % width, i / width);
            count += 1;
            bbox = (bbox.0.min(x), bbox.1.min(y), bbox.2.max(x), bbox.3.max(y));
            if samples.len() < 6 {
                samples.push(format!("({x},{y}) want {w:?} got {g:?}"));
            }
        }
    }
    (count > 0).then(|| {
        format!(
            "{count} pixels differ in x {}..={} y {}..={}: {}",
            bbox.0,
            bbox.2,
            bbox.1,
            bbox.3,
            samples.join(", ")
        )
    })
}

/// Compares decoded cues with FFmpeg's: same count, same starts and
/// durations, canvases agreeing under `rule`. Returns every difference.
pub fn cue_diffs(want: &[Cue], got: &[Cue], width: usize, rule: Match) -> Vec<String> {
    let mut diffs = Vec::new();
    if want.len() != got.len() {
        diffs.push(format!("{} cues decoded, FFmpeg decodes {}", got.len(), want.len()));
    }
    for (i, (w, g)) in want.iter().zip(got).enumerate() {
        if (w.start_us, w.duration_us) != (g.start_us, g.duration_us) {
            diffs.push(format!(
                "cue {i}: shown at {} µs for {:?} µs, FFmpeg at {} µs for {:?} µs",
                g.start_us, g.duration_us, w.start_us, w.duration_us
            ));
        }
        if let Some(d) = canvas_diff(&w.canvas, &g.canvas, width, rule) {
            diffs.push(format!("cue {i} at {} µs: {d}", w.start_us));
        }
    }
    diffs
}

/// One subtitle stream decoded through OxideAV registries.
pub struct Decoded {
    pub stream: StreamInfo,
    pub frames: Vec<Frame>,
    /// `send_packet` errors, as (packet index, message): FFmpeg logs a
    /// decode error and goes on, and so does this.
    pub errors: Vec<(usize, String)>,
}

/// Opens `path` with what `registrars` install (probing like
/// `refcheck::decode`), picks subtitle stream `nth` and decodes all of it.
pub fn decode_subtitles(path: &Path, registrars: &[Registrar], nth: usize) -> Decoded {
    let mut ctx = RuntimeContext::new();
    for register in registrars {
        register(&mut ctx);
    }
    let mut head = vec![0; 256 * 1024];
    let n = File::open(path).and_then(|mut f| f.read(&mut head)).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let probe = ProbeData { buf: &head[..n], ext: ext.as_deref() };
    let candidates = ctx.containers.probe_candidates(&probe);
    let by_extension = ext.as_deref().and_then(|e| ctx.containers.container_for_extension(e));
    let format = match (candidates.first(), by_extension) {
        (Some(c), _) if c.score >= PROBE_SCORE_EXTENSION => c.name.to_string(),
        (_, Some(name)) => name.to_string(),
        _ => panic!("{}: no container claims it", path.display()),
    };
    let file = File::open(path).unwrap();
    let mut demuxer = ctx.containers.open_demuxer(&format, Box::new(file), &ctx.codecs).unwrap_or_else(|e| panic!("open {format}: {e}"));
    let stream = demuxer
        .streams()
        .iter()
        .filter(|s| s.params.media_type == MediaType::Subtitle)
        .nth(nth)
        .unwrap_or_else(|| panic!("{}: no subtitle stream #{nth}", path.display()))
        .clone();
    let mut decoder = ctx.codecs.first_decoder(&stream.params).unwrap_or_else(|e| panic!("no decoder for {}: {e}", stream.params.codec_id));
    let mut frames = Vec::new();
    let mut errors = Vec::new();
    let mut index = 0;
    loop {
        match demuxer.next_packet() {
            Ok(packet) if packet.stream_index == stream.index => {
                if let Err(e) = decoder.send_packet(&packet) {
                    errors.push((index, e.to_string()));
                }
                index += 1;
                loop {
                    match decoder.receive_frame() {
                        Ok(frame) => frames.push(frame),
                        Err(Error::NeedMore) | Err(Error::Eof) => break,
                        Err(e) => panic!("receive_frame: {e}"),
                    }
                }
            }
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("demux {}: {e}", path.display()),
        }
    }
    decoder.flush().unwrap();
    loop {
        match decoder.receive_frame() {
            Ok(frame) => frames.push(frame),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("receive_frame: {e}"),
        }
    }
    Decoded { stream, frames, errors }
}

/// `refcheck::system_ffmpeg -c copy` remux of subtitle stream `s:nth` of
/// `path` into a scratch file named `name`, with extra muxer arguments.
pub fn remux(path: &Path, nth: usize, name: &str, muxer: &[&str]) -> PathBuf {
    let scratch = option_env!("CARGO_TARGET_TMPDIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let out = scratch.join(name);
    let map = format!("0:s:{nth}");
    let status = Command::new(refcheck::system_ffmpeg())
        .args(["-v", "error", "-y", "-copyts", "-i"])
        .arg(path)
        .args(["-map", map.as_str(), "-c:s", "copy"])
        .args(muxer)
        .arg(&out)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg remux to {name}");
    out
}
