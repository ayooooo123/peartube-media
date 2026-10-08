//! Actual Player/AppleBackend timing smoke; no screen-capture permission.
//! `sh crates/player/examples/apple_play.sh clip.mkv [--software] [--transport]`
//! Requires the flash/beep clip with the 8-bit frame identifier stripe.
//! Live renderer counters are diagnostics, not presentation proof. Once per
//! second pause and read the actual displayed pixel buffer (AVFoundation
//! disallows that read while running), decode its frame number, and compare
//! its PTS with the audio timebase. These are sampled offsets, not a
//! continuous per-frame presentation-time distribution.

use std::sync::Arc;
use parking_lot::Mutex;
use std::time::{Duration, Instant};
use player::backend::{AudioSink, Backend, Clock, SinkError, SubtitleSink, VideoSink};
use player::{AppleBackend, Player, PlayerOptions};
use oxideav_core::{CodecParameters, Packet, VideoFrame};
use objc2_core_foundation::{CFRetained, CFType};
use objc2_core_video::CVPixelBuffer;

struct Measured {
    native: Arc<AppleBackend>,
    audio_clock: Mutex<Option<Arc<dyn Clock>>>,
    software: bool,
}
impl Backend for Measured {
    fn audio(&self) -> Box<dyn AudioSink> {
        let sink = self.native.audio();
        *self.audio_clock.lock() = Some(sink.clock());
        sink
    }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        let sink = self.native.video(clock);
        Box::new(MeasuredVideo(sink, self.software))
    }
    fn subtitles(&self) -> Box<dyn SubtitleSink> { self.native.subtitles() }
}
struct MeasuredVideo(Box<dyn VideoSink>, bool);
impl VideoSink for MeasuredVideo {
    fn open_compressed(&mut self, p: &CodecParameters, ready: player::backend::PictureReady) -> bool {
        let accepted = !self.1 && self.0.open_compressed(p, ready);
        eprintln!("APPLE_VIDEO compressed={accepted}");
        accepted
    }
    fn present_from(&mut self, start: Duration) { self.0.present_from(start); }
    fn push_packet(&mut self, p: &Packet, pts: Duration, random_access: bool) -> Result<(), SinkError> {
        self.0.push_packet(p, pts, random_access)
    }
    fn open_frames(&mut self, p: &CodecParameters) -> Result<(), SinkError> {
        eprintln!("APPLE_VIDEO software_frames");
        self.0.open_frames(p)
    }
    fn push_frame(&mut self, f: &VideoFrame, pts: Duration) -> Result<(), SinkError> { self.0.push_frame(f, pts) }
    fn frame_lead(&self) -> Duration { self.0.frame_lead() }
    fn finish(&mut self) -> Result<(), SinkError> { self.0.finish() }
    fn flush(&mut self) { self.0.flush(); }
    fn set_playing(&mut self, p: bool) { self.0.set_playing(p); }
}

