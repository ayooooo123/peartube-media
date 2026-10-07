//! Subtitle pipeline: packets → cues → `SubtitleSink::show` on the clock.
//!
//! Text/ASS cues render through the oxideav-subtitle compositor and stay up
//! from their start to their end, overlapping cues together. Bitmap
//! subtitles (PGS, DVB, VobSub, ...) decode to display states: an RGBA
//! canvas the size of the subtitle plane, shown from its pts until its
//! [`VideoFrame::display_duration`] runs out or, without one, until the next
//! frame of the stream replaces it (a blank canvas clears the screen). The
//! sink gets a bitmap state cropped to its visible pixels, positioned on the
//! canvas, with the canvas size as the space it scales to the video.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use oxideav_core::{Decoder, Frame, Packet, TimeBase, VideoFrame};
use parking_lot::Condvar;

use crate::backend::{Clock, SubtitleImage, SubtitleSink};
use crate::clock::current_monotonic_ns;
use crate::engine::{Lane, QUEUE_MAX_SECS};

/// Longest the pipeline waits without looking at the clock again. The lane
/// wakes it on everything that matters (a packet, a seek, the clock starting
/// or stopping, a retire, the player stopping); this only bounds a missed
/// wake.
const MAX_WAIT: Duration = Duration::from_millis(100);

/// Largest bitmap canvas shown: the bitmap decoders' canvas cap.
const MAX_CANVAS_SIDE: usize = 4096;
const MAX_CANVAS_PIXELS: usize = 4096 * 4096;

/// Decoded cues waiting for their start, at most: behind video or audio
/// only cues stamped beyond the demuxer's read-ahead pile up there.
const MAX_PENDING_CUES: usize = 64;
const MAX_PENDING_BYTES: usize = 64 << 20;

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

/// One bitmap display state, as the sink gets it.
#[derive(Clone, Debug)]
pub struct BitmapCue {
    /// Size of the subtitle plane the canvas covers: the coordinate space of
    /// `image`, which the sink scales to the video.
    pub canvas_width: u32,
    pub canvas_height: u32,
    /// The canvas cropped to its visible (non-transparent) pixels; `None`
    /// when the canvas is blank, which clears the screen.
    pub image: Option<SubtitleImage>,
}

/// Reads one bitmap display state: an RGBA canvas `VideoFrame` (straight
/// alpha, `stride / 4` pixels wide, `stride` bytes per row) as bitmap
/// subtitle decoders emit it. `None` when the frame is no such canvas or is
/// larger than the engine's video limits.
pub fn extract_bitmap_cue(frame: &VideoFrame) -> Option<BitmapCue> {
    let plane = frame.image_planes().first()?;
    let stride = plane.stride;
    if stride == 0 || stride % 4 != 0 {
        return None;
    }
    let (width, height) = (stride / 4, plane.data.len() / stride);
    if width == 0
        || height == 0
        || width > MAX_CANVAS_SIDE
        || height > MAX_CANVAS_SIDE
        || width * height > MAX_CANVAS_PIXELS
    {
        return None;
    }
    let rows = || plane.data.chunks_exact(stride).take(height);
    let (mut left, mut right, mut top, mut bottom) = (width, 0, height, 0);
    for (y, row) in rows().enumerate() {
        let mut pixels = row.chunks_exact(4);
        let Some(first) = pixels.position(|px| px[3] != 0) else {
            continue;
        };
        let last = row.chunks_exact(4).rposition(|px| px[3] != 0).unwrap_or(first);
        left = left.min(first);
        right = right.max(last + 1);
        top = top.min(y);
        bottom = y + 1;
    }
    let image = (left < right).then(|| {
        let mut rgba = Vec::with_capacity((right - left) * (bottom - top) * 4);
        for row in rows().skip(top).take(bottom - top) {
            rgba.extend_from_slice(&row[left * 4..right * 4]);
        }
        SubtitleImage {
            x: left as i32,
            y: top as i32,
            width: (right - left) as u32,
            height: (bottom - top) as u32,
            rgba,
        }
    });
    Some(BitmapCue {
        canvas_width: width as u32,
        canvas_height: height as u32,
        image,
    })
}

