//! Video output: an `AVSampleBufferDisplayLayer` driven by a dedicated,
//! bounded native owner thread and an acknowledged main-queue bridge.
//!
//! Lifecycle requests and input buffers are mediated through a short-lock
//! mailbox. VideoToolbox creation, decoding, draining, invalidation, and
//! software pixel conversion execute strictly on the background owner thread.
//! Main-queue operations (layer binding, flushing, sample enqueueing, rate
//! changes, and disassociation) are bounded by one in-flight credit and
//! acknowledged upon completion.

use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2_av_foundation::{
    AVLayerVideoGravityResizeAspect, AVQueuedSampleBufferRenderingStatus, AVSampleBufferDisplayLayer,
    AVSampleBufferRenderSynchronizer, AVSampleBufferVideoRenderer,
};
use objc2_core_foundation::{CFNumber, CFRetained, CFType};
use objc2_core_media::{
    CMClock, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
    CMTimebase, CMVideoCodecType, CMVideoFormatDescription, CMVideoFormatDescriptionCreate,
    CMVideoFormatDescriptionCreateForImageBuffer,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets,
    CMVideoFormatDescriptionMatchesImageBuffer,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
    CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferPool, CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferWidthKey, kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSNotification, NSNotificationCenter};
use oxideav_core::{CodecParameters, Packet, PixelFormat, VideoFrame};
use oxideav_pixfmt::convert::{self, ConvertOptions, FrameInfo as PixFrameInfo};
use parking_lot::{Condvar, Mutex};

use crate::apple::util::{
    annex_b_to_length_prefixed, create_block_buffer_from_bytes, find_atom, parse_avcc,
    parse_hvcc, SendSync,
};
use crate::backend::{
    Clock, PictureReady, ProducerId, SinkError, VideoControl, VideoError, VideoMode,
    VideoOutput, VideoRequest, VideoSink, VideoTarget,
};
use crate::video_owner::{self, OwnerError, OwnerTicket};

const FRAME_LEAD: Duration = Duration::from_millis(100);

static SINK_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Shared handoff record across playbacks for the reused video layer.
///
/// Distinguishes an unused reservation from an actually native-bound lease so
/// cancellation before bind can release capacity without a false unbind success.
pub(crate) struct LayerLeaseState {
    pub(crate) bound_sink_id: Option<u64>,
    pub(crate) reserved_sink_id: Option<u64>,
    pub(crate) unbind_pending: bool,
    pub(crate) quarantined: bool,
}

impl LayerLeaseState {
    pub(crate) fn new() -> Self {
        Self {
            bound_sink_id: None,
            reserved_sink_id: None,
            unbind_pending: false,
            quarantined: false,
        }
    }

    /// Reserve the layer for `sink_id` without claiming native binding yet.
    pub(crate) fn try_reserve(&mut self, sink_id: u64) -> bool {
        if self.quarantined || self.unbind_pending {
            return false;
        }
        if self.reserved_sink_id.is_some_and(|id| id != sink_id) {
            return false;
        }
        match self.bound_sink_id {
            Some(id) if id == sink_id => {
                self.reserved_sink_id = Some(sink_id);
                true
            }
            None => {
                self.reserved_sink_id = Some(sink_id);
                true
            }
            _ => false,
        }
    }

    /// Confirm native bind completed for a previously reserved sink.
    pub(crate) fn confirm_bound(&mut self, sink_id: u64) {
        if self.reserved_sink_id == Some(sink_id) {
            self.bound_sink_id = Some(sink_id);
            self.reserved_sink_id = None;
        }
    }

    /// Drop a reservation that never reached native bind.
    pub(crate) fn release_unused_reservation(&mut self, sink_id: u64) {
        if self.reserved_sink_id == Some(sink_id) {
            self.reserved_sink_id = None;
            // Only clear bound id when this sink never confirmed bind and is not bound.
            if self.bound_sink_id == Some(sink_id) && !self.unbind_pending {
                // already bound by a prior successful bind; reservation was re-entry
            } else if self.bound_sink_id.is_none() {
                // nothing else to clear
            }
        }
    }

    pub(crate) fn try_acquire(&mut self, sink_id: u64) -> bool {
        self.try_reserve(sink_id)
    }

    pub(crate) fn start_unbind(&mut self, sink_id: u64) {
        if self.bound_sink_id == Some(sink_id) || self.reserved_sink_id == Some(sink_id) {
            self.unbind_pending = true;
        }
    }

    pub(crate) fn finish_unbind(&mut self, sink_id: u64, clean: bool) -> bool {
        let ours = self.bound_sink_id == Some(sink_id) || self.reserved_sink_id == Some(sink_id);
        if !ours {
            return false;
        }
        self.unbind_pending = false;
        self.reserved_sink_id = None;
        if clean {
            self.bound_sink_id = None;
            true
        } else {
            self.quarantined = true;
            false
        }
    }

    pub(crate) fn is_bound_for(&self, sink_id: u64) -> bool {
        self.bound_sink_id == Some(sink_id) && !self.unbind_pending && !self.quarantined
    }
}

/// Producer-tagged input buffer. One shared credit across queued and owner-held input.
pub(crate) enum MailboxInput {
    Packet {
        producer: ProducerId,
        packet: Packet,
        pts: Duration,
        random_access: bool,
    },
    Frame {
        producer: ProducerId,
        frame: VideoFrame,
        pts: Duration,
    },
}

impl MailboxInput {
    pub(crate) fn producer(&self) -> ProducerId {
        match self {
            Self::Packet { producer, .. } => *producer,
            Self::Frame { producer, .. } => *producer,
        }
    }
}

pub(crate) struct MailboxState {
    pub(crate) latest_producer: Option<ProducerId>,
    pub(crate) current_producer: Option<ProducerId>,
    pub(crate) configured_mode: Option<VideoMode>,
    /// Immutable desired request retained for idempotent polling.
    pub(crate) desired_request: Option<VideoRequest>,
    /// One-shot work flag; owner clears after observing a new producer setup.
    pub(crate) setup_pending: bool,
    pub(crate) transition_result: Option<(ProducerId, Result<VideoMode, VideoError>)>,
    pub(crate) input_slot: Option<MailboxInput>,
    pub(crate) input_held_by_owner: bool,
    pub(crate) present_from: Duration,
    pub(crate) playing: bool,
    /// Coalesced dirty marker for playing / present_from control-only updates.
    pub(crate) control_revision: u64,
    /// Input sealed for this producer; further push is rejected.
    pub(crate) input_sealed: Option<ProducerId>,
    /// Drain requested exactly once per sealed producer.
    pub(crate) drain_requested: Option<ProducerId>,
    /// Drain completed for producer (may be overridden by later async error).
    pub(crate) drain_completed: Option<ProducerId>,
    pub(crate) current_error: Option<(ProducerId, VideoError)>,
    /// Untagged layer notifications request a current native-status observation.
    renderer_status_dirty: bool,
    renderer_status_checking: bool,
    /// Same-seek reset=false: already-accepted predecessor still authorized for
    /// native enqueue after latest_producer advanced.
    pub(crate) authorized_predecessor: Option<ProducerId>,
    authorized_seek: u64,
    /// Last producer that successfully passed a native enqueue boundary.
    pub(crate) last_enqueued_producer: Option<ProducerId>,
    pub(crate) retired: bool,
    pub(crate) frames_enqueued: u64,
}

pub(crate) struct Mailbox {
    state: Mutex<MailboxState>,
    pub(crate) condvar: Condvar,
}

