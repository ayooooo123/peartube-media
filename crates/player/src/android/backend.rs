use super::audio::AndroidAudioSink;
use super::subtitle::AndroidSubtitleSink;
use super::video::AndroidVideoSink;
use crate::backend::{
    AudioSink, Backend, Clock, SinkError, SubtitleImage, SubtitleSink, VideoSink,
};
use ndk::native_window::NativeWindow;
use oxideav_core::{CodecParameters, Packet, VideoFrame};
use parking_lot::{Mutex, RwLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

pub struct BackendShared {
    pub video_window: RwLock<Option<NativeWindow>>,
    pub subtitle_window: RwLock<Option<NativeWindow>>,
    pub is_suspended: AtomicBool,
    pub active_audio: Mutex<Option<Weak<Mutex<AndroidAudioSink>>>>,
    pub active_video: Mutex<Option<Weak<Mutex<AndroidVideoSink>>>>,
    pub active_subtitles: Mutex<Option<Weak<Mutex<AndroidSubtitleSink>>>>,
}

pub struct AndroidBackend {
    shared: Arc<BackendShared>,
}

impl AndroidBackend {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(BackendShared {
                video_window: RwLock::new(None),
                subtitle_window: RwLock::new(None),
                is_suspended: AtomicBool::new(false),
                active_audio: Mutex::new(None),
                active_video: Mutex::new(None),
                active_subtitles: Mutex::new(None),
            }),
        })
    }

    /// The backend's shared state, for callers that drive the sinks
    /// directly (the on-device probe).
    pub fn shared(&self) -> &Arc<BackendShared> {
        &self.shared
    }

    /// Sets or clears the video window. When clearing, this blocks until
    /// nothing touches the old window any more: the codec is released or
    /// reconfigured away from it, any software-path lock/post finished, and
    /// the backend's own reference is dropped. Android invalidates the
    /// surface as soon as `SurfaceHolder.Callback.surfaceDestroyed` returns,
    /// so the caller is safe to release it right after this call returns.
    /// A later `set_video_window(Some(new))` reconfigures a compressed stream
    /// on the new window and resumes from the next keyframe.
    pub fn set_video_window(&self, window: Option<NativeWindow>) {
        if window.is_some() {
            *self.shared.video_window.write() = window;
            let weak = self.shared.active_video.lock().clone();
            if let Some(video_sink) = weak.as_ref().and_then(|w| w.upgrade()) {
                video_sink.lock().on_window_available();
            }
        } else {
            // Take the sink's mutex (a push_packet/push_frame or an in-flight
            // open_compressed finishes and no new one starts), publish the
            // loss (so later pushes report Unavailable), then release the
            // codec. teardown joins the output thread and stops the codec,
            // so when this returns nothing can touch the old window and the
            // caller may drop it at once.
            let weak = self.shared.active_video.lock().clone();
            let sink_arc = weak.as_ref().and_then(|w| w.upgrade());
            *self.shared.video_window.write() = None;
            if let Some(video_sink) = sink_arc {
                video_sink.lock().detach_codec_from_window();
            }
        }
    }

    /// Sets or clears the subtitle window. When clearing, this blocks until
    /// no lock/post on the old window is running (the subtitle sink holds it
    /// only inside `show`), so the caller can release the surface as soon as
    /// this returns.
    pub fn set_subtitle_window(&self, window: Option<NativeWindow>) {
        if window.is_some() {
            *self.shared.subtitle_window.write() = window;
        } else {
            let weak = self.shared.active_subtitles.lock().clone();
            let sink_lock = weak.as_ref().and_then(|w| w.upgrade());
            let _guard = sink_lock.as_ref().map(|s| s.lock());
            *self.shared.subtitle_window.write() = None;
        }
    }
}

struct AudioSinkWrapper {
    inner: Arc<Mutex<AndroidAudioSink>>,
}

impl AudioSink for AudioSinkWrapper {
    fn open(&mut self, sample_rate: u32, channels: u16) -> Result<(), SinkError> {
        self.inner.lock().open(sample_rate, channels)
    }

    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError> {
        self.inner.lock().write(pcm, pts)
    }

    fn play(&mut self) {
        self.inner.lock().play()
    }

    fn pause(&mut self) {
        self.inner.lock().pause()
    }

    fn flush(&mut self) {
        self.inner.lock().flush()
    }

    fn clock(&self) -> Arc<dyn Clock> {
        self.inner.lock().clock()
    }
}

struct VideoSinkWrapper {
    inner: Arc<Mutex<AndroidVideoSink>>,
}

impl VideoSink for VideoSinkWrapper {
    fn open_compressed(&mut self, params: &CodecParameters) -> bool {
        self.inner.lock().open_compressed(params)
    }

    fn push_packet(&mut self, packet: &Packet, pts: Duration) -> Result<(), SinkError> {
        self.inner.lock().push_packet(packet, pts)
    }

    fn open_frames(&mut self, params: &CodecParameters) -> Result<(), SinkError> {
        self.inner.lock().open_frames(params)
    }

    fn push_frame(&mut self, frame: &VideoFrame, pts: Duration) -> Result<(), SinkError> {
        self.inner.lock().push_frame(frame, pts)
    }

    fn flush(&mut self) {
        self.inner.lock().flush()
    }

    fn set_playing(&mut self, playing: bool) {
        self.inner.lock().set_playing(playing)
    }
}

struct SubtitleSinkWrapper {
    inner: Arc<Mutex<AndroidSubtitleSink>>,
}

impl SubtitleSink for SubtitleSinkWrapper {
    fn show(&mut self, images: &[SubtitleImage], video_width: u32, video_height: u32) {
        self.inner.lock().show(images, video_width, video_height)
    }
}

impl Backend for AndroidBackend {
    fn audio(&self) -> Box<dyn AudioSink> {
        let (sink, _clock) = AndroidAudioSink::new();
        let sink_arc = Arc::new(Mutex::new(sink));
        *self.shared.active_audio.lock() = Some(Arc::downgrade(&sink_arc));
        Box::new(AudioSinkWrapper { inner: sink_arc })
    }

    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        let sink = AndroidVideoSink::new(self.shared.clone(), clock);
        let sink_arc = Arc::new(Mutex::new(sink));
        *self.shared.active_video.lock() = Some(Arc::downgrade(&sink_arc));
        Box::new(VideoSinkWrapper { inner: sink_arc })
    }

    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        let sink = AndroidSubtitleSink::new(self.shared.clone());
        let sink_arc = Arc::new(Mutex::new(sink));
        *self.shared.active_subtitles.lock() = Some(Arc::downgrade(&sink_arc));
        Box::new(SubtitleSinkWrapper { inner: sink_arc })
    }

    fn suspend(&self) {
        self.shared.is_suspended.store(true, Ordering::SeqCst);
        if let Some(weak) = self.shared.active_audio.lock().as_ref() {
            if let Some(audio_sink) = weak.upgrade() {
                audio_sink.lock().suspend();
            }
        }
        if let Some(weak) = self.shared.active_video.lock().as_ref() {
            if let Some(video_sink) = weak.upgrade() {
                video_sink.lock().suspend();
            }
        }
    }

    fn resume(&self) {
        self.shared.is_suspended.store(false, Ordering::SeqCst);
        if let Some(weak) = self.shared.active_audio.lock().as_ref() {
            if let Some(audio_sink) = weak.upgrade() {
                let _ = audio_sink.lock().resume();
            }
        }
        if let Some(weak) = self.shared.active_video.lock().as_ref() {
            if let Some(video_sink) = weak.upgrade() {
                video_sink.lock().resume();
            }
        }
    }
}