/// `ticks` of `time_base` as media time; negative times are 0.
fn media_time(ticks: i64, time_base: TimeBase) -> Duration {
    if !time_base.is_valid() || ticks <= 0 {
        return Duration::ZERO;
    }
    let nanos = time_base.rescale(ticks, TimeBase::new(1, 1_000_000_000));
    Duration::from_nanos(nanos.max(0) as u64)
}

/// One decoded subtitle: what goes on screen from `start`.
struct Cue {
    start: Duration,
    /// When it leaves the screen; `None` until the next cue of the stream
    /// replaces it (a bitmap state without a display duration).
    end: Option<Duration>,
    content: Content,
}

impl Cue {
    /// Bytes of the bitmap it holds.
    fn bytes(&self) -> usize {
        match &self.content {
            Content::Text(image) => image.rgba.len(),
            Content::Bitmap(state) => state.image.as_ref().map_or(0, |image| image.rgba.len()),
        }
    }
}

enum Content {
    /// A text cue on a video-sized canvas; it shares the screen with the
    /// text cues it overlaps.
    Text(SubtitleImage),
    /// A bitmap display state: it replaces whatever is up.
    Bitmap(BitmapCue),
}

fn decoded_cue(frame: Frame, packet: &Packet, time_base: TimeBase, width: u32, height: u32) -> Option<Cue> {
    match frame {
        Frame::Subtitle(cue) => {
            let start = Duration::from_micros(cue.start_us.max(0) as u64);
            let end = Duration::from_micros(cue.end_us.max(cue.start_us).max(0) as u64);
            Some(Cue { start, end: Some(end), content: Content::Text(render_text_cue(&cue, width, height)) })
        }
        Frame::Video(vf) => {
            let start = media_time(vf.pts.or(packet.pts).unwrap_or(0), time_base);
            let end = vf.display_duration().and_then(|d| start.checked_add(d));
            let state = extract_bitmap_cue(&vf)?;
            Some(Cue { start, end, content: Content::Bitmap(state) })
        }
        _ => None,
    }
}

/// What is on screen: text cues, or one bitmap state (a stream carries one
/// kind; a cue of the other kind replaces everything up).
#[derive(Default)]
struct OnScreen {
    text: Vec<SubtitleImage>,
    /// When each of `text` goes.
    text_ends: Vec<Duration>,
    /// The bitmap state and when it goes, if it has an end.
    bitmap: Option<(BitmapCue, Option<Duration>)>,
}

impl OnScreen {
    /// When the next thing on screen goes.
    fn next_end(&self) -> Option<Duration> {
        let bitmap = self.bitmap.as_ref().and_then(|(_, end)| *end);
        self.text_ends.iter().copied().chain(bitmap).min()
    }

    /// Takes down everything that goes at or before `at`.
    fn expire(&mut self, at: Duration) {
        if self.bitmap.as_ref().is_some_and(|(_, end)| end.is_some_and(|end| end <= at)) {
            self.bitmap = None;
        }
        let mut i = 0;
        while i < self.text.len() {
            if self.text_ends[i] <= at {
                self.text.remove(i);
                self.text_ends.remove(i);
            } else {
                i += 1;
            }
        }
    }

    fn put(&mut self, cue: Cue) {
        match cue.content {
            Content::Text(image) => {
                self.bitmap = None;
                self.text.push(image);
                self.text_ends.push(cue.end.unwrap_or(cue.start));
            }
            Content::Bitmap(state) => {
                self.text.clear();
                self.text_ends.clear();
                // A blank state has already cleared the screen; its own
                // nominal duration must not delay EOF or schedule a second
                // clear (DVB attaches its page timeout to blank states too).
                let end = state.image.as_ref().and(cue.end);
                self.bitmap = Some((state, end));
            }
        }
    }

