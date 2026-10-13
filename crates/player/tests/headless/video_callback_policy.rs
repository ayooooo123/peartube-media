//! Failure modes: recovery or fatal sink callbacks inherit Mach realtime policy;
//! moving restoration too early instead removes the policy from frame admission.
use super::*;
use player::backend::*;
use std::{sync::mpsc, task::Poll};

#[allow(deprecated)]
fn policy() -> (i32, i32) {
    unsafe extern "C" {
        fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> libc::kern_return_t;
    }
    let mut value = libc::thread_time_constraint_policy { period: 0, computation: 0, constraint: 0, preemptible: 0 };
    let mut count = libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT;
    let mut default = 0;
    let result = unsafe {
        let thread = libc::mach_thread_self();
        let result = libc::thread_policy_get(thread, libc::THREAD_TIME_CONSTRAINT_POLICY as u32,
            std::ptr::from_mut(&mut value).cast(), &mut count, &mut default);
        mach_port_deallocate(libc::mach_task_self(), thread);
        result
    };
    (result, default)
}

fn observe(tx: mpsc::Sender<(Event, (i32, i32))>) -> impl Fn(Event) + Send + Sync + 'static {
    move |event| {
        if std::thread::current().name() == Some("peartube-video") {
            let _ = tx.send((event, policy()));
        }
    }
}

fn ordinary_callbacks(events: &[(Event, (i32, i32))]) {
    assert!(!events.is_empty(), "video worker never notified the application");
    for (_, (result, default)) in events {
        assert_eq!(*result, libc::KERN_SUCCESS);
        assert_ne!(*default, 0, "application callback inherited realtime policy");
    }
}

#[test]
fn recovery_callbacks_restore_video_policy() {
    let _cpu = realtime_test();
    let bytes = ffmpeg_file("mkv", &[
        "-f", "lavfi", "-i", "testsrc2=size=160x96:rate=25:duration=6",
        "-f", "lavfi", "-i", "sine=sample_rate=48000:duration=6",
        "-c:v", "mpeg2video", "-g", "25", "-bf", "0", "-c:a", "flac",
    ]);
    let path = tempfile("mkv");
    std::fs::write(&path, bytes).unwrap();
    let mut context = oxideav_core::RuntimeContext::new();
    context.codecs.register(oxideav_core::CodecInfo::new(oxideav_core::CodecId::new("mpeg2video"))
        .capabilities(oxideav_core::CodecCapabilities::video("slow_software"))
        .decoder(SlowSoftwareVideo::open));
    codecs::register_all(&mut context);
    let (tx, rx) = mpsc::channel();
    let backend = Headless::new();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(context), PlayerOptions::default(), observe(tx));
    let (_, held) = sample_until(&player, Duration::from_secs(12), |s| finished(s) || (s.buffering && s.position > Duration::from_millis(200)));
    assert!(held.buffering && !finished(&held), "no midstream recovery: {held:?}");
    let (_, state) = sample_until(&player, Duration::from_secs(15), finished);
    drop(player);
    std::fs::remove_file(path).unwrap();
    assert!(state.ended && state.error.is_none(), "{state:?}");
    assert_eq!(backend.capture().video[0].pts.last(), Some(&Duration::from_millis(5960)));
    ordinary_callbacks(&rx.into_iter().collect::<Vec<_>>());
}

struct FailingOutput { inner: Arc<Headless>, admission: mpsc::Sender<(i32, i32)> }
struct FailingVideo { inner: Box<dyn VideoSink>, admission: mpsc::Sender<(i32, i32)> }
impl Backend for FailingOutput {
    fn audio(&self) -> Box<dyn AudioSink> { self.inner.audio() }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        Box::new(FailingVideo { inner: self.inner.video(clock), admission: self.admission.clone() })
    }
    fn subtitles(&self) -> Box<dyn SubtitleSink> { self.inner.subtitles() }
}
impl VideoSink for FailingVideo {
    fn output(&self) -> VideoOutput { self.inner.output() }
    fn poll_transition(&mut self, request: &VideoRequest) -> Poll<Result<VideoMode, VideoError>> { self.inner.poll_transition(request) }
    fn push_packet(&mut self, p: ProducerId, packet: &mut Option<oxideav_core::Packet>, pts: Duration, rap: bool) -> Result<(), VideoError> { self.inner.push_packet(p, packet, pts, rap) }
    fn push_frame(&mut self, p: ProducerId, frame: &mut Option<oxideav_core::VideoFrame>, pts: Duration) -> Result<(), VideoError> {
        if pts >= Duration::from_millis(80) {
            self.admission.send(policy()).unwrap();
            return Err(SinkError::Fatal("injected presentation failure".into()).into());
        }
        self.inner.push_frame(p, frame, pts)
    }
    fn present_from(&mut self, p: ProducerId, pts: Duration) -> Result<(), VideoError> { self.inner.present_from(p, pts) }
    fn set_playing(&mut self, p: ProducerId, playing: bool) -> Result<(), VideoError> { self.inner.set_playing(p, playing) }
    fn frame_lead(&self) -> Duration { self.inner.frame_lead() }
    fn poll_finish(&mut self, p: ProducerId) -> Poll<Result<(), VideoError>> { self.inner.poll_finish(p) }
}

#[test]
fn fatal_video_callback_restores_policy_after_guarded_admission() {
    let _cpu = realtime_test();
    let path = tempfile("mkv");
    std::fs::write(&path, make_ref_mkv()).unwrap();
    let (admission, admitted) = mpsc::channel();
    let backend = Arc::new(FailingOutput { inner: Headless::new(), admission });
    let (tx, rx) = mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), backend, test_context(), PlayerOptions::default(), observe(tx));
    let (_, state) = sample_until(&player, Duration::from_secs(10), finished);
    drop(player);
    std::fs::remove_file(path).unwrap();
    assert!(state.error.is_some(), "sink failure not reported");
    assert_eq!(admitted.recv_timeout(Duration::from_secs(1)).unwrap(), (libc::KERN_SUCCESS, 0), "frame admission lost realtime scheduling");
    let events: Vec<_> = rx.into_iter().collect();
    assert!(events.iter().any(|(event, _)| matches!(event, Event::Error(_))), "no error callback");
    ordinary_callbacks(&events);
}