impl Mailbox {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(MailboxState {
                latest_producer: None,
                current_producer: None,
                configured_mode: None,
                desired_request: None,
                setup_pending: false,
                transition_result: None,
                input_slot: None,
                input_held_by_owner: false,
                present_from: Duration::ZERO,
                playing: false,
                control_revision: 0,
                input_sealed: None,
                drain_requested: None,
                drain_completed: None,
                current_error: None,
                renderer_status_dirty: false,
                renderer_status_checking: false,
                authorized_predecessor: None,
                authorized_seek: 0,
                last_enqueued_producer: None,
                retired: false,
                frames_enqueued: 0,
            }),
            condvar: Condvar::new(),
        }
    }

    pub(crate) fn lock(&self) -> parking_lot::MutexGuard<'_, MailboxState> {
        self.state.lock()
    }

    pub(crate) fn submit_request(&self, request: &VideoRequest) -> Result<(), VideoError> {
        let mut state = self.state.lock();
        if let Some(cur) = state.latest_producer {
            if request.producer.0 < cur.0 {
                return Err(VideoError::Superseded);
            }
            if request.producer == cur {
                // Idempotent: keep desired request, do not re-queue setup.
                return Ok(());
            }
        }
        // Publish preservation with the request, before in-flight owner work can
        // observe the cancelled predecessor. A reset breaks this chain.
        state.authorized_predecessor = state.current_producer.filter(|current| {
            matches!(request.target, VideoTarget::Frames { reset: false, .. })
                && state.desired_request.as_ref().is_some_and(|previous| {
                    previous.seek_generation == request.seek_generation
                        && previous.output_revision == request.output_revision
                        && (previous.producer == *current
                            || (state.authorized_predecessor == Some(*current)
                                && state.authorized_seek == previous.seek_generation))
                })
        });
        state.authorized_seek = request.seek_generation;
        state.latest_producer = Some(request.producer);
        state.desired_request = Some(request.clone());
        state.setup_pending = true;
        state.transition_result = None;
        state.input_sealed = None;
        state.drain_requested = None;
        state.drain_completed = None;
        // Seed present_from from the immutable compressed request.
        if let VideoTarget::Compressed { present_from, .. } = &request.target {
            state.present_from = *present_from;
            state.control_revision = state.control_revision.wrapping_add(1);
        }
        drop(state);
        self.condvar.notify_all();
        Ok(())
    }

    /// Observe and clear the one-shot setup flag without dropping desired_request.
    pub(crate) fn take_setup_request(&self) -> Option<VideoRequest> {
        let mut state = self.state.lock();
        if !state.setup_pending {
            return None;
        }
        state.setup_pending = false;
        state.desired_request.clone()
    }

    pub(crate) fn take_pending_request(&self) -> Option<VideoRequest> {
        self.take_setup_request()
    }

    pub(crate) fn take_input(&self) -> Option<MailboxInput> {
        let mut state = self.state.lock();
        let input = state.input_slot.take();
        if input.is_some() {
            state.input_held_by_owner = true;
        }
        input
    }

    pub(crate) fn discard_input(&self, _producer: ProducerId) {
        let mut state = self.state.lock();
        state.input_held_by_owner = false;
        drop(state);
        self.condvar.notify_all();
    }

    pub(crate) fn release_input_credit(&self) {
        let mut state = self.state.lock();
        state.input_held_by_owner = false;
        drop(state);
        self.condvar.notify_all();
    }

    fn admit_input(state: &MailboxState, producer: ProducerId) -> Result<(), VideoError> {
        if state.latest_producer != Some(producer) {
            return Err(VideoError::Superseded);
        }
        // Replacement in flight: current still names the predecessor device.
        // New-producer admission waits until install publishes current=latest.
        if state.current_producer.is_some() && state.current_producer != Some(producer) {
            return Err(VideoError::Sink(SinkError::WouldBlock));
        }
        if state.input_sealed == Some(producer) {
            return Err(VideoError::Sink(SinkError::Fatal(
                "input sealed for producer".into(),
            )));
        }
        if let Some((p, err)) = &state.current_error {
            if *p == producer {
                return Err(err.clone());
            }
        }
        if state.input_slot.is_some() || state.input_held_by_owner {
            return Err(VideoError::Sink(SinkError::WouldBlock));
        }
        Ok(())
    }

    pub(crate) fn clear_authorized_predecessor(&self) {
        let mut state = self.state.lock();
        state.authorized_predecessor = None;
        drop(state);
        self.condvar.notify_all();
    }

    fn preserving_successor(state: &MailboxState, producer: ProducerId) -> Option<&VideoRequest> {
        state.desired_request.as_ref().filter(|request| {
            !state.retired
                && state.authorized_predecessor == Some(producer)
                && state.latest_producer == Some(request.producer)
                && state.authorized_seek == request.seek_generation
                && matches!(request.target, VideoTarget::Frames { reset: false, .. })
                && !request.control.cancelled(request.producer, request.seek_generation)
                && request.control.active_now() < request.deadline
        })
    }

    /// Native boundary gate: latest producer or explicitly authorized predecessor.
    pub(crate) fn allows_native_work(state: &MailboxState, producer: ProducerId) -> bool {
        !state.retired
            && (state.latest_producer == Some(producer)
                || Self::preserving_successor(state, producer).is_some())
    }

    /// Cancel at a native boundary does not drop authorized predecessor work.
    pub(crate) fn cancel_blocks_native(
        state: &MailboxState,
        control: &Arc<dyn VideoControl>,
        producer: ProducerId,
        seek_gen: u64,
    ) -> bool {
        if state.retired {
            return true;
        }
        if state.latest_producer == Some(producer) {
            return control.cancelled(producer, seek_gen);
        }
        Self::preserving_successor(state, producer)
            .is_none_or(|successor| successor.seek_generation != seek_gen)
    }

    /// Owner-only wait: cancellation can precede publication of the next request.
    /// Keep accepted work until that request decides preserve versus reset.
    /// Neither the frontend nor a main-queue callback may wait here.
    fn wait_for_published_request(
        &self,
        control: &Arc<dyn VideoControl>,
        producer: ProducerId,
        seek_gen: u64,
    ) -> bool {
        let mut state = self.lock();
        while !state.retired
            && (state.latest_producer == Some(producer)
                || state.authorized_predecessor == Some(producer))
            && state.desired_request.as_ref().is_some_and(|request| {
                request.control.cancelled(request.producer, request.seek_generation)
            })
        {
            self.condvar.wait(&mut state);
        }
        Self::allows_native_work(&state, producer)
            && !Self::cancel_blocks_native(&state, control, producer, seek_gen)
    }

    pub(crate) fn push_packet(
        &self,
        producer: ProducerId,
        packet: &mut Option<Packet>,
        pts: Duration,
        random_access: bool,
    ) -> Result<(), VideoError> {
        let mut state = self.state.lock();
        Self::admit_input(&state, producer)?;
        if state.configured_mode != Some(VideoMode::Compressed) {
            return Err(VideoError::Sink(SinkError::Fatal(
                "push_packet in non-compressed mode".into(),
            )));
        }
        let Some(pkt) = packet.take() else {
            return Ok(());
        };
        state.input_slot = Some(MailboxInput::Packet {
            producer,
            packet: pkt,
            pts,
            random_access,
        });
        drop(state);
        self.condvar.notify_all();
        Ok(())
    }

    pub(crate) fn push_frame(
        &self,
        producer: ProducerId,
        frame: &mut Option<VideoFrame>,
        pts: Duration,
    ) -> Result<(), VideoError> {
        let mut state = self.state.lock();
        Self::admit_input(&state, producer)?;
        if state.configured_mode != Some(VideoMode::Frames) {
            return Err(VideoError::Sink(SinkError::Fatal(
                "push_frame in non-frames mode".into(),
            )));
        }
        let Some(frm) = frame.take() else {
            return Ok(());
        };
        state.input_slot = Some(MailboxInput::Frame {
            producer,
            frame: frm,
            pts,
        });
        drop(state);
        self.condvar.notify_all();
        Ok(())
    }

    pub(crate) fn present_from(
        &self,
        producer: ProducerId,
        start: Duration,
    ) -> Result<(), VideoError> {
        let mut state = self.state.lock();
        if state.latest_producer != Some(producer) {
            return Err(VideoError::Superseded);
        }
        if state.present_from != start {
            state.present_from = start;
            state.control_revision = state.control_revision.wrapping_add(1);
        }
        drop(state);
        self.condvar.notify_all();
        Ok(())
    }

    pub(crate) fn set_playing(
        &self,
        producer: ProducerId,
        playing: bool,
    ) -> Result<(), VideoError> {
        let mut state = self.state.lock();
        if state.latest_producer != Some(producer) {
            return Err(VideoError::Superseded);
        }
        if state.playing != playing {
            state.playing = playing;
            state.control_revision = state.control_revision.wrapping_add(1);
        }
        drop(state);
        self.condvar.notify_all();
        Ok(())
    }

    pub(crate) fn poll_finish(&self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        let mut state = self.state.lock();
        if state.latest_producer != Some(producer) {
            return Poll::Ready(Err(VideoError::Superseded));
        }
        if let Some((p, err)) = &state.current_error {
            if *p == producer {
                // Late async error overrides cached finish success.
                return Poll::Ready(Err(err.clone()));
            }
        }
        if state.renderer_status_dirty || state.renderer_status_checking {
            return Poll::Pending;
        }
        if state.drain_completed == Some(producer) {
            return Poll::Ready(Ok(()));
        }
        // Seal input once; request exactly one drain.
        if state.input_sealed != Some(producer) {
            state.input_sealed = Some(producer);
        }
        if state.drain_requested != Some(producer) {
            state.drain_requested = Some(producer);
            drop(state);
            self.condvar.notify_all();
            return Poll::Pending;
        }
        Poll::Pending
    }

    /// Reject stale producers; never overwrite a newer producer's error slot.
    pub(crate) fn report_error(
        &self,
        producer: ProducerId,
        error: VideoError,
        control: &Arc<dyn VideoControl>,
    ) {
        let wake = {
            let mut state = self.state.lock();
            if let Some(latest) = state.latest_producer {
                if producer.0 < latest.0 {
                    return;
                }
            }
            if let Some((existing, _)) = &state.current_error {
                if existing.0 > producer.0 {
                    return;
                }
            }
            state.current_error = Some((producer, error));
            // A late error invalidates a prior drain success for this producer.
            if state.drain_completed == Some(producer) {
                state.drain_completed = None;
            }
            true
        };
        if wake {
            self.condvar.notify_all();
            control.wake();
        }
    }

    fn renderer_status_changed(&self) {
        let control = {
            let mut state = self.state.lock();
            if state.retired {
                return;
            }
            state.renderer_status_dirty = true;
            state.desired_request.as_ref().map(|request| Arc::clone(&request.control))
        };
        self.condvar.notify_all();
        if let Some(control) = control {
            control.wake();
        }
    }

    pub(crate) fn check_transition_deadline(
        &self,
        request: &VideoRequest,
    ) -> Result<(), VideoError> {
        // Retirement remains observable after the setup budget expires.
        if matches!(request.target, VideoTarget::Retired) {
            return Ok(());
        }
        let state = self.state.lock();
        // Setup deadline applies only while this producer is not yet configured.
        let pending = state.configured_mode.is_none()
            || state.current_producer != Some(request.producer)
            || state.transition_result.as_ref().is_none_or(|(p, _)| *p != request.producer);
        if pending && request.control.active_now() >= request.deadline {
            return Err(VideoError::Sink(SinkError::WouldBlock));
        }
        Ok(())
    }

    pub(crate) fn poll_transition_status(
        &self,
        producer: ProducerId,
    ) -> Poll<Result<VideoMode, VideoError>> {
        let state = self.state.lock();
        if let Some((p, err)) = &state.current_error {
            if *p == producer {
                return Poll::Ready(Err(err.clone()));
            }
        }
        if let Some((p, res)) = &state.transition_result {
            if *p == producer {
                if res.is_ok() && (state.renderer_status_dirty || state.renderer_status_checking) {
                    return Poll::Pending;
                }
                return Poll::Ready(res.clone());
            }
        }
        Poll::Pending
    }

    pub(crate) fn owner_wait_predicate(
        state: &MailboxState,
        applied_control_revision: u64,
        has_held_output: bool,
    ) -> bool {
        let drain_outstanding = match state.drain_requested {
            Some(p) => state.drain_completed != Some(p),
            None => false,
        };
        state.retired
            || state.setup_pending
            || state.renderer_status_dirty
            || state.input_slot.is_some()
            || drain_outstanding
            || state.control_revision != applied_control_revision
            || has_held_output
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompressedKind {
    H264,
    Hevc,
}

struct Compressed {
    kind: CompressedKind,
    length_size: usize,
    annex_b: bool,
    format: CFRetained<CMVideoFormatDescription>,
    reorder_depth: usize,
    primed: bool,
    first_irap: bool,
    discard_rasl: bool,
    present_from: Duration,
}

impl Compressed {
    fn filter_leading(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if self.kind != CompressedKind::Hevc {
            return None;
        }
        let picture = split_length_prefixed(data, self.length_size).find_map(|nal| {
            let header = nal.get(..2)?;
            let kind = (header[0] >> 1) & 0x3f;
            (kind <= 31).then_some(kind)
        })?;
        if (16..=23).contains(&picture) {
            self.discard_rasl = self.first_irap || picture <= 20;
            self.first_irap = false;
        }
        if !self.discard_rasl || !matches!(picture, 8 | 9) {
            return None;
        }
        let keep = |nal: &&[u8]| nal.len() < 2 || !matches!((nal[0] >> 1) & 0x3f, 8 | 9);
        let size = split_length_prefixed(data, self.length_size)
            .filter(keep)
            .map(|nal| self.length_size + nal.len())
            .sum();
        let mut kept = Vec::with_capacity(size);
        for nal in split_length_prefixed(data, self.length_size).filter(keep) {
            kept.extend_from_slice(&(nal.len() as u32).to_be_bytes()[4 - self.length_size..]);
            kept.extend_from_slice(nal);
        }
        Some(kept)
    }
}

#[repr(C)]
struct DecompressionCallback {
    callback: unsafe extern "C" fn(
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
        i32,
        u32,
        *mut CVImageBuffer,
        CMTime,
        CMTime,
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
        session: *const CFType,
        sample: *const CMSampleBuffer,
        flags: u32,
        source: *mut std::ffi::c_void,
        info: *mut u32,
    ) -> i32;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: *const CFType) -> i32;
    fn VTDecompressionSessionInvalidate(session: *const CFType);
}

struct NativeDecoder {
    session: CFRetained<CFType>,
    output: Arc<DecodedOutput>,
    /// When true, Drop must not wait/invalidate again (already retired).
    retired: bool,
}

struct DecodedOutput {
    reports: Arc<Mutex<u64>>,
    generation: u64,
    present_from: AtomicU64,
    producer: AtomicU64,
    mailbox: Arc<Mailbox>,
    control: Arc<Mutex<Option<Arc<dyn VideoControl>>>>,
    failed: Arc<Mutex<Option<String>>>,
    format: Mutex<Option<CFRetained<CMVideoFormatDescription>>>,
    reorder_depth: usize,
    pictures: Mutex<Vec<(Duration, SendSync<CFRetained<CMSampleBuffer>>)>>,
}

