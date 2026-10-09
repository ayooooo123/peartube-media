//! Video output: an `AVSampleBufferDisplayLayer` on a container layer.
//!
//! Compressed H.264/HEVC goes through VideoToolbox; its decoded-picture
//! callback releases the engine's clock hold. Software-decoded frames are
//! converted with `oxideav-pixfmt` to NV12 (8-bit 4:2:0) or 10-bit 4:2:0
//! and wrapped in IOSurface-backed `CVPixelBuffer`s from a pool.
//! Both paths enqueue pixel-buffer samples on the same layer, which shows
//! each sample at its timestamp on the playback's clock: it is a renderer of
//! the audio's `AVSampleBufferRenderSynchronizer` when there is audio, and
//! otherwise runs on a timebase of its own, anchored to the engine's clock
//! whenever that starts or jumps. All layer work happens on the main queue;
//! engine threads only touch CoreMedia/VideoToolbox and dispatch async blocks.

use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dispatch2::{run_on_main, DispatchQueue};
use objc2::rc::Retained;
use objc2_av_foundation::{
    AVLayerVideoGravityResizeAspect, AVQueuedSampleBufferRenderingStatus,
    AVSampleBufferDisplayLayer, AVSampleBufferRenderSynchronizer, AVSampleBufferVideoRenderer,
};
use objc2_core_foundation::{CFNumber, CFRetained, CFType};
use objc2_core_media::{
    CMClock, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
    CMTimebase, CMVideoCodecType, CMVideoFormatDescription, CMVideoFormatDescriptionCreate,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets,
    CMVideoFormatDescriptionCreateForImageBuffer, CMVideoFormatDescriptionMatchesImageBuffer,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidthOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSNotification, NSNotificationCenter};
use oxideav_core::{CodecParameters, Packet, PixelFormat, VideoFrame};
use oxideav_pixfmt::convert::{self, ConvertOptions, FrameInfo as PixFrameInfo};

use crate::apple::util::{
    annex_b_to_length_prefixed, create_block_buffer_from_bytes, find_atom, parse_avcc,
    parse_hvcc, SendSync,
};
use crate::backend::{Clock, PictureReady, SinkError, VideoSink};

/// Codec id → VideoToolbox format-description creation, and the annex-B
/// rewrite that packets may need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompressedKind {
    H264,
    Hevc,
}

struct Compressed {
    kind: CompressedKind,
    /// NAL length size from avcC/hvcC (1, 2 or 4).
    length_size: usize,
    /// Stream framing from extradata, not ambiguous packet length bytes.
    annex_b: bool,
    format: CFRetained<CMVideoFormatDescription>,
    reorder_depth: usize,
    /// First keyframe seen; nothing is enqueued before it, so decoding
    /// starts at a random-access point.
    primed: bool,
    /// HEVC NoRaslOutputFlag: leading pictures after an entry CRA/BLA
    /// can reference pictures before the decoder's start.
    first_irap: bool,
    discard_rasl: bool,
    present_from: Duration,
}

impl Compressed {
    fn filter_leading(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if self.kind != CompressedKind::Hevc { return None; }
        let picture = split_length_prefixed(data, self.length_size).find_map(|nal| {
            let header = nal.get(..2)?;
            let kind = (header[0] >> 1) & 0x3f;
            (kind <= 31).then_some(kind)
        })?;
        if (16..=23).contains(&picture) {
            // HEVC 8.1.3: every IDR/BLA, and a CRA at decoder entry.
            self.discard_rasl = self.first_irap || picture <= 20;
            self.first_irap = false;
        }
        if !self.discard_rasl || !matches!(picture, 8 | 9) { return None; }
        // Drop only RASL slices, not parameter sets or SEI sharing the AU.
        // These slices are not decoded under NoRaslOutputFlag (FFmpeg's
        // hevcdec.c decode_slice); sending them to VT gives missing refs.
        let keep = |nal: &&[u8]| nal.len() < 2 || !matches!((nal[0] >> 1) & 0x3f, 8 | 9);
        let size = split_length_prefixed(data, self.length_size).filter(keep)
            .map(|nal| self.length_size + nal.len()).sum();
        let mut kept = Vec::with_capacity(size);
        for nal in split_length_prefixed(data, self.length_size).filter(keep) {
            kept.extend_from_slice(&(nal.len() as u32).to_be_bytes()[4 - self.length_size..]);
            kept.extend_from_slice(nal);
        }
        Some(kept)
    }
}

// VideoToolbox's C ABI. The session owns the callback's context until all
// outstanding callbacks have returned; queued display work holds its own Arc.
#[repr(C)]
struct DecompressionCallback {
    callback: unsafe extern "C" fn(
        *mut std::ffi::c_void, *mut std::ffi::c_void, i32, u32,
        *mut CVImageBuffer, CMTime, CMTime,
    ),
    context: *mut std::ffi::c_void,
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: *const std::ffi::c_void,
        format: *const CMVideoFormatDescription,
        specification: *const std::ffi::c_void,
        attributes: *const std::ffi::c_void,
        callback: *const DecompressionCallback,
        out: *mut *mut CFType,
    ) -> i32;
    fn VTDecompressionSessionDecodeFrame(
        session: *const CFType, sample: *const CMSampleBuffer, flags: u32,
        source: *mut std::ffi::c_void, info: *mut u32,
    ) -> i32;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: *const CFType) -> i32;
    fn VTDecompressionSessionInvalidate(session: *const CFType);
}

struct NativeDecoder {
    session: CFRetained<CFType>,
    output: Arc<DecodedOutput>,
}

struct DecodedOutput {
    layer: SinkHandle,
    main: dispatch2::DispatchRetained<DispatchQueue>,
    reports: Arc<Mutex<u64>>,
    generation: u64,
    present_from: AtomicU64,
    ready: PictureReady,
    failed: Arc<Mutex<Option<String>>>,
    frames_enqueued: Arc<AtomicU64>,
    format: Mutex<Option<CFRetained<CMVideoFormatDescription>>>,
    reorder_depth: usize,
    pictures: Mutex<Vec<(Duration, SendSync<CFRetained<CMSampleBuffer>>)>>,
    pending_main: AtomicU64,
}