    fn clear(&mut self) {
        *self = OnScreen::default();
    }
}

/// Applies, in time order, every change due by `now`: things going at their
/// end and pending cues coming up at their start (an end before a start at
/// the same time). True when anything changed.
fn advance(on: &mut OnScreen, pending: &mut VecDeque<Cue>, now: Duration) -> bool {
    let mut changed = false;
    loop {
        let end = on.next_end().filter(|&end| end <= now);
        let start = pending.front().map(|cue| cue.start).filter(|&start| start <= now);
        match (end, start) {
            (Some(end), start) if start.is_none_or(|start| end <= start) => on.expire(end),
            (_, Some(_)) => {
                if let Some(cue) = pending.pop_front() {
                    on.put(cue);
                }
            }
            _ => return changed,
        }
        changed = true;
    }
}

/// Drops the cues due last while more wait than `MAX_PENDING_*` allow,
/// keeping the next one.
fn bound(pending: &mut VecDeque<Cue>) {
    let mut bytes: usize = pending.iter().map(Cue::bytes).sum();
    while pending.len() > 1 && (pending.len() > MAX_PENDING_CUES || bytes > MAX_PENDING_BYTES) {
        if let Some(dropped) = pending.pop_back() {
            bytes -= dropped.bytes();
        }
    }
}

/// The sink, and whether anything is up on it.
struct Screen {
    sink: Box<dyn SubtitleSink>,
    /// The space text cues are rendered in.
    video_width: u32,
    video_height: u32,
    shown: bool,
}

impl Screen {
    /// Shows `images` laid out in a `width x height` space; a clear when
    /// nothing is up is skipped.
    fn put(&mut self, images: &[SubtitleImage], width: u32, height: u32) {
        if images.is_empty() && !self.shown {
            return;
        }
        self.sink.show(images, width, height);
        self.shown = !images.is_empty();
    }

    fn show(&mut self, on: &OnScreen) {
        match &on.bitmap {
            Some((state, _)) => {
                let images = state.image.as_ref().map(std::slice::from_ref).unwrap_or_default();
                self.put(images, state.canvas_width, state.canvas_height);
            }
            None => self.put(&on.text, self.video_width, self.video_height),
        }
    }

    fn clear(&mut self) {
        self.put(&[], self.video_width, self.video_height);
    }
}

/// What woke a wait on the lane.
enum Woke {
    Packet(Packet),
    /// The demuxer's end marker.
    Eof,
    /// Anything else: the awaited time came, a seek, a retire, the player
    /// stopping, the clock starting or stopping. The caller looks again.
    Other,
}

/// The subtitle lane's consumer and what it works with.
pub(crate) struct SubtitlePipeline {
    pub(crate) decoder: Box<dyn Decoder>,
    /// A fresh decoder for the stream, when one cannot be reset after a seek.
    pub(crate) new_decoder: Box<dyn Fn() -> oxideav_core::Result<Box<dyn Decoder>> + Send>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) time_base: TimeBase,
    pub(crate) video_width: u32,
    pub(crate) video_height: u32,
    pub(crate) realtime: bool,
    pub(crate) lane: Arc<Lane>,
    pub(crate) demux_cv: Arc<Condvar>,
    /// The playback's seek generation; a change starts the pipeline over.
    pub(crate) seek_generation: Box<dyn Fn() -> u64 + Send>,
    /// Video or audio pipelines bound the demuxer's read-ahead (they drain
    /// their own lanes); otherwise only this lane does.
    pub(crate) paced: Box<dyn Fn() -> bool + Send>,
    pub(crate) stopped: Arc<AtomicBool>,
    pub(crate) retired: Arc<AtomicBool>,
}

impl SubtitlePipeline {
    fn quit(&self) -> bool {
        self.stopped.load(Ordering::SeqCst) || self.retired.load(Ordering::SeqCst)
    }

