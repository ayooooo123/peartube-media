//! Audio output: f32-interleaved LPCM `CMSampleBuffer`s coalesced and enqueued
//! on an `AVSampleBufferAudioRenderer` driven by the playback's
//! `AVSampleBufferRenderSynchronizer`.
//!
//! Lossless codecs like Dolby TrueHD/MLP drip 40-sample (0.83 ms @ 48 kHz)
//! access units — 1,200 tiny writes a second. Stuffing those raw into CoreMedia
//! starves `FigAudioQueue`'s 4-buffer ring below the 11.6 ms speaker callback
//! and triggers a nonstop `AudioQueueFlush` choke. We hold and coalesce frames
//! until each `CMSampleBuffer` carries at least 1,024 samples and >= 25 ms,
//! stamping presentation times from an exact integer sample counter so every
//! buffer kisses the next without container-rounding clicks.

use std::ptr::{self, NonNull};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_av_foundation::{
    AVSampleBufferAudioRenderer, AVSampleBufferAudioRendererWasFlushedAutomaticallyNotification,
    AVSampleBufferRenderSynchronizer,
};
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescription, CMItemCount, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol};

use crate::apple::clock::AppleClock;
use crate::apple::util::{SendSync, create_block_buffer_from_bytes};
use crate::audio_mix::StereoMix;
use crate::backend::{AudioSink, Clock, SinkError};

/// `requestMediaDataWhenReady` blocks the engine's audio thread in
/// `write` until the renderer pulls; this callback is what wakes it.
type ReadyFlag = Arc<(Mutex<bool>, Condvar)>;

/// Floor per `CMSampleBuffer` (matches AetherEngine's 1024-sample floor, scaled
/// to >= 25 ms so 96/192 kHz streams never dip under an 11.6 ms HAL cycle).
const MIN_BUFFER_SAMPLES: usize = 1024;
/// Source PTS jump large enough to count as a real timeline discontinuity
/// rather than container millisecond rounding jitter.
const DISCONTINUITY_SECS: f64 = 0.100;

#[inline]
fn min_buffer_frames(sample_rate: u32) -> usize {
    ((sample_rate as usize) / 40).max(MIN_BUFFER_SAMPLES)
}

/// Gapless integer-tick clock anchor: locks onto the first frame's PTS and
/// marches by exact emitted sample count so consecutive `CMSampleBuffer`s
/// abut cleanly without ±0.5 ms MKV timestamp jitter.
#[derive(Default)]
struct SampleClockAnchor {
    anchor_ticks: Option<i64>,
    emitted_frames: i64,
}

impl SampleClockAnchor {
    fn reset(&mut self) {
        self.anchor_ticks = None;
        self.emitted_frames = 0;
    }

    fn resolve(&self, start_ticks: i64, sample_rate: u32) -> (i64, bool) {
        if let Some(anchor) = self.anchor_ticks {
            let predicted = anchor.saturating_add(self.emitted_frames);
            let diff_secs = (start_ticks - predicted).abs() as f64 / f64::from(sample_rate.max(1));
            if diff_secs <= DISCONTINUITY_SECS {
                return (predicted, false);
            }
        }
        (start_ticks, true)
    }

    fn commit(&mut self, out_ticks: i64, reanchor: bool, frames: usize) {
        if reanchor {
            self.anchor_ticks = Some(out_ticks);
            self.emitted_frames = frames as i64;
        } else {
            self.emitted_frames = self.emitted_frames.saturating_add(frames as i64);
        }
    }
}

