//! EIA-608 cues equal FFmpeg's `cc_dec` output, cue by cue.
//!
//! Buffered mode (FFmpeg's default): start and end as `ffmpeg -c:s srt`
//! writes them (milliseconds), the text as its SRT (tags and alignment
//! stripped, `\h` a no-break space) and the exact ASS event text of
//! `-c:s ass` (positions, italics, colors). Real time mode (`-real_time 1`,
//! what the player shows): every event of `-c:s ass`, its start
//! (centiseconds) and exact text, each up until the next. FFmpeg runs with
//! `-copyts`, so both sides keep the stream's own times.
//!
//! Inputs: the roll-up FATE sample and SCTE-20 FATE sample through our
//! extraction and timeline, the roll-up captions re-encoded into H.264 in
//! Matroska the same way, and FATE `sub/witch.scc` (pop-on, special
//! characters, italics) as FFmpeg's SCC demuxer packets it.

mod support;

use std::path::Path;
use std::process::Command;

use oxideav_core::Segment;
use subs_cc::eia608::{ticks_to_us, Caption, Cc608};
use support::{ffprobe_packets, generated, our_captions, rollup, scte20};

/// One FFmpeg cue: SRT times and text, and the ASS event text.
#[derive(Debug)]
struct FfCue {
    start_ms: i64,
    end_ms: i64,
    srt: String,
    ass: String,
}

fn run_ffmpeg(input: &[&str], format: &str, real_time: bool) -> String {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-copyts"])
        .args(if real_time { &["-real_time", "1"][..] } else { &[][..] })
        .args(input)
        .args(["-map", "0:s", "-c:s", format, "-f", format, "-"])
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg {input:?} {format}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn parse_srt_time(s: &str) -> i64 {
    let (hms, ms) = s.trim().split_once(',').unwrap();
    let mut parts = hms.split(':').map(|p| p.parse::<i64>().unwrap());
    let (h, m, sec) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
    ((h * 60 + m) * 60 + sec) * 1000 + ms.parse::<i64>().unwrap()
}

fn ffmpeg_cues(input: &[&str]) -> Vec<FfCue> {
    let srt = run_ffmpeg(input, "srt", false);
    let ass = run_ffmpeg(input, "ass", false);
    let mut cues = Vec::new();
    for block in srt.replace("\r\n", "\n").split("\n\n").filter(|b| !b.trim().is_empty()) {
        let mut lines = block.lines();
        lines.next(); // index
        let (start, end) = lines.next().unwrap().split_once(" --> ").unwrap();
        cues.push(FfCue {
            start_ms: parse_srt_time(start),
            end_ms: parse_srt_time(end),
            srt: lines.collect::<Vec<_>>().join("\n"),
            ass: String::new(),
        });
    }
    let events: Vec<String> = ass
        .lines()
        .filter_map(|l| l.strip_prefix("Dialogue: "))
        .map(|l| l.splitn(10, ',').nth(9).unwrap().to_string())
        .collect();
    assert_eq!(events.len(), cues.len(), "{input:?}: SRT and ASS cue counts");
    for (cue, event) in cues.iter_mut().zip(events) {
        cue.ass = event;
    }
    cues
}

/// FFmpeg's SRT text without tags, `{\an…}` overrides, with `\h` as the
/// no-break space it stands for.
fn srt_plain(srt: &str) -> String {
    let mut out = String::new();
    let mut chars = srt.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => while chars.next().is_some_and(|c| c != '>') {},
            '{' => while chars.next().is_some_and(|c| c != '}') {},
            '\\' if chars.peek() == Some(&'h') => {
                chars.next();
                out.push('\u{a0}');
            }
            _ => out.push(c),
        }
    }
    out
}

fn segments_plain(segments: &[Segment], out: &mut String) {
    for segment in segments {
        match segment {
            Segment::Text(t) => out.push_str(t),
            Segment::LineBreak => out.push('\n'),
            Segment::Italic(c) | Segment::Underline(c) | Segment::Bold(c) | Segment::Strike(c) => segments_plain(c, out),
            Segment::Color { children, .. } => segments_plain(children, out),
            other => panic!("unexpected segment {other:?}"),
        }
    }
}

/// av_rescale_q(us, AV_TIME_BASE_Q, {1, 1000}).
fn us_to_ms(us: i64) -> i64 {
    if us >= 0 { (us + 500) / 1000 } else { -((-us + 500) / 1000) }
}

fn assert_cues(what: &str, ours: &[Caption], theirs: &[FfCue]) {
    println!("{what}: FFmpeg {} cues, ours {}", theirs.len(), ours.len());
    assert!(!theirs.is_empty(), "{what}: FFmpeg decoded no cue");
    for (i, (a, b)) in ours.iter().zip(theirs).enumerate() {
        let start_ms = us_to_ms(a.start_us);
        let end_ms = start_ms + a.duration_ms.expect("buffered captions have an end");
        let ass: String = a.rects.iter().map(|r| r.ass.as_str()).collect();
        let mut plain = String::new();
        for rect in &a.rects {
            segments_plain(&rect.segments, &mut plain);
        }
        assert_eq!((start_ms, end_ms), (b.start_ms, b.end_ms), "{what}: cue {i} times ({:?})", b.srt);
        assert_eq!(ass, b.ass, "{what}: cue {i} ASS text");
        assert_eq!(plain, srt_plain(&b.srt), "{what}: cue {i} text against FFmpeg's SRT {:?}", b.srt);
    }
    assert_eq!(ours.len(), theirs.len(), "{what}: cue count");
}

