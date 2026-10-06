//! The playback clock: an `AVSampleBufferRenderSynchronizer`'s timebase.

use std::time::Duration;

use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMClock, CMClockOrTimebase, CMTime, CMTimeFlags, CMTimebase};

use crate::backend::Clock;

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
        let cm_time = CMTime {
            value: at.as_nanos().min(i64::MAX as u128) as i64,
            timescale: 1_000_000_000,
            flags: CM_TIME_VALID,
            epoch: 0,
        };
        // CMSyncConvertTime takes CMClockOrTimebase (= CFType) references;
        // both wrappers store exactly that representation.
        let from: &CMClockOrTimebase = unsafe { cast_clock_or_timebase(&self.timebase) };
        let to: &CMClockOrTimebase = unsafe { cast_clock_or_timebase(&self.host_clock) };
        let host_time = unsafe { objc2_core_media::CMSyncConvertTime(cm_time, from, to) };
        if host_time.flags.contains(CM_TIME_VALID) && host_time.timescale > 0 {
            let secs = host_time.value as f64 / host_time.timescale as f64;
            if secs.is_finite() {
                return Some((secs * 1e9) as i64);
            }
        }
        None
    }
}

/// `CMClockOrTimebase` is a plain `CFType` alias, so a `&CMTimebase` /
/// `&CMClock` reinterprets as it — same representation, both CF objects.
#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn cast_clock_or_timebase<T>(t: &T) -> &CMClockOrTimebase {
    unsafe { &*(t as *const T as *const CMClockOrTimebase) }
}
