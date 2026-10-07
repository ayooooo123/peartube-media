//! The playback clock: an `AVSampleBufferRenderSynchronizer`'s timebase.

use std::time::Duration;

use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMClock, CMClockOrTimebase, CMTime, CMTimeFlags, CMTimebase};

use crate::backend::Clock;
use crate::clock::current_monotonic_ns;

/// The Valid flag, re-declared for clarity (objc2-core-media has it as a
/// bitflags variant).
const CM_TIME_VALID: CMTimeFlags = CMTimeFlags::Valid;

/// Wraps an Objective-C/CoreFoundation object so it can move between the
/// engine's threads. `CMTimebase` and `CMClock` are documented thread-safe
/// (CoreMedia sync API); the wrappers below keep the +1 retain count and
/// release on the last drop. Callers only ever touch them behind
/// `AppleClock`, whose methods are `&self`.
pub struct SendSync<T>(pub T);
unsafe impl<T> Send for SendSync<T> {}
unsafe impl<T> Sync for SendSync<T> {}
impl<T> std::ops::Deref for SendSync<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.0
    }
}

/// Media time straight from the synchronizer's timebase. `None` until the
/// audio sink has anchored the timebase at the first write.
pub struct AppleClock {
    timebase: SendSync<CFRetained<CMTimebase>>,
    host_clock: SendSync<CFRetained<CMClock>>,
}

// SAFETY: CMTimebase/CMClock are CoreFoundation objects; CoreMedia
// documents CMSync access as thread-safe. CFRetained keeps a +1 count and
// releases from whichever thread drops it last — CFRelease is thread-safe.
unsafe impl Send for AppleClock {}
unsafe impl Sync for AppleClock {}

impl AppleClock {
    pub fn from_timebase(timebase: CFRetained<CMTimebase>) -> Self {
        let host_clock = unsafe { CMClock::host_time_clock() };
        Self {
            timebase: SendSync(timebase),
            host_clock: SendSync(host_clock),
        }
    }
}

impl Clock for AppleClock {
    fn now(&self) -> Option<Duration> {
        let time = unsafe { self.timebase.time() };
        if time.flags.contains(CM_TIME_VALID) && time.timescale > 0 && time.value >= 0 {
            Some(Duration::from_secs_f64(
                time.value as f64 / time.timescale as f64,
            ))
        } else {
            None
        }
    }

    fn monotonic_ns_at(&self, at: Duration) -> Option<i64> {
        // A timebase that stands still says nothing about when `at` comes.
        if unsafe { self.timebase.rate() } == 0.0 {
            return None;
        }
        let cm_time = CMTime {
            value: at.as_nanos().min(i64::MAX as u128) as i64,
            timescale: 1_000_000_000,
            flags: CM_TIME_VALID,
            epoch: 0,
        };
        // CMSyncConvertTime takes CMClockOrTimebase (= CFType) references;
        // both wrappers store exactly that representation.
        let from: &CMClockOrTimebase = unsafe { cast_clock_or_timebase(&**self.timebase) };
        let to: &CMClockOrTimebase = unsafe { cast_clock_or_timebase(&**self.host_clock) };
        let host_at = unsafe { objc2_core_media::CMSyncConvertTime(cm_time, from, to) };
        let host_now = unsafe { self.host_clock.time() };
        // Host time (mach_absolute_time) stops while the machine sleeps,
        // CLOCK_MONOTONIC does not: carry the distance over instead of the
        // epoch.
        let seconds = |t: CMTime| {
            (t.flags.contains(CM_TIME_VALID) && t.timescale > 0)
                .then(|| t.value as f64 / f64::from(t.timescale))
                .filter(|s| s.is_finite())
        };
        let ahead = seconds(host_at)? - seconds(host_now)?;
        Some(current_monotonic_ns() + (ahead * 1e9) as i64)
    }
}

/// `CMClockOrTimebase` is a plain `CFType` alias, so a `&CMTimebase` /
/// `&CMClock` reinterprets as it — same representation, both CF objects.
#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn cast_clock_or_timebase<T>(t: &T) -> &CMClockOrTimebase {
    unsafe { &*(t as *const T as *const CMClockOrTimebase) }
}
