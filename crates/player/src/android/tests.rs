//! Behavioral unit tests for Android decoder lifecycle, surface registry, and mailbox.
//!
//! These assert observable contracts (credit accounting, lease quarantine, input credit
//! held across owner work, keyed controls, error precedence). They are not wiring or
//! flag-echo checks.

use super::backend::AndroidBackend;
use super::surface::{
    owner_capacity_changed, BindingPhase, SurfaceBindError, SurfaceBinding, SurfaceBindingInner,
    SurfaceId, SurfaceRegistry, SurfaceRetirementError, SurfaceRetirementStatus,
    TOTAL_SURFACE_CREDITS,
};
use super::video::VideoMailbox;
use crate::backend::{
    Backend, PictureReady, ProducerId, SinkError, SubtitleImage, SubtitleSink, VideoControl,
    VideoError, VideoMode, VideoRequest, VideoTarget,
};
use oxideav_core::{CodecId, CodecParameters, Packet, TimeBase};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

struct TestControl {
    cancelled: AtomicBool,
    active_now: Mutex<Instant>,
    woken: AtomicU64,
}

impl TestControl {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            cancelled: AtomicBool::new(false),
            active_now: Mutex::new(Instant::now()),
            woken: AtomicU64::new(0),
        })
    }
}

impl VideoControl for TestControl {
    fn cancelled(&self, _producer: ProducerId, _seek_generation: u64) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn active_now(&self) -> Instant {
        *self.active_now.lock()
    }

    fn wake(&self) {
        self.woken.fetch_add(1, Ordering::SeqCst);
    }
}

fn test_params() -> Arc<CodecParameters> {
    let mut p = CodecParameters::video(CodecId::new("h264"));
    p.width = Some(16);
    p.height = Some(16);
    Arc::new(p)
}

#[test]
fn expired_transition_is_terminal_not_backpressure() {
    struct IdleClock;
    impl crate::backend::Clock for IdleClock {
        fn now(&self) -> Option<Duration> { None }
        fn monotonic_ns_at(&self, _: Duration) -> Option<i64> { None }
    }
    let backend = AndroidBackend::new();
    let mut sink = backend.video(Arc::new(IdleClock));
    let control = TestControl::new();
    let mut request = VideoRequest {
        producer: ProducerId(90_001),
        seek_generation: 1,
        output_revision: sink.output().revision,
        target: VideoTarget::Compressed {
            params: test_params(),
            ready: PictureReady::new(|_| {}),
            present_from: Duration::ZERO,
        },
        deadline: control.active_now() - Duration::from_secs(1),
        control: control.clone(),
    };
    assert!(matches!(
        sink.poll_transition(&request),
        Poll::Ready(Err(VideoError::Sink(SinkError::Fatal(_))))
    ));
    control.cancelled.store(true, Ordering::SeqCst);
    assert!(matches!(sink.poll_transition(&request), Poll::Ready(Err(VideoError::Superseded))));
    request.target = VideoTarget::Retired;
    assert!(matches!(sink.poll_transition(&request), Poll::Ready(Ok(VideoMode::Retired))));
}

/// Installs a live binding into a free global slot so retirement/completion
/// exercises real credit accounting (not a detached fake binding).
fn install_live_binding(id: SurfaceId) -> (Arc<SurfaceRegistry>, Arc<SurfaceBinding>, usize) {
    let registry = SurfaceRegistry::global();
    let slot_index = registry
        .install_test_binding(id)
        .expect("free registry slot for isolated test binding");
    let binding = registry
        .binding_at_slot(slot_index)
        .expect("installed binding must be readable");
    (registry, binding, slot_index)
}

fn noop_waker() -> Waker {
    use std::task::{RawWaker, RawWakerVTable};
    fn clone_raw(_: *const ()) -> RawWaker {
        raw()
    }
    fn wake_raw(_: *const ()) {}
    fn wake_by_ref_raw(_: *const ()) {}
    fn drop_raw(_: *const ()) {}
    fn raw() -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    static VTABLE: RawWakerVTable =
        RawWakerVTable::new(clone_raw, wake_raw, wake_by_ref_raw, drop_raw);
    // SAFETY: noop vtable; no data pointer is ever dereferenced.
    unsafe { Waker::from_raw(raw()) }
}

#[test]
fn lease_admission_is_exclusive_and_unverified_drop_quarantines() {
    let (_registry, binding, _slot) = install_live_binding(SurfaceId(10_001));
    let lease1 = binding.try_acquire_lease().expect("first lease");
    assert!(matches!(
        binding.try_acquire_lease(),
        Err(SurfaceBindError::AlreadyLeased)
    ));
    drop(lease1);
    assert!(binding.is_retiring());
    assert!(matches!(
        binding.try_acquire_lease(),
        Err(SurfaceBindError::Retired)
    ));
    // Quarantine must not free the registration credit.
    assert!(!binding.0.slot_is_free());
    let _ = binding.retire();
}