impl NativeDecoder {
    fn new(format: &CMVideoFormatDescription, output: DecodedOutput) -> Result<Self, SinkError> {
        let output = Arc::new(output);
        let callback = DecompressionCallback {
            callback: decoded_picture,
            context: Arc::as_ptr(&output).cast_mut().cast(),
        };
        // The display layer needs IOSurface-backed decoded buffers, just
        // like the software-frame pool below. Keep VT's native pixel format.
        let surface = objc2_core_foundation::CFDictionary::<CFType, CFType>::empty();
        let attributes = unsafe {
            cf_dictionary(&[(kCVPixelBufferIOSurfacePropertiesKey, &surface as &CFType)])
        }.ok_or_else(|| SinkError::Fallback("could not create decoded pixel attributes".into()))?;
        let mut raw = ptr::null_mut();
        let status = unsafe {
            VTDecompressionSessionCreate(
                ptr::null(), format, ptr::null(), ptr::from_ref(&*attributes).cast(),
                &callback, &mut raw,
            )
        };
        if status != 0 || raw.is_null() {
            return Err(SinkError::Fallback(format!("VTDecompressionSessionCreate: {status}")));
        }
        // SAFETY: Create returned a +1 CF object.
        let session = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };
        Ok(Self { session, output })
    }

    fn present_from(&self, start: Duration) {
        self.output.present_from.store(
            u64::try_from(start.as_nanos()).unwrap_or(u64::MAX), Ordering::Release,
        );
    }

    fn decode(&self, sample: &CMSampleBuffer) -> Result<(), SinkError> {
        // Keep callbacks in decode order. The SPS-bounded queue below
        // puts their pictures in presentation order before rendering.
        let status = unsafe {
            VTDecompressionSessionDecodeFrame(
                &*self.session, sample, 0, ptr::null_mut(), ptr::null_mut(),
            )
        };
        if status == 0 { Ok(()) } else {
            Err(SinkError::Fallback(format!("VTDecompressionSessionDecodeFrame: {status}")))
        }
    }

    fn finish(&self) -> Result<(), SinkError> {
        let status = unsafe { VTDecompressionSessionWaitForAsynchronousFrames(&*self.session) };
        if status != 0 {
            return Err(SinkError::Fallback(format!("VTDecompressionSessionWaitForAsynchronousFrames: {status}")));
        }
        let current = self.output.reports.lock().expect("video reports");
        if *current == self.output.generation {
            for (pts, sample) in self.output.pictures.lock().expect("decoded pictures").drain(..) {
                self.output.enqueue(sample, pts);
            }
        }
        Ok(())
    }
}

impl Drop for NativeDecoder {
    fn drop(&mut self) {
        // No callback may outlive `output`. Callbacks only dispatch async
        // layer work, so this wait never needs the main thread to run it.
        unsafe {
            VTDecompressionSessionWaitForAsynchronousFrames(&*self.session);
            VTDecompressionSessionInvalidate(&*self.session);
        }
    }
}

impl DecodedOutput {
    fn sample(&self, image: &CVImageBuffer, pts: CMTime, duration: CMTime) -> Result<CFRetained<CMSampleBuffer>, SinkError> {
        let mut cached = self.format.lock().expect("decoded format");
        unsafe {
            if cached.as_ref().is_none_or(|format| !CMVideoFormatDescriptionMatchesImageBuffer(format, image)) {
                let mut raw = ptr::null();
                let status = CMVideoFormatDescriptionCreateForImageBuffer(None, image, NonNull::from(&mut raw));
                if status != 0 || raw.is_null() {
                    return Err(SinkError::Fallback(format!("CMVideoFormatDescriptionCreateForImageBuffer: {status}")));
                }
                *cached = Some(CFRetained::from_raw(NonNull::new_unchecked(raw.cast_mut())));
            }
            let timing = CMSampleTimingInfo {
                duration,
                presentationTimeStamp: pts,
                decodeTimeStamp: objc2_core_media::kCMTimeInvalid,
            };
            let mut raw = ptr::null_mut();
            let status = CMSampleBuffer::create_ready_with_image_buffer(
                None, image, cached.as_ref().expect("decoded format"),
                NonNull::from(&timing), NonNull::from(&mut raw),
            );
            if status != 0 || raw.is_null() {
                return Err(SinkError::Fallback(format!("CMSampleBufferCreateReadyWithImageBuffer: {status}")));
            }
            // The sample retains the callback's borrowed image buffer.
            Ok(CFRetained::from_raw(NonNull::new_unchecked(raw)))
        }
    }

    fn enqueue(self: &Arc<Self>, sample: SendSync<CFRetained<CMSampleBuffer>>, pts: Duration) {
        let output = Arc::clone(self);
        self.pending_main.fetch_add(1, Ordering::Release);
        self.main.exec_async(move || {
            let current = output.reports.lock().expect("video reports");
            output.pending_main.fetch_sub(1, Ordering::Release);
            if *current != output.generation
                || pts.as_nanos() < u128::from(output.present_from.load(Ordering::Acquire))
            {
                return;
            }
            let enqueued = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                with_renderer(output.layer.layer(), |renderer| unsafe {
                    let _: () = objc2::msg_send![renderer, enqueueSampleBuffer: &**sample];
                });
            }));
            if enqueued.is_ok() {
                output.frames_enqueued.fetch_add(1, Ordering::Relaxed);
                output.ready.ready(pts);
            } else {
                *output.failed.lock().expect("failed lock") = Some("Obj-C exception enqueueing a decoded picture".into());
            }
        });
    }
}

unsafe extern "C" fn decoded_picture(
    context: *mut std::ffi::c_void,
    _source: *mut std::ffi::c_void,
    status: i32,
    _flags: u32,
    image: *mut CVImageBuffer,
    timestamp: CMTime,
    duration: CMTime,
) {
    // SAFETY: NativeDecoder holds this Arc until VT has joined its callbacks.
    // The added strong reference keeps queued main-thread work alive too.
    let output = unsafe {
        let context = context.cast::<DecodedOutput>();
        Arc::increment_strong_count(context);
        Arc::from_raw(context)
    };
    let current = output.reports.lock().expect("video reports");
    if *current != output.generation { return; }
    if status != 0 {
        *output.failed.lock().expect("failed lock") = Some(format!("VideoToolbox decoded-picture callback: {status}"));
        return;
    }
    let Some(image) = (unsafe { image.as_ref() }) else { return };
    if !timestamp.flags.contains(CMTimeFlags::Valid) || timestamp.timescale <= 0 { return; }
    let pts = Duration::from_secs_f64((timestamp.value as f64 / f64::from(timestamp.timescale)).max(0.0));
    if pts.as_nanos() < u128::from(output.present_from.load(Ordering::Acquire)) { return; }
    let sample = match output.sample(image, timestamp, duration) {
        Ok(sample) => SendSync(sample),
        Err(error) => {
            *output.failed.lock().expect("failed lock") = Some(error.to_string());
            return;
        }
    };
    let mut pictures = output.pictures.lock().expect("decoded pictures");
    let at = pictures.partition_point(|(time, _)| *time <= pts);
    pictures.insert(at, (pts, sample));
    if pictures.len() > output.reorder_depth {
        let (pts, sample) = pictures.remove(0);
        output.enqueue(sample, pts);
    }
}

