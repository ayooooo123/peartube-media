//! WebVTT cues come up where their settings place them (W3C WebVTT §7),
//! through the player's registry and its real subtitle sink, whatever
//! carries them:
//! - a `.vtt` file: settings and identifiers from each cue's timing and
//!   identifier lines, regions from its header;
//! - Matroska and WebM, as FFmpeg remuxes that file: settings and
//!   identifiers in each block (`D_WEBVTT/SUBTITLES`). FFmpeg keeps no
//!   header there, so its region cue shows outside any region;
//! - MP4 (`wvtt`, ISO/IEC 14496-30), which FFmpeg cannot write: built here,
//!   the header in the sample entry's `vttC` box, each cue a `vttc` box
//!   with its `iden` and `sttg`, gaps `vtte`.
//!
//! The player runs without video, so text lays out on its 320x240 canvas.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use player::backend::{AudioSink, Backend, Clock, SubtitleImage, SubtitleSink, VideoSink};
use player::{Event, Headless, Player, PlayerOptions};

/// One show: the canvas, then each image's rectangle.
type Shown = ((u32, u32), Vec<(i32, i32, u32, u32)>);

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
            let rects = images.iter().map(|i| (i.x, i.y, i.width, i.height)).collect();
            self.shown.lock().push(((width, height), rects));
        }
    }
}

/// Every show while `path`'s first stream plays, each cue on its own
/// (capture mode).
fn shown(path: &Path) -> Vec<Shown> {
    let media = Headless::new();
    media.set_active_streams(None, None, None, false);
    let backend = Arc::new(CaptureBackend { media, shown: Arc::new(Mutex::new(Vec::new())) });
    let (tx, rx) = std::sync::mpsc::channel();
    let options = PlayerOptions { realtime: false, subtitle: Some(0), ..PlayerOptions::default() };
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()), options, move |event| {
        let _ = tx.send(event);
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(Event::Ended) => break,
            Ok(Event::Error(error)) => panic!("{}: {error}", path.display()),
            Ok(Event::Changed) => {}
            Err(error) => panic!("{}: player failed to end: {error}: {:?}", path.display(), player.state()),
        }
    }
    let state = player.state();
    assert!(state.error.is_none(), "{state:?}");
    drop(player);
    let shown = backend.shown.lock().clone();
    shown
}

/// The cues: identifier, timing, settings, text.
const CUES: [(&str, u32, &str, &str); 7] = [
    ("top", 0, "line:0", "Top line"),
    ("", 1, "align:start size:50%", "Right half"),
    ("mid", 2, "line:50%,center", "Middle"),
    ("", 3, "region:fred align:left", "In the region"),
    ("", 4, "vertical:rl line:0", "Vertical"),
    ("", 5, "", "Default bottom"),
    ("third", 6, "line:2", "Third line"),
];

const HEADER: &str = "WEBVTT\n\nREGION\nid:fred width:40% lines:3 regionanchor:0%,100% viewportanchor:10%,90%";

fn vtt() -> String {
    let mut out = format!("{HEADER}\n\n");
    for (id, second, settings, text) in CUES {
        if !id.is_empty() {
            out.push_str(&format!("{id}\n"));
        }
        out.push_str(&format!("00:00:0{second}.000 --> 00:00:0{second}.900 {settings}\n{text}\n\n"));
    }
    out
}

fn scratch() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("webvtt-placement");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Where each cue comes up on the 320x240 canvas, checked against WebVTT's
/// placement. `regions` is false where the container kept no header.
fn assert_placed(what: &str, shown: &[Shown], regions: bool) {
    assert_eq!(shown.len(), CUES.len(), "{what}: {shown:?}");
    for (((canvas, rects), (_, _, settings, _)), index) in shown.iter().zip(CUES).zip(0..) {
        assert_eq!(*canvas, (320, 240), "{what}");
        assert_eq!(rects.len(), 1, "{what}: cue {index}");
        let (x, y, w, h) = rects[0];
        let (w, h) = (w as i32, h as i32);
        let at = format!("{what}: cue {index} ({settings}) at {x},{y} {w}x{h}");
        assert!(x >= 0 && y >= 0 && x + w <= 320 && y + h <= 240, "{at}");
        match index {
            // line:0 — the top line.
            0 => assert_eq!(y, 0, "{at}"),
            // align:start size:50% — the right half's start.
            1 => assert_eq!(x, 160, "{at}"),
            // line:50%,center — centered on the middle.
            2 => assert!((y + h / 2 - 120).abs() <= 1, "{at}"),
            // region:fred (10%,90% anchor, 40% wide, 3 lines): its bottom
            // line, the region's width from 32 (bottom at 216).
            3 if regions => assert_eq!((x, w, y + h), (32, 128, 216), "{at}"),
            // Without the header: no region, so the default bottom line.
            3 => assert_eq!(y + h, 240, "{at}"),
            // vertical:rl line:0 — a column at the right edge.
            4 => assert!(x + w == 320 && h > w, "{at}"),
            // No settings: the bottom line box, on the canvas's bottom edge
            // (clear of the region box above it).
            5 => assert_eq!(y + h, 240, "{at}"),
            // line:2 — the third line from the top.
            _ => assert_eq!(y, 2 * h, "{at}"),
        }
    }
}

