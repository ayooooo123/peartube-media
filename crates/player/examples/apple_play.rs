//! Actual Player/AppleBackend timing smoke; no screen-capture permission.
//! `sh crates/player/examples/apple_play.sh clip.mkv [--software] [--transport]`
//! Requires the flash/beep clip with the 8-bit frame identifier stripe.
//! Once per second pause and read the actual displayed pixel buffer
//! (AVFoundation disallows that read while running), decode its frame
//! number, and compare its PTS with the audio timebase. Loss of the probe
//! window's visibility invalidates the whole run, even if it later returns.
//! These are sampled offsets, not continuous presentation timing.

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
        let accepted = if self.1 { false } else {
            let report = player::backend::PictureReady::new(move |pts| {
                // Apple reports after enqueueing a decoded picture, not display.
                let mono_ns = player::clock::current_monotonic_ns();
                ready.ready(pts);
                eprintln!("APPLE_OUTPUT mono_ns={mono_ns} pts_s={:.9}", pts.as_secs_f64());
            });
            self.0.open_compressed(p, report)
        };
        eprintln!("APPLE_VIDEO compressed={accepted} mono_ns={}", player::clock::current_monotonic_ns());
        accepted
    }
    fn present_from(&mut self, start: Duration) {
        self.0.present_from(start);
        eprintln!("APPLE_PRESENT_FROM mono_ns={} pts_s={:.9}", player::clock::current_monotonic_ns(), start.as_secs_f64());
    }
    fn push_packet(&mut self, p: &Packet, pts: Duration, random_access: bool) -> Result<(), SinkError> {
        let entered_ns = player::clock::current_monotonic_ns();
        let result = self.0.push_packet(p, pts, random_access);
        if result.is_ok() {
            eprintln!("APPLE_PACKET entered_ns={entered_ns} accepted_ns={} pts_s={:.9} random_access={random_access}",
                player::clock::current_monotonic_ns(), pts.as_secs_f64());
        }
        result
    }
    fn open_frames(&mut self, p: &CodecParameters) -> Result<(), SinkError> {
        eprintln!("APPLE_VIDEO software_frames");
        self.0.open_frames(p)
    }
    fn push_frame(&mut self, f: &VideoFrame, pts: Duration) -> Result<(), SinkError> { self.0.push_frame(f, pts) }
    fn frame_lead(&self) -> Duration { self.0.frame_lead() }
    fn finish(&mut self) -> Result<(), SinkError> {
        let result = self.0.finish();
        if result.is_ok() {
            eprintln!("APPLE_VIDEO_DRAINED mono_ns={}", player::clock::current_monotonic_ns());
        }
        result
    }
    fn flush(&mut self) {
        self.0.flush();
        eprintln!("APPLE_VIDEO_FLUSH mono_ns={}", player::clock::current_monotonic_ns());
    }
    fn set_playing(&mut self, p: bool) {
        self.0.set_playing(p);
        eprintln!("APPLE_VIDEO_PLAYING mono_ns={} playing={p}", player::clock::current_monotonic_ns());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SurfaceSnapshot {
    mono_ns: i64,
    window: isize,
    level: objc2_app_kit::NSWindowLevel,
    active: bool,
    visible: bool,
    occlusion: objc2_app_kit::NSWindowOcclusionState,
    on_active_space: bool,
    screen: bool,
}

impl SurfaceSnapshot {
    fn available(self) -> bool {
        self.visible && self.screen && self.on_active_space
            && self.occlusion.contains(objc2_app_kit::NSWindowOcclusionState::Visible)
    }
}

#[derive(Default)]
struct SurfaceGuard {
    armed: bool,
    first_loss: Option<SurfaceSnapshot>,
}

impl SurfaceGuard {
    fn arm(&mut self, current: SurfaceSnapshot) -> Result<(), SurfaceSnapshot> {
        self.armed = true;
        self.observe(current)
    }

    fn observe(&mut self, current: SurfaceSnapshot) -> Result<(), SurfaceSnapshot> {
        if self.armed && !current.available() && self.first_loss.is_none() {
            self.first_loss = Some(current);
        }
        self.first_loss.map_or(Ok(()), Err)
    }
}

struct ProbeSurface {
    window: objc2::rc::Retained<objc2_app_kit::NSPanel>,
    guard: SurfaceGuard,
}

#[derive(Debug)]
struct SurfaceBlocked {
    phase: &'static str,
    first_loss: SurfaceSnapshot,
    current: SurfaceSnapshot,
}

thread_local! {
    static SURFACE: std::cell::RefCell<Option<ProbeSurface>> = const { std::cell::RefCell::new(None) };
}

// Always samples the actual hosting panel, not another window in NSApp.
// Only the worker may turn this result into an unwind; never a Cocoa callback.
fn observe_surface(mtm: objc2::MainThreadMarker, phase: &'static str, arm: bool)
    -> (SurfaceSnapshot, Result<(), SurfaceBlocked>)
{
    SURFACE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let surface = slot.as_mut().expect("probe surface");
        let window = &surface.window;
        let current = SurfaceSnapshot {
            mono_ns: player::clock::current_monotonic_ns(),
            window: window.windowNumber(),
            level: window.level(),
            active: objc2_app_kit::NSApplication::sharedApplication(mtm).isActive(),
            visible: window.isVisible(),
            occlusion: window.occlusionState(),
            on_active_space: window.isOnActiveSpace(),
            screen: window.screen().is_some(),
        };
        let result = if arm { surface.guard.arm(current) } else { surface.guard.observe(current) };
        if phase != "poll" || result.is_err() {
            eprintln!("APPLE_SURFACE phase={phase} mono_ns={} window={} level={} active={} visible={} occlusion={:?} on_active_space={} screen={} armed={} first_loss_ns={:?}",
                current.mono_ns, current.window, current.level, current.active, current.visible,
                current.occlusion, current.on_active_space, current.screen, surface.guard.armed,
                surface.guard.first_loss.map(|lost| lost.mono_ns));
        }
        (current, result.map_err(|first_loss| SurfaceBlocked { phase, first_loss, current }))
    })
}

