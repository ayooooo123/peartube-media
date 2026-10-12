//! Android VideoSink backed by a persistent owner thread via `video_owner`.
//!
//! All MediaCodec and software Surface operations occur on the owner thread.
//! The frontend communicates through a bounded, short-lock mailbox.

use super::backend::BackendShared;
use super::surface::{SurfaceBinding, SurfaceBindingLease, SurfaceId};
use crate::annexb::convert_packet_to_annex_b;
use crate::backend::{
    Clock, PictureReady, ProducerId, SinkError, VideoControl, VideoError, VideoMode, VideoOutput,
    VideoRequest, VideoSink, VideoTarget,
};
use crate::clock::current_monotonic_ns;
use crate::video_owner::{try_spawn, OwnerError, OwnerTicket};
use ndk::hardware_buffer_format::HardwareBufferFormat;
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
    OutputBuffer,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use oxideav_core::{CodecParameters, Packet, PixelFormat, VideoFrame};
use oxideav_pixfmt::FrameInfo;
use parking_lot::{Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

const BUFFER_FLAG_KEY_FRAME: u32 = 1;
const BUFFER_FLAG_CODEC_CONFIG: u32 = 2;
const BUFFER_FLAG_END_OF_STREAM: u32 = 4;

pub(crate) enum MailboxInput {
    Packet {
        producer: ProducerId,
        packet: Packet,
        pts: Duration,
        random_access: bool,
    },
    Frame {
        producer: ProducerId,
        request: VideoRequest,
        frame_epoch: u64,
        frame: VideoFrame,
        pts: Duration,
    },
}

impl MailboxInput {
    pub fn producer(&self) -> ProducerId {
        match self {
            Self::Packet { producer, .. } | Self::Frame { producer, .. } => *producer,
        }
    }
}

/// Owner-held input: credit stays charged until accept-to-native or explicit discard.
pub(crate) struct OwnerHeldInput {
    pub input: MailboxInput,
}

pub(crate) struct MailboxState {
    pub current_request: Option<VideoRequest>,
    pub input: Option<MailboxInput>,
    /// False while mailbox OR owner holds the single shared credit.
    pub input_credit_available: bool,
    pub eos_requested: Option<ProducerId>,
    pub eos_confirmed: Option<ProducerId>,
    pub transition_result: Option<(ProducerId, Result<VideoMode, VideoError>)>,
    pub async_error: Option<(ProducerId, VideoError)>,
    pub playing: bool,
    pub present_from: Duration,
    /// Concrete codec epoch; advanced on every flush/replacement.
    pub codec_epoch: u64,
    /// Invalidates accepted software input on any intervening reset.
    pub frame_epoch: u64,
    pub retire_requested: bool,
    pub configured: bool,
}

pub struct VideoMailbox {
    state: Mutex<MailboxState>,
    wake_owner: Condvar,
    /// Real current request control wake; never a no-op stand-in once set.
    wake_frontend: Mutex<Option<Arc<dyn VideoControl>>>,
}

impl VideoMailbox {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MailboxState {
                current_request: None,
                input: None,
                input_credit_available: true,
                eos_requested: None,
                eos_confirmed: None,
                transition_result: None,
                async_error: None,
                playing: true,
                present_from: Duration::ZERO,
                codec_epoch: 0,
                frame_epoch: 0,
                retire_requested: false,
                configured: false,
            }),
            wake_owner: Condvar::new(),
            wake_frontend: Mutex::new(None),
        }
    }

    fn wake_frontend_outside(&self) {
        let ctrl = self.wake_frontend.lock().clone();
        if let Some(c) = ctrl {
            c.wake();
        }
    }

    pub fn set_request(&self, request: VideoRequest) {
        let mut state = self.state.lock();
        let changed = match &state.current_request {
            Some(existing) => {
                existing.producer != request.producer
                    || existing.output_revision != request.output_revision
                    || existing.seek_generation != request.seek_generation
                    || request_mode_key(&existing.target) != request_mode_key(&request.target)
            }
            None => true,
        };
        if changed {
            let preserving = state.current_request.as_ref().is_some_and(|old| {
                matches!(old.target, VideoTarget::Frames { .. })
                    && matches!(request.target, VideoTarget::Frames { reset: false, .. })
                    && old.seek_generation == request.seek_generation
                    && old.output_revision == request.output_revision
            });
            if !preserving {
                state.frame_epoch = state.frame_epoch.checked_add(1).expect("frame epoch exhausted");
            }
        }
        *self.wake_frontend.lock() = Some(request.control.clone());
        state.current_request = Some(request);
        if changed {
            state.transition_result = None;
            state.async_error = None;
            state.eos_requested = None;
            state.eos_confirmed = None;
            state.configured = false;
            // Do not drop owner-held credit here; owner discards stale producer input.
            self.wake_owner.notify_all();
        }
    }

    pub fn request_retire(&self) {
        let mut state = self.state.lock();
        state.retire_requested = true;
        self.wake_owner.notify_all();
    }

    pub fn poll_transition_result(
        &self,
        producer: ProducerId,
    ) -> Poll<Result<VideoMode, VideoError>> {
        let state = self.state.lock();
        // Error precedes cached success (C7).
        if let Some((p, err)) = &state.async_error {
            if *p == producer {
                return Poll::Ready(Err(err.clone()));
            }
        }
        if let Some((p, res)) = &state.transition_result {
            if *p == producer {
                return Poll::Ready(res.clone());
            }
        }
        Poll::Pending
    }

    pub fn push_packet(
        &self,
        producer: ProducerId,
        packet: &mut Option<Packet>,
        pts: Duration,
        random_access: bool,
    ) -> Result<(), VideoError> {
        let mut state = self.state.lock();

        match &state.current_request {
            Some(req) if req.producer == producer => {
                if req.control.cancelled(producer, req.seek_generation) {
                    return Err(VideoError::Superseded);
                }
            }
            Some(_) => return Err(VideoError::Superseded),
            None => {}
        }

        if let Some((p, err)) = &state.async_error {
            if *p == producer {
                return Err(err.clone());
            }
        }

        if state.eos_requested == Some(producer) {
            return Err(VideoError::Sink(SinkError::WouldBlock));
        }

        if !state.input_credit_available || state.input.is_some() {
            return Err(VideoError::Sink(SinkError::WouldBlock));
        }

        let pkt = packet.take().expect("push_packet called with Some");
        state.input = Some(MailboxInput::Packet {
            producer,
            packet: pkt,
            pts,
            random_access,
        });
        state.input_credit_available = false;
        self.wake_owner.notify_all();
        Ok(())
    }

    pub fn push_frame(
        &self,
        producer: ProducerId,
        frame: &mut Option<VideoFrame>,
        pts: Duration,
    ) -> Result<(), VideoError> {
        let mut state = self.state.lock();

        match &state.current_request {
            Some(req) if req.producer == producer => {
                if req.control.cancelled(producer, req.seek_generation) {
                    return Err(VideoError::Superseded);
                }
            }
            Some(_) => return Err(VideoError::Superseded),
            None => return Err(VideoError::Superseded),
        }

        if let Some((p, err)) = &state.async_error {
            if *p == producer {
                return Err(err.clone());
            }
        }

        if state.eos_requested == Some(producer) {
            return Err(VideoError::Sink(SinkError::WouldBlock));
        }

        if !state.input_credit_available || state.input.is_some() {
            return Err(VideoError::Sink(SinkError::WouldBlock));
        }

        let request = state.current_request.as_ref().unwrap().clone();
        if !matches!(request.target, VideoTarget::Frames { .. }) {
            return Err(VideoError::Unsupported);
        }
        let frame_epoch = state.frame_epoch;
        let f = frame.take().expect("push_frame called with Some");
        state.input = Some(MailboxInput::Frame {
            producer,
            request,
            frame_epoch,
            frame: f,
            pts,
        });
        state.input_credit_available = false;
        self.wake_owner.notify_all();
        Ok(())
    }

    pub fn present_from(&self, producer: ProducerId, start: Duration) -> Result<(), VideoError> {
        let mut state = self.state.lock();
        match &state.current_request {
            Some(req) if req.producer == producer => {
                state.present_from = start;
                self.wake_owner.notify_all();
                Ok(())
            }
            _ => Err(VideoError::Superseded),
        }
    }

    pub fn set_playing(&self, producer: ProducerId, playing: bool) -> Result<(), VideoError> {
        let mut state = self.state.lock();
        match &state.current_request {
            Some(req) if req.producer == producer => {
                state.playing = playing;
                self.wake_owner.notify_all();
                Ok(())
            }
            _ => Err(VideoError::Superseded),
        }
    }

    pub fn poll_finish(&self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        let mut state = self.state.lock();
        if let Some((p, err)) = &state.async_error {
            if *p == producer {
                return Poll::Ready(Err(err.clone()));
            }
        }
        if let Some(p) = state.eos_confirmed {
            if p == producer {
                return Poll::Ready(Ok(()));
            }
        }
        match &state.current_request {
            Some(req) if req.producer == producer => {}
            Some(_) => return Poll::Ready(Err(VideoError::Superseded)),
            None => {}
        }
        // Stale finish must not seal or occupy a different producer's marker (C12).
        match state.eos_requested {
            None => {
                state.eos_requested = Some(producer);
                self.wake_owner.notify_all();
            }
            Some(existing) if existing == producer => {}
            Some(_) => return Poll::Ready(Err(VideoError::Superseded)),
        }
        Poll::Pending
    }

    pub fn eos_requested_producer(&self) -> Option<ProducerId> {
        self.state.lock().eos_requested
    }

    /// Take mailbox input into owner-held credit without restoring the credit (C8).
    pub fn take_input_for_owner(&self) -> Option<OwnerHeldInput> {
        let mut state = self.state.lock();
        state.input.take().map(|input| OwnerHeldInput { input })
    }

    pub fn release_input_credit(&self) {
        let mut state = self.state.lock();
        state.input_credit_available = true;
        drop(state);
        self.wake_frontend_outside();
        self.wake_owner.notify_all();
    }

    pub fn accept_owner_input(&self, held: OwnerHeldInput) {
        drop(held);
        self.release_input_credit();
    }

    pub fn discard_owner_input(&self, held: OwnerHeldInput) {
        drop(held);
        self.release_input_credit();
    }

    pub fn notify_owner(&self) {
        self.wake_owner.notify_all();
    }

    pub fn set_transition_result(
        &self,
        producer: ProducerId,
        res: Result<VideoMode, VideoError>,
    ) {
        {
            let mut state = self.state.lock();
            if matches!(res, Ok(_)) {
                state.configured = true;
            }
            state.transition_result = Some((producer, res));
        }
        self.wake_frontend_outside();
    }

    pub fn set_async_error(&self, producer: ProducerId, err: VideoError) {
        {
            let mut state = self.state.lock();
            state.async_error = Some((producer, err));
        }
        self.wake_frontend_outside();
    }

    pub fn confirm_eos(&self, producer: ProducerId) {
        {
            let mut state = self.state.lock();
            state.eos_confirmed = Some(producer);
        }
        self.wake_frontend_outside();
    }

    pub fn publish_picture_ready(&self, producer: ProducerId, pts: Duration) {
        let ready_cb = {
            let state = self.state.lock();
            match &state.current_request {
                Some(req) if req.producer == producer => match &req.target {
                    VideoTarget::Compressed { ready, .. } | VideoTarget::Frames { ready, .. } => {
                        Some(ready.clone())
                    }
                    _ => None,
                },
                _ => None,
            }
        };
        if let Some(ready) = ready_cb {
            ready.ready(pts);
        }
    }

    /// Only the native owner waits here. Cancellation can precede publication of
    /// a preserving successor; it is not, by itself, permission to lose input.
    fn await_frame_policy(
        &self,
        accepted: &VideoRequest,
        epoch: u64,
        backend: &BackendShared,
    ) -> bool {
        loop {
            let desired = {
                let state = self.state.lock();
                if state.retire_requested || state.frame_epoch != epoch {
                    return false;
                }
                state.current_request.clone()
            };
            let output = backend.video_output();
            if backend.is_suspended() || !output.available
                || output.revision != accepted.output_revision
            {
                return false;
            }
            let Some(desired) = desired else { return false };
            let compatible = desired.producer == accepted.producer
                || (desired.seek_generation == accepted.seek_generation
                    && desired.output_revision == accepted.output_revision
                    && matches!(desired.target, VideoTarget::Frames { reset: false, .. }));
            if !compatible {
                return false;
            }
            // User control code never runs under the mailbox lock.
            let cancelled = desired.control.cancelled(desired.producer, desired.seek_generation);
            let expired_successor = desired.producer != accepted.producer
                && desired.control.active_now() > desired.deadline;
            let mut state = self.state.lock();
            if state.retire_requested || state.frame_epoch != epoch {
                return false;
            }
            if state.current_request.as_ref().map(|r| r.producer) != Some(desired.producer) {
                continue;
            }
            if expired_successor {
                return false;
            }
            if !cancelled {
                return true;
            }
            self.wake_owner.wait_for(&mut state, Duration::from_millis(10));
        }
    }

    pub fn codec_epoch(&self) -> u64 {
        self.state.lock().codec_epoch
    }

    pub fn bump_codec_epoch(&self) -> u64 {
        let mut state = self.state.lock();
        state.codec_epoch = state.codec_epoch.wrapping_add(1);
        state.codec_epoch
    }

    pub fn is_valid_codec_epoch(&self, epoch: u64) -> bool {
        self.state.lock().codec_epoch == epoch
    }

    pub fn has_pending_input(&self) -> bool {
        self.state.lock().input.is_some()
    }

    pub fn is_input_empty(&self) -> bool {
        self.state.lock().input.is_none()
    }

    pub fn is_credit_free_and_empty(&self) -> bool {
        let state = self.state.lock();
        state.input.is_none() && state.input_credit_available
    }

    pub fn wait_owner(&self, timeout: Duration) {
        let mut state = self.state.lock();
        self.wake_owner.wait_for(&mut state, timeout);
    }

    pub fn is_configured(&self, producer: ProducerId) -> bool {
        let state = self.state.lock();
        state.configured
            && matches!(&state.transition_result, Some((p, Ok(_))) if *p == producer)
    }
}

