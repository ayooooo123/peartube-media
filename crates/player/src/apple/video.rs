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
use objc2_core_foundation::{CFBoolean, CFData, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    kCMFormatDescriptionExtension_ColorPrimaries,
    kCMFormatDescriptionExtension_FullRangeVideo,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
    kCMFormatDescriptionExtension_TransferFunction,
    kCMFormatDescriptionExtension_YCbCrMatrix,
    kCMFormatDescriptionColorPrimaries_DCI_P3,
    kCMFormatDescriptionColorPrimaries_EBU_3213,
    kCMFormatDescriptionColorPrimaries_ITU_R_2020,
    kCMFormatDescriptionColorPrimaries_ITU_R_709_2,
    kCMFormatDescriptionColorPrimaries_P3_D65,
    kCMFormatDescriptionColorPrimaries_SMPTE_C,
    kCMFormatDescriptionTransferFunction_ITU_R_2100_HLG,
    kCMFormatDescriptionTransferFunction_ITU_R_709_2,
    kCMFormatDescriptionTransferFunction_Linear,
    kCMFormatDescriptionTransferFunction_SMPTE_240M_1995,
    kCMFormatDescriptionTransferFunction_SMPTE_ST_2084_PQ,
    kCMFormatDescriptionTransferFunction_sRGB,
    kCMFormatDescriptionYCbCrMatrix_ITU_R_2020,
    kCMFormatDescriptionYCbCrMatrix_ITU_R_601_4,
    kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2,
    kCMFormatDescriptionYCbCrMatrix_SMPTE_240M_1995,
    CMClock, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
    CMTimebase, CMVideoFormatDescription,
    CMVideoFormatDescriptionCreate, CMVideoFormatDescriptionCreateForImageBuffer,
    kCMFormatDescriptionExtension_ContentLightLevelInfo,
    kCMFormatDescriptionExtension_MasteringDisplayColorVolume,
    kCMVideoCodecType_DolbyVisionHEVC, kCMVideoCodecType_HEVC,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets,
    CMVideoFormatDescriptionMatchesImageBuffer,
};
use objc2_core_video::{
    CVAttachmentMode, CVBuffer, CVImageBuffer,
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
    CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVImageBufferColorPrimaries_DCI_P3, kCVImageBufferColorPrimaries_EBU_3213,
    kCVImageBufferColorPrimaries_ITU_R_2020, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferColorPrimaries_P3_D65, kCVImageBufferColorPrimaries_SMPTE_C,
    kCVImageBufferColorPrimariesKey,
    kCVImageBufferPixelAspectRatioHorizontalSpacingKey,
    kCVImageBufferPixelAspectRatioKey,
    kCVImageBufferPixelAspectRatioVerticalSpacingKey,
    kCVImageBufferTransferFunction_ITU_R_2100_HLG,
    kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferTransferFunction_Linear,
    kCVImageBufferTransferFunction_SMPTE_240M_1995,
    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
    kCVImageBufferTransferFunction_sRGB,
    kCVImageBufferTransferFunctionKey,
    kCVImageBufferYCbCrMatrix_ITU_R_2020,
    kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_SMPTE_240M_1995,
    kCVImageBufferYCbCrMatrixKey,
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
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
    Vp9,
    Av1,
}

/// Session-defining VP9 parameters. A change forces a format/session rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Vp9SessionConfig {
    profile: u8,
    bit_depth: u8,
    /// ISOBMFF vpcC chroma_subsampling: 1=4:2:0, 2=4:2:2, 3=4:4:4.
    chroma_subsampling: u8,
    full_range: bool,
    width: u32,
    height: u32,
}

struct Vp9State {
    config: Vp9SessionConfig,
    color_signal: oxideav_core::ColorSignal,
    frame_rate: Option<oxideav_core::Rational>,
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
    params: CodecParameters,
    /// What the current format was built from.
    headers: StreamHeaders,
    vp9: Option<Vp9State>,
}

/// What a video stream's headers say beyond its parameter sets. HDR10's mastering display
/// and content light (HEVC SEI 137/144, AV1 metadata OBUs) reach VideoToolbox's pictures only
/// from the format description, never from the bitstream, and Dolby Vision decodes only from
/// a format that carries the DV record. From the setup bytes, or the first keyframe once it
/// arrives.
#[derive(Clone, Default, PartialEq)]
struct StreamHeaders {
    mastering: Option<[u8; 24]>,
    content_light: Option<[u8; 4]>,
    dovi: Option<DolbyVision>,
    av1: Option<Av1Record>,
}

/// What an AV1 format is built from: the `av1C` record and the sequence header's size.
#[derive(Clone, PartialEq)]
struct Av1Record {
    av1c: Vec<u8>,
    width: u32,
    height: u32,
}

#[derive(Clone, PartialEq)]
struct DolbyVision {
    /// The `hvcC` payload the DV format carries beside the DV record.
    hvcc: Vec<u8>,
    width: u32,
    height: u32,
    /// `dvcC` up to profile 7, `dvvC` above.
    atom: &'static str,
    record: [u8; 24],
    /// Profile 5 has no backward-compatible base layer: `dvh1`, not `hvc1`.
    dvh1: bool,
}

fn stream_headers(params: &CodecParameters, keyframe: &[u8]) -> StreamHeaders {
    use crate::headers::codec;
    let Some(mut config) = codec::video_config(params, keyframe) else {
        return StreamHeaders::default();
    };
    codec::apply_dovi(&mut config, codec::find_dovi_box(&params.extradata), keyframe, codec::video_framing(params));
    let (record, width, height) = (config.record, config.width, config.height);
    let (av1, dovi) = if config.codec == crate::headers::types::VideoCodec::Av1 {
        (Some(Av1Record { av1c: record, width, height }), None)
    } else {
        // Profile 7 needs every RPU rewritten: its HDR10 base layer plays as it is.
        let dovi = config.dovi.filter(|d| !d.convert_p7_to_81).map(|d| DolbyVision {
            hvcc: record,
            width,
            height,
            atom: if d.profile > 7 { "dvvC" } else { "dvcC" },
            record: d.record,
            dvh1: d.use_dvh1,
        });
        (None, dovi)
    };
    StreamHeaders { mastering: config.mastering_display, content_light: config.content_light, dovi, av1 }
}

