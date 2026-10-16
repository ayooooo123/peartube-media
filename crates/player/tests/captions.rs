//! Closed captions carried in the video, through the real Player in
//! realtime: H.264 with ATSC A/53 captions (the FATE roll-up captions,
//! re-encoded by libx264 with B-frames and `-a53cc 1`), in Matroska and in
//! TS. The playback lists its EIA-608 and CEA-708 caption tracks, and with a
//! caption track selected each caption screen comes up on the playback
//! clock when it changes, until the next replaces it:
//!
//! - EIA-608 at FFmpeg's times: the events of `ffmpeg -real_time 1` over
//!   the lavfi `subcc` output (FFmpeg's live captions);
//! - CEA-708 at the times of the decoder's outputs (VLC's decoder, checked
//!   output for output in subs-cc's tests).
//!
//! Every image the Player puts up is the caption screen current on the
//! clock at that moment, and comes at most 300 ms after that screen's time;
//! every clear comes while the current screen is blank (or at Ended); every
//! screen that stays 300 ms or more comes up. The bound is loose because
//! these tests run unoptimized, often beside builds; subs-cc's tests check
//! the times themselves against FFmpeg and VLC exactly. The screen is clear
//! at Ended.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use oxideav_core::{Error, MediaType};
use parking_lot::Mutex;
use player::backend::{AudioSink, Backend, Clock, NativePresenter, NativeStatus, SubtitleImage, SubtitleSink, VideoSink};
use player::{Headless, Player, PlayerOptions, State, TrackKind};
use subs_cc::cea708::Cea708;
use subs_cc::eia608::ticks_to_us;
use subs_cc::{CaptionTimeline, CcExtractor};

/// One realtime playback at a time, as headless.rs runs them.
static REALTIME: Mutex<()> = Mutex::new(());

/// The synthetic caption streams the Player lists.
const CAPTIONS_608: u32 = 0x1_0000;
const CAPTIONS_708: u32 = 0x1_0001;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir()
            .join(format!("peartube-captions-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ffmpeg(program: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> String {
    let out = Command::new(program).args(["-v", "error", "-nostdin", "-y"]).args(args).output().expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// The FATE roll-up captions in H.264 (`container`: mkv or ts), scaled to
/// 160x96 like the other realtime subtitle tests' video: the caption data
/// rides on the frames through the scaler.
fn captioned(scratch: &Scratch, container: &str) -> PathBuf {
    let path = scratch.file(&format!("captions.{container}"));
    let source = refcheck::fate("sub/Closedcaption_rollup.m2v");
    ffmpeg(refcheck::system_ffmpeg(), &[
        "-i", source.to_str().unwrap(), "-an", "-vf", "scale=160:96", "-c:v", "libx264", "-preset", "veryfast",
        "-bf", "3", "-a53cc", "1", path.to_str().unwrap(),
    ]);
    path
}

/// When `path`'s video ends, in seconds: its last packet's time plus its
/// duration, as ffprobe reads them.
fn video_end(path: &Path) -> f64 {
    let out = Command::new(refcheck::pinned_ffprobe())
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time,duration_time", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("the pinned ffprobe runs");
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let (pts, duration) = line.split_once(',')?;
            Some(pts.parse::<f64>().ok()? + duration.trim_end_matches(',').parse::<f64>().unwrap_or(0.0))
        })
        .fold(f64::MIN, f64::max)
}

/// FFmpeg's live EIA-608 events for `path`: each event's start
/// (centiseconds) and whether it shows text (anything but blanks once the
/// `{...}` overrides, `\N` and `\h` are out).
fn ffmpeg_events(path: &Path) -> Vec<(i64, bool)> {
    let ass = ffmpeg(refcheck::pinned_ffmpeg(), &[
        "-copyts", "-real_time", "1", "-f", "lavfi", "-i", &format!("movie={}[out0+subcc]", path.to_str().unwrap()),
        "-map", "0:s", "-c:s", "ass", "-f", "ass", "-",
    ]);
    ass.lines()
        .filter_map(|l| l.strip_prefix("Dialogue: "))
        .map(|l| {
            let fields: Vec<&str> = l.splitn(10, ',').collect();
            let mut parts = fields[1].split([':', '.']).map(|p| p.parse::<i64>().unwrap());
            let (h, m, s, c) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
            let mut text = String::new();
            let mut overrides = 0;
            for ch in fields[9].replace("\\N", " ").replace("\\h", " ").chars() {
                match ch {
                    '{' => overrides += 1,
                    '}' => overrides -= 1,
                    _ if overrides == 0 => text.push(ch),
                    _ => {}
                }
            }
            (((h * 60 + m) * 60 + s) * 100 + c, !text.trim().is_empty())
        })
        .collect()
}