fn request_mode_key(target: &VideoTarget) -> u8 {
    match target {
        VideoTarget::Compressed { .. } => 0,
        VideoTarget::Frames { .. } => 1,
        VideoTarget::Retired => 2,
    }
}

pub struct AndroidVideoSink {
    backend: Arc<BackendShared>,
    clock: Arc<dyn Clock>,
    mailbox: Arc<VideoMailbox>,
    owner_ticket: Option<OwnerTicket>,
    prefer_software: bool,
    terminally_retired: bool,
    terminal_error: Option<VideoError>,
}

impl AndroidVideoSink {
    pub fn new(backend: Arc<BackendShared>, clock: Arc<dyn Clock>) -> Self {
        Self {
            backend,
            clock,
            mailbox: Arc::new(VideoMailbox::new()),
            owner_ticket: None,
            prefer_software: false,
            terminally_retired: false,
            terminal_error: None,
        }
    }

    pub fn prefer_software_decoder(&mut self, prefer: bool) {
        self.prefer_software = prefer;
    }

    pub fn on_output_invalidated(&mut self) {
        // Output revision/suspension already revoke the old producer. Only the
        // native owner changes codec epochs when it flushes or replaces a codec:
        // this notification can arrive after the successor has configured.
        self.mailbox.notify_owner();
        self.mailbox.wake_frontend_outside();
    }
}

impl VideoSink for AndroidVideoSink {
    fn output(&self) -> VideoOutput {
        self.backend.video_output()
    }

