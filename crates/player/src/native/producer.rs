//! Makes the native route's media segments, on a thread of its own, from a
//! demuxer of its own: AVPlayer asks for them by index, mostly in order,
//! and jumps on a seek. Reading on from the last segment is the fast path;
//! any other segment costs a seek. Produced segments are kept up to a byte
//! budget, and the thread works ahead of the last request.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use oxideav_core::{AudioTrim, Demuxer, Packet, StreamInfo, TimeBase};
use parking_lot::{Condvar, Mutex};

use super::bridge::{self, Bridge, DecoderFactory, BLOCK, PREROLL_SECS};
use super::codec::{self, Framing};
use super::fmp4::{self, Run, Sample};
use super::plan::Plan;
use super::server::Body;
use super::types::{AudioConfig, VideoConfig};

pub(crate) const VIDEO_TRACK: u32 = 1;
pub(crate) const AUDIO_TRACK: u32 = 2;
pub(crate) const VIDEO_TIMESCALE: u32 = 90_000;

/// Produced segments kept in memory.
const CACHE_BYTES: usize = 192 << 20;
/// How far the thread works ahead of the last request.
const PREFETCH_SEGMENTS: usize = 6;
/// How far past a segment's end the read goes for its audio: an audio track
/// that ends early must not read on through the rest of the title.
const LOOKAHEAD_SECS: f64 = 4.0;

/// What a session remuxes. Fixed for the producer's life.
pub(crate) struct Streams {
    pub video: StreamInfo,
    pub video_config: VideoConfig,
    pub framing: Framing,
    /// The container stamps presentation times only (Matroska): decode
    /// times are derived per segment.
    pub synthesize_dts: bool,
    /// One frame, in video ticks: extrapolates decode times.
    pub frame_ticks: i64,
    pub audio: Option<(StreamInfo, AudioConfig)>,
    /// How the audio is made when it is not copied: decoded and encoded
    /// as FLAC by the bridge.
    pub bridging: Option<Bridging>,
    /// Source time, in video ticks, of item time 0: the first video decode
    /// time.
    pub origin: i64,
}

/// A bridged audio track: the engine's decoder for it, and the FLAC it
/// becomes.
#[derive(Clone)]
pub(crate) struct Bridging {
    pub factory: DecoderFactory,
    pub output: bridge::Output,
}

impl Streams {
    fn origin_secs(&self) -> f64 {
        self.video.time_base.seconds_of(self.origin)
    }
}

/// A segment's bytes, or why there are none.
pub(crate) type Segment = Result<Arc<[u8]>, String>;

pub(crate) struct Producer {
    inner: Arc<Inner>,
    thread: Mutex<Option<JoinHandle<Box<dyn Demuxer>>>>,
}

struct Inner {
    state: Mutex<State>,
    cv: Condvar,
    stopped: AtomicBool,
    /// A request missed the cache: a prefetch under way gives up.
    urgent: AtomicBool,
    /// The last segment went out: the playlist's end is in AVPlayer's hands.
    served_last: AtomicBool,
    /// Some segment went out.
    served_any: AtomicBool,
    /// AVPlayer asked for some segment.
    requested_any: AtomicBool,
    /// The segment being made, `usize::MAX` for none: a request for it waits
    /// instead of aborting it.
    in_flight: AtomicUsize,
    segments: usize,
}

#[derive(Default)]
struct State {
    cache: BTreeMap<usize, Arc<[u8]>>,
    bytes: usize,
    waiters: HashMap<usize, Vec<mpsc::Sender<Segment>>>,
    /// Missed segments, newest last: a seek's request goes first.
    misses: Vec<usize>,
    last_request: usize,
    /// Segments whose prefetch failed: made again only when asked for.
    failed: HashSet<usize>,
}