impl Compressed {
    /// The first keyframe can say what the setup bytes did not (a Dolby Vision RPU, HDR10's
    /// SEI or AV1 metadata OBUs). Before anything is decoded, a format and decoder that know it
    /// replace `old`; when they cannot be made, `old` stays and the stream plays without it.
    fn reformat(&mut self, keyframe: &[u8], old: NativeDecoder) -> NativeDecoder {
        let headers = stream_headers(&self.params, keyframe);
        if headers == self.headers {
            return old;
        }
        let Ok((format, length_size, annex_b, reorder_depth)) = create_format(self.kind, &self.params, &headers)
        else {
            return old;
        };
        let Ok(replacement) = old.rebuilt(&format, reorder_depth) else {
            return old;
        };
        let _ = old.retire();
        (self.format, self.length_size, self.annex_b, self.reorder_depth) = (format, length_size, annex_b, reorder_depth);
        self.headers = headers;
        replacement
    }

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
    fn VTSessionCopyProperty(
        session: *const CFType,
        property_key: *const CFString,
        allocator: *const std::ffi::c_void,
        property_value_out: *mut *const CFType,
    ) -> i32;
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
        Self::with_output(format, Arc::new(output))
    }

    fn with_output(
        format: &CMVideoFormatDescription,
        output: Arc<DecodedOutput>,
    ) -> Result<Self, SinkError> {
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

    /// A session for `format` that reports exactly as this one does; nothing decoded yet.
    fn rebuilt(&self, format: &CMVideoFormatDescription, reorder_depth: usize) -> Result<Self, SinkError> {
        let o = &self.output;
        Self::new(
            format,
            DecodedOutput {
                reports: Arc::clone(&o.reports),
                generation: o.generation,
                present_from: AtomicU64::new(o.present_from.load(Ordering::Acquire)),
                producer: AtomicU64::new(o.producer.load(Ordering::Acquire)),
                mailbox: Arc::clone(&o.mailbox),
                control: Arc::clone(&o.control),
                failed: Arc::clone(&o.failed),
                format: Mutex::new(None),
                reorder_depth,
                pictures: Mutex::new(Vec::new()),
            },
        )
    }

    /// Finish the old session and open a new one sharing the same decoded-output Arc.
    fn recreate(self, format: &CMVideoFormatDescription) -> Result<Self, SinkError> {
        let output = Arc::clone(&self.output);
        self.retire().map_err(SinkError::Fallback)?;
        *output.format.lock() = None;
        Self::with_output(format, output)
    }

    fn using_hardware(&self) -> bool {
        // The literal key, not the framework constant: the constant is iOS 17+,
        // and a strong reference to it would stop the app loading on older iOS.
        let key = CFString::from_str("UsingHardwareAcceleratedVideoDecoder");
        let mut raw: *const CFType = ptr::null();
        let status = unsafe {
            VTSessionCopyProperty(&*self.session, &*key, ptr::null(), &mut raw)
        };
        if status != 0 || raw.is_null() {
            return false;
        }
        let value = unsafe { CFRetained::<CFType>::from_raw(NonNull::new_unchecked(raw.cast_mut())) };
        value
            .downcast_ref::<CFBoolean>()
            .map(|b| b.value())
            .unwrap_or(false)
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
    color_signal: oxideav_core::ColorSignal,
    sample_aspect_ratio: Option<oxideav_core::Rational>,
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
            "vp9" => CompressedKind::Vp9,
            "av1" => CompressedKind::Av1,
            _ => return Err(VideoError::Unsupported),
        };
        // VP9's headers stay the empty default; AV1's carry its av1C record.
        let headers = stream_headers(params, &[]);

        let (format, length_size, annex_b, reorder_depth, vp9) = match kind {
            CompressedKind::Vp9 => {
                super::vt_caps::register_supplemental(super::vt_caps::CODEC_VP9);
                let config = provisional_vp9_config(params).map_err(|e| match e {
                    SinkError::Fallback(_) => VideoError::Unsupported,
                    other => VideoError::Sink(other),
                })?;
                let format = create_vp9_format(&config, params.color_signal, params.frame_rate)
                    .map_err(|e| match e {
                        SinkError::Fallback(_) => VideoError::Unsupported,
                        other => VideoError::Sink(other),
                    })?;
                let vp9 = Vp9State {
                    config,
                    color_signal: params.color_signal,
                    frame_rate: params.frame_rate,
                };
                (format, 0usize, false, 0usize, Some(vp9))
            }
            CompressedKind::Av1 => {
                // Only a hardware decoder beats the engine's own; without one AV1 stays in
                // software (Apple's AV1 decoder is hardware: A17 Pro, M3 and later).
                if !super::vt_caps::hardware_decode_supported(super::vt_caps::CODEC_AV1) {
                    return Err(VideoError::Unsupported);
                }
                let format = create_av1_format(&headers, params.color_signal).map_err(|e| match e {
                    SinkError::Fallback(_) => VideoError::Unsupported,
                    other => VideoError::Sink(other),
                })?;
                (format, 0usize, false, 0usize, None)
            }
            CompressedKind::H264 | CompressedKind::Hevc => {
                if params.extradata.is_empty() {
                    return Err(VideoError::Unsupported);
                }
                let (format, length_size, annex_b, reorder_depth) =
                    create_h264_or_hevc_format(kind, params, &headers).map_err(|e| match e {
                        SinkError::Fallback(_) => VideoError::Unsupported,
                        other => VideoError::Sink(other),
                    })?;
                (format, length_size, annex_b, reorder_depth, None)
            }
        };

        let compressed = Compressed {
            kind,
            length_size,
            annex_b,
            format,
            reorder_depth,
            primed: false,
            first_irap: true,
            discard_rasl: false,
            present_from,
            params: params.clone(),
            headers,
            vp9,
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
        .map_err(|e| match e {
            // Configure-time session failure → Unsupported so the engine falls back immediately.
            SinkError::Fallback(_) if matches!(kind, CompressedKind::Vp9 | CompressedKind::Av1) => {
                VideoError::Unsupported
            }
            other => VideoError::Sink(other),
        })?;

        if matches!(kind, CompressedKind::Vp9 | CompressedKind::Av1) {
            let hw = decoder.using_hardware();
            eprintln!("[peartube-apple] {kind:?} on VideoToolbox, hardware: {hw}");
        }

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
        let is_10bit = matches!(
            src_format,
            PixelFormat::Yuv420P10Le
                | PixelFormat::Yuv422P10Le
                | PixelFormat::Yuv444P10Le
                | PixelFormat::Yuv420P12Le
                | PixelFormat::Yuv422P12Le
                | PixelFormat::Yuv444P12Le
                | PixelFormat::Yuv420P16Le
                | PixelFormat::Yuv422P16Le
                | PixelFormat::Yuv444P16Le
        ) || matches!(
            params.color_signal.transfer,
            oxideav_core::TransferCharacteristics::SMPTE_ST2084
                | oxideav_core::TransferCharacteristics::ARIB_STD_B67
        );
        let full_range = params.resolved_color_range() == oxideav_core::ColorRange::Full
            && !matches!(
                src_format,
                PixelFormat::YuvJ420P | PixelFormat::YuvJ422P | PixelFormat::YuvJ444P
            );
        let dst_ostype = match (is_10bit, full_range) {
            (false, false) => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            (false, true) => kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            (true, false) => kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
            (true, true) => kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
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
        let sample_aspect_ratio = crate::headers::codec::video_config(params, &[])
            .and_then(|vc| vc.pixel_aspect)
            .map(|(w, h)| oxideav_core::Rational::new(i64::from(w), i64::from(h)));

        Ok(Self::Frames {
            software: SoftwarePath {
                src_format,
                width,
                height,
                dst_ostype,
                pool: SendSync(pool),
                color_signal: params.color_signal,
                sample_aspect_ratio,
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
                                if let Some(old) = decoder.take() {
                                    *decoder = Some(compressed.reformat(&packet.data, old));
                                }
                            }
                            if compressed.primed {
                                if decoder.is_some() {
                                    if let Some(dec) = decoder.as_ref() {
                                        dec.set_producer_control(
                                            inp_producer,
                                            Arc::clone(&control),
                                        );
                                        dec.present_from(compressed.present_from);
                                    }
                                    match process_compressed_packet(
                                        compressed,
                                        decoder,
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
                                        Err(e) => {
                                            mailbox.report_error(inp_producer, e, &control)
                                        }
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
    decoder: &mut Option<NativeDecoder>,
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

    if compressed.kind == CompressedKind::Vp9 {
        ensure_vp9_session(
            compressed,
            decoder,
            &packet.data,
            ready,
            inp_producer,
            main_bridge,
            mailbox,
            control,
            seek_gen,
            held_output,
        )?;
    }

    let dec = decoder
        .as_ref()
        .ok_or_else(|| VideoError::Sink(SinkError::Fallback("video decoder missing".into())))?;

    let mut data = match compressed.kind {
        // VP9 and AV1 packets go to VT as they are: no length-prefix or Annex-B rewrite.
        CompressedKind::Vp9 => std::borrow::Cow::Borrowed(packet.data.as_slice()),
        CompressedKind::Av1 => {
            std::borrow::Cow::Borrowed(crate::headers::codec::without_temporal_delimiter(&packet.data))
        }
        _ if compressed.annex_b => {
            std::borrow::Cow::Owned(annex_b_to_length_prefixed(&packet.data, compressed.length_size))
        }
        _ => std::borrow::Cow::Borrowed(packet.data.as_slice()),
    };
    if compressed.kind != CompressedKind::Vp9 {
        if let Some(filtered) = compressed.filter_leading(&data) {
            data = std::borrow::Cow::Owned(filtered);
        }
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
    drain_decoded_pictures(
        compressed,
        dec,
        ready,
        inp_producer,
        main_bridge,
        mailbox,
        control,
        seek_gen,
        held_output,
        false,
    )
}

/// Drain pictures past reorder depth (or all remaining when `all` is true).
fn drain_decoded_pictures(
    compressed: &Compressed,
    dec: &NativeDecoder,
    ready: &PictureReady,
    inp_producer: ProducerId,
    main_bridge: &Arc<AppleMain>,
    mailbox: &Arc<Mailbox>,
    control: &Arc<dyn VideoControl>,
    seek_gen: u64,
    held_output: &mut Option<HeldSample>,
    all: bool,
) -> Result<(), VideoError> {
    loop {
        if held_output.is_some() {
            break;
        }
        let maybe = {
            let mut pictures = dec.output.pictures.lock();
            if all {
                if pictures.is_empty() {
                    None
                } else {
                    Some(pictures.remove(0))
                }
            } else if pictures.len() > dec.output.reorder_depth {
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
                    *held_output = Some(HeldSample {
                        sample: s,
                        pts: out_pts,
                        producer: inp_producer,
                        is_ready_candidate: is_cand,
                        ready: ready.clone(),
                        seek_gen,
                        input_credit: true,
                    });
                }
                break;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Align or rebuild the VP9 session from a keyframe header when the config changes.
fn ensure_vp9_session(
    compressed: &mut Compressed,
    decoder: &mut Option<NativeDecoder>,
    packet_data: &[u8],
    ready: &PictureReady,
    inp_producer: ProducerId,
    main_bridge: &Arc<AppleMain>,
    mailbox: &Arc<Mailbox>,
    control: &Arc<dyn VideoControl>,
    seek_gen: u64,
    held_output: &mut Option<HeldSample>,
) -> Result<(), VideoError> {
    let Some(info) = parse_vp9_keyframe_header(packet_data) else {
        return Ok(());
    };
    let new_cfg = Vp9SessionConfig::from_keyframe(&info);
    let needs_rebuild = {
        let vp9 = compressed
            .vp9
            .as_ref()
            .ok_or_else(|| VideoError::Sink(SinkError::Fallback("VP9 state missing".into())))?;
        vp9.config != new_cfg
    };
    if !needs_rebuild {
        return Ok(());
    }

    // Finish the old session and drain its pictures before replacing it.
    if let Some(dec) = decoder.as_ref() {
        dec.finish().map_err(VideoError::Sink)?;
        if let Some(msg) = dec.take_failed() {
            return Err(VideoError::Sink(SinkError::Fallback(msg)));
        }
        drain_decoded_pictures(
            compressed,
            dec,
            ready,
            inp_producer,
            main_bridge,
            mailbox,
            control,
            seek_gen,
            held_output,
            true,
        )?;
        // Pictures still queued after a blocked enqueue stay in the shared
        // output (`recreate` keeps it), sorted by time, so they are shown
        // before the new session's pictures.
    }

    let (color_signal, frame_rate) = {
        let vp9 = compressed.vp9.as_ref().expect("VP9 state");
        (vp9.color_signal, vp9.frame_rate)
    };
    let format = create_vp9_format(&new_cfg, color_signal, frame_rate).map_err(VideoError::Sink)?;

    let old = decoder
        .take()
        .ok_or_else(|| VideoError::Sink(SinkError::Fallback("VP9 decoder missing".into())))?;
    let new_dec = old.recreate(&format).map_err(VideoError::Sink)?;
    let hw = new_dec.using_hardware();
    eprintln!("[peartube-apple] VP9 on VideoToolbox, hardware: {hw}");
    *decoder = Some(new_dec);
    compressed.format = format;
    if let Some(vp9) = compressed.vp9.as_mut() {
        vp9.config = new_cfg;
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
    let is_nv12_dst = software.dst_ostype == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        || software.dst_ostype == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
    let direct_yuv420p = software.src_format == PixelFormat::Yuv420P
        && is_nv12_dst
        && frame.planes.len() >= 3;

    let sample = unsafe {
        let pixel = pool_pixel_buffer(&software.pool.0)?;
        if direct_yuv420p {
            copy_yuv420p_into_nv12_pixel_buffer(
                &pixel,
                &frame.planes[0],
                &frame.planes[1],
                &frame.planes[2],
                software.width as usize,
                software.height as usize,
            )?;
        } else {
            let conv_format = match software.dst_ostype {
                t if t == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
                    || t == kCVPixelFormatType_420YpCbCr10BiPlanarFullRange =>
                {
                    PixelFormat::Yuv420P10Le
                }
                _ => PixelFormat::Nv12,
            };
            let converted = convert::convert(
                frame,
                PixFrameInfo::new(software.src_format, software.width, software.height),
                conv_format,
                &ConvertOptions::default(),
            )
            .map_err(|e| {
                VideoError::Sink(SinkError::Fatal(format!("pixel conversion failed: {e}")))
            })?;
            copy_planes_into_pixel_buffer(
                &pixel,
                &converted,
                conv_format,
                software.width,
                software.height,
            )?;
        }
        attach_frame_metadata(
            &pixel,
            frame,
            software.color_signal,
            software.sample_aspect_ratio,
        );
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

    fn platform_decoder(&self) -> &'static str {
        "VideoToolbox"
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

/// Parsed VP9 uncompressed header fields from a keyframe (superframe first frame).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Vp9KeyframeInfo {
    profile: u8,
    bit_depth: u8,
    color_space: u8,
    full_range: bool,
    subsampling_x: u8,
    subsampling_y: u8,
    width: u32,
    height: u32,
}

impl Vp9SessionConfig {
    fn from_keyframe(info: &Vp9KeyframeInfo) -> Self {
        Self {
            profile: info.profile,
            bit_depth: info.bit_depth,
            chroma_subsampling: vp9_chroma_subsampling(info.subsampling_x, info.subsampling_y),
            full_range: info.full_range,
            width: info.width,
            height: info.height,
        }
    }
}

fn vp9_chroma_subsampling(ss_x: u8, ss_y: u8) -> u8 {
    match (ss_x, ss_y) {
        (0, 0) => 3, // 4:4:4
        (1, 0) => 2, // 4:2:2
        _ => 1,      // 4:2:0 (colocated), including (1,1)
    }
}

/// MSB-first bit reader for the VP9 uncompressed header.
struct Vp9Bits<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> Vp9Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn get(&mut self, n: usize) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        let mut v = 0u32;
        for _ in 0..n {
            let byte_i = self.bit / 8;
            let b = *self.data.get(byte_i)?;
            let shift = 7 - (self.bit % 8);
            v = (v << 1) | u32::from((b >> shift) & 1);
            self.bit += 1;
        }
        Some(v)
    }
}

/// Parse the VP9 uncompressed header of a keyframe starting at packet byte 0.
/// Returns `None` for non-keyframes, show_existing_frame, truncated input, or bad sync.
fn parse_vp9_keyframe_header(data: &[u8]) -> Option<Vp9KeyframeInfo> {
    let mut br = Vp9Bits::new(data);
    if br.get(2)? != 2 {
        return None; // frame_marker
    }
    let profile_low = br.get(1)? as u8;
    let profile_high = br.get(1)? as u8;
    let profile = (profile_high << 1) | profile_low;
    if profile == 3 {
        if br.get(1)? != 0 {
            return None; // reserved zero
        }
    }
    if br.get(1)? != 0 {
        return None; // show_existing_frame
    }
    if br.get(1)? != 0 {
        return None; // frame_type: 0 = KEY_FRAME
    }
    let _show_frame = br.get(1)?;
    let _error_resilient = br.get(1)?;
    // sync code 0x49 0x83 0x42
    if br.get(8)? != 0x49 || br.get(8)? != 0x83 || br.get(8)? != 0x42 {
        return None;
    }

    let bit_depth = if profile >= 2 {
        if br.get(1)? != 0 {
            12
        } else {
            10
        }
    } else {
        8
    };
    let color_space = br.get(3)? as u8;
    let (full_range, ss_x, ss_y) = if color_space != 7 {
        let full_range = br.get(1)? != 0;
        let (ss_x, ss_y) = if profile == 1 || profile == 3 {
            let x = br.get(1)? as u8;
            let y = br.get(1)? as u8;
            if br.get(1)? != 0 {
                return None; // reserved zero
            }
            (x, y)
        } else {
            (1, 1)
        };
        (full_range, ss_x, ss_y)
    } else {
        // RGB: full range, and profile 1/3 have ss=0,0 + reserved
        if profile == 1 || profile == 3 {
            if br.get(1)? != 0 {
                return None;
            }
        }
        (true, 0, 0)
    };

    let width = br.get(16)? as u32 + 1;
    let height = br.get(16)? as u32 + 1;
    Some(Vp9KeyframeInfo {
        profile,
        bit_depth,
        color_space,
        full_range,
        subsampling_x: ss_x,
        subsampling_y: ss_y,
        width,
        height,
    })
}

fn provisional_vp9_config(params: &CodecParameters) -> Result<Vp9SessionConfig, SinkError> {
    let width = params.width.unwrap_or(0);
    let height = params.height.unwrap_or(0);
    if width == 0 || height == 0 {
        return Err(SinkError::Fallback("VP9 stream without dimensions".into()));
    }
    let (profile, bit_depth, chroma) = match params.pixel_format {
        Some(PixelFormat::Yuv420P) | None => (0, 8, 1),
        Some(PixelFormat::Yuv420P10Le) => (2, 10, 1),
        Some(PixelFormat::Yuv420P12Le) => (2, 12, 1),
        Some(PixelFormat::Yuv422P) => (1, 8, 2),
        Some(PixelFormat::Yuv422P10Le) => (3, 10, 2),
        Some(PixelFormat::Yuv444P) => (1, 8, 3),
        Some(PixelFormat::Yuv444P10Le) => (3, 10, 3),
        // Unknown high-bit or unusual layouts: still try 8-bit 4:2:0 profile 0.
        _ => (0, 8, 1),
    };
    let full_range = matches!(
        params.resolved_color_range(),
        oxideav_core::ColorRange::Full
    );
    Ok(Vp9SessionConfig {
        profile,
        bit_depth,
        chroma_subsampling: chroma,
        full_range,
        width,
        height,
    })
}

/// VP9 level idc from the coded picture size and optional frame rate.
///
/// Level is taken from the VP9 level table (MaxLumaPs / MaxLumaSampleRate).
/// When `frame_rate` is unknown the sample-rate check is skipped and only
/// MaxLumaPs applies — that still selects level 4.0 for 1080p and 3.1 for
/// 720p, matching the vpcC ffmpeg writes for the test files. Level 0 was
/// not used: VT accepts a computed level and the bitstream-accurate value
/// avoids under-declaring high-rate content.
fn vp9_level_idc(width: u32, height: u32, frame_rate: Option<oxideav_core::Rational>) -> u8 {
    let luma_ps = (width as u64).saturating_mul(height as u64);
    let sample_rate = frame_rate.and_then(|r| {
        if r.den == 0 {
            None
        } else {
            let fps = r.as_f64();
            if fps.is_finite() && fps > 0.0 {
                Some((luma_ps as f64 * fps).ceil() as u64)
            } else {
                None
            }
        }
    });
    // (level_idc, max_luma_ps, max_luma_sample_rate)
    const LEVELS: &[(u8, u64, u64)] = &[
        (10, 36864, 829440),
        (11, 73728, 2764800),
        (20, 122880, 4608000),
        (21, 245760, 9216000),
        (30, 552960, 20736000),
        (31, 983040, 36864000),
        (40, 2228224, 83558400),
        (41, 2228224, 160432128),
        (50, 8912896, 311951360),
        (51, 8912896, 588251136),
        (52, 8912896, 1176502272),
        (60, 35651584, 1176502272),
        (61, 35651584, 2353004544),
        (62, 35651584, 4706009088),
    ];
    for &(idc, max_ps, max_sr) in LEVELS {
        if luma_ps <= max_ps && sample_rate.map(|sr| sr <= max_sr).unwrap_or(true) {
            return idc;
        }
    }
    62
}

fn build_vpcc(
    config: &Vp9SessionConfig,
    color_signal: oxideav_core::ColorSignal,
    frame_rate: Option<oxideav_core::Rational>,
) -> [u8; 12] {
    let level = vp9_level_idc(config.width, config.height, frame_rate);
    let bit_depth = config.bit_depth.min(15);
    let chroma = config.chroma_subsampling.min(3);
    let full = u8::from(config.full_range);
    let colour_byte = (bit_depth << 4) | (chroma << 1) | full;

    // Prefer stream colour when known; otherwise leave H.273 "unspecified" (2).
    let (prim, xfer, mtrx) = vpcc_colour_bytes(color_signal);

    [
        0x01,
        0x00,
        0x00,
        0x00,
        config.profile,
        level,
        colour_byte,
        prim,
        xfer,
        mtrx,
        0x00,
        0x00,
    ]
}

fn vpcc_colour_bytes(sig: oxideav_core::ColorSignal) -> (u8, u8, u8) {
    let prim = if sig.primaries.is_unspecified() {
        2
    } else {
        sig.primaries.code_point()
    };
    let xfer = if sig.transfer.is_unspecified() {
        2
    } else {
        sig.transfer.code_point()
    };
    let mtrx = if sig.matrix.is_unspecified() {
        2
    } else {
        sig.matrix.code_point()
    };
    (prim, xfer, mtrx)
}

fn create_vp9_format(
    config: &Vp9SessionConfig,
    color_signal: oxideav_core::ColorSignal,
    frame_rate: Option<oxideav_core::Rational>,
) -> Result<CFRetained<CMVideoFormatDescription>, SinkError> {
    let vpcc = build_vpcc(config, color_signal, frame_rate);
    let vpcc_data = CFData::from_bytes(&vpcc);
    let vpcc_key = CFString::from_static_str("vpcC");
    let atoms = unsafe {
        cf_dictionary_owned(&[(
            &*vpcc_key as *const CFString as *const std::ffi::c_void,
            &*vpcc_data as *const CFData as *const std::ffi::c_void,
        )])
    }
    .ok_or_else(|| SinkError::Fallback("could not build vpcC atoms dictionary".into()))?;

    // Extensions always carry SampleDescriptionExtensionAtoms; colour keys when known.
    let atoms_key = unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms };
    let mut entries: Vec<(*const std::ffi::c_void, *const std::ffi::c_void)> = vec![(
        atoms_key as *const CFString as *const std::ffi::c_void,
        &*atoms as *const _ as *const std::ffi::c_void,
    )];

    // Hold colour CFString values for the duration of Create.
    let color_refs;
    let full_range_bool;
    if color_is_known(color_signal) {
        color_refs = Some(cm_color_extension_values(color_signal));
        let (prim, xfer, mtrx) = color_refs.as_ref().unwrap();
        entries.push((
            unsafe { kCMFormatDescriptionExtension_ColorPrimaries } as *const CFString
                as *const std::ffi::c_void,
            *prim as *const CFString as *const std::ffi::c_void,
        ));
        entries.push((
            unsafe { kCMFormatDescriptionExtension_TransferFunction } as *const CFString
                as *const std::ffi::c_void,
            *xfer as *const CFString as *const std::ffi::c_void,
        ));
        entries.push((
            unsafe { kCMFormatDescriptionExtension_YCbCrMatrix } as *const CFString
                as *const std::ffi::c_void,
            *mtrx as *const CFString as *const std::ffi::c_void,
        ));
        if let Some(full) = color_signal.range.full_range_flag() {
            full_range_bool = Some(CFBoolean::new(full));
            entries.push((
                unsafe { kCMFormatDescriptionExtension_FullRangeVideo } as *const CFString
                    as *const std::ffi::c_void,
                full_range_bool.unwrap() as *const CFBoolean as *const std::ffi::c_void,
            ));
        } else {
            full_range_bool = None;
        }
    } else {
        color_refs = None;
        full_range_bool = None;
    }
    let _keep = (&color_refs, full_range_bool, &atoms, &vpcc_data, &vpcc_key);

    let extensions = unsafe { cf_dictionary_owned(&entries) }
        .ok_or_else(|| SinkError::Fallback("could not build VP9 format extensions".into()))?;

    let mut raw: *const CMVideoFormatDescription = ptr::null();
    let status = unsafe {
        CMVideoFormatDescriptionCreate(
            None,
            super::vt_caps::CODEC_VP9,
            config.width as i32,
            config.height as i32,
            Some(&extensions),
            NonNull::from(&mut raw),
        )
    };
    if status != 0 || raw.is_null() {
        return Err(SinkError::Fallback(format!(
            "CMVideoFormatDescriptionCreate(vp09): {status}"
        )));
    }
    Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw.cast_mut())) })
}

fn color_is_known(sig: oxideav_core::ColorSignal) -> bool {
    !sig.primaries.is_unspecified()
        || !sig.transfer.is_unspecified()
        || !sig.matrix.is_unspecified()
        || sig.range.full_range_flag().is_some()
}

fn cm_color_extension_values(
    sig: oxideav_core::ColorSignal,
) -> (&'static CFString, &'static CFString, &'static CFString) {
    // Map through the same code points resolve_colorimetry uses, but to the
    // CMFormatDescription colour constants (same CFString values as CV's).
    let prim_cp = sig.primaries.code_point();
    let xfer_cp = sig.transfer.code_point();
    let mtrx_cp = sig.matrix.code_point();
    unsafe {
        let prim = match prim_cp {
            1 => kCMFormatDescriptionColorPrimaries_ITU_R_709_2,
            5 => kCMFormatDescriptionColorPrimaries_EBU_3213,
            6 | 7 => kCMFormatDescriptionColorPrimaries_SMPTE_C,
            9 => kCMFormatDescriptionColorPrimaries_ITU_R_2020,
            11 => kCMFormatDescriptionColorPrimaries_DCI_P3,
            12 => kCMFormatDescriptionColorPrimaries_P3_D65,
            _ => match mtrx_cp {
                5 | 6 => kCMFormatDescriptionColorPrimaries_SMPTE_C,
                9 | 10 => kCMFormatDescriptionColorPrimaries_ITU_R_2020,
                _ => kCMFormatDescriptionColorPrimaries_ITU_R_709_2,
            },
        };
        let xfer = match xfer_cp {
            1 | 6 | 14 | 15 => kCMFormatDescriptionTransferFunction_ITU_R_709_2,
            7 => kCMFormatDescriptionTransferFunction_SMPTE_240M_1995,
            8 => kCMFormatDescriptionTransferFunction_Linear,
            13 => kCMFormatDescriptionTransferFunction_sRGB,
            16 => kCMFormatDescriptionTransferFunction_SMPTE_ST_2084_PQ,
            18 => kCMFormatDescriptionTransferFunction_ITU_R_2100_HLG,
            _ => kCMFormatDescriptionTransferFunction_ITU_R_709_2,
        };
        let mtrx = match mtrx_cp {
            1 => kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2,
            5 | 6 => kCMFormatDescriptionYCbCrMatrix_ITU_R_601_4,
            7 => kCMFormatDescriptionYCbCrMatrix_SMPTE_240M_1995,
            9 | 10 => kCMFormatDescriptionYCbCrMatrix_ITU_R_2020,
            _ => match prim_cp {
                5 | 6 | 7 => kCMFormatDescriptionYCbCrMatrix_ITU_R_601_4,
                9 => kCMFormatDescriptionYCbCrMatrix_ITU_R_2020,
                _ => kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2,
            },
        };
        (prim, xfer, mtrx)
    }
}

unsafe fn cf_dictionary_owned(
    entries: &[(*const std::ffi::c_void, *const std::ffi::c_void)],
) -> Option<CFRetained<objc2_core_foundation::CFDictionary>> {
    let mut keys: Vec<*const std::ffi::c_void> = Vec::with_capacity(entries.len());
    let mut vals: Vec<*const std::ffi::c_void> = Vec::with_capacity(entries.len());
    for &(k, v) in entries {
        keys.push(k);
        vals.push(v);
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

/// Pack planar 8-bit YUV420P into biplanar NV12 (Y + interleaved UV).
pub(crate) fn pack_yuv420p_to_nv12(
    y_src: &[u8],
    y_src_stride: usize,
    u_src: &[u8],
    u_src_stride: usize,
    v_src: &[u8],
    v_src_stride: usize,
    y_dst: &mut [u8],
    y_dst_stride: usize,
    uv_dst: &mut [u8],
    uv_dst_stride: usize,
    width: usize,
    height: usize,
) {
    for r in 0..height {
        let src_start = r * y_src_stride;
        let dst_start = r * y_dst_stride;
        if src_start + width > y_src.len() || dst_start + width > y_dst.len() {
            continue;
        }
        y_dst[dst_start..dst_start + width].copy_from_slice(&y_src[src_start..src_start + width]);
    }

    let chroma_h = height.div_ceil(2);
    let chroma_w = width.div_ceil(2);
    for r in 0..chroma_h {
        let u_start = r * u_src_stride;
        let v_start = r * v_src_stride;
        let uv_start = r * uv_dst_stride;
        if u_start + chroma_w > u_src.len()
            || v_start + chroma_w > v_src.len()
            || uv_start + chroma_w * 2 > uv_dst.len()
        {
            continue;
        }
        let u_row = &u_src[u_start..u_start + chroma_w];
        let v_row = &v_src[v_start..v_start + chroma_w];
        let uv_row = &mut uv_dst[uv_start..uv_start + chroma_w * 2];
        for c in 0..chroma_w {
            uv_row[c * 2] = u_row[c];
            uv_row[c * 2 + 1] = v_row[c];
        }
    }
}

unsafe fn copy_yuv420p_into_nv12_pixel_buffer(
    pixel: &CFRetained<CVPixelBuffer>,
    y_plane: &oxideav_core::VideoPlane,
    u_plane: &oxideav_core::VideoPlane,
    v_plane: &oxideav_core::VideoPlane,
    width: usize,
    height: usize,
) -> Result<(), SinkError> {
    let lock = unsafe { CVPixelBufferLockBaseAddress(pixel, CVPixelBufferLockFlags(0)) };
    if lock != 0 {
        return Err(SinkError::Fatal(format!(
            "CVPixelBufferLockBaseAddress: {lock}"
        )));
    }
    let result = (|| {
        if CVPixelBufferGetPlaneCount(pixel) != 2 {
            return Err(SinkError::Fatal(
                "NV12 pixel buffer does not have 2 planes".into(),
            ));
        }
        let y_base = CVPixelBufferGetBaseAddressOfPlane(pixel, 0);
        let uv_base = CVPixelBufferGetBaseAddressOfPlane(pixel, 1);
        if y_base.is_null() || uv_base.is_null() {
            return Err(SinkError::Fatal("pixel buffer plane base null".into()));
        }
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel, 0);
        let y_h = CVPixelBufferGetHeightOfPlane(pixel, 0);
        let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel, 1);
        let uv_h = CVPixelBufferGetHeightOfPlane(pixel, 1);
        let y_dst = unsafe { std::slice::from_raw_parts_mut(y_base as *mut u8, y_stride * y_h) };
        let uv_dst = unsafe { std::slice::from_raw_parts_mut(uv_base as *mut u8, uv_stride * uv_h) };
        pack_yuv420p_to_nv12(
            &y_plane.data,
            y_plane.stride,
            &u_plane.data,
            u_plane.stride,
            &v_plane.data,
            v_plane.stride,
            y_dst,
            y_stride,
            uv_dst,
            uv_stride,
            width,
            height,
        );
        Ok(())
    })();
    unsafe { CVPixelBufferUnlockBaseAddress(pixel, CVPixelBufferLockFlags(0)) };
    result
}

/// The format description for `kind` from its parameter sets and `headers`, with the NAL
/// length size, Annex-B flag and reorder depth the samples need.
fn create_format(
    kind: CompressedKind,
    params: &CodecParameters,
    headers: &StreamHeaders,
) -> Result<(CFRetained<CMVideoFormatDescription>, usize, bool, usize), SinkError> {
    match kind {
        CompressedKind::Av1 => Ok((create_av1_format(headers, params.color_signal)?, 0, false, 0)),
        _ => create_h264_or_hevc_format(kind, params, headers),
    }
}

/// An `av01` format: the `av1C` record as the sample entry's atom, the stream's colour, and
/// HDR10's mastering display and content light when the stream carries them.
fn create_av1_format(
    headers: &StreamHeaders,
    color_signal: oxideav_core::ColorSignal,
) -> Result<CFRetained<CMVideoFormatDescription>, SinkError> {
    let av1 = headers
        .av1
        .as_ref()
        .ok_or_else(|| SinkError::Fallback("no AV1 sequence header in the setup bytes".into()))?;
    let av1c_key = CFString::from_static_str("av1C");
    let av1c = CFData::from_bytes(&av1.av1c);
    let atoms = unsafe { cf_dictionary(&[(&av1c_key, &av1c as &CFType)]) }
        .ok_or_else(|| SinkError::Fallback("could not build av1C atoms".into()))?;
    let mastering = headers.mastering.map(|m| CFData::from_bytes(&m));
    let light = headers.content_light.map(|c| CFData::from_bytes(&c));
    let mut entries: Vec<(&CFString, &CFType)> =
        vec![(unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms }, &atoms as &CFType)];
    if color_is_known(color_signal) {
        let (primaries, transfer, matrix) = cm_color_extension_values(color_signal);
        entries.push((unsafe { kCMFormatDescriptionExtension_ColorPrimaries }, primaries as &CFType));
        entries.push((unsafe { kCMFormatDescriptionExtension_TransferFunction }, transfer as &CFType));
        entries.push((unsafe { kCMFormatDescriptionExtension_YCbCrMatrix }, matrix as &CFType));
        if let Some(full) = color_signal.range.full_range_flag() {
            entries.push((unsafe { kCMFormatDescriptionExtension_FullRangeVideo }, CFBoolean::new(full) as &CFType));
        }
    }
    if let Some(m) = &mastering {
        entries.push((unsafe { kCMFormatDescriptionExtension_MasteringDisplayColorVolume }, m as &CFType));
    }
    if let Some(c) = &light {
        entries.push((unsafe { kCMFormatDescriptionExtension_ContentLightLevelInfo }, c as &CFType));
    }
    let extensions = unsafe { cf_dictionary(&entries) }
        .ok_or_else(|| SinkError::Fallback("could not build AV1 format extensions".into()))?;
    let mut raw: *const CMVideoFormatDescription = ptr::null();
    let status = unsafe {
        CMVideoFormatDescriptionCreate(
            None,
            super::vt_caps::CODEC_AV1,
            av1.width as i32,
            av1.height as i32,
            Some(&extensions),
            NonNull::from(&mut raw),
        )
    };
    if status != 0 || raw.is_null() {
        return Err(SinkError::Fallback(format!("CMVideoFormatDescriptionCreate(av01): {status}")));
    }
    Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw.cast_mut())) })
}

fn create_h264_or_hevc_format(
    kind: CompressedKind,
    params: &CodecParameters,
    headers: &StreamHeaders,
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
            CompressedKind::Vp9 | CompressedKind::Av1 => {
                return Err(SinkError::Fallback(format!("create_h264_or_hevc_format called for {kind:?}")));
            }
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
    // HDR10's static metadata (HEVC: the H.264 constructor takes no extensions) and, for
    // Dolby Vision, the HEVC and DV records as the sample entry's atoms.
    let mastering = headers.mastering.map(|m| CFData::from_bytes(&m));
    let light = headers.content_light.map(|c| CFData::from_bytes(&c));
    let dovi = headers.dovi.as_ref().map(|d| {
        (
            CFString::from_static_str("hvcC"),
            CFString::from_static_str(d.atom),
            CFData::from_bytes(&d.hvcc),
            CFData::from_bytes(&d.record),
        )
    });
    let atoms = match &dovi {
        Some((hvcc_key, dovi_key, hvcc, record)) => Some(
            unsafe { cf_dictionary(&[(hvcc_key, hvcc as &CFType), (dovi_key, record as &CFType)]) }
                .ok_or_else(|| SinkError::Fallback("could not build Dolby Vision atoms".into()))?,
        ),
        None => None,
    };
    let mut entries: Vec<(&CFString, &CFType)> = Vec::new();
    if let Some(m) = &mastering {
        entries.push((unsafe { kCMFormatDescriptionExtension_MasteringDisplayColorVolume }, m as &CFType));
    }
    if let Some(c) = &light {
        entries.push((unsafe { kCMFormatDescriptionExtension_ContentLightLevelInfo }, c as &CFType));
    }
    if let Some(a) = &atoms {
        entries.push((unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms }, a as &CFType));
    }
    let extensions = if entries.is_empty() {
        None
    } else {
        Some(
            unsafe { cf_dictionary(&entries) }
                .ok_or_else(|| SinkError::Fallback("could not build format extensions".into()))?,
        )
    };
    let mut raw: *const CMFormatDescription = ptr::null();
    let status = match (kind, &headers.dovi) {
        (CompressedKind::Hevc, Some(dv)) => unsafe {
            CMVideoFormatDescriptionCreate(
                None,
                if dv.dvh1 { kCMVideoCodecType_DolbyVisionHEVC } else { kCMVideoCodecType_HEVC },
                dv.width as i32,
                dv.height as i32,
                extensions.as_deref(),
                NonNull::from(&mut raw).cast(),
            )
        },
        (CompressedKind::H264, _) => unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                nals.len(),
                NonNull::from(ptrs.as_mut_slice()).cast(),
                NonNull::from(sizes.as_mut_slice()).cast(),
                length_size as i32,
                NonNull::from(&mut raw),
            )
        },
        (CompressedKind::Hevc, None) => unsafe {
            CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                None,
                nals.len(),
                NonNull::from(ptrs.as_mut_slice()).cast(),
                NonNull::from(sizes.as_mut_slice()).cast(),
                length_size as i32,
                extensions.as_deref(),
                NonNull::from(&mut raw),
            )
        },
        (CompressedKind::Vp9 | CompressedKind::Av1, _) => {
            return Err(SinkError::Fallback(format!("create_h264_or_hevc_format called for {kind:?}")));
        }
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
                CompressedKind::Vp9 => None,
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
    entries: &[(&objc2_core_foundation::CFString, &objc2_core_foundation::CFType)],
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