fn main() {
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSPanel, NSWindowStyleMask};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    let path = std::env::args().nth(1).expect("apple_play clip.mkv [--software] [--transport]");
    let software = std::env::args().any(|a| a == "--software");
    let transport = std::env::args().any(|a| a == "--transport");
    let audio_tail = std::env::args().any(|a| a == "--audio-tail-seek");
    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.finishLaunching();
    let frame = CGRect { origin: CGPoint { x: 40.0, y: 40.0 }, size: CGSize { width: 320.0, height: 192.0 } };
    let window = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm), frame, NSWindowStyleMask::Titled | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered, false);
    window.setTitle(&objc2_foundation::NSString::from_str("EngineSync timing"));
    // An occluded layer can retain a stale readback. Keep this short
    // display probe visible while other applications have focus.
    window.setFloatingPanel(true);
    window.setHidesOnDeactivate(false);
    window.setCollectionBehavior(objc2_app_kit::NSWindowCollectionBehavior::CanJoinAllSpaces
        | objc2_app_kit::NSWindowCollectionBehavior::FullScreenAuxiliary
        | objc2_app_kit::NSWindowCollectionBehavior::CanJoinAllApplications);
    let view = window.contentView().unwrap();
    let native = AppleBackend::new();
    native.set_muted(true);
    unsafe { native.attach(std::ptr::from_ref(&*view).cast_mut().cast()); }
    native.set_frame([0.0, 0.0, 320.0, 192.0]);
    native.video_layer().setFrame(CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: frame.size });
    let (surface_tx, surface_rx) = std::sync::mpsc::sync_channel(1);
    let surface_changed = block2::RcBlock::new(move |_: std::ptr::NonNull<objc2_foundation::NSNotification>| {
        let mtm = MainThreadMarker::new().expect("window notification off main thread");
        let app = NSApplication::sharedApplication(mtm);
        for window in app.windows().iter() {
            let visible = window.occlusionState().contains(objc2_app_kit::NSWindowOcclusionState::Visible);
            eprintln!("APPLE_SURFACE active={} visible={} occlusion={:?} on_active_space={} screen={}",
                app.isActive(), window.isVisible(), window.occlusionState(),
                window.isOnActiveSpace(), window.screen().is_some());
            if visible { let _ = surface_tx.try_send(()); }
        }
    });
    // The callback captures only a Send channel; this window posts on main.
    let _surface_observer = unsafe {
        objc2_foundation::NSNotificationCenter::defaultCenter()
            .addObserverForName_object_queue_usingBlock(
                Some(objc2_app_kit::NSWindowDidChangeOcclusionStateNotification),
                Some(&window), None, &surface_changed)
    };
    dispatch2::DispatchQueue::main().exec_async(|| {
        let mtm = MainThreadMarker::new().unwrap();
        let app = NSApplication::sharedApplication(mtm);
        for window in app.windows().iter() {
            window.orderFrontRegardless();
        }
    });
    let backend = Arc::new(Measured { native, audio_clock: Mutex::new(None), software });
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            surface_rx.recv_timeout(Duration::from_secs(5))
                .expect("no visible AppKit surface; launch with apple_play.sh in a graphical session");
            if audio_tail { measure_audio_tail(path, backend) }
            else { measure(path, backend, transport) }
        }));
        if let Err(e) = &result { eprintln!("Apple timing smoke failed: {e:?}"); }
        dispatch2::run_on_main(|_| READBACK.with(|slot| { slot.borrow_mut().take(); }));
        let code = if result.is_ok() { 0 } else { 1 };
        // LaunchServices' `open -W` status is not the application's exit code.
        if let Some(path) = std::env::var_os("APPLE_PLAY_EXIT_STATUS") {
            if let Err(error) = std::fs::write(path, code.to_string()) {
                eprintln!("Cannot write native probe exit status: {error}");
                std::process::exit(1);
            }
        }
        std::process::exit(code);
    });
    app.run();
}