impl NativeDecoder {
    fn new(format: &CMVideoFormatDescription, output: DecodedOutput) -> Result<Self, SinkError> {
        let output = Arc::new(output);
        let callback = DecompressionCallback {
            callback: decoded_picture,
            context: Arc::as_ptr(&output).cast_mut().cast(),
        };
        let surface = objc2_core_foundation::CFDictionary::<CFType, CFType>::empty();
        let attributes = unsafe {
            cf_dictionary(&[(kCVPixelBufferIOSurfacePropertiesKey, &surface as &CFType)])
        }
        .ok_or_else(|| SinkError::Fallback("could not create decoded pixel attributes".into()))?;
        let mut raw = ptr::null_mut();
        let status = unsafe {
            VTDecompressionSessionCreate(
                ptr::null(),
                format,
                ptr::null(),
                ptr::from_ref(&*attributes).cast(),
                &callback,
                &mut raw,
            )
        };
        if status != 0 || raw.is_null() {
            return Err(SinkError::Fallback(format!(
                "VTDecompressionSessionCreate: {status}"
            )));
        }
        let session = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };
        Ok(Self {
            session,
            output,
            retired: false,
        })
    }

    fn set_producer_control(&self, producer: ProducerId, control: Arc<dyn VideoControl>) {
        self.output.producer.store(producer.0, Ordering::Release);
        *self.output.control.lock() = Some(control);
    }

    fn present_from(&self, start: Duration) {
        self.output.present_from.store(
            u64::try_from(start.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Release,
        );
        let mut pictures = self.output.pictures.lock();
        pictures.retain(|(pts, _)| *pts >= start);
    }

    fn decode(&self, sample: &CMSampleBuffer) -> Result<(), SinkError> {
        let status = unsafe {
            VTDecompressionSessionDecodeFrame(
                &*self.session,
                sample,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(SinkError::Fallback(format!(
                "VTDecompressionSessionDecodeFrame: {status}"
            )))
        }
    }

    fn finish(&self) -> Result<(), SinkError> {
        let status = unsafe { VTDecompressionSessionWaitForAsynchronousFrames(&*self.session) };
        if status != 0 {
            return Err(SinkError::Fallback(format!(
                "VTDecompressionSessionWaitForAsynchronousFrames: {status}"
            )));
        }
        Ok(())
    }

    /// Explicit fallible retirement. Drop remains best-effort last resort.
    fn retire(mut self) -> Result<(), String> {
        let status = unsafe { VTDecompressionSessionWaitForAsynchronousFrames(&*self.session) };
        unsafe { VTDecompressionSessionInvalidate(&*self.session) };
        self.retired = true;
        if status != 0 {
            return Err(format!(
                "VTDecompressionSessionWaitForAsynchronousFrames on retire: {status}"
            ));
        }
        Ok(())
    }

    fn take_failed(&self) -> Option<String> {
        self.output.failed.lock().take()
    }
}

impl Drop for NativeDecoder {
    fn drop(&mut self) {
        if self.retired {
            return;
        }
        // Best-effort last resort; not proof of successful cleanup.
        unsafe {
            let _ = VTDecompressionSessionWaitForAsynchronousFrames(&*self.session);
            VTDecompressionSessionInvalidate(&*self.session);
        }
    }
}

impl DecodedOutput {
    fn sample(
        &self,
        image: &CVImageBuffer,
        pts: CMTime,
        duration: CMTime,
    ) -> Result<CFRetained<CMSampleBuffer>, SinkError> {
        let mut cached = self.format.lock();
        unsafe {
            if cached
                .as_ref()
                .is_none_or(|format| !CMVideoFormatDescriptionMatchesImageBuffer(format, image))
            {
                let mut raw = ptr::null();
                let status = CMVideoFormatDescriptionCreateForImageBuffer(
                    None,
                    image,
                    NonNull::from(&mut raw),
                );
                if status != 0 || raw.is_null() {
                    return Err(SinkError::Fallback(format!(
                        "CMVideoFormatDescriptionCreateForImageBuffer: {status}"
                    )));
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
                None,
                image,
                cached.as_ref().expect("decoded format"),
                NonNull::from(&timing),
                NonNull::from(&mut raw),
            );
            if status != 0 || raw.is_null() {
                return Err(SinkError::Fallback(format!(
                    "CMSampleBufferCreateReadyWithImageBuffer: {status}"
                )));
            }
            Ok(CFRetained::from_raw(NonNull::new_unchecked(raw)))
        }
    }

    fn publish_failure(&self, msg: String) {
        *self.failed.lock() = Some(msg.clone());
        let producer = ProducerId(self.producer.load(Ordering::Acquire));
        let control = self.control.lock().clone();
        if let Some(control) = control {
            // Observation only: no frontend mutex held across user wake path
            // beyond the short mailbox lock inside report_error.
            self.mailbox.report_error(
                producer,
                VideoError::Sink(SinkError::Fallback(msg)),
                &control,
            );
        }
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
    let output = unsafe {
        let context = context.cast::<DecodedOutput>();
        Arc::increment_strong_count(context);
        Arc::from_raw(context)
    };
    let current = output.reports.lock();
    if *current != output.generation {
        return;
    }
    if status != 0 {
        drop(current);
        output.publish_failure(format!("VideoToolbox decoded-picture callback: {status}"));
        return;
    }
    let Some(image) = (unsafe { image.as_ref() }) else {
        return;
    };
    if !timestamp.flags.contains(CMTimeFlags::Valid) || timestamp.timescale <= 0 {
        return;
    }
    let pts = Duration::from_secs_f64(
        (timestamp.value as f64 / f64::from(timestamp.timescale)).max(0.0),
    );
    if pts.as_nanos() < u128::from(output.present_from.load(Ordering::Acquire)) {
        return;
    }
    let sample = match output.sample(image, timestamp, duration) {
        Ok(sample) => SendSync(sample),
        Err(error) => {
            drop(current);
            output.publish_failure(error.to_string());
            return;
        }
    };
    let mut pictures = output.pictures.lock();
    let at = pictures.partition_point(|(time, _)| *time <= pts);
    pictures.insert(at, (pts, sample));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RendererPath {
    Modern,
    LegacyLayer,
}

fn renderer_path(layer: &AVSampleBufferDisplayLayer) -> RendererPath {
    static PATH: std::sync::OnceLock<RendererPath> = std::sync::OnceLock::new();
    *PATH.get_or_init(|| unsafe {
        let responds: bool = objc2::msg_send![
            layer,
            respondsToSelector: objc2::sel!(sampleBufferRenderer)
        ];
        if responds {
            RendererPath::Modern
        } else {
            RendererPath::LegacyLayer
        }
    })
}

fn with_renderer<R>(
    layer: &AVSampleBufferDisplayLayer,
    f: impl FnOnce(&objc2::runtime::ProtocolObject<dyn objc2_av_foundation::AVQueuedSampleBufferRendering>) -> R,
) -> R {
    match renderer_path(layer) {
        RendererPath::Modern => {
            let renderer: Retained<AVSampleBufferVideoRenderer> =
                unsafe { layer.sampleBufferRenderer() };
            let proto: Retained<
                objc2::runtime::ProtocolObject<
                    dyn objc2_av_foundation::AVQueuedSampleBufferRendering,
                >,
            > = objc2::runtime::ProtocolObject::from_retained(renderer);
            f(&proto)
        }
        RendererPath::LegacyLayer => {
            let proto = objc2::runtime::ProtocolObject::from_ref(layer);
            f(proto)
        }
    }
}

#[derive(Clone)]
enum TimingMode {
    None,
    Synchronized(SendSync<Retained<AVSampleBufferRenderSynchronizer>>),
    Own(SendSync<CFRetained<CMTimebase>>),
}

/// Exactly one in-flight main operation; completion is per exact ticket.
pub(crate) struct MainBridgeSlot {
    pub(crate) in_flight: bool,
    pub(crate) active_ticket: u64,
    pub(crate) completed_ticket: Option<u64>,
    pub(crate) ticket_result: Option<Result<(), String>>,
}

impl MainBridgeSlot {
    pub(crate) fn new() -> Self {
        Self {
            in_flight: false,
            active_ticket: 0,
            completed_ticket: None,
            ticket_result: None,
        }
    }

    pub(crate) fn can_begin(&self) -> bool {
        !self.in_flight
    }

    pub(crate) fn begin_operation(&mut self, _producer: ProducerId, _seek_gen: u64) -> u64 {
        assert!(!self.in_flight);
        self.in_flight = true;
        self.active_ticket += 1;
        self.completed_ticket = None;
        self.ticket_result = None;
        self.active_ticket
    }

    pub(crate) fn complete_operation(&mut self, ticket: u64, result: Result<(), String>) {
        if self.in_flight && self.active_ticket == ticket {
            self.in_flight = false;
            self.completed_ticket = Some(ticket);
            self.ticket_result = Some(result);
        }
    }
}

struct HeldSample {
    sample: SendSync<CFRetained<CMSampleBuffer>>,
    pts: Duration,
    producer: ProducerId,
    is_ready_candidate: bool,
    ready: PictureReady,
    seek_gen: u64,
    /// True when this held sample still charges the shared input credit.
    input_credit: bool,
}

/// Release the shared input credit when a charged held sample is discarded
/// (setup/reset/retire/threshold). Centralized so bare `held_output = None`
/// cannot leave `input_held_by_owner` stuck true.
pub(crate) fn drop_held_input_credit(mailbox: &Mailbox, input_credit: bool) {
    if input_credit {
        mailbox.release_input_credit();
    }
}

/// Drop a held sample and release its input credit exactly once when charged.
fn discard_held_output(held: &mut Option<HeldSample>, mailbox: &Mailbox) {
    if let Some(h) = held.take() {
        drop_held_input_credit(mailbox, h.input_credit);
    }
}

pub(crate) struct AppleMain {
    layer: SendSync<Retained<AVSampleBufferDisplayLayer>>,
    main: DispatchRetained<DispatchQueue>,
    slot: Mutex<MainBridgeSlot>,
    slot_condvar: Condvar,
    layer_lease: Arc<Mutex<LayerLeaseState>>,
    sink_id: u64,
    observer: Mutex<
        Option<
            SendSync<
                Retained<
                    objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>,
                >,
            >,
        >,
    >,
    timing_mode: Mutex<TimingMode>,
    /// Native bind completed for this sink.
    is_bound: AtomicBool,
    /// Lease reserved but native bind not yet confirmed.
    lease_reserved: AtomicBool,
    /// True once a native attach side-effect occurred (addRenderer/timebase).
    native_attached: AtomicBool,
}

impl AppleMain {
    pub(crate) fn new(
        layer: Retained<AVSampleBufferDisplayLayer>,
        layer_lease: Arc<Mutex<LayerLeaseState>>,
        sink_id: u64,
    ) -> Self {
        let main = unsafe {
            DispatchRetained::retain(std::ptr::NonNull::from(DispatchQueue::main()))
        };
        Self {
            layer: SendSync(layer),
            main,
            slot: Mutex::new(MainBridgeSlot::new()),
            slot_condvar: Condvar::new(),
            layer_lease,
            sink_id,
            observer: Mutex::new(None),
            timing_mode: Mutex::new(TimingMode::None),
            is_bound: AtomicBool::new(false),
            lease_reserved: AtomicBool::new(false),
            native_attached: AtomicBool::new(false),
        }
    }

    fn work_cancelled(
        control: &Arc<dyn VideoControl>,
        producer: ProducerId,
        seek_gen: u64,
        mailbox: Option<&Mailbox>,
    ) -> bool {
        match mailbox {
            Some(mb) => {
                let state = mb.lock();
                Mailbox::cancel_blocks_native(&state, control, producer, seek_gen)
            }
            None => control.cancelled(producer, seek_gen),
        }
    }

    fn wait_ticket(
        &self,
        ticket: u64,
        control: &Arc<dyn VideoControl>,
        producer: ProducerId,
        seek_gen: u64,
        mailbox: Option<&Mailbox>,
    ) -> Result<(), VideoError> {
        let mut slot = self.slot.lock();
        let mut cancelled = false;
        // Credit stays held until the exact ticket acknowledges, even if cancelled.
        while slot.completed_ticket != Some(ticket) {
            if Self::work_cancelled(control, producer, seek_gen, mailbox) {
                cancelled = true;
            }
            self.slot_condvar
                .wait_for(&mut slot, Duration::from_millis(50));
        }
        let res = slot.ticket_result.take().unwrap_or(Ok(()));
        if cancelled || Self::work_cancelled(control, producer, seek_gen, mailbox) {
            return Err(VideoError::Superseded);
        }
        match res {
            Ok(()) => Ok(()),
            Err(err) if err == "cancelled_at_enqueue" => Err(VideoError::Superseded),
            Err(err) if err == "renderer_not_ready" => {
                Err(VideoError::Sink(SinkError::WouldBlock))
            }
            Err(err) => Err(VideoError::Sink(SinkError::Fallback(err))),
        }
    }

    fn acquire_operation_slot(
        &self,
        control: &Arc<dyn VideoControl>,
        producer: ProducerId,
        seek_gen: u64,
        mailbox: Option<&Mailbox>,
    ) -> Result<u64, VideoError> {
        let mut slot = self.slot.lock();
        while !slot.can_begin() {
            if Self::work_cancelled(control, producer, seek_gen, mailbox) {
                return Err(VideoError::Superseded);
            }
            self.slot_condvar
                .wait_for(&mut slot, Duration::from_millis(50));
        }
        if Self::work_cancelled(control, producer, seek_gen, mailbox) {
            return Err(VideoError::Superseded);
        }
        Ok(slot.begin_operation(producer, seek_gen))
    }

    fn inspect_renderer_status(
        self: &Arc<Self>,
        request: &VideoRequest,
        mailbox: &Arc<Mailbox>,
    ) -> Result<(), VideoError> {
        let ticket = self.acquire_operation_slot(
            &request.control, request.producer, request.seek_generation, Some(mailbox),
        )?;
        let this = Arc::clone(self);
        let request = request.clone();
        let control = Arc::clone(&request.control);
        let mailbox_for_main = Arc::clone(mailbox);
        let producer = request.producer;
        let seek_generation = request.seek_generation;
        self.main.exec_async(move || {
            if Self::work_cancelled(
                &request.control, producer, seek_generation, Some(&mailbox_for_main),
            ) {
                this.complete_ticket(ticket, Err("cancelled renderer observation".into()));
                return;
            }
            // Notifications contain no sample/producer identity. Read the current
            // binding instead of retagging a delayed notification as a new failure.
            let result = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
                let error = match renderer_path(&this.layer) {
                    RendererPath::Modern => {
                        let renderer = this.layer.sampleBufferRenderer();
                        if renderer.status() != AVQueuedSampleBufferRenderingStatus::Failed {
                            return Ok(());
                        }
                        renderer.error()
                    }
                    RendererPath::LegacyLayer => {
                        if this.layer.status() != AVQueuedSampleBufferRenderingStatus::Failed {
                            return Ok(());
                        }
                        this.layer.error()
                    }
                };
                let message = error.map(|error| error.localizedDescription().to_string())
                    .unwrap_or_else(|| "unknown native renderer failure".into());
                Err(format!("current renderer failed: {message}"))
            }));
            this.complete_ticket(ticket, result.unwrap_or_else(|_| {
                Err("Obj-C exception observing renderer status".into())
            }));
        });
        self.wait_ticket(ticket, &control, producer, seek_generation, Some(mailbox))
    }

    /// Cleanup path: wait for the sole credit without a live control identity.
    fn acquire_cleanup_slot(&self) -> u64 {
        let mut slot = self.slot.lock();
        while !slot.can_begin() {
            self.slot_condvar
                .wait_for(&mut slot, Duration::from_millis(50));
        }
        slot.begin_operation(ProducerId(0), 0)
    }

    fn wait_cleanup_ticket(&self, ticket: u64) -> Result<(), String> {
        let mut slot = self.slot.lock();
        while slot.completed_ticket != Some(ticket) {
            self.slot_condvar
                .wait_for(&mut slot, Duration::from_millis(50));
        }
        slot.ticket_result.take().unwrap_or(Ok(()))
    }

    pub(crate) fn complete_ticket(&self, ticket: u64, result: Result<(), String>) {
        let mut slot = self.slot.lock();
        slot.complete_operation(ticket, result);
        drop(slot);
        self.slot_condvar.notify_all();
    }

    fn release_unused_lease(&self) {
        if !self.native_attached.load(Ordering::Acquire)
            && self.lease_reserved.swap(false, Ordering::AcqRel)
            && !self.is_bound.load(Ordering::Acquire)
        {
            self.layer_lease.lock().release_unused_reservation(self.sink_id);
        }
    }

    pub(crate) fn bind(
        self: &Arc<Self>,
        synchronizer: Option<Retained<AVSampleBufferRenderSynchronizer>>,
        _clock: Arc<dyn Clock>,
        mailbox: Arc<Mailbox>,
        control: Arc<dyn VideoControl>,
        producer: ProducerId,
        seek_gen: u64,
        deadline: Instant,
    ) -> Result<(), VideoError> {
        // Reuse the existing binding and clock, including its untagged status observer.
        if self.is_bound.load(Ordering::Acquire)
            && self.layer_lease.lock().is_bound_for(self.sink_id)
        {
            return Ok(());
        }

        loop {
            if control.cancelled(producer, seek_gen) {
                return Err(VideoError::Superseded);
            }
            if control.active_now() >= deadline {
                return Err(VideoError::Sink(SinkError::WouldBlock));
            }
            if self.layer_lease.lock().try_reserve(self.sink_id) {
                self.lease_reserved.store(true, Ordering::Release);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let ticket = match self.acquire_operation_slot(&control, producer, seek_gen, Some(&mailbox)) {
            Ok(t) => t,
            Err(e) => {
                self.release_unused_lease();
                return Err(e);
            }
        };

        if control.cancelled(producer, seek_gen) {
            // Retain credit until we complete; release reservation.
            self.release_unused_lease();
            self.complete_ticket(ticket, Err("cancelled before bind dispatch".into()));
            return Err(VideoError::Superseded);
        }
        if control.active_now() >= deadline {
            self.release_unused_lease();
            self.complete_ticket(ticket, Err("deadline before bind dispatch".into()));
            return Err(VideoError::Sink(SinkError::WouldBlock));
        }

        let layer_for_bind = SendSync(self.layer.0.clone());
        let this = Arc::clone(self);
        let mailbox_for_obs = Arc::clone(&mailbox);
        let control_for_obs = Arc::clone(&control);
        let old_observer = self.observer.lock().take();
        let mut synchronizer = SendSync(synchronizer);

        let deadline_for_main = deadline;

        self.main.exec_async(move || {
            // Execution-time identity / cancel / deadline guards.
            if control_for_obs.cancelled(producer, seek_gen) {
                this.release_unused_lease();
                this.complete_ticket(ticket, Err("cancelled at bind execution".into()));
                return;
            }
            if control_for_obs.active_now() >= deadline_for_main {
                this.release_unused_lease();
                this.complete_ticket(ticket, Err("deadline at bind execution".into()));
                return;
            }

            // Record cleanup ownership before entering any attaching native call.
            let res = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
                // Remove previous observer before replacing.
                if let Some(obs) = old_observer {
                    NSNotificationCenter::defaultCenter().removeObserver(obs.0.as_ref());
                }

                layer_for_bind.setVideoGravity(
                    AVLayerVideoGravityResizeAspect
                        .expect("AVLayerVideoGravityResizeAspect is documented"),
                );
                match synchronizer.take() {
                    Some(sync) => {
                        *this.timing_mode.lock() = TimingMode::Synchronized(SendSync(sync.clone()));
                        this.native_attached.store(true, Ordering::Release);
                        with_renderer(&layer_for_bind, |renderer| sync.addRenderer(renderer));
                    }
                    None => {
                        let mut raw: *mut CMTimebase = ptr::null_mut();
                        #[allow(deprecated)]
                        let status = CMTimebase::create_with_master_clock(
                            None,
                            &CMClock::host_time_clock(),
                            NonNull::from(&mut raw),
                        );
                        if status != 0 || raw.is_null() {
                            return Err(format!("CMTimebaseCreateWithMasterClock: {status}"));
                        }
                        let timebase = CFRetained::from_raw(NonNull::new_unchecked(raw));
                        let status = timebase.set_rate(0.0);
                        if status != 0 {
                            return Err(format!("CMTimebaseSetRate: {status}"));
                        }
                        *this.timing_mode.lock() = TimingMode::Own(SendSync(timebase.clone()));
                        this.native_attached.store(true, Ordering::Release);
                        layer_for_bind.setControlTimebase(Some(&timebase));
                    }
                };

                let observer = NSNotificationCenter::defaultCenter()
                    .addObserverForName_object_queue_usingBlock(
                        Some(
                            objc2_av_foundation::AVSampleBufferDisplayLayerFailedToDecodeNotification,
                        ),
                        Some(layer_for_bind.as_ref()),
                        None,
                        &block2::RcBlock::new(move |_note: NonNull<NSNotification>| {
                            mailbox_for_obs.renderer_status_changed();
                        }),
                    );
                Ok(observer)
            }));

            match res {
                Ok(Ok(obs)) => {
                    *this.observer.lock() = Some(SendSync(obs));
                    this.layer_lease.lock().confirm_bound(this.sink_id);
                    this.lease_reserved.store(false, Ordering::Release);
                    this.is_bound.store(true, Ordering::Release);
                    this.complete_ticket(ticket, Ok(()));
                }
                Ok(Err(err)) => {
                    if this.native_attached.load(Ordering::Acquire) {
                        // Side effect occurred: quarantine, do not free as unused.
                        this.layer_lease.lock().finish_unbind(this.sink_id, false);
                        this.lease_reserved.store(false, Ordering::Release);
                    } else {
                        this.lease_reserved.store(false, Ordering::Release);
                        this.layer_lease.lock().release_unused_reservation(this.sink_id);
                    }
                    this.complete_ticket(ticket, Err(err));
                }
                Err(_) => {
                    if this.native_attached.load(Ordering::Acquire) {
                        this.layer_lease.lock().finish_unbind(this.sink_id, false);
                        this.lease_reserved.store(false, Ordering::Release);
                    } else {
                        this.lease_reserved.store(false, Ordering::Release);
                        this.layer_lease.lock().release_unused_reservation(this.sink_id);
                    }
                    this.complete_ticket(
                        ticket,
                        Err("Obj-C exception binding display layer".into()),
                    );
                }
            }
        });

        match self.wait_ticket(ticket, &control, producer, seek_gen, Some(&mailbox)) {
            Ok(()) => Ok(()),
            Err(e) => {
                // If wait saw cancellation after dispatch, credit already released by ack.
                if !self.is_bound.load(Ordering::Acquire) {
                    if self.native_attached.load(Ordering::Acquire) {
                        // Partial native attach: quarantine rather than free.
                        self.layer_lease.lock().finish_unbind(self.sink_id, false);
                        self.lease_reserved.store(false, Ordering::Release);
                    } else {
                        self.release_unused_lease();
                    }
                }
                Err(e)
            }
        }
    }

    pub(crate) fn flush(
        self: &Arc<Self>,
        remove_displayed_image: bool,
        control: &Arc<dyn VideoControl>,
        producer: ProducerId,
        seek_gen: u64,
        deadline: Instant,
    ) -> Result<(), VideoError> {
        let ticket = self.acquire_operation_slot(control, producer, seek_gen, None)?;
        let layer_for_flush = SendSync(self.layer.0.clone());
        let this = Arc::clone(self);
        let control_flush = Arc::clone(control);

        self.main.exec_async(move || {
            if control_flush.cancelled(producer, seek_gen) {
                this.complete_ticket(ticket, Err("cancelled at flush execution".into()));
                return;
            }
            if control_flush.active_now() >= deadline {
                this.complete_ticket(ticket, Err("deadline at flush execution".into()));
                return;
            }
            let res = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                match renderer_path(&layer_for_flush) {
                    RendererPath::Modern => {
                        let renderer: Retained<AVSampleBufferVideoRenderer> =
                            unsafe { layer_for_flush.sampleBufferRenderer() };
                        let responds: bool = unsafe {
                            objc2::msg_send![
                                &*renderer,
                                respondsToSelector: objc2::sel!(flushWithRemovalOfDisplayedImage:completionHandler:)
                            ]
                        };
                        if responds {
                            let this_cb = Arc::clone(&this);
                            let block = block2::RcBlock::new(move || {
                                this_cb.complete_ticket(ticket, Ok(()));
                            });
                            unsafe {
                                let _: () = objc2::msg_send![
                                    &*renderer,
                                    flushWithRemovalOfDisplayedImage: remove_displayed_image,
                                    completionHandler: &*block
                                ];
                            }
                            return;
                        }
                        if remove_displayed_image {
                            this.complete_ticket(ticket, Err("renderer cannot acknowledge image removal".into()));
                            return;
                        }
                        unsafe {
                            let _: () = objc2::msg_send![&*renderer, flush];
                        }
                        this.complete_ticket(ticket, Ok(()));
                    }
                    RendererPath::LegacyLayer => {
                        unsafe {
                            if remove_displayed_image {
                                let _: () = objc2::msg_send![&*layer_for_flush, flushAndRemoveImage];
                            } else {
                                let _: () = objc2::msg_send![&*layer_for_flush, flush];
                            }
                        }
                        this.complete_ticket(ticket, Ok(()));
                    }
                }
            }));
            if res.is_err() {
                this.complete_ticket(ticket, Err("Obj-C exception flushing renderer".into()));
            }
        });

        self.wait_ticket(ticket, control, producer, seek_gen, None)
    }

    /// Enqueue a decoded sample. On renderer capacity exhaustion the sample is
    /// returned via `returned` so the owner retains the single output credit.
    pub(crate) fn enqueue_sample(
        self: &Arc<Self>,
        sample: SendSync<CFRetained<CMSampleBuffer>>,
        pts: Duration,
        producer: ProducerId,
        is_ready_candidate: bool,
        ready: PictureReady,
        mailbox: Arc<Mailbox>,
        control: Arc<dyn VideoControl>,
        seek_gen: u64,
        returned: Arc<Mutex<Option<SendSync<CFRetained<CMSampleBuffer>>>>>,
    ) -> Result<(), VideoError> {
        if !mailbox.wait_for_published_request(&control, producer, seek_gen) {
            return Err(VideoError::Superseded);
        }
        let ticket = match self.acquire_operation_slot(&control, producer, seek_gen, Some(&mailbox)) {
            Ok(ticket) => ticket,
            Err(error) => {
                *returned.lock() = Some(sample);
                return Err(error);
            }
        };
        let layer_for_enqueue = SendSync(self.layer.0.clone());
        let this = Arc::clone(self);
        let control_cb = Arc::clone(&control);
        let mailbox_cb = Arc::clone(&mailbox);
        let ready_cb = ready.clone();
        let returned_cb = Arc::clone(&returned);

        self.main.exec_async(move || {
            // Cancellation / retirement / producer checks at the native boundary.
            // Authorized predecessor work remains valid after latest_producer advances.
            {
                let state = mailbox_cb.lock();
                if !Mailbox::allows_native_work(&state, producer)
                    || Mailbox::cancel_blocks_native(&state, &control_cb, producer, seek_gen)
                {
                    drop(state);
                    *returned_cb.lock() = Some(sample);
                    this.complete_ticket(ticket, Err("cancelled_at_enqueue".into()));
                    return;
                }
                // Recheck present_from threshold immediately before native enqueue.
                if pts < state.present_from {
                    drop(state);
                    this.complete_ticket(ticket, Ok(()));
                    return;
                }
            }

            let capacity = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                with_renderer(&layer_for_enqueue, |renderer| unsafe {
                    let ready_now: bool = objc2::msg_send![renderer, isReadyForMoreMediaData];
                    ready_now
                })
            }));
            let ready_now = match capacity {
                Ok(v) => v,
                Err(_) => {
                    this.complete_ticket(
                        ticket,
                        Err("Obj-C exception reading renderer capacity".into()),
                    );
                    return;
                }
            };
            if !ready_now {
                *returned_cb.lock() = Some(sample);
                this.complete_ticket(ticket, Err("renderer_not_ready".into()));
                return;
            }

            let enqueued = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                with_renderer(&layer_for_enqueue, |renderer| unsafe {
                    let _: () = objc2::msg_send![renderer, enqueueSampleBuffer: &**sample];
                });
            }));

            if enqueued.is_ok() {
                {
                    let mut state = mailbox_cb.lock();
                    state.frames_enqueued += 1;
                    state.last_enqueued_producer = Some(producer);
                }
                // Readiness observation outside the mailbox mutex.
                if is_ready_candidate {
                    ready_cb.ready(pts);
                }
                this.complete_ticket(ticket, Ok(()));
            } else {
                let err_msg = "Obj-C exception enqueueing decoded sample".to_string();
                mailbox_cb.report_error(
                    producer,
                    VideoError::Sink(SinkError::Fallback(err_msg.clone())),
                    &control_cb,
                );
                this.complete_ticket(ticket, Err(err_msg));
            }
        });

        self.wait_ticket(ticket, &control, producer, seek_gen, Some(&mailbox))
    }

    /// Rate / anchor change uses the same single main-bridge credit.
    pub(crate) fn set_playing(
        self: &Arc<Self>,
        playing: bool,
        clock_now: Option<Duration>,
        control: &Arc<dyn VideoControl>,
        producer: ProducerId,
        seek_gen: u64,
    ) -> Result<(), VideoError> {
        let ticket = self.acquire_operation_slot(control, producer, seek_gen, None)?;
        let rate = if playing { 1.0f64 } else { 0.0f64 };
        // Move/clone timing under lock briefly; native work stays on main.
        let timing_mode = self.timing_mode.lock().clone();
        let this = Arc::clone(self);
        let control_cb = Arc::clone(control);

        self.main.exec_async(move || {
            if control_cb.cancelled(producer, seek_gen) {
                this.complete_ticket(ticket, Err("cancelled at set_playing".into()));
                return;
            }
            let res = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
                match timing_mode {
                    TimingMode::Synchronized(sync) => {
                        if let Some(now) = clock_now {
                            let time = cm_time_from_duration(now, 1_000_000_000);
                            let _: () =
                                objc2::msg_send![&*sync.0, setRate: rate as f32, time: time];
                        } else {
                            sync.0.setRate(rate as f32);
                        }
                    }
                    TimingMode::Own(timebase) => {
                        let status = if let Some(now) = clock_now {
                            let host_now = CMClock::host_time_clock().time();
                            timebase.set_rate_and_anchor_time(
                                rate,
                                cm_time_from_duration(now, 1_000_000_000),
                                host_now,
                            )
                        } else {
                            timebase.set_rate(rate)
                        };
                        if status != 0 {
                            return Err(format!("CMTimebase set_rate status: {status}"));
                        }
                    }
                    TimingMode::None => {}
                }
                Ok(())
            }));
            match res {
                Ok(Ok(())) => this.complete_ticket(ticket, Ok(())),
                Ok(Err(err)) => this.complete_ticket(ticket, Err(err)),
                Err(_) => this.complete_ticket(
                    ticket,
                    Err("Obj-C exception applying playback rate".into()),
                ),
            }
        });

        self.wait_ticket(ticket, control, producer, seek_gen, None)
    }

    pub(crate) fn unbind(self: &Arc<Self>) -> Result<(), String> {
        // Unused reservation only: release without pretending native unbind succeeded.
        if !self.is_bound.load(Ordering::Acquire) {
            if self.native_attached.load(Ordering::Acquire) {
                return Err("partial native attachment remains quarantined".into());
            }
            self.release_unused_lease();
            return Ok(());
        }

        self.layer_lease.lock().start_unbind(self.sink_id);
        let ticket = self.acquire_cleanup_slot();

        let layer_for_unbind = SendSync(self.layer.0.clone());
        // Move native-owned fields out so Drop does not release them from the frontend.
        let timing_mode = std::mem::replace(&mut *self.timing_mode.lock(), TimingMode::None);
        let observer = self.observer.lock().take();
        let this = Arc::clone(self);

        self.main.exec_async(move || {
            let res = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
                if let Some(obs) = observer {
                    NSNotificationCenter::defaultCenter().removeObserver(obs.0.as_ref());
                }

                match timing_mode {
                    TimingMode::Synchronized(sync) => {
                        let this_cb = Arc::clone(&this);
                        let block = block2::RcBlock::new(move |did_remove: objc2::runtime::Bool| {
                            if did_remove.as_bool() {
                                this_cb
                                    .layer_lease
                                    .lock()
                                    .finish_unbind(this_cb.sink_id, true);
                                this_cb.is_bound.store(false, Ordering::Release);
                                this_cb.native_attached.store(false, Ordering::Release);
                                this_cb.complete_ticket(ticket, Ok(()));
                            } else {
                                // didRemoveRenderer=false is cleanup failure, not retired.
                                this_cb
                                    .layer_lease
                                    .lock()
                                    .finish_unbind(this_cb.sink_id, false);
                                this_cb.complete_ticket(
                                    ticket,
                                    Err("removeRenderer returned didRemoveRenderer=false".into()),
                                );
                            }
                        });
                        with_renderer(&layer_for_unbind, |renderer| {
                            sync.0.removeRenderer_atTime_completionHandler(
                                renderer,
                                objc2_core_media::kCMTimeInvalid,
                                Some(&block),
                            );
                        });
                    }
                    TimingMode::Own(timebase) => {
                        // Clear layer, then release native ownership before acknowledgement.
                        layer_for_unbind.setControlTimebase(None);
                        drop(timebase);
                        this.layer_lease.lock().finish_unbind(this.sink_id, true);
                        this.is_bound.store(false, Ordering::Release);
                        this.native_attached.store(false, Ordering::Release);
                        this.complete_ticket(ticket, Ok(()));
                    }
                    TimingMode::None => {
                        this.layer_lease.lock().finish_unbind(this.sink_id, true);
                        this.is_bound.store(false, Ordering::Release);
                        this.complete_ticket(ticket, Ok(()));
                    }
                }
            }));
            if res.is_err() {
                this.layer_lease.lock().finish_unbind(this.sink_id, false);
                this.complete_ticket(ticket, Err("Obj-C exception during unbind".into()));
            }
        });

        self.wait_cleanup_ticket(ticket)
    }
}

