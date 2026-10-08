/// Presented frame count and its device-time anchor. Without a fresh timestamp,
/// only the endpoint's consumed-frame counter can advance position, not time.
pub(super) fn observe(
    held: f64,
    rate: u32,
    written: u64,
    started_ns: i64,
    now_ns: i64,
    timestamp: Option<(f64, i64)>,
    read_frames: f64,
) -> (f64, Option<(f64, i64)>) {
    let Some(anchor) = timestamp.filter(|&(_, ns)| ns >= started_ns) else {
        return (read_frames.clamp(held, written as f64), None);
    };
    let frames = (anchor.0 + (now_ns - anchor.1) as f64 * f64::from(rate) / 1e9)
        .clamp(held, written as f64);
    (frames, Some(anchor))
}