impl Producer {
    /// Starts producing for `plan` from `demuxer` (set to read only the
    /// video and audio streams).
    pub(crate) fn start(mut demuxer: Box<dyn Demuxer>, streams: Streams, plan: Plan) -> Producer {
        let mut active = vec![streams.video.index];
        active.extend(streams.audio.as_ref().map(|(s, _)| s.index));
        demuxer.set_active_streams(&active);
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            stopped: AtomicBool::new(false),
            urgent: AtomicBool::new(false),
            served_last: AtomicBool::new(false),
            served_any: AtomicBool::new(false),
            requested_any: AtomicBool::new(false),
            in_flight: AtomicUsize::new(usize::MAX),
            segments: plan.len(),
        });
        let work = Work {
            demuxer,
            streams,
            plan,
            next: None,
            carry: VecDeque::new(),
            audio_pos: None,
            bridge: None,
        };
        let thread_inner = Arc::clone(&inner);
        let thread = std::thread::Builder::new()
            .name("peartube-native-segments".into())
            .spawn(move || work.run(&thread_inner))
            .expect("failed to spawn the segment thread");
        Producer { inner, thread: Mutex::new(Some(thread)) }
    }

    /// Segment `index` (`None` past the plan): ready from the cache, or a
    /// promise of it.
    pub(crate) fn request(&self, index: usize) -> Option<Body> {
        if index >= self.inner.segments || self.inner.stopped.load(Ordering::SeqCst) {
            return None;
        }
        self.inner.requested_any.store(true, Ordering::SeqCst);
        let mut state = self.inner.state.lock();
        state.last_request = index;
        if let Some(bytes) = state.cache.get(&index) {
            let bytes = Arc::clone(bytes);
            drop(state);
            self.served(index);
            self.inner.cv.notify_all();
            return Some(Body::Ready(bytes));
        }
        let (tx, rx) = mpsc::channel();
        state.waiters.entry(index).or_default().push(tx);
        state.failed.remove(&index);
        if self.inner.in_flight.load(Ordering::SeqCst) != index {
            state.misses.retain(|&i| i != index);
            state.misses.push(index);
            self.inner.urgent.store(true, Ordering::SeqCst);
        }
        drop(state);
        self.inner.cv.notify_all();
        Some(Body::Pending(rx))
    }

    fn served(&self, index: usize) {
        self.inner.served_any.store(true, Ordering::SeqCst);
        if index + 1 == self.inner.segments {
            self.inner.served_last.store(true, Ordering::SeqCst);
        }
    }

    /// The last segment has been handed to AVPlayer.
    pub(crate) fn served_last(&self) -> bool {
        self.inner.served_last.load(Ordering::SeqCst)
    }

    /// Stops the thread and hands back the demuxer (once). The demuxer's
    /// source must already be stopped if the thread may sit in a read.
    pub(crate) fn stop(&self) -> Option<Box<dyn Demuxer>> {
        self.halt();
        let thread = self.thread.lock().take();
        thread.and_then(|thread| thread.join().ok())
    }

    /// A segment has gone out: AVPlayer has the media it starts with.
    pub(crate) fn served_any(&self) -> bool {
        self.inner.served_any.load(Ordering::SeqCst)
    }

    /// AVPlayer has asked for a segment: it got past the playlists.
    pub(crate) fn requested_any(&self) -> bool {
        self.inner.requested_any.load(Ordering::SeqCst)
    }

    fn halt(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.urgent.store(true, Ordering::SeqCst);
        let mut state = self.inner.state.lock();
        for (_, waiters) in state.waiters.drain() {
            for waiter in waiters {
                let _ = waiter.send(Err("stopped".into()));
            }
        }
        drop(state);
        self.inner.cv.notify_all();
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl State {
    /// Frees cached segments until `need` more bytes fit the budget: those
    /// behind the last request first (farthest first, the one just before
    /// it last), then those beyond the prefetch window. Never `keep` or the
    /// window itself. Whether they fit now.
    fn make_room(&mut self, need: usize, keep: usize) -> bool {
        let from = self.last_request;
        let window = from..=from + PREFETCH_SEGMENTS;
        while self.bytes + need > CACHE_BYTES {
            let behind = self.cache.keys().copied().filter(|&i| i < from && i != keep).min();
            let beyond = self.cache.keys().copied().filter(|&i| !window.contains(&i) && i > from && i != keep).max();
            let Some(drop) = behind.or(beyond) else { return false };
            if let Some(bytes) = self.cache.remove(&drop) {
                self.bytes -= bytes.len();
            }
        }
        true
    }
}

impl Inner {
    fn deliver(&self, index: usize, segment: Segment) {
        let mut state = self.state.lock();
        let waiters = state.waiters.remove(&index).unwrap_or_default();
        if let Ok(bytes) = &segment {
            if !state.cache.contains_key(&index) {
                state.bytes += bytes.len();
                state.cache.insert(index, Arc::clone(bytes));
            }
            state.make_room(0, index);
        }
        drop(state);
        if !waiters.is_empty() && segment.is_ok() {
            self.served_any.store(true, Ordering::SeqCst);
            if index + 1 == self.segments {
                self.served_last.store(true, Ordering::SeqCst);
            }
        }
        for waiter in waiters {
            let _ = waiter.send(segment.clone());
        }
    }

    /// The next segment to make: the newest miss, else the next one not yet
    /// made within reach of the last request; `None` when stopped.
    fn next_job(&self) -> Option<(usize, bool)> {
        let mut state = self.state.lock();
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return None;
            }
            self.urgent.store(false, Ordering::SeqCst);
            while let Some(index) = state.misses.pop() {
                if let Some(bytes) = state.cache.get(&index).cloned() {
                    drop(state);
                    self.deliver(index, Ok(bytes));
                    state = self.state.lock();
                    continue;
                }
                return Some((index, true));
            }
            let from = state.last_request;
            let ahead = (from..self.segments.min(from + PREFETCH_SEGMENTS + 1))
                .find(|i| !state.cache.contains_key(i) && !state.failed.contains(i));
            if let Some(index) = ahead {
                let typical = state.bytes.checked_div(state.cache.len()).unwrap_or(0);
                if state.make_room(typical, index) {
                    return Some((index, false));
                }
            }
            self.cv.wait_for(&mut state, Duration::from_millis(500));
        }
    }
}

