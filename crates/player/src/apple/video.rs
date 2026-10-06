//! Video output: an `AVSampleBufferDisplayLayer` on a container layer.
//!
//! Compressed H.264/HEVC goes in as `CMSampleBuffer`s built from the
//! stream's avcC/hvcC extradata plus the packet payloads. Software-decoded
//! frames are converted with `oxideav-pixfmt` to NV12 (8-bit 4:2:0) or
//! 10-bit 4:2:0 as appropriate, wrapped in IOSurface-backed `CVPixelBuffer`s
//! from a pool, and enqueued on the same layer. All layer work happens on
//! the main queue; engine threads only touch CoreMedia objects and
//! `dispatch2` async blocks.

use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dispatch2::{run_on_main, DispatchQueue};
use objc2::rc::Retained;
use objc2_av_foundation::{
    AVLayerVideoGravityResizeAspect, AVQueuedSampleBufferRenderingStatus,
    AVSampleBufferDisplayLayer, AVSampleBufferVideoRenderer,
};
use objc2_core_foundation::{CFNumber, CFRetained};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
    CMVideoCodecType, CMVideoFormatDescription, CMVideoFormatDescriptionCreate,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
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
use crate::backend::{SinkError, VideoSink};

/// Codec id → VideoToolbox format-description creation, and the annex-B
/// rewrite that packets may need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompressedKind {
    H264,
    Hevc,
}

struct Compressed {
    /// NAL length size from avcC/hvcC (1, 2 or 4).
    length_size: usize,
    format: CFRetained<CMVideoFormatDescription>,
    /// First keyframe seen; nothing is enqueued before it, so decoding
    /// starts at a random-access point.
    primed: bool,
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
    observers: Vec<SendSync<Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>>>,
    _screenshot_probe: (),
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

// SAFETY: the layer is only touched on the main queue (all uses wrap in
// exec_async / run-on-main); the other fields are Send/Sync by type.
unsafe impl Send for AppleVideoSink {}
unsafe impl Sync for AppleVideoSink {}

/// A raw pointer wrapper that is `Send + Sync` so a +1-retained pointer can
/// travel to the main queue inside a closure (dereferenced only there).
struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

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

    pub fn new(layer: Retained<AVSampleBufferDisplayLayer>) -> Self {
        let main = unsafe { dispatch2::DispatchRetained::retain(std::ptr::NonNull::from(DispatchQueue::main())) };
        let failed: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let failed_for_obs = failed.clone();
        let frames_enqueued = Arc::new(AtomicU64::new(0));

        unsafe {
            layer.setVideoGravity(
                AVLayerVideoGravityResizeAspect
                    .expect("AVLayerVideoGravityResizeAspect is a documented constant"),
            );
        }

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
                        .map(|e| format!("{e:?}"))
                        .unwrap_or_else(|| "unknown decode failure".into());
                    *failed_for_obs.lock().expect("failed lock") = Some(err);
                }),
            )
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
            observers: vec![SendSync(observer)],
            _screenshot_probe: (),
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

    fn renderer_status_errors(&self) -> Option<String> {
        // Reading status from a non-main thread is safe on
        // AVSampleBufferVideoRenderer (documented thread-safe enqueue API).
        let renderer: Retained<AVSampleBufferVideoRenderer> = unsafe { self.layer.sampleBufferRenderer() };
        let status: AVQueuedSampleBufferRenderingStatus = unsafe { objc2::msg_send![&*renderer, status] };
        if status == AVQueuedSampleBufferRenderingStatus::Failed {
            let msg: Option<objc2::rc::Retained<objc2_foundation::NSError>> = unsafe {
                let err: *mut objc2_foundation::NSError = objc2::msg_send![&*renderer, error];
                if err.is_null() {
                    None
                } else {
                    // -error returns an autoreleased instance per the Get
                    // rule; retain for ownership.
                    // -error returned a valid non-null instance; `retain`
                    // bumps the count for ownership.
                    Some(objc2::rc::Retained::retain(err).expect("NSError null"))
                }
            };
            Some(
                msg.map(|e| format!("{e:?}"))
                    .unwrap_or_else(|| "layer status failed".into()),
            )
        } else {
            None
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
) -> Result<(CFRetained<CMVideoFormatDescription>, usize, bool), SinkError> {
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
        (split_length_prefixed(&rewritten, 4), 4)
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
    let _ = length_size;
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
    Ok((format, length_size, annex_b))
}

/// Splits 4-byte-length-prefixed NAL units.
fn split_length_prefixed(data: &[u8], length_size: usize) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + length_size <= data.len() {
        let len = match length_size {
            1 => data[p] as usize,
            2 => u16::from_be_bytes([data[p], data[p + 1]]) as usize,
            _ => u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]) as usize,
        };
        p += length_size;
        let end = match p.checked_add(len) {
            Some(e) if e <= data.len() => e,
            _ => break,
        };
        out.push(&data[p..end]);
        p = end;
    }
    out
}

impl VideoSink for AppleVideoSink {
    fn open_compressed(&mut self, params: &CodecParameters) -> bool {
        let kind = match params.codec_id.as_str() {
            "h264" => CompressedKind::H264,
            "hevc" | "h265" => CompressedKind::Hevc,
            _ => return false,
        };
        if params.extradata.is_empty() {
            return false;
        }
        let (format, length_size, annex_b) = match create_h264_or_hevc_format(kind, params) {
            Ok(v) => v,
            Err(_) => return false,
        };
        let mut state = self.state.lock().expect("video state");
        state.compressed = Some(Compressed {
            length_size,
            format,
            primed: false,
        });
        let _ = annex_b;
        true
    }

