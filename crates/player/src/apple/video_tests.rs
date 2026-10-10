use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::task::Poll;
use std::time::{Duration, Instant};

use oxideav_core::{CodecId, CodecParameters, Packet, TimeBase, VideoFrame};
use parking_lot::Mutex;

use crate::apple::video::{
    drop_held_input_credit, LayerLeaseState, Mailbox, MailboxInput, MainBridgeSlot,
};
use crate::backend::{
    PictureReady, ProducerId, SinkError, VideoControl, VideoError, VideoMode, VideoRequest,
    VideoTarget,
};

struct TestControl {
    cancelled_producer: AtomicU64,
    cancelled_seek: AtomicU64,
    active_now_offset: AtomicU64,
    wake_count: AtomicU64,
}

impl TestControl {
    fn new() -> Self {
        Self {
            cancelled_producer: AtomicU64::new(u64::MAX),
            cancelled_seek: AtomicU64::new(u64::MAX),
            active_now_offset: AtomicU64::new(0),
            wake_count: AtomicU64::new(0),
        }
    }
}

impl VideoControl for TestControl {
    fn cancelled(&self, producer: ProducerId, seek_generation: u64) -> bool {
        producer.0 == self.cancelled_producer.load(Ordering::Acquire)
            || seek_generation == self.cancelled_seek.load(Ordering::Acquire)
    }

    fn active_now(&self) -> Instant {
        static START: LazyLock<Instant> = LazyLock::new(Instant::now);
        *START + Duration::from_millis(self.active_now_offset.load(Ordering::Acquire))
    }

    fn wake(&self) {
        self.wake_count.fetch_add(1, Ordering::Release);
    }
}

fn dyn_control(c: Arc<TestControl>) -> Arc<dyn VideoControl> {
    c
}

fn test_packet(data: Vec<u8>) -> Packet {
    Packet::new(0, TimeBase::new(1, 1000), data)
}

fn test_frame() -> VideoFrame {
    VideoFrame {
        pts: None,
        planes: vec![],
    }
}

fn compressed_request(
    producer: u64,
    control: Arc<dyn VideoControl>,
    deadline: Instant,
    present_from: Duration,
) -> VideoRequest {
    let mut params = CodecParameters::video(CodecId::new("h264"));
    params.width = Some(1920);
    params.height = Some(1080);
    VideoRequest {
        producer: ProducerId(producer),
        seek_generation: 0,
        output_revision: 1,
        target: VideoTarget::Compressed {
            params: Arc::new(params),
            ready: PictureReady::new(|_| {}),
            present_from,
        },
        deadline,
        control,
    }
}

fn frames_request(
    producer: u64,
    control: Arc<dyn VideoControl>,
    deadline: Instant,
    reset: bool,
) -> VideoRequest {
    let mut params = CodecParameters::video(CodecId::new("rawvideo"));
    params.width = Some(64);
    params.height = Some(64);
    VideoRequest {
        producer: ProducerId(producer),
        seek_generation: 0,
        output_revision: 1,
        target: VideoTarget::Frames {
            params: Arc::new(params),
            ready: PictureReady::new(|_| {}),
            reset,
        },
        deadline,
        control,
    }
}

#[test]
fn test_layer_lease_mutual_exclusion() {
    let lease = Arc::new(Mutex::new(LayerLeaseState::new()));
    assert!(lease.lock().try_reserve(1));
    assert!(!lease.lock().try_reserve(2));
    lease.lock().confirm_bound(1);
    assert!(!lease.lock().try_reserve(2));
    lease.lock().start_unbind(1);
    assert!(lease.lock().finish_unbind(1, true));
    assert!(lease.lock().try_reserve(2));
}

#[test]
fn test_cleanup_false_error_retains_lease() {
    let lease = Arc::new(Mutex::new(LayerLeaseState::new()));
    assert!(lease.lock().try_reserve(1));
    lease.lock().confirm_bound(1);
    lease.lock().start_unbind(1);
    assert!(!lease.lock().finish_unbind(1, false));
    assert!(lease.lock().quarantined);
    assert!(!lease.lock().try_reserve(2));
}

