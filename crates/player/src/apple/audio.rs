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
    AudioStreamBasicDescription, AudioStreamPacketDescription, kAudioFormatEnhancedAC3,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescription, CMItemCount, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol};
use oxideav_core::{ChannelLayout, ChannelPosition};

use crate::apple::clock::AppleClock;
use crate::apple::util::{SendSync, create_block_buffer_from_bytes};
use crate::audio_mix::StereoMix;
use crate::backend::{AudioOutputMode, AudioSink, Clock, PcmFormat, PcmSlice, SinkError};
use crate::headers::types::AudioCodec;

const AUDIO_FORMAT_AC3: u32 = u32::from_be_bytes(*b"ac-3");
const AUDIO_FORMAT_FLAG_SIGNED_INT: u32 = 1 << 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct CoreAudioChannelDescription {
    channel_label: u32,
    channel_flags: u32,
    coordinates: [f32; 3],
}

/// Builds a serialized CoreAudio `AudioChannelLayout` (`kAudioChannelLayoutTag_UseChannelDescriptions = 0`)
/// when `layout` names discrete speaker positions, keeping LFE and rear/side/top channels intact.
fn build_coreaudio_channel_layout(layout: ChannelLayout) -> Option<Vec<u8>> {
    let count = usize::from(layout.channel_count());
    if count == 0 || count > 64 {
        return None;
    }
    let mut descs = Vec::with_capacity(count);
    for i in 0..count {
        let label: u32 = match layout.position(i)? {
            ChannelPosition::FrontLeft => 1,
            ChannelPosition::FrontRight => 2,
            ChannelPosition::FrontCenter => 3,
            ChannelPosition::LowFrequency => 4,
            ChannelPosition::BackLeft => 5,
            ChannelPosition::BackRight => 6,
            ChannelPosition::FrontLeftOfCenter => 7,
            ChannelPosition::FrontRightOfCenter => 8,
            ChannelPosition::BackCenter => 9,
            ChannelPosition::SideLeft => 10,
            ChannelPosition::SideRight => 11,
            ChannelPosition::TopFrontLeft => 13,
            ChannelPosition::TopFrontRight => 15,
            ChannelPosition::TopBackLeft => 16,
            ChannelPosition::TopBackRight => 18,
            _ => return None,
        };
        descs.push(CoreAudioChannelDescription {
            channel_label: label,
            channel_flags: 0,
            coordinates: [0.0; 3],
        });
    }
    let mut raw = Vec::with_capacity(12 + count * std::mem::size_of::<CoreAudioChannelDescription>());
    raw.extend_from_slice(&0u32.to_ne_bytes());
    raw.extend_from_slice(&0u32.to_ne_bytes());
    raw.extend_from_slice(&(count as u32).to_ne_bytes());
    for d in descs {
        raw.extend_from_slice(&d.channel_label.to_ne_bytes());
        raw.extend_from_slice(&d.channel_flags.to_ne_bytes());
        for c in d.coordinates {
            raw.extend_from_slice(&c.to_ne_bytes());
        }
    }
    Some(raw)
}

#[cfg(target_os = "macos")]
mod macos_hal {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct AudioObjectPropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct AudioBuffer {
        number_channels: u32,
        data_byte_size: u32,
        data: *mut std::ffi::c_void,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct AudioValueRange {
        minimum: f64,
        maximum: f64,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectGetPropertyDataSize(
            object_id: u32,
            address: *const AudioObjectPropertyAddress,
            qualifier_data_size: u32,
            qualifier_data: *const std::ffi::c_void,
            out_data_size: *mut u32,
        ) -> i32;
        fn AudioObjectGetPropertyData(
            object_id: u32,
            address: *const AudioObjectPropertyAddress,
            qualifier_data_size: u32,
            qualifier_data: *const std::ffi::c_void,
            io_data_size: *mut u32,
            out_data: *mut std::ffi::c_void,
        ) -> i32;
        fn AudioObjectSetPropertyData(
            object_id: u32,
            address: *const AudioObjectPropertyAddress,
            qualifier_data_size: u32,
            qualifier_data: *const std::ffi::c_void,
            data_size: u32,
            data: *const std::ffi::c_void,
        ) -> i32;
    }

