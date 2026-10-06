//! Audio output: f32-interleaved LPCM `CMSampleBuffer`s enqueued on an
//! `AVSampleBufferAudioRenderer` driven by the playback's
//! `AVSampleBufferRenderSynchronizer`.

use std::ptr::{self, NonNull};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::rc::Retained;
use objc2_av_foundation::{AVSampleBufferAudioRenderer, AVSampleBufferRenderSynchronizer};
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescription, CMItemCount, CMTime, CMTimeFlags, CMSampleBuffer,
    CMSampleTimingInfo,
};
use crate::apple::clock::AppleClock;
use crate::apple::util::{create_block_buffer_from_bytes, SendSync};
use crate::backend::{AudioSink, Clock, SinkError};

/// `requestMediaDataWhenReady` blocks the engine's audio thread in
/// `write` until the renderer pulls; this callback is what wakes it.
type ReadyFlag = Arc<(Mutex<bool>, Condvar)>;

pub struct AppleAudioSink {
    synchronizer: SendSync<Retained<AVSampleBufferRenderSynchronizer>>,
    audio_renderer: SendSync<Retained<AVSampleBufferAudioRenderer>>,
    clock: Arc<AppleClock>,
    sample_rate: u32,
    channels: u16,
    format_desc: Option<CFRetained<CMAudioFormatDescription>>,
    ready: ReadyFlag,
    requesting: std::cell::Cell<bool>,
    /// Set once the timebase has been anchored (first `write` after open
    /// or flush); `play`/`pause` before that only record the intent.
    anchored: bool,
    playing: bool,
}

// SAFETY: AVSampleBufferAudioRenderer is documented thread-safe (enqueue
// from any thread); the synchronizer's setRate/timebase calls are too.
// Shared state lives behind the mutex/condvar.
unsafe impl Send for AppleAudioSink {}
unsafe impl Sync for AppleAudioSink {}

impl AppleAudioSink {
    pub fn new(
        synchronizer: Retained<AVSampleBufferRenderSynchronizer>,
        audio_renderer: Retained<AVSampleBufferAudioRenderer>,
        clock: Arc<AppleClock>,
    ) -> Self {
        Self {
            synchronizer: SendSync(synchronizer),
            audio_renderer: SendSync(audio_renderer),
            clock,
            sample_rate: 0,
            channels: 0,
            format_desc: None,
            ready: Arc::new((Mutex::new(true), Condvar::new())),
            requesting: std::cell::Cell::new(false),
            anchored: false,
            playing: false,
        }
    }

    fn wait_ready(&self) {
        // Fast path: renderer is accepting data right now.
        let ready_now = unsafe {
            let renderer: &AVSampleBufferAudioRenderer = &self.audio_renderer;
            objc2::msg_send![renderer, isReadyForMoreMediaData]
        };
        if ready_now {
            return;
        }
        // Register the pull callback once; it fires on `queue` whenever the
        // renderer becomes ready again.
        let pair = self.ready.clone();
        let was_requesting = self.requesting.get();
        if !was_requesting {
            let block = RcBlock::new(move || {
                let (lock, cvar) = &*pair;
                let mut ready = lock.lock().expect("ready lock");
                *ready = true;
                cvar.notify_all();
            });
            let queue = DispatchQueue::new("peartube.audio.ready", DispatchQueueAttr::SERIAL);
            unsafe {
                let renderer: &AVSampleBufferAudioRenderer = &self.audio_renderer;
                let _: () = objc2::msg_send![renderer, requestMediaDataWhenReadyOnQueue: &*queue, usingBlock: &*block];
            }
            self.requesting.set(true);
        }
        // Block `write` (the contract: "Blocks while the output's buffer is
        // full") until ready. The condition is re-checked against the
        // renderer after every wake: requestMediaDataWhenReady fires the
        // block repeatedly while ready, so a stale flag cannot hang us.
        loop {
            let ready_now = unsafe {
            let renderer: &AVSampleBufferAudioRenderer = &self.audio_renderer;
            objc2::msg_send![renderer, isReadyForMoreMediaData]
        };
            if ready_now {
                return;
            }
            let (lock, cvar) = &*self.ready;
            let guard = lock.lock().expect("ready lock");
            let (guard, _timeout) = cvar
                .wait_timeout(guard, Duration::from_millis(20))
                .expect("ready condvar");
            drop(guard);
        }
    }
}