pub struct AppleVideoSink {
    /// The display layer; every use must be on the main thread, hence the
    /// wrapper. The container layer owns it after `attach`.
    layer: SendSync<Retained<AVSampleBufferDisplayLayer>>,
    /// Main-queue dispatcher for layer work.
    main: dispatch2::DispatchRetained<DispatchQueue>,
    state: Arc<Mutex<VideoState>>,
    /// Written on failure; the next `push_packet` returns Fallback so the
    /// engine switches to software from the next keyframe.
    failed: Arc<Mutex<Option<String>>>,
    frames_enqueued: Arc<AtomicU64>,
    /// Set to `true` once `failed` has been observed; further software
    /// frames then flow normally.
    fallback_reported: std::cell::Cell<bool>,
    /// What times the layer's presentation.
    timing: Timing,
    observers: Vec<SendSync<Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>>>,
    ready: Option<PictureReady>,
    decoder: Option<NativeDecoder>,
    /// Flush invalidates both old callbacks and their queued display work.
    reports: Arc<Mutex<u64>>,
}

/// What times the layer's presentation: it shows each sample when this
/// reaches the sample's timestamp.
enum Timing {
    /// The layer is a renderer of the playback's synchronizer, beside the
    /// audio renderer: its timebase is the audio clock.
    Synchronized {
        synchronizer: SendSync<Retained<AVSampleBufferRenderSynchronizer>>,
        clock: Arc<dyn Clock>,
    },
    /// No audio output: the layer's own timebase (on the host clock),
    /// anchored to the engine's clock whenever presentation starts, stops or
    /// jumps.
    Own {
        timebase: SendSync<CFRetained<CMTimebase>>,
        clock: Arc<dyn Clock>,
    },
    /// Timebase creation failed: report output failure, never silently
    /// display compressed video without synchronization.
    Unavailable,
}

impl Timing {
    /// Starts or stops the layer's presentation with the engine's clock. A
    /// synchronized layer runs its synchronizer too: once the audio has
    /// ended the video still pauses and plays.
    fn set_playing(&self, playing: bool) {
        let rate = if playing { 1.0 } else { 0.0 };
        match self {
            Timing::Synchronized { synchronizer, .. } if !playing => unsafe {
                synchronizer.setRate(0.0);
            },
            Timing::Synchronized { .. } | Timing::Own { .. } => self.anchor(rate),
            Timing::Unavailable => {}
        }
    }

    /// Re-anchor after a seek even if audio never writes another sample.
    fn jumped(&self, playing: bool) {
        self.anchor(if playing { 1.0 } else { 0.0 });
    }

    /// Follow the engine's position when it is using its free-running clock.
    /// When audio leads, both clocks already read the same native timebase.
    fn anchor(&self, rate: f64) {
        match self {
            Timing::Synchronized { synchronizer, clock } => unsafe {
                let timebase = synchronizer.timebase();
                let seconds = |time: CMTime| {
                    (time.flags.contains(CMTimeFlags::Valid) && time.timescale > 0)
                        .then(|| time.value as f64 / f64::from(time.timescale))
                };
                // Bracket the clock read: thread preemption between reads
                // must not look like skew when audio owns this timebase.
                let before = seconds(timebase.time());
                let now = clock.now();
                let after = seconds(timebase.time());
                if let Some(now) = now {
                    let aligned = before.zip(after).is_some_and(|(before, after)| {
                        let at = now.as_secs_f64();
                        at >= before - 0.001 && at <= after + 0.001
                    });
                    if !aligned {
                        let sync: &AVSampleBufferRenderSynchronizer = synchronizer;
                        let time = cm_time_from_duration(now, 1_000_000_000);
                        let _: () = objc2::msg_send![sync, setRate: rate as f32, time: time];
                        return;
                    }
                }
                synchronizer.setRate(rate as f32);
            },
            Timing::Own { timebase, clock } => {
                let Some(now) = clock.now() else { return };
                // CoreMedia's synchronization API is thread-safe.
                unsafe {
                    let host_now = CMClock::host_time_clock().time();
                    let _ = timebase.set_rate_and_anchor_time(rate, cm_time_from_duration(now, 1_000_000_000), host_now);
                }
            }
            Timing::Unavailable => {}
        }
    }
}

/// Makes the layer (its `AVSampleBufferVideoRenderer` where there is one)
/// a renderer of `synchronizer`, so it shows samples on that timebase.
/// False when AVFoundation refuses.
fn join_synchronizer(
    layer: &Retained<AVSampleBufferDisplayLayer>,
    synchronizer: &Retained<AVSampleBufferRenderSynchronizer>,
) -> bool {
    let (layer, synchronizer) = (SendSync(layer.clone()), SendSync(synchronizer.clone()));
    run_on_main(move |_mtm| {
        objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
            with_renderer(&layer, |renderer| unsafe { synchronizer.addRenderer(renderer) })
        }))
        .is_ok()
    })
}

/// A stopped timebase on the host clock, made the layer's control timebase.
fn own_timebase(layer: &Retained<AVSampleBufferDisplayLayer>) -> Option<CFRetained<CMTimebase>> {
    let mut raw: *mut CMTimebase = ptr::null_mut();
    // SAFETY: CF allocation with a valid out pointer; Create rule (+1).
    #[allow(deprecated)] // create_with_master_clock: the only bound constructor
    let status = unsafe {
        CMTimebase::create_with_master_clock(None, &CMClock::host_time_clock(), NonNull::from(&mut raw))
    };
    if status != 0 || raw.is_null() {
        return None;
    }
    let timebase = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };
    unsafe {
        let _ = timebase.set_rate(0.0);
    }
    let (layer, shared) = (SendSync(layer.clone()), SendSync(timebase.clone()));
    let set = run_on_main(move |_mtm| {
        objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
            layer.setControlTimebase(Some(&shared));
        }))
        .is_ok()
    });
    set.then_some(timebase)
}

struct VideoState {
    compressed: Option<Compressed>,
    /// Software path: pixel format + dimensions + pool.
    software: Option<SoftwarePath>,
    playing: bool,
}

struct SoftwarePath {
    src_format: PixelFormat,
    width: u32,
    height: u32,
    dst_ostype: u32,
    pool: SendSync<CFRetained<CVPixelBufferPool>>,
}

/// How long before its timestamp a software frame should reach the layer:
/// time to cross to the main queue and be queued before it is due.
const FRAME_LEAD: Duration = Duration::from_millis(100);



// SAFETY: the layer is only touched on the main queue (all uses wrap in
// exec_async / run-on-main); the other fields are Send/Sync by type.
unsafe impl Send for AppleVideoSink {}
unsafe impl Sync for AppleVideoSink {}

/// A raw pointer wrapper that is `Send + Sync` so a +1-retained pointer can
/// travel to the main queue inside a closure (dereferenced only there).
struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