/// The CEA-708 screens of `path`'s video: each output's start (µs) and
/// whether it shows text, from the same extraction and decoder the Player
/// runs (a picture without a time takes the last one, as the decoder
/// does), and a blank screen at an output's stop when no output follows by
/// then (VLC's subpicture lasts 10 s at most).
fn cea708_screens(path: &Path) -> Vec<(i64, bool)> {
    let ctx = codecs::context();
    let format = refcheck::probe_container(&ctx, path).unwrap();
    let mut demuxer = ctx.containers.open_demuxer(&format, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap();
    let video = demuxer.streams().iter().find(|s| s.params.media_type == MediaType::Video).unwrap().clone();
    let mut extractor = CcExtractor::new(video.params.codec_id.as_str(), &video.params.extradata).unwrap();
    let mut timeline = CaptionTimeline::new();
    let mut pictures = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(packet) if packet.stream_index == video.index => {
                let triplets = extractor.extract(&packet.data);
                pictures.extend(timeline.push(packet.pts, packet.dts, triplets));
            }
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("demux: {e}"),
        }
    }
    pictures.extend(timeline.finish());
    let tb = video.time_base.0;
    let mut decoder = Cea708::new(1);
    let mut outputs = Vec::new();
    let mut last = 0;
    for (ts, triplets) in pictures {
        let us = ts.and_then(|t| ticks_to_us(t, tb.num, tb.den)).unwrap_or(last);
        last = us;
        let data: Vec<u8> = triplets.into_iter().flatten().collect();
        outputs.extend(decoder.decode(&data, us).into_iter().map(|o| {
            let text = o.regions.iter().flat_map(|r| &r.segments).any(|s| s.text.iter().any(|b| !b.is_ascii_whitespace()));
            (o.start, o.stop, text)
        }));
    }
    let mut screens = Vec::new();
    for (i, &(start, stop, text)) in outputs.iter().enumerate() {
        screens.push((start, text));
        if text && outputs.get(i + 1).is_none_or(|next| next.0 > stop) {
            screens.push((stop, false));
        }
    }
    screens
}

#[derive(Default)]
struct Observation {
    clock: Option<Arc<dyn Clock>>,
    /// Each show: the clock, and whether it put an image up.
    shows: Vec<(Duration, bool)>,
}

struct MockPresenterState {
    playing: bool,
    start_time: Option<Instant>,
    start_pos: Duration,
    item_origin: Option<Duration>,
    closed: bool,
}

struct MockPresenter {
    end: Duration,
    seek_serial: AtomicU64,
    state: Arc<Mutex<MockPresenterState>>,
    on_change: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
}

struct MockPresenterClock {
    end: Duration,
    state: Arc<Mutex<MockPresenterState>>,
    source_axis: bool,
}

impl Clock for MockPresenterClock {
    fn now(&self) -> Option<Duration> {
        let state = self.state.lock();
        if state.closed {
            return None;
        }
        let origin = state.item_origin.unwrap_or(Duration::ZERO);
        let item_end = self.end + origin;
        let item_pos = if state.playing {
            let elapsed = state.start_time.map_or(Duration::ZERO, |t| t.elapsed());
            (state.start_pos + elapsed).min(item_end)
        } else {
            state.start_pos.min(item_end)
        };
        if self.source_axis {
            Some(item_pos.saturating_sub(origin))
        } else {
            Some(item_pos)
        }
    }

    fn monotonic_ns_at(&self, at: Duration) -> Option<i64> {
        let state = self.state.lock();
        if !state.playing {
            return None;
        }
        let start_time = state.start_time?;
        let start_pos = state.start_pos;
        let origin = state.item_origin.unwrap_or(Duration::ZERO);
        drop(state);
        let now = start_pos + start_time.elapsed();
        let now_axis = if self.source_axis { now.saturating_sub(origin) } else { now };
        let ahead_secs = at.as_secs_f64() - now_axis.as_secs_f64();
        Some(player::clock::current_monotonic_ns() + (ahead_secs * 1e9) as i64)
    }
}

