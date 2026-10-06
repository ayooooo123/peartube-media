//! When the playback clock runs: the user's play/pause intent plus the
//! buffering hold. The clock stands still at the start until the first
//! frames are ready, after a seek, and whenever a pipeline runs dry while
//! the source has no bytes to give, until about a second of media is
//! buffered past it (or the input ends). Play/pause stay the user's intent:
//! the hold never changes them.
//!
//! The clock itself is the playback's `MasterClock`: the audio output's
//! clock while audio plays (the audio pipeline pauses its output while the
//! clock is held), the free-running clock otherwise. Everything that waits
//! for a media time here re-reads it on every wake, so it follows an audio
//! output that runs fast or slow, and stops with it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{notify_changed_now, SharedState};
use crate::backend::Clock;
use crate::clock::current_monotonic_ns;

/// Media demuxed past the clock before a hold lets go.
const BUFFER_AHEAD_SECS: f64 = 1.0;
/// The most a decoded video frame goes to its sink before it is due (see
/// `VideoSink::frame_lead`); frames later than this past due are dropped
/// (CPU starvation, never data starvation: the clock holds before it passes
/// media that has not arrived).
const VIDEO_LEAD: Duration = Duration::from_millis(100);
/// While the clock stands still, audio and compressed video are handed to
/// their sinks at most this far past it: enough to have the output primed,
/// short of filling a paused output so far that `write`/`push_packet` block.
const PREROLL: Duration = Duration::from_millis(100);
/// The longest timed wait for a media time: it is re-evaluated at least
/// this often, whatever the clock's mapping said.
const MAX_WAIT: Duration = Duration::from_millis(100);
/// The audio's end counts as heard this close to it (the stamps are
/// rounded to the container's time base, the clock counts samples).
const END_SLACK: Duration = Duration::from_millis(5);
/// A running clock that stands still this long has played all its output.
const DRAINED: Duration = Duration::from_millis(100);

/// The pipelines whose data the clock waits for (subtitles never hold it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pipe {
    Video = 0,
    Audio = 1,
}

#[derive(Debug, Default)]
struct PipeState {
    /// Running pipeline threads (at most one: a selection switch retires
    /// the old thread before it starts the new one).
    live: u32,
    /// Waiting on an empty lane that has not reached its end.
    starved: bool,
    /// Seek generation for which the pipeline has had output ready.
    primed: Option<u64>,
    /// End (seconds) of the newest packet demuxed for the pipeline since the
    /// demuxer last seeked.
    horizon: Option<f64>,
}

/// The clock's run state; `SharedState::transport` guards it.
#[derive(Debug)]
pub(super) struct Transport {
    /// The user's intent: `pause()` until the next `play()`.
    paused: bool,
    /// Holding the clock for data. Mirrored in `State::buffering`.
    buffering: bool,
    /// The pipelines exist; until then the source is being probed/opened.
    started: bool,
    /// Playback ended or failed: the clock stops for good.
    done: bool,
    /// The clock runs (not paused, not buffering, not done): the free clock
    /// plays, and the audio pipeline plays its output.
    running: bool,
    /// Latest seek generation, and the one the demuxer has applied.
    seek_gen: u64,
    demux_gen: u64,
    /// The demuxer is blocked in a read that waits for bytes.
    source_starved: bool,
    /// The demuxer reached the end of the input.
    demux_eof: bool,
    /// The demuxer waits because a lane is full: it cannot buffer more.
    demux_full: bool,
    pipes: [PipeState; 2],
}

impl Transport {
    pub(super) fn new() -> Self {
        Self {
            paused: false,
            buffering: true,
            started: false,
            done: false,
            running: false,
            seek_gen: 0,
            demux_gen: 0,
            source_starved: false,
            demux_eof: false,
            demux_full: false,
            pipes: Default::default(),
        }
    }

    fn pipe(&mut self, pipe: Pipe) -> &mut PipeState {
        &mut self.pipes[pipe as usize]
    }
}

/// When a decoded video frame may go to the sink.
pub(super) enum Due {
    Now,
    /// More than `VIDEO_LEAD` past due: drop it.
    Late,
    /// The player stopped or a seek superseded the frame.
    Abort,
}

