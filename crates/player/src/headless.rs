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
    clock: Arc<HeadlessClock>,
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
}

#[derive(Clone, Debug)]
pub struct AudioCapture {
    pub stream: u32,
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub pcm: Vec<f32>,
}

#[derive(Clone, Debug)]
pub struct SubtitleCapture {
    pub stream: u32,
    pub codec: String,
    pub shows: Vec<(Duration, usize)>,
}

struct HeadlessInner {
    realtime: bool,
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

pub struct HeadlessClock {
    state: Mutex<HeadlessClockState>,
}

struct HeadlessClockState {
    realtime: bool,
    /// Without realtime: the end of the audio written so far.
    now: Option<Duration>,
    /// Realtime: media time at `run_start_ns` while playing, or where the
    /// clock stands while paused; set by the first write after a flush.
    base: Option<Duration>,
    /// Realtime: CLOCK_MONOTONIC when the clock last started running.
    run_start_ns: Option<i64>,
    /// The sink's `play` / `pause`: like an audio device, the clock only
    /// advances while the output plays.
    playing: bool,
}

impl Default for HeadlessClock {
    fn default() -> Self {
        Self::new()
    }
}

impl HeadlessClock {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(HeadlessClockState {
                realtime: false,
                now: None,
                base: None,
                run_start_ns: None,
                playing: true,
            }),
        }
    }

    pub fn set_realtime(&self, rt: bool) {
        self.state.lock().realtime = rt;
    }

    pub fn set_now(&self, pts: Duration) {
        let mut st = self.state.lock();
        st.now = Some(pts);
    }

    pub fn on_audio_write(&self, pts: Duration) {
        let mut st = self.state.lock();
        if st.realtime && st.base.is_none() {
            st.base = Some(pts);
            if st.playing {
                st.run_start_ns = Some(current_monotonic_ns());
            }
        }
        st.now = Some(pts);
    }

    /// Starts or stops the realtime clock with the output.
    pub fn set_playing(&self, playing: bool) {
        let mut st = self.state.lock();
        if st.playing == playing {
            return;
        }
        st.playing = playing;
        if playing {
            if st.base.is_some() {
                st.run_start_ns = Some(current_monotonic_ns());
            }
        } else if let (Some(base), Some(start_ns)) = (st.base, st.run_start_ns.take()) {
            let elapsed_ns = (current_monotonic_ns() - start_ns).max(0) as u64;
            st.base = Some(base + Duration::from_nanos(elapsed_ns));
        }
    }

    pub fn flush(&self) {
        let mut st = self.state.lock();
        st.now = None;
        st.base = None;
        st.run_start_ns = None;
    }
}

impl Clock for HeadlessClock {
    fn now(&self) -> Option<Duration> {
        let st = self.state.lock();
        match (st.realtime, st.base) {
            (true, Some(base)) => Some(match st.run_start_ns {
                Some(start_ns) => {
                    let elapsed_ns = (current_monotonic_ns() - start_ns).max(0) as u64;
                    base + Duration::from_nanos(elapsed_ns)
                }
                None => base,
            }),
            _ => st.now,
        }
    }

    fn monotonic_ns_at(&self, at: Duration) -> Option<i64> {
        let st = self.state.lock();
        if let (Some(start_ns), Some(base)) = (st.run_start_ns, st.base) {
            let at_ns = at.as_nanos() as i64;
            let base_ns = base.as_nanos() as i64;
            Some(start_ns + (at_ns - base_ns))
        } else {
            Some(current_monotonic_ns())
        }
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
        let clock = Arc::new(HeadlessClock::new());
        let this = Arc::new(Headless {
            inner: Arc::new(Mutex::new(HeadlessInner {
                realtime: true,
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
            clock,
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
        self.clock.set_realtime(realtime);
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
        Box::new(HeadlessAudioSink {
            inner: Arc::clone(&self.inner),
            clock: Arc::clone(&self.clock),
            sample_rate: 0,
            channels: 0,
        })
    }

    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        Box::new(HeadlessVideoSink {
            inner: Arc::clone(&self.inner),
            clock,
            headless_clock: Arc::clone(&self.clock),
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
    clock: Arc<HeadlessClock>,
    sample_rate: u32,
    channels: u16,
}

impl AudioSink for HeadlessAudioSink {
    fn open(&mut self, sample_rate: u32, channels: u16) -> Result<(), SinkError> {
        self.sample_rate = sample_rate;
        self.channels = channels;
        let mut inner = self.inner.lock();
        let stream = inner.active_audio_stream.unwrap_or(0);
        let codec = inner
            .active_audio_codec
            .clone()
            .unwrap_or_else(|| "audio".into());

        if let Some(ac) = inner.audio.iter_mut().find(|a| a.stream == stream) {
            ac.sample_rate = sample_rate;
            ac.channels = channels;
        } else {
            inner.audio.push(AudioCapture {
                stream,
                codec,
                sample_rate,
                channels,
                pcm: Vec::new(),
            });
        }
        Ok(())
    }

    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError> {
        if self.channels == 0 || self.sample_rate == 0 {
            return Ok(0);
        }
        let frames = pcm.len() / (self.channels as usize);
        let dur = Duration::from_secs_f64(frames as f64 / self.sample_rate as f64);

        {
            let mut inner = self.inner.lock();
            let stream = inner.active_audio_stream.unwrap_or(0);
            if let Some(ac) = inner.audio.iter_mut().find(|a| a.stream == stream) {
                ac.pcm.extend_from_slice(pcm);
            }
        }

        self.clock.on_audio_write(pts);
        if !self.inner.lock().realtime {
            self.clock.set_now(pts + dur);
        } else {
            // Realtime pacing: do not buffer more than 100 ms ahead of the clock
            if let Some(now) = self.clock.now() {
                if pts + dur > now + Duration::from_millis(100) {
                    let sleep_time = (pts + dur) - (now + Duration::from_millis(100));
                    std::thread::sleep(sleep_time);
                }
            }
        }

        Ok(frames)
    }

    fn play(&mut self) {
        self.clock.set_playing(true);
    }

    fn pause(&mut self) {
        self.clock.set_playing(false);
    }

    fn flush(&mut self) {
        self.clock.flush();
    }

    fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }
}

struct HeadlessVideoSink {
    inner: Arc<Mutex<HeadlessInner>>,
    clock: Arc<dyn Clock>,
    headless_clock: Arc<HeadlessClock>,
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

    fn push_packet(&mut self, _packet: &Packet, _pts: Duration) -> Result<(), SinkError> {
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
            });
        }
        Ok(())
    }

    fn push_frame(&mut self, frame: &VideoFrame, pts: Duration) -> Result<(), SinkError> {
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
        }

        if !inner.realtime && self.clock.now().is_none() {
            self.headless_clock.set_now(pts);
        }
        Ok(())
    }

    fn flush(&mut self) {}

    fn set_playing(&mut self, _playing: bool) {}
}

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
    out
}

struct HeadlessSubtitleSink {
    inner: Arc<Mutex<HeadlessInner>>,
    clock: Arc<HeadlessClock>,
}

impl SubtitleSink for HeadlessSubtitleSink {
    fn show(&mut self, images: &[SubtitleImage], _video_width: u32, _video_height: u32) {
        let mut inner = self.inner.lock();
        let stream = inner.active_subtitle_stream.unwrap_or(0);
        let codec = inner
            .active_subtitle_codec
            .clone()
            .unwrap_or_else(|| "subtitle".into());
        let time = self.clock.now().unwrap_or(Duration::ZERO);

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
