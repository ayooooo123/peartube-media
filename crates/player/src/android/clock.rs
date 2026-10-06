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

    /// Frame position and the CLOCK_MONOTONIC time at which it was heard.
    /// Ignore the previous run's timestamp after pause/resume until AAudio
    /// supplies a fresh one; stale extrapolation would include the pause.
    fn anchor(&self) -> (f64, i64) {
        if let Some(stream) = &self.stream {
            if let Ok(ts) = stream.0.timestamp(Clockid::Monotonic) {
                if ts.time_nanoseconds >= self.started_ns {
                    return ((ts.frame_position - self.base_frame) as f64, ts.time_nanoseconds);
                }
            }
        }
        (self.held_frames, self.started_ns)
    }

    pub fn presented_frames(&self) -> f64 {
        if !self.playing || self.base.is_none() {
            return self.held_frames;
        }
        let (frame, at) = self.anchor();
        (frame + (current_monotonic_ns() - at) as f64 * f64::from(self.rate) / 1e9)
            .clamp(0.0, self.written as f64)
    }
}

#[derive(Clone)]
pub struct AudioClock {
    pub(super) inner: Arc<Mutex<AudioClockState>>,
}

impl Clock for AudioClock {
    fn now(&self) -> Option<Duration> {
        let state = self.inner.lock();
        Some(state.base? + Duration::from_secs_f64(state.presented_frames() / f64::from(state.rate)))
    }

    fn monotonic_ns_at(&self, at: Duration) -> Option<i64> {
        let state = self.inner.lock();
        let base = state.base?;
        if !state.playing { return None; }
        let desired = (at.as_secs_f64() - base.as_secs_f64()) * f64::from(state.rate);
        if desired > state.written as f64 { return None; }
        let (frame, ns) = state.anchor();
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