pub(crate) fn pack_yuv420p10le_to_p010(
    y_src: &[u8],
    y_src_stride: usize,
    u_src: &[u8],
    u_src_stride: usize,
    v_src: &[u8],
    v_src_stride: usize,
    y_dst: &mut [u8],
    y_dst_stride: usize,
    uv_dst: &mut [u8],
    uv_dst_stride: usize,
    width: usize,
    height: usize,
) {
    for r in 0..height {
        let src_start = r * y_src_stride;
        let dst_start = r * y_dst_stride;
        if src_start + width * 2 > y_src.len() || dst_start + width * 2 > y_dst.len() {
            continue;
        }
        let src_row = &y_src[src_start..src_start + width * 2];
        let dst_row = &mut y_dst[dst_start..dst_start + width * 2];
        for c in 0..width {
            let sample = u16::from_le_bytes([src_row[c * 2], src_row[c * 2 + 1]]);
            let shifted = sample << 6;
            dst_row[c * 2..c * 2 + 2].copy_from_slice(&shifted.to_ne_bytes());
        }
    }

    let chroma_h = height.div_ceil(2);
    let chroma_w = width.div_ceil(2);
    for r in 0..chroma_h {
        let u_start = r * u_src_stride;
        let v_start = r * v_src_stride;
        let uv_start = r * uv_dst_stride;
        if u_start + chroma_w * 2 > u_src.len()
            || v_start + chroma_w * 2 > v_src.len()
            || uv_start + chroma_w * 4 > uv_dst.len()
        {
            continue;
        }
        let u_row = &u_src[u_start..u_start + chroma_w * 2];
        let v_row = &v_src[v_start..v_start + chroma_w * 2];
        let uv_row = &mut uv_dst[uv_start..uv_start + chroma_w * 4];
        for c in 0..chroma_w {
            let u_sample = u16::from_le_bytes([u_row[c * 2], u_row[c * 2 + 1]]);
            let v_sample = u16::from_le_bytes([v_row[c * 2], v_row[c * 2 + 1]]);
            let u_shifted = u_sample << 6;
            let v_shifted = v_sample << 6;
            uv_row[c * 4..c * 4 + 2].copy_from_slice(&u_shifted.to_ne_bytes());
            uv_row[c * 4 + 2..c * 4 + 4].copy_from_slice(&v_shifted.to_ne_bytes());
        }
    }
}