    fn poll_transition(
        &mut self,
        request: &VideoRequest,
    ) -> Poll<Result<VideoMode, VideoError>> {
        if matches!(request.target, VideoTarget::Retired) {
            if self.terminally_retired {
                if let Some(err) = &self.terminal_error {
                    return Poll::Ready(Err(err.clone()));
                }
                return Poll::Ready(Ok(VideoMode::Retired));
            }
            self.mailbox.request_retire();
            if let Some(ticket) = &self.owner_ticket {
                match ticket.poll() {
                    Poll::Ready(Ok(())) => {
                        self.terminally_retired = true;
                        self.owner_ticket = None;
                        return Poll::Ready(Ok(VideoMode::Retired));
                    }
                    Poll::Ready(Err(e)) => {
                        // Failed retirement stays failed; never becomes successful Retired (C7).
                        self.terminally_retired = true;
                        self.owner_ticket = None;
                        let err = VideoError::Sink(SinkError::Fatal(e));
                        self.terminal_error = Some(err.clone());
                        return Poll::Ready(Err(err));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            } else {
                self.terminally_retired = true;
                return Poll::Ready(Ok(VideoMode::Retired));
            }
        }

        if self.terminally_retired {
            if let Some(err) = &self.terminal_error {
                return Poll::Ready(Err(err.clone()));
            }
            return Poll::Ready(Ok(VideoMode::Retired));
        }

        if request
            .control
            .cancelled(request.producer, request.seek_generation)
        {
            return Poll::Ready(Err(VideoError::Superseded));
        }

        // Ticket failure before cached success (C7).
        if let Some(ticket) = &self.owner_ticket {
            if let Poll::Ready(Err(e)) = ticket.poll() {
                self.owner_ticket = None;
                return Poll::Ready(Err(VideoError::Sink(SinkError::Fatal(e))));
            }
        }

        let cached = self.mailbox.poll_transition_result(request.producer);
        if let Poll::Ready(res) = cached {
            return Poll::Ready(res);
        }

        // Deadline applies only while transition is still pending (not steady configured).
        if !self.mailbox.is_configured(request.producer) {
            if let Some(error) = pending_transition_error(request) {
                return Poll::Ready(Err(error));
            }
        }

        if self.owner_ticket.is_none() {
            let mailbox = self.mailbox.clone();
            let backend = self.backend.clone();
            let clock = self.clock.clone();
            let prefer_sw = self.prefer_software;
            let wake = {
                let ctrl = request.control.clone();
                Arc::new(move || ctrl.wake())
            };

            match try_spawn(
                move || run_video_owner(mailbox, backend, clock, prefer_sw),
                wake,
            ) {
                Ok(ticket) => self.owner_ticket = Some(ticket),
                Err(OwnerError::Capacity) => {
                    return Poll::Pending;
                }
                Err(OwnerError::Spawn(e)) => {
                    return Poll::Ready(Err(VideoError::Sink(SinkError::Fatal(e))));
                }
            }
        }

        self.mailbox.set_request(request.clone());

        if let Some(ticket) = &self.owner_ticket {
            if let Poll::Ready(Err(e)) = ticket.poll() {
                self.owner_ticket = None;
                return Poll::Ready(Err(VideoError::Sink(SinkError::Fatal(e))));
            }
        }

        self.mailbox.poll_transition_result(request.producer)
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
        Duration::ZERO
    }

    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        if let Some(err) = &self.terminal_error {
            return Poll::Ready(Err(err.clone()));
        }
        if let Some(ticket) = &self.owner_ticket {
            if let Poll::Ready(Err(e)) = ticket.poll() {
                self.owner_ticket = None;
                return Poll::Ready(Err(VideoError::Sink(SinkError::Fatal(e))));
            }
        }
        self.mailbox.poll_finish(producer)
    }
}

impl Drop for AndroidVideoSink {
    fn drop(&mut self) {
        self.mailbox.request_retire();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CsdProgress {
    Need0,
    Need1,
    Done,
}

struct CodecMeta {
    mime: String,
    width: u32,
    height: u32,
    csd0: Vec<u8>,
    csd1: Vec<u8>,
    nal_length_size: usize,
    packets_are_annex_b: bool,
    bound_surface_id: SurfaceId,
    output_revision: u64,
    prefer_software: bool,
    has_seen_output: bool,
    csd_progress: CsdProgress,
    current_producer: ProducerId,
    codec_epoch: u64,
}

struct CodecSession {
    codec: MediaCodec,
    meta: CodecMeta,
}

impl CodecSession {
    fn can_reuse(
        &self,
        params: &CodecParameters,
        current_revision: u64,
        current_surface_id: Option<SurfaceId>,
        prefer_software: bool,
    ) -> bool {
        let mime = match codec_id_to_mime(&params.codec_id.0) {
            Some(m) => m,
            None => return false,
        };
        if self.meta.mime != mime {
            return false;
        }
        if self.meta.output_revision != current_revision {
            return false;
        }
        if Some(self.meta.bound_surface_id) != current_surface_id {
            return false;
        }
        if params.width.unwrap_or(0) != self.meta.width
            || params.height.unwrap_or(0) != self.meta.height
        {
            return false;
        }
        if self.meta.prefer_software != prefer_software {
            return false;
        }
        let (new_csd0, new_csd1, new_nls, new_annex_b) = extract_csd(mime, &params.extradata);
        self.meta.csd0 == new_csd0
            && self.meta.csd1 == new_csd1
            && self.meta.nal_length_size == new_nls
            && self.meta.packets_are_annex_b == new_annex_b
    }
}

/// Held output uses the real NDK borrowed OutputBuffer; codec and meta are split
/// so CSD/input can mutate meta without extending lifetimes (C1).
struct HeldOutput<'a> {
    buffer: OutputBuffer<'a>,
    pts: Duration,
    producer: ProducerId,
    codec_epoch: u64,
    eos: bool,
}

enum SessionOutcome {
    Retire,
    Reconfigure,
    SurfaceLost,
}

#[derive(Clone, PartialEq, Eq)]
struct RequestKey {
    producer: ProducerId,
    seek_generation: u64,
    output_revision: u64,
    mode: u8,
}

fn try_drain_csd(codec: &MediaCodec, meta: &mut CodecMeta) -> Result<bool, VideoError> {
    loop {
        match meta.csd_progress {
            CsdProgress::Done => return Ok(true),
            CsdProgress::Need0 => {
                if meta.csd0.is_empty() {
                    meta.csd_progress = if meta.csd1.is_empty() {
                        CsdProgress::Done
                    } else {
                        CsdProgress::Need1
                    };
                    continue;
                }
                match codec.dequeue_input_buffer(Duration::ZERO) {
                    Ok(DequeuedInputBufferResult::Buffer(mut buf)) => {
                        let raw = buf.buffer_mut();
                        if raw.len() < meta.csd0.len() {
                            return Err(VideoError::Sink(SinkError::Fallback(
                                "CSD0 buffer capacity too small".into(),
                            )));
                        }
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                meta.csd0.as_ptr(),
                                raw.as_mut_ptr().cast(),
                                meta.csd0.len(),
                            );
                        }
                        codec
                            .queue_input_buffer(
                                buf,
                                0,
                                meta.csd0.len(),
                                0,
                                BUFFER_FLAG_CODEC_CONFIG,
                            )
                            .map_err(|e| {
                                VideoError::Sink(SinkError::Fallback(format!(
                                    "queue CSD0 error: {e:?}"
                                )))
                            })?;
                        meta.csd_progress = if meta.csd1.is_empty() {
                            CsdProgress::Done
                        } else {
                            CsdProgress::Need1
                        };
                    }
                    Ok(DequeuedInputBufferResult::TryAgainLater) => return Ok(false),
                    Err(e) => {
                        return Err(VideoError::Sink(SinkError::Fallback(format!(
                            "dequeue for CSD0 error: {e:?}"
                        ))));
                    }
                }
            }
            CsdProgress::Need1 => {
                if meta.csd1.is_empty() {
                    meta.csd_progress = CsdProgress::Done;
                    continue;
                }
                match codec.dequeue_input_buffer(Duration::ZERO) {
                    Ok(DequeuedInputBufferResult::Buffer(mut buf)) => {
                        let raw = buf.buffer_mut();
                        if raw.len() < meta.csd1.len() {
                            return Err(VideoError::Sink(SinkError::Fallback(
                                "CSD1 buffer capacity too small".into(),
                            )));
                        }
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                meta.csd1.as_ptr(),
                                raw.as_mut_ptr().cast(),
                                meta.csd1.len(),
                            );
                        }
                        codec
                            .queue_input_buffer(
                                buf,
                                0,
                                meta.csd1.len(),
                                0,
                                BUFFER_FLAG_CODEC_CONFIG,
                            )
                            .map_err(|e| {
                                VideoError::Sink(SinkError::Fallback(format!(
                                    "queue CSD1 error: {e:?}"
                                )))
                            })?;
                        meta.csd_progress = CsdProgress::Done;
                    }
                    Ok(DequeuedInputBufferResult::TryAgainLater) => return Ok(false),
                    Err(e) => {
                        return Err(VideoError::Sink(SinkError::Fallback(format!(
                            "dequeue for CSD1 error: {e:?}"
                        ))));
                    }
                }
            }
        }
    }
}

fn request_still_valid(
    backend: &BackendShared,
    mailbox: &VideoMailbox,
    req: &VideoRequest,
    bound_surface: SurfaceId,
    codec_epoch: u64,
) -> bool {
    if req
        .control
        .cancelled(req.producer, req.seek_generation)
    {
        return false;
    }
    if backend.is_suspended() {
        return false;
    }
    if backend.current_surface_id() != Some(bound_surface) {
        return false;
    }
    if req.output_revision != backend.video_output().revision {
        return false;
    }
    let state = mailbox.state.lock();
    !state.retire_requested
        && state.codec_epoch == codec_epoch
        && state.current_request.as_ref().is_some_and(|current| current.producer == req.producer)
}

