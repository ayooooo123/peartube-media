//! Text subtitles carried inside Matroska, WebM, MP4, MOV and Ogg must play
//! through the Player. Every file here is generated from `tests/data` by
//! FFmpeg (or mkvmerge, the usual producer of `S_TEXT/SSA`), or is FATE's.
//! For each one:
//!
//! * the production registry decodes FFmpeg's cues: same count, start, end
//!   and visible text (flattened as the standalone acceptance does);
//! * Matroska/WebM and OGM cue times equal `ffprobe -show_packets` to the
//!   microsecond;
//! * the Player shows every cue with text and draws its requested colours.
//!   ASS retains its own styles and is compared with libass in subs-render;
//!   a SubRip conversion is not a rendering oracle for ASS overrides.
//!
//! FFmpeg cannot open Ogg Kate or CMML. Their oracles are the libkate
//! encoding of FATE's Kate sample and the Xiph CMML mapping.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{ffmpeg_cues, srt_timing, visible_text};
use oxideav_core::{Frame, MediaType};
use parking_lot::Mutex;
use player::backend::{AudioSink, Backend, Clock, SubtitleImage, SubtitleSink, VideoSink};
use player::{Event, Headless, Player, PlayerOptions, TrackKind};

struct Shown {
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
    fn show(&mut self, images: &[SubtitleImage], _width: u32, _height: u32) {
        // A cue of spaces renders as an empty image: nothing on screen.
        if images.iter().any(|image| image.width > 0 && image.height > 0) {
            self.shown.lock().push(Shown { images: images.to_vec() });
        }
    }
}

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

fn run(program: impl AsRef<std::ffi::OsStr>, args: &[&str]) {
    let program = program.as_ref();
    let output = Command::new(program).args(args).output().unwrap_or_else(|e| panic!("run {}: {e}", program.to_string_lossy()));
    assert!(output.status.success(), "{} {args:?}: {}", program.to_string_lossy(), String::from_utf8_lossy(&output.stderr));
}

/// A generated file's path in Cargo's test scratch directory.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("subs-text-player");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// `source` converted by `refcheck::system_ffmpeg` with subtitle codec
/// `codec` into `name`.
fn ffmpeg_file(source: &str, codec: &str, name: &str) -> PathBuf {
    let out = scratch(name);
    let source = data(source);
    run(refcheck::system_ffmpeg(), &["-nostdin", "-v", "error", "-y", "-i", source.to_str().unwrap(), "-c:s", codec, out.to_str().unwrap()]);
    out
}

/// The pinned `ffprobe -show_packets` of the subtitle stream: `(start_us, end_us)`.
fn ffprobe_packets(path: &Path) -> Vec<(i64, i64)> {
    let output = Command::new(refcheck::pinned_ffprobe())
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
    // Catches a hang. The OGM file also decodes 640x480 XVID and AC-3 in
    // full, about a minute on a loaded host.
    let deadline = Instant::now() + Duration::from_secs(240);
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
    // Black outlines blend with antialiased foreground pixels. Compare hue
    // after that blend, not the bitmap font's fully covered RGB samples.
    let wanted = [rgb.0, rgb.1, rgb.2];
    let peak = u16::from(*wanted.iter().max().unwrap()).max(1);
    image.rgba.chunks_exact(4).filter(|p| {
        let intensity = u16::from(*p[..3].iter().max().unwrap());
        p[3] >= 32 && intensity >= 64 && (0..3).all(|c| {
            u16::from(p[c]).abs_diff(u16::from(wanted[c]) * intensity / peak) <= 2
        })
    }).count()
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

    let actual = decoded_cues(path, codec);
    assert_eq!(actual.iter().map(|(t, b, _)| (t.clone(), b.clone())).collect::<Vec<_>>(), text,
        "cue timing and visible text of {}", path.display());
    if packets_are_cues {
        let times: Vec<(i64, i64)> = actual.iter().map(|(_, _, times)| *times).collect();
        assert_eq!(times, ffprobe_packets(path), "cue times vs ffprobe -show_packets of {}", path.display());
    }

    // A cue without text puts nothing on screen.
    let styled: Vec<&(String, String)> = styled.iter().filter(|(_, body)| !body.is_empty()).collect();
    let stream = refcheck_stream_index(path);
    let shown = play(path, stream, codec);
    assert_eq!(shown.len(), styled.len(), "cues shown by the Player for {}", path.display());
    for &(i, rgb) in colors {
        let n = pixels_of(&shown[i].images[0], rgb);
        assert!(n > 0, "cue {i} of {} shows no {rgb:?} text", path.display());
    }
}