    /// Waits on the lane until `at` on the clock (`None`: no timed change
    /// ahead), until it can hand out a packet or the end marker for seek
    /// generation `seen_seek` (when `take`), or until anything else wakes
    /// it. The clock is read with the lane locked: a clock that starts or
    /// stops after that wakes the lane (`wake_lanes`) once this waits.
    fn wait(&self, at: Option<Duration>, take: bool, seen_seek: u64) -> Woke {
        let mut q = self.lane.queue.lock();
        if self.quit() || (self.seek_generation)() != seen_seek {
            return Woke::Other;
        }
        if take && self.lane.seek_gen.load(Ordering::SeqCst) == seen_seek {
            match q.first() {
                Some(p) if p.packet.stream_index == u32::MAX => {
                    q.remove(0);
                    return Woke::Eof;
                }
                Some(_) => {
                    let packet = q.remove(0).packet;
                    drop(q);
                    self.demux_cv.notify_one();
                    return Woke::Packet(packet);
                }
                None => {}
            }
        }
        let timeout = match at {
            None => MAX_WAIT,
            Some(at) => {
                if self.clock.now().is_some_and(|now| now >= at) {
                    return Woke::Other;
                }
                // While the clock stands still nothing is due: its start
                // wakes the lane.
                match self.clock.monotonic_ns_at(at) {
                    Some(ns) => {
                        let left = ns.saturating_sub(current_monotonic_ns()).max(1_000_000);
                        Duration::from_nanos(left as u64).min(MAX_WAIT)
                    }
                    None => MAX_WAIT,
                }
            }
        };
        if take {
            // The demuxer may be waiting for this lane to make room.
            self.demux_cv.notify_one();
        }
        self.lane.cv.wait_for(&mut q, timeout);
        Woke::Other
    }

    /// Decodes `packet` under `catch_unwind` (a panicking subtitle decoder
    /// drops the cue, never the playback) and queues its cues.
    fn decode(&mut self, packet: &Packet, pending: &mut VecDeque<Cue>) {
        let decoder = &mut self.decoder;
        match std::panic::catch_unwind(AssertUnwindSafe(|| decoder.send_packet(packet))) {
            Ok(Ok(())) => {}
            _ => return,
        }
        let (w, h) = (self.video_width.max(320), self.video_height.max(240));
        loop {
            let decoder = &mut self.decoder;
            let frame = match std::panic::catch_unwind(AssertUnwindSafe(|| decoder.receive_frame())) {
                Ok(Ok(frame)) => frame,
                _ => break,
            };
            let cue = std::panic::catch_unwind(AssertUnwindSafe(|| {
                decoded_cue(frame, packet, self.time_base, w, h)
            }));
            if let Ok(Some(cue)) = cue {
                // Cues come up by start time, as VLC picks subpictures by
                // date: one stamped out of order (a hostile stamp, a PTS
                // jump) neither holds back the cues after it nor comes up
                // early. Equal starts keep their decode order.
                let at = pending.partition_point(|queued| queued.start <= cue.start);
                pending.insert(at, cue);
            }
        }
    }

    /// After a seek: the decoder starts over (`reset`, as FFmpeg flushes a
    /// subtitle decoder; a fresh one if that fails).
    fn restart_decoder(&mut self) -> bool {
        let decoder = &mut self.decoder;
        if let Ok(Ok(())) = std::panic::catch_unwind(AssertUnwindSafe(|| decoder.reset())) {
            return true;
        }
        match std::panic::catch_unwind(AssertUnwindSafe(|| (self.new_decoder)())) {
            Ok(Ok(decoder)) => {
                self.decoder = decoder;
                true
            }
            _ => false,
        }
    }
}