fn measure(path: String, backend: Arc<Measured>, transport: bool) {
    let player = Player::open(&path, backend.clone(), Arc::new(codecs::context()), PlayerOptions::default(), |_| {});
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut previous = (0isize, 0isize, 0.0f64);
    let mut offsets = Vec::new();
    let mut next_sample = Duration::from_millis(1008);
    let mut swapped = false;
    println!("mono_ns,audio_s,engine_s,frames,dropped,delay_sum_ms,delta_frames,delta_delay_ms");
    loop {
        // Bracket the engine read so a descheduled probe cannot count
        // elapsed wall time between two snapshots as clock skew.
        let audio_before = backend.audio_clock.lock().as_ref().and_then(|c| c.now()).unwrap_or_default();
        let state = player.state();
        assert!(state.error.is_none(), "{state:?}");
        assert!(Instant::now() < deadline, "timing smoke timed out: {state:?}");
        let audio = backend.audio_clock.lock().as_ref().and_then(|c| c.now()).unwrap_or_default();
        if state.playing && !state.buffering && state.audio.is_some()
            && audio > Duration::from_secs(1) && state.duration.is_some_and(|end| state.position < end) {
            let margin = Duration::from_millis(40);
            assert!(state.position >= audio_before.saturating_sub(margin) && state.position <= audio + margin,
                "engine not on audio: {audio_before:?}..{audio:?}, {state:?}");
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let b = backend.clone();
        dispatch2::run_on_main(move |_| unsafe {
            let renderer = b.native.video_layer().sampleBufferRenderer();
            let block = block2::RcBlock::new(move |metrics: *mut objc2_av_foundation::AVVideoPerformanceMetrics| {
                let snapshot = metrics.as_ref().map(|m| (m.totalNumberOfFrames(), m.numberOfDroppedFrames(), m.totalAccumulatedFrameDelay()));
                let _ = tx.send(snapshot);
            });
            renderer.loadVideoPerformanceMetricsWithCompletionHandler(&block);
        });
        if let Some(metrics) = rx.recv_timeout(Duration::from_secs(2)).expect("renderer metrics timeout") {
            if metrics.0 != previous.0 || metrics.1 != previous.1 {
                let count = metrics.0 - previous.0;
                let delay = (metrics.2 - previous.2) * 1000.0;
                println!("{},{:.9},{:.9},{},{},{:.6},{},{:.6}", player::clock::current_monotonic_ns(), audio.as_secs_f64(), state.position.as_secs_f64(), metrics.0, metrics.1, metrics.2 * 1000.0, count, delay);
                // The renderer may count queued frames and report no delay
                // data. These counters do not certify synchronization.
                previous = metrics;
            }
        }
        // There is no audio sample to compare after EOS. In particular,
        // the free-running tail clock must not schedule an 8.008 s sample
        // for the 8 s fixture after the native audio timebase has stopped.
        if !state.ended && !state.buffering && state.position >= next_sample
            && state.duration.is_none_or(|end| next_sample < end) {
            player.pause();
            let frozen = Instant::now() + Duration::from_secs(1);
            loop {
                let clock = backend.audio_clock.lock().clone().expect("audio clock");
                if clock.monotonic_ns_at(state.position).is_none() { break; }
                assert!(Instant::now() < frozen, "audio did not pause");
                std::thread::sleep(Duration::from_millis(1));
            }
            // Allow the renderer to finish its in-flight presentation at
            // the now-stationary timebase, then inspect its actual pixels.
            std::thread::sleep(Duration::from_millis(50));
            let audio_at = backend.audio_clock.lock().as_ref().and_then(|c| c.now()).unwrap();
            let frame = displayed_frame(backend.clone()).expect("renderer returned no readable displayed frame");
            let offset = frame as f64 * 40.0 - audio_at.as_secs_f64() * 1000.0;
            eprintln!("APPLE_FRAME audio_s={:.9} video_frame={frame} offset_ms={offset:.6}", audio_at.as_secs_f64());
            offsets.push(offset);
            next_sample = Duration::from_secs(audio_at.as_secs() + 1) + Duration::from_millis(8);
            player.play();
        }
        if transport && !swapped && state.position > Duration::from_millis(2200) {
            player.pause();
            let held = player.state().position;
            std::thread::sleep(Duration::from_millis(250));
            assert_eq!(player.state().position, held);
            player.seek(Duration::from_millis(4100));
            player.play();
            swapped = true;
        }
        if state.ended { break; }
        std::thread::sleep(Duration::from_millis(5));
    }
    // The decoder's reordered tail must reach the screen at end of stream.
    let duration = player.state().duration.expect("duration");
    std::thread::sleep(Duration::from_millis(200));
    let last = ((duration.as_millis() / 40) as u32 - 1) % 256;
    let shown = displayed_frame(backend.clone());
    eprintln!("APPLE_EOS expected_frame={last} displayed={shown:?}");
    assert_eq!(shown, Some(last), "final reordered frame not displayed at end of stream");
    assert!(offsets.len() >= 4, "insufficient renderer readback samples: {offsets:?}");
    eprintln!("Apple sampled offsets_ms={offsets:?} final_metrics={previous:?} transport={swapped}");
    assert!(offsets.iter().all(|v| v.abs() <= 40.0), "displayed video/audio offset exceeds 40 ms: {offsets:?}");
    drop(player);
}

/// The fixture has eight seconds of barcode video and only three of audio.
/// This checks seek recovery, not continuous A/V presentation timing.
fn measure_audio_tail(path: String, backend: Arc<Measured>) {
    let player = Player::open(&path, backend.clone(), Arc::new(codecs::context()), PlayerOptions::default(), |_| {});
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let state = player.state();
        assert!(state.error.is_none(), "{state:?}");
        assert!(Instant::now() < deadline, "initial playback stalled: {state:?}");
        if !state.buffering && state.position >= Duration::from_secs(1) { break; }
        std::thread::sleep(Duration::from_millis(5));
    }
    player.seek(Duration::from_secs(6));
    loop {
        let state = player.state();
        assert!(state.error.is_none(), "{state:?}");
        assert!(Instant::now() < deadline, "audio-less seek stalled: {state:?}");
        assert!(!state.ended, "ended before the tail observation: {state:?}");
        if !state.buffering && state.position >= Duration::from_secs(7) { break; }
        std::thread::sleep(Duration::from_millis(5));
    }
    let native = backend.native.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    dispatch2::run_on_main(move |_| unsafe {
        use objc2_av_foundation::AVQueuedSampleBufferRendering;
        let renderer = native.video_layer().sampleBufferRenderer();
        eprintln!("APPLE_TAIL_RUNNING rate={} time={:?}", renderer.timebase().rate(), renderer.timebase().time());
        let block = block2::RcBlock::new(move |metrics: *mut objc2_av_foundation::AVVideoPerformanceMetrics| {
            let value = metrics.as_ref().map(|m| (m.totalNumberOfFrames(), m.numberOfDroppedFrames(), m.totalAccumulatedFrameDelay()));
            let _ = tx.send(value);
        });
        renderer.loadVideoPerformanceMetricsWithCompletionHandler(&block);
    });
    eprintln!("APPLE_TAIL_METRICS {:?} state={:?}", rx.recv_timeout(Duration::from_secs(2)).unwrap(), player.state());
    player.pause();
    let frozen = Instant::now() + Duration::from_secs(1);
    loop {
        let native = backend.native.clone();
        let stopped = dispatch2::run_on_main(move |_| unsafe {
            use objc2_av_foundation::AVQueuedSampleBufferRendering;
            native.video_layer().sampleBufferRenderer().timebase().rate() == 0.0
        });
        if stopped { break; }
        assert!(Instant::now() < frozen, "tail renderer did not pause");
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(50));
    let position = player.state().position;
    let frame = displayed_frame(backend).expect("no displayed video after seeking beyond audio");
    let offset_ms = f64::from(frame) * 40.0 - position.as_secs_f64() * 1000.0;
    eprintln!("APPLE_AUDIO_TAIL engine_s={:.9} frame={frame} offset_ms={offset_ms:.6}", position.as_secs_f64());
    assert!(offset_ms.abs() <= 40.0, "stale video timebase after audio-less seek: {offset_ms} ms");
    drop(player);
}

