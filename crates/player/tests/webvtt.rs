//! The WebM subtitle track must resolve through the player's registry, retain
//! FFmpeg's text and cue timing, and reach the actual subtitle sink.
use std::{path::PathBuf, process::Command, sync::Arc, time::Duration};
use parking_lot::Mutex;
use oxideav_core::{Frame, MediaType};
use player::{Event, Headless, Player, PlayerOptions, TrackKind};
use player::backend::{AudioSink, Backend, Clock, SubtitleImage, SubtitleSink, VideoSink};

type ImageSignature = (i32, i32, u32, u32, String);
type RenderedCue = (u32, u32, Vec<ImageSignature>);

fn signature(image: &SubtitleImage) -> ImageSignature {
    (image.x, image.y, image.width, image.height, refcheck::md5_hex(&image.rgba))
}

struct CaptureBackend {
    media: Arc<Headless>,
    cues: Arc<Mutex<Vec<RenderedCue>>>,
}

impl Backend for CaptureBackend {
    fn audio(&self) -> Box<dyn AudioSink> { self.media.audio() }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> { self.media.video(clock) }
    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        Box::new(CaptureSink { cues: self.cues.clone() })
    }
}

struct CaptureSink {
    cues: Arc<Mutex<Vec<RenderedCue>>>,
}

impl SubtitleSink for CaptureSink {
    fn show(&mut self, images: &[SubtitleImage], width: u32, height: u32) {
        if !images.is_empty() {
            self.cues.lock().push((width, height, images.iter().map(signature).collect()));
        }
    }
}

#[test]
fn generated_webvtt_matches_ffmpeg_and_reaches_player_sink() {
    let root = std::env::var_os("PEARTUBE_CORPUS_DIR").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join("projects/peartube-media-corpus"));
    let path = root.join("vp9_opus_vtt.webm");
    let output = Command::new(refcheck::pinned_ffmpeg()).args(["-v", "error", "-nostdin", "-i"])
        .arg(&path).args(["-map", "0:s:0", "-c:s", "webvtt", "-f", "webvtt", "-"])
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let reference = oxideav_subtitle::webvtt::parse(&output.stdout).unwrap();
    assert_eq!(reference.cues.len(), 3);
    let decoded = refcheck::decode(&path, &[codecs::register_all], MediaType::Subtitle, 0);
    assert_eq!(decoded.frames.len(), reference.cues.len());
    for (frame, expected) in decoded.frames.iter().zip(&reference.cues) {
        let Frame::Subtitle(cue) = frame else { panic!("non-subtitle frame") };
        assert_eq!((cue.start_us, cue.end_us), (expected.start_us, expected.end_us));
        assert_eq!(oxideav_subtitle::srt::render_segments(&cue.segments), oxideav_subtitle::srt::render_segments(&expected.segments));
    }
    let media = Headless::new();
    media.set_active_streams(None, None, None, false);
    let backend = Arc::new(CaptureBackend { media, cues: Arc::new(Mutex::new(Vec::new())) });
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions { realtime: false, subtitle: Some(2), ..PlayerOptions::default() },
        move |event| { let _ = tx.send(event); });
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(Event::Ended) => break,
            Ok(Event::Error(error)) => panic!("{error}"),
            Ok(Event::Changed) => {},
            Err(error) => panic!("player failed to end: {error}: {:?}", player.state()),
        }
    }
    let state = player.state();
    assert!(state.error.is_none(), "{state:?}");
    let track = state.tracks.iter().find(|s| s.stream == 2).expect("subtitle track opened");
    assert_eq!(track.kind, TrackKind::Subtitle);
    assert_eq!(track.codec, "webvtt");
    drop(player);
    let shown = backend.cues.lock();
    assert_eq!(shown.len(), reference.cues.len());
    for ((width, height, actual), expected) in shown.iter().zip(&reference.cues) {
        let expected_image = player::subs::render_text_cue(expected, *width, *height);
        assert_eq!(*actual, vec![signature(&expected_image)], "player rendered different cue text");
    }
}