/// Which enqueue path the display layer supports. `sampleBufferRenderer`
/// is macOS 14+/iOS 17+ only; the layer's own (deprecated) rendering
/// methods exist since 10.8/8.0 and are used below that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RendererPath {
    /// `layer.sampleBufferRenderer` + `AVSampleBufferVideoRenderer`.
    Modern,
    /// The layer's own `AVQueuedSampleBufferRendering` conformance.
    LegacyLayer,
}

fn renderer_path(layer: &AVSampleBufferDisplayLayer) -> RendererPath {
    static PATH: std::sync::OnceLock<RendererPath> = std::sync::OnceLock::new();
    *PATH.get_or_init(|| {
        // SAFETY: objc runtime queries on a live object.
        unsafe {
            let responds: bool = objc2::msg_send![
                layer,
                respondsToSelector: objc2::sel!(sampleBufferRenderer)
            ];
            if responds {
                RendererPath::Modern
            } else {
                RendererPath::LegacyLayer
            }
        }
    })
}

/// Runs `f` with the rendering target for `layer`: either the modern
/// `AVSampleBufferVideoRenderer` or the layer itself on older systems.
fn with_renderer<R>(
    layer: &AVSampleBufferDisplayLayer,
    f: impl FnOnce(&objc2::runtime::ProtocolObject<dyn objc2_av_foundation::AVQueuedSampleBufferRendering>) -> R,
) -> R {
    match renderer_path(layer) {
        RendererPath::Modern => {
            let renderer: Retained<AVSampleBufferVideoRenderer> = unsafe {
                layer.sampleBufferRenderer()
            };
            let proto: Retained<objc2::runtime::ProtocolObject<
                dyn objc2_av_foundation::AVQueuedSampleBufferRendering,
            >> = objc2::runtime::ProtocolObject::from_retained(renderer);
            f(&proto)
        }
        RendererPath::LegacyLayer => {
            let proto = objc2::runtime::ProtocolObject::from_ref(layer);
            f(proto)
        }
    }
}

/// A main-thread handle to the display layer, clonable across threads; the
/// layer is only dereferenced on the main queue.
struct SinkHandle(SendSync<Retained<AVSampleBufferDisplayLayer>>);
unsafe impl Send for SinkHandle {}
unsafe impl Sync for SinkHandle {}
impl SinkHandle {
    fn layer(&self) -> &AVSampleBufferDisplayLayer {
        &self.0
    }
}

impl AppleVideoSink {
    /// A clonable main-thread handle to the video layer for `run_on_main`
    /// closures.
    fn clone_sink_handle(&self) -> SinkHandle {
        SinkHandle(SendSync(self.layer.0.clone()))
    }

    /// A sink on `layer`. With `synchronizer` (the playback's, made for its
    /// audio) the layer becomes one of its renderers and shows samples on
    /// the audio clock; without one it gets a timebase of its own that
    /// follows `clock`.
    pub fn new(
        layer: Retained<AVSampleBufferDisplayLayer>,
        synchronizer: Option<Retained<AVSampleBufferRenderSynchronizer>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let main = unsafe { dispatch2::DispatchRetained::retain(std::ptr::NonNull::from(DispatchQueue::main())) };
        let failed: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let failed_for_obs = failed.clone();
        let frames_enqueued = Arc::new(AtomicU64::new(0));

        let gravity_layer = SendSync(layer.clone());
        run_on_main(move |_| unsafe {
            gravity_layer.setVideoGravity(
                AVLayerVideoGravityResizeAspect
                    .expect("AVLayerVideoGravityResizeAspect is a documented constant"),
            );
        });

        // FailedToDecode → record the error; the next push returns
        // SinkError::Fallback (packet pushes run on engine threads).
        let observer = unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(objc2_av_foundation::AVSampleBufferDisplayLayerFailedToDecodeNotification),
                None,
                None,
                &block2::RcBlock::new(move |note: NonNull<NSNotification>| {
                    let err = note
                        .as_ref()
                        .userInfo()
                        .and_then(|u| {
                            u.objectForKey(
                                objc2_av_foundation::AVSampleBufferDisplayLayerFailedToDecodeNotificationErrorKey,
                            )
                        })
                        .map(|error| {
                            let description: Retained<objc2_foundation::NSString> =
                                objc2::msg_send![&*error, description];
                            description.to_string()
                        })
                        .unwrap_or_else(|| "unknown decode failure".into());
                    *failed_for_obs.lock().expect("failed lock") = Some(err);
                }),
            )
        };

        let timing = match synchronizer {
            Some(synchronizer) => {
                if join_synchronizer(&layer, &synchronizer) {
                    Timing::Synchronized { synchronizer: SendSync(synchronizer), clock }
                } else {
                    Timing::Unavailable
                }
            }
            None => match own_timebase(&layer) {
                Some(timebase) => Timing::Own {
                    timebase: SendSync(timebase),
                    clock,
                },
                None => Timing::Unavailable,
            },
        };

        Self {
            layer: SendSync(layer),
            main,
            state: Arc::new(Mutex::new(VideoState {
                compressed: None,
                software: None,
                playing: false,
            })),
            failed,
            frames_enqueued,
            fallback_reported: std::cell::Cell::new(false),
            timing,
            observers: vec![SendSync(observer)],
            ready: None,
            decoder: None,
            reports: Arc::new(Mutex::new(0)),
        }
    }

    pub fn frames_enqueued(&self) -> u64 {
        self.frames_enqueued.load(Ordering::Relaxed)
    }

    fn enqueue_on_main(&self, f: impl FnOnce(&AVSampleBufferDisplayLayer) + Send + 'static) {
        // SAFETY: the raw pointer is only dereferenced on the main queue
        // (every f body in this file runs there); the +1 retain from
        // `clone` keeps it alive until the closure runs.
        let layer_ptr = std::sync::Arc::new(SendPtr(Retained::into_raw(
            self.layer.0.clone(),
        )));
        self.main.exec_async(move || {
            // SAFETY: non-null by construction (Retained::into_raw of a
            // cloned +1 reference).
            let layer = unsafe { Retained::from_raw(layer_ptr.0) }
                .expect("layer pointer null");
            f(&layer);
        });
    }

    fn create_decoder(&self, format: &CMVideoFormatDescription, reorder_depth: usize) -> Result<NativeDecoder, SinkError> {
        NativeDecoder::new(format, DecodedOutput {
            layer: self.clone_sink_handle(),
            main: self.main.clone(),
            reports: Arc::clone(&self.reports),
            generation: *self.reports.lock().expect("video reports"),
            present_from: AtomicU64::new(0),
            ready: self.ready.clone().expect("compressed ready callback"),
            failed: Arc::clone(&self.failed),
            frames_enqueued: Arc::clone(&self.frames_enqueued),
            format: Mutex::new(None),
            reorder_depth,
            pictures: Mutex::new(Vec::with_capacity(reorder_depth + 1)),
            pending_main: AtomicU64::new(0),
        })
    }

    fn renderer_status_errors(&self) -> Option<String> {
        // Status read happens on the enqueue thread; both the modern
        // renderer and the layer's own conformance are documented
        // thread-safe for these queries. An NSException raised inside AVF
        // unwinds through this Rust frame and would abort the process, so
        // catch and surface it as a fallback trigger.
        let read = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
            with_renderer(&self.layer, |r| {
                let status: AVQueuedSampleBufferRenderingStatus =
                    objc2::msg_send![r, status];
                if status != AVQueuedSampleBufferRenderingStatus::Failed {
                    return None;
                }
                let msg: Option<objc2::rc::Retained<objc2_foundation::NSError>> = {
                    let err: *mut objc2_foundation::NSError = objc2::msg_send![r, error];
                    if err.is_null() {
                        None
                    } else {
                        // -error returns an autoreleased instance per the
                        // Get rule; retain for ownership.
                        Some(objc2::rc::Retained::retain(err).expect("NSError null"))
                    }
                };
                Some(
                    msg.map(|e| format!("{} {}: {}", e.domain(), e.code(), e.localizedDescription()))
                        .unwrap_or_else(|| "layer status failed".into()),
                )
            })
        }));
        match read {
            Ok(v) => v,
            Err(_) => Some("Obj-C exception reading renderer status".into()),
        }
    }
}