fn displayed_frame(backend: Arc<Measured>) -> Option<u32> {
    dispatch2::run_on_main(move |mtm| unsafe {
        use objc2_core_video::*;
        use objc2_av_foundation::AVQueuedSampleBufferRendering;
        let renderer = backend.native.video_layer().sampleBufferRenderer();
        let timebase = renderer.timebase();
        eprintln!("APPLE_READBACK ready={} status={:?} renderer_rate={} renderer_time={:?}",
            backend.native.video_layer().isReadyForDisplay(), renderer.status(), timebase.rate(), timebase.time());
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        for window in app.windows().iter() {
            eprintln!("APPLE_WINDOW active={} visible={} occlusion={:?} layer_frame={:?}",
                app.isActive(), window.isVisible(), window.occlusionState(), backend.native.video_layer().frame());
        }
        let pixel: Option<objc2::rc::Retained<CVPixelBuffer>> =
            objc2::msg_send![&*renderer, copyDisplayedPixelBuffer];
        let Some(pixel) = pixel else {
            return None;
        };
        let format = CVPixelBufferGetPixelFormatType(&pixel);
        eprintln!("APPLE_PIXEL format={format:#010x} width={} height={} planes={}",
            CVPixelBufferGetWidth(&pixel), CVPixelBufferGetHeight(&pixel), CVPixelBufferGetPlaneCount(&pixel));
        // Native decoders may return Apple's hardware-compressed '&8v0'.
        // Never interpret those blocks as luma bytes. Transfer the actual
        // displayed buffer to linear NV12, reusing one session and buffer.
        let converted = if format != kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
            && format != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            && format != kCVPixelFormatType_32BGRA {
            Some(READBACK.with(|slot| {
                let mut slot = slot.borrow_mut();
                let size = (CVPixelBufferGetWidth(&pixel), CVPixelBufferGetHeight(&pixel));
                if slot.as_ref().is_none_or(|reader| reader.size != size) {
                    *slot = Some(PixelReadback::new(size)?);
                }
                slot.as_ref()?.copy(&pixel)
            })?)
        } else { None };
        let pixel = converted.as_deref().unwrap_or(&pixel);
        let format = CVPixelBufferGetPixelFormatType(pixel);
        let ten_bit = format == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange;
        if !ten_bit && format != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            && format != kCVPixelFormatType_32BGRA {
            eprintln!("APPLE_READBACK unsupported_pixel_format={format:#010x}");
            return None;
        }
        let flags = CVPixelBufferLockFlags::ReadOnly;
        let status = CVPixelBufferLockBaseAddress(&pixel, flags);
        if status != 0 {
            eprintln!("APPLE_READBACK lock_failed={status} format={}", CVPixelBufferGetPixelFormatType(&pixel));
            return None;
        }
        let planar = CVPixelBufferGetPlaneCount(&pixel) > 0;
        let (base, stride) = if planar {
            (CVPixelBufferGetBaseAddressOfPlane(&pixel, 0), CVPixelBufferGetBytesPerRowOfPlane(&pixel, 0))
        } else {
            (CVPixelBufferGetBaseAddress(&pixel), CVPixelBufferGetBytesPerRow(&pixel))
        };
        let mut frame = 0;
        if !base.is_null() && CVPixelBufferGetWidth(&pixel) >= 128 && CVPixelBufferGetHeight(&pixel) >= 8 {
            for bit in 0..8 {
                let x = 8 + bit * 16;
                let address = base.cast::<u8>().add(4 * stride + x * if ten_bit { 2 } else if planar { 1 } else { 4 });
                let bright = if ten_bit {
                    address.cast::<u16>().read_unaligned() > 32768
                } else {
                    *address > 128
                };
                if bright { frame |= 1 << bit; }
            }
        }
        CVPixelBufferUnlockBaseAddress(&pixel, flags);
        (!base.is_null()).then_some(frame)
    })
}

