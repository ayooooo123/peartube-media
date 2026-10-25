//! Where the native route's segments start: at keyframes from the
//! container's own index (Matroska Cues, an MP4 sample table), about
//! `TARGET_SECS` apart. AVPlayer fetches a segment by its place in the
//! playlist, so the plan has to be right before the first segment exists:
//! an index that cannot be trusted means the playback is not eligible.

use oxideav_core::TimeBase;

/// Wanted segment length. A segment runs from one planned keyframe to the
/// next, so it is at least this long (but the last) and as long as the
/// keyframes make it.
pub(crate) const TARGET_SECS: f64 = 4.0;

/// An index that stops this far before the end is a partial one (Cues cut
/// short, an unfinished download).
const MAX_TRAILING_GAP_SECS: f64 = 60.0;

/// Segment starts on the source axis, in the video stream's time base:
/// keyframe presentation times, ascending. Segment `i` runs to
/// `starts[i + 1]`, the last one to `end`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Plan {
    pub starts: Vec<i64>,
    /// End of the last video picture.
    pub end: i64,
}

impl Plan {
    pub(crate) fn len(&self) -> usize {
        self.starts.len()
    }

    /// Start and end of segment `index`.
    pub(crate) fn bounds(&self, index: usize) -> (i64, i64) {
        let end = self.starts.get(index + 1).copied().unwrap_or(self.end);
        (self.starts[index], end)
    }

    /// The segment that holds `ticks` (the first one before the first start).
    pub(crate) fn segment_at(&self, ticks: i64) -> usize {
        self.starts.partition_point(|&start| start <= ticks).saturating_sub(1)
    }
}

/// Whether the keyframes an index lists can be trusted for a plan: they
/// reach to within a minute of the end and never gap more than
/// `max(4 * TARGET_SECS, 30 s)` (AetherEngine's tail and gap witnesses; its
/// coverage witness guards a planner that must cut, and the tail witness
/// already rejects an index bunched at the start of anything longer than a
/// minute). Checked before anything reads near the last keyframe: a partial
/// index would send that read through the whole title.
pub(crate) fn trusted(keyframes: &[i64], tb: TimeBase, duration_secs: f64) -> bool {
    let mut keys = keyframes.to_vec();
    keys.sort_unstable();
    keys.dedup();
    let Some(&last) = keys.last() else {
        return false;
    };
    let secs = |ticks: i64| tb.seconds_of(ticks);
    if !duration_secs.is_finite() || duration_secs - secs(last) > MAX_TRAILING_GAP_SECS {
        return false;
    }
    let max_gap = (4.0 * TARGET_SECS).max(30.0);
    !keys.windows(2).any(|pair| secs(pair[1]) - secs(pair[0]) > max_gap)
}

/// Ceiling on a single GOP or static end-credits stretch in an otherwise
/// complete container index (`--keyint 720` at 23.976 fps is 30.030 s; UHD
/// studio logos and end credits routinely hold 60–180 s without a cut).
const MAX_USABLE_GAP_SECS: f64 = 120.0;
const MAX_USABLE_TAIL_SECS: f64 = 300.0;
const MAX_PLAN_SEGMENTS: usize = 200_000;

/// Whether a container's own Cues/stss table spans the title cleanly enough
/// for an exact keyframe plan even when long UHD GOPs (e.g. 30.03 s) or static
/// end credits trip `trusted`'s strict 30 s / 60 s thresholds.
pub(crate) fn usable_cues(keyframes: &[i64], tb: TimeBase, duration_secs: f64) -> bool {
    if trusted(keyframes, tb, duration_secs) {
        return true;
    }
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return false;
    }
    let mut keys = keyframes.to_vec();
    keys.sort_unstable();
    keys.dedup();
    let (Some(&first), Some(&last)) = (keys.first(), keys.last()) else {
        return false;
    };
    let secs = |ticks: i64| tb.seconds_of(ticks);
    let coverage = secs(last) - secs(first);
    if keys.len() < 2 || coverage < TARGET_SECS {
        return false;
    }
    if duration_secs - secs(last) > MAX_USABLE_TAIL_SECS {
        return false;
    }
    !keys.windows(2).any(|pair| secs(pair[1]) - secs(pair[0]) > MAX_USABLE_GAP_SECS)
}

