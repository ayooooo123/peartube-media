//! Runs the real Player with AAudio and a draining AImageReader surface.
//! PEARTUBE_SYNC_TRACE=1 android_sync clip.mkv [--software] [--transport]
//! The trace reports AAudio timestamp pairs and MediaCodec release targets.
//! Read back the frame identifier stripe from the flash/beep fixture at the
//! ImageReader consumer; callback times are not physical-display scanout.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::time::{Duration, Instant};
use player::backend::{
    AudioSink, Backend, Clock, ProducerId, SubtitleSink, VideoError, VideoMode, VideoOutput,
    VideoRequest, VideoSink, VideoTarget,
};
use player::{AndroidBackend, Player, PlayerOptions};
use oxideav_core::{Packet, VideoFrame};

struct Probe { native: Arc<AndroidBackend>, software: bool }
impl Backend for Probe {
    fn audio(&self) -> Box<dyn AudioSink> { self.native.audio() }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        let sink = self.native.video(clock);
        if let Some(s) = self.native.shared().active_video.lock().as_ref().and_then(|s| s.upgrade()) {
            s.lock().prefer_software_decoder(true);
        }
        if self.software { Box::new(Software(sink)) } else { sink }
    }
    fn subtitles(&self) -> Box<dyn SubtitleSink> { self.native.subtitles() }
}

struct Software(Box<dyn VideoSink>);
impl VideoSink for Software {
    fn output(&self) -> VideoOutput {
        self.0.output()
    }
    fn poll_transition(&mut self, request: &VideoRequest) -> Poll<Result<VideoMode, VideoError>> {
        if matches!(request.target, VideoTarget::Compressed { .. }) {
            return Poll::Ready(Err(VideoError::Unsupported));
        }
        self.0.poll_transition(request)
    }
    fn push_packet(&mut self, producer: ProducerId, packet: &mut Option<Packet>, pts: Duration, random_access: bool) -> Result<(), VideoError> {
        self.0.push_packet(producer, packet, pts, random_access)
    }
    fn push_frame(&mut self, producer: ProducerId, frame: &mut Option<VideoFrame>, pts: Duration) -> Result<(), VideoError> {
        self.0.push_frame(producer, frame, pts)
    }
    fn present_from(&mut self, producer: ProducerId, start: Duration) -> Result<(), VideoError> {
        self.0.present_from(producer, start)
    }
    fn set_playing(&mut self, producer: ProducerId, playing: bool) -> Result<(), VideoError> {
        self.0.set_playing(producer, playing)
    }
    fn frame_lead(&self) -> Duration {
        self.0.frame_lead()
    }
    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>> {
        self.0.poll_finish(producer)
    }
}

fn main() {
    use ndk::media::image_reader::{AcquireResult, ImageFormat, ImageReader};
    use ndk::hardware_buffer::HardwareBufferUsage;
    let file = std::env::args().nth(1).expect("android_sync clip.mkv [--software] [--transport]");
    let software = std::env::args().any(|a| a == "--software");
    let transport = std::env::args().any(|a| a == "--transport");
    let mut reader = if software {
        ImageReader::new(160, 96, ImageFormat::RGBA_8888, 8)
    } else {
        ImageReader::new_with_usage(160, 96, ImageFormat::YUV_420_888,
            HardwareBufferUsage::GPU_COLOR_OUTPUT | HardwareBufferUsage::GPU_SAMPLED_IMAGE
                | HardwareBufferUsage::VIDEO_ENCODE | HardwareBufferUsage::CPU_READ_OFTEN, 8)
    }.expect("ImageReader");
    let frames = Arc::new(AtomicUsize::new(0));
    let count = frames.clone();
    reader.set_image_listener(Box::new(move |r| {
        while let Ok(AcquireResult::Image(image)) = r.acquire_next_image() {
            let received = player::clock::current_monotonic_ns();
            let pixels = image.plane_data(0).expect("readable surface pixels");
            let row = image.plane_row_stride(0).unwrap() as usize;
            let pixel = image.plane_pixel_stride(0).unwrap() as usize;
            let mut frame = 0u32;
            for bit in 0..8 {
                if pixels[4 * row + (8 + bit * 16) * pixel] > 128 { frame |= 1 << bit; }
            }
            count.fetch_add(1, Ordering::Relaxed);
            eprintln!("ENGINE_SYNC surface mono_ns={received} video_frame={frame}");
            drop(image);
        }
    })).unwrap();

    let reservation = player::android::SurfaceRegistry::global().reserve_native().expect("reserve_native");
    let window = reader.window().expect("reader window");
    let binding = reservation.commit(window);

    let native = AndroidBackend::new();
    native.set_video_surface(binding.clone()).expect("set_video_surface");
    let backend = Arc::new(Probe { native, software });
    let player = Player::open(&file, backend, Arc::new(codecs::context()), PlayerOptions::default(), |_| {});
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut swapped = false;
    loop {
        let state = player.state();
        assert!(state.error.is_none(), "{state:?}");
        assert!(Instant::now() < deadline, "timeout: {state:?}");
        if transport && !swapped && state.position >= Duration::from_millis(2200) {
            player.pause();
            let held = player.state().position;
            std::thread::sleep(Duration::from_millis(250));
            assert_eq!(player.state().position, held);
            player.seek(Duration::from_millis(4100));
            player.play();
            swapped = true;
        }
        if state.ended { eprintln!("ENGINE_SYNC ended {state:?}"); break; }
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(player);

    let retirement = binding.retire();
    {
        fn noop_waker() -> Waker {
            fn clone_raw(_: *const ()) -> RawWaker { RawWaker::new(std::ptr::null(), &VTABLE) }
            fn wake_raw(_: *const ()) {}
            fn wake_by_ref_raw(_: *const ()) {}
            fn drop_raw(_: *const ()) {}
            static VTABLE: RawWakerVTable =
                RawWakerVTable::new(clone_raw, wake_raw, wake_by_ref_raw, drop_raw);
            unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
        }
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let t0 = Instant::now();
        let mut ok = false;
        while t0.elapsed() < Duration::from_secs(10) {
            match retirement.poll(&mut cx) {
                Poll::Ready(Ok(_)) => { ok = true; break; }
                Poll::Ready(Err(e)) => panic!("retirement failed: {e:?}"),
                Poll::Pending => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        assert!(ok, "retirement timed out while still Pending");
    }
    drop(reader);

    let received = frames.load(Ordering::Relaxed);
    assert!(received >= 25, "only {received} surface frames");
    println!("Android timing completed: surface_frames={received} software={software} transport={swapped}");
}