/// Whether media may go to a sink while the clock stands still.
pub(super) enum Preroll {
    Go,
    /// The clock's run state changed: apply it to the sink, then ask again.
    Resync,
    /// The player stopped, the pipeline was retired, or a seek superseded
    /// the media.
    Abort,
}

/// Counts a pipeline thread as live from spawn until the thread ends,
/// however it ends.
pub(super) struct Live {
    shared: Arc<SharedState>,
    pipe: Pipe,
}

impl Live {
    pub(super) fn new(shared: &Arc<SharedState>, pipe: Pipe) -> Live {
        shared.update(|t| t.pipe(pipe).live += 1);
        Live {
            shared: Arc::clone(shared),
            pipe,
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let pipe = self.pipe;
        self.shared.update(|t| {
            let p = t.pipe(pipe);
            p.live = p.live.saturating_sub(1);
            if p.live == 0 {
                p.starved = false;
            }
        });
    }
}

impl SharedState {
    /// The clock runs: neither paused, buffering nor done.
    pub(super) fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Applies `f`, re-derives the hold and the clock's run state, and wakes
    /// everything that waits on them. `State::buffering` follows the hold;
    /// each change of it is reported as `Event::Changed`.
    fn update(&self, f: impl FnOnce(&mut Transport)) {
        let (changed, run_changed, buffering_changed) = {
            let mut t = self.transport.lock();
            let before = (t.paused, t.buffering, t.seek_gen);
            f(&mut t);
            self.rebuffer(&mut t);
            let run = !t.paused && !t.buffering && !t.done && !self.stopped.load(Ordering::SeqCst);
            let run_changed = run != t.running;
            if run_changed {
                t.running = run;
                self.master.set_running(run);
                self.running.store(run, Ordering::SeqCst);
            }
            let buffering_changed = before.1 != t.buffering;
            if buffering_changed {
                self.state.lock().buffering = t.buffering;
            }
            let changed = run_changed || before != (t.paused, t.buffering, t.seek_gen);
            (changed, run_changed, buffering_changed)
        };
        if changed {
            self.transport_cv.notify_all();
        }
        if run_changed {
            // Pipelines idle on an empty lane still have to pause or resume
            // their sinks.
            self.wake_lanes();
        }
        if buffering_changed {
            notify_changed_now(self);
        }
    }

    fn wake_lanes(&self) {
        for lane in self.lanes.lock().iter() {
            drop(lane.queue.lock());
            lane.cv.notify_all();
        }
    }

    /// Enters or leaves the buffering hold.
    fn rebuffer(&self, t: &mut Transport) {
        if t.done {
            t.buffering = false;
            return;
        }
        if !t.started || self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let live = |p: &&PipeState| p.live > 0;
        if !t.buffering {
            // Underrun: a pipeline ran dry and the source has no bytes for
            // the demuxer. Stop before the clock passes media that has not
            // arrived (released again below if enough is in fact buffered).
            let dry = t.pipes.iter().filter(live).any(|p| p.starved);
            if !(dry && t.source_starved && !t.demux_eof) {
                return;
            }
            t.buffering = true;
        }
        if t.demux_gen != t.seek_gen {
            // The demuxer has not seeked yet: whatever is queued is stale.
            return;
        }
        let generation = t.seek_gen;
        let ready = |p: &&PipeState| p.primed == Some(generation);
        if t.demux_full {
            // The demuxer cannot queue more until the clock moves: go once
            // every pipeline has output, except those waiting on an empty
            // lane (they cannot get any before the demuxer moves on).
            if t.pipes.iter().filter(live).all(|p| ready(&p) || p.starved) {
                t.buffering = false;
            }
            return;
        }
        let now = self.master.now().unwrap_or_default().as_secs_f64();
        let primed = t.pipes.iter().filter(live).all(|p| ready(&p));
        let enough = t.demux_eof
            || t.pipes
                .iter()
                .filter(live)
                .all(|p| p.horizon.is_some_and(|h| h >= now + BUFFER_AHEAD_SECS));
        if primed && enough {
            t.buffering = false;
        }
    }

