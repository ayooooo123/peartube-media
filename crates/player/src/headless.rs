//! The headless backend, for tests. Every sink records what it got
//! ([`Capture`]). Audio plays into a simulated sound card whose clock leads
//! the playback like a real device's; video frames are shown when the
//! engine hands them over, which it does when they are due on that clock.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;

use oxideav_core::{CodecParameters, Packet, PixelFormat, VideoFrame};

use crate::backend::{
    AudioSink, Backend, Clock, SinkError, SubtitleImage, SubtitleSink, VideoSink,
};
use crate::clock::current_monotonic_ns;

pub struct Headless {
    inner: Arc<Mutex<HeadlessInner>>,
    /// The clock of the playback being captured; subtitle shows are stamped
    /// with it.
    clock: Arc<Mutex<Option<Arc<dyn Clock>>>>,
}

pub struct Capture {
    pub video: Vec<VideoCapture>,
    pub audio: Vec<AudioCapture>,
    pub subtitles: Vec<SubtitleCapture>,
}

#[derive(Clone, Debug)]
pub struct VideoCapture {
    pub stream: u32,
    pub codec: String,
    pub pixel_format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub frame_md5: Vec<String>,
    pub pts: Vec<Duration>,
    /// When each frame was shown: CLOCK_MONOTONIC nanoseconds, one per entry
    /// of `pts`. The engine hands a frame over when it is due on the clock,
    /// and this sink shows it at once.
    pub shown_at: Vec<i64>,
    /// One entry per `VideoSink::flush` (the engine flushes on a seek): how
    /// many frames had been captured by then.
    pub flushes: Vec<usize>,
}

#[derive(Clone, Debug)]
pub struct AudioCapture {
    pub stream: u32,
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub pcm: Vec<f32>,
    /// One entry per accepted `AudioSink::write`: the pts the engine gave
    /// it and where its samples start in `pcm` (interleaved sample index).
    pub writes: Vec<(Duration, usize)>,
    /// One entry per `AudioSink::flush` (the engine flushes on a seek): how
    /// many writes had been captured by then.
    pub flushes: Vec<usize>,
    /// When the samples were heard (realtime playback): runs of continuous
    /// playback, each `(start, end, at)`: the device played `pcm[start..end]`
    /// (interleaved sample indices) from CLOCK_MONOTONIC `at` nanoseconds on,
    /// `device_rate` frames a second. Samples in no run were never heard
    /// (flushed by a seek, or still queued when the playback ended).
    pub played: Vec<(usize, usize, i64)>,
    /// Frames a second the simulated device plays: the sample rate times its
    /// speed (`Headless::set_audio_speed`).
    pub device_rate: f64,
}

#[derive(Clone, Debug)]
pub struct SubtitleCapture {
    pub stream: u32,
    pub codec: String,
    pub shows: Vec<(Duration, usize)>,
}

struct HeadlessInner {
    realtime: bool,
    /// How fast the simulated audio device plays, relative to its sample
    /// rate.
    audio_speed: f64,
    video: Vec<VideoCapture>,
    audio: Vec<AudioCapture>,
    subtitles: Vec<SubtitleCapture>,
    active_video_stream: Option<u32>,
    active_video_codec: Option<String>,
    active_audio_stream: Option<u32>,
    active_audio_codec: Option<String>,
    active_subtitle_stream: Option<u32>,
    active_subtitle_codec: Option<String>,
}

impl HeadlessInner {
    fn active_audio(&mut self) -> Option<&mut AudioCapture> {
        let stream = self.active_audio_stream.unwrap_or(0);
        self.audio.iter_mut().find(|a| a.stream == stream)
    }
}

/// How much audio the simulated device holds.
const DEVICE_BUFFER: Duration = Duration::from_millis(100);

/// The simulated sound card behind a headless audio sink. In realtime it
/// holds up to `DEVICE_BUFFER` of audio and, while it plays, plays it
/// `speed` times as fast as its sample rate (a device whose crystal is off);
/// its clock is the media time of the sample being heard, and stands still
/// while paused or when it has played everything it was given. Without
/// realtime it takes everything at once and its clock is the end of the
/// audio written.
struct Device {
    realtime: bool,
    rate: u32,
    channels: u16,
    speed: f64,
    playing: bool,
    /// Media time of the first frame written since the last open/flush.
    base: Option<Duration>,
    /// Frames taken since the last open/flush.
    written: u64,
    /// Frames played when playback last stopped.
    played: f64,
    /// Playback in progress: from frame `.0` on, since CLOCK_MONOTONIC `.1`.
    run: Option<(f64, i64)>,
    /// Where frame 0 since the last open/flush sits in the capture's `pcm`.
    pcm_base: usize,
}