/// The producer thread's side: the demuxer and where it stands.
struct Work {
    demuxer: Box<dyn Demuxer>,
    streams: Streams,
    plan: Plan,
    /// The segment the demuxer reads on into, after the last one made.
    next: Option<usize>,
    /// Packets read past the last segment made, in read order, with their
    /// audio trims.
    carry: VecDeque<(Packet, Option<AudioTrim>)>,
    /// Where the next audio sample starts on the item axis (samples), when
    /// the demuxer reads on from the last segment.
    audio_pos: Option<i64>,
    /// The bridge, while production reads on from the last segment.
    bridge: Option<Bridge>,
}

/// One segment's packets.
#[derive(Default)]
struct Gathered {
    video: Vec<Packet>,
    /// With their trims: the bridge's decoder applies them.
    audio: Vec<(Packet, Option<AudioTrim>)>,
    /// Presentation times of the next segment's first video packets, in
    /// decode order: its first decode time derives from them.
    next_video_pts: Vec<i64>,
    /// The next segment's first packet's decode time, for containers that
    /// stamp it.
    next_video_dts: Option<i64>,
}

impl Work {
    fn run(mut self, inner: &Inner) -> Box<dyn Demuxer> {
        while let Some((index, urgent)) = inner.next_job() {
            inner.in_flight.store(index, Ordering::SeqCst);
            let abort = || inner.stopped.load(Ordering::SeqCst) || (!urgent && inner.urgent.load(Ordering::SeqCst));
            let made = std::panic::catch_unwind(AssertUnwindSafe(|| self.produce(index, &abort)))
                .unwrap_or_else(|_| Err("segment production panicked".into()));
            inner.in_flight.store(usize::MAX, Ordering::SeqCst);
            match made {
                Ok(Some(bytes)) => inner.deliver(index, Ok(Arc::from(bytes))),
                Ok(None) => {
                    // A request that waited on this prefetch is still owed
                    // it, after the miss that cut it short.
                    let mut state = inner.state.lock();
                    if state.waiters.contains_key(&index) && !state.misses.contains(&index) {
                        state.misses.insert(0, index);
                    }
                }
                Err(e) => {
                    log(&format!("segment {index}: {e}"));
                    self.next = None;
                    // A request since then (or this one's) gets the error;
                    // a failed prefetch is not tried again unasked.
                    inner.state.lock().failed.insert(index);
                    inner.deliver(index, Err(e));
                }
            }
        }
        self.demuxer
    }

