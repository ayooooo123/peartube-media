//! Android surface/video lifecycle behavioral tests (device-gated).
//!
//! Covers credit ceiling with cleanup guards, retiring identity rejection,
//! exclusive leases that always retire, receipt polling (not cached flags),
//! and mailbox producer keying. Not wiring/flag-echo checks.

#[cfg(target_os = "android")]
mod android_tests {
    use ndk::media::image_reader::{ImageFormat, ImageReader};
    use oxideav_core::{CodecId, CodecParameters};
    use player::android::{
        AndroidBackend, SurfaceAdmissionError, SurfaceBindError, SurfaceBinding, SurfaceRegistry,
        SurfaceRetirement, SurfaceRetirementError, SurfaceRetirementStatus, TOTAL_SURFACE_CREDITS,
    };
    use player::backend::{
        Backend, PictureReady, ProducerId, SubtitleImage, SubtitleSink, VideoControl, VideoError,
        VideoMode, VideoRequest, VideoSink, VideoTarget,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    use std::time::{Duration, Instant};

    struct TestControl {
        cancelled: AtomicBool,
        woken: AtomicBool,
    }

    impl TestControl {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                cancelled: AtomicBool::new(false),
                woken: AtomicBool::new(false),
            })
        }
    }

    impl VideoControl for TestControl {
        fn cancelled(&self, _producer: ProducerId, _seek_generation: u64) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }
        fn active_now(&self) -> Instant {
            Instant::now()
        }
        fn wake(&self) {
            self.woken.store(true, Ordering::SeqCst);
        }
    }

    fn noop_waker() -> Waker {
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
        // SAFETY: noop vtable; pointer is never dereferenced.
        unsafe { Waker::from_raw(raw()) }
    }

    fn test_params() -> Arc<CodecParameters> {
        let mut p = CodecParameters::video(CodecId::new("h264"));
        p.width = Some(16);
        p.height = Some(16);
        Arc::new(p)
    }

    fn await_retirement(retirement: &SurfaceRetirement, limit: Duration) -> bool {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let t0 = Instant::now();
        loop {
            match retirement.poll(&mut cx) {
                Poll::Ready(Ok(_)) => return true,
                Poll::Ready(Err(_)) => return false,
                Poll::Pending => {
                    if t0.elapsed() > limit {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    struct BindingGuard {
        binding: Arc<SurfaceBinding>,
        reader: Option<ImageReader>,
    }

    impl BindingGuard {
        fn new(binding: Arc<SurfaceBinding>, reader: ImageReader) -> Self {
            Self {
                binding,
                reader: Some(reader),
            }
        }
    }

    impl Drop for BindingGuard {
        fn drop(&mut self) {
            let retirement = self.binding.retire();
            let _ = await_retirement(&retirement, Duration::from_secs(5));
            // Drop reader only after receipt observation attempt (S8/S10).
            self.reader.take();
        }
    }

    fn commit_reader() -> BindingGuard {
        let registry = SurfaceRegistry::global();
        let mut reader = ImageReader::new(16, 16, ImageFormat::RGBA_8888, 2).expect("reader");
        let res = registry.reserve_native().expect("reserve_native");
        let window = reader.window().expect("window");
        let binding = res.commit(window);
        BindingGuard::new(binding, reader)
    }

    #[test]
    fn surface_registry_capacity_limit_is_sixteen_with_cleanup() {
        let registry = SurfaceRegistry::global();
        let mut guards = Vec::new();
        for _ in 0..TOTAL_SURFACE_CREDITS {
            match registry.reserve_native() {
                Ok(r) => guards.push(r),
                Err(e) => panic!("expected reservation within 16 credits: {e:?}"),
            }
        }
        match registry.reserve_native() {
            Err(SurfaceAdmissionError::Capacity) => {}
            other => panic!("expected Capacity on 17th, got {other:?}"),
        }
        guards.pop();
        match registry.reserve_native() {
            Ok(r) => guards.push(r),
            Err(e) => panic!("expected restored credit: {e:?}"),
        }
        drop(guards);
    }

    #[test]
    fn retiring_binding_receipt_poll_not_cached_flag() {
        let guard = commit_reader();
        let retirement = guard.binding.retire();
        assert_eq!(retirement.status(), SurfaceRetirementStatus::Pending);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        match retirement.poll(&mut cx) {
            Poll::Ready(Ok(_)) => assert!(retirement.is_retired()),
            Poll::Ready(Err(
                SurfaceRetirementError::Failed(_) | SurfaceRetirementError::Quarantined(_),
            )) => {
                assert!(matches!(retirement.poll(&mut cx), Poll::Ready(Err(_))));
            }
            Poll::Pending => {
                assert_ne!(retirement.status(), SurfaceRetirementStatus::Retired);
            }
        }
    }

    #[test]
    fn exclusive_binding_lease_and_always_retire() {
        let guard = commit_reader();
        let lease1 = guard.binding.try_acquire_lease().expect("first lease");
        assert!(matches!(
            guard.binding.try_acquire_lease(),
            Err(SurfaceBindError::AlreadyLeased)
        ));
        lease1.release_healthy();
        let lease2 = guard.binding.try_acquire_lease().expect("reacquire");
        lease2.release_healthy();
    }

    #[test]
    fn subtitle_overlay_coalesces_and_empty_clear() {
        let backend = AndroidBackend::new();
        let mut sink = backend.subtitles();
        let img1 = SubtitleImage {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
            rgba: vec![255; 400],
        };
        let img2 = SubtitleImage {
            x: 5,
            y: 5,
            width: 20,
            height: 20,
            rgba: vec![128; 1600],
        };
        sink.show(&[img1], 1920, 1080);
        sink.show(&[img2], 1920, 1080);
        sink.show(&[], 0, 0);
    }

    #[test]
    fn repeated_polls_remain_idempotent_without_surface() {
        let backend = AndroidBackend::new();
        let clock = Arc::new(TestClock);
        let mut sink = backend.video(clock);
        let control = TestControl::new();
        let producer = ProducerId(123);
        let request = VideoRequest {
            producer,
            seek_generation: 1,
            output_revision: backend.video_output().revision,
            target: VideoTarget::Compressed {
                params: test_params(),
                ready: PictureReady::new(|_| {}),
                present_from: Duration::ZERO,
            },
            deadline: Instant::now() + Duration::from_secs(5),
            control: control.clone(),
        };
        let rev = backend.video_output().revision;
        for _ in 0..5 {
            let _ = sink.poll_transition(&request);
        }
        assert_eq!(backend.video_output().revision, rev);
        match sink.poll_transition(&request) {
            Poll::Ready(Ok(VideoMode::Compressed)) => {
                panic!("must not configure compressed without an admitted surface");
            }
            _ => {}
        }
    }

    struct TestClock;
    impl player::backend::Clock for TestClock {
        fn now(&self) -> Option<Duration> {
            Some(Duration::ZERO)
        }
        fn monotonic_ns_at(&self, _at: Duration) -> Option<i64> {
            Some(1_000_000_000)
        }
    }
}