    const SYSTEM_OBJECT: u32 = 1;
    const DEFAULT_OUTPUT_DEVICE: u32 = u32::from_be_bytes(*b"dOut");
    const STREAM_CONFIGURATION: u32 = u32::from_be_bytes(*b"slay");
    const NOMINAL_SAMPLE_RATE: u32 = u32::from_be_bytes(*b"nsrt");
    const AVAILABLE_NOMINAL_RATES: u32 = u32::from_be_bytes(*b"nsr#");
    const SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob");
    const SCOPE_OUTPUT: u32 = u32::from_be_bytes(*b"outp");

    fn default_output_device() -> Option<u32> {
        let addr = AudioObjectPropertyAddress {
            selector: DEFAULT_OUTPUT_DEVICE,
            scope: SCOPE_GLOBAL,
            element: 0,
        };
        let mut dev_id: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &addr,
                0,
                std::ptr::null(),
                &mut size,
                (&mut dev_id as *mut u32).cast(),
            )
        };
        (status == 0 && dev_id != 0).then_some(dev_id)
    }

    pub(super) fn output_device_channels() -> usize {
        let Some(dev) = default_output_device() else { return 2 };
        let addr = AudioObjectPropertyAddress {
            selector: STREAM_CONFIGURATION,
            scope: SCOPE_OUTPUT,
            element: 0,
        };
        let mut size: u32 = 0;
        if unsafe { AudioObjectGetPropertyDataSize(dev, &addr, 0, std::ptr::null(), &mut size) } != 0
            || size < 4
            || size > 65_536
        {
            return 2;
        }
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        if unsafe {
            AudioObjectGetPropertyData(
                dev,
                &addr,
                0,
                std::ptr::null(),
                &mut size,
                buf.as_mut_ptr().cast(),
            )
        } != 0
        {
            return 2;
        }
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), size as usize)
        };
        if bytes.len() < 8 {
            return 2;
        }
        let num_buffers = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let stride = std::mem::size_of::<AudioBuffer>();
        let mut total = 0usize;
        for i in 0..num_buffers {
            let off = 8 + i * stride;
            if off + 4 > bytes.len() {
                break;
            }
            let ch = u32::from_ne_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]) as usize;
            total = total.saturating_add(ch);
        }
        total.max(2)
    }

    /// Moves the default output device to `target_hz` and returns the rate it
    /// had before, so the playback can hand the device back when it ends. Left
    /// alone, a 48 kHz film would leave every other app on the machine
    /// resampling until something else touched the device: measured on the
    /// built-in speakers, a bit-perfect 48 kHz run held the device at 48 kHz
    /// after the player was dropped.
    pub(super) fn match_device_sample_rate(target_hz: u32) -> Option<f64> {
        let dev = default_output_device()?;
        let target = f64::from(target_hz);
        let cur_addr = AudioObjectPropertyAddress {
            selector: NOMINAL_SAMPLE_RATE,
            scope: SCOPE_GLOBAL,
            element: 0,
        };
        let mut cur: f64 = 0.0;
        let mut cur_size = std::mem::size_of::<f64>() as u32;
        if unsafe {
            AudioObjectGetPropertyData(
                dev,
                &cur_addr,
                0,
                std::ptr::null(),
                &mut cur_size,
                (&mut cur as *mut f64).cast(),
            )
        } == 0
            && (cur - target).abs() < 0.5
        {
            return None;
        }
        let ranges_addr = AudioObjectPropertyAddress {
            selector: AVAILABLE_NOMINAL_RATES,
            scope: SCOPE_GLOBAL,
            element: 0,
        };
        let mut rsize: u32 = 0;
        if unsafe { AudioObjectGetPropertyDataSize(dev, &ranges_addr, 0, std::ptr::null(), &mut rsize) } != 0
            || rsize == 0
            || rsize > 16_384
        {
            return None;
        }
        let count = (rsize as usize) / std::mem::size_of::<AudioValueRange>();
        let mut ranges = vec![AudioValueRange { minimum: 0.0, maximum: 0.0 }; count];
        if unsafe {
            AudioObjectGetPropertyData(
                dev,
                &ranges_addr,
                0,
                std::ptr::null(),
                &mut rsize,
                ranges.as_mut_ptr().cast(),
            )
        } != 0
        {
            return None;
        }
        let supported = ranges
            .iter()
            .any(|r| target >= r.minimum - 0.5 && target <= r.maximum + 0.5);
        if !supported {
            return None;
        }
        let moved = unsafe {
            AudioObjectSetPropertyData(
                dev,
                &cur_addr,
                0,
                std::ptr::null(),
                std::mem::size_of::<f64>() as u32,
                (&target as *const f64).cast(),
            )
        };
        (moved == 0 && cur > 0.5).then_some(cur)
    }

    /// Hands the device back to the rate it had before a bit-perfect playback
    /// moved it, so a film does not leave the whole machine resampling.
    pub(super) fn restore_device_sample_rate(rate: f64) {
        let Some(dev) = default_output_device() else { return };
        if !(rate.is_finite() && rate > 0.5) {
            return;
        }
        let addr = AudioObjectPropertyAddress {
            selector: NOMINAL_SAMPLE_RATE,
            scope: SCOPE_GLOBAL,
            element: 0,
        };
        let _ = unsafe {
            AudioObjectSetPropertyData(
                dev,
                &addr,
                0,
                std::ptr::null(),
                std::mem::size_of::<f64>() as u32,
                (&rate as *const f64).cast(),
            )
        };
    }
}