impl MockPresenter {
    fn new(end: Duration) -> Self {
        Self {
            end,
            seek_serial: AtomicU64::new(0),
            state: Arc::new(Mutex::new(MockPresenterState {
                playing: false,
                start_time: None,
                start_pos: Duration::ZERO,
                item_origin: None,
                closed: false,
            })),
            on_change: Arc::new(Mutex::new(None)),
        }
    }

    fn source_clock(&self) -> Arc<dyn Clock> {
        Arc::new(MockPresenterClock {
            end: self.end,
            state: Arc::clone(&self.state),
            source_axis: true,
        })
    }

    fn notify(&self) {
        if let Some(cb) = self.on_change.lock().as_ref() {
            cb();
        }
    }
}

impl NativePresenter for MockPresenter {
    fn open(&self, _url: &str, start: Duration) {
        let mut state = self.state.lock();
        state.playing = false;
        state.start_time = None;
        state.start_pos = start;
        state.item_origin.get_or_insert(start);
        state.closed = false;
        drop(state);
        self.notify();
    }

    fn play(&self) {
        let mut state = self.state.lock();
        if !state.playing {
            state.playing = true;
            state.start_time = Some(Instant::now());
        }
        drop(state);
        self.notify();
    }

    fn pause(&self) {
        let mut state = self.state.lock();
        if state.playing {
            if let Some(t) = state.start_time {
                let item_end = self.end + state.item_origin.unwrap_or(Duration::ZERO);
                state.start_pos = (state.start_pos + t.elapsed()).min(item_end);
            }
            state.playing = false;
            state.start_time = None;
        }
        drop(state);
        self.notify();
    }

    fn seek(&self, to: Duration) -> u64 {
        let serial = self.seek_serial.fetch_add(1, Ordering::SeqCst) + 1;
        let mut state = self.state.lock();
        state.start_pos = to;
        if state.playing {
            state.start_time = Some(Instant::now());
        } else {
            state.start_time = None;
        }
        drop(state);
        self.notify();
        serial
    }

    fn clock(&self) -> Arc<dyn Clock> {
        Arc::new(MockPresenterClock {
            end: self.end,
            state: Arc::clone(&self.state),
            source_axis: false,
        })
    }

    fn status(&self) -> NativeStatus {
        let state = self.state.lock();
        let item_end = self.end + state.item_origin.unwrap_or(Duration::ZERO);
        let pos = if state.closed {
            None
        } else if state.playing {
            let elapsed = state.start_time.map_or(Duration::ZERO, |t| t.elapsed());
            Some((state.start_pos + elapsed).min(item_end))
        } else {
            Some(state.start_pos.min(item_end))
        };
        let ended = pos.is_some_and(|p| p >= item_end);
        let seek_done = self.seek_serial.load(Ordering::SeqCst);
        NativeStatus {
            ready: true,
            picture: true,
            playing: state.playing,
            waiting: false,
            ended,
            seek_done,
            position: pos,
            failed: None,
        }
    }

    fn set_on_change(&self, on_change: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.on_change.lock() = on_change;
    }

    fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        state.playing = false;
        state.start_time = None;
        drop(state);
        self.notify();
    }
}

struct Recorder {
    headless: Arc<Headless>,
    observation: Arc<Mutex<Observation>>,
    native_presenter: Option<Arc<MockPresenter>>,
}

impl Backend for Recorder {
    fn audio(&self) -> Box<dyn AudioSink> {
        self.headless.audio()
    }

    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        self.observation.lock().clock = Some(clock.clone());
        self.headless.video(clock)
    }

    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        Box::new(Stamped(self.observation.clone()))
    }

    fn native(&self) -> Option<Arc<dyn NativePresenter>> {
        self.native_presenter.as_ref().map(|p| p.clone() as Arc<dyn NativePresenter>)
    }
}

struct Stamped(Arc<Mutex<Observation>>);

impl SubtitleSink for Stamped {
    fn show(&mut self, images: &[SubtitleImage], _width: u32, _height: u32) {
        let mut observation = self.0.lock();
        let at = observation.clock.as_ref().and_then(|clock| clock.now()).unwrap_or_default();
        observation.shows.push((at, !images.is_empty()));
    }
}

