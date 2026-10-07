//! Text subtitles beside audio and video through the real Player in
//! realtime: they never hold the playback back, never flush or skip its
//! audio and video, and never crowd its memory.

#[allow(dead_code)]
#[path = "support/bitmap.rs"]
mod bitmap;
#[allow(dead_code)]
#[path = "support/scripted.rs"]
mod scripted;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bitmap::{Scratch, ffmpeg};
use oxideav_core::{CodecParameters, Packet, VideoFrame};
use parking_lot::Mutex;
use player::backend::{AudioSink, Backend, Clock, SinkError, SubtitleImage, SubtitleSink, VideoSink};
use player::{Headless, Player, PlayerOptions, State};
use scripted::Mode;

/// One subtitle sink call: when on the playback clock, how many images and
/// how many RGBA bytes.
#[derive(Clone, Copy, Debug)]
struct Shown {
    at: Duration,
    images: usize,
    bytes: usize,
}

#[derive(Default)]
struct Watch {
    clock: Mutex<Option<Arc<dyn Clock>>>,
    shows: Mutex<Vec<Shown>>,
    audio_flushes: AtomicUsize,
    video_flushes: AtomicUsize,
}

/// The headless backend, watched: subtitle calls stamped with the playback
/// clock, and every audio or video flush (a seek) counted.
struct Watched {
    headless: Arc<Headless>,
    watch: Arc<Watch>,
}

impl Backend for Watched {
    fn audio(&self) -> Box<dyn AudioSink> {
        let sink = self.headless.audio();
        self.watch.clock.lock().get_or_insert_with(|| sink.clock());
        Box::new(WatchedAudio(sink, self.watch.clone()))
    }

    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        *self.watch.clock.lock() = Some(clock.clone());
        Box::new(WatchedVideo(self.headless.video(clock), self.watch.clone()))
    }

    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        Box::new(WatchedSubtitles(self.watch.clone()))
    }
}

struct WatchedAudio(Box<dyn AudioSink>, Arc<Watch>);

impl AudioSink for WatchedAudio {
    fn open(&mut self, sample_rate: u32, channels: u16) -> Result<(), SinkError> { self.0.open(sample_rate, channels) }
    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError> { self.0.write(pcm, pts) }
    fn play(&mut self) { self.0.play() }
    fn pause(&mut self) { self.0.pause() }
    fn flush(&mut self) {
        self.1.audio_flushes.fetch_add(1, Ordering::SeqCst);
        self.0.flush()
    }
    fn clock(&self) -> Arc<dyn Clock> { self.0.clock() }
}

struct WatchedVideo(Box<dyn VideoSink>, Arc<Watch>);

impl VideoSink for WatchedVideo {
    fn open_compressed(&mut self, params: &CodecParameters) -> bool { self.0.open_compressed(params) }
    fn push_packet(&mut self, packet: &Packet, pts: Duration, random_access: bool) -> Result<(), SinkError> {
        self.0.push_packet(packet, pts, random_access)
    }
    fn open_frames(&mut self, params: &CodecParameters) -> Result<(), SinkError> { self.0.open_frames(params) }
    fn push_frame(&mut self, frame: &VideoFrame, pts: Duration) -> Result<(), SinkError> { self.0.push_frame(frame, pts) }
    fn frame_lead(&self) -> Duration { self.0.frame_lead() }
    fn finish(&mut self) -> Result<(), SinkError> { self.0.finish() }
    fn flush(&mut self) {
        self.1.video_flushes.fetch_add(1, Ordering::SeqCst);
        self.0.flush()
    }
    fn set_playing(&mut self, playing: bool) { self.0.set_playing(playing) }
}

struct WatchedSubtitles(Arc<Watch>);

impl SubtitleSink for WatchedSubtitles {
    fn show(&mut self, images: &[SubtitleImage], _width: u32, _height: u32) {
        let at = self.0.clock.lock().as_ref().and_then(|clock| clock.now()).unwrap_or_default();
        let bytes = images.iter().map(|image| image.rgba.len()).sum();
        self.0.shows.lock().push(Shown { at, images: images.len(), bytes });
    }
}

