use crate::backend::Clock;
use crate::clock::current_monotonic_ns;
use ndk::audio::Clockid;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

pub struct SendAudioStream(pub ndk::audio::AudioStream);
// AAudio permits timestamp queries alongside writes and state requests.
unsafe impl Send for SendAudioStream {}
unsafe impl Sync for SendAudioStream {}

pub(super) struct AudioClockState {
    pub stream: Option<Arc<SendAudioStream>>,
    pub rate: u32,
    pub base: Option<Duration>,
    pub base_frame: i64,
    pub written: u64,
    pub playing: bool,
    pub held_frames: f64,
    pub started_ns: i64,
}

impl AudioClockState {
    pub fn new() -> Self {
        Self { stream: None, rate: 48000, base: None, base_frame: 0, written: 0,
            playing: false, held_frames: 0.0, started_ns: 0 }
    }

    /// Never extrapolate from startup/resume time. Without a fresh timestamp,
    /// the endpoint counter still accounts for a tail consumed before pause.
    /// A high-water mark prevents backward steps; a stream reset clears it.
    pub fn observe(&mut self) -> (f64, Option<(f64, i64)>) {
        if !self.playing || self.base.is_none() {
            return (self.held_frames, None);
        }
        let timestamp = self.stream.as_ref()
            .and_then(|stream| stream.0.timestamp(Clockid::Monotonic).ok())
            .map(|ts| ((ts.frame_position - self.base_frame) as f64, ts.time_nanoseconds));
        let read_frames = if timestamp.is_some_and(|(_, ns)| ns >= self.started_ns) {
            self.held_frames
        } else {
            self.stream.as_ref().map_or(self.held_frames,
                |stream| (stream.0.frames_read() - self.base_frame) as f64)
        };
        let observed = super::position::observe(self.held_frames, self.rate, self.written,
            self.started_ns, current_monotonic_ns(), timestamp, read_frames);
        self.held_frames = observed.0;
        observed
    }
}

#[derive(Clone)]
pub struct AudioClock {
    pub(super) inner: Arc<Mutex<AudioClockState>>,
}

impl Clock for AudioClock {
    fn now(&self) -> Option<Duration> {
        let mut state = self.inner.lock();
        Some(state.base? + Duration::from_secs_f64(state.observe().0 / f64::from(state.rate)))
    }

    fn monotonic_ns_at(&self, at: Duration) -> Option<i64> {
        let mut state = self.inner.lock();
        let base = state.base?;
        if !state.playing { return None; }
        let desired = (at.as_secs_f64() - base.as_secs_f64()) * f64::from(state.rate);
        if desired > state.written as f64 { return None; }
        let (frame, ns) = state.observe().1?;
        Some(ns + ((desired - frame) / f64::from(state.rate) * 1e9).round() as i64)
    }
}

/// Opt-in debug-only trace. Release builds contain neither environment
/// lookup nor formatting/output. Stamps are AAudio's, not enqueue times.
#[cfg(debug_assertions)]
pub(super) fn tracing() -> bool {
    static ENABLED: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("PEARTUBE_SYNC_TRACE").is_some());
    *ENABLED
}
