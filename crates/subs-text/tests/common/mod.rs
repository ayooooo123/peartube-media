//! FFmpeg oracle helpers shared by the subtitle acceptance tests.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use oxideav_core::subtitle::Segment;

/// `HH:MM:SS,mmm` of a decoded time, truncated to the millisecond (the
/// conversion the standalone acceptance has used since 26843d3).
pub fn format_srt_time(us: i64) -> String {
    let ms = (us / 1000).max(0);
    let s = ms / 1000;
    let m = s / 60;
    let h = m / 60;
    format!("{:02}:{:02}:{:02},{:03}", h, m % 60, s % 60, ms % 1000)
}

/// `start --> end` of a decoded cue, as FFmpeg's SubRip muxer writes it.
pub fn srt_timing(start_us: i64, end_us: i64) -> String {
    format!("{} --> {}", format_srt_time(start_us), format_srt_time(end_us))
}

/// FFmpeg's decode of the first subtitle stream of `path`, re-encoded with
/// `encoder` (`text` = visible text only, `srt` = SubRip markup) and muxed as
/// SubRip: one `(timing, body)` per cue FFmpeg emits.
pub fn ffmpeg_cues(path: &Path, options: &[&str], encoder: &str) -> Vec<(String, String)> {
    let output = Command::new(refcheck::pinned_ffmpeg())
        .args(["-nostdin", "-v", "error"])
        .args(options)
        .args(["-i", path.to_str().unwrap(), "-map", "0:s:0", "-c:s", encoder, "-f", "srt", "-"])
        .output()
        .expect("run ffmpeg");
    assert!(output.status.success(), "ffmpeg failed: {:?}", output);
    parse_srt_text(&String::from_utf8_lossy(&output.stdout))
}

/// The cues of a SubRip document: a counter line, a timing line, then body
/// lines up to the next counter-and-timing pair.
pub fn parse_srt_text(text: &str) -> Vec<(String, String)> {
    let mut cues = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let starts_cue = |i: usize| {
        let line = lines[i].trim();
        !line.is_empty()
            && line.chars().all(|c| c.is_ascii_digit())
            && i + 1 < lines.len()
            && lines[i + 1].contains("-->")
    };
    let mut i = 0;
    while i < lines.len() {
        if !starts_cue(i) {
            i += 1;
            continue;
        }
        let timing = lines[i + 1].trim().to_string();
        i += 2;
        let mut body_lines = Vec::new();
        while i < lines.len() && !starts_cue(i) {
            body_lines.push(lines[i]);
            i += 1;
        }
        cues.push((timing, body_lines.join("\n").trim().to_string()));
    }
    cues
}

/// The text a cue puts on screen: what the compositor draws from its
/// segments. Raw segments are drawn literally, so they stay literal here;
/// stripping their markup would conceal decoder errors.
pub fn visible_text(segments: &[Segment], out: &mut String) {
    for segment in segments {
        match segment {
            Segment::Text(text) | Segment::Raw(text) => out.push_str(text),
            Segment::LineBreak => out.push('\n'),
            Segment::Voice { name, children } => {
                out.push_str(name);
                out.push_str(": ");
                visible_text(children, out);
            }
            Segment::Bold(children)
            | Segment::Italic(children)
            | Segment::Underline(children)
            | Segment::Strike(children)
            | Segment::Color { children, .. }
            | Segment::Font { children, .. }
            | Segment::Class { children, .. }
            | Segment::Karaoke { children, .. } => visible_text(children, out),
            Segment::Timestamp { .. } => {}
        }
    }
}