#[test]
fn test_unused_reservation_releases_without_false_unbind() {
    let lease = Arc::new(Mutex::new(LayerLeaseState::new()));
    assert!(lease.lock().try_reserve(7));
    lease.lock().release_unused_reservation(7);
    assert_eq!(lease.lock().reserved_sink_id, None);
    assert!(!lease.lock().quarantined);
    assert!(lease.lock().try_reserve(8));
}

#[test]
fn test_same_request_polled_during_setup() {
    let mailbox = Arc::new(Mailbox::new());
    let control = dyn_control(Arc::new(TestControl::new()));
    let deadline = control.active_now() + Duration::from_secs(5);
    let req = compressed_request(1, Arc::clone(&control), deadline, Duration::ZERO);
    assert!(mailbox.submit_request(&req).is_ok());
    assert!(mailbox.take_setup_request().is_some());
    assert!(mailbox.submit_request(&req).is_ok());
    assert!(mailbox.take_setup_request().is_none());
}

#[test]
fn test_async_ack_not_completed_on_cancellation() {
    let mut slot = MainBridgeSlot::new();
    let t = slot.begin_operation(ProducerId(5), 10);
    assert!(!slot.can_begin());
    slot.complete_operation(t, Ok(()));
    assert!(slot.can_begin());
}

#[test]
fn test_exact_ticket_completion_not_max() {
    let mut slot = MainBridgeSlot::new();
    let t1 = slot.begin_operation(ProducerId(1), 0);
    slot.complete_operation(t1, Ok(()));
    let t2 = slot.begin_operation(ProducerId(2), 0);
    slot.complete_operation(1, Ok(()));
    assert!(slot.in_flight, "stale ticket ack must not clear active credit");
    slot.complete_operation(t2, Err("fail".into()));
    assert!(!slot.in_flight);
}

#[test]
fn test_cancelled_predecessor_requires_live_preserving_successor() {
    let mailbox = Mailbox::new();
    let control = Arc::new(TestControl::new());
    let old_control = dyn_control(Arc::clone(&control));
    let deadline = old_control.active_now() + Duration::from_secs(5);
    let first = frames_request(1, Arc::clone(&old_control), deadline, true);
    mailbox.submit_request(&first).unwrap();
    {
        let mut state = mailbox.lock();
        state.current_producer = Some(first.producer);
        state.configured_mode = Some(VideoMode::Frames);
    }
    let mut frame = Some(test_frame());
    mailbox.push_frame(first.producer, &mut frame, Duration::ZERO).unwrap();
    let accepted = mailbox.take_input().unwrap();
    control.cancelled_producer.store(1, Ordering::Release);
    let successor_control = Arc::new(TestControl::new());
    let successor = frames_request(2, dyn_control(Arc::clone(&successor_control)), deadline, false);
    mailbox.submit_request(&successor).unwrap();
    // Submission must preserve already-owned work before the owner sees setup.
    assert!(!Mailbox::cancel_blocks_native(&mailbox.lock(), &old_control, accepted.producer(), 0));
    successor_control.cancelled_producer.store(2, Ordering::Release);
    assert!(Mailbox::cancel_blocks_native(&mailbox.lock(), &old_control, accepted.producer(), 0));
    successor_control.cancelled_producer.store(u64::MAX, Ordering::Release);
    let reset = frames_request(3, Arc::clone(&old_control), deadline, true);
    mailbox.submit_request(&reset).unwrap();
    assert!(Mailbox::cancel_blocks_native(&mailbox.lock(), &old_control, accepted.producer(), 0));
    // A later preserving request cannot revive work across the intervening reset.
    let later = frames_request(4, old_control.clone(), deadline, false);
    mailbox.submit_request(&later).unwrap();
    assert!(Mailbox::cancel_blocks_native(&mailbox.lock(), &old_control, accepted.producer(), 0));
}

