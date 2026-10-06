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
//! sub2video also emits heartbeat frames at packet times (a blank canvas
//! once the shown subtitle has ended, otherwise a repeat of the shown
//! canvas). [`ffmpeg_reference`] picks the canvas of every subtitle out of
//! that stream and checks that every other frame is such a heartbeat.
//!
//! Decoders here follow `oxideav-sub-image`'s model: one RGBA canvas
//! `VideoFrame` per displayed state. [`timeline`] turns FFmpeg's subtitle
//! list into the frames such a decoder must emit: each subtitle's canvas at
//! its start, and a blank canvas at its end when it ends before the next
//! subtitle starts.

#![allow(dead_code)]

use std::io::{BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use oxideav_core::{Frame, TimeBase};

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

/// One displayed state: the canvas shown from `at_us` on.
#[derive(Clone)]
pub struct Shown {
    pub at_us: i64,
    pub canvas: Vec<u8>,
}

fn run(program: &str, args: &[&str]) -> Vec<u8> {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {program}: {e}"));
    assert!(out.status.success(), "{program} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// Every `AVSubtitle` FFmpeg's decoder returns for stream `s:nth`.
pub fn ffprobe_subtitles(path: &Path, nth: usize) -> Vec<FfSubtitle> {
    let stream = format!("s:{nth}");
    let text = run(
        "ffprobe",
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

fn sub2video_args(path: &Path, nth: usize, format: &str) -> Vec<String> {
    // sub2video's canvas is AV_PIX_FMT_RGB32 (bgra in memory on little
    // endian); asking for bgra keeps FFmpeg from converting it.
    [
        "-v",
        "error",
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
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// `(width, height, pts_us of every frame)` of FFmpeg's sub2video output.
fn sub2video_times(path: &Path, nth: usize) -> (usize, usize, Vec<i64>) {
    let args = sub2video_args(path, nth, "framecrc");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let text = String::from_utf8(run("ffmpeg", &args)).unwrap();
    let mut dims = None;
    let mut times = Vec::new();
    for line in text.lines() {
        if let Some(d) = line.strip_prefix("#dimensions 0:") {
            let (w, h) = d.trim().split_once('x').expect("dimensions");
            dims = Some((w.parse().unwrap(), h.parse().unwrap()));
        } else if !line.starts_with('#') {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            assert_eq!(f.len(), 6, "framecrc line {line:?}");
            times.push(f[2].parse().unwrap());
        }
    }
    let (w, h) = dims.expect("framecrc dimensions");
    (w, h, times)
}

/// Which sub2video frames are subtitle canvases: the frame of a subtitle
/// is the last one of its start time, after the heartbeats the same
/// packet produced. Returns one frame index per subtitle.
fn pick_subtitle_frames(times: &[i64], subs: &[FfSubtitle]) -> Vec<usize> {
    let mut picks = Vec::with_capacity(subs.len());
    let mut next = 0;
    let mut i = 0;
    while i < times.len() {
        let t = times[i];
        let mut run_end = i;
        while run_end < times.len() && times[run_end] == t {
            run_end += 1;
        }
        let mut k = 0;
        while next + k < subs.len() && subs[next + k].start_us() == t {
            k += 1;
        }
        assert!(
            k <= run_end - i,
            "sub2video shows {} frames at {t} µs, ffprobe lists {k} subtitles there",
            run_end - i
        );
        picks.extend(run_end - k..run_end);
        next += k;
        assert!(
            next == subs.len() || subs[next].start_us() > t,
            "subtitle #{next} at {} µs has no sub2video frame",
            subs[next].start_us()
        );
        i = run_end;
    }
    assert_eq!(next, subs.len(), "sub2video showed {} of {} subtitles", next, subs.len());
    picks
}

/// FFmpeg's decode of subtitle stream `s:nth`: every `AVSubtitle` with the
/// canvas sub2video paints for it.
pub fn ffmpeg_reference(path: &Path, nth: usize) -> Reference {
    let subs = ffprobe_subtitles(path, nth);
    let (width, height, times) = sub2video_times(path, nth);
    let picks = pick_subtitle_frames(&times, &subs);

    let args = sub2video_args(path, nth, "rawvideo");
    let mut child = Command::new("ffmpeg")
        .args(&args)
        .stdout(Stdio::piped())
        .spawn()
        .expect("run ffmpeg");
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let size = width * height * 4;
    let mut canvases = Vec::with_capacity(picks.len());
    let mut shown = vec![0u8; size];
    let mut frame = vec![0u8; size];
    let mut pick = picks.iter().peekable();
    for (i, &t) in times.iter().enumerate() {
        out.read_exact(&mut frame).unwrap_or_else(|e| panic!("sub2video frame {i}: {e}"));
        bgra_to_rgba(&mut frame);
        if pick.peek() == Some(&&i) {
            pick.next();
            canvases.push(frame.clone());
            shown.copy_from_slice(&frame);
        } else {
            // A heartbeat: a blank canvas or the shown one again.
            let blank = frame.iter().all(|&b| b == 0);
            assert!(blank || frame == shown, "sub2video frame {i} at {t} µs is neither a subtitle nor a heartbeat");
            if blank {
                shown.fill(0);
            }
        }
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

/// The frames a canvas decoder must emit for FFmpeg's subtitles: each
/// subtitle's canvas at its start, then a blank canvas at its end when it
/// ends before the next subtitle starts (or is the last one).
pub fn timeline(reference: &Reference) -> Vec<Shown> {
    let blank = vec![0u8; reference.width * reference.height * 4];
    let mut out = Vec::new();
    for (i, cue) in reference.cues.iter().enumerate() {
        out.push(Shown { at_us: cue.sub.start_us(), canvas: cue.canvas.clone() });
        if let Some(end) = cue.sub.end_us() {
            let next = reference.cues.get(i + 1).map(|c| c.sub.start_us());
            if next.is_none_or(|n| end < n) {
                out.push(Shown { at_us: end, canvas: blank.clone() });
            }
        }
    }
    out
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

/// Every decoded frame as a shown state: its pts in microseconds and its
/// RGBA canvas packed to `width * 4` bytes per row. Panics when a frame is
/// not a `width x height` RGBA canvas.
pub fn shown_states(frames: &[Frame], time_base: TimeBase, width: usize, height: usize) -> Vec<Shown> {
    frames
        .iter()
        .enumerate()
        .map(|(i, frame)| {
            let Frame::Video(v) = frame else { panic!("frame {i} is not a bitmap canvas") };
            let plane = &v.planes[0];
            assert!(plane.stride >= width * 4, "frame {i}: stride {} for a {width}-pixel canvas", plane.stride);
            assert_eq!(plane.data.len() / plane.stride, height, "frame {i}: canvas height");
            let mut canvas = Vec::with_capacity(width * height * 4);
            for row in plane.data.chunks(plane.stride).take(height) {
                canvas.extend_from_slice(&row[..width * 4]);
            }
            Shown { at_us: to_us(v.pts.expect("frame pts"), time_base), canvas }
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

/// Compares decoded states with the expected ones: same count, same times,
/// canvases agreeing under `rule`. Returns every difference found.
pub fn timeline_diffs(want: &[Shown], got: &[Shown], width: usize, rule: Match) -> Vec<String> {
    let mut diffs = Vec::new();
    if want.len() != got.len() {
        diffs.push(format!("{} states shown, FFmpeg shows {}", got.len(), want.len()));
    }
    for (i, (w, g)) in want.iter().zip(got).enumerate() {
        if w.at_us != g.at_us {
            diffs.push(format!("state {i}: shown at {} µs, FFmpeg at {} µs", g.at_us, w.at_us));
        }
        if let Some(d) = canvas_diff(&w.canvas, &g.canvas, width, rule) {
            diffs.push(format!("state {i} at {} µs: {d}", w.at_us));
        }
    }
    diffs
}