fn run_codec_session(
    session: &mut CodecSession,
    backend: &Arc<BackendShared>,
    mailbox: &Arc<VideoMailbox>,
    clock: &Arc<dyn Clock>,
    prefer_software: bool,
    eos_input_queued: &mut Option<(ProducerId, u64)>,
    accepted_in_flight: &mut bool,
) -> SessionOutcome {
    // Split codec/meta so OutputBuffer can borrow codec while meta mutates (C1).
    let CodecSession { codec, meta } = session;
    let mut held_output: Option<HeldOutput<'_>> = None;

    loop {
        let (req_opt, retire_req, playing, present_from, eos_requested) = {
            let state = mailbox.state.lock();
            (
                state.current_request.clone(),
                state.retire_requested,
                state.playing,
                state.present_from,
                state.eos_requested,
            )
        };

        if retire_req {
            if let Some(held) = held_output.take() {
                let _ = codec.release_output_buffer(held.buffer, false);
            }
            return SessionOutcome::Retire;
        }

        if backend.current_surface_id() != Some(meta.bound_surface_id) {
            if let Some(held) = held_output.take() {
                let _ = codec.release_output_buffer(held.buffer, false);
            }
            return SessionOutcome::SurfaceLost;
        }

        if let Some(req) = &req_opt {
            let is_new_request = req.producer != meta.current_producer
                || req.output_revision != meta.output_revision;

            if is_new_request {
                match &req.target {
                    VideoTarget::Compressed { params, .. } => {
                        let current_surf_id = backend.current_surface_id();
                        if session_can_reuse(
                            meta,
                            params,
                            req.output_revision,
                            current_surf_id,
                            prefer_software,
                        ) {
                            // Quiesce held index BEFORE flush (C6).
                            if let Some(held) = held_output.take() {
                                if codec.release_output_buffer(held.buffer, false).is_err() {
                                    return SessionOutcome::Reconfigure;
                                }
                            }
                            let new_epoch = mailbox.bump_codec_epoch();
                            meta.codec_epoch = new_epoch;
                            meta.current_producer = req.producer;
                            meta.output_revision = req.output_revision;
                            *eos_input_queued = None;
                            *accepted_in_flight = false;
                            match codec.flush() {
                                Ok(()) => {
                                    if !meta.has_seen_output {
                                        // Early CSD only before first output (C11).
                                        meta.csd_progress = if !meta.csd0.is_empty() {
                                            CsdProgress::Need0
                                        } else if !meta.csd1.is_empty() {
                                            CsdProgress::Need1
                                        } else {
                                            CsdProgress::Done
                                        };
                                    }
                                    // Do not blindly duplicate CSD after healthy flush with output.
                                    if req.control.cancelled(req.producer, req.seek_generation) {
                                        mailbox.set_transition_result(
                                            req.producer,
                                            Err(VideoError::Superseded),
                                        );
                                    } else {
                                        mailbox.set_transition_result(
                                            req.producer,
                                            Ok(VideoMode::Compressed),
                                        );
                                    }
                                }
                                Err(e) => {
                                    mailbox.set_async_error(
                                        req.producer,
                                        VideoError::Sink(SinkError::Fallback(format!(
                                            "flush: {e:?}"
                                        ))),
                                    );
                                    return SessionOutcome::Reconfigure;
                                }
                            }
                        } else {
                            if let Some(held) = held_output.take() {
                                let _ = codec.release_output_buffer(held.buffer, false);
                            }
                            return SessionOutcome::Reconfigure;
                        }
                    }
                    _ => {
                        if let Some(held) = held_output.take() {
                            let _ = codec.release_output_buffer(held.buffer, false);
                        }
                        return SessionOutcome::Reconfigure;
                    }
                }
            }
        }

        // Present or discard held output with validity checks (C6).
        if let Some(held) = &held_output {
            if !req_opt.as_ref().is_some_and(|req| {
                req.producer == held.producer
                    && request_still_valid(backend, mailbox, req, meta.bound_surface_id, held.codec_epoch)
            }) {
                let held_buf = held_output.take().unwrap();
                if let Err(error) = codec.release_output_buffer(held_buf.buffer, false) {
                    mailbox.set_async_error(meta.current_producer, VideoError::Sink(
                        SinkError::Fallback(format!("discard cancelled output: {error:?}")),
                    ));
                    return SessionOutcome::Reconfigure;
                }
            } else {
                let pts = held.pts;
                let producer = held.producer;
                let is_eos = held.eos;
                let now = current_monotonic_ns();
                if playing {
                    if let Some(target) = clock.monotonic_ns_at(pts) {
                        // Clock mapping is an external call. Cancellation/retirement
                        // can become visible while it is running.
                        if !req_opt.as_ref().is_some_and(|req| {
                            request_still_valid(backend, mailbox, req, meta.bound_surface_id, meta.codec_epoch)
                        }) {
                            let held_buf = held_output.take().unwrap();
                            if let Err(error) = codec.release_output_buffer(held_buf.buffer, false) {
                                mailbox.set_async_error(producer, VideoError::Sink(
                                    SinkError::Fallback(format!("discard cancelled output: {error:?}")),
                                ));
                                return SessionOutcome::Reconfigure;
                            }
                            continue;
                        }
                        if target < now - 30_000_000 {
                            let held_buf = held_output.take().unwrap();
                            if let Err(e) = codec.release_output_buffer(held_buf.buffer, false) {
                                mailbox.set_async_error(
                                    producer,
                                    VideoError::Sink(SinkError::Fallback(format!(
                                        "release buffer: {e:?}"
                                    ))),
                                );
                            }
                            if is_eos {
                                *accepted_in_flight = false;
                                if mailbox.is_credit_free_and_empty() {
                                    mailbox.confirm_eos(producer);
                                }
                            }
                        } else if target <= now + 5_000_000 {
                            let held_buf = held_output.take().unwrap();
                            if let Err(e) =
                                codec.release_output_buffer_at_time(held_buf.buffer, target)
                            {
                                mailbox.set_async_error(
                                    producer,
                                    VideoError::Sink(SinkError::Fallback(format!(
                                        "release buffer at time: {e:?}"
                                    ))),
                                );
                            }
                            if is_eos {
                                *accepted_in_flight = false;
                                if mailbox.is_credit_free_and_empty() {
                                    mailbox.confirm_eos(producer);
                                }
                            }
                        }
                    }
                }
            }
        }

        if held_output.is_none() {
            if let Some(req) = &req_opt {
                if !request_still_valid(
                    backend,
                    mailbox,
                    req,
                    meta.bound_surface_id,
                    meta.codec_epoch,
                ) {
                    mailbox.wait_owner(Duration::from_millis(10));
                    continue;
                }
            }
            match codec.dequeue_output_buffer(Duration::ZERO) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(out_buf)) => {
                    // Validate after native dequeue (C6).
                    if let Some(req) = &req_opt {
                        if req.producer != meta.current_producer
                            || !request_still_valid(backend, mailbox, req, meta.bound_surface_id, meta.codec_epoch)
                        {
                            let _ = codec.release_output_buffer(out_buf, false);
                        } else {
                            meta.has_seen_output = true;
                            let info = *out_buf.info();
                            let flags = info.flags();
                            let size = info.size();
                            let pts_us = info.presentation_time_us().max(0) as u64;
                            let pts = Duration::from_micros(pts_us);
                            let eos = flags & BUFFER_FLAG_END_OF_STREAM != 0;

                            if eos {
                                // Payload-bearing EOS: preserve final picture (C5).
                                if size > 0 && pts >= present_from {
                                    mailbox.publish_picture_ready(meta.current_producer, pts);
                                    held_output = Some(HeldOutput {
                                        buffer: out_buf,
                                        pts,
                                        producer: meta.current_producer,
                                        codec_epoch: meta.codec_epoch,
                                        eos: true,
                                    });
                                } else {
                                    if let Err(e) = codec.release_output_buffer(out_buf, false) {
                                        mailbox.set_async_error(
                                            meta.current_producer,
                                            VideoError::Sink(SinkError::Fallback(format!(
                                                "release eos: {e:?}"
                                            ))),
                                        );
                                    }
                                    *accepted_in_flight = false;
                                    if mailbox.is_credit_free_and_empty()
                                        && held_output.is_none()
                                    {
                                        mailbox.confirm_eos(meta.current_producer);
                                    }
                                }
                            } else if pts < present_from {
                                if let Err(e) = codec.release_output_buffer(out_buf, false) {
                                    mailbox.set_async_error(
                                        meta.current_producer,
                                        VideoError::Sink(SinkError::Fallback(format!(
                                            "release early: {e:?}"
                                        ))),
                                    );
                                }
                            } else {
                                mailbox.publish_picture_ready(meta.current_producer, pts);
                                held_output = Some(HeldOutput {
                                    buffer: out_buf,
                                    pts,
                                    producer: meta.current_producer,
                                    codec_epoch: meta.codec_epoch,
                                    eos: false,
                                });
                            }
                        }
                    } else {
                        let _ = codec.release_output_buffer(out_buf, false);
                    }
                }
                Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => {}
                Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                    meta.has_seen_output = true;
                }
                Ok(DequeuedOutputBufferInfoResult::OutputBuffersChanged) => {}
                Err(e) => {
                    mailbox.set_async_error(
                        meta.current_producer,
                        VideoError::Sink(SinkError::Fallback(format!(
                            "dequeue_output_buffer: {e:?}"
                        ))),
                    );
                }
            }
        }

        let mut csd_ready = true;
        match try_drain_csd(codec, meta) {
            Ok(ready) => csd_ready = ready,
            Err(err) => {
                mailbox.set_async_error(meta.current_producer, err);
                csd_ready = false;
            }
        }

        if csd_ready {
            // Snapshot under a short lock; never hold mailbox.state across take/native (C6/C8).
            let pending_producer = {
                let state = mailbox.state.lock();
                state.input.as_ref().map(|i| i.producer())
            };
            if let Some(held_peek) = pending_producer {
                if held_peek != meta.current_producer {
                    if let Some(stale) = mailbox.take_input_for_owner() {
                        mailbox.discard_owner_input(stale);
                    }
                } else if let Some(req) = &req_opt {
                    if req.control.cancelled(held_peek, req.seek_generation) {
                        if let Some(stale) = mailbox.take_input_for_owner() {
                            mailbox.discard_owner_input(stale);
                        }
                    } else if eos_requested == Some(held_peek) {
                        // Drain admitted input before empty EOS (C5) — fall through to queue with possible EOS flag.
                        match codec.dequeue_input_buffer(Duration::ZERO) {
                            Ok(DequeuedInputBufferResult::Buffer(mut in_buf)) => {
                                if let Some(OwnerHeldInput {
                                    input:
                                        MailboxInput::Packet {
                                            packet,
                                            pts,
                                            random_access,
                                            producer,
                                        },
                                }) = mailbox.take_input_for_owner()
                                {
                                    if !request_still_valid(
                                        backend,
                                        mailbox,
                                        req,
                                        meta.bound_surface_id,
                                        meta.codec_epoch,
                                    ) {
                                        mailbox.discard_owner_input(OwnerHeldInput {
                                            input: MailboxInput::Packet {
                                                producer,
                                                packet,
                                                pts,
                                                random_access,
                                            },
                                        });
                                    } else {
                                        let annex_b_data = if !meta.packets_are_annex_b {
                                            convert_packet_to_annex_b(
                                                &packet.data,
                                                meta.nal_length_size,
                                            )
                                        } else {
                                            packet.data
                                        };
                                        let raw_dest = in_buf.buffer_mut();
                                        if raw_dest.len() < annex_b_data.len() {
                                            mailbox.set_async_error(
                                                producer,
                                                VideoError::Sink(SinkError::Fallback(
                                                    "input buffer capacity too small".into(),
                                                )),
                                            );
                                            drop(annex_b_data);
                                            mailbox.release_input_credit();
                                        } else {
                                            unsafe {
                                                std::ptr::copy_nonoverlapping(
                                                    annex_b_data.as_ptr(),
                                                    raw_dest.as_mut_ptr().cast(),
                                                    annex_b_data.len(),
                                                );
                                            }
                                            let pts_us = pts.as_micros() as u64;
                                            // Preserve payload through final input; EOS flag on last admitted (C5).
                                            let mut flags = if random_access {
                                                BUFFER_FLAG_KEY_FRAME
                                            } else {
                                                0
                                            };
                                            flags |= BUFFER_FLAG_END_OF_STREAM;
                                            match codec.queue_input_buffer(
                                                in_buf,
                                                0,
                                                annex_b_data.len(),
                                                pts_us,
                                                flags,
                                            ) {
                                                Ok(()) => {
                                                    drop(annex_b_data);
                                                    mailbox.release_input_credit();
                                                    *eos_input_queued =
                                                        Some((producer, meta.codec_epoch));
                                                    *accepted_in_flight = true;
                                                }
                                                Err(e) => {
                                                    mailbox.set_async_error(
                                                        producer,
                                                        VideoError::Sink(SinkError::Fallback(
                                                            format!("queue_input_buffer: {e:?}"),
                                                        )),
                                                    );
                                                    drop(annex_b_data);
                                                    mailbox.release_input_credit();
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(DequeuedInputBufferResult::TryAgainLater) => {}
                            Err(e) => {
                                mailbox.set_async_error(
                                    held_peek,
                                    VideoError::Sink(SinkError::Fallback(format!(
                                        "dequeue_input_buffer: {e:?}"
                                    ))),
                                );
                            }
                        }
                    } else {
                        match codec.dequeue_input_buffer(Duration::ZERO) {
                            Ok(DequeuedInputBufferResult::Buffer(mut in_buf)) => {
                                if let Some(OwnerHeldInput {
                                    input:
                                        MailboxInput::Packet {
                                            packet,
                                            pts,
                                            random_access,
                                            producer,
                                        },
                                }) = mailbox.take_input_for_owner()
                                {
                                    if !request_still_valid(
                                        backend,
                                        mailbox,
                                        req,
                                        meta.bound_surface_id,
                                        meta.codec_epoch,
                                    ) {
                                        mailbox.discard_owner_input(OwnerHeldInput {
                                            input: MailboxInput::Packet {
                                                producer,
                                                packet,
                                                pts,
                                                random_access,
                                            },
                                        });
                                    } else {
                                        let annex_b_data = if !meta.packets_are_annex_b {
                                            convert_packet_to_annex_b(
                                                &packet.data,
                                                meta.nal_length_size,
                                            )
                                        } else {
                                            packet.data
                                        };
                                        let raw_dest = in_buf.buffer_mut();
                                        if raw_dest.len() < annex_b_data.len() {
                                            mailbox.set_async_error(
                                                producer,
                                                VideoError::Sink(SinkError::Fallback(
                                                    "input buffer capacity too small".into(),
                                                )),
                                            );
                                            drop(annex_b_data);
                                            mailbox.release_input_credit();
                                        } else {
                                            unsafe {
                                                std::ptr::copy_nonoverlapping(
                                                    annex_b_data.as_ptr(),
                                                    raw_dest.as_mut_ptr().cast(),
                                                    annex_b_data.len(),
                                                );
                                            }
                                            let pts_us = pts.as_micros() as u64;
                                            let flags = if random_access {
                                                BUFFER_FLAG_KEY_FRAME
                                            } else {
                                                0
                                            };
                                            match codec.queue_input_buffer(
                                                in_buf,
                                                0,
                                                annex_b_data.len(),
                                                pts_us,
                                                flags,
                                            ) {
                                                Ok(()) => {
                                                    drop(annex_b_data);
                                                    mailbox.release_input_credit();
                                                    *accepted_in_flight = true;
                                                }
                                                Err(e) => {
                                                    mailbox.set_async_error(
                                                        producer,
                                                        VideoError::Sink(SinkError::Fallback(
                                                            format!("queue_input_buffer: {e:?}"),
                                                        )),
                                                    );
                                                    // Credit released after failed queue; error stays tagged.
                                                    drop(annex_b_data);
                                                    mailbox.release_input_credit();
                                                }
                                            }
                                        }
                                    }
                                } else if let Some(held) = mailbox.take_input_for_owner() {
                                    // Non-packet while compressed: discard.
                                    mailbox.discard_owner_input(held);
                                    let _ = codec.queue_input_buffer(in_buf, 0, 0, 0, 0);
                                }
                            }
                            Ok(DequeuedInputBufferResult::TryAgainLater) => {}
                            Err(e) => {
                                mailbox.set_async_error(
                                    held_peek,
                                    VideoError::Sink(SinkError::Fallback(format!(
                                        "dequeue_input_buffer: {e:?}"
                                    ))),
                                );
                            }
                        }
                    }
                }
            }
        }

        // Exactly one empty EOS after all admitted input drained (C5).
        if let Some(eos_prod) = eos_requested {
            if meta.current_producer == eos_prod
                && mailbox.is_credit_free_and_empty()
                && *eos_input_queued != Some((eos_prod, meta.codec_epoch))
            {
                match codec.dequeue_input_buffer(Duration::ZERO) {
                    Ok(DequeuedInputBufferResult::Buffer(in_buf)) => {
                        match codec.queue_input_buffer(
                            in_buf,
                            0,
                            0,
                            0,
                            BUFFER_FLAG_END_OF_STREAM,
                        ) {
                            Ok(()) => {
                                *eos_input_queued = Some((eos_prod, meta.codec_epoch));
                                *accepted_in_flight = true;
                            }
                            Err(e) => {
                                mailbox.set_async_error(
                                    eos_prod,
                                    VideoError::Sink(SinkError::Fallback(format!(
                                        "queue EOS: {e:?}"
                                    ))),
                                );
                            }
                        }
                    }
                    Ok(DequeuedInputBufferResult::TryAgainLater) => {}
                    Err(e) => {
                        mailbox.set_async_error(
                            eos_prod,
                            VideoError::Sink(SinkError::Fallback(format!(
                                "dequeue for EOS: {e:?}"
                            ))),
                        );
                    }
                }
            }
        }

        // Finish only after output-side EOS handled and accepted input credit free (C5).
        // `accepted_in_flight` is cleared when an EOS output buffer is fully released.
        if let Some(eos_prod) = eos_requested {
            if meta.current_producer == eos_prod
                && *eos_input_queued == Some((eos_prod, meta.codec_epoch))
                && !*accepted_in_flight
                && held_output.is_none()
                && mailbox.is_credit_free_and_empty()
            {
                mailbox.confirm_eos(eos_prod);
            }
        }

        let wait_timeout = if held_output.is_some() {
            Duration::from_millis(2)
        } else if mailbox.has_pending_input() {
            Duration::from_millis(1)
        } else {
            Duration::from_millis(10)
        };
        mailbox.wait_owner(wait_timeout);
    }
}

fn session_can_reuse(
    meta: &CodecMeta,
    params: &CodecParameters,
    current_revision: u64,
    current_surface_id: Option<SurfaceId>,
    prefer_software: bool,
) -> bool {
    let mime = match codec_id_to_mime(&params.codec_id.0) {
        Some(m) => m,
        None => return false,
    };
    if meta.mime != mime {
        return false;
    }
    if meta.output_revision != current_revision {
        return false;
    }
    if Some(meta.bound_surface_id) != current_surface_id {
        return false;
    }
    if params.width.unwrap_or(0) != meta.width || params.height.unwrap_or(0) != meta.height {
        return false;
    }
    if meta.prefer_software != prefer_software {
        return false;
    }
    let (new_csd0, new_csd1, new_nls, new_annex_b) = extract_csd(mime, &params.extradata);
    meta.csd0 == new_csd0
        && meta.csd1 == new_csd1
        && meta.nal_length_size == new_nls
        && meta.packets_are_annex_b == new_annex_b
}

/// Stop/delete codec and drop window BEFORE healthy lease release (C2).
/// Failures quarantine the lease (drop without release_healthy).
fn teardown_codec_resources(
    session: Option<CodecSession>,
    window: &mut Option<NativeWindow>,
    lease: &mut Option<SurfaceBindingLease>,
    quarantine_on_error: bool,
) -> Result<(), String> {
    if let Some(session) = session {
        if let Err(e) = session.codec.stop() {
            let _ = window.take();
            // Drop lease without healthy release → quarantine (C2).
            let _ = lease.take();
            return Err(format!("codec stop failed: {e:?}"));
        }
        // MediaCodec Drop may panic; catch via owner catch_unwind at pool level.
        drop(session.codec);
    }
    drop(window.take());
    if let Some(l) = lease.take() {
        if quarantine_on_error {
            drop(l);
        } else {
            l.release_healthy();
        }
    }
    Ok(())
}

fn run_video_owner(
    mailbox: Arc<VideoMailbox>,
    backend: Arc<BackendShared>,
    clock: Arc<dyn Clock>,
    prefer_software: bool,
) -> Result<(), String> {
    let mut active_codec: Option<CodecSession> = None;
    let mut active_lease: Option<SurfaceBindingLease> = None;
    let mut current_window: Option<NativeWindow> = None;
    let mut current_binding: Option<Arc<SurfaceBinding>> = None;
    let mut sw_frame_info: Option<(PixelFormat, u32, u32)> = None;
    let mut applied_request_key: Option<RequestKey> = None;
    let mut eos_input_queued: Option<(ProducerId, u64)> = None;
    let mut accepted_in_flight = false;

    loop {
        if mailbox.state.lock().retire_requested {
            teardown_codec_resources(
                active_codec.take(),
                &mut current_window,
                &mut active_lease,
                false,
            )?;
            drop(current_binding.take());
            return Ok(());
        }

        // Software owners must release a revoked window even with no more input.
        if active_codec.is_none()
            && current_binding.as_ref().is_some_and(|binding| {
                binding.is_retiring()
                    || backend.current_surface_id() != Some(binding.id())
                    || backend.is_suspended()
                    || applied_request_key.as_ref().is_some_and(|key| {
                        key.output_revision != backend.video_output().revision
                    })
            })
        {
            teardown_codec_resources(None, &mut current_window, &mut active_lease, false)?;
            drop(current_binding.take());
            sw_frame_info = None;
            if let Some(key) = applied_request_key.take() {
                mailbox.set_transition_result(
                    key.producer,
                    Err(VideoError::Sink(SinkError::Unavailable)),
                );
            }
        }

        if let Some(session) = &mut active_codec {
            match run_codec_session(
                session,
                &backend,
                &mailbox,
                &clock,
                prefer_software,
                &mut eos_input_queued,
                &mut accepted_in_flight,
            ) {
                SessionOutcome::Retire => {
                    teardown_codec_resources(
                        active_codec.take(),
                        &mut current_window,
                        &mut active_lease,
                        false,
                    )?;
                    drop(current_binding.take());
                    return Ok(());
                }
                SessionOutcome::Reconfigure | SessionOutcome::SurfaceLost => {
                    if let Err(e) = teardown_codec_resources(
                        active_codec.take(),
                        &mut current_window,
                        &mut active_lease,
                        false,
                    ) {
                        // Quarantine already applied inside on stop failure.
                        return Err(e);
                    }
                    drop(current_binding.take());
                    sw_frame_info = None;
                    applied_request_key = None;
                    eos_input_queued = None;
                    accepted_in_flight = false;
                    continue;
                }
            }
        }

        let (req_opt, retire_req, eos_requested) = {
            let state = mailbox.state.lock();
            (
                state.current_request.clone(),
                state.retire_requested,
                state.eos_requested,
            )
        };

        if retire_req {
            teardown_codec_resources(
                None,
                &mut current_window,
                &mut active_lease,
                false,
            )?;
            drop(current_binding.take());
            return Ok(());
        }

        if let Some(req) = &req_opt {
            let mode = request_mode_key(&req.target);
            let key = RequestKey {
                producer: req.producer,
                seek_generation: req.seek_generation,
                output_revision: req.output_revision,
                mode,
            };

            let needs_transition = match &applied_request_key {
                Some(applied) => applied != &key,
                None => true,
            };

            let finishing_predecessor = sw_frame_info.is_some()
                && matches!(req.target, VideoTarget::Frames { reset: false, .. })
                && applied_request_key.as_ref().is_some_and(|old| {
                    old.seek_generation == req.seek_generation
                        && old.output_revision == req.output_revision
                })
                && !mailbox.is_credit_free_and_empty();
            if needs_transition && !finishing_predecessor {
                if let Some(error) = pending_transition_error(req) {
                    mailbox.set_transition_result(req.producer, Err(error));
                    applied_request_key = Some(key);
                } else {
                    match &req.target {
                        VideoTarget::Compressed { params, .. } => {
                            // Retire software resources before replacement acquisition (C10).
                            if sw_frame_info.is_some() || current_window.is_some() {
                                if let Err(e) = teardown_codec_resources(
                                    None,
                                    &mut current_window,
                                    &mut active_lease,
                                    false,
                                ) {
                                    mailbox.set_transition_result(
                                        req.producer,
                                        Err(VideoError::Sink(SinkError::Fatal(e))),
                                    );
                                    applied_request_key = Some(key);
                                    continue;
                                }
                                drop(current_binding.take());
                                sw_frame_info = None;
                            }

                            let surface_binding = backend.video_surface_binding();
                            if let Some(binding) = surface_binding {
                                match binding.try_acquire_lease() {
                                    Ok(lease) => {
                                        match backend.import_window_for_lease(&lease) {
                                            Ok(window) => {
                                                if req.control.cancelled(
                                                    req.producer,
                                                    req.seek_generation,
                                                ) {
                                                    drop(window);
                                                    lease.release_healthy();
                                                    mailbox.set_transition_result(
                                                        req.producer,
                                                        Err(VideoError::Superseded),
                                                    );
                                                    applied_request_key = Some(key);
                                                    continue;
                                                }
                                                let epoch = mailbox.bump_codec_epoch();
                                                match configure_and_start_codec(
                                                    params,
                                                    &backend,
                                                    &window,
                                                    req,
                                                    binding.id(),
                                                    req.output_revision,
                                                    prefer_software,
                                                    epoch,
                                                ) {
                                                    Ok(session) => {
                                                        // Recheck after start before publication (C6).
                                                        let admission_error = pending_transition_error(req).or_else(|| {
                                                            (backend.current_surface_id() != Some(binding.id())
                                                                || backend.video_output().revision != req.output_revision
                                                                || backend.is_suspended())
                                                                .then_some(VideoError::Superseded)
                                                        });
                                                        if let Some(error) = admission_error {
                                                            let mut w = Some(window);
                                                            let mut l = Some(lease);
                                                            teardown_codec_resources(
                                                                Some(session),
                                                                &mut w,
                                                                &mut l,
                                                                false,
                                                            )?;
                                                            mailbox.set_transition_result(
                                                                req.producer,
                                                                Err(error),
                                                            );
                                                        } else {
                                                            active_lease = Some(lease);
                                                            current_binding = Some(binding.clone());
                                                            current_window = Some(window);
                                                            active_codec = Some(session);
                                                            mailbox.set_transition_result(
                                                                req.producer,
                                                                Ok(VideoMode::Compressed),
                                                            );
                                                            applied_request_key = Some(key);
                                                            continue;
                                                        }
                                                    }
                                                    Err(err) => {
                                                        drop(window);
                                                        lease.release_healthy();
                                                        mailbox.set_transition_result(
                                                            req.producer,
                                                            Err(err),
                                                        );
                                                    }
                                                }
                                                applied_request_key = Some(key);
                                            }
                                            Err(_) => {
                                                lease.release_healthy();
                                                mailbox.set_transition_result(
                                                    req.producer,
                                                    Err(VideoError::Sink(SinkError::Unavailable)),
                                                );
                                                applied_request_key = Some(key);
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        mailbox.set_transition_result(
                                            req.producer,
                                            Err(VideoError::Sink(SinkError::Unavailable)),
                                        );
                                        applied_request_key = Some(key);
                                    }
                                }
                            } else {
                                mailbox.set_transition_result(
                                    req.producer,
                                    Err(VideoError::Sink(SinkError::Unavailable)),
                                );
                                applied_request_key = Some(key);
                            }
                        }
                        VideoTarget::Frames {
                            params, reset, ..
                        } => {
                            let new_info = (
                                params.pixel_format.unwrap_or(PixelFormat::Yuv420P),
                                params.width.unwrap_or(0),
                                params.height.unwrap_or(0),
                            );

                            if *reset {
                                // reset=true: full reopen after quiescing prior software work.
                                if active_lease.is_some() || current_window.is_some() {
                                    if let Err(e) = teardown_codec_resources(
                                        None,
                                        &mut current_window,
                                        &mut active_lease,
                                        false,
                                    ) {
                                        mailbox.set_transition_result(
                                            req.producer,
                                            Err(VideoError::Sink(SinkError::Fatal(e))),
                                        );
                                        applied_request_key = Some(key);
                                        continue;
                                    }
                                    drop(current_binding.take());
                                }
                            }
                            sw_frame_info = Some(new_info);

                            if current_window.is_none() {
                                let surface_binding = backend.video_surface_binding();
                                if let Some(binding) = surface_binding {
                                    match binding.try_acquire_lease() {
                                        Ok(lease) => match backend.import_window_for_lease(&lease)
                                        {
                                            Ok(window) => {
                                                let admission_error = pending_transition_error(req).or_else(|| {
                                                    (backend.current_surface_id() != Some(binding.id())
                                                        || backend.video_output().revision != req.output_revision
                                                        || backend.is_suspended())
                                                        .then_some(VideoError::Superseded)
                                                });
                                                if let Some(error) = admission_error {
                                                    drop(window);
                                                    lease.release_healthy();
                                                    mailbox.set_transition_result(
                                                        req.producer,
                                                        Err(error),
                                                    );
                                                } else {
                                                    active_lease = Some(lease);
                                                    current_binding = Some(binding.clone());
                                                    current_window = Some(window);
                                                    mailbox.bump_codec_epoch();
                                                    // Frames must NOT report Configured without window/lease.
                                                    mailbox.set_transition_result(
                                                        req.producer,
                                                        Ok(VideoMode::Frames),
                                                    );
                                                }
                                            }
                                            Err(_) => {
                                                lease.release_healthy();
                                                mailbox.set_transition_result(
                                                    req.producer,
                                                    Err(VideoError::Sink(SinkError::Unavailable)),
                                                );
                                            }
                                        },
                                        Err(_) => {
                                            mailbox.set_transition_result(
                                                req.producer,
                                                Err(VideoError::Sink(SinkError::Unavailable)),
                                            );
                                        }
                                    }
                                } else {
                                    mailbox.set_transition_result(
                                        req.producer,
                                        Err(VideoError::Sink(SinkError::Unavailable)),
                                    );
                                }
                            } else if active_lease.is_some() && current_window.is_some() {
                                let admission_error = pending_transition_error(req).or_else(|| {
                                    (backend.current_surface_id() != current_binding.as_ref().map(|binding| binding.id())
                                        || backend.video_output().revision != req.output_revision
                                        || backend.is_suspended())
                                        .then_some(VideoError::Superseded)
                                });
                                mailbox.set_transition_result(
                                    req.producer,
                                    admission_error.map_or(Ok(VideoMode::Frames), Err),
                                );
                            } else {
                                mailbox.set_transition_result(
                                    req.producer,
                                    Err(VideoError::Sink(SinkError::Unavailable)),
                                );
                            }
                            applied_request_key = Some(key);
                        }
                        VideoTarget::Retired => {
                            applied_request_key = Some(key);
                        }
                    }
                }
            }
        }

        // Software frame path.
        if active_codec.is_none() && sw_frame_info.is_some() {
            if let Some(window) = current_window.as_ref() {
                if sw_frame_info.is_some() {
                    let has_frame = {
                        let state = mailbox.state.lock();
                        matches!(state.input, Some(MailboxInput::Frame { .. }))
                    };
                    if has_frame {
                        if let Some(held) = mailbox.take_input_for_owner() {
                            let (frame, pts, producer, accepted, frame_epoch) = match held.input {
                                MailboxInput::Frame {
                                    frame,
                                    pts,
                                    producer,
                                    request,
                                    frame_epoch,
                                } => (frame, pts, producer, request, frame_epoch),
                                other => {
                                    mailbox.discard_owner_input(OwnerHeldInput { input: other });
                                    continue;
                                }
                            };
                            let (src_fmt, w, h, ready) = match &accepted.target {
                                VideoTarget::Frames { params, ready, .. } => (
                                    params.pixel_format.unwrap_or(PixelFormat::Yuv420P),
                                    params.width.unwrap_or(0),
                                    params.height.unwrap_or(0),
                                    ready,
                                ),
                                _ => unreachable!("frame admission requires Frames"),
                            };
                            let req_ok = mailbox.await_frame_policy(&accepted, frame_epoch, &backend);
                            if !req_ok || w == 0 || h == 0 {
                                mailbox.release_input_credit();
                            } else {
                                let frame_info = FrameInfo::new(src_fmt, w, h);
                                match oxideav_pixfmt::convert(
                                    &frame,
                                    frame_info,
                                    PixelFormat::Rgba,
                                    &oxideav_pixfmt::ConvertOptions::default(),
                                ) {
                                    Ok(rgba_frame) => {
                                        if !mailbox.await_frame_policy(&accepted, frame_epoch, &backend) {
                                            mailbox.release_input_credit();
                                            continue;
                                        }
                                        if let Err(e) = window.set_buffers_geometry(
                                            w as i32,
                                            h as i32,
                                            Some(HardwareBufferFormat::R8G8B8A8_UNORM),
                                        ) {
                                            mailbox.set_async_error(
                                                producer,
                                                VideoError::Sink(SinkError::Fatal(format!(
                                                    "geometry: {e:?}"
                                                ))),
                                            );
                                            mailbox.release_input_credit();
                                        } else {
                                            match window.lock(None) {
                                                Ok(mut guard) => {
                                                    let Some(bpp) =
                                                        guard.format().bytes_per_pixel()
                                                    else {
                                                        mailbox.set_async_error(
                                                            producer,
                                                            VideoError::Sink(SinkError::Fatal(
                                                                "locked buffer format has no bytes_per_pixel"
                                                                    .into(),
                                                            )),
                                                        );
                                                        mailbox.release_input_credit();
                                                        drop(guard);
                                                        continue;
                                                    };
                                                    let dst_stride_bytes = guard.stride() * bpp;
                                                    let copy_h = (h as usize).min(guard.height());
                                                    let Some(dst) = guard.bytes() else {
                                                        mailbox.set_async_error(
                                                            producer,
                                                            VideoError::Sink(SinkError::Fatal(
                                                                "locked buffer has no writable bytes"
                                                                    .into(),
                                                            )),
                                                        );
                                                        mailbox.release_input_credit();
                                                        drop(guard);
                                                        continue;
                                                    };
                                                    let src_stride = rgba_frame.planes[0].stride;
                                                    let src_bytes = &rgba_frame.planes[0].data;
                                                    let copy_w = (w as usize * bpp)
                                                        .min(dst_stride_bytes)
                                                        .min(src_stride);
                                                    for y in 0..copy_h {
                                                        let s_off = y * src_stride;
                                                        let d_off = y * dst_stride_bytes;
                                                        if s_off + copy_w <= src_bytes.len()
                                                            && d_off + copy_w <= dst.len()
                                                        {
                                                            unsafe {
                                                                std::ptr::copy_nonoverlapping(
                                                                    src_bytes.as_ptr().add(s_off),
                                                                    dst.as_mut_ptr()
                                                                        .add(d_off)
                                                                        .cast(),
                                                                    copy_w,
                                                                );
                                                            }
                                                        }
                                                    }
                                                    drop(guard);
                                                    ready.ready(pts);
                                                    mailbox.release_input_credit();
                                                }
                                                Err(e) => {
                                                    mailbox.set_async_error(
                                                        producer,
                                                        VideoError::Sink(SinkError::Fatal(
                                                            format!("window.lock: {e:?}"),
                                                        )),
                                                    );
                                                    mailbox.release_input_credit();
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        mailbox.set_async_error(
                                            producer,
                                            VideoError::Sink(SinkError::Fatal(format!(
                                                "convert: {e:?}"
                                            ))),
                                        );
                                        mailbox.release_input_credit();
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(eos_prod) = eos_requested {
                if mailbox.is_credit_free_and_empty() {
                    mailbox.confirm_eos(eos_prod);
                }
            }
        }

        let wait_timeout = if mailbox.has_pending_input() {
            Duration::from_millis(1)
        } else {
            Duration::from_millis(10)
        };
        mailbox.wait_owner(wait_timeout);
    }
}

fn pending_transition_error(request: &VideoRequest) -> Option<VideoError> {
    if request.control.cancelled(request.producer, request.seek_generation) {
        Some(VideoError::Superseded)
    } else if request.control.active_now() > request.deadline {
        Some(VideoError::Sink(SinkError::Fatal(
            "native video transition timed out".into(),
        )))
    } else {
        None
    }
}

fn configure_and_start_codec(
    params: &CodecParameters,
    backend: &BackendShared,
    window: &NativeWindow,
    request: &VideoRequest,
    surface_id: SurfaceId,
    output_revision: u64,
    force_software: bool,
    codec_epoch: u64,
) -> Result<CodecSession, VideoError> {
    let check_current = || -> Result<(), VideoError> {
        if let Some(error) = pending_transition_error(request) {
            return Err(error);
        }
        if backend.current_surface_id() != Some(surface_id)
            || backend.video_output().revision != output_revision
            || backend.is_suspended()
        {
            return Err(VideoError::Superseded);
        }
        Ok(())
    };
    check_current()?;
    let mime = match codec_id_to_mime(&params.codec_id.0) {
        Some(m) => m,
        None => return Err(VideoError::Unsupported),
    };

    let (csd0, csd1, nal_len_size, packets_annex_b) = extract_csd(mime, &params.extradata);

    let codec = if force_software {
        match software_decoder_name(mime).and_then(MediaCodec::from_codec_name) {
            Some(c) => c,
            None => return Err(VideoError::Unsupported),
        }
    } else {
        match MediaCodec::from_decoder_type(mime) {
            Some(c) => c,
            None => return Err(VideoError::Unsupported),
        }
    };
    check_current()?;

    let mut format = MediaFormat::new();
    format.set_str("mime", mime);
    if let Some(w) = params.width {
        format.set_i32("width", w as i32);
    }
    if let Some(h) = params.height {
        format.set_i32("height", h as i32);
    }
    if !csd0.is_empty() {
        format.set_buffer("csd-0", &csd0);
    }
    if !csd1.is_empty() {
        format.set_buffer("csd-1", &csd1);
    }

    check_current()?;

    match codec.configure(&format, Some(window), MediaCodecDirection::Decoder) {
        Ok(()) => {}
        Err(e) => {
            return Err(VideoError::Sink(SinkError::Fallback(format!(
                "configure: {e:?}"
            ))));
        }
    }

    check_current()?;

    match codec.start() {
        Ok(()) => Ok(CodecSession {
            codec,
            meta: CodecMeta {
                mime: mime.to_string(),
                width: params.width.unwrap_or(0),
                height: params.height.unwrap_or(0),
                csd0,
                csd1,
                nal_length_size: nal_len_size,
                packets_are_annex_b: packets_annex_b,
                bound_surface_id: surface_id,
                output_revision,
                prefer_software: force_software,
                has_seen_output: false,
                csd_progress: CsdProgress::Done,
                current_producer: request.producer,
                codec_epoch,
            },
        }),
        Err(e) => Err(VideoError::Sink(SinkError::Fallback(format!(
            "start: {e:?}"
        )))),
    }
}

fn extract_csd(mime: &str, extradata: &[u8]) -> (Vec<u8>, Vec<u8>, usize, bool) {
    let mut csd0 = Vec::new();
    let mut csd1 = Vec::new();
    let mut nal_len_size = 4;
    let mut packets_annex_b = false;

    if mime == "video/avc" {
        if !extradata.is_empty() {
            packets_annex_b = is_annex_b(extradata);
            if let Some((s0, s1, nls)) = parse_avcc_to_annex_b(extradata) {
                csd0 = s0;
                csd1 = s1;
                nal_len_size = nls;
            }
        }
    } else if mime == "video/hevc" {
        if !extradata.is_empty() {
            packets_annex_b = is_annex_b(extradata);
            if let Some((s0, nls)) = parse_hvcc_to_annex_b(extradata) {
                csd0 = s0;
                nal_len_size = nls;
            }
        }
    } else if !extradata.is_empty() {
        csd0 = extradata.to_vec();
    }

    (csd0, csd1, nal_len_size, packets_annex_b)
}

fn software_decoder_name(mime: &str) -> Option<&'static str> {
    match mime {
        "video/avc" => Some("c2.android.avc.decoder"),
        "video/hevc" => Some("c2.android.hevc.decoder"),
        "video/x-vnd.on2.vp8" => Some("c2.android.vp8.decoder"),
        "video/x-vnd.on2.vp9" => Some("c2.android.vp9.decoder"),
        "video/av01" => Some("c2.android.av1.decoder"),
        "video/mp4v-es" => Some("c2.android.mpeg4.decoder"),
        "video/3gpp" => Some("c2.android.h263.decoder"),
        _ => None,
    }
}

fn codec_id_to_mime(codec_id: &str) -> Option<&'static str> {
    match codec_id.to_ascii_lowercase().as_str() {
        "h264" | "avc" => Some("video/avc"),
        "hevc" | "h265" => Some("video/hevc"),
        "vp8" => Some("video/x-vnd.on2.vp8"),
        "vp9" => Some("video/x-vnd.on2.vp9"),
        "av1" => Some("video/av01"),
        "mpeg4" | "mp4v-es" => Some("video/mp4v-es"),
        "h263" | "3gpp" => Some("video/3gpp"),
        "mpeg2video" | "mpeg2" => Some("video/mpeg2"),
        _ => None,
    }
}

fn is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])
}

fn parse_avcc_to_annex_b(data: &[u8]) -> Option<(Vec<u8>, Vec<u8>, usize)> {
    if is_annex_b(data) {
        return Some((data.to_vec(), Vec::new(), 4));
    }
    if data.len() < 7 || data[0] != 1 {
        return None;
    }
    let nal_length_size = ((data[4] & 0x03) + 1) as usize;
    let num_sps = (data[5] & 0x1F) as usize;
    let mut offset = 6;
    let mut csd0 = Vec::new();

    for _ in 0..num_sps {
        if offset + 2 > data.len() {
            return None;
        }
        let sps_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
        offset += 2;
        if offset + sps_len > data.len() {
            return None;
        }
        csd0.extend_from_slice(&[0, 0, 0, 1]);
        csd0.extend_from_slice(&data[offset..offset + sps_len]);
        offset += sps_len;
    }

    let mut csd1 = Vec::new();
    if offset < data.len() {
        let num_pps = data[offset] as usize;
        offset += 1;
        for _ in 0..num_pps {
            if offset + 2 > data.len() {
                return None;
            }
            let pps_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
            offset += 2;
            if offset + pps_len > data.len() {
                return None;
            }
            csd1.extend_from_slice(&[0, 0, 0, 1]);
            csd1.extend_from_slice(&data[offset..offset + pps_len]);
            offset += pps_len;
        }
    }

    Some((csd0, csd1, nal_length_size))
}

fn parse_hvcc_to_annex_b(data: &[u8]) -> Option<(Vec<u8>, usize)> {
    if is_annex_b(data) {
        return Some((data.to_vec(), 4));
    }
    if data.len() < 23 || data[0] != 1 {
        return None;
    }
    let nal_length_size = ((data[21] & 0x03) + 1) as usize;
    let num_arrays = data[22] as usize;
    let mut offset = 23;
    let mut csd0 = Vec::new();

    for _ in 0..num_arrays {
        if offset + 3 > data.len() {
            return None;
        }
        let num_nalus = u16::from_be_bytes([data[offset + 1], data[offset + 2]]) as usize;
        offset += 3;
        for _ in 0..num_nalus {
            if offset + 2 > data.len() {
                return None;
            }
            let nalu_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
            offset += 2;
            if offset + nalu_len > data.len() {
                return None;
            }
            csd0.extend_from_slice(&[0, 0, 0, 1]);
            csd0.extend_from_slice(&data[offset..offset + nalu_len]);
            offset += nalu_len;
        }
    }

    Some((csd0, nal_length_size))
}
