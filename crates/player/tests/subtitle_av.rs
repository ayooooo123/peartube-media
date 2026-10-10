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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bitmap::{Scratch, ffmpeg};
use oxideav_core::{CodecParameters, Packet, VideoFrame};
use parking_lot::Mutex;
use std::task::Poll;
use player::backend::{
    AudioSink, Backend, Clock, ProducerId, SinkError, SubtitleImage, SubtitleSink,
    VideoError, VideoMode, VideoOutput, VideoRequest, VideoSink, VideoTarget,
};
use player::{Headless, Player, PlayerOptions, State};
use scripted::{Hold, Mode};

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
        Box::new(WatchedVideo {
            sink: self.headless.video(clock),
            watch: self.watch.clone(),
            seen_producers: std::collections::HashSet::new(),
            initial_producer: None,
        })
    }

    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        Box::new(WatchedSubtitles(self.watch.clone()))
    }
}

struct WatchedAudio(Box<dyn AudioSink>, Arc<Watch>);

impl AudioSink for WatchedAudio {
    fn open(&mut self, sample_rate: u32, layout: oxideav_core::ChannelLayout) -> Result<(), SinkError> { self.0.open(sample_rate, layout) }
    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError> { self.0.write(pcm, pts) }
    fn play(&mut self) { self.0.play() }
    fn pause(&mut self) { self.0.pause() }
    fn flush(&mut self) {
        self.1.audio_flushes.fetch_add(1, Ordering::SeqCst);
        self.0.flush()
    }
    fn clock(&self) -> Arc<dyn Clock> { self.0.clock() }
}

struct WatchedVideo {
    sink: Box<dyn VideoSink>,
    watch: Arc<Watch>,
    /// Successful non-initial reset transitions counted once per producer.
    seen_producers: std::collections::HashSet<u64>,
    initial_producer: Option<u64>,
}