    /// Segment `index`'s bytes, or `None` when `abort` said to give up.
    fn produce(&mut self, index: usize, abort: &dyn Fn() -> bool) -> Result<Option<Vec<u8>>, String> {
        if self.next != Some(index) {
            self.reposition(index)?;
        }
        // From here an abort leaves the demuxer mid-segment.
        self.next = None;
        let Some(gathered) = self.gather(index, abort)? else {
            return Ok(None);
        };
        let Gathered { video, audio, next_video_pts, next_video_dts } = gathered;
        let video = self.video_run(index, video, &next_video_pts, next_video_dts)?;
        let audio = if self.streams.bridging.is_some() {
            match self.bridged_run(index, audio, abort)? {
                Some(run) => run,
                None => return Ok(None),
            }
        } else {
            self.audio_run(audio.into_iter().map(|(packet, _)| packet).collect())
        };
        let mut runs = vec![Run { track_id: VIDEO_TRACK, base_decode_time: video.0, samples: &video.1 }];
        if let Some((base, samples)) = &audio {
            runs.push(Run { track_id: AUDIO_TRACK, base_decode_time: *base, samples });
        }
        let bytes = fmp4::media_segment(index as u32 + 1, &runs);
        self.next = Some(index + 1);
        Ok(Some(bytes))
    }

    /// Lands one segment early, so the audio that a container stores before
    /// the keyframe opening `index` is read too.
    fn reposition(&mut self, index: usize) -> Result<(), String> {
        let land = self.plan.starts[index.saturating_sub(1)];
        let video = self.streams.video.index;
        let demuxer = &mut self.demuxer;
        match std::panic::catch_unwind(AssertUnwindSafe(|| demuxer.seek_to(video, land))) {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(format!("seek to segment {index} failed: {e}")),
            Err(_) => return Err(format!("seek to segment {index} panicked")),
        }
        self.carry.clear();
        self.audio_pos = None;
        self.bridge = None;
        Ok(())
    }