    fn push_packet(&mut self, packet: &Packet, pts: Duration) -> Result<(), SinkError> {
        if let Some(err) = self.failed.lock().expect("failed lock").take() {
            self.fallback_reported.set(true);
            return Err(SinkError::Fallback(err));
        }
        if let Some(err) = self.renderer_status_errors() {
            *self.failed.lock().expect("failed lock") = Some(err.clone());
            self.fallback_reported.set(true);
            return Err(SinkError::Fallback(err));
        }

        let (format, data, primed) = {
            let mut state = self.state.lock().expect("video state");
            let comp = state
                .compressed
                .as_mut()
                .ok_or_else(|| SinkError::Fatal("push_packet without open_compressed".into()))?;
            let is_key = packet.flags.keyframe;
            if !comp.primed {
                if !is_key {
                    // Wait for a random-access point before first enqueue.
                    return Ok(());
                }
                comp.primed = true;
            }
            let primed = comp.primed;
            let data = if packet_is_annex_b(&packet.data) {
                annex_b_to_length_prefixed(&packet.data, comp.length_size)
            } else {
                packet.data.clone()
            };
            (comp.format.clone(), data, primed)
        };
        let _ = primed;
        if data.is_empty() {
            return Ok(());
        }

        let block = unsafe { create_block_buffer_from_bytes(&data)? };
        let mut raw: *mut CMSampleBuffer = ptr::null_mut();
        let timing = CMSampleTimingInfo {
            duration: unsafe { objc2_core_media::kCMTimeInvalid },
            presentationTimeStamp: cm_time_from_duration(pts, 600),
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
        self.frames_enqueued.fetch_add(1, Ordering::Relaxed);
        // The sample buffer is only dereferenced on the main queue; hand it
        // over as a raw +1 pointer.
        let sample_ptr = std::sync::Arc::new(SendPtr(CFRetained::into_raw(sample).as_ptr()));
        self.enqueue_on_main(move |layer| {
            // An Obj-C exception here (renderer gone mid-teardown) must not
            // abort the process; the next push observes the failed status
            // and returns Fallback.
            let _ = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
                let sample = CFRetained::from_raw(NonNull::new_unchecked(
                    sample_ptr.0 as *mut CMSampleBuffer,
                ));
                let renderer = layer.sampleBufferRenderer();
                let _: () = objc2::msg_send![&*renderer, enqueueSampleBuffer: &*sample];
            }));
        });
        Ok(())
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
        self.frames_enqueued.store(0, Ordering::Relaxed);
        {
            let mut state = self.state.lock().expect("video state");
            if let Some(comp) = state.compressed.as_mut() {
                comp.primed = false;
            }
        }
        *self.failed.lock().expect("failed lock") = None;
        self.fallback_reported.set(false);
        self.enqueue_on_main(|layer| {
            let _ = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
                let renderer = layer.sampleBufferRenderer();
                let _: () = objc2::msg_send![&*renderer, flush];
            }));
        });
    }

    #[allow(dead_code)] // trait item; the engine calls it, tests/examples may not
    fn set_playing(&mut self, playing: bool) {
        self.state.lock().expect("video state").playing = playing;
        // Presentation timing follows the synchronizer rate; the layer has
        // no independent rate.
    }
}

impl AppleVideoSink {
    fn open_frames_inner(&mut self, params: &CodecParameters) -> Result<(), SinkError> {
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

        let mut state = self.state.lock().expect("video state");
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
                // Mark the frame display-immediately: with no audio stream
                // anchoring the synchronizer timebase, timestamp-based
                // display may never trigger, while this attachment shows
                // the frame as soon as it is decoded.
                unsafe extern "C-unwind" {
                    fn CFArrayGetCount(array: &objc2_core_foundation::CFArray)
                        -> isize;
                    fn CFArrayGetValueAtIndex(
                        array: &objc2_core_foundation::CFArray,
                        index: isize,
                    ) -> *const std::ffi::c_void;
                }
                let attachments =
                    CMSampleBuffer::sample_attachments_array(&sample, true)
                        .expect("attachments array");
                if CFArrayGetCount(&attachments) > 0 {
                    // SAFETY: the per-sample attachments dictionary of a
                    // freshly created sample buffer is mutable.
                    let dict = &*(CFArrayGetValueAtIndex(&attachments, 0)
                        as *const objc2_core_foundation::CFMutableDictionary);
                    objc2_core_foundation::CFMutableDictionary::set_value(
                        Some(dict),
                        (objc2_core_media::kCMSampleAttachmentKey_DisplayImmediately
                            as *const objc2_core_foundation::CFString)
                            .cast(),
                        (objc2_core_foundation::kCFBooleanTrue.unwrap()
                            as *const objc2_core_foundation::CFBoolean)
                            .cast(),
                    );
                }
                let renderer = sink.layer().sampleBufferRenderer();
                let _: () = objc2::msg_send![&*renderer, enqueueSampleBuffer: &*sample];
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

    #[allow(dead_code)] // trait item; the engine calls it, tests/examples may not
    fn set_playing(&mut self, playing: bool) {
        self.state.lock().expect("video state").playing = playing;
        // Presentation timing follows the synchronizer rate; the layer has
        // no independent rate.
    }
}

impl Drop for AppleVideoSink {
    fn drop(&mut self) {
        for obs in self.observers.drain(..) {
            unsafe {
                NSNotificationCenter::defaultCenter()
                    .removeObserver(obs.0.as_ref());
            }
        }
    }
}

impl VideoState {
    fn _probe(&self) {}
}

/// True when the packet payload is Annex-B (start-code delimited).
fn packet_is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])
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