/// Uniform-stride fallback plan (AetherEngine's `buildUniformSegmentPlan`):
/// anchored at `first_key` when Cues are missing, cut short, or absent
/// (`mpegts`), so AVPlayer still rides the native route while the producer
/// snaps every segment cut to the first real keyframe at or past its boundary.
pub(crate) fn uniform(first_key: i64, end: i64, stride_secs: f64, tb: TimeBase) -> Option<Plan> {
    if end <= first_key {
        return None;
    }
    let span_secs = tb.seconds_of(end - first_key);
    if !span_secs.is_finite() || span_secs <= 0.0 {
        return None;
    }
    let mut stride = if stride_secs.is_finite() && stride_secs > 0.0 {
        stride_secs.max(TARGET_SECS)
    } else {
        TARGET_SECS
    };
    if span_secs / stride > MAX_PLAN_SEGMENTS as f64 {
        stride = span_secs / MAX_PLAN_SEGMENTS as f64;
    }
    let step = tb.ticks_of(stride).max(1);
    let mut starts = Vec::new();
    let mut at = first_key;
    while at < end && starts.len() < MAX_PLAN_SEGMENTS {
        starts.push(at);
        at = at.saturating_add(step);
    }
    if starts.len() > 1 && tb.seconds_of(end - *starts.last().expect("one start")) < TARGET_SECS {
        starts.pop();
    }
    Some(Plan { starts, end })
}

/// The plan for trusted `keyframes` whose last video picture ends at
/// `end`: a segment starts at the first keyframe at least `TARGET_SECS`
/// after the previous start, and a last segment shorter than that joins
/// the one before it. `None` when no keyframe lies before `end`.
pub(crate) fn plan(keyframes: &[i64], tb: TimeBase, end: i64) -> Option<Plan> {
    let mut keys = keyframes.to_vec();
    keys.sort_unstable();
    keys.dedup();
    keys.retain(|&k| k < end);
    let secs = |ticks: i64| tb.seconds_of(ticks);
    let mut starts = vec![*keys.first()?];
    for &key in &keys[1..] {
        if secs(key) - secs(*starts.last().expect("one start")) >= TARGET_SECS {
            starts.push(key);
        }
    }
    if starts.len() > 1 && secs(end) - secs(*starts.last().expect("one start")) < TARGET_SECS {
        starts.pop();
    }
    Some(Plan { starts, end })
}

