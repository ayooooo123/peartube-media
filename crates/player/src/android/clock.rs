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
    pub base_read_frame: i64,
    pub written: u64,
    pub playing: bool,
    pub held_frames: f64,
    pub started_ns: i64,
    pub resume_read_frames: f64,
    pub last_read_frames: f64,
    pub resume_written: u64,
    pub read_anchor: Option<(f64, i64)>,
    pub write_anchor: Option<(f64, i64)>,
    pub logged_fallback: bool,
}

impl AudioClockState {
    pub fn new() -> Self {
        Self {
            stream: None,
            rate: 48000,
            base: None,
            base_frame: 0,
            base_read_frame: 0,
            written: 0,
            playing: false,
            held_frames: 0.0,
            started_ns: 0,
            resume_read_frames: 0.0,
            last_read_frames: 0.0,
            resume_written: 0,
            read_anchor: None,
            write_anchor: None,
            logged_fallback: false,
        }
    }

    /// Never extrapolate from startup/resume time. Without a fresh timestamp,
    /// the endpoint counter still accounts for a tail consumed before pause.
    /// A high-water mark prevents backward steps; a stream reset clears it.
    pub fn observe(&mut self) -> (f64, Option<(f64, i64)>) {
        if !self.playing || self.base.is_none() {
            return (self.held_frames, None);
        }
        let now_ns = current_monotonic_ns();
        let ts_res = self
            .stream
            .as_ref()
            .map(|stream| stream.0.timestamp(Clockid::Monotonic));
        let timestamp = ts_res
            .as_ref()
            .and_then(|res| res.as_ref().ok())
            .map(|ts| ((ts.frame_position - self.base_frame) as f64, ts.time_nanoseconds));
        let read_frames = if timestamp.is_some_and(|(_, ns)| ns >= self.started_ns) {
            self.held_frames
        } else {
            self.stream.as_ref().map_or(self.held_frames, |stream| {
                (stream.0.frames_read() - self.base_read_frame) as f64
            })
        };
        let observed = super::position::observe(
            self.held_frames,
            self.rate,
            self.written,
            self.started_ns,
            now_ns,
            timestamp,
            read_frames,
        );
        self.held_frames = observed.0;
        if observed.1.is_some() {
            return observed;
        }

        // Direct HDMI IEC61937 streams (AudioStreamLegacy on Android TV) often omit
        // `AAudioStream_getTimestamp`. Once playback genuinely advances past the
        // start/resume watermark, anchor to `frames_read()` with a gentle PLL (so
        // discrete 32–64 ms ALSA DMA bursts do not jerk video frame targets across
        // 23.976 Hz VSYNC boundaries), or fall back to blocking `write()` pacing.
        if read_frames > self.resume_read_frames {
            let raw_read = read_frames.min(self.written as f64);
            if raw_read > self.last_read_frames {
                self.last_read_frames = raw_read;
                match self.read_anchor {
                    None => {
                        self.read_anchor = Some((raw_read.max(self.held_frames), now_ns));
                    }
                    Some((anc_frame, anc_ns)) => {
                        let predicted = anc_frame
                            + (now_ns - anc_ns) as f64 * f64::from(self.rate) / 1e9;
                        let err = raw_read - predicted;
                        let max_snap = f64::from(self.rate) * 0.15;
                        let deadband = f64::from(self.rate) * 0.025;
                        if err.abs() > max_snap {
                            self.read_anchor = Some((raw_read, now_ns));
                        } else if err.abs() > deadband {
                            let smoothed = predicted + err * 0.03;
                            self.read_anchor = Some((smoothed, now_ns));
                        }
                    }
                }
            }
        }
        if let Some(anchor) = self.read_anchor {
            if !self.logged_fallback {
                self.logged_fallback = true;
                super::audio::log_info(
                    c"peartube-audio",
                    &format!(
                        "clock fallback=frames_read frame={:.0} written={} ts={:?}",
                        anchor.0,
                        self.written,
                        ts_res.as_ref().map(|r| r.as_ref().err())
                    ),
                );
            }
            let frames = (anchor.0 + (now_ns - anchor.1) as f64 * f64::from(self.rate) / 1e9)
                .clamp(self.held_frames, self.written as f64);
            self.held_frames = frames;
            return (frames, Some(anchor));
        }
        if let Some(anchor) = self.write_anchor {
            if !self.logged_fallback {
                self.logged_fallback = true;
                super::audio::log_info(
                    c"peartube-audio",
                    &format!(
                        "clock fallback=write_pace frame={:.0} written={} read={:.0} ts={:?}",
                        anchor.0,
                        self.written,
                        read_frames,
                        ts_res.as_ref().map(|r| r.as_ref().err())
                    ),
                );
            }
            let frames = (anchor.0 + (now_ns - anchor.1) as f64 * f64::from(self.rate) / 1e9)
                .clamp(self.held_frames, self.written as f64);
            self.held_frames = frames;
            return (frames, Some(anchor));
        }
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
