//! Exact logical media-time evidence for the real subtitle consumer. The
//! injected clock is held at each boundary, so OS scheduling cannot turn a
//! correctly timed state change into a late wall-clock measurement.

#[path = "../../tests/support/bitmap.rs"]
mod bitmap;

use std::sync::mpsc::{self, Sender};

use bitmap::{Scratch, Show, ffmpeg, oracle, pgs_with_clears};
use oxideav_core::Error;

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
        let mut ctx = codecs::context();
        subs_bitmap::register(&mut ctx);
        let ctx = Arc::new(ctx);
        let mut demux = ctx.containers.open_demuxer(format, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap();
        let stream = demux.streams()[0].clone();
        let decoder = ctx.codecs.first_decoder(&stream.params).unwrap();
        let params = stream.params.clone();
        let factory_ctx = ctx.clone();
        let lane = Lane::new();
        let demux_cv = Arc::new(Condvar::new());
        let consumer = Consumer::new(&lane, &demux_cv);
        loop {
            match demux.next_packet() {
                Ok(packet) => lane.push(packet),
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
            video_width: 160,
            video_height: 90,
            realtime: true,
            lane: lane.clone(),
            demux_cv,
            seek_generation: Box::new(|| 0),
            stopped: stopped.clone(),
            retired: Arc::new(AtomicBool::new(false)),
        };
        let handle = std::thread::spawn(move || {
            let _consumer = consumer;
            run_subtitle_loop(pipeline, sink);
        });
        let mut running = TestThread { stopped, lane: lane.clone(), handle: Some(handle) };
        for (index, expected) in reference.cues.iter().enumerate() {
            let at = Duration::from_micros(expected.sub.start_us() as u64);
            clock.set(at - Duration::from_micros(1), &lane);
            clock.synchronize();
            assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)), "{format}: unexpected show/clear before event {index}");
            clock.set(at, &lane);
            let show = rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|error| panic!("{format} event {index}: {error}"));
            assert_eq!(show.at, at, "{format} event {index}: exact media-time boundary");
            show.assert_canvas(&reference, index);
        }
        running.handle.take().unwrap().join().unwrap();
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)), "{format}: extra trailing show/clear");
    }
}
