//! Real Player integration: compare the complete PGS show/replace/clear
//! sequence and canvases with FFmpeg. Logical media-time deadlines are
//! asserted exactly by engine::subtitle_tests with an injected clock.
//! This integration test does NOT establish a wall-clock presentation bound:
//! under shared-machine load a requested 5.9 ms wait took 65 ms and 100 ms
//! waits took up to 313 ms (observed while developing the timing test).

#[path = "support/bitmap.rs"]
mod bitmap;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bitmap::{Scratch, Show, ffmpeg, oracle, pgs_with_clears};
use parking_lot::Mutex;
use player::backend::{AudioSink, Backend, Clock, SubtitleImage, SubtitleSink, VideoSink};
use player::{Headless, Player, PlayerOptions};

#[derive(Default)]
struct Observation {
    clock: Option<Arc<dyn Clock>>,
    shows: Vec<Show>,
}

struct TimedHeadless {
    headless: Arc<Headless>,
    observation: Arc<Mutex<Observation>>,
}

impl Backend for TimedHeadless {
    fn audio(&self) -> Box<dyn AudioSink> {
        self.headless.audio()
    }

    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        self.observation.lock().clock = Some(clock.clone());
        self.headless.video(clock)
    }

    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        Box::new(TimedSubtitles(self.observation.clone()))
    }
}

struct TimedSubtitles(Arc<Mutex<Observation>>);

impl SubtitleSink for TimedSubtitles {
    fn show(&mut self, images: &[SubtitleImage], width: u32, height: u32) {
        let mut observation = self.0.lock();
        let at = observation.clock.as_ref().expect("video clock").now().expect("clock started");
        observation.shows.push(Show::from_images(at, width, height, images.iter().map(|image| (
            image.x, image.y, image.width, image.height, image.rgba.as_slice(),
        ))));
    }
}

#[test]
fn pgs_player_display_state_sequence_matches_ffmpeg() {
    let scratch = Scratch::new();
    let sup = scratch.file("display-states.sup");
    let subtitles = scratch.file("display-states.mks");
    let movie = scratch.file("display-states.mkv");
    pgs_with_clears(&sup);
    ffmpeg(&["-copyts", "-i", sup.to_str().unwrap(), "-map", "0:s", "-c:s", "copy", "-f", "matroska", subtitles.to_str().unwrap()]);
    ffmpeg(&[
        "-copyts", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=10:duration=3.8",
        "-i", subtitles.to_str().unwrap(), "-map", "0:v", "-map", "1:s",
        "-c:v", "libx264", "-preset", "ultrafast", "-c:s", "copy", movie.to_str().unwrap(),
    ]);
    let reference = oracle::ffmpeg_reference(&subtitles, 0);
    assert_eq!(oracle::ffprobe_subtitles(&movie, 0), reference.cues.iter().map(|cue| cue.sub.clone()).collect::<Vec<_>>());
    assert_eq!(reference.cues.len(), 5);
    assert_eq!(reference.cues.iter().map(|cue| cue.sub.num_rects).collect::<Vec<_>>(), [2, 2, 0, 2, 0]);

    let observation = Arc::new(Mutex::new(Observation::default()));
    let backend = Arc::new(TimedHeadless { headless: Headless::new(), observation: observation.clone() });
    let mut ctx = codecs::context();
    subs_bitmap::register(&mut ctx);
    let player = Player::open(
        movie.to_str().unwrap(), backend.clone(), Arc::new(ctx),
        PlayerOptions { subtitle: Some(1), realtime: true, ..PlayerOptions::default() }, |_| {},
    );
    let begun = Instant::now();
    loop {
        let state = player.state();
        if state.ended || state.error.is_some() {
            assert!(state.ended && state.error.is_none(), "playback: {state:?}");
            break;
        }
        assert!(begun.elapsed() < Duration::from_secs(15), "subtitle playback did not end: {state:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(player);
    assert!(!backend.headless.capture().video.is_empty(), "video pipeline not exercised");
    let observation = observation.lock();
    assert_eq!(observation.shows.len(), reference.cues.len(), "extra/missing shows or clears");
    for (index, show) in observation.shows.iter().enumerate() {
        let want = Duration::from_micros(reference.cues[index].sub.start_us() as u64);
        assert!(show.at >= want, "event {index}: early at {:?}, FFmpeg {want:?}", show.at);
        show.assert_canvas(&reference, index);
    }
}