fn cm_time_from_duration(d: Duration, timescale: i32) -> CMTime {
    let value = (d.as_secs_f64() * timescale as f64).round() as i64;
    CMTime {
        value,
        timescale,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    }
}

fn create_h264_or_hevc_format(
    kind: CompressedKind,
    params: &CodecParameters,
) -> Result<(CFRetained<CMVideoFormatDescription>, usize, bool, usize), SinkError> {
    // The MOV/MP4 demuxers hand the raw stsd-extension atom run (starting
    // with the avcC/hvcC atom header); strip to the record body.
    let record: &[u8] = if let Some(body) = find_atom(&params.extradata, b"avcC") {
        body
    } else if let Some(body) = find_atom(&params.extradata, b"hvcC") {
        body
    } else {
        &params.extradata
    };
    let annex_b = record.starts_with(&[0, 0, 0, 1]) || record.starts_with(&[0, 0, 1]);
    let rewritten;
    let (nals, length_size) = if annex_b {
        // Annex-B extradata: split into NALs and treat as parameter sets.
        rewritten = annex_b_to_length_prefixed(record, 4);
        (split_length_prefixed(&rewritten, 4).collect::<Vec<_>>(), 4)
    } else {
        let (kind_nals, len) = match kind {
            CompressedKind::H264 => parse_avcc(record)
                .ok_or_else(|| SinkError::Fallback("malformed avcC extradata".into()))?,
            CompressedKind::Hevc => parse_hvcc(record)
                .ok_or_else(|| SinkError::Fallback("malformed hvcC extradata".into()))?,
        };
        (kind_nals, len)
    };
    if nals.is_empty() || nals.len() > 64 {
        return Err(SinkError::Fallback(format!(
            "unsupported parameter-set count {}",
            nals.len()
        )));
    }
    let mut ptrs: Vec<NonNull<u8>> = Vec::with_capacity(nals.len());
    let mut sizes: Vec<usize> = Vec::with_capacity(nals.len());
    for nal in &nals {
        if nal.is_empty() {
            return Err(SinkError::Fallback("empty parameter set".into()));
        }
        ptrs.push(NonNull::from(&nal[0]));
        sizes.push(nal.len());
    }
    let mut raw: *const CMFormatDescription = ptr::null();
    let status = match kind {
        CompressedKind::H264 => unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                nals.len(),
                NonNull::from(ptrs.as_mut_slice()).cast(),
                NonNull::from(sizes.as_mut_slice()).cast(),
                length_size as i32,
                NonNull::from(&mut raw),
            )
        },
        CompressedKind::Hevc => unsafe {
            CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                None,
                nals.len(),
                NonNull::from(ptrs.as_mut_slice()).cast(),
                NonNull::from(sizes.as_mut_slice()).cast(),
                length_size as i32,
                None,
                NonNull::from(&mut raw),
            )
        },
    };
    if status != 0 || raw.is_null() {
        return Err(SinkError::Fallback(format!(
            "CMVideoFormatDescriptionCreateFrom*ParameterSets: {status}"
        )));
    }
    // SAFETY: Create-rule function returned +1.
    let format =
        unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw as *mut CMVideoFormatDescription)) };
    Ok((format, length_size, annex_b, reorder_depth(kind, &nals)))
}

/// VT's callback order is decode order, not display order. Reuse the
/// codec parsers for the SPS reorder bound; missing AVC VUI uses the
/// standard's maximum DPB size (16 pictures).
fn reorder_depth(kind: CompressedKind, nals: &[&[u8]]) -> usize {
    nals.iter().filter_map(|nal| {
        let header = *nal.first()?;
        match kind {
            CompressedKind::H264 if header & 0x1f == 7 => {
                let rbsp = oxideav_h264::nal::rbsp_from_nal_payload(&nal[1..]);
                let sps = oxideav_h264::sps::Sps::parse(&rbsp).ok()?;
                Some(sps.vui.as_ref().and_then(|vui| vui.bitstream_restriction.as_ref())
                    .map_or(16, |restriction| restriction.max_num_reorder_frames))
            }
            CompressedKind::Hevc if (header >> 1) & 0x3f == 33 => {
                let rbsp = oxideav_h264::nal::rbsp_from_nal_payload(nal.get(2..)?);
                let sps = oxideav_h265::sps::SeqParameterSet::parse(&rbsp).ok()?;
                Some(sps.sub_layer_ordering_info[usize::from(sps.max_sub_layers_minus1)].max_num_reorder_pics)
            }
            _ => None,
        }
    }).max().unwrap_or(16).min(16) as usize
}

/// Borrows length-prefixed NAL units without allocating a packet-sized list.
fn split_length_prefixed(mut data: &[u8], length_size: usize) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        if !(1..=4).contains(&length_size) { return None; }
        let prefix = data.get(..length_size)?;
        let length = prefix.iter().fold(0usize, |size, &byte| (size << 8) | usize::from(byte));
        let end = length_size.checked_add(length)?;
        let nal = data.get(length_size..end)?;
        data = &data[end..];
        Some(nal)
    })
}

