//! Audio disc CUE sheet parser and chapter representation (`#![forbid(unsafe_code)]`).
//!
//! Supports both standalone `.cue` files (resolving a safe sibling audio file in
//! the same directory/URL prefix without path traversal) and embedded `CUESHEET`
//! tags in FLAC, WavPack, Monkey's Audio, and Matroska containers.

#![forbid(unsafe_code)]

use std::time::Duration;

/// Maximum `.cue` text accepted from untrusted input (256 KiB).
pub const MAX_CUE_BYTES: usize = 256 * 1024;
const MAX_CUE_TRACKS: usize = 256;

/// One chapter or CUE sheet track on the media timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chapter {
    pub title: Option<String>,
    pub performer: Option<String>,
    pub start: Duration,
    pub end: Option<Duration>,
}

/// Parsed CUE sheet with optional safe sibling audio filename and ordered tracks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CueSheet {
    pub title: Option<String>,
    pub performer: Option<String>,
    /// Safe single-component filename from the first `FILE "..."` directive.
    pub audio_file: Option<String>,
    pub chapters: Vec<Chapter>,
}

fn unquote_first(rest: &str) -> Option<&str> {
    let rest = rest.trim();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(&stripped[..end])
    } else {
        rest.split_whitespace().next()
    }
}