pub struct AppleAudioSink {
    synchronizer: SendSync<Retained<AVSampleBufferRenderSynchronizer>>,
    audio_renderer: SendSync<Retained<AVSampleBufferAudioRenderer>>,
    auto_flush_obs: Option<SendSync<Retained<ProtocolObject<dyn NSObjectProtocol>>>>,
    clock: Arc<AppleClock>,
    sample_rate: u32,
    channels: u16,
    output_channels: usize,
    mix: Option<StereoMix>,
    scratch: Vec<f32>,
    /// Coalesced output-channel f32 samples waiting until `min_buffer_frames`
    /// fills so CoreMedia gets thick, steady buffers instead of 0.83 ms scraps.
    pending_pcm: Vec<f32>,
    pending_frames: usize,
    pending_start_ticks: i64,
    clock_anchor: SampleClockAnchor,
    format_desc: Option<CFRetained<CMAudioFormatDescription>>,
    ready: ReadyFlag,
    requesting: std::cell::Cell<bool>,
    /// Set by the first emitted buffer, or the first one after an explicit flush.
    /// Format reopens preserve it; unanchored `play`/`pause` record intent.
    anchored: bool,
    playing: bool,
    playback: super::PlaybackSlot,
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
        playback: super::PlaybackSlot,
    ) -> Self {
        let ready: ReadyFlag = Arc::new((Mutex::new(true), Condvar::new()));

        // When macOS switches audio routes (AirPods/Bluetooth/HDMI), CoreMedia
        // flushes the renderer behind our back; sweep any concurrent future-stamped
        // survivor on a side queue and wake the feed thread so her throat stays full.
        let flush_renderer = SendSync(audio_renderer.clone());
        let flush_ready = Arc::clone(&ready);
        let flush_queue = DispatchQueue::new("peartube.audio.autoflush", DispatchQueueAttr::SERIAL);
        let obs = unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(AVSampleBufferAudioRendererWasFlushedAutomaticallyNotification),
                Some(audio_renderer.as_ref()),
                None,
                &RcBlock::new(move |_note: NonNull<NSNotification>| {
                    let flush_renderer = &flush_renderer;
                    let renderer = SendSync(flush_renderer.0.clone());
                    let pair = Arc::clone(&flush_ready);
                    flush_queue.exec_async(move || {
                        let renderer = &renderer;
                        let r: &AVSampleBufferAudioRenderer = &renderer.0;
                        let _: () = objc2::msg_send![r, flush];
                        let (lock, cvar) = &*pair;
                        if let Ok(mut guard) = lock.lock() {
                            *guard = true;
                            cvar.notify_all();
                        }
                    });
                }),
            )
        };

        Self {
            synchronizer: SendSync(synchronizer),
            audio_renderer: SendSync(audio_renderer),
            auto_flush_obs: Some(SendSync(obs)),
            clock,
            sample_rate: 0,
            channels: 0,
            output_channels: 0,
            mix: None,
            scratch: Vec::new(),
            pending_pcm: Vec::new(),
            pending_frames: 0,
            pending_start_ticks: 0,
            clock_anchor: SampleClockAnchor::default(),
            format_desc: None,
            ready,
            requesting: std::cell::Cell::new(false),
            anchored: false,
            playing: false,
            playback,
        }
    }

    fn wait_ready(&self) -> bool {
        // Fast path: renderer is accepting data right now.
        let ready_now = unsafe {
            let renderer: &AVSampleBufferAudioRenderer = &self.audio_renderer;
            objc2::msg_send![renderer, isReadyForMoreMediaData]
        };
        if ready_now {
            return true;
        }
        if !self.playing {
            return false;
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
        // A bounded wait returns control to the engine so it can pause,
        // seek or stop even when the renderer stops requesting data.
        let (lock, cvar) = &*self.ready;
        let guard = lock.lock().expect("ready lock");
        let (guard, _) = cvar
            .wait_timeout(guard, Duration::from_millis(20))
            .expect("ready condvar");
        drop(guard);
        unsafe {
            let renderer: &AVSampleBufferAudioRenderer = &self.audio_renderer;
            objc2::msg_send![renderer, isReadyForMoreMediaData]
        }
    }

    fn clear_pending(&mut self) {
        self.pending_pcm.clear();
        self.pending_frames = 0;
        self.pending_start_ticks = 0;
    }

    /// Wraps `self.pending_pcm` into one `CMSampleBuffer` and hands it to
    /// `AVSampleBufferAudioRenderer`, anchoring the synchronizer on first taste.
    fn emit_pending(&mut self) -> Result<(), SinkError> {
        if self.pending_frames == 0 || self.pending_pcm.is_empty() {
            return Ok(());
        }
        let format_desc = self
            .format_desc
            .as_ref()
            .ok_or_else(|| SinkError::Fatal("audio sink not open".into()))?;
        let frames = self.pending_frames;
        let output_channels = self.output_channels.max(1);
        let sample_rate = self.sample_rate;

        let pcm_bytes = unsafe {
            std::slice::from_raw_parts(
                self.pending_pcm.as_ptr().cast::<u8>(),
                self.pending_pcm.len() * 4,
            )
        };
        let block = unsafe { create_block_buffer_from_bytes(pcm_bytes)? };

        let (out_ticks, reanchor) = self.clock_anchor.resolve(self.pending_start_ticks, sample_rate);
        let pts_cm = CMTime {
            value: out_ticks,
            timescale: sample_rate as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let timing = CMSampleTimingInfo {
            duration: CMTime {
                value: 1,
                timescale: sample_rate as i32,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            },
            presentationTimeStamp: pts_cm,
            decodeTimeStamp: unsafe { objc2_core_media::kCMTimeInvalid },
        };

        let mut raw: *mut CMSampleBuffer = ptr::null_mut();
        let sample_size = output_channels * 4;
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
        self.clear_pending();
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
        self.clock_anchor.commit(out_ticks, reanchor, frames);

        if !self.anchored {
            self.anchored = true;
            let rate = if self.playing { 1.0 } else { 0.0 };
            unsafe {
                let sync: &AVSampleBufferRenderSynchronizer = &self.synchronizer;
                let _: () = objc2::msg_send![sync, setRate: rate as f32, time: pts_cm];
            }
        }
        Ok(())
    }
}

impl AudioSink for AppleAudioSink {
    fn open(&mut self, sample_rate: u32, layout: oxideav_core::ChannelLayout) -> Result<(), SinkError> {
        let channels = layout.channel_count();
        if sample_rate == 0 || channels == 0 {
            return Err(SinkError::Fatal(format!(
                "invalid audio format: {sample_rate} Hz, {channels} channels"
            )));
        }
        // Flush any coalesced samples in the previous format before swapping
        // format descriptions; queued buffers keep their own description and
        // the shared synchronizer anchor stays untouched across format changes.
        if self.pending_frames > 0 && self.format_desc.is_some() {
            let _ = self.emit_pending();
        }
        if self.sample_rate != sample_rate {
            self.clock_anchor.reset();
        }
        self.sample_rate = sample_rate;
        self.channels = channels;
        // CoreAudio gets unlabeled channels from a count alone and placed 5.1
        // dialogue on one side of 2-speaker hardware; mix surround ourselves as Android does.
        let (output_channels, mix) = if channels > 2 {
            let mix = StereoMix::new(layout).map_err(|e| SinkError::Fatal(e.into()))?;
            (2usize, Some(mix))
        } else {
            (channels as usize, None)
        };
        self.output_channels = output_channels;
        self.mix = mix;
        let bytes_per_frame = output_channels as u32 * 4;
        let mut asbd = AudioStreamBasicDescription {
            mSampleRate: sample_rate as f64,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: bytes_per_frame,
            mFramesPerPacket: 1,
            mBytesPerFrame: bytes_per_frame,
            mChannelsPerFrame: output_channels as u32,
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
        if self.format_desc.is_none() {
            return Err(SinkError::Fatal("audio sink not open".into()));
        }
        let channels = self.channels as usize;
        if channels == 0 {
            return Err(SinkError::Fatal("audio sink opened with 0 channels".into()));
        }
        let frames = pcm.len() / channels;
        if frames == 0 {
            return Ok(0);
        }

        let incoming_ticks = (pts.as_secs_f64() * f64::from(self.sample_rate)).round() as i64;
        let min_frames = min_buffer_frames(self.sample_rate);

        // Check whether this write crosses a real PTS discontinuity while
        // samples are pending, or fills the coalescer to `min_frames`. In
        // either case we verify renderer readiness BEFORE mutating `pending`
        // so a full/paused renderer gets `Ok(0)` without dropping a sample.
        let discontinuous = if self.pending_frames > 0 {
            let expected_ticks = self.pending_start_ticks.saturating_add(self.pending_frames as i64);
            let diff_secs =
                (incoming_ticks - expected_ticks).abs() as f64 / f64::from(self.sample_rate.max(1));
            diff_secs > DISCONTINUITY_SECS
        } else {
            false
        };
        let will_emit = discontinuous || (self.pending_frames + frames >= min_frames);
        if will_emit && !self.wait_ready() {
            return Ok(0);
        }
        if discontinuous {
            self.emit_pending()?;
        }

        if self.pending_frames == 0 {
            self.pending_start_ticks = incoming_ticks;
            let reserve = min_frames.max(frames) * self.output_channels.max(1);
            if self.pending_pcm.capacity() < reserve {
                self.pending_pcm.reserve(reserve - self.pending_pcm.len());
            }
        }

        if let Some(mix) = &self.mix {
            let needed = frames * 2;
            if self.scratch.len() < needed {
                self.scratch.resize(needed, 0.0);
            }
            mix.mix(&pcm[..frames * channels], &mut self.scratch[..needed]);
            self.pending_pcm.extend_from_slice(&self.scratch[..needed]);
        } else {
            self.pending_pcm.extend_from_slice(&pcm[..frames * channels]);
        }
        self.pending_frames += frames;

        if self.pending_frames >= min_frames {
            self.emit_pending()?;
        }
        Ok(frames)
    }

    fn drain(&mut self) -> Result<bool, SinkError> {
        if self.pending_frames == 0 {
            return Ok(true);
        }
        if !self.wait_ready() {
            return Ok(false);
        }
        self.emit_pending()?;
        Ok(true)
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
        self.clear_pending();
        self.clock_anchor.reset();
        unsafe {
            self.synchronizer.setRate(0.0);
        }
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
        if let Some(obs) = self.auto_flush_obs.take() {
            unsafe {
                NSNotificationCenter::defaultCenter().removeObserver(obs.0.as_ref());
            }
        }
        let mut slot = self.playback.lock().expect("playback lock");
        if slot.as_ref().is_some_and(|s| std::ptr::eq(&*s.0, &*self.synchronizer.0)) {
            *slot = None;
        }
        drop(slot);
        if self.requesting.get() {
            unsafe {
                let renderer: &AVSampleBufferAudioRenderer = &self.audio_renderer;
                let _: () = objc2::msg_send![renderer, stopRequestingMediaData];
            }
            self.requesting.set(false);
        }
        // Video may still own this synchronizer. Retire only the audio
        // renderer; stopping the shared clock would freeze the video tail.
        unsafe {
            let renderer = objc2::runtime::ProtocolObject::from_ref(&*self.audio_renderer.0);
            let _: () = objc2::msg_send![&*self.audio_renderer.0, flush];
            self.synchronizer.removeRenderer_atTime_completionHandler(
                renderer,
                objc2_core_media::kCMTimeInvalid,
                None,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::ChannelLayout;

    #[test]
    fn sample_clock_anchor_erases_mkv_millisecond_jitter_and_honors_seeks() {
        let mut anchor = SampleClockAnchor::default();
        let rate = 48_000u32;
        // Simulate 25-ms (1,200-sample) coalesced buffers fed from 1-ms quantized MKV stamps.
        for i in 0..10 {
            let jittered_ms = (i * 25) + (i % 2); // ±1 ms rounding jitter
            let incoming_ticks = (jittered_ms as f64 * 0.001 * rate as f64).round() as i64;
            let (out_ticks, reanchor) = anchor.resolve(incoming_ticks, rate);
            assert_eq!(reanchor, i == 0);
            assert_eq!(out_ticks, i * 1_200);
            anchor.commit(out_ticks, reanchor, 1_200);
        }
        // A real jump (> 100 ms) re-anchors cleanly.
        let jump_ticks = 48_000 * 5;
        let (out_ticks, reanchor) = anchor.resolve(jump_ticks, rate);
        assert!(reanchor);
        assert_eq!(out_ticks, jump_ticks);
        anchor.commit(out_ticks, reanchor, 1_200);
        let (next_ticks, next_reanchor) = anchor.resolve(jump_ticks + 1_190, rate);
        assert!(!next_reanchor);
        assert_eq!(next_ticks, jump_ticks + 1_200);
    }

    #[test]
    fn coalesces_40_frame_truehd_units_and_preserves_anchor_across_format_reopen() {
        let sync = unsafe { AVSampleBufferRenderSynchronizer::new() };
        let renderer = unsafe { AVSampleBufferAudioRenderer::new() };
        let proto: Retained<ProtocolObject<dyn objc2_av_foundation::AVQueuedSampleBufferRendering>> =
            ProtocolObject::from_retained(renderer.clone());
        let tb_cf = unsafe {
            sync.setDelaysRateChangeUntilHasSufficientMediaData(false);
            sync.addRenderer(&proto);
            let tb: Retained<objc2_core_media::CMTimebase> = objc2::msg_send![&*sync, timebase];
            let ptr = Retained::into_raw(tb);
            CFRetained::from_raw(NonNull::new_unchecked(ptr))
        };
        let clock = Arc::new(AppleClock::from_timebase(tb_cf));
        let slot: super::super::PlaybackSlot = Arc::new(Mutex::new(None));
        let mut sink = AppleAudioSink::new(sync, renderer, clock.clone(), slot);

        // 48 kHz 7.1 TrueHD: 40 frames (0.833 ms) per write at paused start 2.000 s.
        sink.open(48_000, ChannelLayout::Surround71).unwrap();
        let unit = vec![0.1f32; 40 * 8];
        for i in 0..29 {
            let pts = Duration::from_secs_f64(2.0 + (i as f64 * 40.0 / 48_000.0));
            assert_eq!(sink.write(&unit, pts).unwrap(), 40);
            // Below 1,200-frame (25 ms) floor: held in coalescer, not yet anchored.
            assert!(!sink.anchored);
        }
        // 30th TrueHD AU completes 1,200 frames (25.0 ms) -> enqueues & anchors at 2.000 s.
        let pts_29 = Duration::from_secs_f64(2.0 + (29.0 * 40.0 / 48_000.0));
        assert_eq!(sink.write(&unit, pts_29).unwrap(), 40);
        assert!(sink.anchored);
        let anchored_at = clock.now().expect("anchored clock");
        assert!((anchored_at.as_secs_f64() - 2.0).abs() < 0.001);

        // Midstream format change (48 kHz 7.1 -> 44.1 kHz mono) flushes any pending tail
        // without reanchoring the paused clock away from 2.000 s.
        assert_eq!(sink.write(&unit, Duration::from_secs_f64(2.025)).unwrap(), 40);
        sink.open(44_100, ChannelLayout::Mono).unwrap();
        assert_eq!(sink.pending_frames, 0);
        let still_at = clock.now().expect("clock stays anchored");
        assert!((still_at.as_secs_f64() - 2.0).abs() < 0.001);

        // Explicit flush clears anchor and allows backward seek anchor at 1.000 s via drain().
        sink.flush();
        assert!(!sink.anchored);
        let mono_tail = vec![0.05f32; 200];
        assert_eq!(sink.write(&mono_tail, Duration::from_secs(1)).unwrap(), 200);
        assert!(!sink.anchored);
        assert!(sink.drain().unwrap());
        assert!(sink.anchored);
        let seek_at = clock.now().expect("reanchored clock");
        assert!((seek_at.as_secs_f64() - 1.0).abs() < 0.001);
    }
}
