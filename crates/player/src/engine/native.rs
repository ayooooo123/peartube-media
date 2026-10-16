//! The native route as the engine plays it (see `crate::native`): the
//! platform player presents the video and audio from the HLS a session
//! serves, the playback's clock follows the platform player's item, and
//! the engine's subtitle pipeline draws on top from the engine's own
//! demuxer. A playback the platform player cannot start goes back to the
//! decode route.

use super::*;
use crate::backend::{NativePresenter, NativeStatus};
use crate::native::session::{self, Session};

/// From opening a playlist to AVPlayer's first segment request, and from
/// the first segment handed to it to its first picture.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
/// At the end, AVPlayer waiting this long for media past the last segment
/// means the playback is over (it never posts its end for a VOD playlist
/// whose last picture ends before the playlist's duration).
const TAIL_GRACE: Duration = Duration::from_secs(3);
/// Reloads at the position after the platform player failed mid-playback.
const MAX_RELOADS: u32 = 2;
/// A seek whose completion has not come this long after the item stood
/// ready, with a picture, at its target counts as landed.
const LANDING_GRACE: Duration = Duration::from_secs(2);

pub(super) enum Outcome {
    /// The playback ended, failed or was dropped.
    Finished,
    /// The decode route plays it, from `at` when that is set.
    Declined { demuxer: Box<dyn Demuxer>, at: Option<Duration> },
}

/// What the decode route would play.
pub(super) struct Selection {
    pub(super) video: Option<u32>,
    pub(super) audio: Option<u32>,
    pub(super) subtitle: Option<u32>,
    pub(super) select_gen: u64,
}

fn eligible_video(stream: &StreamInfo) -> bool {
    matches!(stream.params.codec_id.as_str(), "h264" | "h265" | "hevc")
}

fn log(message: &str) {
    eprintln!("[peartube-native] {message}");
}

/// Plays the selection natively, or declines before anything played.
#[allow(clippy::too_many_arguments)]
pub(super) fn play(
    shared: &Arc<SharedState>,
    presenter: Arc<dyn NativePresenter>,
    url: &str,
    container: &str,
    demuxer: Box<dyn Demuxer>,
    streams: &[StreamInfo],
    selection: Selection,
    duration: Option<Duration>,
    options: &PlayerOptions,
) -> Outcome {
    let video = find_stream(streams, selection.video);
    let audio = find_stream(streams, selection.audio);
    let Some(video) = video.filter(eligible_video) else {
        return Outcome::Declined { demuxer, at: None };
    };
    if std::env::var_os("PEARTUBE_NO_NATIVE").is_some() {
        return Outcome::Declined { demuxer, at: None };
    }
    let keep = |monitor: SourceMonitor| shared.native_sources.lock().push(monitor);
    let prepared = session::prepare(
        &shared.ctx,
        url,
        container,
        &video,
        audio.as_ref(),
        duration.map_or(f64::NAN, |d| d.as_secs_f64()),
        &keep,
        &|demuxer| decode_order::wrap(demuxer),
        bridge_provider(&shared.ctx),
        &shared.stopped,
    );
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(reason) => {
            stop_native_sources(shared);
            if shared.stopped.load(Ordering::SeqCst) {
                return Outcome::Finished;
            }
            log(&format!("decode route: {reason}"));
            return Outcome::Declined { demuxer, at: None };
        }
    };
    let codec = prepared.video_config().codec_string.clone();
    let session = match Session::start(prepared) {
        Ok(session) => session,
        Err(reason) => {
            stop_native_sources(shared);
            log(&format!("decode route: {reason}"));
            return Outcome::Declined { demuxer, at: None };
        }
    };
    log(&format!("AVPlayer route: {codec}, {}", session.url()));
    {
        let mut state = shared.state.lock();
        state.video_decoder = Some(format!("native {codec}"));
    }
    shared.beside_media.store(true, Ordering::SeqCst);
    notify_changed(shared);

    let route = Route {
        shared,
        presenter,
        session: Some(session),
        streams,
        video,
        audio: selection.audio,
        subtitle: selection.subtitle,
        select_gen: selection.select_gen,
        demuxer: Some(demuxer),
        feeder: None,
        options,
        media_direct: false,
    };
    route.run()
}

/// The engine's audio decoders, for tracks the native route bridges to FLAC.
fn bridge_provider(ctx: &Arc<RuntimeContext>) -> session::BridgeProvider {
    let ctx = Arc::clone(ctx);
    Arc::new(move |stream: &StreamInfo| bridge::factory(Arc::clone(&ctx), stream.clone()))
}