impl AudioSink for AppleAudioSink {
    fn open(&mut self, sample_rate: u32, channels: u16) -> Result<(), SinkError> {
        if sample_rate == 0 || channels == 0 {
            return Err(SinkError::Fatal(format!(
                "invalid audio format: {sample_rate} Hz, {channels} channels"
            )));
        }
        self.sample_rate = sample_rate;
        self.channels = channels;
        let bytes_per_frame = channels as u32 * 4;
        let mut asbd = AudioStreamBasicDescription {
            mSampleRate: sample_rate as f64,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: bytes_per_frame,
            mFramesPerPacket: 1,
            mBytesPerFrame: bytes_per_frame,
            mChannelsPerFrame: channels as u32,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let mut raw: *const CMAudioFormatDescription = ptr::null();
        let status = unsafe {
            objc2_core_media::CMAudioFormatDescriptionCreate(
                None,
                NonNull::from(&mut asbd),
                0,
                ptr::null(),
                0,
                ptr::null(),
                None,
                NonNull::from(&mut raw),
            )
        };
        if status != 0 || raw.is_null() {
            return Err(SinkError::Fatal(format!(
                "CMAudioFormatDescriptionCreate: {status}"
            )));
        }
        // SAFETY: Create-rule function returned +1.
        self.format_desc = Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw as *mut _)) });
        Ok(())
    }

    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError> {
        let format_desc = self
            .format_desc
            .as_ref()
            .ok_or_else(|| SinkError::Fatal("audio sink not open".into()))?;
        let channels = self.channels as usize;
        if channels == 0 {
            return Err(SinkError::Fatal("audio sink opened with 0 channels".into()));
        }
        let frames = pcm.len() / channels;
        if frames == 0 {
            return Ok(0);
        }

        self.wait_ready();

        let pcm_bytes =
            unsafe { std::slice::from_raw_parts(pcm.as_ptr().cast::<u8>(), pcm.len() * 4) };
        let block = unsafe { create_block_buffer_from_bytes(pcm_bytes)? };

        let pts_cm = CMTime {
            value: (pts.as_secs_f64() * self.sample_rate as f64).round() as i64,
            timescale: self.sample_rate as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let timing = CMSampleTimingInfo {
            duration: CMTime {
                value: 1,
                timescale: self.sample_rate as i32,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            },
            presentationTimeStamp: pts_cm,
            decodeTimeStamp: unsafe { objc2_core_media::kCMTimeInvalid },
        };

        let mut raw: *mut CMSampleBuffer = ptr::null_mut();
        let sample_size = channels * 4;
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(&*format_desc),
                frames as CMItemCount,
                1,
                &timing,
                1,
                &sample_size,
                NonNull::from(&mut raw),
            )
        };
        if status != 0 || raw.is_null() {
            return Err(SinkError::Fatal(format!(
                "CMSampleBuffer::create_ready (audio): {status}"
            )));
        }
        // SAFETY: Create-rule function returned +1; the sample buffer holds
        // its own retain on `block` and `format_desc`.
        let sample = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };

        unsafe {
            let renderer: &AVSampleBufferAudioRenderer = &self.audio_renderer;
            let _: () = objc2::msg_send![renderer, enqueueSampleBuffer: &*sample];
        }

        if !self.anchored {
            self.anchored = true;
            let rate = if self.playing { 1.0 } else { 0.0 };
            unsafe {
                let sync: &AVSampleBufferRenderSynchronizer = &self.synchronizer;
                let _: () = objc2::msg_send![sync, setRate: rate as f32, time: pts_cm];
            }
        }
        Ok(frames)
    }

    fn play(&mut self) {
        self.playing = true;
        if self.anchored {
            unsafe {
                let sync: &AVSampleBufferRenderSynchronizer = &self.synchronizer;
                let _: () = objc2::msg_send![sync, setRate: 1.0f32];
            }
        }
    }

    fn pause(&mut self) {
        self.playing = false;
        unsafe {
            let sync: &AVSampleBufferRenderSynchronizer = &self.synchronizer;
            let _: () = objc2::msg_send![sync, setRate: 0.0f32];
        }
    }

    fn flush(&mut self) {
        // The plain `-flush` lives on the AVQueuedSampleBufferRendering
        // protocol; the renderer also inherits it via that conformance.
        unsafe {
            let renderer: &AVSampleBufferAudioRenderer = &self.audio_renderer;
            let _: () = objc2::msg_send![renderer, flush];
        }
        self.anchored = false;
        let (lock, cvar) = &*self.ready;
        *lock.lock().expect("ready lock") = true;
        cvar.notify_all();
    }

    fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }
}

impl Drop for AppleAudioSink {
    fn drop(&mut self) {
        if self.requesting.get() {
            unsafe {
                let renderer: &AVSampleBufferAudioRenderer = &self.audio_renderer;
                let _: () = objc2::msg_send![renderer, stopRequestingMediaData];
            }
            self.requesting.set(false);
        }
    }
}
