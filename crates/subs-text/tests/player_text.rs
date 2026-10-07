//! Text subtitles carried inside Matroska, WebM, MP4 and MOV must play
//! through the Player. Every file here is generated from `tests/data` by
//! FFmpeg (or mkvmerge, the usual producer of `S_TEXT/SSA`). For each one:
//!
//! * the production registry decodes FFmpeg's cues: same count, start, end
//!   and visible text (flattened as the standalone acceptance does);
//! * Matroska/WebM cue times equal `ffprobe -show_packets` to the
//!   microsecond;
//! * the Player shows every cue, each rendered exactly as FFmpeg's SubRip
//!   conversion of that cue renders, read by oxideav-subtitle's SubRip
//!   parser: FFmpeg applies the ASS styles carried in CodecPrivate in that
//!   conversion, so this is where they are checked, and the colours they
//!   assign must be on screen.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{ffmpeg_cues, srt_timing, visible_text};
use oxideav_core::{CuePosition, Frame, MediaType, SubtitleCue, TextAlign};
use parking_lot::Mutex;
use player::backend::{AudioSink, Backend, Clock, SubtitleImage, SubtitleSink, VideoSink};
use player::{Event, Headless, Player, PlayerOptions, TrackKind};

type Signature = (i32, i32, u32, u32, String);

fn signature(image: &SubtitleImage) -> Signature {
    (image.x, image.y, image.width, image.height, refcheck::md5_hex(&image.rgba))
}

struct Shown {
    width: u32,
    height: u32,
    images: Vec<SubtitleImage>,
}

struct CaptureBackend {
    media: Arc<Headless>,
    shown: Arc<Mutex<Vec<Shown>>>,
}

impl Backend for CaptureBackend {
    fn audio(&self) -> Box<dyn AudioSink> {
        self.media.audio()
    }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        self.media.video(clock)
    }
    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        Box::new(CaptureSink { shown: self.shown.clone() })
    }
}

struct CaptureSink {
    shown: Arc<Mutex<Vec<Shown>>>,
}

impl SubtitleSink for CaptureSink {
    fn show(&mut self, images: &[SubtitleImage], width: u32, height: u32) {
        if !images.is_empty() {
            self.shown.lock().push(Shown { width, height, images: images.to_vec() });
        }
    }
}

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

fn run(program: &str, args: &[&str]) {
    let output = Command::new(program).args(args).output().unwrap_or_else(|e| panic!("run {program}: {e}"));
    assert!(output.status.success(), "{program} {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

/// A generated file's path in Cargo's test scratch directory.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("subs-text-player");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// `source` converted by FFmpeg with subtitle codec `codec` into `name`.
fn ffmpeg_file(source: &str, codec: &str, name: &str) -> PathBuf {
    let out = scratch(name);
    let source = data(source);
    run("ffmpeg", &["-nostdin", "-v", "error", "-y", "-i", source.to_str().unwrap(), "-c:s", codec, out.to_str().unwrap()]);
    out
}

/// `ffprobe -show_packets` of the subtitle stream: `(start_us, end_us)`.
fn ffprobe_packets(path: &Path) -> Vec<(i64, i64)> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "s:0", "-show_entries", "packet=pts_time,duration_time", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert!(output.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&output.stderr));
    let us = |s: &str| (s.trim().parse::<f64>().unwrap() * 1e6).round() as i64;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (pts, duration) = l.split_once(',').unwrap();
            (us(pts), us(pts) + us(duration))
        })
        .collect()
}

/// FFmpeg's SubRip rendering of one cue (styles applied), as the cue must
/// look on screen. It is read by oxideav-subtitle's SubRip parser, code
/// independent of the decoders under test; the one `{\anN}` FFmpeg's
/// encoder may write is its alignment marker, which that parser does not
/// know, so it becomes the cue's horizontal alignment here.
fn styled_cue(body: &str) -> SubtitleCue {
    let mut body = body.to_string();
    let mut align = None;
    if let Some(at) = body.find("{\\an") {
        let n = body.as_bytes().get(at + 4).copied();
        if body.as_bytes().get(at + 5) == Some(&b'}') {
            align = match n {
                Some(b'1' | b'4' | b'7') => Some(TextAlign::Left),
                Some(b'3' | b'6' | b'9') => Some(TextAlign::Right),
                _ => None,
            };
            body.replace_range(at..at + 6, "");
        }
    }
    let document = format!("1\n00:00:00,000 --> 00:00:01,000\n{body}\n");
    let mut track = oxideav_subtitle::srt::parse(document.as_bytes()).unwrap_or_else(|e| panic!("SubRip {body:?}: {e}"));
    assert_eq!(track.cues.len(), 1, "FFmpeg's SubRip cue {body:?} parses to {} cues", track.cues.len());
    let mut cue = track.cues.remove(0);
    cue.positioning = align.map(|align| CuePosition { x: None, y: None, align, size: None });
    cue
}