impl VideoSink for AppleVideoSink {
    fn open_compressed(&mut self, params: &CodecParameters, ready: PictureReady) -> bool {
        if matches!(self.timing, Timing::Unavailable) { return false; }
        let kind = match params.codec_id.as_str() {
            "h264" => CompressedKind::H264,
            "hevc" | "h265" => CompressedKind::Hevc,
            _ => return false,
        };
        if params.extradata.is_empty() {
            return false;
        }
        let (format, length_size, annex_b, reorder_depth) = match create_h264_or_hevc_format(kind, params) {
            Ok(v) => v,
            Err(_) => return false,
        };
        self.ready = Some(ready);
        let decoder = match self.create_decoder(&format, reorder_depth) {
            Ok(decoder) => decoder,
            Err(_) => return false,
        };
        self.decoder = Some(decoder);
        let mut state = self.state.lock().expect("video state");
        state.compressed = Some(Compressed {
            kind,
            length_size,
            annex_b,
            format,
            reorder_depth,
            primed: false,
            first_irap: true,
            discard_rasl: false,
            present_from: Duration::ZERO,
        });
        true
    }

    fn present_from(&mut self, start: Duration) {
        if let Some(comp) = self.state.lock().expect("video state").compressed.as_mut() {
            comp.present_from = start;
        }
        if let Some(decoder) = &self.decoder {
            decoder.present_from(start);
        }
    }

    fn push_packet(&mut self, packet: &Packet, pts: Duration, random_access: bool) -> Result<(), SinkError> {
        if let Some(err) = self.failed.lock().expect("failed lock").take() {
            self.fallback_reported.set(true);
            return Err(SinkError::Fallback(err));
        }
        if let Some(err) = self.renderer_status_errors() {
            *self.failed.lock().expect("failed lock") = Some(err.clone());
            self.fallback_reported.set(true);
            return Err(SinkError::Fallback(err));
        }
        // Bound both queued main-thread work and the renderer's sample
        // queue. Input timestamps cannot bound a decoder's reorder delay.
        if self.decoder.as_ref().is_some_and(|decoder| decoder.output.pending_main.load(Ordering::Acquire) >= 2) {
            return Err(SinkError::WouldBlock);
        }
        let ready = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
            with_renderer(&self.layer, |renderer| unsafe {
                let ready: bool = objc2::msg_send![renderer, isReadyForMoreMediaData];
                ready
            })
        })).map_err(|_| SinkError::Fallback("Obj-C exception reading renderer input capacity".into()))?;
        if !ready { return Err(SinkError::WouldBlock); }

        let (format, data, present_from, reorder_depth) = {
            let mut state = self.state.lock().expect("video state");
            let comp = state
                .compressed
                .as_mut()
                .ok_or_else(|| SinkError::Fatal("push_packet without open_compressed".into()))?;
            if !comp.primed {
                if !random_access {
                    // Wait for a random-access point before first enqueue.
                    return Ok(());
                }
                comp.primed = true;
            }
            let mut data = if comp.annex_b {
                std::borrow::Cow::Owned(annex_b_to_length_prefixed(&packet.data, comp.length_size))
            } else {
                std::borrow::Cow::Borrowed(packet.data.as_slice())
            };
            if let Some(filtered) = comp.filter_leading(&data) {
                data = std::borrow::Cow::Owned(filtered);
            }
            (comp.format.clone(), data, comp.present_from, comp.reorder_depth)
        };
        if data.is_empty() {
            return Ok(());
        }

        let block = unsafe { create_block_buffer_from_bytes(&data)? };
        let mut raw: *mut CMSampleBuffer = ptr::null_mut();
        let timing = CMSampleTimingInfo {
            duration: unsafe { objc2_core_media::kCMTimeInvalid },
            presentationTimeStamp: cm_time_from_duration(pts, 1_000_000_000),
            decodeTimeStamp: unsafe { objc2_core_media::kCMTimeInvalid },
        };
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(&*format),
                1,
                1,
                &timing,
                1,
                ptr::from_ref(&data.len()),
                NonNull::from(&mut raw),
            )
        };
        if status != 0 || raw.is_null() {
            return Err(SinkError::Fallback(format!(
                "CMSampleBufferCreateReady (video): {status}"
            )));
        }
        // SAFETY: Create-rule function returned +1.
        let sample = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };
        if self.decoder.is_none() {
            let decoder = self.create_decoder(&format, reorder_depth)?;
            decoder.present_from(present_from);
            self.decoder = Some(decoder);
        }
        self.decoder.as_ref().expect("compressed decoder").decode(&sample)
    }

    fn open_frames(&mut self, params: &CodecParameters) -> Result<(), SinkError> {
        match objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
            self.open_frames_inner(params)
        })) {
            Ok(r) => r,
            Err(_) => Err(SinkError::Fatal("Obj-C exception in open_frames".into())),
        }
    }

    fn push_frame(&mut self, frame: &VideoFrame, pts: Duration) -> Result<(), SinkError> {
        match objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
            self.push_frame_inner(frame, pts)
        })) {
            Ok(r) => r,
            Err(_) => Err(SinkError::Fatal("Obj-C exception in push_frame".into())),
        }
    }

    fn flush(&mut self) {
        *self.reports.lock().expect("video reports") += 1;
        self.decoder.take();
        self.frames_enqueued.store(0, Ordering::Relaxed);
        let playing = {
            let mut state = self.state.lock().expect("video state");
            if let Some(comp) = state.compressed.as_mut() {
                comp.primed = false;
                comp.first_irap = true;
                comp.discard_rasl = false;
                comp.present_from = Duration::ZERO;
            }
            state.playing
        };
        *self.failed.lock().expect("failed lock") = None;
        self.fallback_reported.set(false);
        self.enqueue_on_main(|layer| {
            let _ = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                with_renderer(layer, |r| unsafe {
                    let _: () = objc2::msg_send![r, flush];
                });
            }));
        });
        self.timing.jumped(playing);
    }

    fn set_playing(&mut self, playing: bool) {
        self.state.lock().expect("video state").playing = playing;
        self.timing.set_playing(playing);
    }

    fn frame_lead(&self) -> Duration {
        match self.timing {
            Timing::Unavailable => Duration::ZERO,
            Timing::Synchronized { .. } | Timing::Own { .. } => FRAME_LEAD,
        }
    }

    /// Drain VideoToolbox's reordered tail before the engine waits for EOS.
    fn finish(&mut self) -> Result<(), SinkError> {
        if let Some(decoder) = &self.decoder {
            decoder.finish()?;
        }
        if let Some(error) = self.failed.lock().expect("failed lock").take() {
            return Err(SinkError::Fallback(error));
        }
        Ok(())
    }
}


impl AppleVideoSink {
    fn open_frames_inner(&mut self, params: &CodecParameters) -> Result<(), SinkError> {
        if matches!(self.timing, Timing::Unavailable) { return Err(SinkError::Unavailable); }
        let width = params.width.ok_or_else(|| {
            SinkError::Fatal("software video stream without width".into())
        })?;
        let height = params.height.ok_or_else(|| {
            SinkError::Fatal("software video stream without height".into())
        })?;
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return Err(SinkError::Fatal(format!(
                "unsupported video dimensions {width}x{height}"
            )));
        }
        let src_format = params.pixel_format.unwrap_or(PixelFormat::Yuv420P);
        let dst_ostype = match src_format {
            PixelFormat::Yuv420P10Le | PixelFormat::Yuv422P10Le | PixelFormat::Yuv444P10Le => {
                kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
            }
            _ => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        };