// VideoToolbox's documented C API; no production dependency or source-frame
// access. All session use and destruction stay on AppKit's main thread.
#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTPixelTransferSessionCreate(allocator: *const std::ffi::c_void, out: *mut *mut CFType) -> i32;
    fn VTPixelTransferSessionTransferImage(session: *const CFType, source: *const CVPixelBuffer, destination: *const CVPixelBuffer) -> i32;
    fn VTPixelTransferSessionInvalidate(session: *const CFType);
}

thread_local! {
    static READBACK: std::cell::RefCell<Option<PixelReadback>> = const { std::cell::RefCell::new(None) };
}

struct PixelReadback {
    session: CFRetained<CFType>,
    pixel: CFRetained<CVPixelBuffer>,
    size: (usize, usize),
}

impl PixelReadback {
    fn new(size: (usize, usize)) -> Option<Self> {
        unsafe {
            let mut raw = std::ptr::null_mut();
            let status = VTPixelTransferSessionCreate(std::ptr::null(), &mut raw);
            if status != 0 { eprintln!("APPLE_READBACK transfer_create_failed={status}"); return None; }
            let session = CFRetained::from_raw(std::ptr::NonNull::new(raw)?);
            let mut raw = std::ptr::null_mut();
            let status = objc2_core_video::CVPixelBufferCreate(
                None, size.0, size.1, objc2_core_video::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                None, std::ptr::NonNull::from(&mut raw),
            );
            if status != 0 { eprintln!("APPLE_READBACK pixel_create_failed={status}"); return None; }
            let pixel = CFRetained::from_raw(std::ptr::NonNull::new(raw)?);
            Some(Self { session, pixel, size })
        }
    }

    fn copy(&self, source: &CVPixelBuffer) -> Option<CFRetained<CVPixelBuffer>> {
        let status = unsafe { VTPixelTransferSessionTransferImage(&*self.session, source, &*self.pixel) };
        if status != 0 { eprintln!("APPLE_READBACK transfer_failed={status}"); return None; }
        Some(self.pixel.clone())
    }
}

impl Drop for PixelReadback {
    fn drop(&mut self) {
        unsafe { VTPixelTransferSessionInvalidate(&*self.session); }
    }
}