    /// `play()` / `pause()`.
    pub(super) fn set_paused(&self, paused: bool) {
        self.update(|t| t.paused = paused);
    }

    /// `seek()`: the clock jumps to `to` and holds until the pipelines have
    /// output from there and enough is buffered. The free clock leads until
    /// the audio plays from `to`.
    pub(super) fn seek_clock(&self, to: Duration) {
        self.update(|t| {
            t.seek_gen = self.seek_gen.load(Ordering::SeqCst);
            self.master.seek(to, t.seek_gen);
            for p in &mut t.pipes {
                p.horizon = None;
            }
            t.demux_eof = false;
            t.demux_full = false;
            t.buffering = true;
        });
    }

    /// The pipelines are running: from now on the hold follows their data.
    pub(super) fn pipelines_started(&self) {
        self.update(|t| t.started = true);
    }

    /// Playback ended or failed: the clock stops where it is.
    pub(super) fn finish(&self) {
        self.update(|t| t.done = true);
    }

    /// `finish` ran: the playback ended or failed.
    pub(super) fn finished(&self) -> bool {
        self.transport.lock().done
    }

    /// The player is being dropped (`stopped` is set): wake every waiter.
    pub(super) fn stop(&self) {
        self.update(|_| {});
        self.wake_clock_waiters();
        self.wake_lanes();
    }

    /// Wakes every thread waiting on the clock, e.g. a retired pipeline.
    pub(super) fn wake_clock_waiters(&self) {
        drop(self.transport.lock());
        self.transport_cv.notify_all();
    }

    pub(super) fn pipe_starved(&self, pipe: Pipe, starved: bool) {
        self.update(|t| t.pipe(pipe).starved = starved);
    }

    /// The pipeline has output ready (decoded and past the seek target) for
    /// seek generation `generation`.
    pub(super) fn pipe_primed(&self, pipe: Pipe, generation: u64) {
        self.update(|t| t.pipe(pipe).primed = Some(generation));
    }

    pub(super) fn source_starved(&self, starved: bool) {
        self.update(|t| t.source_starved = starved);
    }

    /// The demuxer queued media for `pipe` up to `end` seconds.
    pub(super) fn demuxed(&self, pipe: Pipe, end: f64) {
        self.update(|t| {
            let h = &mut t.pipe(pipe).horizon;
            *h = Some(h.map_or(end, |h| h.max(end)));
        });
    }

    /// The demuxer applied seek `generation`: the lanes are empty.
    pub(super) fn demux_seeked(&self, generation: u64) {
        self.update(|t| {
            t.demux_gen = generation;
            for p in &mut t.pipes {
                p.horizon = None;
            }
            t.demux_eof = false;
            t.demux_full = false;
        });
    }

    pub(super) fn demux_eof(&self, eof: bool) {
        self.update(|t| t.demux_eof = eof);
    }

    pub(super) fn demux_full(&self, full: bool) {
        self.update(|t| t.demux_full = full);
    }

    /// Waits until a decoded video frame at `pts` should go to its sink:
    /// `lead` before `pts` on the clock (at most `VIDEO_LEAD`); more than
    /// `VIDEO_LEAD` past `pts` it is late. Re-reads the clock on every wake:
    /// its run state changing, or the time its mapping gives for the frame.
    pub(super) fn wait_due(
        &self,
        pts: Duration,
        lead: Duration,
        seen_seek: u64,
        retired: &AtomicBool,
    ) -> Due {
        #[cfg(target_os = "macos")]
        let _timing = crate::clock::timing::Guard::enter();
        let due = pts.saturating_sub(lead.min(VIDEO_LEAD));
        let mut t = self.transport.lock();
        loop {
            if self.superseded(seen_seek, retired) {
                return Due::Abort;
            }
            let now = self.master.now().unwrap_or_default();
            if now > pts + VIDEO_LEAD {
                return Due::Late;
            }
            if due <= now {
                return Due::Now;
            }
            if t.running {
                let wait = self.until(due, now);
                self.transport_cv.wait_for(&mut t, wait);
            } else {
                self.transport_cv.wait(&mut t);
            }
        }
    }