fn endpoint_output_channels() -> usize {
    #[cfg(target_os = "macos")]
    {
        macos_hal::output_device_channels()
    }
    #[cfg(target_os = "ios")]
    {
        unsafe {
            let cls = objc2::runtime::AnyClass::get(c"AVAudioSession");
            if let Some(cls) = cls {
                let session: *mut objc2::runtime::AnyObject = objc2::msg_send![cls, sharedInstance];
                if !session.is_null() {
                    let max_ch: isize = objc2::msg_send![session, maximumOutputNumberOfChannels];
                    return (max_ch.max(2)) as usize;
                }
            }
            2
        }
    }
}
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
    output_format: PcmFormat,
    bytes_per_frame: usize,
    mix: Option<StereoMix>,
    scratch: Vec<f32>,
    pending_bytes: Vec<u8>,
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
    /// `format_desc` is E-AC-3 and packets arrive through `write_packet`.
    compressed: bool,
    /// The default output device's rate before this playback moved it, if it
    /// did; `Drop` gives it back so the change does not outlive the film.
    device_rate_before: Option<f64>,
    /// Diagnostics only: `PEARTUBE_AUDIO_DUMP` names a file that receives the
    /// exact buffers handed to CoreAudio, after the fold.
    dump: Option<std::fs::File>,
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
            output_format: PcmFormat::F32,
            bytes_per_frame: 0,
            mix: None,
            scratch: Vec::new(),
            pending_bytes: Vec::new(),
            pending_frames: 0,
            pending_start_ticks: 0,
            clock_anchor: SampleClockAnchor::default(),
            format_desc: None,
            ready,
            requesting: std::cell::Cell::new(false),
            anchored: false,
            playing: false,
            compressed: false,
            device_rate_before: None,
            dump: std::env::var_os("PEARTUBE_AUDIO_DUMP")
                .and_then(|path| std::fs::File::create(path).ok()),
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
        self.pending_bytes.clear();
        self.pending_frames = 0;
        self.pending_start_ticks = 0;
    }

    /// Wraps `self.pending_pcm` into one `CMSampleBuffer` and hands it to
    /// `AVSampleBufferAudioRenderer`, anchoring the synchronizer on first taste.
    fn emit_pending(&mut self) -> Result<(), SinkError> {
        if self.pending_frames == 0 || self.pending_bytes.is_empty() {
            return Ok(());
        }
        let format_desc = self
            .format_desc
            .as_ref()
            .ok_or_else(|| SinkError::Fatal("audio sink not open".into()))?;
        let frames = self.pending_frames;
        let sample_size = self.bytes_per_frame.max(1);
        let sample_rate = self.sample_rate;

        let block = unsafe { create_block_buffer_from_bytes(&self.pending_bytes)? };
        // Measurement hook: the bytes below are the ones CoreAudio renders, so
        // they are the only honest boundary to compare with a reference decode.
        if let Some(file) = self.dump.as_mut() {
            use std::io::Write as _;
            let _ = file.write_all(&self.pending_bytes);
        }
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
        self.anchor_synchronizer(pts_cm);
        Ok(())
    }

    /// The first buffer after an explicit flush starts the synchronizer there.
    fn anchor_synchronizer(&mut self, at: CMTime) {
        if !self.anchored {
            self.anchored = true;
            let rate = if self.playing { 1.0 } else { 0.0 };
            unsafe {
                let sync: &AVSampleBufferRenderSynchronizer = &self.synchronizer;
                let _: () = objc2::msg_send![sync, setRate: rate as f32, time: at];
            }
        }
    }
}