/// Owner-path regression: backpressure holds a charged sample; compressed/reset
/// setup discards it via `drop_held_input_credit`. Missing that release leaves
/// `input_held_by_owner` stuck and every subsequent push WouldBlocks forever.
#[test]
fn test_input_credit_released_when_held_discarded_on_setup() {
    let mailbox = Arc::new(Mailbox::new());
    {
        let mut state = mailbox.lock();
        state.latest_producer = Some(ProducerId(3));
        state.current_producer = Some(ProducerId(3));
        state.configured_mode = Some(VideoMode::Frames);
    }
    let mut frame = Some(test_frame());
    assert!(mailbox
        .push_frame(ProducerId(3), &mut frame, Duration::ZERO)
        .is_ok());
    assert!(mailbox.take_input().is_some());
    assert!(mailbox.lock().input_held_by_owner);

    let mut frame2 = Some(test_frame());
    assert!(matches!(
        mailbox.push_frame(ProducerId(3), &mut frame2, Duration::from_millis(1)),
        Err(VideoError::Sink(SinkError::WouldBlock))
    ));
    assert!(frame2.is_some());

    // Central setup/reset discard path (discard_held_output → drop_held_input_credit).
    drop_held_input_credit(&mailbox, true);
    assert!(
        !mailbox.lock().input_held_by_owner,
        "setup discard must free charged held credit"
    );

    assert!(mailbox
        .push_frame(ProducerId(3), &mut frame2, Duration::from_millis(1))
        .is_ok());
    assert!(frame2.is_none());
}

#[test]
fn test_threshold_update_while_setup_blocked() {
    let mailbox = Arc::new(Mailbox::new());
    let control = dyn_control(Arc::new(TestControl::new()));
    let deadline = control.active_now() + Duration::from_secs(5);
    let req = compressed_request(
        4,
        Arc::clone(&control),
        deadline,
        Duration::from_millis(100),
    );
    assert!(mailbox.submit_request(&req).is_ok());
    assert_eq!(mailbox.lock().present_from, Duration::from_millis(100));
    assert!(mailbox
        .present_from(ProducerId(4), Duration::from_millis(250))
        .is_ok());
    assert_eq!(mailbox.lock().present_from, Duration::from_millis(250));
    assert_ne!(mailbox.lock().present_from, Duration::from_millis(100));
}

#[test]
fn test_retirement_after_expired_setup_deadline() {
    let mailbox = Arc::new(Mailbox::new());
    let raw = Arc::new(TestControl::new());
    let control = dyn_control(Arc::clone(&raw));
    let deadline = control.active_now() + Duration::from_millis(50);
    let mut retired = compressed_request(9, Arc::clone(&control), deadline, Duration::ZERO);
    retired.target = VideoTarget::Retired;
    raw.active_now_offset.store(5_000, Ordering::Release);
    assert!(mailbox.check_transition_deadline(&retired).is_ok());
}

#[test]
fn test_setup_deadline_still_applies_when_pending() {
    let mailbox = Arc::new(Mailbox::new());
    let raw = Arc::new(TestControl::new());
    let control = dyn_control(Arc::clone(&raw));
    let deadline = control.active_now() + Duration::from_millis(50);
    let req = compressed_request(8, Arc::clone(&control), deadline, Duration::ZERO);
    raw.active_now_offset.store(5_000, Ordering::Release);
    assert!(matches!(
        mailbox.check_transition_deadline(&req),
        Err(VideoError::Sink(SinkError::WouldBlock))
    ));
}

#[test]
fn test_steady_playback_beyond_startup_deadline() {
    let mailbox = Arc::new(Mailbox::new());
    let raw = Arc::new(TestControl::new());
    let control = dyn_control(Arc::clone(&raw));
    let deadline = control.active_now() + Duration::from_millis(100);
    let req = compressed_request(7, Arc::clone(&control), deadline, Duration::ZERO);
    {
        let mut state = mailbox.lock();
        state.current_producer = Some(ProducerId(7));
        state.latest_producer = Some(ProducerId(7));
        state.configured_mode = Some(VideoMode::Compressed);
        state.transition_result = Some((ProducerId(7), Ok(VideoMode::Compressed)));
    }
    raw.active_now_offset.store(10_000, Ordering::Release);
    assert!(mailbox.check_transition_deadline(&req).is_ok());
}