struct SoftwarePath {
    src_format: PixelFormat,
    width: u32,
    height: u32,
    dst_ostype: u32,
    pool: SendSync<CFRetained<CVPixelBufferPool>>,
}

enum AppleDevice {
    Compressed {
        compressed: Compressed,
        decoder: Option<NativeDecoder>,
        ready: PictureReady,
        producer: ProducerId,
    },
    Frames {
        software: SoftwarePath,
        ready: PictureReady,
        producer: ProducerId,
    },
}

impl AppleDevice {
    fn compressed(
        params: &CodecParameters,
        present_from: Duration,
        ready: PictureReady,
        producer: ProducerId,
        mailbox: &Arc<Mailbox>,
        control: Arc<dyn VideoControl>,
        reports: &Arc<Mutex<u64>>,
        reports_generation: u64,
    ) -> Result<Self, VideoError> {
        let kind = match params.codec_id.as_str() {
            "h264" => CompressedKind::H264,
            "hevc" | "h265" => CompressedKind::Hevc,
            _ => return Err(VideoError::Unsupported),
        };
        if params.extradata.is_empty() {
            return Err(VideoError::Unsupported);
        }
        let (format, length_size, annex_b, reorder_depth) =
            create_h264_or_hevc_format(kind, params).map_err(|e| match e {
                SinkError::Fallback(_) => VideoError::Unsupported,
                other => VideoError::Sink(other),
            })?;

        let mut compressed = Compressed {
            kind,
            length_size,
            annex_b,
            format,
            reorder_depth,
            primed: false,
            first_irap: true,
            discard_rasl: false,
            present_from,
        };

        // Create VT during compressed configuration, BEFORE publishing Compressed.
        let decoder = NativeDecoder::new(
            &compressed.format,
            DecodedOutput {
                reports: Arc::clone(reports),
                generation: reports_generation,
                present_from: AtomicU64::new(
                    u64::try_from(present_from.as_nanos()).unwrap_or(u64::MAX),
                ),
                producer: AtomicU64::new(producer.0),
                mailbox: Arc::clone(mailbox),
                control: Arc::new(Mutex::new(Some(control))),
                failed: Arc::new(Mutex::new(None)),
                format: Mutex::new(None),
                reorder_depth: compressed.reorder_depth,
                pictures: Mutex::new(Vec::new()),
            },
        )
        .map_err(VideoError::Sink)?;

        let _ = &mut compressed; // keep mut for later primed flags
        Ok(Self::Compressed {
            compressed,
            decoder: Some(decoder),
            ready,
            producer,
        })
    }