    /// How long until the clock reaches `at` (it reads `now`), for a timed
    /// wait: from the clock's own mapping when it has one (an audio output's
    /// timestamps follow a device that runs fast or slow), else the media
    /// time between. Between 1 ms and `MAX_WAIT`: the caller re-reads the
    /// clock when it wakes.
    fn until(&self, at: Duration, now: Duration) -> Duration {
        let wait = match self.master.monotonic_ns_at(at) {
            Some(ns) => Duration::from_nanos((ns - current_monotonic_ns()).max(0) as u64),
            None => at.saturating_sub(now),
        };
        wait.clamp(Duration::from_millis(1), MAX_WAIT)
    }

    /// While the clock stands still, waits until media at `pts` is within
    /// `PREROLL` of it. `Resync` as soon as the clock's run state differs
    /// from `applied`, the one the caller's sink follows: an audio output
    /// left playing while the clock is held would run the clock on.
    pub(super) fn preroll(
        &self,
        pts: Duration,
        applied: Option<bool>,
        ahead: Option<Duration>,
        seen_seek: u64,
        retired: &AtomicBool,
    ) -> Preroll {
        let mut t = self.transport.lock();
        loop {
            if self.superseded(seen_seek, retired) {
                return Preroll::Abort;
            }
            if applied != Some(t.running) {
                return Preroll::Resync;
            }
            let now = self.master.now().unwrap_or_default();
            if t.running {
                if let Some(ahead) = ahead {
                    if pts > now + ahead {
                        let wait = self.until(pts - ahead, now);
                        self.transport_cv.wait_for(&mut t, wait);
                        continue;
                    }
                }
                return Preroll::Go;
            }
            if pts <= now + PREROLL {
                return Preroll::Go;
            }
            self.transport_cv.wait(&mut t);
        }
    }

    /// Waits until the clock reaches `at`, the end of the audio written: its
    /// last samples have been heard. False when stopped, retired or
    /// superseded by a seek. A clock that stands still while running has
    /// played everything it was given, which ends the wait too (its sample
    /// count falls short of the stamps where they have a gap).
    pub(super) fn wait_heard(
        &self, at: Duration, seen_seek: u64, retired: &AtomicBool,
        mut apply_running: impl FnMut(bool),
    ) -> bool {
        #[cfg(target_os = "macos")]
        let _timing = crate::clock::timing::Guard::enter();
        let mut t = self.transport.lock();
        let mut still: Option<(Duration, Instant)> = None;
        loop {
            if self.superseded(seen_seek, retired) {
                return false;
            }
            apply_running(t.running);
            let now = self.master.now().unwrap_or_default();
            if now + END_SLACK >= at {
                return true;
            }
            if !t.running {
                still = None;
                self.transport_cv.wait(&mut t);
                continue;
            }
            match still {
                Some((position, since)) if position == now => {
                    if since.elapsed() >= DRAINED {
                        return true;
                    }
                }
                _ => still = Some((now, Instant::now())),
            }
            let wait = self.until(at, now).min(DRAINED);
            self.transport_cv.wait_for(&mut t, wait);
        }
    }


    /// A sink made no room in its bounded write. Yield on the transport's
    /// condvar, so pause/seek/drop interrupts the retry without losing PCM.
    pub(super) fn wait_output(&self) {
        let mut t = self.transport.lock();
        self.transport_cv.wait_for(&mut t, Duration::from_millis(5));
    }

    /// Without realtime pacing nothing waits on the clock: a paused player
    /// parks its pipelines here instead.
    pub(super) fn wait_while_paused(&self, retired: &AtomicBool) {
        let mut t = self.transport.lock();
        while t.paused && !self.stopped.load(Ordering::SeqCst) && !retired.load(Ordering::SeqCst) {
            self.transport_cv.wait(&mut t);
        }
    }

    /// The caller's work is stale: the player stopped, a selection switch
    /// retired the caller's pipeline thread, or a newer seek arrived.
    fn superseded(&self, seen_seek: u64, retired: &AtomicBool) -> bool {
        self.stopped.load(Ordering::SeqCst)
            || retired.load(Ordering::SeqCst)
            || self.seek_gen.load(Ordering::SeqCst) != seen_seek
    }
}
