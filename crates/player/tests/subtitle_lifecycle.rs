//! Bitmap subtitle states across the playback's lifecycle, through the
//! real Player in realtime: the end of the media, selecting a track while
//! a cue is up, and seeking into a cue. FFmpeg decodes the same streams as
//! the canvas oracle.

#[allow(dead_code)]
#[path = "support/bitmap.rs"]
mod bitmap;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bitmap::{Scratch, Show, ffmpeg, oracle, pgs_states, pgs_with_clears};
use parking_lot::Mutex;
use player::backend::{AudioSink, Backend, Clock, SubtitleImage, SubtitleSink, VideoSink};
use player::{Headless, Player, PlayerOptions, State};

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

/// Stamps every show/clear with the playback clock.
struct TimedSubtitles(Arc<Mutex<Observation>>);

impl SubtitleSink for TimedSubtitles {
    fn show(&mut self, images: &[SubtitleImage], width: u32, height: u32) {
        let mut observation = self.0.lock();
        let at = observation.clock.as_ref().and_then(|clock| clock.now()).unwrap_or_default();
        observation.shows.push(Show::from_images(at, width, height, images.iter().map(|image| (
            image.x, image.y, image.width, image.height, image.rgba.as_slice(),
        ))));
    }
}

fn open(path: &std::path::Path, subtitle: Option<u32>) -> (Player, Arc<TimedHeadless>, Arc<Mutex<Observation>>) {
    open_with(path, subtitle, true)
}

fn open_with(path: &std::path::Path, subtitle: Option<u32>, realtime: bool) -> (Player, Arc<TimedHeadless>, Arc<Mutex<Observation>>) {
    let observation = Arc::new(Mutex::new(Observation::default()));
    let headless = Headless::new();
    headless.set_active_streams(None, None, None, realtime);
    let backend = Arc::new(TimedHeadless { headless, observation: observation.clone() });
    let player = Player::open(
        path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions { subtitle, realtime, ..PlayerOptions::default() }, |_| {},
    );
    (player, backend, observation)
}