fn stop_native_sources(shared: &SharedState) {
    for monitor in shared.native_sources.lock().drain(..) {
        monitor.stop();
    }
}

/// Item time (the platform player's) as the playback's clock: item time 0
/// is the source time `origin`.
struct ItemClock {
    item: Arc<dyn Clock>,
    origin: f64,
}

impl Clock for ItemClock {
    fn now(&self) -> Option<Duration> {
        let at = self.item.now()?.as_secs_f64() + self.origin;
        (at >= 0.0).then(|| Duration::from_secs_f64(at))
    }

    fn monotonic_ns_at(&self, at: Duration) -> Option<i64> {
        let item = at.as_secs_f64() - self.origin;
        (item >= 0.0).then(|| self.item.monotonic_ns_at(Duration::from_secs_f64(item))).flatten()
    }
}

/// Wakes the control loop when the platform player's status changes.
#[derive(Default)]
struct Wake {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Wake {
    fn notify(&self) {
        *self.flag.lock() = true;
        self.cv.notify_all();
    }

    fn wait(&self, timeout: Duration) {
        let mut flag = self.flag.lock();
        if !*flag {
            self.cv.wait_for(&mut flag, timeout);
        }
        *flag = false;
    }
}

struct Route<'a> {
    shared: &'a Arc<SharedState>,
    presenter: Arc<dyn NativePresenter>,
    /// Until `teardown`.
    session: Option<Session>,
    streams: &'a [StreamInfo],
    video: StreamInfo,
    audio: Option<u32>,
    subtitle: Option<u32>,
    select_gen: u64,
    /// The engine's demuxer while no subtitle feeder holds it.
    demuxer: Option<Box<dyn Demuxer>>,
    feeder: Option<Feeder>,
    options: &'a PlayerOptions,
    /// AVPlayer refused the master playlist: it gets the media playlist.
    media_direct: bool,
}

