//! Subtitle pipeline: packets → cue images → `SubtitleSink::show` at cue
//! start/end on the clock. Text/ASS cues render through the oxideav-subtitle
//! compositor; bitmap cues (PGS/VobSub) convert their plane to RGBA.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use oxideav_core::{Decoder, Frame};

use crate::backend::{Clock, SubtitleImage, SubtitleSink};
use crate::engine::Lane;

/// Renders one text/ASS cue onto a video-sized RGBA canvas.
pub fn render_text_cue(
    cue: &oxideav_core::SubtitleCue,
    video_width: u32,
    video_height: u32,
) -> SubtitleImage {
    let w = video_width.max(320);
    let h = video_height.max(240);
    let comp = oxideav_subtitle::compositor::Compositor::new(w, h);
    let rgba = comp.render(cue);
    SubtitleImage {
        x: 0,
        y: 0,
        width: w,
        height: h,
        rgba,
    }
}

/// Converts one bitmap cue (an RGBA/Indexed `VideoFrame`) to a positioned
/// overlay image.
pub fn extract_bitmap_cue(
    frame: &oxideav_core::VideoFrame,
    video_width: u32,
    video_height: u32,
) -> SubtitleImage {
    let w = video_width.max(320);
    let h = video_height.max(240);
    let planes = frame.image_planes();
    let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
    if let Some(plane) = planes.first() {
        let row_bytes = ((w as usize) * 4).min(plane.stride);
        let rows = (h as usize).min(plane.data.len() / plane.stride.max(1));
        for r in 0..rows {
            let src_start = r * plane.stride;
            let src_end = src_start + row_bytes;
            let dst_start = r * (w as usize) * 4;
            let dst_end = dst_start + row_bytes;
            if src_end <= plane.data.len() && dst_end <= rgba.len() {
                rgba[dst_start..dst_end].copy_from_slice(&plane.data[src_start..src_end]);
            }
        }
    }
    SubtitleImage {
        x: 0,
        y: 0,
        width: w,
        height: h,
        rgba,
    }
}

/// The subtitle lane's consumer: pulls packets, decodes under `catch_unwind`
/// (a panicking subtitle decoder drops the cue, never the playback), and
/// shows/hides cues on the clock. In `realtime == false` every cue is shown
/// and cleared immediately so captures see the exact cue sequence. Ends at
/// the lane's end, when the player stops, or when a selection switch sets
/// `retired`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_subtitle_loop(
    mut decoder: Box<dyn Decoder>,
    mut sink: Box<dyn SubtitleSink>,
    clock: Arc<dyn Clock>,
    time_base: oxideav_core::TimeBase,
    video_width: u32,
    video_height: u32,
    realtime: bool,
    lane: Arc<Lane>,
    demux_cv: Arc<parking_lot::Condvar>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
    retired: Arc<std::sync::atomic::AtomicBool>,
) {
    let w = video_width.max(320);
    let h = video_height.max(240);
    let quit = || stopped.load(Ordering::SeqCst) || retired.load(Ordering::SeqCst);

    while !quit() {
        let packet = {
            let mut q = lane.queue.lock();
            loop {
                match q.first() {
                    Some(p) if p.stream_index == u32::MAX => {
                        q.remove(0);
                        break None;
                    }
                    Some(_) => break Some(q.remove(0)),
                    None => {
                        if quit() {
                            break None;
                        }
                        demux_cv.notify_one();
                        lane.cv.wait_for(&mut q, Duration::from_millis(100));
                    }
                }
            }
        };
        let Some(packet) = packet else { break };
        demux_cv.notify_one();

        if decoder.send_packet(&packet).is_err() {
            continue;
        }

        loop {
            if quit() {
                return;
            }
            let recv = std::panic::catch_unwind(AssertUnwindSafe(|| decoder.receive_frame()));
            let frame = match recv {
                Ok(Ok(f)) => f,
                Ok(Err(oxideav_core::Error::NeedMore))
                | Ok(Err(oxideav_core::Error::Eof)) => break,
                Ok(Err(_)) | Err(_) => break,
            };

            let (start_pts, end_pts, image) = match frame {
                Frame::Subtitle(cue) => {
                    let start = Duration::from_micros(cue.start_us.max(0) as u64);
                    let end = Duration::from_micros(cue.end_us.max(cue.start_us) as u64);
                    let img = match std::panic::catch_unwind(AssertUnwindSafe(|| {
                        render_text_cue(&cue, w, h)
                    })) {
                        Ok(img) => img,
                        Err(_) => continue,
                    };
                    (start, end, img)
                }
                Frame::Video(vf) => {
                    let ticks = vf.pts.or(packet.pts).unwrap_or(0).max(0);
                    let secs = time_base.seconds_of(ticks).max(0.0);
                    let start = Duration::from_secs_f64(secs);
                    let dur_secs = packet
                        .duration
                        .map(|d| time_base.seconds_of(d.max(0)))
                        .unwrap_or(2.0);
                    let end = start + Duration::from_secs_f64(dur_secs.max(0.5));
                    let img = match std::panic::catch_unwind(AssertUnwindSafe(|| {
                        extract_bitmap_cue(&vf, w, h)
                    })) {
                        Ok(img) => img,
                        Err(_) => continue,
                    };
                    (start, end, img)
                }
                _ => continue,
            };

            if !realtime {
                sink.show(&[image], w, h);
                sink.show(&[], w, h);
            } else {
                // Show at cue start on the clock.
                if !wait_for(&clock, start_pts, &quit) {
                    return;
                }
                sink.show(&[image], w, h);
                // Hide at cue end.
                if !wait_for(&clock, end_pts, &quit) {
                    // Stopped or retired with the cue up: take it down.
                    sink.show(&[], w, h);
                    return;
                }
                sink.show(&[], w, h);
            }
        }
    }
}

/// Sleeps until the clock reaches `at`. False when the pipeline should quit
/// (the player stopped or a selection switch retired it).
fn wait_for(clock: &Arc<dyn Clock>, at: Duration, quit: &impl Fn() -> bool) -> bool {
    loop {
        if quit() {
            return false;
        }
        let Some(now) = clock.now() else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        if now >= at {
            return true;
        }
        let sleep = (at - now).min(Duration::from_millis(50));
        std::thread::sleep(sleep);
    }
}