impl Device {
    fn new(realtime: bool, speed: f64) -> Self {
        Self {
            realtime,
            rate: 0,
            channels: 0,
            speed,
            playing: false,
            base: None,
            written: 0,
            played: 0.0,
            run: None,
            pcm_base: 0,
        }
    }

    fn frames_per_sec(&self) -> f64 {
        f64::from(self.rate) * self.speed
    }

    fn capacity(&self) -> f64 {
        (DEVICE_BUFFER.as_secs_f64() * f64::from(self.rate)).floor()
    }

    /// Frames played by `now`.
    fn played_at(&self, now: i64) -> f64 {
        match self.run {
            Some((from, since)) => {
                let played = from + (now - since) as f64 * self.frames_per_sec() / 1e9;
                played.min(self.written as f64)
            }
            None => self.played,
        }
    }

    /// Ends the playback in progress at `now`, or where it ran out of audio
    /// before that: `(first frame, end frame, start time)`.
    fn stop(&mut self, now: i64) -> Option<(f64, f64, i64)> {
        let played = self.played_at(now);
        let (from, since) = self.run.take()?;
        self.played = played;
        Some((from, played, since))
    }

    /// Forgets everything written (open, flush): the clock restarts at the
    /// next write.
    fn restart(&mut self) {
        self.base = None;
        self.written = 0;
        self.played = 0.0;
        self.run = None;
    }

    fn now(&self) -> Option<Duration> {
        let base = self.base?;
        let frames = if self.realtime {
            self.played_at(current_monotonic_ns())
        } else {
            self.written as f64
        };
        Some(base + Duration::from_secs_f64(frames / f64::from(self.rate.max(1))))
    }

    /// When media time `at` plays: known while the device plays and `at` is
    /// within the audio it holds.
    fn monotonic_ns_at(&self, at: Duration) -> Option<i64> {
        if !self.realtime {
            return None;
        }
        let base = self.base?;
        let (from, since) = self.run?;
        let frame = (at.as_secs_f64() - base.as_secs_f64()) * f64::from(self.rate);
        if frame > self.written as f64 {
            return None;
        }
        Some(since + ((frame - from) / self.frames_per_sec() * 1e9).round() as i64)
    }
}

/// A headless audio sink's clock: its device's.
struct HeadlessAudioClock(Arc<Mutex<Device>>);

impl Clock for HeadlessAudioClock {
    fn now(&self) -> Option<Duration> {
        self.0.lock().now()
    }

    fn monotonic_ns_at(&self, at: Duration) -> Option<i64> {
        self.0.lock().monotonic_ns_at(at)
    }
}

static HEADLESS_REGISTRY: Mutex<Option<HashMap<usize, Weak<Headless>>>> = Mutex::new(None);

pub fn find_headless(backend_ptr: usize) -> Option<Arc<Headless>> {
    let mut reg = HEADLESS_REGISTRY.lock();
    if let Some(map) = reg.as_mut() {
        map.get(&backend_ptr).and_then(|w| w.upgrade())
    } else {
        None
    }
}

impl Headless {
    pub fn new() -> Arc<Headless> {
        let this = Arc::new(Headless {
            inner: Arc::new(Mutex::new(HeadlessInner {
                realtime: true,
                audio_speed: 1.0,
                video: Vec::new(),
                audio: Vec::new(),
                subtitles: Vec::new(),
                active_video_stream: None,
                active_video_codec: None,
                active_audio_stream: None,
                active_audio_codec: None,
                active_subtitle_stream: None,
                active_subtitle_codec: None,
            })),
            clock: Arc::new(Mutex::new(None)),
        });

        let ptr = Arc::as_ptr(&this) as *const () as usize;
        let mut reg = HEADLESS_REGISTRY.lock();
        let map = reg.get_or_insert_with(HashMap::new);
        map.insert(ptr, Arc::downgrade(&this));

        this
    }