fn play(path: &Path, subtitle: u32) -> (Player, Arc<Mutex<Observation>>) {
    let observation = Arc::new(Mutex::new(Observation::default()));
    let backend = Arc::new(Recorder {
        headless: Headless::new(),
        observation: observation.clone(),
        native_presenter: None,
    });
    let player = Player::open(
        path.to_str().unwrap(),
        backend,
        Arc::new(codecs::context()),
        PlayerOptions { subtitle: Some(subtitle), realtime: true, ..PlayerOptions::default() },
        |_| {},
    );
    (player, observation)
}
fn play_native(path: &Path, subtitle: u32, end: f64) -> (Player, Arc<Mutex<Observation>>) {
    let observation = Arc::new(Mutex::new(Observation::default()));
    let presenter = Arc::new(MockPresenter::new(Duration::from_secs_f64(end)));
    observation.lock().clock = Some(presenter.source_clock());
    let backend = Arc::new(Recorder {
        headless: Headless::new(),
        observation: observation.clone(),
        native_presenter: Some(presenter),
    });
    let player = Player::open(
        path.to_str().unwrap(),
        backend,
        Arc::new(codecs::context()),
        PlayerOptions { subtitle: Some(subtitle), realtime: true, ..PlayerOptions::default() },
        |_| {},
    );
    (player, observation)
}