/// Accepts only a plain basename (`album.flac`) with no path separators, drive
/// prefixes, NUL bytes, or `..` traversal so untrusted `.cue` files cannot
/// escape their own directory.
fn sanitize_basename(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains(':')
        || trimmed.contains('\0')
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// Parses `MM:SS:FF` (75 CD frames per second) into an exact [`Duration`].
fn parse_cd_time(token: &str) -> Option<Duration> {
    let mut parts = token.trim().split(':');
    let mins: u64 = parts.next()?.parse().ok()?;
    let secs: u64 = parts.next()?.parse().ok()?;
    let frames: u64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || secs >= 60 || frames >= 75 {
        return None;
    }
    let total_secs = mins.checked_mul(60)?.checked_add(secs)?;
    let frame_nanos = (frames * 1_000_000_000) / 75;
    Some(Duration::new(total_secs, frame_nanos as u32))
}

#[derive(Default)]
struct PendingTrack {
    is_audio: bool,
    title: Option<String>,
    performer: Option<String>,
    index01: Option<Duration>,
    index00: Option<Duration>,
}

/// Parses a UTF-8 (with optional BOM) CUE sheet into [`CueSheet`].
pub fn parse_cue_sheet(input: &str) -> CueSheet {
    let text = input.strip_prefix('\u{feff}').unwrap_or(input);
    let text = if text.len() > MAX_CUE_BYTES {
        &text[..MAX_CUE_BYTES]
    } else {
        text
    };
    let mut sheet = CueSheet::default();
    let mut current: Option<PendingTrack> = None;

    let flush_track = |track: PendingTrack, out: &mut Vec<Chapter>, sheet_perf: &Option<String>| {
        if out.len() >= MAX_CUE_TRACKS || !track.is_audio {
            return;
        }
        if let Some(start) = track.index01.or(track.index00) {
            out.push(Chapter {
                title: track.title,
                performer: track.performer.or_else(|| sheet_perf.clone()),
                start,
                end: None,
            });
        }
    };

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((cmd, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let rest = rest.trim();
        if cmd.eq_ignore_ascii_case("FILE") {
            if sheet.audio_file.is_none() {
                if let Some(name) = unquote_first(rest).and_then(sanitize_basename) {
                    sheet.audio_file = Some(name);
                }
            }
        } else if cmd.eq_ignore_ascii_case("TRACK") {
            if let Some(prev) = current.take() {
                flush_track(prev, &mut sheet.chapters, &sheet.performer);
            }
            let mut toks = rest.split_whitespace();
            let _num = toks.next();
            let kind = toks.next().unwrap_or("");
            current = Some(PendingTrack {
                is_audio: kind.eq_ignore_ascii_case("AUDIO"),
                ..PendingTrack::default()
            });
        } else if cmd.eq_ignore_ascii_case("TITLE") {
            let val = unquote_first(rest).map(str::to_string);
            if let Some(trk) = &mut current {
                trk.title = val;
            } else {
                sheet.title = val;
            }
        } else if cmd.eq_ignore_ascii_case("PERFORMER") {
            let val = unquote_first(rest).map(str::to_string);
            if let Some(trk) = &mut current {
                trk.performer = val;
            } else {
                sheet.performer = val;
            }
        } else if cmd.eq_ignore_ascii_case("INDEX") {
            if let Some(trk) = &mut current {
                let mut toks = rest.split_whitespace();
                let idx_str = toks.next().unwrap_or("");
                if let Some(ts) = toks.next().and_then(parse_cd_time) {
                    if idx_str == "01" || idx_str == "1" {
                        trk.index01 = Some(ts);
                    } else if idx_str == "00" || idx_str == "0" {
                        trk.index00 = Some(ts);
                    }
                }
            }
        }
    }
    if let Some(last) = current.take() {
        flush_track(last, &mut sheet.chapters, &sheet.performer);
    }

    sheet.chapters.sort_by_key(|c| c.start);
    finalize_chapter_ends(&mut sheet.chapters, None);
    sheet
}

/// Fills each chapter's `end` from the following chapter's `start`, and sets the
/// last chapter's `end` to `total_duration` when known.
pub fn finalize_chapter_ends(chapters: &mut [Chapter], total_duration: Option<Duration>) {
    for i in 0..chapters.len() {
        let next_start = chapters.get(i + 1).map(|c| c.start);
        if let Some(ns) = next_start {
            if ns >= chapters[i].start {
                chapters[i].end = Some(ns);
            }
        } else if chapters[i].end.is_none() {
            if let Some(dur) = total_duration {
                if dur >= chapters[i].start {
                    chapters[i].end = Some(dur);
                }
            }
        }
    }
}

/// Resolves a sibling filename next to `cue_url` (supporting local paths and HTTP URLs).
pub fn resolve_sibling_url(cue_url: &str, audio_basename: &str) -> Option<String> {
    let clean = sanitize_basename(audio_basename)?;
    if cue_url.starts_with("http://") || cue_url.starts_with("https://") {
        let (base_no_query, query) = match cue_url.split_once('?') {
            Some((b, q)) => (b, Some(q)),
            None => (cue_url, None),
        };
        let slash = base_no_query.rfind('/')?;
        let prefix = &base_no_query[..=slash];
        let mut out = format!("{prefix}{clean}");
        if let Some(q) = query {
            out.push('?');
            out.push_str(q);
        }
        Some(out)
    } else {
        let path = std::path::Path::new(cue_url);
        let parent = path.parent()?;
        Some(parent.join(clean).to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cd_sector_accurate_cue_sheet_and_rejects_traversal() {
        let cue = r#"
REM GENRE "Electronic"
PERFORMER "Daft Punk"
TITLE "Random Access Memories"
FILE "ram_24_882.flac" WAVE
  TRACK 01 AUDIO
    TITLE "Give Life Back to Music"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "The Game of Love"
    PERFORMER "Daft Punk"
    INDEX 00 04:34:15
    INDEX 01 04:35:30
"#;
        let parsed = parse_cue_sheet(cue);
        assert_eq!(parsed.audio_file.as_deref(), Some("ram_24_882.flac"));
        assert_eq!(parsed.chapters.len(), 2);
        assert_eq!(parsed.chapters[0].title.as_deref(), Some("Give Life Back to Music"));
        assert_eq!(parsed.chapters[0].start, Duration::ZERO);
        // 04:35:30 = 275 s + 30/75 s = 275.400 s
        let expected_t2 = Duration::from_millis(275_400);
        assert_eq!(parsed.chapters[1].start, expected_t2);
        assert_eq!(parsed.chapters[0].end, Some(expected_t2));

        let hostile = r#"FILE "../../etc/passwd" WAVE"#;
        assert!(parse_cue_sheet(hostile).audio_file.is_none());
    }
}
