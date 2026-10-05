use crate::backend::Clock;
use ndk::audio::Clockid;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub fn monotonic_now_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

pub struct SendAudioStream(pub ndk::audio::AudioStream);
unsafe impl Send for SendAudioStream {}
unsafe impl Sync for SendAudioStream {}

pub struct AudioClockInner {
    pub stream: Mutex<Option<Arc<SendAudioStream>>>,
    pub sample_rate: AtomicU32,
    pub channels: AtomicU32,
    pub base_pts: Mutex<Option<Duration>>,
    pub base_frame_offset: AtomicI64,
    pub start_mono_ns: AtomicI64,
    pub frames_written: AtomicU64,
    pub is_playing: AtomicBool,
    pub pause_mono_ns: AtomicI64,
}

impl AudioClockInner {
    pub fn new() -> Self {
        Self {
            stream: Mutex::new(None),
            sample_rate: AtomicU32::new(48000),
            channels: AtomicU32::new(2),
            base_pts: Mutex::new(None),
            base_frame_offset: AtomicI64::new(0),
            start_mono_ns: AtomicI64::new(0),
            frames_written: AtomicU64::new(0),
            is_playing: AtomicBool::new(false),
            pause_mono_ns: AtomicI64::new(0),
        }
    }
}

#[derive(Clone)]
pub struct AudioClock {
    pub inner: Arc<AudioClockInner>,
}

impl Clock for AudioClock {
    fn now(&self) -> Option<Duration> {
        let base_pts = (*self.inner.base_pts.lock())?;
        let sample_rate = self.inner.sample_rate.load(Ordering::SeqCst) as f64;
        if sample_rate <= 0.0 {
            return Some(base_pts);
        }

        let stream_opt = self.inner.stream.lock().clone();
        if let Some(stream) = stream_opt {
            if let Ok(ts) = stream.0.timestamp(Clockid::Monotonic) {
                let base_offset = self.inner.base_frame_offset.load(Ordering::SeqCst);
                let rel_frames = (ts.frame_position - base_offset).max(0) as f64;
                let hw_media_sec = rel_frames / sample_rate;
                let hw_pts = base_pts + Duration::from_secs_f64(hw_media_sec);

                let now_mono = if self.inner.is_playing.load(Ordering::SeqCst) {
                    monotonic_now_ns()
                } else {
                    let p = self.inner.pause_mono_ns.load(Ordering::SeqCst);
                    if p > 0 {
                        p
                    } else {
                        monotonic_now_ns()
                    }
                };

                let elapsed_ns = (now_mono - ts.time_nanoseconds).max(0);
                return Some(hw_pts + Duration::from_nanos(elapsed_ns as u64));
            }
        }

        // Fallback: from frames written + start time before that. The stream
        // consumes written frames at the configured rate, so the clock cannot
        // run ahead of what the hardware has taken from the queue.
        let start_mono = self.inner.start_mono_ns.load(Ordering::SeqCst);
        let now_mono = if self.inner.is_playing.load(Ordering::SeqCst) {
            monotonic_now_ns()
        } else {
            let p = self.inner.pause_mono_ns.load(Ordering::SeqCst);
            if p > 0 {
                p
            } else {
                monotonic_now_ns()
            }
        };

        let elapsed_ns = (now_mono - start_mono).max(0);
        let written = self.inner.frames_written.load(Ordering::SeqCst) as f64;
        let max_media_dur = Duration::from_secs_f64(written / sample_rate);
        let elapsed = Duration::from_nanos(elapsed_ns as u64).min(max_media_dur);

        Some(base_pts + elapsed)
    }

    fn monotonic_ns_at(&self, at: Duration) -> Option<i64> {
        let base_pts = (*self.inner.base_pts.lock())?;
        let sample_rate = self.inner.sample_rate.load(Ordering::SeqCst) as f64;
        if sample_rate <= 0.0 {
            return None;
        }

        let stream_opt = self.inner.stream.lock().clone();
        if let Some(stream) = stream_opt {
            if let Ok(ts) = stream.0.timestamp(Clockid::Monotonic) {
                let base_offset = self.inner.base_frame_offset.load(Ordering::SeqCst);
                let rel_frames = (ts.frame_position - base_offset).max(0) as f64;
                let hw_media_sec = rel_frames / sample_rate;
                let hw_pts = base_pts + Duration::from_secs_f64(hw_media_sec);

                let delta = if at >= hw_pts {
                    (at - hw_pts).as_nanos() as i64
                } else {
                    -((hw_pts - at).as_nanos() as i64)
                };
                return Some(ts.time_nanoseconds + delta);
            }
        }

        // Fallback: from start time and base_pts
        let start_mono = self.inner.start_mono_ns.load(Ordering::SeqCst);
        let delta = if at >= base_pts {
            (at - base_pts).as_nanos() as i64
        } else {
            -((base_pts - at).as_nanos() as i64)
        };
        Some(start_mono + delta)
    }
}
