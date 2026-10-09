use super::clock::{AudioClock, AudioClockState, SendAudioStream};
use crate::backend::{AudioSink, Clock, SinkError};
use crate::clock::current_monotonic_ns;
use ndk::audio::{AudioDirection, AudioFormat, AudioPerformanceMode, AudioStreamBuilder};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

pub struct AndroidAudioSink {
    clock: AudioClock,
    sample_rate: u32,
    channels: u16,
    suspended: bool,
}

impl AndroidAudioSink {
    pub fn new() -> (Self, Arc<dyn Clock>) {
        let clock = AudioClock { inner: Arc::new(Mutex::new(AudioClockState::new())) };
        let clock_dyn = Arc::new(clock.clone());
        (Self { clock, sample_rate: 0, channels: 0, suspended: false }, clock_dyn)
    }

    pub fn suspend(&mut self) {
        // Freeze the clock without first starting an asynchronous pause:
        // a stop requested during that transition can be rejected.
        let old = {
            let mut state = self.clock.inner.lock();
            if state.playing { state.observe(); }
            state.playing = false;
            state.stream.take()
        };
        self.suspended = true;
        if let Some(stream) = old { let _ = stream.0.request_stop(); }
    }

    pub fn resume(&mut self) -> Result<(), SinkError> {
        self.suspended = false;
        if self.sample_rate != 0 { self.create_stream(self.sample_rate, self.channels)?; }
        Ok(())
    }

    fn create_stream(&self, rate: u32, channels: u16) -> Result<(), SinkError> {
        let stream = AudioStreamBuilder::new()
            .map_err(|e| SinkError::Fatal(format!("AAudio builder: {e:?}")))?
            .channel_count(i32::from(channels)).sample_rate(rate as i32)
            .format(AudioFormat::PCM_Float).direction(AudioDirection::Output)
            .performance_mode(AudioPerformanceMode::LowLatency)
            .open_stream().map_err(|e| SinkError::Fatal(format!("AAudio open: {e:?}")))?;
        let old = {
            let mut state = self.clock.inner.lock();
            let old = state.stream.take();
            *state = AudioClockState::new();
            state.rate = rate;
            state.stream = Some(Arc::new(SendAudioStream(stream)));
            old
        };
        if let Some(stream) = old { let _ = stream.0.request_stop(); }
        Ok(())
    }
}

impl Drop for AndroidAudioSink {
    fn drop(&mut self) {
        // Video may retain the clock after the audio lane exits.
        self.suspend();
    }
}

impl AudioSink for AndroidAudioSink {
    fn open(&mut self, sample_rate: u32, channels: u16) -> Result<(), SinkError> {
        if sample_rate == 0 || sample_rate > i32::MAX as u32 || channels == 0 || channels > 64 {
            return Err(SinkError::Fatal(format!("invalid audio format {sample_rate} Hz / {channels} channels")));
        }
        // Suspended (app in the background): keep the format and let
        // `resume` create the stream. Opening here would restart audio in
        // the background; writes report `Unavailable` until then.
        if !self.suspended {
            self.create_stream(sample_rate, channels)?;
        }
        self.sample_rate = sample_rate;
        self.channels = channels;
        Ok(())
    }

    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError> {
        if self.suspended { return Err(SinkError::Unavailable); }
        let (stream, playing) = {
            let state = self.clock.inner.lock();
            (state.stream.clone().ok_or(SinkError::Unavailable)?, state.playing)
        };
        let frames = pcm.len() / usize::from(self.channels);
        if frames == 0 { return Ok(0); }
        let before = stream.0.frames_written();
        // One bounded write lets the engine react to a hold/seek/drop even
        // if the device is full. Paused preroll is strictly non-blocking.
        let timeout = if playing { 20_000_000 } else { 0 };
        let result = unsafe { stream.0.write(pcm.as_ptr().cast(), frames.min(i32::MAX as usize) as i32, timeout) };
        // ndk 0.9 misclassifies positive AAudio frame counts as errors.
        let written = match result {
            Ok(n) => n as usize,
            Err(ndk::audio::AudioError::__Unknown(n)) if n > 0 => n as usize,
            Err(e) => return Err(SinkError::Fatal(format!("AAudio write: {e:?}"))),
        };
        let mut state = self.clock.inner.lock();
        if written > 0 && state.base.is_none() {
            state.base = Some(pts);
            state.base_frame = before;
            state.started_ns = current_monotonic_ns();
        }
        state.written += written as u64;
        #[cfg(debug_assertions)]
        if super::clock::tracing() {
            if let Ok(ts) = stream.0.timestamp(ndk::audio::Clockid::Monotonic) {
                eprintln!("ENGINE_SYNC audio base_pts_ns={} base_frame={} written={} frame={} mono_ns={} rate={}",
                    state.base.unwrap_or_default().as_nanos(), state.base_frame, state.written,
                    ts.frame_position, ts.time_nanoseconds, state.rate);
            }
        }
        Ok(written)
    }

    fn play(&mut self) {
        // Every master-clock reader takes the clock state lock: update the
        // state under it, make the platform call after releasing it.
        let stream = {
            let mut state = self.clock.inner.lock();
            if state.playing { return; }
            state.started_ns = current_monotonic_ns();
            state.playing = true;
            state.stream.clone()
        };
        if let Some(stream) = stream { let _ = stream.0.request_start(); }
    }

    fn pause(&mut self) {
        let stream = {
            let mut state = self.clock.inner.lock();
            if !state.playing { return; }
            state.observe();
            state.playing = false;
            state.stream.clone()
        };
        if let Some(stream) = stream { let _ = stream.0.request_pause(); }
    }

    fn flush(&mut self) {
        // Recreating makes seek atomic with respect to AAudio's async state
        // transitions; an old timestamp can never anchor the new samples.
        if self.sample_rate != 0 && !self.suspended {
            let playing = self.clock.inner.lock().playing;
            if self.create_stream(self.sample_rate, self.channels).is_ok() && playing { self.play(); }
        }
    }

    fn clock(&self) -> Arc<dyn Clock> { Arc::new(self.clock.clone()) }
}