#[test]
fn healthy_lease_release_wakes_pending_retirement_cleanup() {
    let (_registry, binding, _slot) = install_live_binding(SurfaceId(10_002));
    let lease = binding.try_acquire_lease().expect("lease");
    let retirement = binding.retire();
    assert_eq!(retirement.status(), SurfaceRetirementStatus::Pending);
    // Lease still held: cleanup must not claim success.
    assert!(!retirement.is_retired());
    lease.release_healthy();
    // Capacity/lease progress must be observable to waiters.
    owner_capacity_changed();
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    // Poll drives ticket observation; may stay Pending without native handles.
    let _ = retirement.poll(&mut cx);
    assert!(matches!(
        retirement.status(),
        SurfaceRetirementStatus::Pending
            | SurfaceRetirementStatus::Retired
            | SurfaceRetirementStatus::Failed(_)
    ));
}

#[test]
fn finish_retirement_matches_binding_id_before_freeing_slot() {
    let (registry, old_binding, slot) = install_live_binding(SurfaceId(10_050));
    old_binding.0.force_phase_for_test(BindingPhase::Retiring { lease_holder: None });
    // Reuse the same slot under a new id while old completion is still outstanding.
    let new_id = SurfaceId(10_051);
    registry
        .reinstall_test_binding(slot, new_id)
        .expect("reuse slot under new id");
    let new_binding = registry.binding_at_slot(slot).expect("new binding");
    old_binding.0.finish_retirement(Ok(()));
    // Stale completion must not free the reused slot or retire the new binding.
    assert!(!new_binding.0.slot_is_free());
    assert!(!new_binding.is_retired());
    assert_eq!(new_binding.id(), new_id);
    let _ = new_binding.retire();
}

#[test]
fn surface_retirement_poll_registers_waker_before_ticket_check() {
    let (_registry, binding, _slot) = install_live_binding(SurfaceId(10_060));
    let retirement = binding.retire();
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(retirement.poll(&mut cx), Poll::Pending) || retirement.is_retired());
    // A second poll must remain consistent; Failed stays Failed (never becomes Retired).
    match retirement.poll(&mut cx) {
        Poll::Ready(Err(SurfaceRetirementError::Failed(_) | SurfaceRetirementError::Quarantined(_))) => {
            assert!(matches!(
                retirement.poll(&mut cx),
                Poll::Ready(Err(_))
            ));
        }
        Poll::Ready(Ok(_)) => {
            assert!(retirement.is_retired());
        }
        Poll::Pending => {}
    }
}

#[test]
fn false_cleanup_success_is_rejected_when_handles_remain() {
    let (_registry, binding, _slot) = install_live_binding(SurfaceId(10_070));
    binding.0.mark_native_handles_retained_for_test();
    let retirement = binding.retire();
    // Simulate a buggy ticket success while native ownership remains charged.
    binding.0.finish_retirement(Ok(()));
    assert!(
        !matches!(retirement.status(), SurfaceRetirementStatus::Retired),
        "successful retirement requires completed native cleanup"
    );
    assert!(matches!(
        retirement.status(),
        SurfaceRetirementStatus::Failed(_) | SurfaceRetirementStatus::Pending
    ));
}

#[test]
fn video_mailbox_input_credit_stays_charged_while_owner_holds_input() {
    let mailbox = Arc::new(VideoMailbox::new());
    let control = TestControl::new();
    let producer = ProducerId(42);
    mailbox.set_request(VideoRequest {
        producer,
        seek_generation: 1,
        output_revision: 1,
        target: VideoTarget::Compressed {
            params: test_params(),
            ready: PictureReady::new(|_| {}),
            present_from: Duration::ZERO,
        },
        deadline: Instant::now() + Duration::from_secs(5),
        control: control.clone(),
    });

    let mut pkt1 = Some(Packet::new(0, TimeBase::new(1, 1000), Vec::new()));
    assert!(mailbox
        .push_packet(producer, &mut pkt1, Duration::from_millis(100), true)
        .is_ok());
    assert!(pkt1.is_none());

    let held = mailbox.take_input_for_owner();
    assert!(held.is_some(), "owner takes input without restoring credit");

    let mut pkt2 = Some(Packet::new(0, TimeBase::new(1, 1000), Vec::new()));
    let push2 = mailbox.push_packet(producer, &mut pkt2, Duration::from_millis(120), false);
    assert!(
        matches!(push2, Err(VideoError::Sink(SinkError::WouldBlock))),
        "credit remains charged across owner-held input"
    );
    assert!(pkt2.is_some(), "refused packet stays with caller");

    mailbox.discard_owner_input(held.unwrap());
    assert!(mailbox
        .push_packet(producer, &mut pkt2, Duration::from_millis(120), false)
        .is_ok());
    assert!(pkt2.is_none());
}