pub(crate) fn resolve_colorimetry(
    frame_color: Option<oxideav_core::ColorSignal>,
    stream_color: oxideav_core::ColorSignal,
) -> (
    &'static objc2_core_foundation::CFString,
    &'static objc2_core_foundation::CFString,
    &'static objc2_core_foundation::CFString,
) {
    let sig = frame_color
        .map(|s| s.or(stream_color))
        .unwrap_or(stream_color);

    let prim_cp = sig.primaries.code_point();
    let xfer_cp = sig.transfer.code_point();
    let mtrx_cp = sig.matrix.code_point();

    unsafe {
        let resolved_prim = match prim_cp {
            1 => kCVImageBufferColorPrimaries_ITU_R_709_2,
            5 => kCVImageBufferColorPrimaries_EBU_3213,
            6 | 7 => kCVImageBufferColorPrimaries_SMPTE_C,
            9 => kCVImageBufferColorPrimaries_ITU_R_2020,
            11 => kCVImageBufferColorPrimaries_DCI_P3,
            12 => kCVImageBufferColorPrimaries_P3_D65,
            _ => match mtrx_cp {
                5 | 6 => kCVImageBufferColorPrimaries_SMPTE_C,
                9 | 10 => kCVImageBufferColorPrimaries_ITU_R_2020,
                _ => kCVImageBufferColorPrimaries_ITU_R_709_2,
            },
        };

        let resolved_xfer = match xfer_cp {
            1 | 6 | 14 | 15 => kCVImageBufferTransferFunction_ITU_R_709_2,
            7 => kCVImageBufferTransferFunction_SMPTE_240M_1995,
            8 => kCVImageBufferTransferFunction_Linear,
            13 => kCVImageBufferTransferFunction_sRGB,
            16 => kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
            18 => kCVImageBufferTransferFunction_ITU_R_2100_HLG,
            _ => kCVImageBufferTransferFunction_ITU_R_709_2,
        };

        let resolved_mtrx = match mtrx_cp {
            1 => kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            5 | 6 => kCVImageBufferYCbCrMatrix_ITU_R_601_4,
            7 => kCVImageBufferYCbCrMatrix_SMPTE_240M_1995,
            9 | 10 => kCVImageBufferYCbCrMatrix_ITU_R_2020,
            _ => match prim_cp {
                5 | 6 | 7 => kCVImageBufferYCbCrMatrix_ITU_R_601_4,
                9 => kCVImageBufferYCbCrMatrix_ITU_R_2020,
                _ => kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            },
        };

        (resolved_prim, resolved_xfer, resolved_mtrx)
    }
}

