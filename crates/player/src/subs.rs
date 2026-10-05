use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use oxideav_core::{Decoder, Frame, Packet, TimeBase};

use crate::backend::{Clock, SubtitleImage, SubtitleSink};

pub struct SubtitleCueEvent {
    pub start: Duration,
    pub end: Duration,
    pub image: SubtitleImage,
}

pub struct SubtitlePipeline {
    stopped: Arc<AtomicBool>,
}

impl SubtitlePipeline {
    pub fn new() -> Self {
        Self {
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

impl Default for SubtitlePipeline {
    fn default() -> Self {
        Self::new()
    }
}

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

pub fn run_subtitle_loop(
    mut decoder: Box<dyn Decoder>,
    mut sink: Box<dyn SubtitleSink>,
    clock: Arc<dyn Clock>,
    time_base: TimeBase,
    video_width: u32,
    video_height: u32,
    realtime: bool,
    queue: Arc<Mutex<Vec<Option<Packet>>>>,
    condvar: Arc<Condvar>,
    stopped: Arc<AtomicBool>,
) {
    let w = video_width.max(320);
    let h = video_height.max(240);

    while !stopped.load(Ordering::SeqCst) {
        let packet = {
            let mut q = queue.lock();
            while q.is_empty() && !stopped.load(Ordering::SeqCst) {
                condvar.wait(&mut q);
            }
            if stopped.load(Ordering::SeqCst) {
                break;
            }
            if q.is_empty() {
                continue;
            }
            q.remove(0)
        };

        let Some(packet) = packet else {
            // EOF
            break;
        };

        if decoder.send_packet(&packet).is_err() {
            continue;
        }

        while let Ok(frame) = decoder.receive_frame() {
            if stopped.load(Ordering::SeqCst) {
                break;
            }

            let (start_pts, end_pts, image) = match frame {
                Frame::Subtitle(cue) => {
                    let start = Duration::from_micros(cue.start_us.max(0) as u64);
                    let end = Duration::from_micros(cue.end_us.max(cue.start_us) as u64);
                    let img = render_text_cue(&cue, w, h);
                    (start, end, img)
                }
                Frame::Video(vf) => {
                    let ticks = vf.pts.or(packet.pts).unwrap_or(0).max(0);
                    let secs = time_base.seconds_of(ticks);
                    let start = Duration::from_secs_f64(secs.max(0.0));
                    let dur_secs = packet
                        .duration
                        .map(|d| time_base.seconds_of(d.max(0)))
                        .unwrap_or(2.0);
                    let end = start + Duration::from_secs_f64(dur_secs.max(0.5));
                    let img = extract_bitmap_cue(&vf, w, h);
                    (start, end, img)
                }
                _ => continue,
            };

            if !realtime {
                sink.show(&[image], w, h);
                sink.show(&[], w, h);
            } else {
                while !stopped.load(Ordering::SeqCst) {
                    if let Some(now) = clock.now() {
                        if now >= start_pts {
                            break;
                        }
                        let sleep_dur = (start_pts - now).min(Duration::from_millis(50));
                        std::thread::sleep(sleep_dur);
                    } else {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }

                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                sink.show(&[image], w, h);

                while !stopped.load(Ordering::SeqCst) {
                    if let Some(now) = clock.now() {
                        if now >= end_pts {
                            break;
                        }
                        let sleep_dur = (end_pts - now).min(Duration::from_millis(50));
                        std::thread::sleep(sleep_dur);
                    } else {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }

                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                sink.show(&[], w, h);
            }
        }
    }
}