        // IOSurface-backed pool; IOSurface backing is required for
        // CVPixelBuffers enqueued on AVSampleBufferDisplayLayer.
        let pix_fmt_num = CFNumber::new_i32(dst_ostype as i32);
        let width_num = CFNumber::new_i32(width as i32);
        let height_num = CFNumber::new_i32(height as i32);
        let iosurface_empty: CFRetained<objc2_core_foundation::CFDictionary> = unsafe {
            CFRetained::cast_unchecked(objc2_core_foundation::CFDictionary::<
                objc2_core_foundation::CFType,
                objc2_core_foundation::CFType,
            >::empty())
        };
        let buf_attrs = unsafe {
            cf_dictionary(&[
                // SAFETY: static CFString constants exported by CoreVideo.
                (
                    kCVPixelBufferPixelFormatTypeKey,
                    &pix_fmt_num as &objc2_core_foundation::CFType,
                ),
                (kCVPixelBufferWidthKey, &width_num as &objc2_core_foundation::CFType),
                (kCVPixelBufferHeightKey, &height_num as &objc2_core_foundation::CFType),
                (
                    kCVPixelBufferIOSurfacePropertiesKey,
                    &iosurface_empty as &objc2_core_foundation::CFType,
                ),
            ])
        };
        let mut pool_raw: *mut CVPixelBufferPool = ptr::null_mut();
        let cvret = unsafe {
            CVPixelBufferPool::create(
                None,
                None,
                buf_attrs.as_deref(),
                NonNull::from(&mut pool_raw),
            )
        };
        if cvret != 0 || pool_raw.is_null() {
            return Err(SinkError::Fatal(format!(
                "CVPixelBufferPoolCreate: {cvret}"
            )));
        }
        // SAFETY: Create-rule function returned +1.
        let pool = unsafe { CFRetained::from_raw(NonNull::new_unchecked(pool_raw)) };

        *self.reports.lock().expect("video reports") += 1;
        self.decoder.take();
        let mut state = self.state.lock().expect("video state");
        state.compressed = None;
        state.software = Some(SoftwarePath {
            src_format,
            width,
            height,
            dst_ostype,
            pool: SendSync(pool),
        });
        Ok(())
    }

}

impl AppleVideoSink {
    fn push_frame_inner(&mut self, frame: &VideoFrame, pts: Duration) -> Result<(), SinkError> {
        if let Some(err) = self.failed.lock().expect("failed lock").take() {
            if !self.fallback_reported.get() {
                self.fallback_reported.set(true);
                return Err(SinkError::Fallback(err));
            }
        }
        let (sw_src, sw_w, sw_h, sw_dst) = {
            let state = self.state.lock().expect("video state");
            let sw = state
                .software
                .as_ref()
                .ok_or_else(|| SinkError::Fatal("push_frame without open_frames".into()))?;
            (sw.src_format, sw.width, sw.height, sw.dst_ostype)
        };
        let width = sw_w;
        let height = sw_h;

        // Convert to NV12 (8-bit) or p010-like 10-bit bi-planar.
        let converted = convert::convert(
            frame,
            PixFrameInfo::new(sw_src, width, height),
            match sw_dst {
                t if t == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange => {
                    PixelFormat::Yuv420P10Le
                }
                _ => PixelFormat::Nv12,
            },
            &ConvertOptions::default(),
        )
        .map_err(|e| SinkError::Fatal(format!("pixel conversion failed: {e}")))?;

        let pool = self
            .state
            .lock()
            .expect("video state")
            .software
            .as_ref()
            .map(|s| SendSync(s.pool.0.clone()))
            .expect("software path gone");
        // CoreVideo/CMSampleBuffer calls run on the main queue via
        // exec_async; the video thread blocks on the result channel. Obj-C
        // exceptions convert to SinkError instead of aborting.
        let sink = self.clone_sink_handle();
        let enqueued = run_on_main(move |_mtm| {
            let res = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
                let pixel: CFRetained<CVPixelBuffer> = pool_pixel_buffer(&pool)?;
                copy_planes_into_pixel_buffer(&pixel, &converted, width, height)?;
                let mut raw: *mut CMSampleBuffer = ptr::null_mut();
                let timing = CMSampleTimingInfo {
                    duration: objc2_core_media::kCMTimeInvalid,
                    presentationTimeStamp: cm_time_from_duration(pts, 600),
                    decodeTimeStamp: objc2_core_media::kCMTimeInvalid,
                };
                // A pixel-buffer format description for this exact buffer.
                let fmt = format_for_pixel_buffer(&pixel, width, height)?;
                let status = CMSampleBuffer::create_ready_with_image_buffer(
                    None,
                    &pixel,
                    &fmt,
                    NonNull::from(&timing),
                    NonNull::from(&mut raw),
                );
                if status != 0 || raw.is_null() {
                    return Err(SinkError::Fatal(format!(
                        "CMSampleBufferCreateReadyWithImageBuffer: {status}"
                    )));
                }
                // SAFETY: Create-rule function returned +1.
                let sample = CFRetained::from_raw(NonNull::new_unchecked(raw));
                with_renderer(sink.layer(), |r| {
                    let _: () = objc2::msg_send![r, enqueueSampleBuffer: &*sample];
                });
                Ok(())
            }));
            // The exception value itself is !Send; replace it with a message
            // before crossing back.
            res.map_err(|e| {
                let desc = e
                    .map(|exc| format!("{exc:?}"))
                    .unwrap_or_else(|| "unknown".into());
                SinkError::Fatal(format!("Obj-C exception in push_frame: {desc}"))
            })
        });
        match enqueued {
            Ok(Ok(())) => {
                self.frames_enqueued.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(SinkError::Fatal("push_frame failed".into())),
        }
    }
}

impl Drop for AppleVideoSink {
    fn drop(&mut self) {
        *self.reports.lock().expect("video reports") += 1;
        self.decoder.take();
        for obs in self.observers.drain(..) {
            unsafe {
                NSNotificationCenter::defaultCenter()
                    .removeObserver(obs.0.as_ref());
            }
        }
        // Free the layer for the next playback: out of this playback's
        // synchronizer, or off its own timebase. Asynchronous, so a sink
        // dropped while the main thread waits for the playback to wind down
        // cannot deadlock; the next sink's setup queues behind it.
        let layer = SendSync(self.layer.0.clone());
        match &self.timing {
            Timing::Synchronized { synchronizer, .. } => {
                let synchronizer = synchronizer.clone();
                self.main.exec_async(move || {
                    let _ = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                        with_renderer(&layer, |renderer| unsafe {
                            synchronizer.removeRenderer_atTime_completionHandler(
                                renderer,
                                objc2_core_media::kCMTimeInvalid,
                                None,
                            );
                        })
                    }));
                });
            }
            Timing::Own { .. } => {
                self.main.exec_async(move || {
                    let _ = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
                        layer.setControlTimebase(None);
                    }));
                });
            }
            Timing::Unavailable => {}
        }
    }
}