pub(crate) fn attach_frame_metadata(
    pixel: &CVPixelBuffer,
    frame: &VideoFrame,
    stream_color: oxideav_core::ColorSignal,
    stream_sar: Option<oxideav_core::Rational>,
) {
    let (primaries, transfer, matrix) = resolve_colorimetry(frame.color_signal(), stream_color);

    unsafe {
        CVBuffer::set_attachment(
            pixel,
            kCVImageBufferColorPrimariesKey,
            primaries,
            CVAttachmentMode::ShouldPropagate,
        );
        CVBuffer::set_attachment(
            pixel,
            kCVImageBufferTransferFunctionKey,
            transfer,
            CVAttachmentMode::ShouldPropagate,
        );
        CVBuffer::set_attachment(
            pixel,
            kCVImageBufferYCbCrMatrixKey,
            matrix,
            CVAttachmentMode::ShouldPropagate,
        );
    }

    if let Some(sar) = stream_sar {
        if sar.num > 0 && sar.den > 0 && sar.num != sar.den {
            let h_num = CFNumber::new_i32(sar.num as i32);
            let v_num = CFNumber::new_i32(sar.den as i32);
            if let Some(par_dict) = unsafe {
                cf_dictionary(&[
                    (
                        kCVImageBufferPixelAspectRatioHorizontalSpacingKey,
                        &h_num as &objc2_core_foundation::CFType,
                    ),
                    (
                        kCVImageBufferPixelAspectRatioVerticalSpacingKey,
                        &v_num as &objc2_core_foundation::CFType,
                    ),
                ])
            } {
                unsafe {
                    CVBuffer::set_attachment(
                        pixel,
                        kCVImageBufferPixelAspectRatioKey,
                        &par_dict,
                        CVAttachmentMode::ShouldPropagate,
                    );
                }
                return;
            }
        }
    }

    unsafe {
        CVBuffer::remove_attachment(pixel, kCVImageBufferPixelAspectRatioKey);
    }
}