    pub fn set_active_streams(
        &self,
        video: Option<(u32, String)>,
        audio: Option<(u32, String)>,
        subtitle: Option<(u32, String)>,
        realtime: bool,
    ) {
        let mut inner = self.inner.lock();
        inner.realtime = realtime;
        if let Some((idx, codec)) = video {
            inner.active_video_stream = Some(idx);
            inner.active_video_codec = Some(codec);
        }
        if let Some((idx, codec)) = audio {
            inner.active_audio_stream = Some(idx);
            inner.active_audio_codec = Some(codec);
        }
        if let Some((idx, codec)) = subtitle {
            inner.active_subtitle_stream = Some(idx);
            inner.active_subtitle_codec = Some(codec);
        }
    }

    /// The clock of the playback being captured: subtitle shows are stamped
    /// with it.
    pub fn set_clock(&self, clock: Arc<dyn Clock>) {
        *self.clock.lock() = Some(clock);
    }

    /// Makes the simulated audio device of the next playbacks play `speed`
    /// times as fast as its sample rate: 0.98 is a device running 2% slow.
    pub fn set_audio_speed(&self, speed: f64) {
        assert!(speed.is_finite() && speed > 0.0, "audio speed {speed}");
        self.inner.lock().audio_speed = speed;
    }

    pub fn capture(&self) -> Capture {
        let inner = self.inner.lock();
        Capture {
            video: inner.video.clone(),
            audio: inner.audio.clone(),
            subtitles: inner.subtitles.clone(),
        }
    }
}

impl Drop for Headless {
    fn drop(&mut self) {
        let ptr = self as *const Headless as *const () as usize;
        let mut reg = HEADLESS_REGISTRY.lock();
        if let Some(map) = reg.as_mut() {
            map.remove(&ptr);
        }
    }
}

impl Backend for Headless {
    fn audio(&self) -> Box<dyn AudioSink> {
        let device = {
            let inner = self.inner.lock();
            Device::new(inner.realtime, inner.audio_speed)
        };
        Box::new(HeadlessAudioSink {
            inner: Arc::clone(&self.inner),
            device: Arc::new(Mutex::new(device)),
        })
    }

    fn video(&self, _clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        Box::new(HeadlessVideoSink {
            inner: Arc::clone(&self.inner),
            stream_index: 0,
            codec: String::new(),
            pixel_format: PixelFormat::Yuv420P,
            width: 0,
            height: 0,
        })
    }

    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        Box::new(HeadlessSubtitleSink {
            inner: Arc::clone(&self.inner),
            clock: Arc::clone(&self.clock),
        })
    }
}

struct HeadlessAudioSink {
    inner: Arc<Mutex<HeadlessInner>>,
    device: Arc<Mutex<Device>>,
}

/// Records a run of playback that ended in the active stream's capture.
fn record_run(inner: &mut HeadlessInner, device: &Device, (from, to, since): (f64, f64, i64)) {
    if to <= from {
        return;
    }
    // Whole frames: the run starts at the frame nearest `from`, played when
    // the device got to it.
    let start = from.round();
    let at = since + ((start - from) / device.frames_per_sec() * 1e9).round() as i64;
    let channels = usize::from(device.channels);
    let base = device.pcm_base;
    if let Some(capture) = inner.active_audio() {
        capture.played.push((
            base + start as usize * channels,
            base + to.round() as usize * channels,
            at,
        ));
    }
}

impl HeadlessAudioSink {
    /// Ends the device's playback in progress at `now` and records it.
    fn stop(&self, inner: &mut HeadlessInner, device: &mut Device, now: i64) {
        if let Some(run) = device.stop(now) {
            record_run(inner, device, run);
        }
    }
}

