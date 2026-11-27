//! Hardware VSYNC phase-centering for Android MediaCodec presentation.
//!
//! Android's SurfaceFlinger latches a buffer for display VSYNC `V` when its
//! `desiredPresentTime <= V` (with a small jitter margin near the boundary).
//! Unaligned targets that happen to start within ~3 ms of a VSYNC edge can be
//! nudged across the latch threshold by container millisecond quantization
//! (e.g. MKV `41 ms`/`42 ms` timestamps on a `41.708 ms` 23.976 Hz display),
//! causing periodic dropped frames (`no present fence`).
//!
//! `VsyncPacer` snaps release targets onto the display's hardware VSYNC grid
//! (with ±0.75-period hysteresis on integer-cadence streams so boundary jitter
//! never oscillates) and shifts them to the dead-center of the VSYNC window
//! (`V - period / 2`), giving ±50% of a full refresh period of margin.

/// Snapshot of the active display's hardware VSYNC clock (`CLOCK_MONOTONIC`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayVsync {
    /// A recent hardware VSYNC timestamp (`Choreographer.frameTimeNanos - appVsyncOffsetNanos`).
    pub hw_vsync_ns: i64,
    /// Display refresh period in nanoseconds (e.g. `41_708_375` for 23.976 Hz).
    pub period_ns: i64,
}

/// Stateful per-session VSYNC phase pacer.
#[derive(Debug, Default)]
pub struct VsyncPacer {
    last_period_ns: i64,
    last_raw_target_ns: Option<i64>,
    last_snapped_vsync_ns: Option<i64>,
}

impl VsyncPacer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears cadence tracking (call on seek, flush, pause, or decoder recreate).
    pub fn reset(&mut self) {
        self.last_raw_target_ns = None;
        self.last_snapped_vsync_ns = None;
    }

    /// Adjusts `raw_target_ns` (`CLOCK_MONOTONIC`) to the safe center of its target
    /// hardware VSYNC latch window (`snapped_vsync - period / 2`).
    ///
    /// Returns `raw_target_ns` unchanged when no valid display VSYNC is known.
    pub fn adjust(&mut self, raw_target_ns: i64, vsync: Option<DisplayVsync>) -> i64 {
        let Some(v) = vsync else {
            self.reset();
            return raw_target_ns;
        };
        // Sanity-guard refresh period: 10 Hz (100 ms) .. 500 Hz (2 ms).
        if v.period_ns < 2_000_000 || v.period_ns > 100_000_000 {
            self.reset();
            return raw_target_ns;
        }
        if (v.period_ns - self.last_period_ns).abs() > 500_000 {
            self.reset();
            self.last_period_ns = v.period_ns;
        }

        let snapped = match (self.last_raw_target_ns, self.last_snapped_vsync_ns) {
            (Some(prev_raw), Some(prev_snapped)) => {
                let raw_dt = raw_target_ns - prev_raw;
                if raw_dt <= 0 || raw_dt > 500_000_000 {
                    nearest_vsync(raw_target_ns, v.hw_vsync_ns, v.period_ns)
                } else {
                    let ratio = raw_dt as f64 / v.period_ns as f64;
                    let step = ratio.round() as i64;
                    if step >= 1 && (ratio - step as f64).abs() <= 0.08 {
                        // Integer cadence (e.g. 23.976p@23.976Hz, 24p@48/72/120Hz, 30p@60Hz, 60p@60Hz):
                        // advance on the VSYNC grid with ±0.75-period hysteresis so MKV 1 ms
                        // quantization never flips between adjacent VSYNCs even right on the 0.5 edge.
                        let candidate =
                            nearest_vsync(prev_snapped + step * v.period_ns, v.hw_vsync_ns, v.period_ns);
                        let hysteresis = (v.period_ns * 3) / 4;
                        if (raw_target_ns - candidate).abs() <= hysteresis {
                            candidate
                        } else {
                            nearest_vsync(raw_target_ns, v.hw_vsync_ns, v.period_ns)
                        }
                    } else {
                        // Non-integer cadence (e.g. 24p@60Hz 3:2 pulldown): snap each frame independently.
                        nearest_vsync(raw_target_ns, v.hw_vsync_ns, v.period_ns)
                    }
                }
            }
            _ => nearest_vsync(raw_target_ns, v.hw_vsync_ns, v.period_ns),
        };

        self.last_raw_target_ns = Some(raw_target_ns);
        self.last_snapped_vsync_ns = Some(snapped);
        // Place desiredPresentTime cleanly inside SurfaceFlinger's latch window for
        // `snapped`: strictly before `snapped` (~5.2 ms at 23.976 Hz, ~2.1 ms at 60 Hz)
        // while staying ~37.5% of a period (~15.6 ms at 23.976 Hz) away from the
        // `snapped - 0.5 * period` early-latch boundary where SurfaceFlinger overwrites
        // the preceding frame.
        snapped - (v.period_ns / 8)
    }
}

#[inline]
fn nearest_vsync(target_ns: i64, hw_vsync_ns: i64, period_ns: i64) -> i64 {
    let k = ((target_ns - hw_vsync_ns) as f64 / period_ns as f64).round() as i64;
    hw_vsync_ns + k * period_ns
}
