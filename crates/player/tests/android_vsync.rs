#[path = "../src/android/vsync.rs"]
mod vsync;

use vsync::{DisplayVsync, VsyncPacer};

const PERIOD_23976: i64 = 41_708_375;
const PERIOD_60: i64 = 16_666_667;

#[test]
fn passes_through_unchanged_without_vsync() {
    let mut pacer = VsyncPacer::new();
    assert_eq!(pacer.adjust(1_234_567_890, None), 1_234_567_890);
    assert_eq!(
        pacer.adjust(1_234_567_890, Some(DisplayVsync { hw_vsync_ns: 0, period_ns: 0 })),
        1_234_567_890
    );
}

#[test]
fn centers_23976_mkv_frames_started_near_vsync_edge() {
    let mut pacer = VsyncPacer::new();
    let base_vsync = 10_000_000_000i64;
    let vsync = DisplayVsync {
        hw_vsync_ns: base_vsync,
        period_ns: PERIOD_23976,
    };

    // Start in the exact 2.7 ms danger zone with MKV integer-ms quantized PTS.
    let start_raw = base_vsync - 2_700_000;
    let mut prev = None;
    for i in 0..240i64 {
        let mkv_ms = (i as f64 * (1001.0 / 24.0)).round() as i64;
        let raw = start_raw + mkv_ms * 1_000_000;
        let adjusted = pacer.adjust(raw, Some(vsync));
        let expected = base_vsync + i * PERIOD_23976 - (PERIOD_23976 / 8);
        assert_eq!(adjusted, expected, "frame {i} should stay centered on VSYNC {i}");
        if let Some(p) = prev {
            assert_eq!(adjusted - p, PERIOD_23976, "frame {i} spacing must be exact refresh period");
        }
        prev = Some(adjusted);
    }
}

#[test]
fn hysteresis_prevents_oscillation_right_on_half_period_boundary() {
    let mut pacer = VsyncPacer::new();
    let base_vsync = 20_000_000_000i64;
    let vsync = DisplayVsync {
        hw_vsync_ns: base_vsync,
        period_ns: PERIOD_23976,
    };

    // Start dead-on the worst-case +0.5 period boundary with MKV ±0.5 ms rounding jitter.
    let start_raw = base_vsync + (PERIOD_23976 / 2);
    let mut prev = None;
    for i in 0..120i64 {
        let mkv_ms = (i as f64 * (1001.0 / 24.0)).round() as i64;
        let raw = start_raw + mkv_ms * 1_000_000;
        let adjusted = pacer.adjust(raw, Some(vsync));
        if let Some(p) = prev {
            assert_eq!(
                adjusted - p,
                PERIOD_23976,
                "frame {i} must not drop or double-step on the 0.5-period boundary"
            );
        }
        prev = Some(adjusted);
    }
}

#[test]
fn handles_3_2_pulldown_for_24p_on_60hz_without_locking_wrong_step() {
    let mut pacer = VsyncPacer::new();
    let base_vsync = 30_000_000_000i64;
    let vsync = DisplayVsync {
        hw_vsync_ns: base_vsync,
        period_ns: PERIOD_60,
    };

    let mut steps = Vec::new();
    let mut prev = None;
    for i in 0..20i64 {
        let raw = base_vsync + i * PERIOD_23976;
        let adjusted = pacer.adjust(raw, Some(vsync));
        if let Some(p) = prev {
            let vsync_steps = ((adjusted - p) as f64 / PERIOD_60 as f64).round() as i64;
            assert!(
                vsync_steps == 2 || vsync_steps == 3,
                "24p@60Hz step {i} must be 2 or 3 VSYNCs, got {vsync_steps}"
            );
            steps.push(vsync_steps);
        }
        prev = Some(adjusted);
    }
    assert!(steps.contains(&2) && steps.contains(&3));
}

#[test]
fn resets_cleanly_on_refresh_rate_switch() {
    let mut pacer = VsyncPacer::new();
    let base = 40_000_000_000i64;
    let v60 = DisplayVsync { hw_vsync_ns: base, period_ns: PERIOD_60 };
    let v24 = DisplayVsync { hw_vsync_ns: base + 100_000_000, period_ns: PERIOD_23976 };

    let _ = pacer.adjust(base, Some(v60));
    let _ = pacer.adjust(base + PERIOD_23976, Some(v60));

    // Switch display to 23.976 Hz: subsequent frames lock cleanly to PERIOD_23976.
    let a = pacer.adjust(base + 100_000_000 - 2_000_000, Some(v24));
    let b = pacer.adjust(base + 100_000_000 + 42_000_000 - 2_000_000, Some(v24));
    assert_eq!(b - a, PERIOD_23976);
}
