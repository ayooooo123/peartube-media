use super::clock::{monotonic_now_ns, AudioClock, AudioClockInner, SendAudioStream};
use crate::backend::{AudioSink, Clock, SinkError};
use ndk::audio::{
    AudioDirection, AudioFormat, AudioPerformanceMode, AudioStreamBuilder, AudioStreamState,
};
use parking_lot::Mutex;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

pub struct AndroidAudioSink {
    clock: AudioClock,
    sample_rate: u32,
    channels: u16,
    is_suspended: Arc<Mutex<bool>>,
}

impl AndroidAudioSink {
    pub fn new() -> (Self, Arc<dyn Clock>) {
        let clock_inner = Arc::new(AudioClockInner::new());
        let clock = AudioClock { inner: clock_inner };
        let clock_dyn: Arc<dyn Clock> = Arc::new(clock.clone());

        let sink = Self {
            clock,
            sample_rate: 48000,
            channels: 2,
            is_suspended: Arc::new(Mutex::new(false)),
        };

        (sink, clock_dyn)
    }

    pub fn suspend(&self) {
        *self.is_suspended.lock() = true;
        let mut stream_lock = self.clock.inner.stream.lock();
        if let Some(stream) = stream_lock.take() {
            let _ = stream.0.request_stop();
            // Dropping SendAudioStream closes AAudioStream
        }
    }

    pub fn resume(&self) -> Result<(), SinkError> {
        *self.is_suspended.lock() = false;
        let sample_rate = self.sample_rate;
        let channels = self.channels;
        self.create_stream(sample_rate, channels)?;

        if self.clock.inner.is_playing.load(Ordering::SeqCst) {
            let stream_opt = self.clock.inner.stream.lock().clone();
            if let Some(stream) = stream_opt {
                let _ = stream.0.request_start();
            }
        }
        Ok(())
    }

    fn create_stream(&self, sample_rate: u32, channels: u16) -> Result<(), SinkError> {
        let builder = AudioStreamBuilder::new().map_err(|e| {
            SinkError::Fatal(format!("failed to create AudioStreamBuilder: {e:?}"))
        })?;

        let builder = builder
            .channel_count(channels as i32)
            .sample_rate(sample_rate as i32)
            .format(AudioFormat::PCM_Float)
            .direction(AudioDirection::Output)
            .performance_mode(AudioPerformanceMode::PowerSaving);

        let stream = builder
            .open_stream()
            .map_err(|e| SinkError::Fatal(format!("failed to open AAudio stream: {e:?}")))?;

        *self.clock.inner.stream.lock() = Some(Arc::new(SendAudioStream(stream)));
        self.clock
            .inner
            .sample_rate
            .store(sample_rate, Ordering::SeqCst);
        self.clock
            .inner
            .channels
            .store(channels as u32, Ordering::SeqCst);

        Ok(())
    }
}

impl AudioSink for AndroidAudioSink {
    fn open(&mut self, sample_rate: u32, channels: u16) -> Result<(), SinkError> {
        self.sample_rate = sample_rate;
        self.channels = channels;
        *self.is_suspended.lock() = false;

        self.create_stream(sample_rate, channels)?;
        self.flush();

        Ok(())
    }

    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError> {
        if *self.is_suspended.lock() {
            return Err(SinkError::Unavailable);
        }

        let stream = match self.clock.inner.stream.lock().clone() {
            Some(s) => s,
            None => return Err(SinkError::Unavailable),
        };

        let channels = self.channels.max(1) as usize;
        let num_frames = pcm.len() / channels;
        if num_frames == 0 {
            return Ok(0);
        }

        // Initialize clock on first write after open/flush
        {
            let mut base_pts_guard = self.clock.inner.base_pts.lock();
            if base_pts_guard.is_none() {
                *base_pts_guard = Some(pts);
                self.clock
                    .inner
                    .start_mono_ns
                    .store(monotonic_now_ns(), Ordering::SeqCst);
                let written_before = stream.0.frames_written();
                self.clock
                    .inner
                    .base_frame_offset
                    .store(written_before, Ordering::SeqCst);
            }
        }

        // Blocking write
        let mut total_written = 0usize;
        let timeout_ns = 1_000_000_000i64; // 1 second timeout per chunk

        while total_written < num_frames {
            let offset_frames = total_written;
            let remaining_frames = (num_frames - total_written) as i32;
            let slice_offset = offset_frames * channels;
            let ptr = unsafe { pcm.as_ptr().add(slice_offset) };

            let written = unsafe { stream.0.write(ptr.cast(), remaining_frames, timeout_ns) }
                .map_err(|e| SinkError::Fatal(format!("AAudioStream write error: {e:?}")))?;

            if written == 0 {
                break;
            }
            total_written += written as usize;
            self.clock
                .inner
                .frames_written
                .fetch_add(written as u64, Ordering::SeqCst);
        }

        Ok(total_written)
    }

    fn play(&mut self) {
        self.clock.inner.is_playing.store(true, Ordering::SeqCst);
        let stream_opt = self.clock.inner.stream.lock().clone();
        if let Some(stream) = stream_opt {
            let _ = stream.0.request_start();
        }
    }

    fn pause(&mut self) {
        self.clock.inner.is_playing.store(false, Ordering::SeqCst);
        self.clock
            .inner
            .pause_mono_ns
            .store(monotonic_now_ns(), Ordering::SeqCst);
        let stream_opt = self.clock.inner.stream.lock().clone();
        if let Some(stream) = stream_opt {
            let _ = stream.0.request_pause();
        }
    }

    fn flush(&mut self) {
        *self.clock.inner.base_pts.lock() = None;
        self.clock.inner.frames_written.store(0, Ordering::SeqCst);
        self.clock.inner.start_mono_ns.store(0, Ordering::SeqCst);
        self.clock
            .inner
            .base_frame_offset
            .store(0, Ordering::SeqCst);

        let stream_opt = self.clock.inner.stream.lock().clone();
        if let Some(stream) = stream_opt {
            let state = stream.0.state();
            if state == AudioStreamState::Started {
                let _ = stream.0.request_pause();
                let _ = stream.0.request_flush();
                if self.clock.inner.is_playing.load(Ordering::SeqCst) {
                    let _ = stream.0.request_start();
                }
            } else {
                let _ = stream.0.request_flush();
            }
        }
    }

    fn clock(&self) -> Arc<dyn Clock> {
        Arc::new(self.clock.clone())
    }
}