impl AudioSink for HeadlessAudioSink {
    fn open(&mut self, sample_rate: u32, channels: u16) -> Result<(), SinkError> {
        let mut inner = self.inner.lock();
        let mut device = self.device.lock();
        self.stop(&mut inner, &mut device, current_monotonic_ns());
        device.realtime = inner.realtime;
        device.speed = inner.audio_speed;
        device.rate = sample_rate;
        device.channels = channels;
        device.restart();
        let device_rate = device.frames_per_sec();

        let stream = inner.active_audio_stream.unwrap_or(0);
        let codec = inner
            .active_audio_codec
            .clone()
            .unwrap_or_else(|| "audio".into());
        if let Some(ac) = inner.audio.iter_mut().find(|a| a.stream == stream) {
            ac.sample_rate = sample_rate;
            ac.channels = channels;
            ac.device_rate = device_rate;
        } else {
            inner.audio.push(AudioCapture {
                stream,
                codec,
                sample_rate,
                channels,
                pcm: Vec::new(),
                writes: Vec::new(),
                flushes: Vec::new(),
                played: Vec::new(),
                device_rate,
            });
        }
        Ok(())
    }

    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError> {
        loop {
            let mut inner = self.inner.lock();
            let mut device = self.device.lock();
            let channels = usize::from(device.channels);
            if channels == 0 || device.rate == 0 {
                return Ok(0);
            }
            let frames = pcm.len() / channels;
            let now = current_monotonic_ns();
            if device.run.is_some() && device.played_at(now) >= device.written as f64 {
                // It ran dry: that playback ended where the audio did.
                self.stop(&mut inner, &mut device, now);
            }
            let take = if device.realtime {
                let room = device.capacity() - (device.written as f64 - device.played_at(now));
                (room.max(0.0) as usize).min(frames)
            } else {
                frames
            };
            if take == 0 && frames > 0 {
                if !device.playing {
                    // Paused and full.
                    return Ok(0);
                }
                // Playing and full: the device makes room at its own pace.
                let room = device.capacity() - (device.written as f64 - device.played_at(now));
                let want = (frames as f64).min(device.capacity() / 4.0).max(1.0);
                let wait = Duration::from_secs_f64((want - room).max(1.0) / device.frames_per_sec());
                drop(device);
                drop(inner);
                std::thread::sleep(wait);
                continue;
            }

            if device.base.is_none() {
                device.base = Some(pts);
                device.pcm_base = inner.active_audio().map_or(0, |ac| ac.pcm.len());
            }
            if let Some(ac) = inner.active_audio() {
                let offset = ac.pcm.len();
                ac.pcm.extend_from_slice(&pcm[..take * channels]);
                ac.writes.push((pts, offset));
            }
            device.written += take as u64;
            if device.realtime && device.playing && device.run.is_none() {
                // New audio for an idle device plays from now.
                device.run = Some((device.played, now));
            }
            return Ok(take);
        }
    }

    fn play(&mut self) {
        let mut device = self.device.lock();
        if device.playing {
            return;
        }
        device.playing = true;
        if device.realtime && device.base.is_some() && device.played < device.written as f64 {
            device.run = Some((device.played, current_monotonic_ns()));
        }
    }

    fn pause(&mut self) {
        let mut inner = self.inner.lock();
        let mut device = self.device.lock();
        if !device.playing {
            return;
        }
        device.playing = false;
        self.stop(&mut inner, &mut device, current_monotonic_ns());
    }

    fn flush(&mut self) {
        let mut inner = self.inner.lock();
        if let Some(ac) = inner.active_audio() {
            let writes = ac.writes.len();
            ac.flushes.push(writes);
        }
        let mut device = self.device.lock();
        self.stop(&mut inner, &mut device, current_monotonic_ns());
        device.restart();
    }

    fn clock(&self) -> Arc<dyn Clock> {
        Arc::new(HeadlessAudioClock(Arc::clone(&self.device)))
    }
}

impl Drop for HeadlessAudioSink {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
        let mut device = self.device.lock();
        self.stop(&mut inner, &mut device, current_monotonic_ns());
    }
}

struct HeadlessVideoSink {
    inner: Arc<Mutex<HeadlessInner>>,
    stream_index: u32,
    codec: String,
    pixel_format: PixelFormat,
    width: u32,
    height: u32,
}

impl VideoSink for HeadlessVideoSink {
    fn open_compressed(&mut self, _params: &CodecParameters) -> bool {
        // Declined: force software decoding
        false
    }

    fn push_packet(&mut self, _packet: &Packet, _pts: Duration, _random_access: bool) -> Result<(), SinkError> {
        Ok(())
    }

    fn open_frames(&mut self, params: &CodecParameters) -> Result<(), SinkError> {
        let mut inner = self.inner.lock();
        self.stream_index = inner.active_video_stream.unwrap_or(0);
        self.codec = inner
            .active_video_codec
            .clone()
            .unwrap_or_else(|| params.codec_id.as_str().to_string());
        self.pixel_format = params.pixel_format.unwrap_or(PixelFormat::Yuv420P);
        self.width = params.width.unwrap_or(0);
        self.height = params.height.unwrap_or(0);

        if let Some(vc) = inner
            .video
            .iter_mut()
            .find(|v| v.stream == self.stream_index)
        {
            vc.codec = self.codec.clone();
            vc.pixel_format = self.pixel_format;
            vc.width = self.width;
            vc.height = self.height;
        } else {
            inner.video.push(VideoCapture {
                stream: self.stream_index,
                codec: self.codec.clone(),
                pixel_format: self.pixel_format,
                width: self.width,
                height: self.height,
                frame_md5: Vec::new(),
                pts: Vec::new(),
                shown_at: Vec::new(),
                flushes: Vec::new(),
            });
        }
        Ok(())
    }

