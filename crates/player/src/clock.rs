use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::backend::Clock;

/// Monotonic timestamp in nanoseconds since an arbitrary epoch.
pub fn current_monotonic_ns() -> i64 {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: ts is a valid pointer to stack-allocated memory.
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        }
        (ts.tv_sec as i64) * 1_000_000_000 + (ts.tv_nsec as i64)
    }
    #[cfg(not(unix))]
    {
        static START: std::sync::LazyLock<std::time::Instant> =
            std::sync::LazyLock::new(std::time::Instant::now);
        std::time::Instant::now().duration_since(*START).as_nanos() as i64
    }
}

#[derive(Debug)]
struct ClockInner {
    base_media_time: Duration,
    start_mono_ns: Option<i64>,
    playing: bool,
    started: bool,
}

/// A free-running monotonic clock used when there is no audio output.
#[derive(Clone, Debug)]
pub struct FreeRunningClock {
    inner: Arc<Mutex<ClockInner>>,
}

impl Default for FreeRunningClock {
    fn default() -> Self {
        Self::new()
    }
}

impl FreeRunningClock {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ClockInner {
                base_media_time: Duration::ZERO,
                start_mono_ns: None,
                playing: false,
                started: false,
            })),
        }
    }

    pub fn play(&self) {
        let mut inner = self.inner.lock();
        if !inner.playing {
            inner.playing = true;
            inner.started = true;
            inner.start_mono_ns = Some(current_monotonic_ns());
        }
    }

    pub fn pause(&self) {
        let mut inner = self.inner.lock();
        if inner.playing {
            if let Some(start_ns) = inner.start_mono_ns {
                let now_ns = current_monotonic_ns();
                let elapsed_ns = (now_ns - start_ns).max(0) as u64;
                inner.base_media_time += Duration::from_nanos(elapsed_ns);
            }
            inner.playing = false;
            inner.start_mono_ns = None;
        }
    }

    pub fn seek(&self, to: Duration) {
        let mut inner = self.inner.lock();
        inner.base_media_time = to;
        if inner.playing {
            inner.start_mono_ns = Some(current_monotonic_ns());
        }
    }

    pub fn set_position(&self, to: Duration) {
        let mut inner = self.inner.lock();
        inner.base_media_time = to;
        inner.started = true;
        if inner.playing {
            inner.start_mono_ns = Some(current_monotonic_ns());
        }
    }
}

impl Clock for FreeRunningClock {
    fn now(&self) -> Option<Duration> {
        let inner = self.inner.lock();
        if !inner.started {
            return None;
        }
        if inner.playing {
            if let Some(start_ns) = inner.start_mono_ns {
                let now_ns = current_monotonic_ns();
                let elapsed_ns = (now_ns - start_ns).max(0) as u64;
                Some(inner.base_media_time + Duration::from_nanos(elapsed_ns))
            } else {
                Some(inner.base_media_time)
            }
        } else {
            Some(inner.base_media_time)
        }
    }

    fn monotonic_ns_at(&self, at: Duration) -> Option<i64> {
        let inner = self.inner.lock();
        let start_ns = inner.start_mono_ns?;
        let base_ns = inner.base_media_time.as_nanos() as i64;
        let at_ns = at.as_nanos() as i64;
        Some(start_ns + (at_ns - base_ns))
    }
}