/// The production registry's decode of the first subtitle stream of `path`:
/// per cue its SubRip timing, visible text and `(start_us, end_us)`.
fn decoded_cues(path: &Path, codec: &str) -> Vec<(String, String, (i64, i64))> {
    let decoded = refcheck::decode(path, &[codecs::register_all], MediaType::Subtitle, 0);
    assert_eq!(decoded.params.codec_id.as_str(), codec);
    decoded
        .frames
        .iter()
        .map(|f| match f {
            Frame::Subtitle(cue) => {
                let mut body = String::new();
                visible_text(&cue.segments, &mut body);
                (srt_timing(cue.start_us, cue.end_us), body.trim().to_string(), (cue.start_us, cue.end_us))
            }
            other => panic!("non-subtitle frame {other:?}"),
        })
        .collect()
}

/// Plays a file FFmpeg cannot decode: the Player lists the stream as
/// `codec`, decodes `expected` (timing, visible text), and shows each cue.
fn assert_plays_cues(path: &Path, codec: &str, expected: &[(i64, i64, &str)]) {
    let decoded = decoded_cues(path, codec);
    let actual: Vec<(i64, i64, &str)> = decoded.iter().map(|(_, body, (s, e))| (*s, *e, body.as_str())).collect();
    assert_eq!(actual, expected, "cues of {}", path.display());
    let shown = play(path, refcheck_stream_index(path), codec);
    assert_eq!(shown.len(), expected.len(), "cues shown by the Player for {}", path.display());
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
const BLUE: (u8, u8, u8) = (0, 0, 255);
const WHITE: (u8, u8, u8) = (255, 255, 255);

/// Which of `palette` libass draws at `t` seconds of the ASS script `path`
/// over black (FFmpeg's `ass` filter, RGB throughout). The pinned build has
/// no libass, so this reference comes from `refcheck::system_ffmpeg`.
fn libass_colours(path: &Path, t: f64, palette: &[(u8, u8, u8)]) -> Vec<(u8, u8, u8)> {
    let output = Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i", "color=c=black:s=384x288:r=10,format=rgb24", "-vf"])
        .arg(format!("ass=filename={}", path.display()))
        .args(["-ss", &t.to_string(), "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .expect("run ffmpeg");
    assert!(output.status.success() && !output.stdout.is_empty(), "libass render: {}", String::from_utf8_lossy(&output.stderr));
    palette
        .iter()
        .copied()
        .filter(|&(r, g, b)| output.stdout.chunks_exact(3).filter(|p| *p == [r, g, b]).count() >= 50)
        .collect()
}

/// A style reset (`\r`) returns to the event's own style, and so does a
/// reset naming a style the script lacks: what libass, the reference ASS
/// renderer, draws. FFmpeg's SubRip conversion resets a bare `\r` to
/// `Default` and drops all styling for a missing name, so it cannot be the
/// oracle here; libass's own render is.
///
/// Each reset's effect is the only source of one colour in its cue (the
/// event style is Yellow): a reset that does nothing, or goes to another
/// style, changes that cue's colours. Cue 0 resets an inline colour, cue 1
/// names a style, cue 2 resets a named style, cue 3 names a missing one.
#[test]
fn ass_style_resets_render_as_libass_renders_them() {
    let path = data("reset.ass");
    let palette = [YELLOW, BLUE, WHITE, RED];
    let designed = [[YELLOW, BLUE], [YELLOW, RED], [YELLOW, RED], [YELLOW, RED]];
    let shown = play(&path, 0, "ass");
    assert_eq!(shown.len(), designed.len(), "cues shown");
    for (i, t) in [1.0, 2.5, 4.0, 5.5].into_iter().enumerate() {
        let expected = libass_colours(&path, t, &palette);
        assert_eq!(expected, designed[i], "cue {i}: libass draws the colours the fixture is built around");
        let ours: Vec<(u8, u8, u8)> = palette.iter().copied().filter(|&c| pixels_of(&shown[i].images[0], c) >= 20).collect();
        assert_eq!(ours, expected, "cue {i}: text colours drawn vs libass");
    }
}

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

/// FFmpeg writes `mov_text` into MOV as a QuickTime `text` entry (above);
/// a `tx3g` entry, the MP4 form, must play the same.
#[test]
fn mov_tx3g_from_srt() {
    let path = scratch("srt_tx3g.mov");
    let source = data("styled.srt");
    run(refcheck::system_ffmpeg(), &["-nostdin", "-v", "error", "-y", "-i", source.to_str().unwrap(), "-c:s", "mov_text", "-tag:s", "tx3g",
        path.to_str().unwrap()]);
    assert_plays_like_ffmpeg(&path, "mov_text", false, &[]);
}

/// OGM text (FFmpeg `oggparseogm.c`, `textdec.c`): FATE's file has blank
/// cues at both ends, which FFmpeg keeps and nothing shows.
#[test]
fn ogm_text_plays_like_ffmpeg() {
    assert_plays_like_ffmpeg(&refcheck::fate("ogg-ogm/bots01.ogm"), "text", true, &[]);
}

/// libkate encoded FATE's sample: two text events with start and duration
/// in the packets (500 + 1500 and 2500 + 2500 at the ID header's granule
/// rate 1000/1). The decoder breaks lines at `|`.
#[test]
fn ogg_kate_plays_libkates_events() {
    assert_plays_cues(&refcheck::fate("ogg-kate/kate-subtitles.ogg"), "kate",
        &[(500_000, 2_000_000, "Hello from Kate\nfirst line"), (2_500_000, 5_000_000, "Second event")]);
}

/// One Ogg page per packet of a CMML stream (serial 7).
fn cmml_page(flags: u8, granule_position: i64, seq_no: u32, packets: &[&[u8]]) -> Vec<u8> {
    use oxideav_ogg::page::{lace, Page};
    Page {
        flags,
        granule_position,
        serial: 7,
        seq_no,
        lacing: packets.iter().flat_map(|p| lace(p.len())).collect(),
        data: packets.concat(),
    }
    .to_bytes()
}

/// The Xiph CMML mapping (https://wiki.xiph.org/CMML): an ident header
/// (version 2.1, granule rate 1000/1, granule shift 32), the preamble and
/// head headers, then one clip per page. A clip page's granule is its time
/// above the shift and the previous clip's below. A clip lasts until the
/// next clip of its track; the EOS page's empty clip ends the last one.
#[test]
fn ogg_cmml_clips_last_until_the_next_clip() {
    use oxideav_ogg::page::flags;
    let mut ident = b"CMML\0\0\0\0".to_vec();
    ident.extend_from_slice(&2u16.to_le_bytes());
    ident.extend_from_slice(&1u16.to_le_bytes());
    ident.extend_from_slice(&1000i64.to_le_bytes());
    ident.extend_from_slice(&1i64.to_le_bytes());
    ident.push(32);
    let granule = |ms: i64, previous: i64| (ms << 32) | previous;
    let file = [
        cmml_page(flags::FIRST_PAGE, 0, 0, &[&ident]),
        cmml_page(0, 0, 1, &[
            b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<!DOCTYPE cmml SYSTEM \"cmml.dtd\">\n<?cmml lang=\"en\"?>",
            b"<head>\n<title>Fixture</title>\n</head>",
        ]),
        cmml_page(0, granule(1500, 0), 2, &[b"<clip id=\"one\" track=\"main\"><desc>First clip</desc></clip>"]),
        cmml_page(0, granule(4000, 1500), 3,
            &[b"<clip id=\"two\" track=\"main\"><a href=\"http://example.com/\">link</a><desc>Second clip</desc></clip>"]),
        cmml_page(flags::LAST_PAGE, granule(6000, 4000), 4, &[b"<clip track=\"main\"/>"]),
    ]
    .concat();
    let path = scratch("clips.ogg");
    std::fs::write(&path, file).unwrap();
    assert_plays_cues(&path, "cmml", &[(1_500_000, 4_000_000, "First clip"), (4_000_000, 6_000_000, "Second clip")]);
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
