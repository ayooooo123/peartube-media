//! Bitmap subtitle states across the playback's lifecycle, through the
//! real Player in realtime: the end of the media, selecting a track while
//! a cue is up, and seeking into a cue. FFmpeg decodes the same streams as
//! the canvas oracle.

#[allow(dead_code)]
#[path = "support/bitmap.rs"]
mod bitmap;

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use bitmap::{Scratch, Show, ffmpeg, oracle, pgs_states};
use oxideav_core::{CodecId, CodecInfo, CodecParameters, Decoder, Packet, RuntimeContext, VideoFrame};
use parking_lot::{Condvar, Mutex};
use player::backend::{AudioSink, Backend, Clock, SinkError, SubtitleImage, SubtitleSink, VideoSink};
use player::{Headless, Player, PlayerOptions, State};

#[derive(Default)]
struct Observation {
    clock: Option<Arc<dyn Clock>>,
    shows: Vec<Show>,
}

struct TimedHeadless {
    headless: Arc<Headless>,
    observation: Arc<Mutex<Observation>>,
    /// The video output says when the video pipeline drops it, as it ends.
    signal_video_end: bool,
}

impl Backend for TimedHeadless {
    fn audio(&self) -> Box<dyn AudioSink> {
        self.headless.audio()
    }

    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        self.observation.lock().clock = Some(clock.clone());
        let sink = self.headless.video(clock);
        if self.signal_video_end { Box::new(VideoEnd(sink)) } else { sink }
    }

    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        Box::new(TimedSubtitles(self.observation.clone()))
    }
}

/// Set when the video pipeline has dropped its output.
static VIDEO_ENDED: Mutex<bool> = Mutex::new(false);
static VIDEO_ENDED_CHANGED: Condvar = Condvar::new();

struct VideoEnd(Box<dyn VideoSink>);

impl VideoSink for VideoEnd {
    fn open_compressed(&mut self, params: &CodecParameters, ready: player::backend::PictureReady) -> bool { self.0.open_compressed(params, ready) }
    fn present_from(&mut self, start: Duration) { self.0.present_from(start); }
    fn push_packet(&mut self, packet: &Packet, pts: Duration, random_access: bool) -> Result<(), SinkError> {
        self.0.push_packet(packet, pts, random_access)
    }
    fn open_frames(&mut self, params: &CodecParameters) -> Result<(), SinkError> { self.0.open_frames(params) }
    fn push_frame(&mut self, frame: &VideoFrame, pts: Duration) -> Result<(), SinkError> { self.0.push_frame(frame, pts) }
    fn frame_lead(&self) -> Duration { self.0.frame_lead() }
    fn finish(&mut self) -> Result<(), SinkError> { self.0.finish() }
    fn flush(&mut self) { self.0.flush() }
    fn set_playing(&mut self, playing: bool) { self.0.set_playing(playing) }
}

impl Drop for VideoEnd {
    fn drop(&mut self) {
        *VIDEO_ENDED.lock() = true;
        VIDEO_ENDED_CHANGED.notify_all();
    }
}

static PLAIN: LazyLock<RuntimeContext> = LazyLock::new(codecs::context);

/// The production PGS decoder, opened only once the video pipeline has
/// ended (and its lane with it): a subtitle worker whose first turn comes
/// after the media it belongs to.
fn pgs_after_the_video(params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
    let mut ended = VIDEO_ENDED.lock();
    VIDEO_ENDED_CHANGED.wait_while_for(&mut ended, |ended| !*ended, Duration::from_secs(30));
    drop(ended);
    std::thread::sleep(Duration::from_millis(200));
    PLAIN.codecs.first_decoder(params)
}

/// The production registry with `pgs_after_the_video` first for PGS.
fn late_pgs_context() -> RuntimeContext {
    let id = CodecId::new("hdmv_pgs_subtitle");
    let caps = PLAIN.codecs.implementations(&id).iter().find(|i| i.make_decoder.is_some()).map(|i| i.caps.clone()).unwrap();
    let mut ctx = RuntimeContext::new();
    ctx.codecs.register(CodecInfo::new(id).capabilities(caps).decoder(pgs_after_the_video));
    codecs::register_all(&mut ctx);
    ctx
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
    open_in(path, subtitle, realtime, codecs::context(), false)
}

