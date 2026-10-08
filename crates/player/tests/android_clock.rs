#[path = "../src/android/position.rs"]
mod position;
use position::observe;
#[cfg(test)]
mod tests {
    use super::observe;

    #[test]
    fn startup_needs_a_hardware_timestamp() {
        assert_eq!(observe(0.0, 48000, 48000, 1_000_000_000, 1_100_000_000, None, 0.0), (0.0, None));
    }

    #[test]
    fn resume_rejects_old_run_timestamps_without_extrapolating() {
        let old = Some((4800.0, 1_100_000_000));
        assert_eq!(observe(4800.0, 48000, 9600, 2_000_000_000, 2_100_000_000, old, 4800.0), (4800.0, None));
        let fresh = Some((4800.0, 2_100_000_000));
        assert_eq!(observe(4800.0, 48000, 9600, 2_000_000_000, 2_110_000_000, fresh, 6000.0), (5280.0, fresh));
    }

    #[test]
    fn late_device_correction_cannot_move_presented_time_backwards() {
        let first = observe(0.0, 48000, 9600, 1_000_000_000, 1_020_000_000, Some((0.0, 1_000_000_000)), 960.0).0;
        assert_eq!(first, 960.0);
        let corrected = Some((480.0, 1_020_000_000));
        assert_eq!(observe(first, 48000, 9600, 1_000_000_000, 1_025_000_000, corrected, 1920.0), (960.0, corrected));
        assert_eq!(observe(first, 48000, 9600, 1_000_000_000, 1_050_000_000, corrected, 1920.0).0, 1920.0);
    }

    #[test]
    fn starvation_and_fresh_stream_reset_bound_the_position() {
        let ts = Some((0.0, 1_000_000_000));
        assert_eq!(observe(0.0, 48000, 480, 1_000_000_000, 1_200_000_000, ts, 480.0).0, 480.0);
        assert_eq!(observe(480.0, 48000, 480, 1_000_000_000, 1_300_000_000, None, 480.0), (480.0, None));
        assert_eq!(observe(0.0, 48000, 480, 2_000_000_000, 2_100_000_000, ts, 0.0), (0.0, None));
    }

    #[test]
    fn drained_resume_uses_consumed_frames_without_inventing_a_time_anchor() {
        assert_eq!(observe(20701.232, 48000, 24000, 2_000_000_000, 3_000_000_000, None, 24000.0), (24000.0, None));
    }
}