impl VideoSink for WatchedVideo {
    fn output(&self) -> VideoOutput {
        self.sink.output()
    }
    fn poll_transition(&mut self, request: &VideoRequest) -> Poll<Result<VideoMode, VideoError>> {
        let result = self.sink.poll_transition(request);
        if let Poll::Ready(Ok(_)) = &result {
            let id = request.producer.0;
            if self.initial_producer.is_none() {
                self.initial_producer = Some(id);
                self.seen_producers.insert(id);
            } else if !self.seen_producers.contains(&id) {
                // Count each successful reset transition once per producer excluding initial.
                let is_reset = match &request.target {
                    VideoTarget::Frames { reset: true, .. } | VideoTarget::Compressed { .. } => true,
                    VideoTarget::Frames { reset: false, .. } => false,
                    VideoTarget::Retired => false,
                };
                if is_reset {
                    self.seen_producers.insert(id);
                    self.watch.video_flushes.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        result
    }
    fn push_packet(
        &mut self,
        producer: ProducerId,
        packet: &mut Option<Packet>,
        pts: Duration,
        random_access: bool,
    ) -> Result<(), VideoError> {
        self.sink.push_packet(producer, packet, pts, random_access)
    }
    fn push_frame(
        &mut self,
        producer: ProducerId,
        frame: &mut Option<VideoFrame>,
        pts: Duration,
    ) -> Result<(), VideoError> {
        self.sink.push_frame(producer, frame, pts)
    }
    fn present_from(&mut self, producer: ProducerId, start: Duration) -> Result<(), VideoError> {
        self.sink.present_from(producer, start)
    }
    fn set_playing(&mut self, producer: ProducerId, playing: bool) -> Result<(), VideoError> {
        self.sink.set_playing(producer, playing)
    }
    fn frame_lead(&self) -> Duration {
        self.sink.frame_lead()
    }
    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        self.sink.poll_finish(producer)
    }
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
    play_with(path, subtitle, true)
}

fn play_with(path: &Path, subtitle: impl Into<Option<u32>>, realtime: bool) -> (Player, Arc<Watched>) {
    let headless = Headless::new();
    headless.set_active_streams(None, None, None, realtime);
    let backend = Arc::new(Watched { headless, watch: Arc::new(Watch::default()) });
    let player = Player::open(
        path.to_str().unwrap(), backend.clone(), Arc::new(scripted::context()),
        PlayerOptions { subtitle: subtitle.into(), realtime, ..PlayerOptions::default() }, |_| {},
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
/// source and GOP length), then a 48 kHz stereo PCM track per tone of
/// `tones` (Hz), then one WebVTT track per SubRip file of `subtitles`, in
/// that stream order. (The text decoders take Matroska's WebVTT blocks;
/// its SubRip and ASS blocks they reject.)
fn movie(path: &Path, seconds: u32, video: Option<(&str, &str)>, tones: &[u32], subtitles: &[&Path]) {
    let duration = seconds.to_string();
    let mut args: Vec<String> = Vec::new();
    if let Some((source, _)) = video {
        args.extend(["-f".into(), "lavfi".into(), "-i".into(), format!("{source}:duration={seconds}")]);
    }
    for tone in tones {
        let sine = format!("sine=frequency={tone}:sample_rate=48000:duration={seconds}");
        args.extend(["-f".into(), "lavfi".into(), "-i".into(), sine]);
    }
    for subtitle in subtitles {
        args.extend(["-i".into(), subtitle.to_str().unwrap().into()]);
    }
    let inputs = usize::from(video.is_some()) + tones.len() + subtitles.len();
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
        movie(&mkv, 6, video, &[440], &[&cues]);
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

/// A subtitle switch never seeks, whether the demuxer can seek, cannot
/// (`seek_to` unsupported) or fails: no audio or video is flushed or
/// skipped, the inter-coded video decodes on unbroken (one keyframe in five
/// seconds), and the new track shows from its next cue past what the
/// demuxer had read ahead (up to two seconds) at the switch. Its cue up at
/// the switch (0.3 s to 3.5 s) is not shown; its cue from 4.0 s is.
#[test]
fn a_subtitle_switch_never_seeks() {
    for mode in [Mode::Seekable, Mode::Unseekable, Mode::SeekFails] {
        let scratch = Scratch::new();
        let first = scratch.file("first.srt");
        let second = scratch.file("second.srt");
        srt(&first, &[(200, 800), (2600, 3200)]);
        srt(&second, &[(300, 3500), (4000, 4800)]);
        let mkv = scratch.file("two-tracks.mkv");
        movie(&mkv, 5, Some(("testsrc2=size=160x90:rate=10", "1000")), &[440], &[&first, &second]);
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
        let shown_before = watch.shows.lock().len();
        player.select_subtitle(Some(3));
        let state = wait_until(&player, Duration::from_secs(20), &format!("{mode:?}: playback end"), |state| state.ended);
        assert_eq!(flushes(), before, "{mode:?}: the subtitle switch flushed audio or video");
        assert_audio_and_video_complete(&mkv, &backend, &state, Some(50), &format!("{mode:?}"));
        let expected = refcheck::ffmpeg_video_md5s(&mkv, 0, "yuv420p");
        let presented = backend.headless.capture().video[0].frame_md5.clone();
        let mut remaining = expected.iter();
        assert!(
            presented.iter().all(|md5| remaining.any(|want| want == md5)),
            "{mode:?}: every presented frame is FFmpeg's, in order",
        );
        let shows = watch.shows.lock()[shown_before..].to_vec();
        let visible: Vec<_> = shows.iter().filter(|show| show.images > 0).collect();
        assert!(
            !visible.is_empty() && visible.iter().all(|show| show.at >= Duration::from_millis(4000)),
            "{mode:?}: after the switch only the second track's next cue, from 4.0 s: {shows:?}",
        );
        drop(player);
    }
}

/// The default audio track, chosen at open, stays selected and playing
/// when a subtitle track is selected afterwards; the track shows from its
/// next cue past the demuxer's read-ahead.
#[test]
fn selecting_a_subtitle_keeps_the_default_audio_track() {
    let scratch = Scratch::new();
    let cues = scratch.file("cues.srt");
    srt(&cues, &[(200, 1500), (4000, 5800)]);
    let mkv = scratch.file("default-audio.mkv");
    movie(&mkv, 6, None, &[440], &[&cues]);
    let (player, backend) = play(&mkv, None::<u32>);
    let state = wait_until(&player, Duration::from_secs(10), "media time 1 s", |state| state.position >= Duration::from_secs(1));
    assert_eq!(state.audio, Some(0), "the default audio track");
    let pcm = || backend.headless.capture().audio.iter().map(|audio| audio.pcm.len()).sum::<usize>();
    let before = pcm();
    player.select_subtitle(Some(1));
    let state = wait_until(&player, Duration::from_secs(20), "playback end", |state| state.ended);
    assert_eq!((state.audio, state.subtitle), (Some(0), Some(1)), "audio kept, subtitles added");
    // The rest of the six seconds, less what the device took ahead.
    let after = pcm() - before;
    assert!(after >= 4 * 48_000 * 2, "{after} PCM samples after the switch");
    let shows = backend.watch.shows.lock().clone();
    assert!(shows.iter().any(|show| show.images > 0 && show.at >= Duration::from_millis(4000)), "the next cue, from 4.0 s: {shows:?}");
    drop(player);
}

/// A flood of overlapping cues beside video stays within the image and byte
/// bounds; text that has no free line is not shown. The flood expires, later
/// cues overlap, and a seek clears the old cues without losing audio/video.
#[test]
fn overlapping_text_cues_stay_bounded_beside_1440p_video() {
    let scratch = Scratch::new();
    let cues = scratch.file("flood.srt");
    let mut timing: Vec<(u32, u32)> = (0..80).map(|i| (200 + 40 * i, 4200)).collect();
    timing.extend([(4600, 5300), (4800, 5500)]);
    srt(&cues, &timing);
    let mkv = scratch.file("flood.mkv");
    movie(&mkv, 6, Some(("color=c=0x204060:size=1920x1440:rate=5", "5")), &[440], &[&cues]);
    let path = scratch.file("flood.ptscript");
    scripted::write(&path, &mkv, Mode::Unseekable);
    let bounded = |shows: &[Shown]| {
        for show in shows {
            assert!(show.images <= 64 && show.bytes <= 64 << 20, "a show of {} images, {} bytes", show.images, show.bytes);
        }
    };

    let (player, backend) = play(&path, 2);
    let state = wait_until(&player, Duration::from_secs(60), "playback end", |state| state.ended);
    assert_audio_and_video_complete(&mkv, &backend, &state, Some(30), "1440p flood");
    let shows = backend.watch.shows.lock().clone();
    bounded(&shows);
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

/// Tests that hold a demuxer (`scripted::arm`) share one gate: they run
/// one at a time.
static GATE_TESTS: Mutex<()> = Mutex::new(());

/// Sets its flag when dropped, also while a panic unwinds.
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Subtitle selections made while the Player opens never cost the audio
/// track chosen by default, and an explicit audio choice made then stands.
/// Held cases: the selections are made while the demuxer is held inside its
/// open (before the Player reads its selection) or before its first packet
/// (after the demux loop's first look for switches). Concurrent cases:
/// subtitles are selected over and over from before the Player reads its
/// selection until it publishes its tracks, so some selections land while
/// it picks the default audio track. A default recorded only when no
/// selection came meanwhile (92ae878) is lost there. Each case checks the
/// selection once the playback has run past every selection.
#[test]
fn subtitle_selections_while_opening_keep_the_audio_choice() {
    let _gate = GATE_TESTS.lock();
    let scratch = Scratch::new();
    let cues = scratch.file("cues.srt");
    srt(&cues, &[(100, 3900)]);
    let mkv = scratch.file("opening.mkv");
    movie(&mkv, 4, None, &[440, 880], &[&cues]);
    let path = scratch.file("opening.ptscript");
    scripted::write_gated(&path, &mkv, Mode::Seekable);
    let settled = |player: &Player, backend: &Watched, what: &str, explicit: bool| {
        // Well past every selection: the demux loop has applied them all.
        let state = wait_until(player, Duration::from_secs(20), &format!("{what}: media time 0.3 s"), |state| {
            state.position >= Duration::from_millis(300)
        });
        let want = if explicit { Some(1) } else { Some(0) };
        assert_eq!((state.audio, state.subtitle), (want, Some(2)), "{what}: the audio and subtitle choice");
        assert!(backend.headless.capture().audio.iter().any(|audio| !audio.pcm.is_empty()), "{what}: no audio played");
    };
    for at in [Hold::Open, Hold::FirstPacket] {
        for explicit in [false, true] {
            let what = format!("held at {at:?}, explicit audio {explicit}");
            scripted::arm(&[at]);
            let (player, backend) = play(&path, None::<u32>);
            assert!(scripted::entered(at, Duration::from_secs(10)), "{what}: the demuxer held");
            player.select_subtitle(Some(2));
            if explicit {
                player.select_audio(Some(1));
            }
            player.select_subtitle(Some(2));
            scripted::release();
            settled(&player, &backend, &what, explicit);
            drop(player);
        }
    }
    for attempt in 0..40 {
        let explicit = attempt % 4 == 3;
        let what = format!("concurrent attempt {attempt}, explicit audio {explicit}");
        scripted::arm(&[Hold::Open]);
        let (player, backend) = play(&path, None::<u32>);
        assert!(scripted::entered(Hold::Open, Duration::from_secs(10)), "{what}: the demuxer held");
        if explicit {
            player.select_audio(Some(1));
        }
        let (selecting, published) = (AtomicBool::new(false), AtomicBool::new(false));
        std::thread::scope(|scope| {
            scope.spawn(|| loop {
                player.select_subtitle(Some(2));
                selecting.store(true, Ordering::SeqCst);
                if published.load(Ordering::SeqCst) {
                    break;
                }
            });
            // The selections stop however this closure ends: a failed wait
            // below must not leave the scope joining them forever.
            let _stop = SetOnDrop(&published);
            // The selections run before the Player reads its selection and
            // go on until it has published its tracks.
            let begun = Instant::now();
            while !selecting.load(Ordering::SeqCst) {
                assert!(begun.elapsed() < Duration::from_secs(10), "{what}: the selections never started");
                std::thread::yield_now();
            }
            scripted::release();
            wait_until(&player, Duration::from_secs(10), &format!("{what}: the tracks"), |state| !state.tracks.is_empty());
        });
        settled(&player, &backend, &what, explicit);
        drop(player);
    }
}

/// A subtitle-only playback whose audio is switched on while its subtitle
/// pipeline runs: the playback now has audio, so it ends with the
/// open-ended PGS state down, realtime or not. Two holds make the start
/// subtitle-only for sure: audio is switched off while the demuxer is held
/// inside its open (before the Player reads its selection), and on again
/// while it is held before its first packet (the subtitle pipeline started
/// without audio).
#[test]
fn subtitles_joined_by_audio_end_cleared() {
    let _gate = GATE_TESTS.lock();
    let scratch = Scratch::new();
    let sup = scratch.file("open-ended.sup");
    let subtitles = scratch.file("open-ended.mks");
    let mkv = scratch.file("joined.mkv");
    bitmap::pgs_states(&sup, &[(200, true)]);
    ffmpeg(&["-copyts", "-i", sup.to_str().unwrap(), "-map", "0:s", "-c:s", "copy", "-f", "matroska", subtitles.to_str().unwrap()]);
    ffmpeg(&[
        "-copyts", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=1", "-i", subtitles.to_str().unwrap(),
        "-map", "0:a", "-map", "1:s", "-c:a", "pcm_s16le", "-ac", "2", "-c:s", "copy", mkv.to_str().unwrap(),
    ]);
    let path = scratch.file("joined.ptscript");
    scripted::write_gated(&path, &mkv, Mode::Seekable);
    for realtime in [false, true] {
        let what = format!("realtime {realtime}");
        scripted::arm(&[Hold::Open, Hold::FirstPacket]);
        let (player, backend) = play_with(&path, Some(1), realtime);
        assert!(scripted::entered(Hold::Open, Duration::from_secs(10)), "{what}: the demuxer held in its open");
        player.select_audio(None);
        scripted::release();
        assert!(scripted::entered(Hold::FirstPacket, Duration::from_secs(10)), "{what}: the demuxer held before its first packet");
        let state = player.state();
        assert_eq!((state.audio, state.subtitle), (None, Some(1)), "{what}: subtitles alone at the start");
        player.select_audio(Some(0));
        scripted::release();
        let state = wait_until(&player, Duration::from_secs(20), &format!("{what}: playback end"), |state| state.ended);
        assert_eq!(state.audio, Some(0), "{what}: audio on");
        let shows = backend.watch.shows.lock().clone();
        assert!(shows.iter().any(|show| show.images > 0), "{what}: the state shows: {shows:?}");
        assert!(shows.last().is_some_and(|show| show.images == 0), "{what}: cleared at Ended: {shows:?}");
        drop(player);
    }
}