fn open_in(
    path: &std::path::Path, subtitle: Option<u32>, realtime: bool, ctx: RuntimeContext, signal_video_end: bool,
) -> (Player, Arc<TimedHeadless>, Arc<Mutex<Observation>>) {
    let observation = Arc::new(Mutex::new(Observation::default()));
    let headless = Headless::new();
    headless.set_active_streams(None, None, None, realtime);
    let backend = Arc::new(TimedHeadless { headless, observation: observation.clone(), signal_video_end });
    let player = Player::open(
        path.to_str().unwrap(), backend.clone(), Arc::new(ctx),
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

/// The subtitle worker of a video playback may first run after the video
/// has ended (here its decoder opens only then), never seeing the video
/// drain its lane. Without realtime the playback still ends with the
/// open-ended state down.
#[test]
fn a_subtitle_worker_starting_after_the_video_still_ends_cleared() {
    *VIDEO_ENDED.lock() = false;
    let scratch = Scratch::new();
    let (movie, reference) = open_ended_movie(&scratch);
    let (player, backend, observation) = open_in(&movie, Some(1), false, late_pgs_context(), true);
    wait_until(&player, Duration::from_secs(30), "playback end", |state| state.ended);
    assert!(*VIDEO_ENDED.lock(), "the video pipeline ended first");
    assert_eq!(backend.headless.capture().video[0].frame_md5.len(), 10, "every video frame");
    assert_ends_cleared(&observation, |shows| {
        assert_eq!(shows.len(), 2, "the state, then its clear");
        shows[0].assert_canvas(&reference, 0);
    });
    drop(player);
}

/// Selecting a PGS track never seeks: the track shows from its next display
/// set past what the demuxer had read ahead (up to two seconds) at the
/// switch, not the one up at the switch. Seeking into a cue shows it at
/// once. The track repeats a real FATE display set: visible from 0.2 s,
/// cleared at 1.5 s, visible from 4.5 s, cleared at 5.0 s, visible from
/// 5.5 s, cleared at 5.9 s.
#[test]
fn subtitle_switch_shows_the_next_state_and_seek_the_current_one() {
    let scratch = Scratch::new();
    let sup = scratch.file("switch.sup");
    let subtitles = scratch.file("switch.mks");
    let movie = scratch.file("switch.mkv");
    pgs_states(&sup, &[(200, true), (1500, false), (4500, true), (5000, false), (5500, true), (5900, false)]);
    ffmpeg(&["-copyts", "-i", sup.to_str().unwrap(), "-map", "0:s", "-c:s", "copy", "-f", "matroska", subtitles.to_str().unwrap()]);
    ffmpeg(&[
        "-copyts", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=10:duration=6.2",
        "-i", subtitles.to_str().unwrap(), "-map", "0:v", "-map", "1:s", "-map", "1:s",
        "-c:v", "libx264", "-preset", "ultrafast", "-c:s", "copy", movie.to_str().unwrap(),
    ]);
    let reference = oracle::ffmpeg_reference(&subtitles, 0);
    assert_eq!(reference.cues.iter().map(|cue| cue.sub.num_rects).collect::<Vec<_>>(), [2, 0, 2, 0, 2, 0]);

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
        assert!(begun.elapsed() < Duration::from_secs(10), "the selected track's next state never showed");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(at >= Duration::from_millis(4500), "a state showed at {at:?}: the switch must not seek back to the state up since 0.2 s");
    assert_eq!(oracle::canvas_diff(&reference.cues[2].canvas, &canvas, reference.width, oracle::Match::Visible), None);

    let seek_to = Duration::from_millis(5600);
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
    assert!(at < Duration::from_millis(5900), "the cue up since 5.5 s showed at {at:?} after seeking to {seek_to:?}");
    assert_eq!(oracle::canvas_diff(&reference.cues[4].canvas, &canvas, reference.width, oracle::Match::Visible), None);
    drop(player);
}
