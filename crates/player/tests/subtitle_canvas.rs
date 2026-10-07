//! Actual PS video + bitmap subtitle playback. Complete visible canvases,
//! including the first cue, must match FFmpeg (DVD) or native VLC (CVD/OGT);
//! playback must never stall behind its subtitle lane. spumux streams are
//! encoder fixtures, not archived-disc coverage. Nonrealtime captures prove
//! geometry/sequence, not physical presentation time.

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
use parking_lot::Mutex;
use player::backend::{AudioSink, Backend, Clock, SinkError, SubtitleImage, SubtitleSink, VideoSink};
use player::{Headless, Player, PlayerOptions};

struct CanvasBackend {
    video: Arc<Headless>,
    shows: Arc<Mutex<Vec<bitmap::Show>>>,
    /// Every presented frame's MD5, packed at this picture size.
    hashed: Option<((u32, u32), Arc<Mutex<Vec<String>>>)>,
}
impl Backend for CanvasBackend {
    fn audio(&self) -> Box<dyn AudioSink> { self.video.audio() }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        let sink = self.video.video(clock);
        match &self.hashed {
            Some((size, md5)) => Box::new(HashedVideo { sink, size: *size, md5: md5.clone() }),
            None => sink,
        }
    }
    fn subtitles(&self) -> Box<dyn SubtitleSink> { Box::new(Canvases(self.shows.clone())) }
}

/// Hashes each frame the engine presents, packed at the picture size FFmpeg
/// decodes. The headless sink packs at the size the container declared,
/// which these streams do not.
struct HashedVideo {
    sink: Box<dyn VideoSink>,
    size: (u32, u32),
    md5: Arc<Mutex<Vec<String>>>,
}
impl VideoSink for HashedVideo {
    fn open_compressed(&mut self, params: &CodecParameters) -> bool { self.sink.open_compressed(params) }
    fn push_packet(&mut self, packet: &Packet, pts: Duration, random_access: bool) -> Result<(), SinkError> {
        self.sink.push_packet(packet, pts, random_access)
    }
    fn open_frames(&mut self, params: &CodecParameters) -> Result<(), SinkError> { self.sink.open_frames(params) }
    fn push_frame(&mut self, frame: &VideoFrame, pts: Duration) -> Result<(), SinkError> {
        let (width, height) = (self.size.0 as usize, self.size.1 as usize);
        let packed = refcheck::pack(frame, &[(width, height), (width / 2, height / 2), (width / 2, height / 2)]);
        self.md5.lock().push(refcheck::md5_hex(&packed));
        self.sink.push_frame(frame, pts)
    }
    fn frame_lead(&self) -> Duration { self.sink.frame_lead() }
    fn finish(&mut self) -> Result<(), SinkError> { self.sink.finish() }
    fn flush(&mut self) { self.sink.flush(); }
    fn set_playing(&mut self, playing: bool) { self.sink.set_playing(playing); }
}
struct Canvases(Arc<Mutex<Vec<bitmap::Show>>>);
impl SubtitleSink for Canvases {
    fn show(&mut self, images: &[SubtitleImage], width: u32, height: u32) {
        self.0.lock().push(bitmap::Show::from_images(Duration::ZERO, width, height,
            images.iter().map(|image| (image.x, image.y, image.width, image.height, image.rgba.as_slice()))));
    }
}