    fn frames(
        params: &CodecParameters,
        ready: PictureReady,
        producer: ProducerId,
    ) -> Result<Self, VideoError> {
        let width = params.width.ok_or_else(|| {
            VideoError::Sink(SinkError::Fatal(
                "software video stream without width".into(),
            ))
        })?;
        let height = params.height.ok_or_else(|| {
            VideoError::Sink(SinkError::Fatal(
                "software video stream without height".into(),
            ))
        })?;
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return Err(VideoError::Sink(SinkError::Fatal(format!(
                "unsupported video dimensions {width}x{height}"
            ))));
        }
        let src_format = params.pixel_format.unwrap_or(PixelFormat::Yuv420P);
        let dst_ostype = match src_format {
            PixelFormat::Yuv420P10Le | PixelFormat::Yuv422P10Le | PixelFormat::Yuv444P10Le => {
                kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
            }
            _ => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        };

        let pix_fmt_num = CFNumber::new_i32(dst_ostype as i32);
        let width_num = CFNumber::new_i32(width as i32);
        let height_num = CFNumber::new_i32(height as i32);
        let iosurface_empty: CFRetained<objc2_core_foundation::CFDictionary> = unsafe {
            CFRetained::cast_unchecked(
                objc2_core_foundation::CFDictionary::<
                    objc2_core_foundation::CFType,
                    objc2_core_foundation::CFType,
                >::empty(),
            )
        };
        let buf_attrs = unsafe {
            cf_dictionary(&[
                (
                    kCVPixelBufferPixelFormatTypeKey,
                    &pix_fmt_num as &objc2_core_foundation::CFType,
                ),
                (
                    kCVPixelBufferWidthKey,
                    &width_num as &objc2_core_foundation::CFType,
                ),
                (
                    kCVPixelBufferHeightKey,
                    &height_num as &objc2_core_foundation::CFType,
                ),
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
            return Err(VideoError::Sink(SinkError::Fatal(format!(
                "CVPixelBufferPoolCreate: {cvret}"
            ))));
        }
        let pool = unsafe { CFRetained::from_raw(NonNull::new_unchecked(pool_raw)) };

        Ok(Self::Frames {
            software: SoftwarePath {
                src_format,
                width,
                height,
                dst_ostype,
                pool: SendSync(pool),
            },
            ready,
            producer,
        })
    }

    fn producer(&self) -> ProducerId {
        match self {
            Self::Compressed { producer, .. } => *producer,
            Self::Frames { producer, .. } => *producer,
        }
    }

    fn retire(self) -> Result<(), String> {
        match self {
            Self::Compressed { decoder, .. } => {
                if let Some(dec) = decoder {
                    dec.retire()?;
                }
            }
            Self::Frames { .. } => {}
        }
        Ok(())
    }

    fn take_vt_failure(&self) -> Option<(ProducerId, String)> {
        match self {
            Self::Compressed {
                decoder: Some(dec),
                producer,
                ..
            } => dec.take_failed().map(|m| (*producer, m)),
            _ => None,
        }
    }
}

pub struct AppleVideoSink {
    sink_id: u64,
    layer: SendSync<Retained<AVSampleBufferDisplayLayer>>,
    synchronizer: Option<Retained<AVSampleBufferRenderSynchronizer>>,
    clock: Arc<dyn Clock>,
    mailbox: Arc<Mailbox>,
    main_bridge: Arc<AppleMain>,
    owner_ticket: Option<OwnerTicket>,
    cached_output: VideoOutput,
}

unsafe impl Send for AppleVideoSink {}
unsafe impl Sync for AppleVideoSink {}

impl AppleVideoSink {
    pub(crate) fn new(
        layer: Retained<AVSampleBufferDisplayLayer>,
        synchronizer: Option<Retained<AVSampleBufferRenderSynchronizer>>,
        clock: Arc<dyn Clock>,
        layer_lease: Arc<Mutex<LayerLeaseState>>,
    ) -> Self {
        let sink_id = SINK_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mailbox = Arc::new(Mailbox::new());
        let main_bridge = Arc::new(AppleMain::new(layer.clone(), layer_lease, sink_id));

        Self {
            sink_id,
            layer: SendSync(layer),
            synchronizer,
            clock,
            mailbox,
            main_bridge,
            owner_ticket: None,
            cached_output: VideoOutput {
                revision: sink_id,
                available: true,
            },
        }
    }