fn play(path: &Path, subtitle: impl Into<Option<u32>>) -> (Player, Arc<Watched>) {
    let headless = Headless::new();
    headless.set_active_streams(None, None, None, true);
    let backend = Arc::new(Watched { headless, watch: Arc::new(Watch::default()) });
    let player = Player::open(
        path.to_str().unwrap(), backend.clone(), Arc::new(scripted::context()),
        PlayerOptions { subtitle: subtitle.into(), realtime: true, ..PlayerOptions::default() }, |_| {},
    );
    (player, backend)
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

/// SubRip text with one cue per `(start_ms, end_ms)`.
fn srt(path: &Path, cues: &[(u32, u32)]) {
    let stamp = |ms: u32| format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    let text: String = cues.iter().enumerate()
        .map(|(index, &(start, end))| format!("{}\n{} --> {}\nline {index}\n\n", index + 1, stamp(start), stamp(end)))
        .collect();
    std::fs::write(path, text).unwrap();
}

/// A Matroska file of `seconds`: optional H.264 video (`video`: its lavfi
/// source and GOP length), then 48 kHz stereo PCM audio, then one WebVTT
/// track per SubRip file of `subtitles`, in that stream order. (The text
/// decoders take Matroska's WebVTT blocks; its SubRip and ASS blocks they
/// reject.)
fn movie(path: &Path, seconds: u32, video: Option<(&str, &str)>, subtitles: &[&Path]) {
    let duration = seconds.to_string();
    let sine = format!("sine=frequency=440:sample_rate=48000:duration={seconds}");
    let mut args: Vec<String> = Vec::new();
    if let Some((source, _)) = video {
        args.extend(["-f".into(), "lavfi".into(), "-i".into(), format!("{source}:duration={seconds}")]);
    }
    args.extend(["-f".into(), "lavfi".into(), "-i".into(), sine]);
    for subtitle in subtitles {
        args.extend(["-i".into(), subtitle.to_str().unwrap().into()]);
    }
    let inputs = usize::from(video.is_some()) + 1 + subtitles.len();
    for input in 0..inputs {
        args.extend(["-map".into(), input.to_string()]);
    }
    if let Some((_, gop)) = video {
        args.extend(["-c:v", "libx264", "-preset", "ultrafast", "-g", gop, "-pix_fmt", "yuv420p"].map(String::from));
    }
    args.extend(["-c:a", "pcm_s16le", "-ac", "2", "-c:s", "webvtt", "-t", &duration].map(String::from));
    args.push(path.to_str().unwrap().into());
    ffmpeg(&args.iter().map(String::as_str).collect::<Vec<_>>());
}

/// What the playback put out, against FFmpeg's decode of `source`: every
/// PCM sample, and every video frame presented or counted late.
fn assert_audio_and_video_complete(source: &Path, backend: &Watched, state: &State, video_frames: Option<usize>, what: &str) {
    let capture = backend.headless.capture();
    let expected = refcheck::ffmpeg_audio_f32(source, 0);
    let pcm = &capture.audio[0].pcm;
    assert_eq!(pcm.len(), expected.len(), "{what}: every PCM sample");
    assert!(pcm.iter().zip(&expected).all(|(a, b)| a.to_bits() == b.to_bits()), "{what}: PCM equal to FFmpeg's");
    if let Some(frames) = video_frames {
        let presented = capture.video[0].frame_md5.len();
        assert_eq!(presented + state.dropped_frames as usize, frames, "{what}: every video frame presented or counted late");
    }
}

/// Audio queued through 0.5 s, then subtitle cues at 1, 3 and 5 s, then
/// the rest of the media. Waiting for its 1 s cue to come up before taking
/// more, the subtitle worker would leave the 3 s and 5 s packets filling
/// its lane: the demuxer could not reach the audio past 0.5 s, the audio
/// clock would stop there, and the 1 s cue would never come up. Behind
/// audio, with and without video, the playback must play out every sample
/// and frame and show each cue from its start.
#[test]
fn subtitles_demuxed_ahead_of_the_audio_never_stall_it() {
    for video in [None, Some(("testsrc2=size=160x90:rate=10", "10"))] {
        let what = if video.is_some() { "audio and video" } else { "audio" };
        let scratch = Scratch::new();
        let cues = scratch.file("cues.srt");
        srt(&cues, &[(1000, 1500), (3000, 3500), (5000, 5500)]);
        let mkv = scratch.file("media.mkv");
        movie(&mkv, 6, video, &[&cues]);
        let path = scratch.file("subtitles-early.ptscript");
        scripted::write(&path, &mkv, Mode::SubtitlesEarly);
        let subtitle = if video.is_some() { 2 } else { 1 };
        let (player, backend) = play(&path, subtitle);
        let state = wait_until(&player, Duration::from_secs(30), &format!("{what}: playback end"), |state| state.ended);
        assert_audio_and_video_complete(&mkv, &backend, &state, video.map(|_| 60), what);
        let shows = backend.watch.shows.lock().clone();
        let visible: Vec<_> = shows.iter().filter(|show| show.images > 0).collect();
        assert_eq!(visible.len(), 3, "{what}: every cue: {shows:?}");
        for (show, start) in visible.iter().zip([1, 3, 5]) {
            assert!(show.at >= Duration::from_secs(start), "{what}: the cue from {start} s showed at {:?}", show.at);
        }
        assert!(shows.last().is_some_and(|show| show.images == 0), "{what}: cleared at the end: {shows:?}");
        drop(player);
    }
}

/// Switching subtitle tracks re-reads from the clock's position when the
/// demuxer can seek. When it cannot (`seek_to` unsupported) or the seek
/// fails, nothing of the playback may change: no audio or video is
/// flushed or skipped, the inter-coded video decodes on unbroken (one
/// keyframe in four seconds), and the new track shows its cues from where
/// the demuxer reads.
#[test]
fn a_subtitle_switch_that_cannot_seek_leaves_audio_and_video_untouched() {
    for mode in [Mode::Unseekable, Mode::SeekFails] {
        let scratch = Scratch::new();
        let first = scratch.file("first.srt");
        let second = scratch.file("second.srt");
        srt(&first, &[(200, 800), (2600, 3200)]);
        srt(&second, &[(300, 3500), (3600, 3900)]);
        let mkv = scratch.file("two-tracks.mkv");
        movie(&mkv, 4, Some(("testsrc2=size=160x90:rate=10", "1000")), &[&first, &second]);
        let path = scratch.file("two-tracks.ptscript");
        scripted::write(&path, &mkv, mode);
        let (player, backend) = play(&path, 2);
        wait_until(&player, Duration::from_secs(10), &format!("{mode:?}: media time 1 s"), |state| {
            state.position >= Duration::from_secs(1)
        });
        let watch = &backend.watch;
        let flushes = || (watch.audio_flushes.load(Ordering::SeqCst), watch.video_flushes.load(Ordering::SeqCst));
        // The audio pipeline flushes its output once as it starts.
        let before = flushes();
        player.select_subtitle(Some(3));
        let state = wait_until(&player, Duration::from_secs(20), &format!("{mode:?}: playback end"), |state| state.ended);
        assert_eq!(flushes(), before, "{mode:?}: the failed refresh flushed audio or video");
        assert_audio_and_video_complete(&mkv, &backend, &state, Some(40), &format!("{mode:?}"));
        let expected = refcheck::ffmpeg_video_md5s(&mkv, 0, "yuv420p");
        let presented = backend.headless.capture().video[0].frame_md5.clone();
        let mut remaining = expected.iter();
        assert!(
            presented.iter().all(|md5| remaining.any(|want| want == md5)),
            "{mode:?}: every presented frame is FFmpeg's, in order",
        );
        let shows = watch.shows.lock().clone();
        assert!(
            shows.iter().any(|show| show.images > 0 && show.at >= Duration::from_millis(3600)),
            "{mode:?}: the second track's cue from 3.6 s: {shows:?}",
        );
        drop(player);
    }
}

/// The default audio track, chosen at open, stays selected and playing
/// when a subtitle track is selected afterwards.
#[test]
fn selecting_a_subtitle_keeps_the_default_audio_track() {
    let scratch = Scratch::new();
    let cues = scratch.file("cues.srt");
    srt(&cues, &[(200, 3800)]);
    let mkv = scratch.file("default-audio.mkv");
    movie(&mkv, 4, None, &[&cues]);
    let (player, backend) = play(&mkv, None::<u32>);
    let state = wait_until(&player, Duration::from_secs(10), "media time 1 s", |state| state.position >= Duration::from_secs(1));
    assert_eq!(state.audio, Some(0), "the default audio track");
    let pcm = || backend.headless.capture().audio.iter().map(|audio| audio.pcm.len()).sum::<usize>();
    let before = pcm();
    player.select_subtitle(Some(1));
    let state = wait_until(&player, Duration::from_secs(20), "playback end", |state| state.ended);
    assert_eq!((state.audio, state.subtitle), (Some(0), Some(1)), "audio kept, subtitles added");
    // The rest of the four seconds, less what the device took ahead.
    let after = pcm() - before;
    assert!(after >= 2 * 48_000 * 2, "{after} PCM samples after the switch");
    assert!(backend.watch.shows.lock().iter().any(|show| show.images > 0), "the cue up since 0.2 s shows");
    drop(player);
}

/// Text cues each get their own image: a flood of 80 cues starting 40 ms
/// apart, all up until 4.2 s, over 1080p video. At most 64 are up at once
/// (the earliest go first); the video and audio play on; the flood comes
/// down at its end; two later cues that overlap are shown together; a seek
/// in the middle of the flood clears it.
#[test]
fn overlapping_text_cues_stay_bounded_beside_1080p_video() {
    let scratch = Scratch::new();
    let cues = scratch.file("flood.srt");
    let mut timing: Vec<(u32, u32)> = (0..80).map(|i| (200 + 40 * i, 4200)).collect();
    timing.extend([(4600, 5300), (4800, 5500)]);
    srt(&cues, &timing);
    let mkv = scratch.file("flood.mkv");
    movie(&mkv, 6, Some(("color=c=0x204060:size=1920x1080:rate=5", "5")), &[&cues]);
    let path = scratch.file("flood.ptscript");
    scripted::write(&path, &mkv, Mode::Unseekable);
    let bounded = |shows: &[Shown]| {
        for show in shows {
            assert!(show.images <= 64 && show.bytes <= 64 << 20, "a show of {} images, {} bytes", show.images, show.bytes);
        }
    };

    let (player, backend) = play(&path, 2);
    let state = wait_until(&player, Duration::from_secs(60), "playback end", |state| state.ended);
    assert_audio_and_video_complete(&mkv, &backend, &state, Some(30), "1080p flood");
    let shows = backend.watch.shows.lock().clone();
    bounded(&shows);
    assert!(shows.iter().any(|show| show.images == 64), "the flood reaches the bound: {:?}", shows.iter().map(|s| s.images).max());
    let flood_down = shows.iter().position(|show| show.images == 0 && show.at >= Duration::from_millis(4200))
        .expect("the flood comes down at its end");
    assert!(shows[flood_down].at < Duration::from_millis(4600), "down before the next cue: {:?}", shows[flood_down]);
    assert!(
        shows[flood_down..].iter().any(|show| show.images == 2 && show.at >= Duration::from_millis(4800) && show.at < Duration::from_millis(5300)),
        "two overlapping cues up together: {:?}", &shows[flood_down..],
    );
    assert!(shows.last().is_some_and(|show| show.images == 0), "cleared at the end");
    drop(player);

    // The plain Matroska file seeks.
    let (player, backend) = play(&mkv, 2);
    wait_until(&player, Duration::from_secs(30), "the flood up", |state| state.position >= Duration::from_secs(3));
    let before = backend.watch.shows.lock().len();
    player.seek(Duration::from_millis(4400));
    let begun = Instant::now();
    let cleared = loop {
        let shows = backend.watch.shows.lock().clone();
        if let Some(show) = shows.get(before) {
            break *show;
        }
        assert!(begun.elapsed() < Duration::from_secs(10), "no subtitle call after the seek");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(cleared.images, 0, "the seek clears the flood");
    wait_until(&player, Duration::from_secs(30), "playback end after the seek", |state| state.ended);
    let shows = backend.watch.shows.lock().clone();
    bounded(&shows);
    assert!(shows[before..].iter().all(|show| show.images <= 2), "no flood cue after the seek: {:?}", &shows[before..]);
    drop(player);
}