#[test]
fn test_after_final_input_error() {
    let mailbox = Arc::new(Mailbox::new());
    let control = dyn_control(Arc::new(TestControl::new()));
    {
        let mut state = mailbox.lock();
        state.current_producer = Some(ProducerId(10));
        state.latest_producer = Some(ProducerId(10));
        state.configured_mode = Some(VideoMode::Compressed);
        state.transition_result = Some((ProducerId(10), Ok(VideoMode::Compressed)));
        state.drain_completed = Some(ProducerId(10));
    }
    mailbox.report_error(
        ProducerId(10),
        VideoError::Sink(SinkError::Fallback("late decode failure".into())),
        &control,
    );
    assert!(matches!(
        mailbox.poll_transition_status(ProducerId(10)),
        Poll::Ready(Err(VideoError::Sink(SinkError::Fallback(_))))
    ));
    assert!(matches!(
        mailbox.poll_finish(ProducerId(10)),
        Poll::Ready(Err(VideoError::Sink(SinkError::Fallback(_))))
    ));
}

#[test]
fn test_report_error_rejects_stale_producer() {
    let mailbox = Arc::new(Mailbox::new());
    let control = dyn_control(Arc::new(TestControl::new()));
    {
        let mut state = mailbox.lock();
        state.latest_producer = Some(ProducerId(5));
    }
    mailbox.report_error(
        ProducerId(5),
        VideoError::Sink(SinkError::Fallback("current".into())),
        &control,
    );
    mailbox.report_error(
        ProducerId(3),
        VideoError::Sink(SinkError::Fallback("stale".into())),
        &control,
    );
    assert!(matches!(
        &mailbox.lock().current_error,
        Some((ProducerId(5), VideoError::Sink(SinkError::Fallback(msg)))) if msg == "current"
    ));
}

#[test]
fn test_control_revision_wakes_owner_predicate() {
    let mailbox = Arc::new(Mailbox::new());
    {
        let mut state = mailbox.lock();
        state.latest_producer = Some(ProducerId(1));
        state.current_producer = Some(ProducerId(1));
        state.configured_mode = Some(VideoMode::Compressed);
    }
    assert!(!Mailbox::owner_wait_predicate(&mailbox.lock(), 0, false));
    assert!(mailbox.set_playing(ProducerId(1), true).is_ok());
    let state = mailbox.lock();
    assert!(Mailbox::owner_wait_predicate(&state, 0, false));
    assert!(!Mailbox::owner_wait_predicate(
        &state,
        state.control_revision,
        false
    ));
}

#[test]
fn test_mailbox_single_input_credit_preserves_packet() {
    let mailbox = Arc::new(Mailbox::new());
    {
        let mut state = mailbox.lock();
        state.current_producer = Some(ProducerId(10));
        state.latest_producer = Some(ProducerId(10));
        state.configured_mode = Some(VideoMode::Compressed);
    }
    let mut packet1 = Some(test_packet(vec![0, 0, 0, 1, 0x65]));
    assert!(mailbox
        .push_packet(ProducerId(10), &mut packet1, Duration::from_millis(100), true)
        .is_ok());
    let mut packet2 = Some(test_packet(vec![0, 0, 0, 1, 0x41]));
    assert!(matches!(
        mailbox.push_packet(ProducerId(10), &mut packet2, Duration::from_millis(200), false),
        Err(VideoError::Sink(SinkError::WouldBlock))
    ));
    assert!(packet2.is_some());
    assert!(mailbox.take_input().is_some());
    mailbox.discard_input(ProducerId(10));
    assert!(mailbox
        .push_packet(ProducerId(10), &mut packet2, Duration::from_millis(200), false)
        .is_ok());
}

#[test]
fn test_mailbox_single_input_credit_preserves_frame() {
    let mailbox = Arc::new(Mailbox::new());
    {
        let mut state = mailbox.lock();
        state.current_producer = Some(ProducerId(20));
        state.latest_producer = Some(ProducerId(20));
        state.configured_mode = Some(VideoMode::Frames);
    }
    let mut frame1 = Some(test_frame());
    assert!(mailbox
        .push_frame(ProducerId(20), &mut frame1, Duration::from_millis(40))
        .is_ok());
    let mut frame2 = Some(test_frame());
    assert!(matches!(
        mailbox.push_frame(ProducerId(20), &mut frame2, Duration::from_millis(80)),
        Err(VideoError::Sink(SinkError::WouldBlock))
    ));
    assert!(mailbox.take_input().is_some());
    mailbox.discard_input(ProducerId(20));
    assert!(mailbox
        .push_frame(ProducerId(20), &mut frame2, Duration::from_millis(80))
        .is_ok());
}

