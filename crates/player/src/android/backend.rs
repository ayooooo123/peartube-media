use super::audio::AndroidAudioSink;
use super::subtitle::AndroidSubtitleSink;
use super::surface::{
    BackendRetireObserver, SurfaceBindError, SurfaceBinding, SurfaceBindingLease, SurfaceId,
    SurfaceRegistry,
};
use super::video::AndroidVideoSink;
use crate::backend::{
    AudioSink, Backend, Clock, ProducerId, SubtitleImage, SubtitleSink, VideoError, VideoMode,
    VideoOutput, VideoRequest, VideoSink,
};
#[cfg(target_os = "android")]
use ndk::native_window::NativeWindow;
use oxideav_core::{Packet, VideoFrame};
use parking_lot::{Mutex, RwLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::task::Poll;
use std::time::Duration;

pub struct BackendShared {
    pub output_revision: AtomicU64,
    pub output_available: AtomicBool,
    pub video_surface: RwLock<Option<Arc<SurfaceBinding>>>,
    pub subtitle_surface: RwLock<Option<Arc<SurfaceBinding>>>,
    pub is_suspended: AtomicBool,
    pub active_audio: Mutex<Option<Weak<Mutex<AndroidAudioSink>>>>,
    pub active_video: Mutex<Option<Weak<Mutex<AndroidVideoSink>>>>,
    pub active_subtitles: Mutex<Option<Weak<Mutex<AndroidSubtitleSink>>>>,
}

impl BackendRetireObserver for BackendShared {
    fn on_surface_retired(&self, id: SurfaceId) {
        let mut invalidated_video = false;
        {
            let mut v_guard = self.video_surface.write();
            if let Some(b) = v_guard.as_ref() {
                if b.id() == id {
                    *v_guard = None;
                    self.output_available.store(false, Ordering::SeqCst);
                    self.output_revision.fetch_add(1, Ordering::SeqCst);
                    invalidated_video = true;
                }
            }
        }
        if invalidated_video {
            let weak = self.active_video.lock().clone();
            if let Some(video_sink) = weak.as_ref().and_then(|w| w.upgrade()) {
                video_sink.lock().on_output_invalidated();
            }
        }

        let mut invalidated_subs = false;
        {
            let mut s_guard = self.subtitle_surface.write();
            if let Some(b) = s_guard.as_ref() {
                if b.id() == id {
                    *s_guard = None;
                    invalidated_subs = true;
                }
            }
        }
        if invalidated_subs {
            let weak = self.active_subtitles.lock().clone();
            if let Some(sub_sink) = weak.as_ref().and_then(|w| w.upgrade()) {
                sub_sink.lock().on_surface_lost();
            }
        }
    }
}

impl BackendShared {
    pub fn video_output(&self) -> VideoOutput {
        let suspended = self.is_suspended.load(Ordering::SeqCst);
        let has = self.video_surface.read().is_some();
        VideoOutput {
            revision: self.output_revision.load(Ordering::SeqCst),
            available: has && !suspended && self.output_available.load(Ordering::SeqCst),
        }
    }

    pub fn video_surface_binding(&self) -> Option<Arc<SurfaceBinding>> {
        self.video_surface.read().clone()
    }

    pub fn subtitle_surface_binding(&self) -> Option<Arc<SurfaceBinding>> {
        self.subtitle_surface.read().clone()
    }

    pub fn current_surface_id(&self) -> Option<SurfaceId> {
        self.video_surface.read().as_ref().map(|b| b.id())
    }

    pub fn is_suspended(&self) -> bool {
        self.is_suspended.load(Ordering::SeqCst)
    }

    /// Owner/lease-bound import. No frontend path; short handle locks only.
    #[cfg(target_os = "android")]
    pub(crate) fn import_window_for_lease(
        &self,
        lease: &SurfaceBindingLease,
    ) -> Result<NativeWindow, String> {
        lease.ensure_window()
    }
}

pub struct AndroidBackend {
    shared: Arc<BackendShared>,
}

impl AndroidBackend {
    pub fn new() -> Arc<Self> {
        let shared = Arc::new(BackendShared {
            output_revision: AtomicU64::new(1),
            output_available: AtomicBool::new(false),
            video_surface: RwLock::new(None),
            subtitle_surface: RwLock::new(None),
            is_suspended: AtomicBool::new(false),
            active_audio: Mutex::new(None),
            active_video: Mutex::new(None),
            active_subtitles: Mutex::new(None),
        });

        SurfaceRegistry::global()
            .register_observer(Arc::downgrade(&shared) as Weak<dyn BackendRetireObserver>);

        Arc::new(Self { shared })
    }

    pub fn shared(&self) -> &Arc<BackendShared> {
        &self.shared
    }

    pub fn set_video_surface(
        &self,
        surface: Arc<SurfaceBinding>,
    ) -> Result<VideoOutput, SurfaceBindError> {
        // Serialize phase admission with publication under the surface write lock.
        let mut guard = self.shared.video_surface.write();
        surface.admit_for_publish()?;
        let replaced = guard.as_ref().map(|b| b.id()) != Some(surface.id());
        *guard = Some(surface);
        let suspended = self.shared.is_suspended.load(Ordering::SeqCst);
        self.shared
            .output_available
            .store(!suspended, Ordering::SeqCst);
        let rev = if replaced {
            self.shared.output_revision.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            self.shared.output_revision.load(Ordering::SeqCst)
        };
        drop(guard);
        self.wake_active_video();
        Ok(VideoOutput {
            revision: rev,
            available: !suspended,
        })
    }

    pub fn clear_video_surface(&self) -> VideoOutput {
        *self.shared.video_surface.write() = None;
        self.shared.output_available.store(false, Ordering::SeqCst);
        let rev = self.shared.output_revision.fetch_add(1, Ordering::SeqCst) + 1;
        self.wake_active_video();
        VideoOutput {
            revision: rev,
            available: false,
        }
    }

    pub fn set_subtitle_surface(
        &self,
        surface: Arc<SurfaceBinding>,
    ) -> Result<(), SurfaceBindError> {
        let mut guard = self.shared.subtitle_surface.write();
        surface.admit_for_publish()?;
        let changed = guard.as_ref().map(|b| b.id()) != Some(surface.id());
        *guard = Some(surface);
        drop(guard);
        if changed {
            self.wake_active_subtitles_changed();
        }
        Ok(())
    }

    pub fn clear_subtitle_surface(&self) {
        *self.shared.subtitle_surface.write() = None;
        self.wake_active_subtitles_changed();
    }

    pub fn video_output(&self) -> VideoOutput {
        self.shared.video_output()
    }

    fn wake_active_video(&self) {
        let weak = self.shared.active_video.lock().clone();
        if let Some(video_sink) = weak.as_ref().and_then(|w| w.upgrade()) {
            video_sink.lock().on_output_invalidated();
        }
    }

    fn wake_active_subtitles_changed(&self) {
        let weak = self.shared.active_subtitles.lock().clone();
        if let Some(sub_sink) = weak.as_ref().and_then(|w| w.upgrade()) {
            sub_sink.lock().on_surface_changed();
        }
    }
}

struct AudioSinkWrapper {
    inner: Arc<Mutex<AndroidAudioSink>>,
}

impl AudioSink for AudioSinkWrapper {
    fn open(&mut self, sample_rate: u32, channels: u16) -> Result<(), crate::backend::SinkError> {
        self.inner.lock().open(sample_rate, channels)
    }

    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, crate::backend::SinkError> {
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
    fn output(&self) -> VideoOutput {
        self.inner.lock().output()
    }

    fn poll_transition(
        &mut self,
        request: &VideoRequest,
    ) -> Poll<Result<VideoMode, VideoError>> {
        self.inner.lock().poll_transition(request)
    }

    fn push_packet(
        &mut self,
        producer: ProducerId,
        packet: &mut Option<Packet>,
        pts: Duration,
        random_access: bool,
    ) -> Result<(), VideoError> {
        self.inner
            .lock()
            .push_packet(producer, packet, pts, random_access)
    }

    fn push_frame(
        &mut self,
        producer: ProducerId,
        frame: &mut Option<VideoFrame>,
        pts: Duration,
    ) -> Result<(), VideoError> {
        self.inner.lock().push_frame(producer, frame, pts)
    }

    fn present_from(&mut self, producer: ProducerId, start: Duration) -> Result<(), VideoError> {
        self.inner.lock().present_from(producer, start)
    }

    fn set_playing(&mut self, producer: ProducerId, playing: bool) -> Result<(), VideoError> {
        self.inner.lock().set_playing(producer, playing)
    }

    fn frame_lead(&self) -> Duration {
        self.inner.lock().frame_lead()
    }

    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        self.inner.lock().poll_finish(producer)
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
        if self.shared.is_suspended.load(Ordering::SeqCst) {
            sink_arc.lock().suspend();
        }
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
        self.shared.output_available.store(false, Ordering::SeqCst);
        self.shared.output_revision.fetch_add(1, Ordering::SeqCst);

        if let Some(weak) = self.shared.active_audio.lock().as_ref() {
            if let Some(audio_sink) = weak.upgrade() {
                audio_sink.lock().suspend();
            }
        }
        if let Some(weak) = self.shared.active_video.lock().as_ref() {
            if let Some(video_sink) = weak.upgrade() {
                video_sink.lock().on_output_invalidated();
            }
        }
        if let Some(weak) = self.shared.active_subtitles.lock().as_ref() {
            if let Some(sub_sink) = weak.upgrade() {
                sub_sink.lock().on_surface_lost();
            }
        }
    }

    fn resume(&self) {
        self.shared.is_suspended.store(false, Ordering::SeqCst);
        let has_surface = self.shared.video_surface.read().is_some();
        self.shared
            .output_available
            .store(has_surface, Ordering::SeqCst);
        self.shared.output_revision.fetch_add(1, Ordering::SeqCst);

        if let Some(weak) = self.shared.active_audio.lock().as_ref() {
            if let Some(audio_sink) = weak.upgrade() {
                let _ = audio_sink.lock().resume();
            }
        }
        if let Some(weak) = self.shared.active_video.lock().as_ref() {
            if let Some(video_sink) = weak.upgrade() {
                video_sink.lock().on_output_invalidated();
            }
        }
        if let Some(weak) = self.shared.active_subtitles.lock().as_ref() {
            if let Some(sub_sink) = weak.upgrade() {
                sub_sink.lock().on_surface_changed();
            }
        }
    }
}