unsafe fn copy_planes_into_pixel_buffer(
    pixel: &CFRetained<CVPixelBuffer>,
    frame: &VideoFrame,
    format: PixelFormat,
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
        let pixel_format = CVPixelBufferGetPixelFormatType(pixel);
        let is_p010 = format == PixelFormat::Yuv420P10Le
            || pixel_format == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
            || pixel_format == kCVPixelFormatType_420YpCbCr10BiPlanarFullRange;

        if is_p010 && planes == 2 {
            if frame.planes.len() < 3 {
                return Err(SinkError::Fatal(
                    "Yuv420P10Le frame requires 3 planes".into(),
                ));
            }
            let y_plane = &frame.planes[0];
            let u_plane = &frame.planes[1];
            let v_plane = &frame.planes[2];

            let y_base = CVPixelBufferGetBaseAddressOfPlane(pixel, 0);
            let uv_base = CVPixelBufferGetBaseAddressOfPlane(pixel, 1);
            if y_base.is_null() || uv_base.is_null() {
                return Err(SinkError::Fatal("pixel buffer plane base null".into()));
            }
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel, 0);
            let y_h = CVPixelBufferGetHeightOfPlane(pixel, 0);
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel, 1);
            let uv_h = CVPixelBufferGetHeightOfPlane(pixel, 1);

            let y_dst = unsafe {
                std::slice::from_raw_parts_mut(y_base as *mut u8, y_stride * y_h)
            };
            let uv_dst = unsafe {
                std::slice::from_raw_parts_mut(uv_base as *mut u8, uv_stride * uv_h)
            };

            pack_yuv420p10le_to_p010(
                &y_plane.data,
                y_plane.stride,
                &u_plane.data,
                u_plane.stride,
                &v_plane.data,
                v_plane.stride,
                y_dst,
                y_stride,
                uv_dst,
                uv_stride,
                width as usize,
                height as usize,
            );
            return Ok(());
        }

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
    _width: u32,
    _height: u32,
) -> Result<CFRetained<CMVideoFormatDescription>, SinkError> {
    let mut raw: *const CMVideoFormatDescription = ptr::null();
    let status = unsafe {
        CMVideoFormatDescriptionCreateForImageBuffer(
            None,
            pixel,
            NonNull::from(&mut raw),
        )
    };
    if status != 0 || raw.is_null() {
        return Err(SinkError::Fatal(format!(
            "CMVideoFormatDescriptionCreateForImageBuffer: {status}"
        )));
    }
    Ok(unsafe {
        CFRetained::from_raw(NonNull::new_unchecked(
            raw.cast_mut(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{
        ColorPrimaries, ColorSignal, MatrixCoefficients, Rational, TransferCharacteristics,
        VideoPlane,
    };

    #[test]
    fn test_pack_yuv420p10le_to_p010_shifts_and_interleaves() {
        let width = 4;
        let height = 4;
        let y_src_stride = 12; // 4 samples * 2 = 8 bytes + 4 padding
        let u_src_stride = 8;  // 2 samples * 2 = 4 bytes + 4 padding
        let v_src_stride = 8;
        let y_dst_stride = 16; // padded
        let uv_dst_stride = 16;

        let mut y_src = vec![0u8; y_src_stride * height];
        let mut u_src = vec![0u8; u_src_stride * (height / 2)];
        let mut v_src = vec![0u8; v_src_stride * (height / 2)];

        // Row 0 luma: 1023, 512, 0, 1
        let row0_y = [1023u16, 512, 0, 1];
        for (i, &val) in row0_y.iter().enumerate() {
            y_src[i * 2..i * 2 + 2].copy_from_slice(&val.to_le_bytes());
        }

        // Row 1 luma: 512, 1023, 100, 200
        let row1_y = [512u16, 1023, 100, 200];
        for (i, &val) in row1_y.iter().enumerate() {
            let offset = y_src_stride + i * 2;
            y_src[offset..offset + 2].copy_from_slice(&val.to_le_bytes());
        }

        // Row 0 chroma: U = [1023, 512], V = [512, 1023]
        u_src[0..2].copy_from_slice(&1023u16.to_le_bytes());
        u_src[2..4].copy_from_slice(&512u16.to_le_bytes());
        v_src[0..2].copy_from_slice(&512u16.to_le_bytes());
        v_src[2..4].copy_from_slice(&1023u16.to_le_bytes());

        let mut y_dst = vec![0xAAu8; y_dst_stride * height];
        let mut uv_dst = vec![0xBBu8; uv_dst_stride * (height / 2)];

        pack_yuv420p10le_to_p010(
            &y_src,
            y_src_stride,
            &u_src,
            u_src_stride,
            &v_src,
            v_src_stride,
            &mut y_dst,
            y_dst_stride,
            &mut uv_dst,
            uv_dst_stride,
            width,
            height,
        );

        // Verify row 0 luma:
        // 1023 << 6 = 0xFFC0
        let y0_0 = u16::from_ne_bytes([y_dst[0], y_dst[1]]);
        assert_eq!(y0_0, 0xFFC0);
        // 512 << 6 = 0x8000
        let y0_1 = u16::from_ne_bytes([y_dst[2], y_dst[3]]);
        assert_eq!(y0_1, 0x8000);
        // 0 << 6 = 0x0000
        let y0_2 = u16::from_ne_bytes([y_dst[4], y_dst[5]]);
        assert_eq!(y0_2, 0x0000);
        // 1 << 6 = 0x0040
        let y0_3 = u16::from_ne_bytes([y_dst[6], y_dst[7]]);
        assert_eq!(y0_3, 0x0040);
        // Verify padding unaffected in row 0
        assert_eq!(y_dst[8..16], [0xAA; 8]);

        // Verify row 1 luma:
        let y1_0 = u16::from_ne_bytes([y_dst[y_dst_stride], y_dst[y_dst_stride + 1]]);
        assert_eq!(y1_0, 0x8000);
        let y1_1 = u16::from_ne_bytes([y_dst[y_dst_stride + 2], y_dst[y_dst_stride + 3]]);
        assert_eq!(y1_1, 0xFFC0);

        // Verify chroma row 0: interleaved (U0, V0, U1, V1)
        // U0 = 1023 -> 0xFFC0
        let uv0_u0 = u16::from_ne_bytes([uv_dst[0], uv_dst[1]]);
        assert_eq!(uv0_u0, 0xFFC0);
        // V0 = 512 -> 0x8000
        let uv0_v0 = u16::from_ne_bytes([uv_dst[2], uv_dst[3]]);
        assert_eq!(uv0_v0, 0x8000);
        // U1 = 512 -> 0x8000
        let uv0_u1 = u16::from_ne_bytes([uv_dst[4], uv_dst[5]]);
        assert_eq!(uv0_u1, 0x8000);
        // V1 = 1023 -> 0xFFC0
        let uv0_v1 = u16::from_ne_bytes([uv_dst[6], uv_dst[7]]);
        assert_eq!(uv0_v1, 0xFFC0);
        // Padding in UV row 0
        assert_eq!(uv_dst[8..16], [0xBB; 8]);
    }

    #[test]
    fn test_color_attachment_resolution() {
        // 1. BT.2020 + SMPTE_ST2084 PQ
        let pq_sig = ColorSignal::new(
            oxideav_core::ColorRange::Limited,
            ColorPrimaries::BT2020,
            TransferCharacteristics::SMPTE_ST2084,
            MatrixCoefficients::BT2020_NCL,
        );
        let (prim, xfer, mtrx) = resolve_colorimetry(None, pq_sig);
        unsafe {
            assert_eq!(prim, kCVImageBufferColorPrimaries_ITU_R_2020);
            assert_eq!(xfer, kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ);
            assert_eq!(mtrx, kCVImageBufferYCbCrMatrix_ITU_R_2020);
        }

        // 2. BT.2020 + ARIB_STD_B67 HLG
        let hlg_sig = ColorSignal::new(
            oxideav_core::ColorRange::Limited,
            ColorPrimaries::BT2020,
            TransferCharacteristics::ARIB_STD_B67,
            MatrixCoefficients::BT2020_NCL,
        );
        let (prim, xfer, mtrx) = resolve_colorimetry(None, hlg_sig);
        unsafe {
            assert_eq!(prim, kCVImageBufferColorPrimaries_ITU_R_2020);
            assert_eq!(xfer, kCVImageBufferTransferFunction_ITU_R_2100_HLG);
            assert_eq!(mtrx, kCVImageBufferYCbCrMatrix_ITU_R_2020);
        }

        // 3. BT.601 (unspecified primaries & transfer, BT.601 matrix => primaries default to SMPTE_C)
        let bt601_sig = ColorSignal::new(
            oxideav_core::ColorRange::Limited,
            ColorPrimaries::UNSPECIFIED,
            TransferCharacteristics::UNSPECIFIED,
            MatrixCoefficients::BT601_525,
        );
        let (prim, xfer, mtrx) = resolve_colorimetry(None, bt601_sig);
        unsafe {
            assert_eq!(prim, kCVImageBufferColorPrimaries_SMPTE_C);
            assert_eq!(xfer, kCVImageBufferTransferFunction_ITU_R_709_2);
            assert_eq!(mtrx, kCVImageBufferYCbCrMatrix_ITU_R_601_4);
        }

        // 4. Unspecified -> BT.709 defaults
        let unspec_sig = ColorSignal::unspecified();
        let (prim, xfer, mtrx) = resolve_colorimetry(None, unspec_sig);
        unsafe {
            assert_eq!(prim, kCVImageBufferColorPrimaries_ITU_R_709_2);
            assert_eq!(xfer, kCVImageBufferTransferFunction_ITU_R_709_2);
            assert_eq!(mtrx, kCVImageBufferYCbCrMatrix_ITU_R_709_2);
        }
    }

    #[test]
    fn test_p010_pool_and_format_description() {
        let width = 64u32;
        let height = 64u32;
        let pix_fmt_num = CFNumber::new_i32(kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange as i32);
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
        assert_eq!(cvret, 0);
        let pool = unsafe { CFRetained::from_raw(NonNull::new_unchecked(pool_raw)) };

        let pixel = unsafe { pool_pixel_buffer(&pool).expect("pool pixel buffer") };
        assert_eq!(
            CVPixelBufferGetPixelFormatType(&pixel),
            kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
        );
        assert_eq!(CVPixelBufferGetPlaneCount(&pixel), 2);

        let stream_color = ColorSignal::new(
            oxideav_core::ColorRange::Limited,
            ColorPrimaries::BT2020,
            TransferCharacteristics::SMPTE_ST2084,
            MatrixCoefficients::BT2020_NCL,
        );
        let frame = VideoFrame {
            pts: None,
            planes: vec![
                VideoPlane { stride: 128, data: vec![0u8; 128 * 64] },
                VideoPlane { stride: 64, data: vec![0u8; 64 * 32] },
                VideoPlane { stride: 64, data: vec![0u8; 64 * 32] },
            ],
        };

        // Attach BT.2020 PQ and 4:3 SAR
        attach_frame_metadata(&pixel, &frame, stream_color, Some(Rational::new(4, 3)));

        let fmt = unsafe {
            format_for_pixel_buffer(&pixel, width, height)
                .expect("format_for_pixel_buffer should succeed")
        };
        let _ = fmt;

        // Remove SAR attachment when square (1:1)
        attach_frame_metadata(&pixel, &frame, stream_color, Some(Rational::new(1, 1)));
    }

    /// Build a minimal VP9 keyframe uncompressed header for tests.
    fn make_vp9_keyframe_header(
        profile: u8,
        bit_depth: u8,
        color_space: u8,
        full_range: bool,
        ss_x: u8,
        ss_y: u8,
        width: u32,
        height: u32,
        show_existing: bool,
        is_key: bool,
    ) -> Vec<u8> {
        let mut bits: Vec<bool> = Vec::new();
        let push = |bits: &mut Vec<bool>, n: usize, v: u32| {
            for i in (0..n).rev() {
                bits.push(((v >> i) & 1) != 0);
            }
        };
        push(&mut bits, 2, 2); // frame_marker
        push(&mut bits, 1, u32::from(profile & 1));
        push(&mut bits, 1, u32::from((profile >> 1) & 1));
        if profile == 3 {
            push(&mut bits, 1, 0);
        }
        push(&mut bits, 1, u32::from(show_existing));
        if show_existing {
            // rest unused for our parser
            while bits.len() % 8 != 0 {
                bits.push(false);
            }
            let mut out = Vec::new();
            for chunk in bits.chunks(8) {
                let mut b = 0u8;
                for (i, &bit) in chunk.iter().enumerate() {
                    if bit {
                        b |= 1 << (7 - i);
                    }
                }
                out.push(b);
            }
            return out;
        }
        push(&mut bits, 1, u32::from(!is_key)); // frame_type 0=key
        push(&mut bits, 1, 1); // show_frame
        push(&mut bits, 1, 0); // error_resilient
        push(&mut bits, 8, 0x49);
        push(&mut bits, 8, 0x83);
        push(&mut bits, 8, 0x42);
        if profile >= 2 {
            push(&mut bits, 1, u32::from(bit_depth == 12));
        }
        push(&mut bits, 3, u32::from(color_space));
        if color_space != 7 {
            push(&mut bits, 1, u32::from(full_range));
            if profile == 1 || profile == 3 {
                push(&mut bits, 1, u32::from(ss_x));
                push(&mut bits, 1, u32::from(ss_y));
                push(&mut bits, 1, 0);
            }
        } else if profile == 1 || profile == 3 {
            push(&mut bits, 1, 0);
        }
        push(&mut bits, 16, width.saturating_sub(1));
        push(&mut bits, 16, height.saturating_sub(1));
        while bits.len() % 8 != 0 {
            bits.push(false);
        }
        let mut out = Vec::new();
        for chunk in bits.chunks(8) {
            let mut b = 0u8;
            for (i, &bit) in chunk.iter().enumerate() {
                if bit {
                    b |= 1 << (7 - i);
                }
            }
            out.push(b);
        }
        out
    }

    #[test]
    fn test_parse_vp9_keyframe_header_profile0_8bit() {
        let h = make_vp9_keyframe_header(0, 8, 0, false, 1, 1, 1920, 1080, false, true);
        let info = parse_vp9_keyframe_header(&h).expect("p0 keyframe");
        assert_eq!(info.profile, 0);
        assert_eq!(info.bit_depth, 8);
        assert!(!info.full_range);
        assert_eq!(info.subsampling_x, 1);
        assert_eq!(info.subsampling_y, 1);
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        // Matches the real 1080p test file head.
        let real = hex_bytes("824983420077f04376");
        let real_info = parse_vp9_keyframe_header(&real).expect("real p0");
        assert_eq!(real_info.width, 1920);
        assert_eq!(real_info.height, 1080);
        assert_eq!(real_info.profile, 0);
        assert_eq!(real_info.bit_depth, 8);
    }

    #[test]
    fn test_parse_vp9_keyframe_header_profile2_10bit() {
        let h = make_vp9_keyframe_header(2, 10, 0, false, 1, 1, 1280, 720, false, true);
        let info = parse_vp9_keyframe_header(&h).expect("p2 keyframe");
        assert_eq!(info.profile, 2);
        assert_eq!(info.bit_depth, 10);
        assert_eq!(info.width, 1280);
        assert_eq!(info.height, 720);
        let real = hex_bytes("924983420027f8167b");
        let real_info = parse_vp9_keyframe_header(&real).expect("real p2");
        assert_eq!(real_info.profile, 2);
        assert_eq!(real_info.bit_depth, 10);
        assert_eq!(real_info.width, 1280);
        assert_eq!(real_info.height, 720);
    }

    #[test]
    fn test_parse_vp9_keyframe_header_profile1_444() {
        let h = make_vp9_keyframe_header(1, 8, 0, false, 0, 0, 64, 48, false, true);
        let info = parse_vp9_keyframe_header(&h).expect("p1 4:4:4");
        assert_eq!(info.profile, 1);
        assert_eq!(info.bit_depth, 8);
        assert_eq!(info.subsampling_x, 0);
        assert_eq!(info.subsampling_y, 0);
        assert_eq!(info.width, 64);
        assert_eq!(info.height, 48);
    }

    #[test]
    fn test_parse_vp9_keyframe_header_rejects_non_key_and_truncated() {
        let show_existing =
            make_vp9_keyframe_header(0, 8, 0, false, 1, 1, 64, 64, true, true);
        assert!(parse_vp9_keyframe_header(&show_existing).is_none());
        let inter = make_vp9_keyframe_header(0, 8, 0, false, 1, 1, 64, 64, false, false);
        assert!(parse_vp9_keyframe_header(&inter).is_none());
        assert!(parse_vp9_keyframe_header(&[0x82, 0x49]).is_none());
        assert!(parse_vp9_keyframe_header(&[]).is_none());
    }

    #[test]
    fn test_vpcc_matches_ffmpeg_layout() {
        let cfg0 = Vp9SessionConfig {
            profile: 0,
            bit_depth: 8,
            chroma_subsampling: 1,
            full_range: false,
            width: 1920,
            height: 1080,
        };
        let bytes0 = build_vpcc(
            &cfg0,
            ColorSignal::unspecified(),
            Some(Rational::new(30, 1)),
        );
        assert_eq!(
            bytes0,
            [0x01, 0x00, 0x00, 0x00, 0x00, 0x28, 0x82, 0x02, 0x02, 0x02, 0x00, 0x00]
        );
        let cfg2 = Vp9SessionConfig {
            profile: 2,
            bit_depth: 10,
            chroma_subsampling: 1,
            full_range: false,
            width: 1280,
            height: 720,
        };
        let bytes2 = build_vpcc(
            &cfg2,
            ColorSignal::unspecified(),
            Some(Rational::new(30, 1)),
        );
        assert_eq!(
            bytes2,
            [0x01, 0x00, 0x00, 0x00, 0x02, 0x1f, 0xa2, 0x02, 0x02, 0x02, 0x00, 0x00]
        );
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn test_pack_yuv420p_to_nv12_matches_convert_path() {
        // Odd sizes + padded strides; compare one-pass pack vs convert→copy.
        for &(width, height, y_ss, u_ss, v_ss, y_ds, uv_ds) in &[
            (4usize, 4usize, 8usize, 4usize, 4usize, 16usize, 16usize),
            (5, 3, 8, 4, 4, 16, 16),
            (7, 5, 16, 8, 8, 24, 24),
            (1, 1, 4, 2, 2, 8, 8),
        ] {
            let chroma_h = height.div_ceil(2);
            let chroma_w = width.div_ceil(2);
            let mut y_src = vec![0u8; y_ss * height];
            let mut u_src = vec![0u8; u_ss * chroma_h];
            let mut v_src = vec![0u8; v_ss * chroma_h];
            for r in 0..height {
                for c in 0..width {
                    y_src[r * y_ss + c] = ((r * 17 + c * 3) % 251) as u8;
                }
            }
            for r in 0..chroma_h {
                for c in 0..chroma_w {
                    u_src[r * u_ss + c] = ((r * 13 + c * 7 + 11) % 251) as u8;
                    v_src[r * v_ss + c] = ((r * 19 + c * 5 + 29) % 251) as u8;
                }
            }

            let mut y_direct = vec![0xAAu8; y_ds * height];
            let mut uv_direct = vec![0xBBu8; uv_ds * chroma_h];
            pack_yuv420p_to_nv12(
                &y_src, y_ss, &u_src, u_ss, &v_src, v_ss, &mut y_direct, y_ds, &mut uv_direct,
                uv_ds, width, height,
            );

            let frame = VideoFrame {
                pts: None,
                planes: vec![
                    VideoPlane {
                        stride: y_ss,
                        data: y_src.clone(),
                    },
                    VideoPlane {
                        stride: u_ss,
                        data: u_src.clone(),
                    },
                    VideoPlane {
                        stride: v_ss,
                        data: v_src.clone(),
                    },
                ],
            };
            let converted = convert::convert(
                &frame,
                PixFrameInfo::new(PixelFormat::Yuv420P, width as u32, height as u32),
                PixelFormat::Nv12,
                &ConvertOptions::default(),
            )
            .expect("convert yuv420p->nv12");
            assert!(converted.planes.len() >= 2);
            let mut y_conv = vec![0xAAu8; y_ds * height];
            let mut uv_conv = vec![0xBBu8; uv_ds * chroma_h];
            // Mirror copy_planes_into_pixel_buffer NV12 plane copy.
            let y_plane = &converted.planes[0];
            let uv_plane = &converted.planes[1];
            for r in 0..height {
                let src_off = r * y_plane.stride;
                let n = width.min(y_plane.stride).min(y_ds);
                if src_off + n <= y_plane.data.len() {
                    y_conv[r * y_ds..r * y_ds + n]
                        .copy_from_slice(&y_plane.data[src_off..src_off + n]);
                }
            }
            for r in 0..chroma_h {
                let src_off = r * uv_plane.stride;
                let row_bytes = (chroma_w * 2).min(uv_plane.stride).min(uv_ds);
                if src_off + row_bytes <= uv_plane.data.len() {
                    uv_conv[r * uv_ds..r * uv_ds + row_bytes]
                        .copy_from_slice(&uv_plane.data[src_off..src_off + row_bytes]);
                }
            }

            assert_eq!(y_direct, y_conv, "Y mismatch {width}x{height}");
            assert_eq!(uv_direct, uv_conv, "UV mismatch {width}x{height}");
        }
    }

}