    fn push_frame(&mut self, frame: &VideoFrame, pts: Duration) -> Result<(), SinkError> {
        // Shown on arrival: the engine hands it over when it is due.
        let shown_at = current_monotonic_ns();
        let packed = pack_frame(frame, self.pixel_format, self.width, self.height);
        let md5_str = format!("{:x}", md5::compute(&packed));

        let mut inner = self.inner.lock();
        if let Some(vc) = inner
            .video
            .iter_mut()
            .find(|v| v.stream == self.stream_index)
        {
            vc.frame_md5.push(md5_str);
            vc.pts.push(pts);
            vc.shown_at.push(shown_at);
        }
        Ok(())
    }

    fn frame_lead(&self) -> Duration {
        Duration::ZERO
    }

    /// Declines compressed input: no decoder to drain.
    fn finish(&mut self) -> Result<(), SinkError> {
        Ok(())
    }

    fn flush(&mut self) {
        let mut inner = self.inner.lock();
        if let Some(vc) = inner
            .video
            .iter_mut()
            .find(|v| v.stream == self.stream_index)
        {
            let frames = vc.frame_md5.len();
            vc.flushes.push(frames);
        }
    }

    fn set_playing(&mut self, _playing: bool) {}
}

/// The frame's image planes packed without stride padding, the layout
/// FFmpeg's framemd5 hashes (`av_image_copy_to_buffer`). A `Pal8` frame is
/// followed by its 256-entry palette, 4 bytes per entry: FFmpeg's ARGB word
/// in little-endian order (B, G, R, A). OxideAV palettes carry RGB only, so
/// present entries are opaque and missing ones zero, as in FFmpeg's zeroed
/// palette buffer.
pub fn pack_frame(frame: &VideoFrame, pix_fmt: PixelFormat, width: u32, height: u32) -> Vec<u8> {
    let plane_count = pix_fmt.plane_count();
    let planes = frame.image_planes();
    let mut out = Vec::new();
    for (i, plane) in planes.iter().take(plane_count).enumerate() {
        if let (Some((_pw, ph)), Some(row_bytes)) = (
            pix_fmt.plane_dimensions(i, width, height),
            pix_fmt.plane_row_bytes(i, width),
        ) {
            for row in 0..ph as usize {
                let start = row * plane.stride;
                let end = start + row_bytes;
                if end <= plane.data.len() {
                    out.extend_from_slice(&plane.data[start..end]);
                }
            }
        }
    }
    if pix_fmt.is_palette() {
        let palette = frame.palette().unwrap_or(&[]);
        for entry in 0..256 {
            match palette.get(entry * 3..entry * 3 + 3) {
                Some(rgb) => out.extend_from_slice(&[rgb[2], rgb[1], rgb[0], 0xFF]),
                None => out.extend_from_slice(&[0; 4]),
            }
        }
    }
    out
}

struct HeadlessSubtitleSink {
    inner: Arc<Mutex<HeadlessInner>>,
    clock: Arc<Mutex<Option<Arc<dyn Clock>>>>,
}

impl SubtitleSink for HeadlessSubtitleSink {
    fn show(&mut self, images: &[SubtitleImage], _video_width: u32, _video_height: u32) {
        let time = self
            .clock
            .lock()
            .as_ref()
            .and_then(|clock| clock.now())
            .unwrap_or(Duration::ZERO);
        let mut inner = self.inner.lock();
        let stream = inner.active_subtitle_stream.unwrap_or(0);
        let codec = inner
            .active_subtitle_codec
            .clone()
            .unwrap_or_else(|| "subtitle".into());

        if let Some(sc) = inner.subtitles.iter_mut().find(|s| s.stream == stream) {
            sc.shows.push((time, images.len()));
        } else {
            inner.subtitles.push(SubtitleCapture {
                stream,
                codec,
                shows: vec![(time, images.len())],
            });
        }
    }
}