    fn spawn_owner(&self, control: &Arc<dyn VideoControl>) -> Result<OwnerTicket, OwnerError> {
        let mailbox = Arc::clone(&self.mailbox);
        let main_bridge = Arc::clone(&self.main_bridge);
        let synchronizer = SendSync(self.synchronizer.clone());
        let clock = Arc::clone(&self.clock);
        let reports = Arc::new(Mutex::new(0u64));
        let wake_engine = {
            let ctrl = Arc::clone(control);
            Arc::new(move || {
                ctrl.wake();
            })
        };

        let run = move || -> Result<(), String> {
            owner_loop(
                mailbox,
                main_bridge,
                synchronizer,
                clock,
                reports,
            )
        };

        video_owner::try_spawn(run, wake_engine)
    }
}

fn owner_loop(
    mailbox: Arc<Mailbox>,
    main_bridge: Arc<AppleMain>,
    synchronizer: SendSync<Option<Retained<AVSampleBufferRenderSynchronizer>>>,
    clock: Arc<dyn Clock>,
    reports: Arc<Mutex<u64>>,
) -> Result<(), String> {
    let mut device: Option<AppleDevice> = None;
    let mut active_request: Option<VideoRequest> = None;
    let mut applied_control_revision: u64 = 0;
    let mut applied_playing: Option<bool> = None;
    let mut force_playing_reapply = false;
    let mut applied_present_from: Option<Duration> = None;
    let mut reports_generation = 0u64;
    // Already-accepted predecessor authorized under a reset=false preserving request.
    let mut authorized_predecessor: Option<ProducerId> = None;
    // Deferred same-seek reset=false request; device install waits for predecessor completion.
    let mut pending_replacement: Option<VideoRequest> = None;
    // Bounded held output while renderer is not ready.
    let mut held_output: Option<HeldSample> = None;
    // Drain completed locally for producer (mailbox also records it).
    let mut local_drain_done: Option<ProducerId> = None;
    /// VT finish() completed once per producer (separate from tail delivery).
    let mut native_finish_done: Option<ProducerId> = None;

    loop {
        // Publish VT failures that arrived asynchronously.
        if let Some(dev) = device.as_ref() {
            if let Some((p, msg)) = dev.take_vt_failure() {
                if let Some(req) = &active_request {
                    mailbox.report_error(
                        p,
                        VideoError::Sink(SinkError::Fallback(msg)),
                        &req.control,
                    );
                }
            }
        }

        let (setup, drain, retired, control_snap) = {
            let mut state = mailbox.lock();
            if !Mailbox::owner_wait_predicate(
                &state,
                applied_control_revision,
                held_output.is_some(),
            ) {
                // Timed wait so VT failure cells still progress without a wake.
                mailbox
                    .condvar
                    .wait_for(&mut state, Duration::from_millis(50));
            }
            if state.retired {
                (None, None, true, None)
            } else {
                let setup = if state.setup_pending {
                    state.setup_pending = false;
                    state.desired_request.clone()
                } else {
                    None
                };
                let drain = state.drain_requested;
                let control_snap = Some((
                    state.control_revision,
                    state.playing,
                    state.present_from,
                    state.latest_producer,
                ));
                (setup, drain, false, control_snap)
            }
        };

        if retired {
            break;
        }

        // Apply coalesced control-only updates after bind, via the main credit.
        // Revision is marked applied only after successful native acknowledgement.
        if let Some((rev, playing, present_from, latest)) = control_snap {
            let successor_controls = active_request.as_ref().and_then(|active| {
                let state = mailbox.lock();
                Mailbox::preserving_successor(&state, active.producer).cloned()
            });
            if let Some(req) = successor_controls.as_ref().or(active_request.as_ref()) {
                if latest == Some(req.producer)
                    && main_bridge.is_bound.load(Ordering::Acquire)
                {
                    let need_present = applied_present_from != Some(present_from);
                    let need_playing = applied_playing != Some(playing)
                        || force_playing_reapply;
                    if rev != applied_control_revision || need_present || need_playing {
                        if need_present {
                            applied_present_from = Some(present_from);
                            if let Some(AppleDevice::Compressed {
                                compressed,
                                decoder,
                                ..
                            }) = device.as_mut()
                            {
                                compressed.present_from = present_from;
                                if let Some(dec) = decoder.as_ref() {
                                    dec.present_from(present_from);
                                }
                            }
                            if held_output.as_ref().is_some_and(|h| h.pts < present_from) {
                                discard_held_output(&mut held_output, &mailbox);
                            }
                        }
                        if need_playing {
                            match main_bridge.set_playing(
                                playing,
                                clock.now(),
                                &req.control,
                                req.producer,
                                req.seek_generation,
                            ) {
                                Ok(()) => {
                                    applied_playing = Some(playing);
                                    force_playing_reapply = false;
                                }
                                Err(VideoError::Superseded) => {}
                                Err(e) => {
                                    mailbox.report_error(req.producer, e, &req.control);
                                }
                            }
                        }
                        // Do not consume a failed or superseded native rate update.
                        if rev != applied_control_revision && applied_playing == Some(playing) {
                            applied_control_revision = rev;
                        }
                    }
                }
            }
        }

        // Process lifecycle setup.
        if let Some(req) = setup {
            if req.control.cancelled(req.producer, req.seek_generation) {
                mailbox.lock().transition_result =
                    Some((req.producer, Err(VideoError::Superseded)));
                discard_held_output(&mut held_output, &mailbox);
                req.control.wake();
                continue;
            }

            match &req.target {
                VideoTarget::Retired => {
                    if let Some(dev) = device.take() {
                        dev.retire()?;
                    }
                    main_bridge.unbind()?;
                    mailbox.lock().transition_result =
                        Some((req.producer, Ok(VideoMode::Retired)));
                    active_request = None;
                    authorized_predecessor = None;
                    discard_held_output(&mut held_output, &mailbox);
                    req.control.wake();
                    break;
                }
                VideoTarget::Compressed {
                    params,
                    ready,
                    present_from: p_from,
                } => {
                    authorized_predecessor = None;
                    pending_replacement = None;
                    mailbox.clear_authorized_predecessor();
                    discard_held_output(&mut held_output, &mailbox);
                    if let Some(dev) = device.take() {
                        dev.retire()?;
                    }
                    native_finish_done = None;
                    local_drain_done = None;
                    *reports.lock() += 1;
                    reports_generation = *reports.lock();

                    if let Err(e) = main_bridge.bind(
                        synchronizer.0.clone(),
                        Arc::clone(&clock),
                        Arc::clone(&mailbox),
                        Arc::clone(&req.control),
                        req.producer,
                        req.seek_generation,
                        req.deadline,
                    ) {
                        mailbox.lock().transition_result = Some((req.producer, Err(e)));
                        mailbox.release_input_credit();
                        req.control.wake();
                        continue;
                    }
                    if let Err(e) = main_bridge.flush(
                        true,
                        &req.control,
                        req.producer,
                        req.seek_generation,
                        req.deadline,
                    ) {
                        mailbox.report_error(req.producer, e, &req.control);
                        mailbox.lock().transition_result = Some((
                            req.producer,
                            Err(VideoError::Sink(SinkError::Fallback(
                                "flush failed during compressed setup".into(),
                            ))),
                        ));
                        req.control.wake();
                        continue;
                    }

                    // Deadline / cancel after main acknowledgements, before publish.
                    if req.control.cancelled(req.producer, req.seek_generation) {
                        mailbox.lock().transition_result =
                            Some((req.producer, Err(VideoError::Superseded)));
                        req.control.wake();
                        continue;
                    }
                    if req.control.active_now() >= req.deadline {
                        mailbox.lock().transition_result = Some((
                            req.producer,
                            Err(VideoError::Sink(SinkError::WouldBlock)),
                        ));
                        req.control.wake();
                        continue;
                    }

                    match AppleDevice::compressed(
                        params,
                        *p_from,
                        ready.clone(),
                        req.producer,
                        &mailbox,
                        Arc::clone(&req.control),
                        &reports,
                        reports_generation,
                    ) {
                        Ok(new_dev) => {
                            // Final deadline check after VT create, before publish.
                            if req.control.cancelled(req.producer, req.seek_generation) {
                                new_dev.retire()?;
                                mailbox.lock().transition_result =
                                    Some((req.producer, Err(VideoError::Superseded)));
                            } else if req.control.active_now() >= req.deadline {
                                new_dev.retire()?;
                                mailbox.lock().transition_result = Some((
                                    req.producer,
                                    Err(VideoError::Sink(SinkError::WouldBlock)),
                                ));
                            } else {
                                device = Some(new_dev);
                                active_request = Some(req.clone());
                                // Seed present_from only from mailbox (set at submit);
                                // never restore immutable request over a later control.
                                let seeded = mailbox.lock().present_from;
                                applied_present_from = Some(seeded);
                                applied_playing = None;
                                force_playing_reapply = true;
                                let mut state = mailbox.lock();
                                state.current_producer = Some(req.producer);
                                state.configured_mode = Some(VideoMode::Compressed);
                                state.transition_result =
                                    Some((req.producer, Ok(VideoMode::Compressed)));
                            }
                        }
                        Err(e) => {
                            mailbox.lock().transition_result = Some((req.producer, Err(e)));
                            mailbox.release_input_credit();
                        }
                    }
                    req.control.wake();
                }
                VideoTarget::Frames {
                    params,
                    ready,
                    reset,
                } => {
                    if *reset {
                        authorized_predecessor = None;
                        pending_replacement = None;
                        mailbox.clear_authorized_predecessor();
                        discard_held_output(&mut held_output, &mailbox);
                        if let Some(dev) = device.take() {
                            dev.retire()?;
                        }
                        *reports.lock() += 1;
                        reports_generation = *reports.lock();

                        if let Err(e) = main_bridge.bind(
                            synchronizer.0.clone(),
                            Arc::clone(&clock),
                            Arc::clone(&mailbox),
                            Arc::clone(&req.control),
                            req.producer,
                            req.seek_generation,
                            req.deadline,
                        ) {
                            mailbox.lock().transition_result = Some((req.producer, Err(e)));
                            mailbox.release_input_credit();
                            req.control.wake();
                            continue;
                        }
                        if let Err(e) = main_bridge.flush(
                            true,
                            &req.control,
                            req.producer,
                            req.seek_generation,
                            req.deadline,
                        ) {
                            mailbox.lock().transition_result = Some((req.producer, Err(e)));
                            req.control.wake();
                            continue;
                        }

                        if req.control.cancelled(req.producer, req.seek_generation) {
                            mailbox.lock().transition_result =
                                Some((req.producer, Err(VideoError::Superseded)));
                            req.control.wake();
                            continue;
                        }
                        if req.control.active_now() >= req.deadline {
                            mailbox.lock().transition_result = Some((
                                req.producer,
                                Err(VideoError::Sink(SinkError::WouldBlock)),
                            ));
                            req.control.wake();
                            continue;
                        }

                        match AppleDevice::frames(params, ready.clone(), req.producer) {
                            Ok(new_dev) => {
                                // Final cancel/deadline after native frames config.
                                if req.control.cancelled(req.producer, req.seek_generation) {
                                    new_dev.retire()?;
                                    mailbox.lock().transition_result =
                                        Some((req.producer, Err(VideoError::Superseded)));
                                } else if req.control.active_now() >= req.deadline {
                                    new_dev.retire()?;
                                    mailbox.lock().transition_result = Some((
                                        req.producer,
                                        Err(VideoError::Sink(SinkError::WouldBlock)),
                                    ));
                                } else {
                                    device = Some(new_dev);
                                    active_request = Some(req.clone());
                                    applied_playing = None;
                                    force_playing_reapply = true;
                                    let mut state = mailbox.lock();
                                    state.current_producer = Some(req.producer);
                                    state.configured_mode = Some(VideoMode::Frames);
                                    state.authorized_predecessor = None;
                                    state.transition_result =
                                        Some((req.producer, Ok(VideoMode::Frames)));
                                }
                            }
                            Err(e) => {
                                mailbox.lock().transition_result = Some((req.producer, Err(e)));
                                mailbox.release_input_credit();
                            }
                        }
                        req.control.wake();
                    } else if let Some(prev) = active_request.as_ref() {
                        if prev.seek_generation == req.seek_generation {
                            // reset=false same seek: authorize predecessor, keep old device,
                            // defer install until accepted old input/output completes.
                            authorized_predecessor = Some(prev.producer);
                            pending_replacement = Some(req.clone());
                            // Do not bind/flush/retire/install here. Input processing below
                            // still sees the old device; try_commit_replacement installs later.
                            req.control.wake();
                        } else {
                            // Different seek under Frames target: full reset semantics.
                            authorized_predecessor = None;
                            pending_replacement = None;
                            mailbox.clear_authorized_predecessor();
                            discard_held_output(&mut held_output, &mailbox);
                            if let Some(dev) = device.take() {
                                dev.retire()?;
                            }
                            if let Err(e) = main_bridge.bind(
                                synchronizer.0.clone(),
                                Arc::clone(&clock),
                                Arc::clone(&mailbox),
                                Arc::clone(&req.control),
                                req.producer,
                                req.seek_generation,
                                req.deadline,
                            ) {
                                mailbox.lock().transition_result = Some((req.producer, Err(e)));
                                req.control.wake();
                                continue;
                            }
                            if let Err(e) = main_bridge.flush(
                                true,
                                &req.control,
                                req.producer,
                                req.seek_generation,
                                req.deadline,
                            ) {
                                mailbox.lock().transition_result = Some((req.producer, Err(e)));
                                req.control.wake();
                                continue;
                            }
                            match AppleDevice::frames(params, ready.clone(), req.producer) {
                                Ok(new_dev) => {
                                    let error = if req.control.cancelled(req.producer, req.seek_generation) {
                                        Some(VideoError::Superseded)
                                    } else if req.control.active_now() >= req.deadline {
                                        Some(VideoError::Sink(SinkError::WouldBlock))
                                    } else {
                                        None
                                    };
                                    if let Some(error) = error {
                                        new_dev.retire()?;
                                        mailbox.lock().transition_result = Some((req.producer, Err(error)));
                                        req.control.wake();
                                        continue;
                                    }
                                    device = Some(new_dev);
                                    active_request = Some(req.clone());
                                    applied_playing = None;
                                    force_playing_reapply = true;
                                    let mut state = mailbox.lock();
                                    state.current_producer = Some(req.producer);
                                    state.configured_mode = Some(VideoMode::Frames);
                                    state.transition_result =
                                        Some((req.producer, Ok(VideoMode::Frames)));
                                }
                                Err(e) => {
                                    mailbox.lock().transition_result =
                                        Some((req.producer, Err(e)));
                                }
                            }
                            req.control.wake();
                        }
                    } else {
                        // No prior device: bind if needed and install immediately.
                        if !main_bridge.is_bound.load(Ordering::Acquire) {
                            if let Err(e) = main_bridge.bind(
                                synchronizer.0.clone(),
                                Arc::clone(&clock),
                                Arc::clone(&mailbox),
                                Arc::clone(&req.control),
                                req.producer,
                                req.seek_generation,
                                req.deadline,
                            ) {
                                mailbox.lock().transition_result = Some((req.producer, Err(e)));
                                req.control.wake();
                                continue;
                            }
                        }
                        if req.control.cancelled(req.producer, req.seek_generation) {
                            mailbox.lock().transition_result =
                                Some((req.producer, Err(VideoError::Superseded)));
                            req.control.wake();
                            continue;
                        }
                        match AppleDevice::frames(params, ready.clone(), req.producer) {
                            Ok(new_dev) => {
                                let error = if req.control.cancelled(req.producer, req.seek_generation) {
                                    Some(VideoError::Superseded)
                                } else if req.control.active_now() >= req.deadline {
                                    Some(VideoError::Sink(SinkError::WouldBlock))
                                } else {
                                    None
                                };
                                if let Some(error) = error {
                                    new_dev.retire()?;
                                    mailbox.lock().transition_result = Some((req.producer, Err(error)));
                                    req.control.wake();
                                    continue;
                                }
                                device = Some(new_dev);
                                active_request = Some(req.clone());
                                applied_playing = None;
                                force_playing_reapply = true;
                                let mut state = mailbox.lock();
                                state.current_producer = Some(req.producer);
                                state.configured_mode = Some(VideoMode::Frames);
                                state.transition_result =
                                    Some((req.producer, Ok(VideoMode::Frames)));
                            }
                            Err(e) => {
                                mailbox.lock().transition_result = Some((req.producer, Err(e)));
                                mailbox.release_input_credit();
                            }
                        }
                        req.control.wake();
                    }
                }
            }
        }

        let status_request = {
            let mut state = mailbox.lock();
            let request = if std::mem::take(&mut state.renderer_status_dirty) {
                active_request.as_ref().and_then(|active| {
                    if state.latest_producer == Some(active.producer)
                        && !active.control.cancelled(active.producer, active.seek_generation)
                    {
                        Some(active.clone())
                    } else {
                        Mailbox::preserving_successor(&state, active.producer).cloned()
                    }
                })
            } else {
                None
            };
            state.renderer_status_checking = request.is_some();
            request
        };
        if let Some(request) = status_request {
            if main_bridge.is_bound.load(Ordering::Acquire) {
                match main_bridge.inspect_renderer_status(&request, &mailbox) {
                    Ok(()) => {}
                    Err(VideoError::Superseded) => {
                        mailbox.lock().renderer_status_dirty = true;
                    }
                    Err(error) => mailbox.report_error(
                        request.producer, error, &request.control,
                    ),
                }
            }
            mailbox.lock().renderer_status_checking = false;
            request.control.wake();
        }

        // Retry held output (after setup so preserving authorization is live).
        if let Some(held) = held_output.take() {
            if let Some(req) = &active_request {
                let threshold = applied_present_from.unwrap_or(Duration::ZERO);
                let allow = mailbox.wait_for_published_request(
                    &req.control, held.producer, held.seek_gen,
                );
                if held.pts < threshold || !allow {
                    // Drop stale held sample; release charged input credit once.
                    if held.input_credit {
                        mailbox.release_input_credit();
                    }
                } else {
                    let returned = Arc::new(Mutex::new(None));
                    let input_credit = held.input_credit;
                    match main_bridge.enqueue_sample(
                        held.sample,
                        held.pts,
                        held.producer,
                        held.is_ready_candidate,
                        held.ready.clone(),
                        Arc::clone(&mailbox),
                        Arc::clone(&req.control),
                        held.seek_gen,
                        Arc::clone(&returned),
                    ) {
                        Ok(()) => {
                            if input_credit {
                                mailbox.release_input_credit();
                            }
                        }
                        Err(VideoError::Sink(SinkError::WouldBlock) | VideoError::Superseded) => {
                            if let Some(s) = returned.lock().take() {
                                held_output = Some(HeldSample {
                                    sample: s,
                                    pts: held.pts,
                                    producer: held.producer,
                                    is_ready_candidate: held.is_ready_candidate,
                                    ready: held.ready,
                                    seek_gen: held.seek_gen,
                                    input_credit,
                                });
                            } else if input_credit {
                                mailbox.release_input_credit();
                            }
                        }
                        Err(e) => {
                            if input_credit {
                                mailbox.release_input_credit();
                            }
                            mailbox.report_error(held.producer, e, &req.control);
                        }
                    }
                }
            } else if held.input_credit {
                mailbox.release_input_credit();
            }
        }

        // Setup never owns queued input: all early exits leave its credit intact.
        let input = mailbox.take_input();
        if let Some(inp) = input {
            let inp_producer = inp.producer();
            let active_ok = active_request
                .as_ref()
                .is_some_and(|req| req.producer == inp_producer);
            let pred_ok = authorized_predecessor == Some(inp_producer);

            if !active_ok && !pred_ok {
                mailbox.discard_input(inp_producer);
                if let Some(req) = &active_request {
                    req.control.wake();
                }
            } else if held_output.is_some() {
                // New media admission blocked until predecessor/bridge credit completes.
                // Put input back.
                {
                    let mut state = mailbox.lock();
                    state.input_slot = Some(inp);
                    state.input_held_by_owner = false;
                }
                mailbox.condvar.notify_all();
            } else if let Some(active_req) = active_request.clone() {
                let control = if pred_ok && !active_ok {
                    // Predecessor work under preserving request: tag stays old.
                    active_req.control.clone()
                } else {
                    active_req.control.clone()
                };
                let seek_gen = active_req.seek_generation;
                if !mailbox.wait_for_published_request(&control, inp_producer, seek_gen) {
                    mailbox.discard_input(inp_producer);
                    control.wake();
                    continue;
                }

                match inp {
                    MailboxInput::Packet {
                        packet,
                        pts,
                        random_access,
                        ..
                    } => {
                        if let Some(AppleDevice::Compressed {
                            compressed,
                            decoder,
                            ready,
                            producer: dev_p,
                        }) = device.as_mut()
                        {
                            if let Some(pf) = applied_present_from {
                                compressed.present_from = pf;
                            }
                            if !compressed.primed && random_access {
                                compressed.primed = true;
                            }
                            if compressed.primed {
                                if let Some(dec) = decoder.as_ref() {
                                    dec.set_producer_control(inp_producer, Arc::clone(&control));
                                    dec.present_from(compressed.present_from);
                                    match process_compressed_packet(
                                        compressed,
                                        dec,
                                        &packet,
                                        pts,
                                        ready,
                                        *dev_p,
                                        inp_producer,
                                        &main_bridge,
                                        &mailbox,
                                        &control,
                                        seek_gen,
                                        &mut held_output,
                                    ) {
                                        Ok(()) => {}
                                        Err(e) => mailbox.report_error(inp_producer, e, &control),
                                    }
                                }
                            }
                        }
                        // Retain input credit while a held sample still charges it.
                        let credit_held = held_output
                            .as_ref()
                            .is_some_and(|h| h.input_credit && h.producer == inp_producer);
                        if !credit_held {
                            mailbox.discard_input(inp_producer);
                        }
                        control.wake();
                    }
                    MailboxInput::Frame { frame, pts, .. } => {
                        if let Some(AppleDevice::Frames {
                            software, ready, ..
                        }) = device.as_mut()
                        {
                            match process_software_frame(
                                software,
                                &frame,
                                pts,
                                ready,
                                inp_producer,
                                applied_present_from.unwrap_or(Duration::ZERO),
                                &main_bridge,
                                &mailbox,
                                &control,
                                seek_gen,
                                &mut held_output,
                            ) {
                                Ok(()) => {}
                                Err(e) => mailbox.report_error(inp_producer, e, &control),
                            }
                        }
                        let credit_held = held_output
                            .as_ref()
                            .is_some_and(|h| h.input_credit && h.producer == inp_producer);
                        if !credit_held {
                            mailbox.discard_input(inp_producer);
                        }
                        control.wake();
                    }
                }
            } else {
                mailbox.discard_input(inp_producer);
            }
        }


        // Complete deferred reset=false replacement only after predecessor work is done.
        if let Some(req) = pending_replacement.clone() {
            let pred = authorized_predecessor.expect("pending replacement has predecessor");
            let pred_held = held_output
                .as_ref()
                .is_some_and(|h| h.producer == pred);
            let slot_is_pred = mailbox
                .lock()
                .input_slot
                .as_ref()
                .is_some_and(|inp| inp.producer() == pred);
            let input_held_pred = mailbox.lock().input_held_by_owner
                && !pred_held
                && (slot_is_pred
                    || held_output
                        .as_ref()
                        .is_some_and(|h| h.input_credit && h.producer == pred));

            // Service remaining compressed reorder under authorization before install.
            // At most one delivery while the output credit is free; no full drain/collect.
            if !pred_held {
                if let Some(AppleDevice::Compressed {
                    compressed,
                    decoder,
                    ready,
                    producer: prev_p,
                }) = device.as_mut()
                {
                    if let Some(dec) = decoder.as_ref() {
                        if native_finish_done != Some(pred) {
                            if let Err(e) = dec.finish() {
                                mailbox.report_error(
                                    pred,
                                    VideoError::Sink(e),
                                    &req.control,
                                );
                            }
                            native_finish_done = Some(pred);
                        }
                        if held_output.is_none() {
                            let next = {
                                let mut pictures = dec.output.pictures.lock();
                                if pictures.is_empty() {
                                    None
                                } else {
                                    Some(pictures.remove(0))
                                }
                            };
                            if let Some((out_pts, out_sample)) = next {
                                if out_pts >= compressed.present_from {
                                    let returned = Arc::new(Mutex::new(None));
                                    let ctrl = active_request
                                        .as_ref()
                                        .map(|r| Arc::clone(&r.control))
                                        .unwrap_or_else(|| Arc::clone(&req.control));
                                    let seek = active_request
                                        .as_ref()
                                        .map(|r| r.seek_generation)
                                        .unwrap_or(req.seek_generation);
                                    match main_bridge.enqueue_sample(
                                        out_sample,
                                        out_pts,
                                        *prev_p,
                                        true,
                                        ready.clone(),
                                        Arc::clone(&mailbox),
                                        ctrl,
                                        seek,
                                        Arc::clone(&returned),
                                    ) {
                                        Ok(()) => {}
                                        Err(VideoError::Sink(SinkError::WouldBlock) | VideoError::Superseded) => {
                                            if let Some(s) = returned.lock().take() {
                                                held_output = Some(HeldSample {
                                                    sample: s,
                                                    pts: out_pts,
                                                    producer: *prev_p,
                                                    is_ready_candidate: true,
                                                    ready: ready.clone(),
                                                    seek_gen: seek,
                                                    input_credit: false,
                                                });
                                            }
                                        }
                                        Err(e) => {
                                            mailbox.report_error(*prev_p, e, &req.control)
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let pred_held = held_output
                .as_ref()
                .is_some_and(|h| h.producer == pred);
            let reorder_pending = match device.as_ref() {
                Some(AppleDevice::Compressed {
                    decoder: Some(dec), ..
                }) => !dec.output.pictures.lock().is_empty(),
                _ => false,
            };
            let slot_is_pred = mailbox
                .lock()
                .input_slot
                .as_ref()
                .is_some_and(|inp| inp.producer() == pred);
            let input_held_pred = mailbox.lock().input_held_by_owner
                && !pred_held
                && (slot_is_pred
                    || held_output
                        .as_ref()
                        .is_some_and(|h| h.input_credit && h.producer == pred));

            let install_ready = !slot_is_pred
                && !input_held_pred
                && !pred_held
                && !reorder_pending
                && held_output.is_none();
            if install_ready {
                if req.control.cancelled(req.producer, req.seek_generation) {
                    pending_replacement = None;
                    authorized_predecessor = None;
                    mailbox.clear_authorized_predecessor();
                    mailbox.lock().transition_result =
                        Some((req.producer, Err(VideoError::Superseded)));
                    req.control.wake();
                } else if req.control.active_now() >= req.deadline {
                    pending_replacement = None;
                    authorized_predecessor = None;
                    mailbox.clear_authorized_predecessor();
                    mailbox.lock().transition_result = Some((
                        req.producer,
                        Err(VideoError::Sink(SinkError::WouldBlock)),
                    ));
                    req.control.wake();
                } else {
                    match &req.target {
                        VideoTarget::Frames { params, ready, .. } => {
                            if let Some(dev) = device.take() {
                                dev.retire()?;
                            }
                            match AppleDevice::frames(params, ready.clone(), req.producer) {
                                Ok(new_dev) => {
                                    if req.control.cancelled(req.producer, req.seek_generation)
                                    {
                                        new_dev.retire()?;
                                        mailbox.lock().transition_result = Some((
                                            req.producer,
                                            Err(VideoError::Superseded),
                                        ));
                                    } else if req.control.active_now() >= req.deadline {
                                        new_dev.retire()?;
                                        mailbox.lock().transition_result = Some((
                                            req.producer,
                                            Err(VideoError::Sink(SinkError::WouldBlock)),
                                        ));
                                    } else {
                                        device = Some(new_dev);
                                        active_request = Some(req.clone());
                                        pending_replacement = None;
                                        authorized_predecessor = None;
                                        mailbox.clear_authorized_predecessor();
                                        applied_playing = None;
                                        force_playing_reapply = true;
                                        let mut state = mailbox.lock();
                                        state.current_producer = Some(req.producer);
                                        state.configured_mode = Some(VideoMode::Frames);
                                        state.authorized_predecessor = None;
                                        state.transition_result =
                                            Some((req.producer, Ok(VideoMode::Frames)));
                                        drop(state);
                                    }
                                    req.control.wake();
                                }
                                Err(e) => {
                                    pending_replacement = None;
                                    authorized_predecessor = None;
                                    mailbox.clear_authorized_predecessor();
                                    mailbox.lock().transition_result =
                                        Some((req.producer, Err(e)));
                                    req.control.wake();
                                }
                            }
                        }
                        _ => {
                            pending_replacement = None;
                            authorized_predecessor = None;
                            mailbox.clear_authorized_predecessor();
                        }
                    }
                }
            }
        }

        // Exactly one drain per producer: finish once, deliver tail without
        // overwriting a held sample or reallocating the reorder queue each pass.
        if let Some(p) = drain {
            let already = mailbox.lock().drain_completed == Some(p) || local_drain_done == Some(p);
            if already {
                // nothing
            } else if held_output.is_some() {
                // One output credit occupied: do not pull another sample yet.
            } else if active_request.as_ref().is_some_and(|r| r.producer == p)
                || authorized_predecessor == Some(p)
            {
                let active_req = active_request.as_ref().unwrap();
                if let Some(AppleDevice::Compressed {
                    compressed,
                    decoder,
                    ready,
                    ..
                }) = device.as_mut()
                {
                    if let Some(dec) = decoder.as_ref() {
                        if native_finish_done != Some(p) {
                            if let Err(e) = dec.finish() {
                                mailbox.report_error(
                                    p,
                                    VideoError::Sink(e),
                                    &active_req.control,
                                );
                            }
                            if let Some(msg) = dec.take_failed() {
                                mailbox.report_error(
                                    p,
                                    VideoError::Sink(SinkError::Fallback(msg)),
                                    &active_req.control,
                                );
                            }
                            native_finish_done = Some(p);
                        }
                        // Deliver at most one picture while credit is free.
                        let next = {
                            let mut pictures = dec.output.pictures.lock();
                            if pictures.is_empty() {
                                None
                            } else {
                                Some(pictures.remove(0))
                            }
                        };
                        if let Some((out_pts, out_sample)) = next {
                            if out_pts >= compressed.present_from {
                                let returned = Arc::new(Mutex::new(None));
                                match main_bridge.enqueue_sample(
                                    out_sample,
                                    out_pts,
                                    p,
                                    true,
                                    ready.clone(),
                                    Arc::clone(&mailbox),
                                    Arc::clone(&active_req.control),
                                    active_req.seek_generation,
                                    Arc::clone(&returned),
                                ) {
                                    Ok(()) => {}
                                    Err(VideoError::Sink(SinkError::WouldBlock) | VideoError::Superseded) => {
                                        if let Some(s) = returned.lock().take() {
                                            held_output = Some(HeldSample {
                                                sample: s,
                                                pts: out_pts,
                                                producer: p,
                                                is_ready_candidate: true,
                                                ready: ready.clone(),
                                                seek_gen: active_req.seek_generation,
                                                input_credit: false,
                                            });
                                        }
                                    }
                                    Err(e) => {
                                        mailbox.report_error(p, e, &active_req.control);
                                    }
                                }
                            }
                        }
                        let reorder_empty = dec.output.pictures.lock().is_empty();
                        if held_output.is_none() && reorder_empty {
                            local_drain_done = Some(p);
                            let mut state = mailbox.lock();
                            if state.current_error.as_ref().is_none_or(|(ep, _)| *ep != p) {
                                state.drain_completed = Some(p);
                            }
                            drop(state);
                            active_req.control.wake();
                        }
                    } else {
                        // No decoder: drain is vacuously complete.
                        local_drain_done = Some(p);
                        let mut state = mailbox.lock();
                        if state.current_error.as_ref().is_none_or(|(ep, _)| *ep != p) {
                            state.drain_completed = Some(p);
                        }
                        drop(state);
                        active_req.control.wake();
                    }
                } else {
                    local_drain_done = Some(p);
                    let mut state = mailbox.lock();
                    if state.current_error.as_ref().is_none_or(|(ep, _)| *ep != p) {
                        state.drain_completed = Some(p);
                    }
                    drop(state);
                    active_req.control.wake();
                }
            }
        }

    }

    // Clean retirement path.
    if let Some(dev) = device.take() {
        dev.retire()?;
    }
    main_bridge.unbind()?;
    Ok(())
}

fn process_compressed_packet(
    compressed: &mut Compressed,
    dec: &NativeDecoder,
    packet: &Packet,
    pts: Duration,
    ready: &PictureReady,
    _dev_producer: ProducerId,
    inp_producer: ProducerId,
    main_bridge: &Arc<AppleMain>,
    mailbox: &Arc<Mailbox>,
    control: &Arc<dyn VideoControl>,
    seek_gen: u64,
    held_output: &mut Option<HeldSample>,
) -> Result<(), VideoError> {
    if !mailbox.wait_for_published_request(control, inp_producer, seek_gen) {
        return Err(VideoError::Superseded);
    }
    let mut data = if compressed.annex_b {
        std::borrow::Cow::Owned(annex_b_to_length_prefixed(
            &packet.data,
            compressed.length_size,
        ))
    } else {
        std::borrow::Cow::Borrowed(packet.data.as_slice())
    };
    if let Some(filtered) = compressed.filter_leading(&data) {
        data = std::borrow::Cow::Owned(filtered);
    }
    if data.is_empty() {
        return Ok(());
    }
    let block = unsafe { create_block_buffer_from_bytes(&data) }.map_err(VideoError::Sink)?;
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
            Some(&*compressed.format),
            1,
            1,
            &timing,
            1,
            ptr::from_ref(&data.len()),
            NonNull::from(&mut raw),
        )
    };
    if status != 0 || raw.is_null() {
        return Err(VideoError::Sink(SinkError::Fallback(format!(
            "CMSampleBufferCreateReady failed: {status}"
        ))));
    }
    let sample = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };
    if !mailbox.wait_for_published_request(control, inp_producer, seek_gen) {
        return Err(VideoError::Superseded);
    }
    dec.decode(&sample).map_err(VideoError::Sink)?;
    if let Some(msg) = dec.take_failed() {
        return Err(VideoError::Sink(SinkError::Fallback(msg)));
    }
    // Drain past reorder depth.
    loop {
        let maybe = {
            let mut pictures = dec.output.pictures.lock();
            if pictures.len() > dec.output.reorder_depth {
                Some(pictures.remove(0))
            } else {
                None
            }
        };
        let Some((out_pts, out_sample)) = maybe else {
            break;
        };
        if out_pts < compressed.present_from {
            continue;
        }
        if !mailbox.wait_for_published_request(control, inp_producer, seek_gen) {
            return Err(VideoError::Superseded);
        }
        let returned = Arc::new(Mutex::new(None));
        let is_cand = out_pts >= compressed.present_from;
        match main_bridge.enqueue_sample(
            out_sample,
            out_pts,
            inp_producer,
            is_cand,
            ready.clone(),
            Arc::clone(mailbox),
            Arc::clone(control),
            seek_gen,
            Arc::clone(&returned),
        ) {
            Ok(()) => {}
            Err(VideoError::Sink(SinkError::WouldBlock) | VideoError::Superseded) => {
                if let Some(s) = returned.lock().take() {
                    *held_output = Some(HeldSample { sample: s, pts: out_pts, producer: inp_producer, is_ready_candidate: is_cand, ready: ready.clone(), seek_gen: seek_gen, input_credit: true });
                }
                break;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn process_software_frame(
    software: &SoftwarePath,
    frame: &VideoFrame,
    pts: Duration,
    ready: &PictureReady,
    inp_producer: ProducerId,
    present_from: Duration,
    main_bridge: &Arc<AppleMain>,
    mailbox: &Arc<Mailbox>,
    control: &Arc<dyn VideoControl>,
    seek_gen: u64,
    held_output: &mut Option<HeldSample>,
) -> Result<(), VideoError> {
    if !mailbox.wait_for_published_request(control, inp_producer, seek_gen) {
        return Err(VideoError::Superseded);
    }
    let converted = convert::convert(
        frame,
        PixFrameInfo::new(software.src_format, software.width, software.height),
        match software.dst_ostype {
            t if t == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange => PixelFormat::Yuv420P10Le,
            _ => PixelFormat::Nv12,
        },
        &ConvertOptions::default(),
    )
    .map_err(|e| {
        VideoError::Sink(SinkError::Fatal(format!("pixel conversion failed: {e}")))
    })?;
    let sample = unsafe {
        let pixel = pool_pixel_buffer(&software.pool.0)?;
        copy_planes_into_pixel_buffer(&pixel, &converted, software.width, software.height)?;
        let mut raw: *mut CMSampleBuffer = ptr::null_mut();
        let timing = CMSampleTimingInfo {
            duration: objc2_core_media::kCMTimeInvalid,
            presentationTimeStamp: cm_time_from_duration(pts, 600),
            decodeTimeStamp: objc2_core_media::kCMTimeInvalid,
        };
        let fmt = format_for_pixel_buffer(&pixel, software.width, software.height)?;
        let status = CMSampleBuffer::create_ready_with_image_buffer(
            None,
            &pixel,
            &fmt,
            NonNull::from(&timing),
            NonNull::from(&mut raw),
        );
        if status != 0 || raw.is_null() {
            return Err(VideoError::Sink(SinkError::Fatal(format!(
                "CMSampleBufferCreateReadyWithImageBuffer: {status}"
            ))));
        }
        SendSync(CFRetained::from_raw(NonNull::new_unchecked(raw)))
    };
    if pts < present_from {
        return Ok(());
    }
    if !mailbox.wait_for_published_request(control, inp_producer, seek_gen) {
        return Err(VideoError::Superseded);
    }
    let returned = Arc::new(Mutex::new(None));
    match main_bridge.enqueue_sample(
        sample,
        pts,
        inp_producer,
        true,
        ready.clone(),
        Arc::clone(mailbox),
        Arc::clone(control),
        seek_gen,
        Arc::clone(&returned),
    ) {
        Ok(()) => Ok(()),
        Err(VideoError::Sink(SinkError::WouldBlock) | VideoError::Superseded) => {
            if let Some(s) = returned.lock().take() {
                *held_output = Some(HeldSample { sample: s, pts: pts, producer: inp_producer, is_ready_candidate: true, ready: ready.clone(), seek_gen: seek_gen, input_credit: true });
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

impl VideoSink for AppleVideoSink {
    fn output(&self) -> VideoOutput {
        self.cached_output
    }

    fn poll_transition(
        &mut self,
        request: &VideoRequest,
    ) -> Poll<Result<VideoMode, VideoError>> {
        if request
            .control
            .cancelled(request.producer, request.seek_generation)
        {
            return Poll::Ready(Err(VideoError::Superseded));
        }

        if let Some(ticket) = &self.owner_ticket {
            if let Poll::Ready(Err(err)) = ticket.poll() {
                return Poll::Ready(Err(VideoError::Sink(SinkError::Fatal(err))));
            }
        }

        if let Some((p, err)) = &self.mailbox.lock().current_error {
            if *p == request.producer {
                return Poll::Ready(Err(err.clone()));
            }
        }

        // Retirement receipt must remain observable after setup budget expires.
        if matches!(request.target, VideoTarget::Retired) {
            // fall through after owner spawn; deadline does not apply
        } else {
            // Setup deadline only while transition pending — not steady playback.
            self.mailbox.check_transition_deadline(request)?;
        }

        if self.owner_ticket.is_none() {
            match self.spawn_owner(&request.control) {
                Ok(ticket) => {
                    self.owner_ticket = Some(ticket);
                }
                Err(OwnerError::Capacity) => {
                    request.control.wake();
                    return Poll::Pending;
                }
                Err(OwnerError::Spawn(err)) => {
                    return Poll::Ready(Err(VideoError::Sink(SinkError::Fatal(err))));
                }
            }
        }

        if matches!(request.target, VideoTarget::Retired) {
            self.mailbox.submit_request(request)?;
            if let Some(ticket) = &self.owner_ticket {
                match ticket.poll() {
                    Poll::Ready(Ok(())) => return Poll::Ready(Ok(VideoMode::Retired)),
                    Poll::Ready(Err(err)) => {
                        return Poll::Ready(Err(VideoError::Sink(SinkError::Fatal(err))));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
            return Poll::Pending;
        }

        self.mailbox.submit_request(request)?;
        self.mailbox.poll_transition_status(request.producer)
    }

    fn push_packet(
        &mut self,
        producer: ProducerId,
        packet: &mut Option<Packet>,
        pts: Duration,
        random_access: bool,
    ) -> Result<(), VideoError> {
        self.mailbox
            .push_packet(producer, packet, pts, random_access)
    }

    fn push_frame(
        &mut self,
        producer: ProducerId,
        frame: &mut Option<VideoFrame>,
        pts: Duration,
    ) -> Result<(), VideoError> {
        self.mailbox.push_frame(producer, frame, pts)
    }

    fn present_from(&mut self, producer: ProducerId, start: Duration) -> Result<(), VideoError> {
        self.mailbox.present_from(producer, start)
    }

    fn set_playing(&mut self, producer: ProducerId, playing: bool) -> Result<(), VideoError> {
        self.mailbox.set_playing(producer, playing)
    }

    fn frame_lead(&self) -> Duration {
        if self.cached_output.available {
            FRAME_LEAD
        } else {
            Duration::ZERO
        }
    }

    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        self.mailbox.poll_finish(producer)
    }
}

impl Drop for AppleVideoSink {
    fn drop(&mut self) {
        let mut state = self.mailbox.lock();
        state.retired = true;
        drop(state);
        self.mailbox.condvar.notify_all();
        // Cleanup receipt remains readable via owner ticket after deadline.
    }
}

fn create_h264_or_hevc_format(
    kind: CompressedKind,
    params: &CodecParameters,
) -> Result<(CFRetained<CMVideoFormatDescription>, usize, bool, usize), SinkError> {
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
    let format = unsafe {
        CFRetained::from_raw(NonNull::new_unchecked(raw as *mut CMVideoFormatDescription))
    };
    Ok((format, length_size, annex_b, reorder_depth(kind, &nals)))
}

fn reorder_depth(kind: CompressedKind, nals: &[&[u8]]) -> usize {
    nals.iter()
        .filter_map(|nal| {
            let header = *nal.first()?;
            match kind {
                CompressedKind::H264 if header & 0x1f == 7 => {
                    let rbsp = oxideav_h264::nal::rbsp_from_nal_payload(&nal[1..]);
                    let sps = oxideav_h264::sps::Sps::parse(&rbsp).ok()?;
                    Some(
                        sps.vui
                            .as_ref()
                            .and_then(|vui| vui.bitstream_restriction.as_ref())
                            .map_or(16, |restriction| restriction.max_num_reorder_frames),
                    )
                }
                CompressedKind::Hevc if (header >> 1) & 0x3f == 33 => {
                    let rbsp = oxideav_h264::nal::rbsp_from_nal_payload(nal.get(2..)?);
                    let sps = oxideav_h265::sps::SeqParameterSet::parse(&rbsp).ok()?;
                    Some(
                        sps.sub_layer_ordering_info[usize::from(sps.max_sub_layers_minus1)]
                            .max_num_reorder_pics,
                    )
                }
                _ => None,
            }
        })
        .max()
        .unwrap_or(16)
        .min(16) as usize
}

fn split_length_prefixed(mut data: &[u8], length_size: usize) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        if !(1..=4).contains(&length_size) {
            return None;
        }
        let prefix = data.get(..length_size)?;
        let length = prefix
            .iter()
            .fold(0usize, |size, &byte| (size << 8) | usize::from(byte));
        let end = length_size.checked_add(length)?;
        let nal = data.get(length_size..end)?;
        data = &data[end..];
        Some(nal)
    })
}

fn cm_time_from_duration(d: Duration, timescale: i32) -> CMTime {
    let value = (d.as_secs_f64() * f64::from(timescale)).round() as i64;
    CMTime {
        value,
        timescale,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    }
}

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

unsafe fn pool_pixel_buffer(
    pool: &CFRetained<CVPixelBufferPool>,
) -> Result<CFRetained<CVPixelBuffer>, SinkError> {
    let mut raw: *mut CVPixelBuffer = ptr::null_mut();
    let ret = unsafe {
        CVPixelBufferPool::create_pixel_buffer(None, pool, NonNull::from(&mut raw))
    };
    if ret != 0 || raw.is_null() {
        return Err(SinkError::Fatal(format!(
            "CVPixelBufferPoolCreatePixelBuffer: {ret}"
        )));
    }
    Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) })
}

unsafe fn copy_planes_into_pixel_buffer(
    pixel: &CFRetained<CVPixelBuffer>,
    frame: &VideoFrame,
    width: u32,
    height: u32,
) -> Result<(), SinkError> {
    let lock = unsafe { CVPixelBufferLockBaseAddress(pixel, CVPixelBufferLockFlags(0)) };
    if lock != 0 {
        return Err(SinkError::Fatal(format!(
            "CVPixelBufferLockBaseAddress: {lock}"
        )));
    }
    let result = (|| {
        let planes = CVPixelBufferGetPlaneCount(pixel);
        let _ = (width, height);
        if planes == 0 {
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
            let dst = unsafe {
                std::slice::from_raw_parts_mut(base as *mut u8, dst_stride * dst_h)
            };
            for row in 0..rows {
                let src = &src_plane.data
                    [row * src_stride..row * src_stride + src_stride.min(dst_stride)];
                dst[row * dst_stride..row * dst_stride + src.len()].copy_from_slice(src);
            }
        } else {
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
                        _ => 2usize,
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
    Ok(unsafe {
        CFRetained::from_raw(NonNull::new_unchecked(
            raw as *mut CMVideoFormatDescription,
        ))
    })
}