impl Route<'_> {
    fn session(&self) -> &Session {
        self.session.as_ref().expect("the session runs until teardown")
    }

    fn playlist(&self) -> String {
        if self.media_direct { self.session().media_url() } else { self.session().url() }
    }

    fn item(&self, source: f64) -> Duration {
        Duration::from_secs_f64((source - self.session().origin_secs()).max(0.0))
    }

    fn clock(&self) -> Arc<dyn Clock> {
        Arc::new(ItemClock { item: self.presenter.clock(), origin: self.session().origin_secs() })
    }

    fn position(&self) -> f64 {
        self.shared.master.now().map_or(0.0, |d| d.as_secs_f64())
    }

    fn run(mut self) -> Outcome {
        let shared = self.shared;
        let wake = Arc::new(Wake::default());
        let notifier = Arc::clone(&wake);
        self.presenter.set_on_change(Some(Arc::new(move || notifier.notify())));
        // The generation first: a seek after it is applied below, not lost.
        let mut seen_seek = shared.seek_gen.load(Ordering::SeqCst);
        // A seek made before the route opened holds the clock at its target.
        let start = self.position();
        if let Some(stream) = find_stream(self.streams, self.subtitle) {
            self.start_feeder(stream, Duration::from_secs_f64(start.max(0.0)));
        }
        self.presenter.open(&self.playlist(), self.item(start));
        // The open's own seek has no serial: seek again to learn when the
        // item stands at the start.
        let mut landing = Some((self.presenter.seek(self.item(start)), seen_seek));
        let mut following = false;
        let mut first_picture = false;
        let mut served_at: Option<Instant> = None;
        // When the playlist AVPlayer starts from was opened.
        let mut opened_at = Instant::now();
        let mut play_sent: Option<bool> = None;
        let mut reloads = 0;
        let mut tail_since: Option<Instant> = None;
        // Since when a pending landing's item stands ready at its target.
        let mut standing_since: Option<Instant> = None;
        // The held position was taken from the item after it stopped.
        let mut reheld = false;

        loop {
            if shared.stopped.load(Ordering::SeqCst) {
                self.teardown();
                return Outcome::Finished;
            }
            let status: NativeStatus = self.presenter.status();
            let position = self.position();

            if let Some(error) = &status.failed {
                if !first_picture && !self.media_direct {
                    log(&format!("AVPlayer refused the master playlist ({error}): the media playlist"));
                    self.media_direct = true;
                    self.switch_item(position, &mut landing);
                    following = false;
                    play_sent = None;
                    served_at = None;
                    opened_at = Instant::now();
                    continue;
                }
                if !first_picture {
                    log(&format!("AVPlayer failed before the first picture ({error}): decode route"));
                    return self.decline(Some(position));
                }
                if reloads < MAX_RELOADS {
                    reloads += 1;
                    log(&format!("AVPlayer failed ({error}): reloading at {position:.3} s"));
                    self.reopen(position, &mut seen_seek, &mut landing);
                    following = false;
                    play_sent = None;
                    continue;
                }
                let error = format!("AVPlayer: {error}");
                self.teardown();
                set_error(shared, error);
                return Outcome::Finished;
            }
            if !first_picture {
                if served_at.is_none() && self.session().served_any() {
                    served_at = Some(Instant::now());
                }
                // Parked on the playlists: no segment asked for at all.
                let parked = !self.session().requested_any() && opened_at.elapsed() > STARTUP_TIMEOUT;
                if parked && !self.media_direct {
                    log("AVPlayer asked for no segment from the master playlist: the media playlist");
                    self.media_direct = true;
                    self.switch_item(position, &mut landing);
                    following = false;
                    play_sent = None;
                    opened_at = Instant::now();
                    continue;
                }
                if parked {
                    log("AVPlayer asked for no segment: decode route");
                    return self.decline(Some(position));
                }
                if served_at.is_some_and(|at| at.elapsed() > STARTUP_TIMEOUT) {
                    log("AVPlayer showed no picture: decode route");
                    return self.decline(Some(position));
                }
            }

            // A seek: the item seeks with zero tolerance, the clock holds
            // at the target until it landed.
            let generation = shared.seek_gen.load(Ordering::SeqCst);
            if generation != seen_seek {
                seen_seek = generation;
                let target = shared.seek_target.lock().take().map_or(Duration::ZERO, |t| t.target);
                landing = Some((self.presenter.seek(self.item(target.as_secs_f64())), generation));
                following = false;
                tail_since = None;
                if let Some(feeder) = &self.feeder {
                    feeder.seek(generation, target);
                }
            }
            if let Some((serial, generation)) = landing {
                let target = self.item(shared.master.now().map_or(position, |d| d.as_secs_f64()));
                let standing = status.ready
                    && status.picture
                    && status.position.is_some_and(|at| at.abs_diff(target) < Duration::from_millis(250));
                standing_since = if standing { standing_since.or(Some(Instant::now())) } else { None };
                let unconfirmed = standing_since.is_some_and(|since| since.elapsed() > LANDING_GRACE);
                if unconfirmed && status.seek_done < serial {
                    log(&format!("seek {serial} never reported its end; the item stands at its target"));
                }
                if status.ready && (status.seek_done >= serial || unconfirmed) {
                    landing = None;
                    standing_since = None;
                    if generation == shared.seek_gen.load(Ordering::SeqCst) {
                        shared.master.follow_audio(self.clock(), generation);
                        following = true;
                    }
                }
            }

            let paused = shared.paused();
            if following && play_sent != Some(!paused) {
                if paused {
                    self.presenter.pause();
                } else {
                    self.presenter.play();
                }
                play_sent = Some(!paused);
            }
            if status.picture {
                first_picture = true;
            }
            if !paused || !following {
                reheld = false;
            } else if !reheld && !status.playing && !status.waiting {
                shared.master.rehold();
                reheld = true;
            }
            // The clock runs while the item has landed and is not stalled.
            shared.native_hold(!following || status.waiting);

            if status.ended {
                self.teardown();
                set_ended(shared);
                return Outcome::Finished;
            }
            let at_end = position >= self.session().end_secs() - 0.5;
            if following && !paused && status.waiting && at_end && self.session().served_last() {
                if tail_since.get_or_insert_with(Instant::now).elapsed() > TAIL_GRACE {
                    self.teardown();
                    set_ended(shared);
                    return Outcome::Finished;
                }
            } else {
                tail_since = None;
            }

            let select_gen = shared.select_gen.load(Ordering::SeqCst);
            if select_gen != self.select_gen {
                self.select_gen = select_gen;
                if let Some(outcome) = self.apply_selection(position, &mut landing) {
                    return outcome;
                }
                following = following && landing.is_none();
                play_sent = if landing.is_some() { None } else { play_sent };
            }
            wake.wait(Duration::from_millis(50));
        }
    }

    /// Loads the current playlist again at `position`.
    fn reopen(&mut self, position: f64, seen_seek: &mut u64, landing: &mut Option<(u64, u64)>) {
        let at = Duration::from_secs_f64(position.max(0.0));
        self.presenter.open(&self.playlist(), self.item(position));
        // Hold the clock at the position until the new item landed there.
        self.shared.request_seek(at);
        *seen_seek = self.shared.seek_gen.load(Ordering::SeqCst);
        *landing = Some((self.presenter.seek(self.item(position)), *seen_seek));
        if let Some(feeder) = &self.feeder {
            feeder.seek(*seen_seek, at);
        }
    }

    /// Loads the session's new playlist at `position` without a seek: the
    /// clock holds where the old item stood, and the subtitles (their
    /// pipeline and feeder follow seek generations) carry on.
    fn switch_item(&mut self, position: f64, landing: &mut Option<(u64, u64)>) {
        let shared = self.shared;
        shared.native_hold(true);
        shared.master.release_audio();
        self.presenter.open(&self.playlist(), self.item(position));
        let generation = shared.seek_gen.load(Ordering::SeqCst);
        *landing = Some((self.presenter.seek(self.item(position)), generation));
    }

    /// Applies `select_audio` / `select_subtitle`. `Some`: the playback
    /// leaves the native route.
    fn apply_selection(&mut self, position: f64, landing: &mut Option<(u64, u64)>) -> Option<Outcome> {
        let shared = self.shared;
        let audio = match *shared.wanted_audio.lock() {
            AudioChoice::Default => self.audio,
            AudioChoice::Chosen(stream) => stream,
        };
        let subtitle = *shared.wanted_subtitle.lock();
        if audio != self.audio {
            let stream = find_stream(self.streams, audio);
            let switched = match self.session.as_mut() {
                Some(session) => session.switch_audio(stream.as_ref(), position, &shared.stopped),
                None => Err("no session".into()),
            };
            if let Err(reason) = switched {
                log(&format!("audio switch: {reason}: decode route"));
                return Some(self.decline(Some(position)));
            }
            self.audio = audio;
            shared.state.lock().audio = audio;
            // The old item played on while the switch read the new track.
            self.switch_item(self.position(), landing);
        }
        if subtitle != self.subtitle {
            if let Some(feeder) = self.feeder.take() {
                self.demuxer = Some(feeder.stop(shared));
            }
            self.subtitle = subtitle;
            shared.state.lock().subtitle = subtitle;
            if let Some(stream) = find_stream(self.streams, subtitle) {
                self.start_feeder(stream, Duration::from_secs_f64(position.max(0.0)));
            }
        }
        notify_changed(shared);
        None
    }

    fn start_feeder(&mut self, stream: StreamInfo, at: Duration) {
        if let Some(demuxer) = self.demuxer.take() {
            self.feeder = Some(Feeder::start(self.shared, demuxer, stream, &self.video, self.options, at));
        }
    }

    /// Stops everything the route runs and hands back the engine's demuxer.
    fn teardown(&mut self) -> Option<Box<dyn Demuxer>> {
        let shared = self.shared;
        // The clock carries on from the item's last time: read it before
        // the item goes.
        shared.master.release_audio();
        self.presenter.set_on_change(None);
        self.presenter.close();
        // A segment read waiting on the network fails instead.
        stop_native_sources(shared);
        if let Some(session) = self.session.take() {
            session.stop();
        }
        if let Some(feeder) = self.feeder.take() {
            self.demuxer = Some(feeder.stop(shared));
        }
        shared.lanes.lock().clear();
        self.demuxer.take()
    }

    /// Hands the playback to the decode route at `at`.
    fn decline(&mut self, at: Option<f64>) -> Outcome {
        let demuxer = self.teardown();
        let shared = self.shared;
        {
            let mut state = shared.state.lock();
            state.video_decoder = Some(self.video.params.codec_id.as_str().to_string());
        }
        shared.beside_media.store(false, Ordering::SeqCst);
        shared.native_hold(true);
        match demuxer {
            Some(demuxer) => Outcome::Declined { demuxer, at: at.map(|s| Duration::from_secs_f64(s.max(0.0))) },
            None => {
                set_error(shared, "native route: the demuxer was lost".into());
                Outcome::Finished
            }
        }
    }
}

