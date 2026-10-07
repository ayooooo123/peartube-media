//! Actual PS video + bitmap subtitle playback. Complete visible canvases,
//! including the first cue, must match FFmpeg (DVD) or native VLC (CVD/OGT).
//! spumux streams are encoder fixtures, not archived-disc coverage. These
//! nonrealtime captures prove geometry/sequence, not physical presentation time.

#[allow(dead_code)]
#[path = "support/bitmap.rs"]
mod bitmap;
use bitmap::oracle as support;
#[allow(dead_code)]
#[path = "../../subs-bitmap/tests/vlc_reference/mod.rs"]
mod vlc_reference;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxideav_core::{CodecParameters, Error, MediaType, Packet, RuntimeContext, StreamInfo, VideoFrame};
use parking_lot::{Condvar, Mutex};
use player::backend::{AudioSink, Backend, Clock, SinkError, SubtitleImage, SubtitleSink, VideoSink};
use player::{Headless, Player, PlayerOptions};

struct CanvasBackend {
    video: Arc<Headless>,
    shows: Arc<Mutex<Vec<bitmap::Show>>>,
    video_start: Option<Arc<VideoStart>>,
}
impl Backend for CanvasBackend {
    fn audio(&self) -> Box<dyn AudioSink> { self.video.audio() }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        let sink = self.video.video(clock);
        match &self.video_start {
            Some(start) => Box::new(HeldVideo { sink, start: start.clone() }),
            None => sink,
        }
    }
    fn subtitles(&self) -> Box<dyn SubtitleSink> { Box::new(Canvases(self.shows.clone())) }
}
struct Canvases(Arc<Mutex<Vec<bitmap::Show>>>);
impl SubtitleSink for Canvases {
    fn show(&mut self, images: &[SubtitleImage], width: u32, height: u32) {
        self.0.lock().push(bitmap::Show::from_images(Duration::ZERO, width, height,
            images.iter().map(|image| (image.x, image.y, image.width, image.height, image.rgba.as_slice()))));
    }
}

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    // Production implementations, with replacements before upstream factories.
    subs_bitmap::register(&mut ctx);
    codecs::register_all(&mut ctx);
    ctx
}
fn directory() -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("player-subtitle-canvas-{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}
fn streams_and_subtitles(path: &Path, format: &str) -> (Vec<StreamInfo>, Vec<Packet>) {
    let ctx = context();
    let mut demux = ctx.containers.open_demuxer(format, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap();
    let streams = demux.streams().to_vec();
    let sub = streams.iter().find(|s| s.params.media_type == MediaType::Subtitle).expect("subtitle at open").index;
    let mut packets = Vec::new();
    loop {
        match demux.next_packet() {
            Ok(packet) if packet.stream_index == sub => packets.push(packet),
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(error) => panic!("{}: {error}", path.display()),
        }
    }
    (streams, packets)
}
fn check_player(path: &Path, streams: &[StreamInfo], canvas_size: (usize, usize), canvases: &[Vec<u8>]) {
    let subtitle = streams.iter().find(|s| s.params.media_type == MediaType::Subtitle).unwrap();
    let headless = Headless::new();
    headless.set_active_streams(None, None, None, false);
    let backend = Arc::new(CanvasBackend { video: headless, shows: Arc::new(Mutex::new(Vec::new())), video_start: None });
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(context()),
        PlayerOptions { subtitle: Some(subtitle.index), realtime: false, ..PlayerOptions::default() }, |_| {});
    let begun = Instant::now();
    let state = loop {
        let state = player.state();
        assert!(state.error.is_none(), "{}: {state:?}", path.display());
        if state.ended { break state; }
        assert!(begun.elapsed() < Duration::from_secs(30), "{}: playback did not end: {state:?}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(player);
    assert!(state.video_size.is_some(), "{}: the real video decoder has not published authoritative dimensions", path.display());
    let capture = backend.video.capture();
    assert_eq!(capture.video.len(), 1, "one actual video stream");
    let video = &capture.video[0];
    let expected_video = refcheck::ffmpeg_video_md5s_with(path, 0, "yuv420p", &["-idct", "simple"]);
    assert!(!expected_video.is_empty(), "FFmpeg must decode video");
    assert_eq!(video.frame_md5, expected_video, "{}: every decoded video frame", path.display());
    assert_eq!(state.video_size, Some((video.width, video.height)), "published video dimensions");
    let shows = backend.shows.lock();
    let visible: Vec<_> = shows.iter().filter(|show| !show.blank).collect();
    assert_eq!(visible.len(), canvases.len(), "{}: every cue, including the first", path.display());
    assert!(!visible.is_empty(), "reference must include visible cues");
    let label = path.file_stem().unwrap().to_str().unwrap();
    let mut evidence = format!("input={}\nvideo={}x{}, {} exact FFmpeg frames\nsubtitle={} canvas={}x{} cues={}\n", path.display(), video.width, video.height, video.frame_md5.len(), subtitle.params.codec_id.as_str(), canvas_size.0, canvas_size.1, visible.len());
    for (index, (show, expected)) in visible.iter().zip(canvases).enumerate() {
        assert_eq!((show.width, show.height), canvas_size, "{label} cue {index}: authoritative canvas");
        let diff = support::canvas_diff(expected, &show.canvas, canvas_size.0, support::Match::Visible);
        assert!(diff.is_none(), "{label} cue {index}: full positioned RGBA canvas: {diff:?}");
        std::fs::write(directory().join(format!("{label}-{index}.rgba")), &show.canvas).unwrap();
        evidence.push_str(&format!("cue={index} rgba_md5={} reference_rgba_md5={} visible_pixels_exact=true\n", refcheck::md5_hex(&show.canvas), refcheck::md5_hex(expected)));
    }
    eprintln!("{evidence}artifact={}", directory().join(format!("{label}.txt")).display());
    std::fs::write(directory().join(format!("{label}.txt")), evidence).unwrap();
}

#[test]
fn cvd_and_svcd_player_use_video_pixels_from_the_first_cue() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../subs-bitmap/tests/data/spumux");
    for (cvd, name, dimensions) in [(true, "cvd-spumux.mpg", (352, 480)), (false, "svcd-spumux.mpg", (480, 480))] {
        let path = data.join(name);
        let (streams, packets) = streams_and_subtitles(&path, "mpeg");
        let video = streams.iter().find(|s| s.params.media_type == MediaType::Video).unwrap();
        assert_eq!((video.params.width, video.params.height), (Some(dimensions.0), Some(dimensions.1)));
        let subtitle = streams.iter().find(|s| s.params.media_type == MediaType::Subtitle).unwrap();
        assert_eq!((subtitle.params.width, subtitle.params.height), (None, None));
        let reference = vlc_reference::reference(cvd, name, &packets, dimensions.0 as usize, dimensions.1 as usize);
        assert_eq!(reference.len(), 3);
        check_player(&path, &streams, (dimensions.0 as usize, dimensions.1 as usize), &reference.into_iter().map(|cue| cue.canvas).collect::<Vec<_>>());
    }
}

#[test]
fn unknown_at_open_ps_video_publishes_canvas_before_first_dvd_cue() {
    // The real MPEG decoder sees the sequence header beyond the PS
    // demuxer's bounded parameter scan. It must publish that geometry;
    // the subtitle worker must not guess it or lose the first cue.
    let path = late_dvd_movie("late-video-size.mpg");
    let (streams, _) = streams_and_subtitles(&path, "mpeg");
    let video = streams.iter().find(|s| s.params.media_type == MediaType::Video).unwrap();
    assert_eq!((video.params.width, video.params.height), (None, None));
    // Independently establish that the same production decoder really
    // decodes this stream, rather than hiding a video failure behind the
    // absent size publication. FFmpeg supplies the frame-count/pixel oracle.
    let decoded = refcheck::decode(&path, &[codecs::register_all], MediaType::Video, 0);
    let hashes: Vec<_> = decoded.frames.iter().map(|frame| {
        let oxideav_core::Frame::Video(frame) = frame else { panic!("non-video frame") };
        refcheck::md5_hex(&refcheck::pack(frame, &[(720, 480), (360, 240), (360, 240)]))
    }).collect();
    let expected = refcheck::ffmpeg_video_md5s_with(&path, 0, "yuv420p", &["-idct", "simple"]);
    assert_eq!(expected.len(), 10);
    assert_eq!(hashes, expected, "real MPEG decoder before dimension publication");
    eprintln!("late-size MPEG-2 decoder: 10/10 complete frames equal FFmpeg; container dimensions absent");
    let reference = support::ffmpeg_reference(&path, 0);
    assert_eq!((reference.width, reference.height), (720, 480));
    assert_eq!(reference.cues.len(), 1);
    check_player(&path, &streams, (reference.width, reference.height), &reference.cues.into_iter().map(|cue| cue.canvas).collect::<Vec<_>>());
}

fn dvd_movie(label: &str, size: &str, codec: &str, format: &str) -> PathBuf {
    let path = directory().join(label);
    let idx = refcheck::fate("sub/vobsub.idx");
    bitmap::ffmpeg(&[
        "-f", "lavfi", "-i", &format!("color=c=0x204060:s={size}:r=5:d=2"),
        "-ss", "132.499", "-i", idx.to_str().unwrap(), "-map", "0:v", "-map", "1:s:0",
        "-t", "2", "-c:v", codec, "-bf", "0", "-g", "1", "-c:s", "copy", "-f", format, path.to_str().unwrap(),
    ]);
    path
}

fn late_dvd_movie(label: &str) -> PathBuf {
    let path = dvd_movie(label, "720x480", "mpeg2video", "vob");
    let original = std::fs::read(&path).unwrap();
    assert_eq!(&original[..4], &[0, 0, 1, 0xba]);
    let pack_end = 14 + usize::from(original[13] & 7);
    let mut bytes = original[..pack_end].to_vec();
    // MPEG-2 PES carrying a leading zero prefix before the first sequence.
    // 300 KB crosses PS's 256 KiB video-parameter scan, but not its 1 MiB
    // retained stream head. Both production MPEG and FFmpeg still decode
    // the unchanged sequence that follows; the subtitle payload is untouched.
    for _ in 0..5 {
        bytes.extend_from_slice(&[0, 0, 1, 0xe0]);
        bytes.extend_from_slice(&60_003u16.to_be_bytes());
        bytes.extend_from_slice(&[0x80, 0, 0]);
        bytes.resize(bytes.len() + 60_000, 0);
    }
    bytes.extend_from_slice(&original[pack_end..]);
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn ntsc_dvd_player_uses_video_canvas() {
    let path = dvd_movie("ntsc-dvd.mpg", "720x480", "mpeg2video", "vob");
    let (streams, _) = streams_and_subtitles(&path, "mpeg");
    let reference = support::ffmpeg_reference(&path, 0);
    assert_eq!((reference.width, reference.height), (720, 480));
    assert_eq!(reference.cues.len(), 1);
    check_player(&path, &streams, (reference.width, reference.height), &reference.cues.into_iter().map(|cue| cue.canvas).collect::<Vec<_>>());
}

#[test]
fn vobsub_explicit_size_wins_over_smaller_video() {
    let path = dvd_movie("explicit-vobsub.mkv", "352x240", "libx264", "matroska");
    let (streams, _) = streams_and_subtitles(&path, "matroska");
    let sub = streams.iter().find(|s| s.params.media_type == MediaType::Subtitle).unwrap();
    assert!(String::from_utf8_lossy(&sub.params.extradata).contains("size: 720x480"));
    let reference = support::ffmpeg_reference(&path, 0);
    assert_eq!((reference.width, reference.height), (720, 480));
    assert_eq!(reference.cues.len(), 1);
    check_player(&path, &streams, (reference.width, reference.height), &reference.cues.into_iter().map(|cue| cue.canvas).collect::<Vec<_>>());
}

/// Hold only decoder startup; the real video decoder, packets and output
/// are unchanged. This makes subtitle retirement race with unknown size
/// deterministically, instead of relying on which worker the OS runs first.
#[derive(Default)]
struct VideoStart {
    state: Mutex<(bool, bool)>, // entered, released
    changed: Condvar,
}
impl VideoStart {
    fn hold(&self) {
        let mut state = self.state.lock();
        state.0 = true;
        self.changed.notify_all();
        while !state.1 { self.changed.wait(&mut state); }
    }
    fn release(&self) {
        self.state.lock().1 = true;
        self.changed.notify_all();
    }
}
struct ReleaseVideo(Arc<VideoStart>);
impl Drop for ReleaseVideo {
    fn drop(&mut self) { self.0.release(); }
}
struct HeldVideo {
    sink: Box<dyn VideoSink>,
    start: Arc<VideoStart>,
}
impl VideoSink for HeldVideo {
    fn open_compressed(&mut self, params: &CodecParameters) -> bool {
        self.start.hold();
        self.sink.open_compressed(params)
    }
    fn open_frames(&mut self, params: &CodecParameters) -> Result<(), SinkError> { self.sink.open_frames(params) }
    fn push_packet(&mut self, packet: &Packet, pts: Duration, random_access: bool) -> Result<(), SinkError> {
        self.sink.push_packet(packet, pts, random_access)
    }
    fn push_frame(&mut self, frame: &VideoFrame, pts: Duration) -> Result<(), SinkError> { self.sink.push_frame(frame, pts) }
    fn frame_lead(&self) -> Duration { self.sink.frame_lead() }
    fn finish(&mut self) -> Result<(), SinkError> { self.sink.finish() }
    fn flush(&mut self) { self.sink.flush(); }
    fn set_playing(&mut self, playing: bool) { self.sink.set_playing(playing); }
}

#[test]
fn subtitle_selection_can_cancel_waiting_for_video_dimensions() {
    let path = late_dvd_movie("cancel-video-size.mpg");
    let (streams, _) = streams_and_subtitles(&path, "mpeg");
    let video = streams.iter().find(|s| s.params.media_type == MediaType::Video).unwrap();
    assert_eq!((video.params.width, video.params.height), (None, None));
    let subtitle = streams.iter().find(|s| s.params.media_type == MediaType::Subtitle).unwrap();
    let start = Arc::new(VideoStart::default());
    let backend = Arc::new(CanvasBackend {
        video: Headless::new(), shows: Arc::new(Mutex::new(Vec::new())), video_start: Some(start.clone()),
    });
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(context()),
        PlayerOptions { subtitle: Some(subtitle.index), realtime: false, ..PlayerOptions::default() }, |_| {});
    // Dropped before Player on a panic, so the real video thread can join.
    let release = ReleaseVideo(start.clone());
    let begun = Instant::now();
    while !start.state.lock().0 || player.state().subtitle != Some(subtitle.index) {
        assert!(begun.elapsed() < Duration::from_secs(10), "video startup: {:?}", player.state());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(player.state().video_size, None);
    player.select_subtitle(None);
    while player.state().subtitle.is_some() {
        assert!(begun.elapsed() < Duration::from_secs(10), "subtitle retirement waited on held video: {:?}", player.state());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(backend.shows.lock().is_empty(), "unknown size must not display a guessed first canvas");
    drop(release);
    drop(player);
}