/// Plays `path` with subtitle stream `stream` selected and returns every
/// image set the subtitle sink was shown, after checking the track.
fn play(path: &Path, stream: u32, codec: &str) -> Vec<Shown> {
    let media = Headless::new();
    media.set_active_streams(None, None, None, false);
    let backend = Arc::new(CaptureBackend { media, shown: Arc::new(Mutex::new(Vec::new())) });
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(
        path.to_str().unwrap(),
        backend.clone(),
        Arc::new(codecs::context()),
        PlayerOptions { realtime: false, subtitle: Some(stream), ..PlayerOptions::default() },
        move |event| {
            let _ = tx.send(event);
        },
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Ended) => break,
            Ok(Event::Error(error)) => panic!("{}: {error}", path.display()),
            Ok(Event::Changed) => {}
            Err(error) => panic!("{}: player did not end: {error}: {:?}", path.display(), player.state()),
        }
    }
    let state = player.state();
    assert!(state.error.is_none(), "{state:?}");
    let track = state.tracks.iter().find(|t| t.stream == stream).expect("subtitle track listed");
    assert_eq!((track.kind, track.codec.as_str()), (TrackKind::Subtitle, codec));
    drop(player);
    std::mem::take(&mut *backend.shown.lock())
}

fn pixels_of(image: &SubtitleImage, rgb: (u8, u8, u8)) -> usize {
    image.rgba.chunks_exact(4).filter(|p| p[..3] == [rgb.0, rgb.1, rgb.2] && p[3] == 255).count()
}

/// The whole acceptance for one generated file. `colors` lists, per cue
/// index, a colour FFmpeg's conversion gives that cue's text: it must be on
/// screen, so a renderer ignoring styles cannot pass by agreeing with an
/// equally unstyled expectation.
fn assert_plays_like_ffmpeg(path: &Path, codec: &str, packets_are_cues: bool, colors: &[(usize, (u8, u8, u8))]) {
    let text = ffmpeg_cues(path, &[], "text");
    let styled = ffmpeg_cues(path, &[], "srt");
    assert!(!text.is_empty(), "FFmpeg decodes no cue from {}", path.display());
    assert_eq!(
        text.iter().map(|(t, _)| t).collect::<Vec<_>>(),
        styled.iter().map(|(t, _)| t).collect::<Vec<_>>(),
        "FFmpeg's text and SubRip encodes disagree on the cues"
    );

    let decoded = refcheck::decode(path, &[codecs::register_all], MediaType::Subtitle, 0);
    assert_eq!(decoded.params.codec_id.as_str(), codec);
    let cues: Vec<&SubtitleCue> = decoded
        .frames
        .iter()
        .map(|f| match f {
            Frame::Subtitle(cue) => cue,
            other => panic!("non-subtitle frame {other:?}"),
        })
        .collect();
    let actual: Vec<(String, String)> = cues
        .iter()
        .map(|cue| {
            let mut body = String::new();
            visible_text(&cue.segments, &mut body);
            (srt_timing(cue.start_us, cue.end_us), body.trim().to_string())
        })
        .collect();
    assert_eq!(actual, text, "cue timing and visible text of {}", path.display());
    if packets_are_cues {
        let times: Vec<(i64, i64)> = cues.iter().map(|c| (c.start_us, c.end_us)).collect();
        assert_eq!(times, ffprobe_packets(path), "cue times vs ffprobe -show_packets of {}", path.display());
    }

    let stream = refcheck_stream_index(path);
    let shown = play(path, stream, codec);
    assert_eq!(shown.len(), styled.len(), "cues shown by the Player for {}", path.display());
    for (i, (shown, (_, body))) in shown.iter().zip(&styled).enumerate() {
        let expected = player::subs::render_text_cue(&styled_cue(body), shown.width, shown.height);
        let actual: Vec<Signature> = shown.images.iter().map(signature).collect();
        assert_eq!(actual, vec![signature(&expected)], "cue {i} of {} renders unlike FFmpeg's {body:?}", path.display());
    }
    for &(i, rgb) in colors {
        let n = pixels_of(&shown[i].images[0], rgb);
        assert!(n > 0, "cue {i} of {} shows no {rgb:?} text", path.display());
    }
}