fn wait_until(player: &Player, limit: Duration, what: &str, done: impl Fn(&State) -> bool) -> State {
    let begun = Instant::now();
    loop {
        let state = player.state();
        assert!(state.error.is_none(), "{what}: {state:?}");
        if done(&state) {
            return state;
        }
        assert!(begun.elapsed() < limit, "{what}: not within {limit:?}: {state:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The last DVB display state of a real FATE prefix is visible with a 15 s
/// page timeout, over 1 s of video: playback ends when the video does, the
/// overlay cleared, instead of running the clock on for the timeout.
#[test]
fn ended_does_not_wait_for_a_subtitle_timeout_past_the_media() {
    let scratch = Scratch::new();
    let source = refcheck::fate("sub/dvbsubtest_filter.ts");
    let first_visible = oracle::ffprobe_subtitles(&source, 0).iter().position(|cue| cue.num_rects > 0).unwrap();
    let count = (first_visible + 1).to_string();
    let subtitles = scratch.file("timeout-past-video.mks");
    let movie = scratch.file("timeout-past-video.mkv");
    // The prefix's packets, 50 ms apart, so the visible state starts while
    // the video plays.
    ffmpeg(&[
        "-i", source.to_str().unwrap(), "-map", "0:s:0", "-c:s", "copy", "-frames:s", &count,
        "-bsf:s", "setts=ts=N*4500", "-f", "matroska", subtitles.to_str().unwrap(),
    ]);
    ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=10:duration=1", "-i", subtitles.to_str().unwrap(),
        "-map", "0:v", "-map", "1:s", "-c:v", "libx264", "-preset", "ultrafast", "-c:s", "copy", movie.to_str().unwrap(),
    ]);
    let reference = oracle::ffmpeg_reference(&subtitles, 0);
    assert_eq!(oracle::ffprobe_subtitles(&movie, 0), reference.cues.iter().map(|cue| cue.sub.clone()).collect::<Vec<_>>());
    let last = reference.cues.last().unwrap();
    assert!(last.sub.num_rects > 0, "the last state is visible");
    let video_end = Duration::from_secs(1);
    assert!(last.sub.start_us() < 1_000_000 && last.sub.end_us().unwrap() > 15_000_000, "{:?}", last.sub);

    let (player, backend, observation) = open(&movie, Some(1));
    let state = wait_until(&player, Duration::from_secs(12), "playback end", |state| state.ended);
    assert!(
        state.position <= video_end + Duration::from_millis(500),
        "ended {:?} into the media; the video ends at {video_end:?}", state.position,
    );
    let frames = backend.headless.capture().video[0].frame_md5.len();
    assert_eq!(frames + state.dropped_frames as usize, 10, "every video frame presented or counted late");
    // Before the Player is dropped (stopping clears the screen too).
    assert_ends_cleared(&observation, |shows| {
        let shown = shows.iter().rposition(|show| !show.blank).expect("the visible state was shown");
        shows[shown].assert_canvas(&reference, reference.cues.len() - 1);
    });
    drop(player);
}

/// At Ended, the last subtitle sink call was a clear; `check` sees every
/// show first.
fn assert_ends_cleared(observation: &Mutex<Observation>, check: impl FnOnce(&[Show])) {
    let observation = observation.lock();
    check(&observation.shows);
    assert!(observation.shows.last().is_some_and(|show| show.blank), "the overlay is cleared at Ended");
}

/// One PGS display set, visible from 0.2 s, that nothing clears, over 1 s
/// of video; FFmpeg's decode of the subtitles alone.
fn open_ended_movie(scratch: &Scratch) -> (std::path::PathBuf, oracle::Reference) {
    let sup = scratch.file("open-ended.sup");
    let subtitles = scratch.file("open-ended.mks");
    let movie = scratch.file("open-ended.mkv");
    pgs_states(&sup, &[(200, true)]);
    ffmpeg(&["-copyts", "-i", sup.to_str().unwrap(), "-map", "0:s", "-c:s", "copy", "-f", "matroska", subtitles.to_str().unwrap()]);
    ffmpeg(&[
        "-copyts", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=10:duration=1",
        "-i", subtitles.to_str().unwrap(), "-map", "0:v", "-map", "1:s",
        "-c:v", "libx264", "-preset", "ultrafast", "-c:s", "copy", movie.to_str().unwrap(),
    ]);
    let reference = oracle::ffmpeg_reference(&subtitles, 0);
    assert_eq!(reference.cues.len(), 1);
    assert!(reference.cues[0].sub.num_rects > 0 && reference.cues[0].sub.end_us().is_none(), "{:?}", reference.cues[0].sub);
    (movie, reference)
}

/// A PGS state without an end (no later display set clears it) over 1 s of
/// video: it stays up while the video plays and comes down when the
/// playback ends, not after the Player is dropped.
#[test]
fn open_ended_pgs_state_is_cleared_at_ended() {
    let scratch = Scratch::new();
    let (movie, reference) = open_ended_movie(&scratch);
    let (player, _backend, observation) = open(&movie, Some(1));
    let state = wait_until(&player, Duration::from_secs(10), "playback end", |state| state.ended);
    assert!(state.position <= Duration::from_millis(1500), "ended {:?} into 1 s of media", state.position);
    assert_ends_cleared(&observation, |shows| {
        assert_eq!(shows.len(), 2, "the state, then its clear");
        shows[0].assert_canvas(&reference, 0);
    });
    drop(player);
}

/// Without realtime the subtitles behind the video still end cleared: the
/// open-ended state is shown as it decodes and is down at Ended.
#[test]
fn open_ended_pgs_state_is_cleared_at_ended_without_realtime() {
    let scratch = Scratch::new();
    let (movie, reference) = open_ended_movie(&scratch);
    let (player, backend, observation) = open_with(&movie, Some(1), false);
    wait_until(&player, Duration::from_secs(20), "playback end", |state| state.ended);
    assert_eq!(backend.headless.capture().video[0].frame_md5.len(), 10, "every video frame");
    assert_ends_cleared(&observation, |shows| {
        assert_eq!(shows.len(), 2, "the state, then its clear");
        shows[0].assert_canvas(&reference, 0);
    });
    drop(player);
}

/// Selecting a PGS track while one of its cues is up shows that cue at
/// once, as an audio switch resumes at the clock; seeking into a cue shows
/// it at once too. The track is a real FATE display set (`pgs_with_clears`:
/// visible from 0.2 s until 2.5 s, from 3.0 s until 3.4 s).
#[test]
fn subtitle_switch_and_seek_show_the_current_state_at_once() {
    let scratch = Scratch::new();
    let sup = scratch.file("switch.sup");
    let subtitles = scratch.file("switch.mks");
    let movie = scratch.file("switch.mkv");
    pgs_with_clears(&sup);
    ffmpeg(&["-copyts", "-i", sup.to_str().unwrap(), "-map", "0:s", "-c:s", "copy", "-f", "matroska", subtitles.to_str().unwrap()]);
    ffmpeg(&[
        "-copyts", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=10:duration=3.8",
        "-i", subtitles.to_str().unwrap(), "-map", "0:v", "-map", "1:s", "-map", "1:s",
        "-c:v", "libx264", "-preset", "ultrafast", "-c:s", "copy", movie.to_str().unwrap(),
    ]);
    let reference = oracle::ffmpeg_reference(&subtitles, 0);
    assert_eq!(reference.cues.iter().map(|cue| cue.sub.num_rects).collect::<Vec<_>>(), [2, 2, 0, 2, 0]);

    let (player, _backend, observation) = open(&movie, None);
    let switch_at = Duration::from_millis(1000);
    wait_until(&player, Duration::from_secs(10), "media time 1 s", |state| state.position >= switch_at);
    player.select_subtitle(Some(2));
    let shown = |after: usize| {
        let observation = observation.lock();
        observation.shows.iter().skip(after).find(|show| !show.blank).map(|show| (show.at, show.canvas.clone()))
    };
    let begun = Instant::now();
    let (at, canvas) = loop {
        if let Some(show) = shown(0) {
            break show;
        }
        assert!(begun.elapsed() < Duration::from_secs(5), "the selected track's current cue never showed");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(at < Duration::from_millis(2000), "the cue up since 0.2 s showed at {at:?}, not at once after the {switch_at:?} switch");
    assert_eq!(oracle::canvas_diff(&reference.cues[0].canvas, &canvas, reference.width, oracle::Match::Visible), None);

    let seek_to = Duration::from_millis(3100);
    let before = observation.lock().shows.len();
    player.seek(seek_to);
    let begun = Instant::now();
    let (at, canvas) = loop {
        if let Some(show) = shown(before) {
            break show;
        }
        assert!(begun.elapsed() < Duration::from_secs(5), "the cue up at the seek target never showed");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(at < Duration::from_millis(3400), "the cue up since 3.0 s showed at {at:?} after seeking to {seek_to:?}");
    assert_eq!(oracle::canvas_diff(&reference.cues[3].canvas, &canvas, reference.width, oracle::Match::Visible), None);
    drop(player);
}