#[test]
fn positioned_cues_from_a_vtt_file_and_ffmpeg_remuxes() {
    let dir = scratch();
    let path = dir.join("cues.vtt");
    std::fs::write(&path, vtt()).unwrap();
    let from_vtt = shown(&path);
    assert_placed("vtt", &from_vtt, true);
    for container in ["mkv", "webm"] {
        let remux = dir.join(format!("cues.{container}"));
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-y", "-i"])
            .arg(&path)
            .args(["-c:s", "copy"])
            .arg(&remux)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let from_container = shown(&remux);
        assert_placed(container, &from_container, false);
        // Every cue but the region's comes up exactly where the .vtt put it
        // (FFmpeg's Matroska and WebM keep no header, so no region).
        for (index, (a, b)) in from_vtt.iter().zip(&from_container).enumerate() {
            if index != 3 {
                assert_eq!(a, b, "{container}: cue {index}");
            }
        }
    }
}

fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    [&(8 + body.len() as u32).to_be_bytes()[..], kind, body].concat()
}

fn full_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    mp4_box(kind, &[&[0u8; 4][..], body].concat())
}

/// An MP4 file with one `wvtt` track holding `CUES` (timescale 1000, a
/// sample per cue and per gap).
fn mp4() -> Vec<u8> {
    let mut samples: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut at = 0;
    for (id, second, settings, text) in CUES {
        let start = second * 1000;
        if start > at {
            samples.push((start - at, mp4_box(b"vtte", b"")));
        }
        let cue = [mp4_box(b"iden", id.as_bytes()), mp4_box(b"sttg", settings.as_bytes()), mp4_box(b"payl", text.as_bytes())].concat();
        samples.push((900, mp4_box(b"vttc", &cue)));
        at = start + 900;
    }
    let duration: u32 = samples.iter().map(|(d, _)| d).sum();
    let be = |v: u32| v.to_be_bytes();
    let matrix: Vec<u8> = [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000].iter().flat_map(|v| v.to_be_bytes()).collect();
    let stts: Vec<u8> = [&be(samples.len() as u32)[..], &samples.iter().flat_map(|(d, _)| [be(1), be(*d)].concat()).collect::<Vec<u8>>()].concat();
    let stsz: Vec<u8> = [&be(0)[..], &be(samples.len() as u32), &samples.iter().flat_map(|(_, s)| be(s.len() as u32)).collect::<Vec<u8>>()].concat();
    let wvtt = mp4_box(b"wvtt", &[&[0u8; 6][..], &1u16.to_be_bytes(), &mp4_box(b"vttC", HEADER.as_bytes())].concat());
    let moov = |chunk_offset: u32| {
        let stbl = mp4_box(b"stbl", &[
            full_box(b"stsd", &[&be(1)[..], &wvtt].concat()),
            full_box(b"stts", &stts),
            full_box(b"stsc", &[be(1), be(1), be(samples.len() as u32), be(1)].concat()),
            full_box(b"stsz", &stsz),
            full_box(b"stco", &[be(1), be(chunk_offset)].concat()),
        ].concat());
        let dinf = mp4_box(b"dinf", &full_box(b"dref", &[&be(1)[..], &mp4_box(b"url ", &[0, 0, 0, 1])].concat()));
        let minf = mp4_box(b"minf", &[full_box(b"nmhd", b""), dinf, stbl].concat());
        let mdhd = full_box(b"mdhd", &[&be(0)[..], &be(0), &be(1000), &be(duration), &[0x55, 0xc4, 0, 0]].concat());
        let hdlr = full_box(b"hdlr", &[&be(0)[..], b"text", &[0u8; 12], b"\0"].concat());
        let mdia = mp4_box(b"mdia", &[mdhd, hdlr, minf].concat());
        let tkhd = mp4_box(b"tkhd", &[&[0, 0, 0, 3][..], &be(0), &be(0), &be(1), &be(0), &be(duration), &[0u8; 8], &[0u8; 8], &matrix, &be(0), &be(0)].concat());
        let trak = mp4_box(b"trak", &[tkhd, mdia].concat());
        let mvhd = full_box(b"mvhd", &[&be(0)[..], &be(0), &be(1000), &be(duration), &be(0x0001_0000), &[1, 0], &[0u8; 10], &matrix, &[0u8; 24], &be(2)].concat());
        mp4_box(b"moov", &[mvhd, trak].concat())
    };
    let ftyp = mp4_box(b"ftyp", &[&b"isom"[..], &be(0x200), b"isom", b"iso6", b"mp41"].concat());
    let offset = (ftyp.len() + moov(0).len() + 8) as u32;
    let data: Vec<u8> = samples.iter().flat_map(|(_, s)| s.clone()).collect();
    [ftyp, moov(offset), mp4_box(b"mdat", &data)].concat()
}

#[test]
fn positioned_cues_from_mp4_wvtt() {
    let path = scratch().join("cues.mp4");
    std::fs::write(&path, mp4()).unwrap();
    let from_mp4 = shown(&path);
    assert_placed("mp4", &from_mp4, true);
    let vtt_path = scratch().join("cues-for-mp4.vtt");
    std::fs::write(&vtt_path, vtt()).unwrap();
    assert_eq!(from_mp4, shown(&vtt_path), "MP4 and .vtt place their cues alike");
}