fn wait_end(player: &Player, what: &str) -> State {
    let begun = Instant::now();
    loop {
        let state = player.state();
        assert!(state.error.is_none(), "{what}: {state:?}");
        if state.ended {
            return state;
        }
        assert!(begun.elapsed() < Duration::from_secs(30), "{what}: no end within 30 s: {state:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// How late a change may come up: these tests run unoptimized, often beside
/// builds (the other realtime subtitle tests allow as much).
const LATE: f64 = 0.300;

/// The Player's shows against the screens a caption track puts up,
/// `(start µs, visible)` in order (see the module docs); `rounding`: how far
/// the screens' times may run past ours (FFmpeg's centiseconds: 5 ms). The
/// last screen lasts until the video's end, when the playback takes the
/// screen down.
fn assert_shows(what: &str, shows: &[(Duration, bool)], screens: &[(i64, bool)], rounding: f64, end: f64) {
    let starts: Vec<f64> = screens.iter().map(|(us, _)| *us as f64 / 1e6).collect();
    assert!(screens.iter().any(|(_, up)| *up), "{what}: no caption to show");
    let mut shown = vec![false; screens.len()];
    let mut latest = 0.0f64;
    let last = shows.len().saturating_sub(1);
    for (i, (at, up)) in shows.iter().enumerate() {
        let t = at.as_secs_f64();
        // The screen current at `t`: the last one started by then.
        let current = starts.iter().rposition(|start| *start <= t + rounding);
        if !up && i == last {
            break; // Ended takes the screen down, whatever is up.
        }
        let Some(k) = current else { panic!("{what}: a show at {t:.3} s, before any caption") };
        let late = t - starts[k];
        assert_eq!(*up, screens[k].1, "{what}: a show at {t:.3} s (image: {up}) for the screen of {:.3} s", starts[k]);
        assert!(late <= LATE, "{what}: a show at {t:.3} s, {late:.3} s after its screen of {:.3} s", starts[k]);
        latest = latest.max(late);
        shown[k] = true;
    }
    for (i, start) in starts.iter().enumerate() {
        let lasting = starts.get(i + 1).copied().unwrap_or(end) - start >= LATE;
        assert!(!lasting || shown[i] || !screens[i].1, "{what}: the caption of {start:.3} s never came up");
    }
    assert!(shows.last().is_some_and(|(_, up)| !up), "{what}: the screen is clear at the end");
    let images = shows.iter().filter(|(_, up)| *up).count();
    println!("{what}: {} screens, {images} images shown, at most {:.0} ms after their time", screens.len(), latest * 1000.0);
}

#[test]
fn eia608_captions_come_up_at_ffmpegs_times() {
    let scratch = Scratch::new();
    let _realtime = REALTIME.lock();
    for container in ["mkv", "ts"] {
        let path = captioned(&scratch, container);
        let screens: Vec<(i64, bool)> = ffmpeg_events(&path).into_iter().map(|(cs, up)| (cs * 10_000, up)).collect();
        let (player, observation) = play(&path, CAPTIONS_608);
        let state = wait_end(&player, container);
        let mut caption_tracks: Vec<(u32, String)> = state
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Subtitle && t.stream >= CAPTIONS_608)
            .map(|t| (t.stream, t.codec.clone()))
            .collect();
        caption_tracks.sort();
        assert_eq!(
            caption_tracks,
            [(CAPTIONS_608, "eia_608".to_string()), (CAPTIONS_708, "cea_708".to_string())],
            "{container}: the caption tracks listed"
        );
        assert_eq!(state.subtitle, Some(CAPTIONS_608), "{container}: the selected caption track");
        assert_shows(&format!("EIA-608 in {container}"), &observation.lock().shows, &screens, 0.005, video_end(&path));
        drop(player);
    }
}

#[test]
fn cea708_captions_come_up_at_the_decoders_times() {
    let scratch = Scratch::new();
    let _realtime = REALTIME.lock();
    let path = captioned(&scratch, "mkv");
    let screens = cea708_screens(&path);
    let (player, observation) = play(&path, CAPTIONS_708);
    wait_end(&player, "mkv");
    assert_shows("CEA-708 in mkv", &observation.lock().shows, &screens, 0.0, video_end(&path));
    drop(player);
}
#[test]
fn eia608_and_cea708_captions_work_on_native_route() {
    let scratch = Scratch::new();
    let _realtime = REALTIME.lock();
    let path = captioned(&scratch, "mkv");
    let end = video_end(&path);

    // Discovery when subtitle is None
    {
        let observation = Arc::new(Mutex::new(Observation::default()));
        let presenter = Arc::new(MockPresenter::new(Duration::from_secs_f64(end)));
        let backend = Arc::new(Recorder {
            headless: Headless::new(),
            observation,
            native_presenter: Some(presenter),
        });
        let player = Player::open(
            path.to_str().unwrap(),
            backend,
            Arc::new(codecs::context()),
            PlayerOptions { subtitle: None, realtime: true, ..PlayerOptions::default() },
            |_| {},
        );
        let state = wait_end(&player, "native mkv discovery with subtitle None");
        assert!(
            state.video_decoder.as_deref().unwrap_or("").starts_with("native "),
            "expected native video decoder, got: {:?}",
            state.video_decoder
        );
        let mut caption_tracks: Vec<(u32, String)> = state
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Subtitle && t.stream >= CAPTIONS_608)
            .map(|t| (t.stream, t.codec.clone()))
            .collect();
        caption_tracks.sort();
        assert_eq!(
            caption_tracks,
            [(CAPTIONS_608, "eia_608".to_string()), (CAPTIONS_708, "cea_708".to_string())],
            "native route: tracks discovered when subtitle was None"
        );
        drop(player);
    }

    // 1. EIA-608
    {
        let screens: Vec<(i64, bool)> = ffmpeg_events(&path).into_iter().map(|(cs, up)| (cs * 10_000, up)).collect();
        let (player, observation) = play_native(&path, CAPTIONS_608, end);
        let state = wait_end(&player, "native mkv 608");
        assert!(
            state.video_decoder.as_deref().unwrap_or("").starts_with("native "),
            "expected native video decoder, got: {:?}",
            state.video_decoder
        );
        let mut caption_tracks: Vec<(u32, String)> = state
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Subtitle && t.stream >= CAPTIONS_608)
            .map(|t| (t.stream, t.codec.clone()))
            .collect();
        caption_tracks.sort();
        assert_eq!(
            caption_tracks,
            [(CAPTIONS_608, "eia_608".to_string()), (CAPTIONS_708, "cea_708".to_string())],
            "native route: the caption tracks listed"
        );
        assert_eq!(state.subtitle, Some(CAPTIONS_608), "native route: selected caption track");
        assert_shows("EIA-608 on native route", &observation.lock().shows, &screens, 0.005, end);
        drop(player);
    }

    // 2. CEA-708
    {
        let screens = cea708_screens(&path);
        let (player, observation) = play_native(&path, CAPTIONS_708, end);
        let state = wait_end(&player, "native mkv 708");
        assert!(
            state.video_decoder.as_deref().unwrap_or("").starts_with("native "),
            "expected native video decoder, got: {:?}",
            state.video_decoder
        );
        let mut caption_tracks: Vec<(u32, String)> = state
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Subtitle && t.stream >= CAPTIONS_608)
            .map(|t| (t.stream, t.codec.clone()))
            .collect();
        caption_tracks.sort();
        assert_eq!(
            caption_tracks,
            [(CAPTIONS_608, "eia_608".to_string()), (CAPTIONS_708, "cea_708".to_string())],
            "native route: the caption tracks listed"
        );
        assert_eq!(state.subtitle, Some(CAPTIONS_708), "native route: selected caption track");
        assert_shows("CEA-708 on native route", &observation.lock().shows, &screens, 0.0, end);
        drop(player);
    }
}