    /// The next packet and its audio trim (`packet_metadata` holds only
    /// until the next read).
    fn read(&mut self) -> Result<Option<(Packet, Option<AudioTrim>)>, String> {
        if let Some(read) = self.carry.pop_front() {
            return Ok(Some(read));
        }
        let demuxer = &mut self.demuxer;
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            demuxer.next_packet().map(|packet| {
                let trim = demuxer.packet_metadata().audio_trim;
                (packet, trim)
            })
        })) {
            Ok(Ok(read)) => Ok(Some(read)),
            Ok(Err(oxideav_core::Error::Eof)) => Ok(None),
            Ok(Err(e)) => Err(format!("demux error: {e}")),
            Err(_) => Err("demuxer panicked".into()),
        }
    }

    /// Reads segment `index`: the video from its opening keyframe up to the
    /// keyframe that opens the next one (decode order), the audio whose
    /// presentation time falls in it, and the next segment's first packets.
    fn gather(&mut self, index: usize, abort: &dyn Fn() -> bool) -> Result<Option<Gathered>, String> {
        let (start, end) = self.plan.bounds(index);
        let last = index + 1 == self.plan.len();
        let video_index = self.streams.video.index;
        let audio_index = self.streams.audio.as_ref().map(|(s, _)| s.index);
        let video_tb = self.streams.video.time_base;
        // The first segment takes audio from item time 0, the others from
        // their own start. A bridged track restarting here decodes from a
        // second before its first block; reading on, it takes everything
        // (what the last segment fed the bridge left the carry).
        let audio_from = match &self.streams.bridging {
            Some(bridging) if self.bridge.is_none() => {
                let rate = f64::from(bridging.output.rate);
                self.streams.origin_secs() + self.grid(index, bridging.output.rate) as f64 / rate - PREROLL_SECS
            }
            Some(_) => f64::NEG_INFINITY,
            None if index == 0 => self.streams.origin_secs(),
            None => video_tb.seconds_of(start),
        };
        let audio_to = video_tb.seconds_of(end);
        // A restarted bridge also takes the last packet before its pre-roll:
        // it may run into it (TTA's 1.045 s frames, long PCM packets).
        let mut before = None;
        let restart = self.streams.bridging.is_some() && self.bridge.is_none();
        let lookahead = self.streams.video_config.reorder_depth as usize + 1;
        let read_until = end + video_tb.ticks_of(LOOKAHEAD_SECS);

        let mut got = Gathered::default();
        let mut carry_out = VecDeque::new();
        let mut opened = false;
        let mut video_done = false;
        let mut audio_done = audio_index.is_none();
        loop {
            if abort() {
                return Ok(None);
            }
            if video_done && audio_done && (got.next_video_pts.len() >= lookahead || last) {
                break;
            }
            let Some((packet, trim)) = self.read()? else { break };
            if packet.stream_index == video_index {
                let Some(pts) = packet.pts.or(packet.dts) else { continue };
                if video_done && pts > read_until {
                    // The audio ended before this segment's end.
                    carry_out.push_back((packet, trim));
                    break;
                }
                if !opened {
                    // Up to the keyframe that opens this segment: the tail
                    // of the one before.
                    if !(packet.flags.keyframe && pts >= start - tolerance(video_tb)) {
                        continue;
                    }
                    opened = true;
                }
                if !last && !video_done && packet.flags.keyframe && pts >= end - tolerance(video_tb) {
                    video_done = true;
                    got.next_video_dts = packet.dts;
                }
                if video_done {
                    if got.next_video_pts.len() < lookahead {
                        got.next_video_pts.push(pts);
                    }
                    carry_out.push_back((packet, trim));
                } else {
                    got.video.push(packet);
                }
            } else if Some(packet.stream_index) == audio_index {
                let Some(pts) = packet.pts.or(packet.dts) else { continue };
                let at = packet.time_base.seconds_of(pts);
                if at < audio_from {
                    if restart {
                        before = Some((packet, trim));
                    }
                    continue;
                }
                if let Some(held) = before.take() {
                    got.audio.push(held);
                }
                if !last && at >= audio_to {
                    audio_done = true;
                    carry_out.push_back((packet, trim));
                } else {
                    got.audio.push((packet, trim));
                }
            }
        }
        got.audio.extend(before);
        if got.video.is_empty() {
            return Err(format!("segment {index}: no video from its keyframe on"));
        }
        carry_out.extend(self.carry.drain(..));
        self.carry = carry_out;
        Ok(Some(got))
    }

    /// The video run: base decode time and samples, on the item axis at
    /// `VIDEO_TIMESCALE`. `next_pts`/`next_dts`: the next segment's first
    /// packets, where this run's last picture ends.
    fn video_run(
        &self,
        index: usize,
        video: Vec<Packet>,
        next_pts: &[i64],
        next_dts: Option<i64>,
    ) -> Result<(u64, Vec<Sample>), String> {
        let streams = &self.streams;
        let tb = streams.video.time_base;
        let frame = streams.frame_ticks.max(1);
        let depth = streams.video_config.reorder_depth as usize;
        let pts: Vec<i64> = video.iter().map(|p| p.pts.or(p.dts).unwrap_or(0)).collect();
        let mut dts: Vec<i64> = if streams.synthesize_dts {
            decode_times(&pts, depth, frame)
        } else {
            video.iter().zip(&pts).map(|(p, &pts)| p.dts.unwrap_or(pts)).collect()
        };
        for i in 1..dts.len() {
            dts[i] = dts[i].max(dts[i - 1] + 1);
        }
        let last = *dts.last().expect("video is not empty");
        let next = if streams.synthesize_dts {
            next_pts.iter().min().map(|&first| first - depth as i64 * frame)
        } else {
            next_dts
        };
        let next = next.filter(|&next| next > last).unwrap_or_else(|| {
            let duration = video.last().and_then(|p| p.duration).filter(|&d| d > 0);
            last + duration.unwrap_or(frame)
        });
        let item = |ticks: i64| rescale(ticks - streams.origin, tb, VIDEO_TIMESCALE);
        let base = item(dts[0]);
        if base < 0 {
            return Err(format!("segment {index} decodes before the item's start"));
        }
        let mut samples = Vec::with_capacity(video.len());
        for (i, packet) in video.into_iter().enumerate() {
            let this = item(dts[i]);
            let following = item(dts.get(i + 1).copied().unwrap_or(next));
            let mut data = codec::video_sample(packet.data, streams.framing);
            if let Some(dovi) = &streams.video_config.dovi {
                data = codec::strip_hevc_el_nals(data, streams.video_config.nal_length_size, dovi.convert_p7_to_81);
            }
            samples.push(Sample {
                duration: (following - this).clamp(1, i64::from(u32::MAX)) as u32,
                composition_offset: (item(pts[i]) - this).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
                sync: packet.flags.keyframe,
                data,
            });
        }
        Ok((base as u64, samples))
    }

    /// Item sample where segment `index`'s audio blocks start: the grid
    /// point at or before the segment's start (`rate` the bridge's).
    fn grid(&self, index: usize, rate: u32) -> i64 {
        if index == 0 {
            return 0;
        }
        let start = self.streams.video.time_base.seconds_of(self.plan.starts[index]);
        let sample = ((start - self.streams.origin_secs()) * f64::from(rate)).round() as i64;
        sample.div_euclid(BLOCK) * BLOCK
    }

    /// A bridged track's run: the FLAC blocks starting in this segment's
    /// stretch of the grid. `Ok(None)`: `abort` said to give up.
    #[allow(clippy::type_complexity)]
    fn bridged_run(
        &mut self,
        index: usize,
        audio: Vec<(Packet, Option<AudioTrim>)>,
        abort: &dyn Fn() -> bool,
    ) -> Result<Option<Option<(u64, Vec<Sample>)>>, String> {
        let bridging = self.streams.bridging.clone().expect("a bridged track");
        let rate = bridging.output.rate;
        let from = self.grid(index, rate);
        let last = index + 1 == self.plan.len();
        let to = (!last).then(|| self.grid(index + 1, rate));
        let audio_index = self.streams.audio.as_ref().map(|(s, _)| s.index);
        // The last segment reads to the end of the file; audio long past the
        // last picture would only make it huge.
        let end_secs = self.streams.video.time_base.seconds_of(self.plan.end);
        let end = ((end_secs - self.streams.origin_secs()) * f64::from(rate)).round() as i64;
        if self.bridge.is_none() {
            self.bridge = Some(Bridge::new(&bridging.factory, bridging.output, self.streams.origin_secs(), from)?);
        }
        let bridge = self.bridge.as_mut().expect("made above");
        for (packet, trim) in &audio {
            if abort() {
                // Half fed: the next production starts it over.
                self.bridge = None;
                return Ok(None);
            }
            let at = packet.pts.or(packet.dts).map(|ticks| packet.time_base.seconds_of(ticks));
            if last && at.is_some_and(|at| at > end_secs + PREROLL_SECS) {
                break;
            }
            bridge.feed(packet, *trim);
        }
        let blocks = match to {
            Some(to) => {
                // A decoder that holds output back: decode on into the next
                // segment's packets, taking them out of the carry (no
                // timestamp tells them apart: Matroska stamps TrueHD's
                // 0.83 ms access units in whole milliseconds).
                let mut i = 0;
                while i < self.carry.len() && bridge.decoded_to() < to {
                    if Some(self.carry[i].0.stream_index) == audio_index {
                        let (packet, trim) = self.carry.remove(i).expect("in range");
                        bridge.feed(&packet, trim);
                    } else {
                        i += 1;
                    }
                }
                bridge.blocks(to)
            }
            None => {
                bridge.finish();
                bridge.tail(end)
            }
        };
        let Some(first) = blocks.first() else { return Ok(Some(None)) };
        let base = first.at.max(0) as u64;
        let samples = blocks
            .into_iter()
            .map(|block| Sample { data: block.data, duration: block.samples, composition_offset: 0, sync: true })
            .collect();
        Ok(Some(Some((base, samples))))
    }

    /// The audio run, or `None` without audio. Durations come from the
    /// codec; the position runs on from the last segment and is set from
    /// the packets' times after a seek or across a gap.
    fn audio_run(&mut self, packets: Vec<Packet>) -> Option<(u64, Vec<Sample>)> {
        let (_, config) = self.streams.audio.as_ref()?;
        let rate = f64::from(config.sample_rate);
        let origin = self.streams.origin_secs();
        let position = |p: &Packet| {
            let pts = p.pts.or(p.dts).unwrap_or(0);
            ((p.time_base.seconds_of(pts) - origin) * rate).round() as i64
        };
        let mut samples: Vec<Sample> = Vec::with_capacity(packets.len());
        let mut base = None;
        let mut pos = self.audio_pos.unwrap_or(0);
        for packet in packets {
            let at = position(&packet);
            let frame = codec::audio_packet_samples(config, &packet.data);
            if frame == 0 {
                continue;
            }
            match base {
                None => {
                    // Continue from the last segment unless the packets
                    // say otherwise by more than a frame.
                    if self.audio_pos.is_none() || (at - pos).abs() > i64::from(frame) {
                        pos = at;
                    }
                    if pos < 0 {
                        continue;
                    }
                    base = Some(pos);
                }
                Some(_) if at - pos > i64::from(frame) => {
                    // A hole in the track: the last sample lasts until
                    // the next one.
                    if let Some(prev) = samples.last_mut() {
                        prev.duration += (at - pos) as u32;
                    }
                    pos = at;
                }
                Some(_) => {}
            }
            samples.push(Sample {
                data: codec::audio_sample(config, packet.data),
                duration: frame,
                composition_offset: 0,
                sync: true,
            });
            pos += i64::from(frame);
        }
        self.audio_pos = Some(pos);
        Some((base.unwrap_or(pos.max(0)) as u64, samples))
    }
}