/// The container index of the first subtitle stream, as the production
/// demuxer reports it.
fn refcheck_stream_index(path: &Path) -> u32 {
    let ctx = codecs::context();
    let format = refcheck::probe_container(&ctx, path).unwrap();
    let demuxer = ctx.containers.open_demuxer(&format, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap();
    demuxer.streams().iter().find(|s| s.params.media_type == MediaType::Subtitle).expect("subtitle stream").index
}

const YELLOW: (u8, u8, u8) = (255, 255, 0);
const RED: (u8, u8, u8) = (255, 0, 0);
const AZURE: (u8, u8, u8) = (0, 128, 255);
const CYAN: (u8, u8, u8) = (0, 255, 255);

#[test]
fn matroska_subrip_copied_from_srt() {
    let path = ffmpeg_file("styled.srt", "copy", "srt_copy.mkv");
    assert_plays_like_ffmpeg(&path, "subrip", true, &[(1, YELLOW), (3, RED)]);
}

#[test]
fn matroska_ass_converted_from_srt() {
    let path = ffmpeg_file("styled.srt", "ass", "srt_ass.mkv");
    assert_plays_like_ffmpeg(&path, "ass", true, &[(1, YELLOW), (3, RED)]);
}

#[test]
fn matroska_ass_copied_with_styles() {
    let path = ffmpeg_file("styled.ass", "copy", "ass_copy.mkv");
    assert_plays_like_ffmpeg(&path, "ass", true, &[(1, YELLOW), (2, AZURE), (3, RED)]);
}

#[test]
fn matroska_subrip_converted_from_ass() {
    let path = ffmpeg_file("styled.ass", "srt", "ass_srt.mkv");
    assert_plays_like_ffmpeg(&path, "subrip", true, &[(1, YELLOW), (2, AZURE), (3, RED)]);
}

#[test]
fn matroska_ass_copied_from_ssa() {
    let path = ffmpeg_file("styled.ssa", "copy", "ssa_copy.mkv");
    assert_plays_like_ffmpeg(&path, "ass", true, &[(1, CYAN)]);
}

#[test]
fn matroska_ssa_from_mkvmerge() {
    let path = scratch("ssa_mkvmerge.mkv");
    run("mkvmerge", &["-q", "-o", path.to_str().unwrap(), data("styled.ssa").to_str().unwrap()]);
    assert_plays_like_ffmpeg(&path, "ssa", true, &[(1, CYAN)]);
}

#[test]
fn mp4_mov_text_from_srt() {
    let path = ffmpeg_file("styled.srt", "mov_text", "srt.mp4");
    assert_plays_like_ffmpeg(&path, "mov_text", false, &[]);
}

#[test]
fn mp4_mov_text_from_ass_with_styles() {
    let path = ffmpeg_file("styled.ass", "mov_text", "ass.mp4");
    assert_plays_like_ffmpeg(&path, "mov_text", false, &[(1, YELLOW), (2, AZURE)]);
}

#[test]
fn mov_mov_text_from_ass() {
    let path = ffmpeg_file("styled.ass", "mov_text", "ass.mov");
    assert_plays_like_ffmpeg(&path, "mov_text", false, &[(1, YELLOW), (2, AZURE)]);
}

#[test]
fn webm_webvtt_from_srt() {
    let path = ffmpeg_file("styled.srt", "webvtt", "srt.webm");
    assert_plays_like_ffmpeg(&path, "webvtt", true, &[]);
}

#[test]
fn matroska_webvtt_from_ass() {
    let path = ffmpeg_file("styled.ass", "webvtt", "ass_vtt.mkv");
    assert_plays_like_ffmpeg(&path, "webvtt", true, &[]);
}