/// Feeds the engine's subtitle pipeline from the engine's demuxer while
/// the platform player plays the media: at most `QUEUE_MAX_SECS` ahead of
/// the clock, starting over at each seek.
struct Feeder {
    control: Arc<FeedControl>,
    thread: JoinHandle<Box<dyn Demuxer>>,
    pipeline: PipelineThread,
    lane: Arc<Lane>,
}

struct FeedControl {
    stop: AtomicBool,
    /// A seek to apply: generation and source time.
    seek: Mutex<Option<(u64, Duration)>>,
    cv: Condvar,
}

impl Feeder {
    fn start(
        shared: &Arc<SharedState>,
        mut demuxer: Box<dyn Demuxer>,
        stream: StreamInfo,
        video: &StreamInfo,
        options: &PlayerOptions,
        at: Duration,
    ) -> Feeder {
        let lane = Lane::new();
        let demux_cv = Arc::new(Condvar::new());
        *shared.lanes.lock() = vec![Arc::clone(&lane)];
        let generation = shared.seek_gen.load(Ordering::SeqCst);
        lane.clear_for_seek(generation);
        let pipeline = spawn_subtitles(shared, stream.clone(), &lane, &demux_cv, true, &options.fonts, demuxer.attachments(), true);
        demuxer.set_active_streams(&[stream.index]);
        let control = Arc::new(FeedControl {
            stop: AtomicBool::new(false),
            seek: Mutex::new(Some((generation, at))),
            cv: Condvar::new(),
        });
        let (shared, feed_lane, feed_control) = (Arc::clone(shared), Arc::clone(&lane), Arc::clone(&control));
        let (video_index, video_tb) = (video.index, video.time_base);
        let thread = std::thread::Builder::new()
            .name("peartube-native-subtitles".into())
            .spawn(move || feed(&shared, demuxer, stream.index, video_index, video_tb, &feed_lane, &feed_control))
            .expect("failed to spawn the subtitle feeder");
        Feeder { control, thread, pipeline, lane }
    }

