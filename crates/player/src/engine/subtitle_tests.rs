//! Logical media-time evidence for the real subtitle consumer, at the
//! resolution of FFmpeg's reference. The injected clock is held at each
//! boundary, so OS scheduling cannot turn a correctly timed state change
//! into a late wall-clock measurement.

#[path = "../../tests/support/bitmap.rs"]
mod bitmap;

use std::sync::mpsc::{self, Sender};

use bitmap::{Scratch, Show, ffmpeg, oracle, pgs_with_clears};
use oxideav_core::{Error, MediaType};

use super::*;
use crate::backend::{SubtitleImage, SubtitleSink};

#[derive(Default)]
struct TestClock {
    state: Mutex<(Duration, usize)>,
    read: Condvar,
}

impl TestClock {
    fn set(&self, at: Duration, lane: &Lane) {
        *self.state.lock() = (at, 0);
        // Same notification the engine uses when its clock starts/stops or
        // seeks. The subtitle thread never assumes a wall-time deadline
        // while the clock is frozen.
        drop(lane.queue.lock());
        lane.cv.notify_all();
    }

    fn synchronize(&self) {
        let timeout = Instant::now() + Duration::from_secs(10);
        let mut state = self.state.lock();
        // now() is read at most once by wait() and once by advance() per
        // turn. Four reads ensure a complete scheduling turn has run at the
        // injected time, including any unexpected early show/clear.
        while state.1 < 4 {
            let now = Instant::now();
            assert!(now < timeout, "subtitle loop did not observe the injected clock");
            self.read.wait_for(&mut state, timeout - now);
        }
    }
}

impl Clock for TestClock {
    fn now(&self) -> Option<Duration> {
        let mut state = self.state.lock();
        state.1 += 1;
        self.read.notify_all();
        Some(state.0)
    }

    fn monotonic_ns_at(&self, _at: Duration) -> Option<i64> {
        // Manually advanced, never a prediction in wall-clock time.
        None
    }
}

struct CaptureSink {
    clock: Arc<TestClock>,
    shows: Sender<Show>,
}

impl SubtitleSink for CaptureSink {
    fn show(&mut self, images: &[SubtitleImage], width: u32, height: u32) {
        let at = self.clock.now().unwrap();
        let event = Show::from_images(at, width, height, images.iter().map(|image| (
            image.x, image.y, image.width, image.height, image.rgba.as_slice(),
        )));
        self.shows.send(event).unwrap();
    }
}