impl AudioSink for AppleAudioSink {
    fn open(&mut self, sample_rate: u32, layout: ChannelLayout) -> Result<(), SinkError> {
        self.open_pcm(sample_rate, layout, PcmFormat::F32, AudioOutputMode::Auto)
    }

    fn open_pcm(
        &mut self,
        sample_rate: u32,
        layout: ChannelLayout,
        format: PcmFormat,
        mode: AudioOutputMode,
    ) -> Result<(), SinkError> {
        let channels = layout.channel_count();
        if sample_rate == 0 || channels == 0 {
            return Err(SinkError::Fatal(format!(
                "invalid audio format: {sample_rate} Hz, {channels} channels"
            )));
        }
        if self.pending_frames > 0 && self.format_desc.is_some() {
            let _ = self.emit_pending();
        }
        if self.sample_rate != sample_rate {
            self.clock_anchor.reset();
        }
        #[cfg(target_os = "macos")]
        if mode == AudioOutputMode::BitPerfectPcm {
            if let Some(previous) = macos_hal::match_device_sample_rate(sample_rate) {
                self.device_rate_before.get_or_insert(previous);
            }
        }

        self.sample_rate = sample_rate;
        self.channels = channels;
        self.compressed = false;

        let keep_multichannel = match mode {
            AudioOutputMode::BitPerfectPcm | AudioOutputMode::Passthrough => {
                channels <= 2 || build_coreaudio_channel_layout(layout).is_some()
            }
            AudioOutputMode::Auto => {
                channels <= 2
                    || (endpoint_output_channels() >= usize::from(channels)
                        && build_coreaudio_channel_layout(layout).is_some())
            }
            AudioOutputMode::Stereo => channels <= 2,
        };

        let (output_channels, output_format, mix, ca_layout) = if !keep_multichannel && channels > 2 {
            let mix = StereoMix::new(layout).map_err(|e| SinkError::Fatal(e.into()))?;
            (2usize, PcmFormat::F32, Some(mix), None)
        } else {
            let ca = if channels > 2 {
                build_coreaudio_channel_layout(layout)
            } else {
                None
            };
            (usize::from(channels), format, None, ca)
        };

        let (format_flags, bits_per_channel) = match output_format {
            PcmFormat::U8 => (kAudioFormatFlagIsPacked, 8u32),
            PcmFormat::S16 => (AUDIO_FORMAT_FLAG_SIGNED_INT | kAudioFormatFlagIsPacked, 16),
            PcmFormat::S24 => (AUDIO_FORMAT_FLAG_SIGNED_INT | kAudioFormatFlagIsPacked, 24),
            PcmFormat::S32 => (AUDIO_FORMAT_FLAG_SIGNED_INT | kAudioFormatFlagIsPacked, 32),
            PcmFormat::F32 => (kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked, 32),
            PcmFormat::F64 => (kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked, 64),
        };
        let bytes_per_frame = output_channels * output_format.bytes_per_sample();
        self.output_channels = output_channels;
        self.output_format = output_format;
        self.bytes_per_frame = bytes_per_frame;
        if self.dump.is_some() {
            eprintln!(
                "APPLE_SINK_OUT rate={sample_rate} source_channels={channels} out_channels={output_channels} out_format={output_format:?} folded={}",
                mix.is_some()
            );
        }
        self.mix = mix;

        let mut asbd = AudioStreamBasicDescription {
            mSampleRate: f64::from(sample_rate),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: format_flags,
            mBytesPerPacket: bytes_per_frame as u32,
            mFramesPerPacket: 1,
            mBytesPerFrame: bytes_per_frame as u32,
            mChannelsPerFrame: output_channels as u32,
            mBitsPerChannel: bits_per_channel,
            mReserved: 0,
        };
        let (layout_size, layout_ptr) = match &ca_layout {
            Some(bytes) => (bytes.len(), bytes.as_ptr().cast()),
            None => (0, ptr::null()),
        };
        let mut raw: *const CMAudioFormatDescription = ptr::null();
        let status = unsafe {
            objc2_core_media::CMAudioFormatDescriptionCreate(
                None,
                NonNull::from(&mut asbd),
                layout_size,
                layout_ptr,
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
        self.format_desc = Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw.cast_mut())) });
        Ok(())
    }

    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError> {
        let channels = self.channels.max(1);
        let bytes = unsafe {
            std::slice::from_raw_parts(pcm.as_ptr().cast::<u8>(), pcm.len() * 4)
        };
        self.write_pcm(
            PcmSlice {
                format: PcmFormat::F32,
                channels,
                data: bytes,
            },
            pts,
        )
    }

    fn write_pcm(&mut self, pcm: PcmSlice<'_>, pts: Duration) -> Result<usize, SinkError> {
        if self.format_desc.is_none() || self.compressed {
            return Err(SinkError::Fatal("audio sink not open for PCM".into()));
        }
        let channels = usize::from(self.channels);
        if channels == 0 || usize::from(pcm.channels) != channels {
            return Err(SinkError::Fatal("audio sink channel count mismatch".into()));
        }
        let frames = pcm.frames();
        if frames == 0 {
            return Ok(0);
        }

        let incoming_ticks = (pts.as_secs_f64() * f64::from(self.sample_rate)).round() as i64;
        let min_frames = min_buffer_frames(self.sample_rate);

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
            let reserve = min_frames.max(frames) * self.bytes_per_frame.max(1);
            if self.pending_bytes.capacity() < reserve {
                self.pending_bytes.reserve(reserve - self.pending_bytes.len());
            }
        }

        if let Some(mix) = &self.mix {
            let needed = frames * 2;
            if self.scratch.len() < needed {
                self.scratch.resize(needed, 0.0);
            }
            mix.mix_pcm(pcm, &mut self.scratch[..needed]);
            let mixed_bytes = unsafe {
                std::slice::from_raw_parts(self.scratch.as_ptr().cast::<u8>(), needed * 4)
            };
            self.pending_bytes.extend_from_slice(mixed_bytes);
        } else if pcm.format == self.output_format {
            let exact_len = frames * self.bytes_per_frame;
            self.pending_bytes.extend_from_slice(&pcm.data[..exact_len]);
        } else {
            let f32_samples = pcm.to_f32_vec();
            let raw = unsafe {
                std::slice::from_raw_parts(f32_samples.as_ptr().cast::<u8>(), f32_samples.len() * 4)
            };
            self.pending_bytes.extend_from_slice(raw);
        }
        self.pending_frames += frames;

        if self.pending_frames >= min_frames {
            self.emit_pending()?;
        }
        Ok(frames)
    }

    fn open_compressed(
        &mut self,
        params: &oxideav_core::CodecParameters,
        first_packet: &[u8],
    ) -> Result<bool, SinkError> {
        self.open_compressed_with_mode(params, first_packet, AudioOutputMode::Auto)
    }

    fn open_compressed_with_mode(
        &mut self,
        params: &oxideav_core::CodecParameters,
        first_packet: &[u8],
        mode: AudioOutputMode,
    ) -> Result<bool, SinkError> {
        if matches!(mode, AudioOutputMode::BitPerfectPcm | AudioOutputMode::Stereo) {
            return Ok(false);
        }
        let Some(config) = crate::headers::codec::audio_config(params, first_packet) else {
            return Ok(false);
        };
        let (format_id, box_tag): (u32, &[u8; 4]) = match config.codec {
            AudioCodec::Eac3 => (kAudioFormatEnhancedAC3, b"dec3"),
            AudioCodec::Ac3 => {
                if mode != AudioOutputMode::Passthrough {
                    return Ok(false);
                }
                (AUDIO_FORMAT_AC3, b"dac3")
            }
        };
        if self.pending_frames > 0 && self.format_desc.is_some() && !self.compressed {
            let _ = self.emit_pending();
        }
        if self.sample_rate != config.sample_rate {
            self.clock_anchor.reset();
        }
        let mut cookie = Vec::with_capacity(8 + config.config.len());
        cookie.extend_from_slice(&((8 + config.config.len()) as u32).to_be_bytes());
        cookie.extend_from_slice(box_tag);
        cookie.extend_from_slice(&config.config);
        let mut asbd = AudioStreamBasicDescription {
            mSampleRate: f64::from(config.sample_rate),
            mFormatID: format_id,
            mFormatFlags: 0,
            mBytesPerPacket: 0,
            mFramesPerPacket: 1536,
            mBytesPerFrame: 0,
            mChannelsPerFrame: u32::from(config.channels),
            mBitsPerChannel: 0,
            mReserved: 0,
        };
        let mut raw: *const CMAudioFormatDescription = ptr::null();
        let status = unsafe {
            objc2_core_media::CMAudioFormatDescriptionCreate(
                None,
                NonNull::from(&mut asbd),
                0,
                ptr::null(),
                cookie.len(),
                cookie.as_ptr().cast(),
                None,
                NonNull::from(&mut raw),
            )
        };
        if status != 0 || raw.is_null() {
            return Ok(false);
        }
        self.format_desc = Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw.cast_mut())) });
        self.sample_rate = config.sample_rate;
        self.channels = config.channels;
        self.mix = None;
        self.compressed = true;
        Ok(true)
    }

    fn write_packet(&mut self, packet: &[u8], pts: Duration, frames: u32) -> Result<bool, SinkError> {
        if !self.compressed || packet.is_empty() {
            return Err(SinkError::Fatal("audio sink not open for compressed bitstream".into()));
        }
        if !self.wait_ready() {
            return Ok(false);
        }
        let format_desc = self.format_desc.as_ref().expect("compressed format");
        let sample_rate = self.sample_rate;
        let ticks = (pts.as_secs_f64() * f64::from(sample_rate)).round() as i64;
        let (out_ticks, reanchor) = self.clock_anchor.resolve(ticks, sample_rate);
        let pts_cm = CMTime { value: out_ticks, timescale: sample_rate as i32, flags: CMTimeFlags::Valid, epoch: 0 };
        let block = unsafe { create_block_buffer_from_bytes(packet)? };
        let description = AudioStreamPacketDescription {
            mStartOffset: 0,
            mVariableFramesInPacket: if frames == 1536 { 0 } else { frames },
            mDataByteSize: packet.len() as u32,
        };
        let mut raw: *mut CMSampleBuffer = ptr::null_mut();
        let status = unsafe {
            objc2_core_media::CMAudioSampleBufferCreateReadyWithPacketDescriptions(
                None,
                &block,
                format_desc,
                1,
                pts_cm,
                &description,
                NonNull::from(&mut raw),
            )
        };
        if status != 0 || raw.is_null() {
            return Err(SinkError::Fatal(format!("CMAudioSampleBufferCreateReadyWithPacketDescriptions: {status}")));
        }
        // SAFETY: Create-rule function returned +1.
        let sample = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };
        unsafe {
            let renderer: &AVSampleBufferAudioRenderer = &self.audio_renderer;
            let _: () = objc2::msg_send![renderer, enqueueSampleBuffer: &*sample];
        }
        self.clock_anchor.commit(out_ticks, reanchor, frames as usize);
        self.anchor_synchronizer(pts_cm);
        Ok(true)
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
        // Give the endpoint back before the playback's state disappears: a
        // 48 kHz film must not leave the machine's speakers at 48 kHz.
        #[cfg(target_os = "macos")]
        if let Some(previous) = self.device_rate_before.take() {
            macos_hal::restore_device_sample_rate(previous);
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

    #[test]
    fn bit_perfect_s32_71_preserves_signed_integer_asbd_and_discrete_lfe_layout() {
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
        let mut sink = AppleAudioSink::new(sync, renderer, clock, slot);

        sink.open_pcm(96_000, ChannelLayout::Surround71, PcmFormat::S32, AudioOutputMode::BitPerfectPcm)
            .expect("open 96kHz 7.1 S32 bit-perfect");
        assert!(sink.mix.is_none(), "BitPerfectPcm must not fold 7.1 to stereo");
        assert_eq!(sink.output_channels, 8);
        assert_eq!(sink.output_format, PcmFormat::S32);
        assert_eq!(sink.bytes_per_frame, 32);

        let mut raw = Vec::with_capacity(40 * 8 * 4);
        for ch in 0..40 * 8 {
            raw.extend_from_slice(&(0x1234_5679i32.wrapping_add(ch)).to_le_bytes());
        }
        let written = sink
            .write_pcm(
                PcmSlice {
                    format: PcmFormat::S32,
                    channels: 8,
                    data: &raw,
                },
                Duration::ZERO,
            )
            .expect("write S32 7.1");
        assert_eq!(written, 40);
        assert_eq!(sink.pending_bytes, raw, "pending buffer must hold bit-identical S32 bytes");
        assert!(sink.drain().expect("drain S32 7.1"));
    }
}