/// FFmpeg's real time events: each ASS Dialogue's start (centiseconds) and
/// text; their end must be the ASS muxer's "until the next" (9:59:59.99).
fn ffmpeg_real_time_events(input: &[&str]) -> Vec<(i64, String)> {
    let ass = run_ffmpeg(input, "ass", true);
    let cs = |t: &str| {
        let mut parts = t.split([':', '.']).map(|p| p.parse::<i64>().unwrap());
        let (h, m, s, c) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
        ((h * 60 + m) * 60 + s) * 100 + c
    };
    ass.lines()
        .filter_map(|l| l.strip_prefix("Dialogue: "))
        .map(|l| {
            let fields: Vec<&str> = l.splitn(10, ',').collect();
            assert_eq!(fields[2], "9:59:59.99", "a real time event ends with the next: {l}");
            (cs(fields[1]), fields[9].to_string())
        })
        .collect()
}

/// av_rescale_q(us, AV_TIME_BASE_Q, {1, 100}).
fn us_to_cs(us: i64) -> i64 {
    if us >= 0 { (us + 5000) / 10000 } else { -((-us + 5000) / 10000) }
}

fn assert_real_time_events(what: &str, ours: &[Caption], theirs: &[(i64, String)]) {
    let ours: Vec<(i64, String)> = ours
        .iter()
        .flat_map(|c| {
            assert!(c.duration_ms.is_none(), "{what}: a real time event without an end");
            c.rects.iter().map(move |r| (us_to_cs(c.start_us), r.ass.clone()))
        })
        .collect();
    println!("{what}: FFmpeg {} real time events, ours {}", theirs.len(), ours.len());
    assert!(!theirs.is_empty(), "{what}: FFmpeg emitted no event");
    for (i, (a, b)) in ours.iter().zip(theirs).enumerate() {
        assert_eq!(a, b, "{what}: event {i} (start in centiseconds, ASS text)");
    }
    assert_eq!(ours.len(), theirs.len(), "{what}: event count");
}

/// Our captions for a video file: extraction and timeline as the player
/// runs them, then the decoder.
fn our_video_cues(path: &Path, mut decoder: Cc608) -> Vec<Caption> {
    let captions = our_captions(path);
    let (num, den) = captions.time_base;
    let mut cues = Vec::new();
    for (ts, triplets) in &captions.pictures {
        let bytes: Vec<u8> = triplets.iter().flatten().copied().collect();
        let pts_us = ts.and_then(|ts| ticks_to_us(ts, num, den));
        cues.extend(decoder.decode(&bytes, pts_us));
    }
    cues.extend(decoder.finish());
    cues
}

fn lavfi(path: &Path) -> String {
    format!("movie={}[out0+subcc]", path.to_str().unwrap())
}

/// A video input in both modes: our extraction and decoder against FFmpeg
/// reading the same file through its lavfi `subcc` output.
fn assert_video_input(what: &str, path: &Path) {
    let input = ["-f", "lavfi", "-i", &lavfi(path)];
    assert_cues(what, &our_video_cues(path, Cc608::new()), &ffmpeg_cues(&input));
    assert_real_time_events(what, &our_video_cues(path, Cc608::real_time()), &ffmpeg_real_time_events(&input));
}

#[test]
fn rollup_captions_from_mpeg2() {
    assert_video_input("Closedcaption_rollup.m2v", &rollup());
}

#[test]
fn scte20_captions_from_ts() {
    assert_video_input("scte20.ts", &scte20());
}

#[test]
fn rollup_captions_from_h264_in_matroska() {
    assert_video_input("H.264 in Matroska", &generated("h264.mkv"));
}

/// Packets of triplets through a decoder, then its end.
fn decode_all(mut decoder: Cc608, packets: &[(Option<i64>, Vec<u8>)]) -> Vec<Caption> {
    let mut ours = Vec::new();
    for (pts_us, data) in packets {
        ours.extend(decoder.decode(data, *pts_us));
    }
    ours.extend(decoder.finish());
    ours
}

#[test]
fn pop_on_captions_from_scc() {
    let path = refcheck::fate("sub/witch.scc");
    let ((num, den), packets) = ffprobe_packets(&path);
    let packets: Vec<_> = packets.into_iter().map(|(pts, data)| (pts.and_then(|t| ticks_to_us(t, num, den)), data)).collect();
    let input = ["-i", path.to_str().unwrap()];
    assert_cues("witch.scc", &decode_all(Cc608::new(), &packets), &ffmpeg_cues(&input));
    assert_real_time_events("witch.scc", &decode_all(Cc608::real_time(), &packets), &ffmpeg_real_time_events(&input));
}