/// The production registry: its bitmap subtitle decoders are subs-bitmap's.
fn context() -> RuntimeContext {
    codecs::context()
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
    let backend = Arc::new(CanvasBackend { video: headless, shows: Arc::new(Mutex::new(Vec::new())), hashed: None });
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
    assert!(state.video_size.is_some(), "{}: State::video_size unknown (only container dimensions at open set it)", path.display());
    let capture = backend.video.capture();
    assert_eq!(capture.video.len(), 1, "one actual video stream");
    let video = &capture.video[0];
    let expected_video = refcheck::ffmpeg_video_md5s_with(path, 0, "yuv420p", &["-idct", "simple"]);
    assert!(!expected_video.is_empty(), "FFmpeg must decode video");
    assert_eq!(video.frame_md5, expected_video, "{}: every decoded video frame", path.display());
    assert_eq!(state.video_size, Some((video.width, video.height)), "State::video_size equals the decoded frames'");
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
#[ignore = "needs a decoded-geometry producer: run_video_thread must publish Decoder::output_video_dimensions() (oxideav-core 96094a9; MPEG-1/2 in the pinned mpeg12video fork) into State::video_size before the subtitle decoder opens. State::video_size holds container dimensions at open only, so this DVD stream keeps the 720x576 fallback canvas where FFmpeg uses 720x480"]
fn unknown_at_open_ps_video_publishes_canvas_before_first_dvd_cue() {
    // The real MPEG decoder sees the sequence header beyond the PS
    // demuxer's bounded parameter scan. Once the video pipeline publishes
    // that geometry, the first cue must use it.
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
    dvd_movie_from(label, size, codec, format, &refcheck::fate("sub/vobsub.idx"), "132.499", "2")
}

/// `seconds` of `size` video with the cues of VobSub index `idx` from
/// `start` on, which must be an index entry's time (FFmpeg seeks the index
/// to the entry at or before it). FATE vobsub.idx has one cue at 132.499 s
/// and cues 2.6 s apart at 180.797 s and 183.433 s.
fn dvd_movie_from(label: &str, size: &str, codec: &str, format: &str, idx: &Path, start: &str, seconds: &str) -> PathBuf {
    let path = directory().join(label);
    bitmap::ffmpeg(&[
        "-f", "lavfi", "-i", &format!("color=c=0x204060:s={size}:r=5:d={seconds}"),
        "-ss", start, "-i", idx.to_str().unwrap(), "-map", "0:v", "-map", "1:s:0",
        "-t", seconds, "-c:v", codec, "-bf", "0", "-g", "1", "-c:s", "copy", "-f", format, path.to_str().unwrap(),
    ]);
    path
}

fn late_dvd_movie(label: &str) -> PathBuf {
    let path = dvd_movie(label, "720x480", "mpeg2video", "vob");
    delay_sequence_header(&path);
    path
}

/// Puts MPEG-2 PES carrying a leading zero prefix before the first
/// sequence header of the program stream at `path`. 300 KB crosses PS's
/// 256 KiB video-parameter scan, but not its 1 MiB retained stream head.
/// Both production MPEG and FFmpeg still decode the unchanged sequence
/// that follows; the subtitle payload is untouched.
fn delay_sequence_header(path: &Path) {
    let original = std::fs::read(path).unwrap();
    assert_eq!(&original[..4], &[0, 0, 1, 0xba]);
    let pack_end = 14 + usize::from(original[13] & 7);
    let mut bytes = original[..pack_end].to_vec();
    for _ in 0..5 {
        bytes.extend_from_slice(&[0, 0, 1, 0xe0]);
        bytes.extend_from_slice(&60_003u16.to_be_bytes());
        bytes.extend_from_slice(&[0x80, 0, 0]);
        bytes.resize(bytes.len() + 60_000, 0);
    }
    bytes.extend_from_slice(&original[pack_end..]);
    std::fs::write(path, bytes).unwrap();
}

/// Moves the first DVD subpicture (private stream 1, substream 0x20) of
/// the program stream at `path` `ticks` of 90 kHz later, in place.
fn delay_first_subpicture(path: &Path, ticks: u64) {
    let mut bytes = std::fs::read(path).unwrap();
    let at = (0..bytes.len() - 14)
        .find(|&i| {
            bytes[i..i + 4] == [0, 0, 1, 0xbd] && bytes[i + 7] & 0x80 != 0
                && bytes.get(i + 9 + usize::from(bytes[i + 8])) == Some(&0x20)
        })
        .expect("a DVD subpicture PES with a PTS")
        + 9;
    let b = &bytes[at..at + 5];
    let pts = (u64::from(b[0] >> 1) & 7) << 30 | u64::from(b[1]) << 22 | u64::from(b[2] >> 1) << 15
        | u64::from(b[3]) << 7 | u64::from(b[4] >> 1);
    let pts = (pts + ticks) & ((1 << 33) - 1);
    let marker = bytes[at] & 0xf0;
    bytes[at] = marker | ((pts >> 29) & 0x0e) as u8 | 1;
    bytes[at + 1] = (pts >> 22) as u8;
    bytes[at + 2] = ((pts >> 14) & 0xfe) as u8 | 1;
    bytes[at + 3] = (pts >> 7) as u8;
    bytes[at + 4] = ((pts << 1) & 0xfe) as u8 | 1;
    std::fs::write(path, bytes).unwrap();
}

/// One playback through the real Player with `subtitle` selected, if any.
struct Played {
    state: player::State,
    /// MD5 of every frame presented, packed at the picture size FFmpeg
    /// decodes.
    frames: Vec<String>,
    shows: Vec<bitmap::Show>,
}

fn play(path: &Path, subtitle: Option<u32>, picture: (u32, u32), realtime: bool, limit: Duration) -> Played {
    let headless = Headless::new();
    headless.set_active_streams(None, None, None, realtime);
    let frames = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(CanvasBackend {
        video: headless, shows: Arc::new(Mutex::new(Vec::new())), hashed: Some((picture, frames.clone())),
    });
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(context()),
        PlayerOptions { subtitle, realtime, ..PlayerOptions::default() }, |_| {});
    let begun = Instant::now();
    let state = loop {
        let state = player.state();
        assert!(state.error.is_none(), "{}: {state:?}", path.display());
        if state.ended { break state; }
        assert!(begun.elapsed() < limit, "{} (realtime {realtime}): playback did not end within {limit:?}: {state:?}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(player);
    assert_eq!(backend.video.capture().video.len(), 1, "one actual video stream");
    let shows = std::mem::take(&mut *backend.shows.lock());
    let frames = std::mem::take(&mut *frames.lock());
    Played { state, frames, shows }
}

/// Every decoded video frame, compared with FFmpeg's simple-IDCT decode.
fn assert_every_video_frame(path: &Path, played: &Played, realtime: bool) {
    let expected = refcheck::ffmpeg_video_md5s_with(path, 0, "yuv420p", &["-idct", "simple"]);
    assert!(!expected.is_empty(), "FFmpeg must decode video");
    assert_eq!(played.state.dropped_frames, 0, "{} (realtime {realtime}): late frames dropped", path.display());
    assert_eq!(played.frames, expected, "{} (realtime {realtime}): every decoded video frame", path.display());
}

/// MPEG-2 video in Matroska whose track declares a 0x0 picture, with
/// VobSub cues whose index has no `size:`: a hostile container leaves the
/// video size unknown at open (FFmpeg decodes it instead). H.264 does not
/// serve: the Matroska demuxer reads its size from the avcC record, and
/// FFmpeg's PS muxer writes no DVD subtitles its own demuxer reads back
/// next to H.264 in a VOB.
fn pixel_width_zero_movie(label: &str) -> PathBuf {
    let source = refcheck::fate("sub/vobsub.idx");
    let text = std::fs::read_to_string(&source).unwrap();
    let idx = directory().join(format!("{label}.idx"));
    std::fs::write(&idx, text.replacen("size: 720x480\n", "", 1)).unwrap();
    std::fs::copy(source.with_extension("sub"), idx.with_extension("sub")).unwrap();
    let path = dvd_movie_from(label, "720x480", "mpeg2video", "matroska", &idx, "180.797", "4");
    let mut bytes = std::fs::read(&path).unwrap();
    // PixelWidth (0xB0) 720 and PixelHeight (0xBA) 480, two-byte values.
    for element in [[0xB0, 0x82, 0x02, 0xD0], [0xBA, 0x82, 0x01, 0xE0]] {
        let at: Vec<_> = bytes.windows(4).enumerate().filter(|(_, w)| *w == element).map(|(i, _)| i).collect();
        assert_eq!(at.len(), 1, "{label}: one {element:02x?}");
        bytes[at[0] + 2..at[0] + 4].fill(0);
    }
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Video size unknown at open, DVD subtitles declaring none, and cues
/// 2.6 s apart: further apart than the subtitle lane's 2 s bound. The
/// MPEG-2 sequence header lies past PS's video-parameter scan, or the
/// Matroska track declares 0x0. The subtitle worker must keep draining
/// its lane: playback ends with every video frame equal to FFmpeg's and
/// both cues shown, realtime or not. Without an authoritative size the
/// canvas is the decoders' 720x576, as before video-sized canvases; the
/// regions keep FFmpeg's pixel positions inside it. In FFmpeg's H.264 VOB
/// (PS discovery parses no H.264 size) neither FFmpeg nor the PS demuxer
/// reads a subtitle packet: selected, that empty stream holds nothing up,
/// realtime or not, and changes none of the frames decoded; in realtime
/// every decoded frame is presented or counted late. Whether those frames
/// are FFmpeg's is `h264_vob_plays_every_ffmpeg_frame`.
#[test]
fn unknown_video_size_never_stalls_playback_behind_dvd_cues() {
    let idx = refcheck::fate("sub/vobsub.idx");
    let late = || {
        let path = dvd_movie_from("late-header-two-cues.vob", "720x480", "mpeg2video", "vob", &idx, "180.797", "4");
        delay_sequence_header(&path);
        path
    };
    for (path, format) in [(late(), "mpeg"), (pixel_width_zero_movie("pixel-width-zero-two-cues.mkv"), "matroska")] {
        let label = path.file_name().unwrap().to_str().unwrap().to_string();
        let (streams, _) = streams_and_subtitles(&path, format);
        let video = streams.iter().find(|s| s.params.media_type == MediaType::Video).unwrap();
        assert!(video.params.width.unwrap_or(0) == 0 || video.params.height.unwrap_or(0) == 0, "{label}: size unknown at open: {:?}",
            (video.params.width, video.params.height));
        let subtitle = streams.iter().find(|s| s.params.media_type == MediaType::Subtitle).unwrap();
        assert!(!String::from_utf8_lossy(&subtitle.params.extradata).contains("size:"), "{label}: no declared canvas");
        let reference = support::ffmpeg_reference(&path, 0);
        assert_eq!((reference.width, reference.height), (720, 480));
        let starts: Vec<_> = reference.cues.iter().map(|cue| cue.sub.start_us()).collect();
        assert_eq!(starts.len(), 2, "{label}: {starts:?}");
        assert!(starts[1] - starts[0] >= 2_000_000, "{label}: cues further apart than the lane bound: {starts:?}");
        for realtime in [false, true] {
            let played = play(&path, Some(subtitle.index), (720, 480), realtime, Duration::from_secs(30));
            assert_every_video_frame(&path, &played, realtime);
            let visible: Vec<_> = played.shows.iter().filter(|show| !show.blank).collect();
            assert_eq!(visible.len(), reference.cues.len(), "{label} (realtime {realtime}): every cue");
            for (index, (show, cue)) in visible.iter().zip(&reference.cues).enumerate() {
                assert_eq!((show.width, show.height), (720, 576), "{label} cue {index}: fallback canvas");
                let (top, bottom) = show.canvas.split_at(720 * 480 * 4);
                let diff = support::canvas_diff(&cue.canvas, top, 720, support::Match::Visible);
                assert!(diff.is_none(), "{label} cue {index}: FFmpeg's pixels at FFmpeg's positions: {diff:?}");
                assert!(bottom.iter().all(|&byte| byte == 0), "{label} cue {index}: nothing below the video rows");
            }
            eprintln!("{label} realtime={realtime}: ended, {} exact FFmpeg frames, {} cues on the 720x576 fallback canvas",
                played.frames.len(), visible.len());
        }
    }
    let path = h264_vob("h264-two-cues.vob");
    let (streams, packets) = streams_and_subtitles(&path, "mpeg");
    let video = streams.iter().find(|s| s.params.media_type == MediaType::Video).unwrap();
    assert_eq!((video.params.codec_id.as_str(), video.params.width, video.params.height), ("h264", None, None));
    assert!(packets.is_empty() && support::ffprobe_packets(&path, 0).is_empty(), "no subtitle packet, as FFmpeg reads none");
    let subtitle = streams.iter().find(|s| s.params.media_type == MediaType::Subtitle).unwrap();
    let alone = play(&path, None, (720, 480), false, Duration::from_secs(30));
    assert!(!alone.frames.is_empty());
    for realtime in [false, true] {
        let played = play(&path, Some(subtitle.index), (720, 480), realtime, Duration::from_secs(30));
        assert!(played.shows.iter().all(|show| show.blank), "h264-two-cues.vob (realtime {realtime}): nothing shown");
        if realtime {
            assert_eq!(played.frames.len() + played.state.dropped_frames as usize, alone.frames.len(),
                "h264-two-cues.vob: every decoded frame presented or counted late");
        } else {
            assert_eq!(played.frames, alone.frames, "h264-two-cues.vob: the frames decoded without a subtitle");
        }
        eprintln!("h264-two-cues.vob realtime={realtime}: ended, {} frames ({} without a subtitle)", played.frames.len(), alone.frames.len());
    }
}

/// FFmpeg's VOB of four seconds of 5 fps H.264 with the FATE VobSub track
/// from 180.797 s.
fn h264_vob(label: &str) -> PathBuf {
    dvd_movie_from(label, "720x480", "libx264", "vob", &refcheck::fate("sub/vobsub.idx"), "180.797", "4")
}

/// Packet P1-1's acceptance for the H.264 VOB: every video frame FFmpeg
/// decodes, with its subtitle stream selected.
#[test]
fn h264_vob_plays_every_ffmpeg_frame() {
    let path = h264_vob("h264-every-frame.vob");
    let (streams, _) = streams_and_subtitles(&path, "mpeg");
    let subtitle = streams.iter().find(|s| s.params.media_type == MediaType::Subtitle).unwrap();
    let played = play(&path, Some(subtitle.index), (720, 480), false, Duration::from_secs(30));
    assert_every_video_frame(&path, &played, false);
}

/// FFmpeg reads `size:` with sscanf("%dx%d") (dvdsubdec.c): a line without
/// a height sets nothing, so the canvas is the video's. The engine must
/// not take such a line for a declared size.
#[test]
fn index_size_line_without_height_uses_the_video_canvas() {
    let source = refcheck::fate("sub/vobsub.idx");
    let text = std::fs::read_to_string(&source).unwrap();
    assert!(text.contains("size: 720x480\n"));
    let idx = directory().join("size-without-height.idx");
    std::fs::write(&idx, text.replace("size: 720x480\n", "size: 720\n")).unwrap();
    std::fs::copy(source.with_extension("sub"), idx.with_extension("sub")).unwrap();
    let path = dvd_movie_from("size-without-height.mkv", "720x480", "mpeg2video", "matroska", &idx, "132.499", "2");
    let (streams, _) = streams_and_subtitles(&path, "matroska");
    let sub = streams.iter().find(|s| s.params.media_type == MediaType::Subtitle).unwrap();
    assert!(String::from_utf8_lossy(&sub.params.extradata).contains("size: 720\n"));
    let reference = support::ffmpeg_reference(&path, 0);
    assert_eq!((reference.width, reference.height), (720, 480), "FFmpeg falls back to the video size");
    assert_eq!(reference.cues.len(), 1);
    check_player(&path, &streams, (reference.width, reference.height), &reference.cues.into_iter().map(|cue| cue.canvas).collect::<Vec<_>>());
}

/// One DVD subpicture stamped ten hours past the rest of the file (hostile,
/// or a PTS jump) must not stop the subtitle lane draining: video plays to
/// its end, every later cue shows at its own time, and the far one never
/// does.
#[test]
fn far_future_dvd_cue_never_starves_video() {
    let idx = refcheck::fate("sub/vobsub.idx");
    let path = dvd_movie_from("far-future-cue.vob", "720x480", "mpeg2video", "vob", &idx, "180.797", "10");
    let reference = support::ffmpeg_reference(&path, 0);
    let later: Vec<_> = reference.cues[1..].iter().filter(|cue| cue.sub.num_rects > 0).collect();
    assert!(later.len() >= 2, "cues after the moved one: {}", later.len());
    delay_first_subpicture(&path, 10 * 3600 * 90_000);
    let (streams, _) = streams_and_subtitles(&path, "mpeg");
    let subtitle = streams.iter().find(|s| s.params.media_type == MediaType::Subtitle).unwrap();
    let played = play(&path, Some(subtitle.index), (720, 480), true, Duration::from_secs(40));
    assert_every_video_frame(&path, &played, true);
    let visible: Vec<_> = played.shows.iter().filter(|show| !show.blank).collect();
    assert_eq!(visible.len(), later.len(), "every later cue, never the far one");
    for (index, (show, cue)) in visible.iter().zip(&later).enumerate() {
        assert_eq!((show.width, show.height), (720, 480));
        let diff = support::canvas_diff(&cue.canvas, &show.canvas, 720, support::Match::Visible);
        assert!(diff.is_none(), "later cue {index}: {diff:?}");
    }
    assert!(played.shows.last().is_some_and(|show| show.blank), "the overlay ends cleared");
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