struct TestThread {
    stopped: Arc<AtomicBool>,
    lane: Arc<Lane>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for TestThread {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        drop(self.lane.queue.lock());
        self.lane.cv.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn pgs_show_replace_clear_times_and_canvases_match_ffmpeg_exactly() {
    let scratch = Scratch::new();
    let sup = scratch.file("clock.sup");
    let mkv = scratch.file("clock.mks");
    pgs_with_clears(&sup);
    ffmpeg(&["-copyts", "-i", sup.to_str().unwrap(), "-c:s", "copy", "-f", "matroska", mkv.to_str().unwrap()]);

    for (format, path) in [("sup", &sup), ("matroska", &mkv)] {
        let reference = oracle::ffmpeg_reference(path, 0);
        assert_eq!(reference.cues.len(), 5);
        check_timing(format, path, &reference);
    }
}

#[test]
fn dvb_show_replace_and_timeout_clear_match_ffmpeg_exactly() {
    let scratch = Scratch::new();
    let source = refcheck::fate("sub/dvbsubtest_filter.ts");
    let mkv = scratch.file("clock-dvb.mks");
    ffmpeg(&["-copyts", "-i", source.to_str().unwrap(), "-map", "0:s:0", "-c:s", "copy", "-f", "matroska", mkv.to_str().unwrap()]);
    for (format, path) in [("mpegts", &source), ("matroska", &mkv)] {
        let reference = oracle::ffmpeg_reference(path, 0);
        assert!(reference.cues.iter().any(|cue| cue.sub.num_rects > 0));
        assert!(reference.cues.iter().all(|cue| cue.sub.end_us().is_some()));
        check_timing(format, path, &reference);
    }
}

#[test]
fn dvd_paired_ps_and_matroska_stop_times_match_ffmpeg_exactly() {
    for (format, sample) in [
        ("vobsub", "sub/vobsub.idx"),
        ("mpeg", "sub/vobsub.sub"),
        ("matroska", "filter/242_4.mkv"),
        ("matroska", "mkv/subtitle_zlib.mks"),
    ] {
        let path = refcheck::fate(sample);
        let reference = oracle::ffmpeg_reference(&path, 0);
        assert!(!reference.cues.is_empty());
        assert!(reference.cues.iter().all(|cue| cue.sub.end_us().is_some()));
        check_timing(format, &path, &reference);
    }
}

/// Independent presentation schedule derived only from FFmpeg's subtitles.
/// Suppress redundant blank states, and expire visible states only when a
/// later cue has not replaced them. A replacement at the exact end has no
/// intermediate visible clear.
fn expected_events(reference: &oracle::Reference) -> Vec<(Duration, Option<usize>)> {
    let mut events = Vec::new();
    let mut visible = false;
    let mut end = None;
    for (index, cue) in reference.cues.iter().enumerate() {
        let start = cue.sub.start_us();
        if visible {
            if let Some(until) = end.filter(|&until| until < start) {
                events.push((Duration::from_micros(until as u64), None));
                visible = false;
            }
        }
        let next_visible = cue.canvas.chunks_exact(4).any(|pixel| pixel[3] != 0);
        if visible || next_visible {
            events.push((Duration::from_micros(start as u64), Some(index)));
        }
        visible = next_visible;
        end = cue.sub.end_us();
    }
    if visible {
        if let Some(until) = end {
            events.push((Duration::from_micros(until as u64), None));
        }
    }
    events
}

/// FFmpeg reports subtitle times in whole microseconds, rounded to nearest
/// (`av_rescale_q`); the engine keeps each stream's exact time, e.g. a
/// 90 kHz PTS of 11924914 is 132499044.4 µs. A change at FFmpeg time `at`
/// must be absent at `at - 0.5 µs - 1 ns` and present at `at + 0.5 µs`,
/// which pins the engine's boundary to FFmpeg's rounding of it.
const HALF_US: Duration = Duration::from_nanos(500);

fn check_timing(format: &str, path: &std::path::Path, reference: &oracle::Reference) {
    let ctx = Arc::new(codecs::context());
    let mut demux = if format == "vobsub" {
        subs_bitmap::open_vobsub(
            Box::new(std::fs::File::open(path).unwrap()),
            Box::new(std::fs::File::open(path.with_extension("sub")).unwrap()),
        ).unwrap()
    } else {
        ctx.containers.open_demuxer(format, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap()
    };
    let stream = demux.streams().iter().find(|stream| stream.params.media_type == MediaType::Subtitle).unwrap().clone();
    let decoder = ctx.codecs.first_decoder(&stream.params).unwrap();
    let params = stream.params.clone();
    let factory_ctx = ctx.clone();
    let lane = Lane::new();
    let demux_cv = Arc::new(Condvar::new());
    let consumer = Consumer::new(&lane, &demux_cv);
    loop {
        match demux.next_packet() {
            Ok(packet) if packet.stream_index == stream.index => {
                lane.push(QueuedPacket { packet, metadata: PacketMetadata::default() })
            }
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(error) => panic!("{format} demux: {error}"),
        }
    }
    lane.push_eof();
    let clock = Arc::new(TestClock::default());
    let stopped = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let sink = Box::new(CaptureSink { clock: clock.clone(), shows: tx });
    let pipeline = SubtitlePipeline {
        decoder,
        new_decoder: Box::new(move || factory_ctx.codecs.first_decoder(&params)),
        clock: clock.clone(),
        time_base: stream.time_base,
        video_width: reference.width as u32,
        video_height: reference.height as u32,
        realtime: true,
        lane: lane.clone(),
        demux_cv,
        seek_generation: Box::new(|| 0),
        // Subtitles alone: the lane is the only bound on the read-ahead.
        paced: Box::new(|| false),
        stopped: stopped.clone(),
        retired: Arc::new(AtomicBool::new(false)),
    };
    let handle = std::thread::spawn(move || {
        let _consumer = consumer;
        run_subtitle_loop(pipeline, sink);
    });
    let mut running = TestThread { stopped, lane: lane.clone(), handle: Some(handle) };
    let events = expected_events(reference);
    for (index, &(at, cue)) in events.iter().enumerate() {
        if index == 0 || events[index - 1].0 != at {
            clock.set(at.saturating_sub(HALF_US + Duration::from_nanos(1)), &lane);
            clock.synchronize();
            assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)), "{format}: unexpected show/clear before event {index}");
            clock.set(at + HALF_US, &lane);
        }
        let show = rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|error| panic!("{format} event {index}: {error}"));
        assert_eq!(show.at, at + HALF_US, "{format} event {index}: media-time boundary within FFmpeg's microsecond rounding");
        match cue {
            Some(cue) => show.assert_canvas(reference, cue),
            None => {
                assert!(show.blank, "{format} event {index}: timeout must clear");
                assert_eq!((show.width, show.height), (reference.width, reference.height));
                assert!(show.canvas.iter().all(|&byte| byte == 0), "complete expired canvas must be transparent");
            }
        }
    }
    // Reach trailing blank states, but never advance to their nominal page
    // timeouts: once blank, nothing remains to expire and EOF must finish.
    let final_cue = Duration::from_micros(reference.cues.last().unwrap().sub.start_us() as u64);
    let final_event = events.last().map_or(Duration::ZERO, |event| event.0);
    clock.set(final_cue.max(final_event) + HALF_US, &lane);
    running.handle.take().unwrap().join().unwrap();
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)), "{format}: extra trailing show/clear");
}

#[test]
fn dvb_final_visible_state_expires_at_ffmpeg_end_exactly() {
    let scratch = Scratch::new();
    let source = refcheck::fate("sub/dvbsubtest_filter.ts");
    let path = scratch.file("clock-dvb-timeout.mks");
    let first_visible = oracle::ffprobe_subtitles(&source, 0).iter().position(|cue| cue.num_rects > 0).unwrap();
    let count = (first_visible + 1).to_string();
    // A real prefix ending with a visible DVB subtitle, with no later
    // replacement/blank to hide an incorrect timeout. FFmpeg supplies both
    // the unchanged bitmap and the last state's finite end.
    ffmpeg(&[
        "-copyts", "-i", source.to_str().unwrap(), "-map", "0:s:0", "-c:s", "copy",
        "-frames:s", &count, "-f", "matroska", path.to_str().unwrap(),
    ]);
    let reference = oracle::ffmpeg_reference(&path, 0);
    assert_eq!(reference.cues.len(), first_visible + 1);
    assert!(reference.cues.last().unwrap().sub.num_rects > 0);
    assert!(expected_events(&reference).last().unwrap().1.is_none(), "must exercise an actual expiry");
    check_timing("matroska", &path, &reference);
}