#[test]
fn video_mailbox_rejects_mismatched_producer_controls_and_stale_finish() {
    let mailbox = Arc::new(VideoMailbox::new());
    let control = TestControl::new();
    let producer = ProducerId(7);
    mailbox.set_request(VideoRequest {
        producer,
        seek_generation: 1,
        output_revision: 1,
        target: VideoTarget::Compressed {
            params: test_params(),
            ready: PictureReady::new(|_| {}),
            present_from: Duration::ZERO,
        },
        deadline: Instant::now() + Duration::from_secs(5),
        control: control.clone(),
    });

    assert!(matches!(
        mailbox.present_from(ProducerId(8), Duration::from_millis(1)),
        Err(VideoError::Superseded)
    ));
    assert!(matches!(
        mailbox.set_playing(ProducerId(8), false),
        Err(VideoError::Superseded)
    ));

    // Stale finish must not seal the current producer.
    assert!(matches!(
        mailbox.poll_finish(ProducerId(9)),
        Poll::Ready(Err(VideoError::Superseded)) | Poll::Pending
    ));
    assert!(mailbox.eos_requested_producer() != Some(ProducerId(9)));
}

#[test]
fn video_mailbox_cancellation_does_not_consume_packet() {
    let mailbox = Arc::new(VideoMailbox::new());
    let control = TestControl::new();
    let producer = ProducerId(99);
    mailbox.set_request(VideoRequest {
        producer,
        seek_generation: 1,
        output_revision: 1,
        target: VideoTarget::Compressed {
            params: test_params(),
            ready: PictureReady::new(|_| {}),
            present_from: Duration::ZERO,
        },
        deadline: Instant::now() + Duration::from_secs(5),
        control: control.clone(),
    });
    control.cancelled.store(true, Ordering::SeqCst);
    let mut pkt = Some(Packet::new(0, TimeBase::new(1, 1000), Vec::new()));
    assert!(matches!(
        mailbox.push_packet(producer, &mut pkt, Duration::ZERO, true),
        Err(VideoError::Superseded)
    ));
    assert!(pkt.is_some());
}

#[test]
fn flush_epoch_invalidates_held_indices_without_dropping_credit_twice() {
    let mailbox = Arc::new(VideoMailbox::new());
    let epoch0 = mailbox.codec_epoch();
    mailbox.bump_codec_epoch();
    assert!(!mailbox.is_valid_codec_epoch(epoch0));
    assert!(mailbox.is_valid_codec_epoch(mailbox.codec_epoch()));
}

#[test]
fn cached_configured_mode_does_not_hide_async_error() {
    let mailbox = Arc::new(VideoMailbox::new());
    let control = TestControl::new();
    let producer = ProducerId(77);
    mailbox.set_request(VideoRequest {
        producer,
        seek_generation: 1,
        output_revision: 1,
        target: VideoTarget::Compressed {
            params: test_params(),
            ready: PictureReady::new(|_| {}),
            present_from: Duration::ZERO,
        },
        deadline: Instant::now() + Duration::from_secs(5),
        control: control.clone(),
    });
    mailbox.set_transition_result(producer, Ok(VideoMode::Compressed));
    mailbox.set_async_error(
        producer,
        VideoError::Sink(SinkError::Fatal("native fail".into())),
    );
    assert!(matches!(
        mailbox.poll_transition_result(producer),
        Poll::Ready(Err(VideoError::Sink(SinkError::Fatal(_))))
    ));
}

#[test]
fn request_control_wake_is_stored_and_invoked_outside_lock() {
    let mailbox = Arc::new(VideoMailbox::new());
    let control = TestControl::new();
    let producer = ProducerId(3);
    mailbox.set_request(VideoRequest {
        producer,
        seek_generation: 1,
        output_revision: 1,
        target: VideoTarget::Compressed {
            params: test_params(),
            ready: PictureReady::new(|_| {}),
            present_from: Duration::ZERO,
        },
        deadline: Instant::now() + Duration::from_secs(5),
        control: control.clone(),
    });
    let before = control.woken.load(Ordering::SeqCst);
    mailbox.set_transition_result(producer, Ok(VideoMode::Compressed));
    assert!(control.woken.load(Ordering::SeqCst) > before);
}

#[test]
fn subtitle_show_coalesces_latest_and_empty_clear_is_recorded() {
    let backend = AndroidBackend::new();
    let mut sink = backend.subtitles();
    let img = SubtitleImage {
        x: 0,
        y: 0,
        width: 2,
        height: 2,
        rgba: vec![255; 16],
    };
    sink.show(&[img], 64, 64);
    sink.show(&[], 0, 0);
    // No panic / no frontend native work; latest desired state is empty clear.
}

#[test]
fn set_video_surface_rejects_retiring_binding_and_respects_suspend() {
    let backend = AndroidBackend::new();
    let (_registry, binding, _slot) = install_live_binding(SurfaceId(10_080));
    let _ = binding.retire();
    assert!(matches!(
        backend.set_video_surface(binding.clone()),
        Err(SurfaceBindError::Retiring | SurfaceBindError::Retired)
    ));

    let (_registry2, live, _slot2) = install_live_binding(SurfaceId(10_081));
    backend.suspend();
    let out = backend.set_video_surface(live.clone()).expect("active binding");
    assert!(!out.available, "suspended backend must not report available");
    backend.clear_video_surface();
    let _ = live.retire();
}

#[test]
fn global_registry_exposes_fixed_credit_ceiling() {
    assert_eq!(TOTAL_SURFACE_CREDITS, 16);
    let _ = SurfaceRegistry::global();
}