fn require_surface(result: Result<(), SurfaceBlocked>) {
    if let Err(blocked) = result {
        eprintln!("APPLE_BLOCKED phase={} first_loss={:?} current={:?}; entire run invalid, no samples accepted",
            blocked.phase, blocked.first_loss, blocked.current);
        std::panic::panic_any(blocked);
    }
}

fn check_surface(phase: &'static str) {
    require_surface(dispatch2::run_on_main(move |mtm| observe_surface(mtm, phase, false).1));
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
    SURFACE.with(|slot| {
        *slot.borrow_mut() = Some(ProbeSurface { window: window.clone(), guard: SurfaceGuard::default() });
    });
    let view = window.contentView().unwrap();
    let native = AppleBackend::new();
    native.set_muted(true);
    unsafe { native.attach(std::ptr::from_ref(&*view).cast_mut().cast()); }
    native.set_frame([0.0, 0.0, 320.0, 192.0]);
    native.video_layer().setFrame(CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: frame.size });
    let (surface_tx, surface_rx) = std::sync::mpsc::sync_channel(1);
    let surface_changed = block2::RcBlock::new(move |_: std::ptr::NonNull<objc2_foundation::NSNotification>| {
        let mtm = MainThreadMarker::new().expect("window notification off main thread");
        let (current, _) = observe_surface(mtm, "notification", false);
        if current.available() { let _ = surface_tx.try_send(()); }
    });
    // The callback captures only a Send channel; the panel stays on main.
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
            require_surface(dispatch2::run_on_main(|mtm| observe_surface(mtm, "arm", true).1));
            if std::env::args().any(|a| a == "--check-surface-loss") {
                // A separate negative check: hide this actual panel, not a
                // mocked visibility value or a substitute video readback.
                dispatch2::run_on_main(|_| {
                    let window = SURFACE.with(|slot| slot.borrow().as_ref().expect("probe surface").window.clone());
                    window.orderOut(None);
                });
                check_surface("intentional_loss");
                panic!("surface-loss check did not block the invalid run");
            }
            if audio_tail { measure_audio_tail(path, backend) }
            else { measure(path, backend, transport) }
        }));
        if let Err(e) = &result { eprintln!("Apple timing smoke failed: {e:?}"); }
        dispatch2::run_on_main(|_| READBACK.with(|slot| { slot.borrow_mut().take(); }));
        let code = match &result {
            Ok(()) => 0,
            Err(error) if error.is::<SurfaceBlocked>() => 2,
            Err(_) => 1,
        };
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
    eprintln!("mono_ns,audio_s,engine_s,frames,dropped,delay_sum_ms,delta_frames,delta_delay_ms");
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
        let surface = dispatch2::run_on_main(move |mtm| unsafe {
            observe_surface(mtm, "poll", false).1?;
            let renderer = b.native.video_layer().sampleBufferRenderer();
            let block = block2::RcBlock::new(move |metrics: *mut objc2_av_foundation::AVVideoPerformanceMetrics| {
                let snapshot = metrics.as_ref().map(|m| (m.totalNumberOfFrames(), m.numberOfDroppedFrames(), m.totalAccumulatedFrameDelay()));
                let _ = tx.send(snapshot);
            });
            renderer.loadVideoPerformanceMetricsWithCompletionHandler(&block);
            Ok(())
        });
        require_surface(surface);
        if let Some(metrics) = rx.recv_timeout(Duration::from_secs(2)).expect("renderer metrics timeout") {
            if metrics.0 != previous.0 || metrics.1 != previous.1 {
                let count = metrics.0 - previous.0;
                let delay = (metrics.2 - previous.2) * 1000.0;
                eprintln!("{},{:.9},{:.9},{},{},{:.6},{},{:.6}", player::clock::current_monotonic_ns(), audio.as_secs_f64(), state.position.as_secs_f64(), metrics.0, metrics.1, metrics.2 * 1000.0, count, delay);
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
            eprintln!("APPLE_FRAME audio_s={:.9} video_frame={frame} offset_ms={offset:.6} mono_ns={}",
                audio_at.as_secs_f64(), player::clock::current_monotonic_ns());
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
    eprintln!("APPLE_EOS expected_frame={last} displayed={shown:?} mono_ns={}", player::clock::current_monotonic_ns());
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
        check_surface("poll");
        assert!(state.error.is_none(), "{state:?}");
        assert!(Instant::now() < deadline, "initial playback stalled: {state:?}");
        if !state.buffering && state.position >= Duration::from_secs(1) { break; }
        std::thread::sleep(Duration::from_millis(5));
    }
    player.seek(Duration::from_secs(6));
    loop {
        let state = player.state();
        check_surface("poll");
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
    check_surface("before_readback");
    let frame = dispatch2::run_on_main(move |mtm| unsafe {
        use objc2_core_video::*;
        use objc2_av_foundation::AVQueuedSampleBufferRendering;
        let renderer = backend.native.video_layer().sampleBufferRenderer();
        let timebase = renderer.timebase();
        eprintln!("APPLE_READBACK ready={} status={:?} renderer_rate={} renderer_time={:?} mono_ns={} layer_frame={:?}",
            backend.native.video_layer().isReadyForDisplay(), renderer.status(), timebase.rate(), timebase.time(),
            player::clock::current_monotonic_ns(), backend.native.video_layer().frame());
        // Observe again in the same main-queue call as the pixel read.
        // A loss here is latched and rejected on the worker after readback.
        let _ = observe_surface(mtm, "readback", false);
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
    });
    eprintln!("APPLE_DISPLAYED mono_ns={} frame={frame:?}", player::clock::current_monotonic_ns());
    check_surface("after_readback");
    frame
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

#[cfg(test)]
mod surface_tests {
    use super::{SurfaceGuard, SurfaceSnapshot};
    use objc2_app_kit::NSWindowOcclusionState;

    fn snapshot(mono_ns: i64, active: bool, occlusion: usize) -> SurfaceSnapshot {
        SurfaceSnapshot { mono_ns, window: 7, level: 3, active, visible: true,
            occlusion: NSWindowOcclusionState(occlusion), on_active_space: true, screen: true }
    }

    #[test]
    fn stale_startup_visibility_does_not_admit_an_occluded_surface() {
        let mut guard = SurfaceGuard::default();
        assert!(guard.observe(snapshot(1, true, 8194)).is_ok());
        let unavailable = snapshot(2, true, 8192);
        assert_eq!(guard.arm(unavailable), Err(unavailable));
    }

    #[test]
    fn visibility_loss_invalidates_the_run_even_after_restoration() {
        let mut guard = SurfaceGuard::default();
        assert!(guard.arm(snapshot(1, true, 8194)).is_ok());
        let lost = snapshot(2, false, 8192);
        assert_eq!(guard.observe(lost), Err(lost));
        assert_eq!(guard.observe(snapshot(3, false, 8194)), Err(lost));
        assert_eq!(guard.arm(snapshot(4, true, 8194)), Err(lost));
    }

    #[test]
    fn inactive_but_visible_surface_remains_admissible() {
        let mut guard = SurfaceGuard::default();
        assert!(guard.arm(snapshot(1, true, 8194)).is_ok());
        assert!(guard.observe(snapshot(2, false, 8194)).is_ok());
    }
}
