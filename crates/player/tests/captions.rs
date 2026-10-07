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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use oxideav_core::{Error, MediaType};
use parking_lot::Mutex;
use player::backend::{AudioSink, Backend, Clock, SubtitleImage, SubtitleSink, VideoSink};
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

fn ffmpeg(args: &[&str]) -> String {
    let out = Command::new("ffmpeg").args(["-v", "error", "-nostdin", "-y"]).args(args).output().expect("ffmpeg on PATH");
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// The FATE roll-up captions in H.264 (`container`: mkv or ts), scaled to
/// 160x96 like the other realtime subtitle tests' video: the caption data
/// rides on the frames through the scaler.
fn captioned(scratch: &Scratch, container: &str) -> PathBuf {
    let path = scratch.file(&format!("captions.{container}"));
    let source = refcheck::fate("sub/Closedcaption_rollup.m2v");
    ffmpeg(&[
        "-i", source.to_str().unwrap(), "-an", "-vf", "scale=160:96", "-c:v", "libx264", "-preset", "veryfast",
        "-bf", "3", "-a53cc", "1", path.to_str().unwrap(),
    ]);
    path
}

/// When `path`'s video ends, in seconds: its last packet's time plus its
/// duration, as ffprobe reads them.
fn video_end(path: &Path) -> f64 {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time,duration_time", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe on PATH");
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
    let ass = ffmpeg(&[
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

struct Recorder {
    headless: Arc<Headless>,
    observation: Arc<Mutex<Observation>>,
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
    let backend = Arc::new(Recorder { headless: Headless::new(), observation: observation.clone() });
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