/// Item-axis positions for the media playlist, in milliseconds: segment
/// `i` starts at `positions[i]`, the last ends at `positions[len]`.
/// `origin` is the source time the item axis starts at (its time 0).
/// The first segment starts at 0 (its tfdt), the others at their
/// keyframe's presentation time. Rounded per position, so durations sum
/// to the end exactly.
pub(crate) fn positions_ms(plan: &Plan, tb: TimeBase, origin: i64) -> Vec<u64> {
    let ms = |ticks: i64| (tb.seconds_of(ticks - origin) * 1000.0).round().max(0.0) as u64;
    let mut positions: Vec<u64> = std::iter::once(0)
        .chain(plan.starts[1..].iter().map(|&start| ms(start)))
        .chain(std::iter::once(ms(plan.end)))
        .collect();
    // Keep them strictly increasing whatever the rounding did.
    for i in 1..positions.len() {
        positions[i] = positions[i].max(positions[i - 1] + 1);
    }
    positions
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: TimeBase = TimeBase::new(1, 1000);

    fn keys(seconds: &[f64]) -> Vec<i64> {
        seconds.iter().map(|s| (s * 1000.0).round() as i64).collect()
    }

    #[test]
    fn cuts_at_the_first_keyframe_a_target_past_the_last_start() {
        // 2.002 s GOPs: starts every other keyframe.
        let k: Vec<f64> = (0..15).map(|i| i as f64 * 2.002).collect();
        assert!(trusted(&keys(&k), MS, 30.03));
        let p = plan(&keys(&k), MS, 30_030).unwrap();
        assert_eq!(p.starts, keys(&[0.0, 4.004, 8.008, 12.012, 16.016, 20.02, 24.024]));
        assert_eq!(p.bounds(6), (24_024, 30_030));
        assert_eq!(p.segment_at(23_999), 5);
        assert_eq!(p.segment_at(24_024), 6);
        assert_eq!(p.segment_at(-5), 0);
    }

    #[test]
    fn a_short_last_segment_joins_the_one_before() {
        let p = plan(&keys(&[0.0, 4.0, 8.0, 12.0]), MS, 13_000).unwrap();
        assert_eq!(p.starts, keys(&[0.0, 4.0, 8.0]));
        assert_eq!(p.bounds(2), (8_000, 13_000));
    }

    #[test]
    fn one_keyframe_makes_one_segment() {
        // x264's default 250-frame GOP: one keyframe in a 10 s clip.
        assert!(trusted(&keys(&[0.0]), MS, 10.0));
        let p = plan(&keys(&[0.0]), MS, 10_000).unwrap();
        assert_eq!(p.starts, vec![0]);
        assert_eq!(p.bounds(0), (0, 10_000));
    }

    #[test]
    fn rejects_an_index_that_stops_short_or_gaps() {
        // Cues for the first minute of a two-hour title.
        let k: Vec<f64> = (0..30).map(|i| i as f64 * 2.0).collect();
        assert!(!trusted(&keys(&k), MS, 7200.0));
        // Clustered: everything near the start and one near the end.
        assert!(!trusted(&keys(&[0.0, 1.0, 2.0, 3.0, 5.0, 590.0]), MS, 600.0));
        // All within the first second of a title over a minute long.
        assert!(!trusted(&keys(&[0.0, 0.5]), MS, 90.0));
        assert!(!trusted(&[], MS, 30.0));
        assert!(!trusted(&keys(&[0.0, 4.0]), MS, f64::NAN));
    }

    #[test]
    fn positions_start_at_zero_and_telescope_to_the_end() {
        let p = Plan { starts: keys(&[0.0, 4.004, 8.008]), end: 12_345 };
        // Origin 84 ms before the first keyframe (B-frame reorder delay).
        let pos = positions_ms(&p, MS, -84);
        assert_eq!(pos, vec![0, 4_088, 8_092, 12_429]);
    }

    #[test]
    fn usable_cues_accepts_720_frame_23976_gops_and_credits_tail_but_rejects_partial_scans() {
        // 720-frame GOPs at 23.976 fps = 30.030 s (> 30.0 s strict cap), plus a 150 s credits card.
        let mut k: Vec<f64> = (0..100).map(|i| i as f64 * 30.030).collect();
        let dur = *k.last().unwrap() + 150.0;
        assert!(!trusted(&keys(&k), MS, dur));
        assert!(usable_cues(&keys(&k), MS, dur));
        // Partial scan stopping 10 minutes short still fails so uniform fallback takes over.
        k.truncate(20);
        assert!(!usable_cues(&keys(&k), MS, dur));
    }

    #[test]
    fn uniform_plan_anchors_at_first_key_and_merges_short_tail() {
        let p = uniform(11_600, 33_000, 10.0, MS).unwrap();
        // 11.6 s, 21.6 s; 31.6..33.0 s is < 4 s so it merges into segment 1.
        assert_eq!(p.starts, vec![11_600, 21_600]);
        assert_eq!(p.bounds(1), (21_600, 33_000));
    }
}