/// Builds a `{key: value}` dictionary of CFString → CFType. Keys are the
/// real `&'static CFString` constants exported by CoreVideo/CoreFoundation.
///
/// # Safety
/// Values must be valid CF objects outliving the call (they are retained
/// by the dictionary).
unsafe fn cf_dictionary(
    entries: &[(&'static objc2_core_foundation::CFString, &objc2_core_foundation::CFType)],
) -> Option<CFRetained<objc2_core_foundation::CFDictionary>> {
    let mut keys: Vec<*const std::ffi::c_void> = Vec::with_capacity(entries.len());
    let mut vals: Vec<*const std::ffi::c_void> = Vec::with_capacity(entries.len());
    for (k, v) in entries {
        keys.push(*k as *const objc2_core_foundation::CFString as *const std::ffi::c_void);
        vals.push(*v as *const objc2_core_foundation::CFType as *const std::ffi::c_void);
    }
    unsafe {
        // kCFType callbacks: the dictionary retains its keys and values,
        // so the pool's internal copy stays valid after the caller's
        // locals are dropped.
        objc2_core_foundation::CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            vals.as_mut_ptr(),
            entries.len() as isize,
            &objc2_core_foundation::kCFTypeDictionaryKeyCallBacks,
            &objc2_core_foundation::kCFTypeDictionaryValueCallBacks,
        )
    }
}

/// Takes a pixel buffer from the pool.
///
/// # Safety
/// `pool` must be a valid retained pool.
unsafe fn pool_pixel_buffer(
    pool: &CFRetained<CVPixelBufferPool>,
) -> Result<CFRetained<CVPixelBuffer>, SinkError> {
    let mut raw: *mut CVPixelBuffer = ptr::null_mut();
    let ret = unsafe { CVPixelBufferPool::create_pixel_buffer(None, pool, NonNull::from(&mut raw)) };
    if ret != 0 || raw.is_null() {
        return Err(SinkError::Fatal(format!(
            "CVPixelBufferPoolCreatePixelBuffer: {ret}"
        )));
    }
    // SAFETY: Create-rule function returned +1.
    Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) })
}

/// Copies converted planes into an IOSurface-backed pixel buffer.
///
/// # Safety
/// `pixel` must be locked/unlocked correctly; dims must match the buffer.
unsafe fn copy_planes_into_pixel_buffer(
    pixel: &CFRetained<CVPixelBuffer>,
    frame: &VideoFrame,
    width: u32,
    height: u32,
) -> Result<(), SinkError> {
    let lock = unsafe { CVPixelBufferLockBaseAddress(pixel, CVPixelBufferLockFlags(0)) };
    if lock != 0 {
        return Err(SinkError::Fatal(format!("CVPixelBufferLockBaseAddress: {lock}")));
    }
    let result = (|| {
        let planes = CVPixelBufferGetPlaneCount(pixel);
        let _ = (width, height);
        if planes == 0 {
            // Chunky — one flat copy.
            let dst_stride = CVPixelBufferGetBytesPerRow(pixel);
            let dst_h = CVPixelBufferGetHeight(pixel);
            let src_plane = frame
                .planes
                .first()
                .ok_or_else(|| SinkError::Fatal("converted frame without planes".into()))?;
            let src_stride = src_plane.stride;
            let rows = dst_h.min(src_plane.data.len() / src_stride.max(1));
            let base = CVPixelBufferGetBaseAddress(pixel);
            if base.is_null() {
                return Err(SinkError::Fatal("pixel buffer base address null".into()));
            }
            let dst = unsafe { std::slice::from_raw_parts_mut(base as *mut u8, dst_stride * dst_h) };
            for row in 0..rows {
                let src = &src_plane.data[row * src_stride..row * src_stride + src_stride.min(dst_stride)];
                dst[row * dst_stride..row * dst_stride + src.len()].copy_from_slice(src);
            }
        } else {
            // Bi-planar: plane 0 = Y, plane 1 = interleaved UV.
            for plane in 0..planes {
                let dp_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel, plane);
                let dp_w = CVPixelBufferGetWidthOfPlane(pixel, plane);
                let dp_h = CVPixelBufferGetHeightOfPlane(pixel, plane);
                let src = frame
                    .planes
                    .get(plane)
                    .ok_or_else(|| SinkError::Fatal("converted frame missing plane".into()))?;
                let s_stride = src.stride;
                let rows = dp_h.min(src.data.len() / s_stride.max(1));
                let row_bytes = dp_w
                    * match plane {
                        0 => 1usize,
                        _ => 2usize, // interleaved 2-channel
                    };
                let base = CVPixelBufferGetBaseAddressOfPlane(pixel, plane);
                if base.is_null() {
                    return Err(SinkError::Fatal("pixel buffer plane base null".into()));
                }
                let dst = unsafe {
                    std::slice::from_raw_parts_mut(base as *mut u8, dp_stride * dp_h)
                };
                let copy_len = row_bytes.min(dp_stride);
                for row in 0..rows {
                    let s = &src.data[row * s_stride..row * s_stride + copy_len];
                    dst[row * dp_stride..row * dp_stride + copy_len].copy_from_slice(s);
                }
            }
        }
        Ok(())
    })();
    unsafe { CVPixelBufferUnlockBaseAddress(pixel, CVPixelBufferLockFlags(0)) };
    result
}

/// A pixel-buffer-backed video format description.
///
/// # Safety
/// `pixel` must be a valid CVPixelBuffer.
unsafe fn format_for_pixel_buffer(
    pixel: &CFRetained<CVPixelBuffer>,
    width: u32,
    height: u32,
) -> Result<CFRetained<CMVideoFormatDescription>, SinkError> {
    let codec: CMVideoCodecType = CVPixelBufferGetPixelFormatType(pixel);
    let mut raw: *const CMVideoFormatDescription = ptr::null();
    let status = unsafe {
        CMVideoFormatDescriptionCreate(
            None,
            codec,
            width as i32,
            height as i32,
            None,
            NonNull::from(&mut raw),
        )
    };
    if status != 0 || raw.is_null() {
        return Err(SinkError::Fatal(format!(
            "CMVideoFormatDescriptionCreate: {status}"
        )));
    }
    // SAFETY: Create-rule function returned +1.
    Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw as *mut CMVideoFormatDescription)) })
}
