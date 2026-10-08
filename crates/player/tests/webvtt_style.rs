//! WebVTT cues drawn as browsers draw them (W3C WebVTT §7.3–§7.4, §8),
//! through the player's registry and its real subtitle sink, from a `.vtt`
//! file and from MP4, whose `vttC` box carries the same header:
//! - `STYLE` blocks: `::cue` rules by class, voice, identifier and type
//!   set colours, backgrounds, a text shadow and sizes;
//! - right-to-left text: `align:start` anchors the cue box's right side;
//! - ruby: the annotation above its base, half size;
//! - a region with `lines:0` shows nothing, as browsers clip a region's
//!   content to its height.
//!
//! The player runs without video, so text lays out on its 320x240 canvas.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use player::backend::{AudioSink, Backend, Clock, SubtitleImage, SubtitleSink, VideoSink};
use player::{Event, Headless, Player, PlayerOptions};

struct CaptureBackend {
    media: Arc<Headless>,
    shown: Arc<Mutex<Vec<Vec<SubtitleImage>>>>,
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
    shown: Arc<Mutex<Vec<Vec<SubtitleImage>>>>,
}

impl SubtitleSink for CaptureSink {
    fn show(&mut self, images: &[SubtitleImage], _width: u32, _height: u32) {
        if !images.is_empty() {
            self.shown.lock().push(images.to_vec());
        }
    }
}

/// Every non-empty show while `path`'s first stream plays, each cue on its
/// own (capture mode).
fn shown(path: &Path) -> Vec<Vec<SubtitleImage>> {
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
    drop(player);
    let shown = backend.shown.lock().clone();
    shown
}

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const LIME: [u8; 4] = [0, 255, 0, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];
const BOX: [u8; 4] = [0, 0, 0, 204];

const HEADER: &str = "WEBVTT\n\nSTYLE\n::cue(.loud) { color: red; background-color: blue }\n::cue(v[voice=\"Roger\"]) { color: lime }\n::cue(#shadowed) { background: none; text-shadow: 2px 2px red }\n::cue(rt) { color: lime; background: transparent }\n\nREGION\nid:hidden lines:0 width:50%";

/// The cues: identifier, settings, text.
const CUES: [(&str, &str, &str); 6] = [
    ("", "", "<c.loud>LOUD</c>"),
    ("", "", "<v Roger>Hello</v>"),
    ("shadowed", "", "Shade"),
    ("", "align:start size:50%", "שלום עולם"),
    ("", "line:0", "<ruby>漢字<rt>kanji</rt></ruby>"),
    ("", "region:hidden", "Never seen"),
];

fn vtt() -> String {
    let mut out = format!("{HEADER}\n\n");
    for (second, (id, settings, text)) in CUES.iter().enumerate() {
        if !id.is_empty() {
            out.push_str(&format!("{id}\n"));
        }
        out.push_str(&format!("00:00:0{second}.000 --> 00:00:0{second}.900 {settings}\n{text}\n\n"));
    }
    out
}

fn scratch() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("webvtt-style");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn pixels(image: &SubtitleImage) -> impl Iterator<Item = [u8; 4]> + '_ {
    image.rgba.chunks_exact(4).map(|p| [p[0], p[1], p[2], p[3]])
}

fn count(image: &SubtitleImage, colour: [u8; 4]) -> usize {
    pixels(image).filter(|p| *p == colour).count()
}

/// Rows of `image` (canvas coordinates) with a pixel of `colour`.
fn rows_with(image: &SubtitleImage, colour: [u8; 4]) -> Vec<i32> {
    let w = image.width as usize;
    (0..image.height as usize).filter(|&r| pixels(image).skip(r * w).take(w).any(|p| p == colour)).map(|r| image.y + r as i32).collect()
}

fn assert_drawn(what: &str, shown: &[Vec<SubtitleImage>]) {
    // The `lines:0` region's cue never comes up.
    assert_eq!(shown.len(), CUES.len() - 1, "{what}: shows");
    let image = |i: usize| -> &SubtitleImage {
        assert_eq!(shown[i].len(), 1, "{what}: cue {i}");
        &shown[i][0]
    };
    // `.loud`: red glyphs on its blue box, inside the cue's black box.
    let loud = image(0);
    assert!(count(loud, RED) > 20 && count(loud, BLUE) > 100, "{what}: loud");
    assert_eq!(count(loud, WHITE), 0, "{what}: loud has no white text");
    // `v[voice="Roger"]`: lime glyphs on the default box.
    let roger = image(1);
    assert!(count(roger, LIME) > 20 && count(roger, BOX) > 100, "{what}: voice");
    // `#shadowed`: no box; white glyphs over a red shadow 2 px down-right.
    let shade = image(2);
    assert_eq!(count(shade, BOX), 0, "{what}: shadowed has no box");
    assert!(count(shade, WHITE) > 20 && count(shade, RED) > 20, "{what}: shadow");
    let (white_rows, red_rows) = (rows_with(shade, WHITE), rows_with(shade, RED));
    assert_eq!(red_rows.last().unwrap() - white_rows.last().unwrap(), 2, "{what}: shadow offset");
    // Right-to-left `align:start size:50%`: the box is the left half and
    // the text sits at its right end (nine 8-pixel characters).
    let rtl = image(3);
    assert_eq!((rtl.x + rtl.width as i32, rtl.width), (160, 72), "{what}: right-to-left start");
    // Ruby on line 0: 10 pixels of annotation above a 20-pixel line box
    // (the cue's box, rows 10..30); the lime annotation in the top band.
    let ruby = image(4);
    assert_eq!(ruby.y + ruby.height as i32, 30, "{what}: ruby line");
    let boxed = rows_with(ruby, BOX);
    assert_eq!((boxed.first(), boxed.last()), (Some(&10), Some(&29)), "{what}: base line box");
    assert!(rows_with(ruby, LIME).iter().all(|&r| r < 10) && !rows_with(ruby, LIME).is_empty(), "{what}: annotation above");
}

#[test]
fn styled_cues_from_a_vtt_file() {
    let path = scratch().join("styled.vtt");
    std::fs::write(&path, vtt()).unwrap();
    assert_drawn("vtt", &shown(&path));
}

fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    [&(8 + body.len() as u32).to_be_bytes()[..], kind, body].concat()
}

fn full_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    mp4_box(kind, &[&[0u8; 4][..], body].concat())
}

/// An MP4 file with one `wvtt` track holding `CUES` (timescale 1000, a
/// sample per cue and per gap), its header in the `vttC` box.
fn mp4() -> Vec<u8> {
    let mut samples: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut at = 0;
    for (second, (id, settings, text)) in CUES.iter().enumerate() {
        let start = second as u32 * 1000;
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
fn styled_cues_from_mp4_wvtt() {
    let path = scratch().join("styled.mp4");
    std::fs::write(&path, mp4()).unwrap();
    let from_mp4 = shown(&path);
    assert_drawn("mp4", &from_mp4);
    let vtt_path = scratch().join("styled-for-mp4.vtt");
    std::fs::write(&vtt_path, vtt()).unwrap();
    let from_vtt = shown(&vtt_path);
    let digest = |shows: &[Vec<SubtitleImage>]| -> Vec<(i32, i32, u32, u32, Vec<u8>)> {
        shows.iter().flatten().map(|i| (i.x, i.y, i.width, i.height, i.rgba.clone())).collect()
    };
    assert!(digest(&from_mp4) == digest(&from_vtt), "MP4 and .vtt draw their cues alike");
}