    fn seek(&self, generation: u64, to: Duration) {
        *self.control.seek.lock() = Some((generation, to));
        self.control.cv.notify_all();
    }

    fn stop(self, shared: &SharedState) -> Box<dyn Demuxer> {
        self.control.stop.store(true, Ordering::SeqCst);
        self.control.cv.notify_all();
        self.pipeline.retire(shared, &self.lane);
        self.thread.join().expect("the subtitle feeder panicked")
    }
}

fn feed(
    shared: &SharedState,
    mut demuxer: Box<dyn Demuxer>,
    stream: u32,
    video: u32,
    video_tb: TimeBase,
    lane: &Lane,
    control: &FeedControl,
) -> Box<dyn Demuxer> {
    let clock = shared.sink_clock();
    let mut eof = false;
    let mut pushed_to: Option<f64> = None;
    loop {
        if control.stop.load(Ordering::SeqCst) || shared.stopped.load(Ordering::SeqCst) {
            return demuxer;
        }
        if let Some((generation, target)) = control.seek.lock().take() {
            lane.clear_for_seek(generation);
            // Subtitle tracks seldom have cues of their own: land by the
            // video's.
            let ticks = video_tb.ticks_of(target.as_secs_f64());
            let demux = &mut demuxer;
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| demux.seek_to(video, ticks)));
            eof = false;
            pushed_to = None;
            continue;
        }
        let now = clock.now().map(|d| d.as_secs_f64());
        let ahead = matches!((pushed_to, now), (Some(pushed), Some(now)) if pushed > now + QUEUE_MAX_SECS);
        if eof || ahead {
            let mut seek = control.seek.lock();
            if seek.is_none() && !control.stop.load(Ordering::SeqCst) {
                control.cv.wait_for(&mut seek, Duration::from_millis(100));
            }
            continue;
        }
        match read_packet(&mut *demuxer) {
            Ok(Ok(queued)) => {
                if queued.packet.stream_index != stream {
                    continue;
                }
                if let Some(ticks) = queued.packet.pts.or(queued.packet.dts) {
                    pushed_to = Some(queued.packet.time_base.seconds_of(ticks));
                }
                lane.push(queued);
            }
            Ok(Err(oxideav_core::Error::Eof)) => {
                lane.push_eof();
                eof = true;
            }
            Ok(Err(e)) => {
                if !shared.stopped.load(Ordering::SeqCst) {
                    log(&format!("subtitles: {e}"));
                }
                eof = true;
            }
            Err(_) => {
                log("subtitles: the demuxer panicked");
                eof = true;
            }
        }
    }
}