/// The subtitle lane's consumer: pulls packets, decodes them and shows each
/// cue on the clock from its start until its end, or a bitmap state until
/// the next one replaces it. In `realtime == false` nothing waits on the
/// clock: every text cue is shown and cleared at once and every bitmap state
/// shown as it decodes (cleared first when the previous one ended before
/// it), so captures see the exact cue sequence. A seek clears the screen
/// and starts over from the demuxer's new position. Alone, it ends at the
/// lane's end once nothing more is due (a bitmap state without an end stays
/// up). Behind video or audio it stays until they have played, then clears
/// the screen (the engine also retires it then). It also ends when the
/// player stops or a selection switch sets `retired`, clearing the screen.
pub(crate) fn run_subtitle_loop(mut pipe: SubtitlePipeline, sink: Box<dyn SubtitleSink>) {
    let mut screen = Screen {
        sink,
        video_width: pipe.video_width.max(320),
        video_height: pipe.video_height.max(240),
        shown: false,
    };
    let mut on = OnScreen::default();
    let mut pending: VecDeque<Cue> = VecDeque::new();
    let mut seen_seek = (pipe.seek_generation)();
    let mut eof = false;
    // Video or audio have drained their lanes during this pipeline.
    let mut was_paced = false;

    while !pipe.quit() {
        let generation = (pipe.seek_generation)();
        if generation != seen_seek {
            seen_seek = generation;
            pending.clear();
            on.clear();
            screen.clear();
            eof = false;
            if !pipe.restart_decoder() {
                break;
            }
            continue;
        }

        if !pipe.realtime {
            if eof {
                // The last state goes at its end.
                if on.next_end().is_some() {
                    screen.clear();
                }
                return;
            }
            match pipe.wait(None, true, seen_seek) {
                Woke::Packet(packet) => pipe.decode(&packet, &mut pending),
                Woke::Eof => eof = true,
                Woke::Other => {}
            }
            while let Some(cue) = pending.pop_front() {
                if let Content::Text(image) = &cue.content {
                    screen.put(std::slice::from_ref(image), screen.video_width, screen.video_height);
                    screen.clear();
                    continue;
                }
                if on.next_end().is_some_and(|end| end <= cue.start) {
                    on.clear();
                    screen.clear();
                }
                on.put(cue);
                screen.show(&on);
            }
            continue;
        }

        let now = pipe.clock.now();
        if let Some(now) = now {
            if advance(&mut on, &mut pending, now) {
                screen.show(&on);
            }
        }
        let next = on.next_end().into_iter().chain(pending.front().map(|cue| cue.start)).min();
        let paced = (pipe.paced)();
        was_paced |= paced;
        // Taking the next packet only once every decoded cue is up makes
        // this lane bound the demuxer's read-ahead when it is alone. Behind
        // video or audio, which bound it themselves, a cue due beyond that
        // read-ahead (a hostile stamp, a PTS jump) must not stop the lane:
        // the demuxer would wait on it and starve them.
        let horizon = now.unwrap_or_default() + Duration::from_secs_f64(QUEUE_MAX_SECS);
        let take = !eof && pending.front().is_none_or(|cue| cue.start > horizon && paced);
        if !take && next.is_none() {
            // Nothing more is due. Alone, the last state stays up as the
            // playback ends. Behind video or audio it stays while they
            // play (a seek may still restart this pipeline) and comes down
            // when they have played: the engine retires the pipeline then.
            if !paced {
                if was_paced {
                    on.clear();
                    screen.clear();
                }
                return;
            }
        }
        match pipe.wait(next, take, seen_seek) {
            Woke::Packet(packet) => {
                pipe.decode(&packet, &mut pending);
                bound(&mut pending);
            }
            Woke::Eof => eof = true,
            Woke::Other => {}
        }
    }
    // Stopped or retired: take down what is up.
    screen.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::VideoPlane;

    fn bitmap(pts: i64, duration: Option<Duration>, visible: bool) -> VideoFrame {
        let mut frame = VideoFrame {
            pts: Some(pts),
            planes: vec![VideoPlane { stride: 4, data: vec![7, 11, 13, if visible { 255 } else { 0 }] }],
        };
        if let Some(duration) = duration {
            frame.set_display_duration(duration);
        }
        frame
    }

    fn cue(frame: VideoFrame) -> Cue {
        let packet = Packet {
            stream_index: 0,
            pts: Some(500),
            dts: None,
            duration: Some(90000),
            time_base: TimeBase::new(1, 1000),
            flags: Default::default(),
            data: Vec::new(),
        };
        decoded_cue(Frame::Video(frame), &packet, packet.time_base, 320, 240).unwrap()
    }

    #[test]
    fn bitmap_uses_frame_end_not_packet_duration_or_minimum_timeout() {
        let open = cue(bitmap(1000, None, true));
        assert_eq!(open.start, Duration::from_secs(1));
        assert_eq!(open.end, None);
        let short = cue(bitmap(1000, Some(Duration::from_millis(125)), true));
        assert_eq!(short.end, Some(Duration::from_millis(1125)));
        let zero = cue(bitmap(1000, Some(Duration::ZERO), true));
        assert_eq!(zero.end, Some(Duration::from_secs(1)));
    }

    #[test]
    fn replacement_cancels_previous_timeout_and_expires_at_its_own_end() {
        let mut on = OnScreen::default();
        let mut pending = VecDeque::from([
            cue(bitmap(1000, Some(Duration::from_secs(10)), true)),
            cue(bitmap(2000, Some(Duration::from_millis(125)), true)),
        ]);
        assert!(advance(&mut on, &mut pending, Duration::from_secs(1)));
        assert_eq!(on.next_end(), Some(Duration::from_secs(11)));
        assert!(advance(&mut on, &mut pending, Duration::from_secs(2)));
        assert_eq!(on.next_end(), Some(Duration::from_millis(2125)));
        assert!(!advance(&mut on, &mut pending, Duration::from_millis(2124)));
        assert!(on.bitmap.as_ref().unwrap().0.image.is_some());
        assert!(advance(&mut on, &mut pending, Duration::from_millis(2125)));
        assert!(on.bitmap.is_none());
        assert!(!advance(&mut on, &mut pending, Duration::from_secs(11)));
    }

    #[test]
    fn open_bitmap_stays_until_a_blank_frame() {
        let mut on = OnScreen::default();
        let mut pending = VecDeque::from([
            cue(bitmap(1000, None, true)),
            cue(bitmap(5000, None, false)),
        ]);
        assert!(advance(&mut on, &mut pending, Duration::from_secs(1)));
        assert!(!advance(&mut on, &mut pending, Duration::from_millis(4999)));
        assert!(on.bitmap.as_ref().unwrap().0.image.is_some());
        assert!(advance(&mut on, &mut pending, Duration::from_secs(5)));
        assert!(on.bitmap.as_ref().unwrap().0.image.is_none());
        assert!(on.next_end().is_none());
    }

    #[test]
    fn finite_blank_state_has_nothing_left_to_expire() {
        let mut on = OnScreen::default();
        on.put(cue(bitmap(1000, Some(Duration::from_secs(15)), true)));
        assert_eq!(on.next_end(), Some(Duration::from_secs(16)));
        on.put(cue(bitmap(2000, Some(Duration::from_secs(15)), false)));
        assert!(on.bitmap.as_ref().unwrap().0.image.is_none());
        assert_eq!(on.next_end(), None);
        assert!(!advance(&mut on, &mut VecDeque::new(), Duration::from_secs(17)));
    }

    #[test]
    fn bitmap_crop_preserves_plane_geometry_and_excludes_side_channels() {
        let mut pixels = vec![0; 4 * 3 * 4];
        pixels[(4 + 2) * 4..(4 + 3) * 4].copy_from_slice(&[20, 30, 40, 128]);
        let frame = VideoFrame {
            pts: None,
            planes: vec![VideoPlane { stride: 16, data: pixels }],
        }.with_display_duration(Duration::from_secs(2));
        let bitmap = extract_bitmap_cue(&frame).unwrap();
        assert_eq!((bitmap.canvas_width, bitmap.canvas_height), (4, 3));
        let image = bitmap.image.unwrap();
        assert_eq!((image.x, image.y, image.width, image.height), (2, 1, 1, 1));
        assert_eq!(image.rgba, [20, 30, 40, 128]);
    }
}