/// Decode times for one segment's video, given its presentation times in
/// decode order: the presentation times sorted, delayed by `depth` frames,
/// and before that extrapolated a frame at a time. With `depth` at least
/// the stream's reorder depth, no picture presents before it decodes.
fn decode_times(pts: &[i64], depth: usize, frame: i64) -> Vec<i64> {
    let mut sorted = pts.to_vec();
    sorted.sort_unstable();
    (0..pts.len())
        .map(|i| if i >= depth { sorted[i - depth] } else { sorted[0] - (depth - i) as i64 * frame })
        .collect()
}

/// Half a millisecond in `tb`, at least a tick: an index stamped at a
/// coarser resolution than the packets still opens its segment.
fn tolerance(tb: TimeBase) -> i64 {
    tb.ticks_of(0.0005).max(1)
}

/// `ticks` in `tb` as `timescale` units, rounded to nearest.
pub(crate) fn rescale(ticks: i64, tb: TimeBase, timescale: u32) -> i64 {
    tb.rescale(ticks, TimeBase::new(1, i64::from(timescale)))
}

fn log(message: &str) {
    eprintln!("[peartube-native] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_times_never_follow_presentation() {
        // I P B B P B B in decode order, 40 ms frames, reorder depth 2.
        let pts = [0, 120, 40, 80, 240, 160, 200];
        let dts = decode_times(&pts, 2, 40);
        assert_eq!(dts, vec![-80, -40, 0, 40, 80, 120, 160]);
        assert!(dts.iter().zip(&pts).all(|(d, p)| d <= p));
        assert!(dts.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn decode_times_join_up_across_segments() {
        // Two closed GOPs cut at the second keyframe (pts 280): the first
        // segment's last decode time is one frame before the second's
        // first.
        let first = decode_times(&[0, 120, 40, 80, 240, 160, 200], 2, 40);
        let second = decode_times(&[280, 400, 320, 360], 2, 40);
        assert_eq!(second[0] - first.last().unwrap(), 40);
    }

    #[test]
    fn rescale_rounds_to_the_nearest_tick() {
        let ms = TimeBase::new(1, 1000);
        assert_eq!(rescale(42, ms, 90_000), 3780);
        assert_eq!(rescale(-84, ms, 90_000), -7560);
        assert_eq!(rescale(1, TimeBase::new(1, 48_000), 90_000), 2);
    }
}