#[test]
fn test_poll_finish_one_shot_drain_and_seal() {
    let mailbox = Arc::new(Mailbox::new());
    {
        let mut state = mailbox.lock();
        state.current_producer = Some(ProducerId(55));
        state.latest_producer = Some(ProducerId(55));
        state.configured_mode = Some(VideoMode::Compressed);
    }
    assert!(matches!(
        mailbox.poll_finish(ProducerId(55)),
        Poll::Pending
    ));
    assert_eq!(mailbox.lock().input_sealed, Some(ProducerId(55)));
    let mut packet = Some(test_packet(vec![1, 2, 3]));
    assert!(matches!(
        mailbox.push_packet(ProducerId(55), &mut packet, Duration::ZERO, false),
        Err(VideoError::Sink(SinkError::Fatal(_)))
    ));
    {
        let mut state = mailbox.lock();
        state.drain_completed = Some(ProducerId(55));
    }
    assert!(matches!(
        mailbox.poll_finish(ProducerId(55)),
        Poll::Ready(Ok(()))
    ));
}

#[test]
fn test_owner_wait_predicate_covers_setup_input_drain_held() {
    let mailbox = Mailbox::new();
    assert!(!Mailbox::owner_wait_predicate(&mailbox.lock(), 0, false));
    {
        let mut state = mailbox.lock();
        state.setup_pending = true;
        assert!(Mailbox::owner_wait_predicate(&state, 0, false));
        state.setup_pending = false;
        state.drain_requested = Some(ProducerId(1));
        assert!(Mailbox::owner_wait_predicate(&state, 0, false));
        state.drain_completed = Some(ProducerId(1));
        assert!(!Mailbox::owner_wait_predicate(&state, 0, false));
    }
    assert!(Mailbox::owner_wait_predicate(&mailbox.lock(), 0, true));
}

#[test]
fn test_partial_bind_quarantine_vs_unused_reservation() {
    let lease = Arc::new(Mutex::new(LayerLeaseState::new()));
    assert!(lease.lock().try_reserve(1));
    lease.lock().release_unused_reservation(1);
    assert_eq!(lease.lock().bound_sink_id, None);
    assert!(!lease.lock().quarantined);

    assert!(lease.lock().try_reserve(2));
    lease.lock().confirm_bound(2);
    lease.lock().start_unbind(2);
    assert!(!lease.lock().finish_unbind(2, false));
    assert!(lease.lock().quarantined);
    assert!(!lease.lock().try_reserve(3));
}

#[test]
fn test_preserving_replacement_blocks_new_until_current_advances() {
    let mailbox = Arc::new(Mailbox::new());
    let control = dyn_control(Arc::new(TestControl::new()));
    let deadline = control.active_now() + Duration::from_secs(5);
    let req1 = frames_request(1, Arc::clone(&control), deadline, true);
    assert!(mailbox.submit_request(&req1).is_ok());
    {
        let mut state = mailbox.lock();
        state.current_producer = Some(ProducerId(1));
        state.configured_mode = Some(VideoMode::Frames);
        state.setup_pending = false;
        state.transition_result = Some((ProducerId(1), Ok(VideoMode::Frames)));
    }
    let mut f1 = Some(test_frame());
    assert!(mailbox.push_frame(ProducerId(1), &mut f1, Duration::ZERO).is_ok());
    assert!(mailbox.take_input().is_some());

    let req2 = frames_request(2, Arc::clone(&control), deadline, false);
    assert!(mailbox.submit_request(&req2).is_ok());
    let mut f2 = Some(test_frame());
    assert!(matches!(
        mailbox.push_frame(ProducerId(2), &mut f2, Duration::from_millis(10)),
        Err(VideoError::Sink(SinkError::WouldBlock))
    ));
}
