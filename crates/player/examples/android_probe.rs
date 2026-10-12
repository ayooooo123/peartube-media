//! On-device probe for the Android backend, run over adb as a plain
//! executable (API 29+).
//!
//! 1. Video: opens an `AImageReader`, takes its window as the video surface,
//!    demuxes an H.264 MP4 with oxideav-mp4, and pushes the packets through
//!    `Backend::video` + `poll_transition`/`push_packet`, counting the
//!    frames the reader receives. Midway through the stream it swaps to a
//!    second `AImageReader` using `SurfaceRegistry` reservation and retirement
//!    receipts, which rebuilds the codec on the new window and resumes from
//!    the next keyframe.
//! 2. Audio: plays 3 s of a 440 Hz sine (48 kHz, stereo, f32) through
//!    `Backend::audio`, sampling the sink's `Clock` before and after and
//!    checking the media clock advances ~3 s.
//!
//! Exit status 0 = selected probes passed. Counts and clock readings go to
//! stdout.
//! `--video-only` isolates Surface/decoder lifecycle checks from AAudio.

use oxideav_core::{MediaType, Packet, RuntimeContext, TimeBase};
use player::android::{
    SurfaceAdmissionError, SurfaceBinding, SurfaceRegistry, SurfaceRetirement,
    SurfaceRetirementError,
};
use player::backend::{
    Backend, Clock, PictureReady, ProducerId, SinkError, VideoControl, VideoError, VideoMode,
    VideoOutput, VideoRequest, VideoSink, VideoTarget,
};
use player::AndroidBackend;
use std::fs::File;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::time::{Duration, Instant};

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
    unsafe { Waker::from_raw(raw()) }
}

/// Wait on the retirement *receipt* via poll, not cached is_retired/status (S8).
fn await_retirement(retirement: &SurfaceRetirement, limit: Duration) -> Result<(), String> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let t0 = Instant::now();
    loop {
        match retirement.poll(&mut cx) {
            Poll::Ready(Ok(_)) => return Ok(()),
            Poll::Ready(Err(SurfaceRetirementError::Failed(e) | SurfaceRetirementError::Quarantined(e))) => {
                return Err(format!("retirement failed: {e}"));
            }
            Poll::Pending => {
                if t0.elapsed() > limit {
                    return Err("retirement timed out while still Pending".into());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

// Shell executables do not inherit an app's Binder pool. These platform entry
// points belong only in this probe's main, never in the Player library.
// Contract: frameworks/native/libs/binder/ndk/include_platform/android/binder_process.h.
fn start_probe_binder_pool() -> Result<(), &'static str> {
    use std::ffi::{c_char, c_int, c_void};
    #[link(name = "dl")]
    unsafe extern "C" {
        fn dlopen(path: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    }
    unsafe {
        // Keep the library resident for the process-owned Binder threads.
        let library = dlopen(c"libbinder_ndk.so".as_ptr(), 2);
        if library.is_null() { return Err("libbinder_ndk unavailable"); }
        let set = dlsym(library, c"ABinderProcess_setThreadPoolMaxThreadCount".as_ptr());
        let start = dlsym(library, c"ABinderProcess_startThreadPool".as_ptr());
        if set.is_null() || start.is_null() {
            return Err("standalone Binder pool entry points unavailable");
        }
        let set: unsafe extern "C" fn(u32) -> bool = std::mem::transmute(set);
        let start: unsafe extern "C" fn() = std::mem::transmute(start);
        if !set(1) { return Err("Binder pool configuration failed"); }
        start();
    }
    Ok(())
}

fn main() {
    eprintln!("[video] probe pid={}", std::process::id());
    start_probe_binder_pool().expect("standalone native probe requires incoming Binder callbacks");
    let mode = if std::env::args().any(|a| a == "--mode=software") {
        Mode::SoftwareFrames
    } else {
        Mode::Compressed
    };
    let mut code = if std::env::args().any(|a| a == "--video-only") { 0 } else { audio_probe() };
    code |= video_probe(mode);
    if std::env::args().any(|a| a == "--all") {
        code |= sw_first_probe();
        code |= nosurface_probe();
    }
    std::process::exit(code);
}

// ---------------------------------------------------------------- video

const EXPECTED_FRAMES: usize = 192;

fn video_probe(mode: Mode) -> i32 {
    println!("[video] starting h264 probe with mid-decode window swap");
    let path = "/data/local/tmp/peartube_probe.mp4";
    let (packets, params, time_base) = match load_h264(path) {
        Ok(v) => v,
        Err(e) => {
            println!("[video] FAIL: cannot load sample: {e}");
            return 1;
        }
    };
    println!(
        "[video] packets: {} (extradata {} B, codec {})",
        packets.len(),
        params.extradata.len(),
        params.codec_id.0
    );
    if packets.is_empty() {
        println!("[video] FAIL: no packets");
        return 1;
    }

    let reader_a = make_reader(mode);
    let frames_a = reader_a.1.clone();
    let reservation_a = SurfaceRegistry::global()
        .reserve_native()
        .map_err(|e| format!("reserve native window A: {e:?}"))
        .unwrap();
    let window_a = reader_a
        .0
        .window()
        .map_err(|e| format!("reader A window: {e:?}"))
        .unwrap();
    let binding_a = reservation_a.commit(window_a);
    let reader_a_slot: Arc<parking_lot::Mutex<Option<ndk::media::image_reader::ImageReader>>> =
        Arc::new(parking_lot::Mutex::new(Some(reader_a.0)));

    let backend = AndroidBackend::new();
    backend.set_video_surface(binding_a.clone()).unwrap();

    let clock = probe_clock();
    let _box_sink = backend.video(clock.clone());
    let sink_video: Arc<parking_lot::Mutex<player::android::AndroidVideoSink>> = backend
        .shared()
        .active_video
        .lock()
        .as_ref()
        .and_then(|w| w.upgrade())
        .expect("Backend::video registers the sink in active_video");
    sink_video.lock().prefer_software_decoder(true);
    let packets = Arc::new(packets);

    let swap_at = packets.len() / 2;
    let swap_state = Arc::new(parking_lot::Mutex::new(Swap::Pending));
    let frames_b_slot: Arc<parking_lot::Mutex<Option<Arc<AtomicUsize>>>> =
        Arc::new(parking_lot::Mutex::new(None));
    let frames_c_slot: Arc<parking_lot::Mutex<Option<Arc<AtomicUsize>>>> =
        Arc::new(parking_lot::Mutex::new(None));

    let swap_result = run_stream(
        &backend,
        &sink_video,
        &clock,
        &params,
        packets.as_ref(),
        time_base,
        swap_at,
        &swap_state,
        &binding_a,
        &reader_a_slot,
        &frames_b_slot,
        &frames_c_slot,
        mode,
    );
    if let Err(e) = &swap_result {
        println!("[video] push error: {e}");
        backend.suspend();
        return 1;
    }

    let a = frames_a.load(Ordering::SeqCst);
    let b = frames_b_slot
        .lock()
        .as_ref()
        .map(|c| c.load(Ordering::SeqCst))
        .unwrap_or(0);
    let c = frames_c_slot
        .lock()
        .as_ref()
        .map(|c| c.load(Ordering::SeqCst))
        .unwrap_or(0);
    println!("[video] frames on window A (before swap): {a}");
    println!("[video] frames on window B (after swap):  {b}");
    if c > 0 {
        println!("[video] frames on window C (after fallback): {c}");
    }
    let total = a + b + c;
    println!("[video] total frames received: {total}");

    let gop_skip = 30 + 12;
    let floor = EXPECTED_FRAMES.saturating_sub(gop_skip);
    let ok = a > 0 && b + c > 0 && total >= floor;
    if ok {
        println!("[video] PASS (window swap resumed on new surface)");
        0
    } else {
        println!(
            "[video] FAIL: need frames on both windows and total >= {floor} (got A={a}, B+C={}, total={total})",
            b + c
        );
        1
    }
}

enum Swap {
    Pending,
    NewWindowSet,
}

struct ProbeControl;
impl VideoControl for ProbeControl {
    fn cancelled(&self, _producer: ProducerId, _seek_generation: u64) -> bool {
        false
    }
    fn active_now(&self) -> Instant {
        Instant::now()
    }
    fn wake(&self) {}
}

fn reopen_mode(
    sink: &mut player::android::AndroidVideoSink,
    mode: Mode,
    params: &oxideav_core::CodecParameters,
    producer: ProducerId,
    clock: &Arc<player::clock::FreeRunningClock>,
) -> Result<(), SinkError> {
    clock.pause();
    let ready_clock = clock.clone();
    let first_picture = std::sync::atomic::AtomicBool::new(true);
    let ready = PictureReady::new(move |pts| {
        if first_picture.swap(false, Ordering::SeqCst) {
            ready_clock.seek(pts);
            ready_clock.play();
            println!("[video] producer {} first picture at {pts:?}; clock released", producer.0);
        }
    });
    let target = match mode {
        Mode::Compressed => {
            sink.prefer_software_decoder(true);
            VideoTarget::Compressed {
                params: Arc::new(params.clone()),
                ready,
                present_from: Duration::ZERO,
            }
        }
        Mode::SoftwareFrames => VideoTarget::Frames {
            params: Arc::new(params.clone()),
            ready,
            reset: true,
        },
    };
    let request = VideoRequest {
        producer,
        seek_generation: 1,
        output_revision: sink.output().revision,
        target,
        deadline: Instant::now() + Duration::from_secs(5),
        control: Arc::new(ProbeControl),
    };
    let t0 = Instant::now();
    loop {
        match sink.poll_transition(&request) {
            Poll::Ready(Ok(_)) => return Ok(()),
            Poll::Ready(Err(e)) => {
                return Err(SinkError::Fatal(format!("transition error: {e:?}")))
            }
            Poll::Pending => {
                if t0.elapsed() > Duration::from_secs(5) {
                    return Err(SinkError::Fatal("transition timed out".into()));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn decode_and_push_frame(
    sink_video: &Arc<parking_lot::Mutex<player::android::AndroidVideoSink>>,
    decoder: &mut Option<Box<dyn oxideav_core::Decoder>>,
    params: &oxideav_core::CodecParameters,
    pkt: &Packet,
    pts: Duration,
    producer: ProducerId,
) -> Result<(), SinkError> {
    let decoder = decoder.get_or_insert_with(|| {
        let mut ctx = oxideav_core::RuntimeContext::new();
        oxideav_h264::register_codecs(&mut ctx.codecs);
        ctx.codecs
            .first_decoder(params)
            .expect("software h264 decoder")
    });
    decoder.send_packet(pkt).map_err(|e| {
        SinkError::Fallback(format!("software decode send: {e}"))
    })?;
    loop {
        match decoder.receive_frame() {
            Ok(oxideav_core::Frame::Video(frame)) => {
                let mut sink = sink_video.lock();
                let mut frame_opt = Some(frame);
                loop {
                    match sink.push_frame(producer, &mut frame_opt, pts) {
                        Ok(()) => break,
                        Err(VideoError::Sink(SinkError::WouldBlock)) => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(VideoError::Sink(e)) => return Err(e),
                        Err(e) => return Err(SinkError::Fatal(format!("{e:?}"))),
                    }
                }
            }
            Ok(_) => {}
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(e) => return Err(SinkError::Fallback(format!("software decode: {e}"))),
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Compressed,
    SoftwareFrames,
}

#[allow(clippy::too_many_arguments)]
fn run_stream(
    backend: &Arc<AndroidBackend>,
    sink_video: &Arc<parking_lot::Mutex<player::android::AndroidVideoSink>>,
    clock: &Arc<player::clock::FreeRunningClock>,
    params: &oxideav_core::CodecParameters,
    packets: &[(Packet, bool)],
    time_base: TimeBase,
    swap_at: usize,
    swap_state: &Arc<parking_lot::Mutex<Swap>>,
    binding_a: &Arc<SurfaceBinding>,
    reader_a_slot: &Arc<parking_lot::Mutex<Option<ndk::media::image_reader::ImageReader>>>,
    frames_b_slot: &Arc<parking_lot::Mutex<Option<Arc<AtomicUsize>>>>,
    frames_c_slot: &Arc<parking_lot::Mutex<Option<Arc<AtomicUsize>>>>,
    mode: Mode,
) -> Result<(), SinkError> {
    let mut sw_decoder: Option<Box<dyn oxideav_core::Decoder>> = None;
    let mut pushed = 0usize;
    let mut fallbacks = 0usize;
    let mut producer_id = 1u64;

    let mut current_producer = ProducerId(producer_id);
    {
        let mut sink = sink_video.lock();
        reopen_mode(&mut sink, mode, params, current_producer, clock)?;
    }
    println!("[video] producer {} configured", current_producer.0);

    let mut reader_b: Option<ndk::media::image_reader::ImageReader> = None;
    let mut binding_b: Option<Arc<SurfaceBinding>> = None;
    let mut reader_c: Option<ndk::media::image_reader::ImageReader> = None;
    let mut binding_c: Option<Arc<SurfaceBinding>> = None;

    for (pkt, random_access) in packets.iter() {
        if pushed == swap_at {
            println!(
                "[video] swap: retiring binding A at packet {pushed}/{}",
                packets.len()
            );
            let t0 = Instant::now();
            let retirement = binding_a.retire();
            match await_retirement(&retirement, Duration::from_secs(5)) {
                Ok(()) => {
                    println!("[video] binding A retired after {:?}", t0.elapsed());
                    *reader_a_slot.lock() = None;
                    println!("[video] old ImageReader destroyed");
                }
                Err(e) => {
                    // Timeout/error must not print retired or drop ImageReader as success (S8).
                    println!("[video] FAIL: binding A retirement incomplete: {e}");
                    return Err(SinkError::Fatal(e));
                }
            }
            backend.clear_video_surface();
            if !random_access {
                return Err(SinkError::Fatal("probe midpoint is not a random-access packet".into()));
            }
            // Pollable output replacement needs an explicit new producer.
            let (reader, counter) = make_reader(mode);
            frames_b_slot.lock().replace(counter);
            let res_b = SurfaceRegistry::global()
                .reserve_native()
                .map_err(|e| SinkError::Fatal(format!("reserve B: {e:?}")))?;
            let window = reader.window()
                .map_err(|e| SinkError::Fatal(format!("reader B window: {e:?}")))?;
            let binding = res_b.commit(window);
            backend.set_video_surface(binding.clone())
                .map_err(|e| SinkError::Fatal(format!("set B: {e:?}")))?;
            *swap_state.lock() = Swap::NewWindowSet;
            producer_id += 1;
            current_producer = ProducerId(producer_id);
            reopen_mode(&mut sink_video.lock(), mode, params, current_producer, clock)?;
            println!("[video] replacement producer {} configured", current_producer.0);
            reader_b = Some(reader);
            binding_b = Some(binding);
        }

        let pts = packet_media_time(pkt, time_base);
        let r = if mode == Mode::SoftwareFrames {
            decode_and_push_frame(sink_video, &mut sw_decoder, params, pkt, pts, current_producer)
        } else {
            let mut sink = sink_video.lock();
            push_packet(&mut *sink, pkt, pts, *random_access, current_producer)
        };

        match r {
            Ok(()) => {}
            Err(SinkError::Fallback(e)) => {
                println!("[video] fallback at packet {pushed}: {e}");
                fallbacks += 1;
                if fallbacks > 1 {
                    return Err(SinkError::Fallback(format!(
                        "{e} (after {fallbacks} fallbacks)"
                    )));
                }
                std::thread::sleep(Duration::from_secs(20));

                if let Some(prev) = binding_b.take() {
                    let ret = prev.retire();
                    await_retirement(&ret, Duration::from_secs(5))
                        .map_err(SinkError::Fatal)?;
                    drop(reader_b.take());
                } else {
                    drop(reader_b.take());
                }
                backend.clear_video_surface();

                let (reader, counter) = make_reader(mode);
                frames_c_slot.lock().replace(counter);
                let res_c = SurfaceRegistry::global()
                    .reserve_native()
                    .map_err(|e| SinkError::Fatal(format!("reserve C: {e:?}")))?;
                let w = reader
                    .window()
                    .map_err(|e| SinkError::Fatal(format!("reader C window: {e:?}")))?;
                let c_binding = res_c.commit(w);
                backend
                    .set_video_surface(c_binding.clone())
                    .map_err(|e| SinkError::Fatal(format!("set C: {e:?}")))?;
                println!("[video] moved to fresh window (reader C)");

                producer_id += 1;
                current_producer = ProducerId(producer_id);
                let mut sink = sink_video.lock();
                reopen_mode(&mut sink, mode, params, current_producer, clock)?;
                push_packet(&mut *sink, pkt, pts, *random_access, current_producer)?;

                reader_c = Some(reader);
                binding_c = Some(c_binding);
            }
            Err(e) => return Err(e),
        }
        pushed += 1;
        if pushed == 1 || pushed % 24 == 0 {
            println!("[video] admitted {pushed}/{} packets", packets.len());
        }
    }

    std::thread::sleep(Duration::from_millis(1500));
    {
        let mut sink = sink_video.lock();
        let t0 = Instant::now();
        let mut drained = false;
        while t0.elapsed() < Duration::from_secs(5) {
            match sink.poll_finish(current_producer) {
                Poll::Ready(Ok(())) => {
                    drained = true;
                    break;
                }
                Poll::Ready(Err(e)) => return Err(SinkError::Fatal(format!("{e:?}"))),
                Poll::Pending => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        if !drained {
            return Err(SinkError::Fatal(
                "EOS probe did not complete drain via poll_finish".into(),
            ));
        }
    }

    if let Some(b) = binding_c.or(binding_b) {
        let ret = b.retire();
        await_retirement(&ret, Duration::from_secs(5)).map_err(SinkError::Fatal)?;
        drop(reader_c.take());
        drop(reader_b.take());
    } else {
        drop(reader_c);
        drop(reader_b);
    }

    Ok(())
}


fn make_reader(mode: Mode) -> (ndk::media::image_reader::ImageReader, Arc<AtomicUsize>) {
    use ndk::hardware_buffer::HardwareBufferUsage;
    use ndk::media::image_reader::{ImageFormat, ImageReader};
    let (format, usage) = match mode {
        Mode::Compressed => (
            ImageFormat::YUV_420_888,
            HardwareBufferUsage::GPU_COLOR_OUTPUT
                | HardwareBufferUsage::GPU_SAMPLED_IMAGE
                | HardwareBufferUsage::VIDEO_ENCODE
                | HardwareBufferUsage::CPU_READ_OFTEN,
        ),
        Mode::SoftwareFrames => (
            ImageFormat::RGBA_8888,
            HardwareBufferUsage::GPU_COLOR_OUTPUT | HardwareBufferUsage::CPU_READ_OFTEN,
        ),
    };
    let mut reader = ImageReader::new_with_usage(160, 120, format, usage, 8)
        .map_err(|e| format!("ImageReader::new: {e:?}"))
        .unwrap();
    let frames = Arc::new(AtomicUsize::new(0));
    let count = frames.clone();
    reader
        .set_image_listener(Box::new(move |r| {
            while let Ok(ndk::media::image_reader::AcquireResult::Image(img)) =
                r.acquire_next_image()
            {
                count.fetch_add(1, Ordering::SeqCst);
                drop(img);
            }
        }))
        .unwrap();
    (reader, frames)
}

fn probe_clock() -> Arc<player::clock::FreeRunningClock> {
    Arc::new(player::clock::FreeRunningClock::new())
}

fn packet_media_time(pkt: &Packet, time_base: TimeBase) -> Duration {
    let pts = pkt.pts.unwrap_or(0);
    let micros = pts as i128 * 1_000_000 * time_base.num() as i128 / time_base.den() as i128;
    Duration::from_micros(micros.max(0) as u64)
}

fn load_h264(
    path: &str,
) -> Result<(Vec<(Packet, bool)>, oxideav_core::CodecParameters, TimeBase), String> {
    let mut ctx = RuntimeContext::new();
    oxideav_mp4::__oxideav_entry(&mut ctx);
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut demuxer = ctx.containers
        .open_demuxer("mp4", Box::new(file), &ctx.codecs)
        .map_err(|e| e.to_string())?;
    let video = demuxer.streams().iter()
        .find(|s| s.params.media_type == MediaType::Video)
        .ok_or("no video stream")?.clone();
    let mut packets = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) if p.stream_index == video.index => {
                let random_access = p.flags.keyframe || demuxer.packet_metadata().container_keyframe;
                packets.push((p, random_access));
            }
            Ok(_) => {}
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => return Err(format!("demux: {e}")),
        }
    }
    Ok((packets, video.params, video.time_base))
}

fn push_packet(
    sink: &mut dyn VideoSink,
    packet: &Packet,
    pts: Duration,
    random_access: bool,
    producer: ProducerId,
) -> Result<(), SinkError> {
    let mut pkt_opt = Some(packet.clone());
    let started = Instant::now();
    loop {
        match sink.push_packet(producer, &mut pkt_opt, pts, random_access) {
            Ok(()) => return Ok(()),
            Err(VideoError::Sink(SinkError::WouldBlock)) => {
                if started.elapsed() >= Duration::from_secs(5) {
                    return Err(SinkError::Fatal(format!(
                        "packet admission stalled: producer={}, pts={pts:?}, input_retained={}",
                        producer.0, pkt_opt.is_some(),
                    )));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(VideoError::Sink(e)) => return Err(e),
            Err(e) => return Err(SinkError::Fatal(format!("{e:?}"))),
        }
    }
}

fn sw_first_probe() -> i32 {
    println!("[sw] starting software-first probe");
    let (packets, params, time_base) = match load_h264("/data/local/tmp/peartube_probe.mp4") {
        Ok(v) => v,
        Err(e) => {
            println!("[sw] FAIL: load: {e}");
            return 1;
        }
    };
    let mut reader = match ndk::media::image_reader::ImageReader::new(
        352,
        288,
        ndk::media::image_reader::ImageFormat::YUV_420_888,
        8,
    ) {
        Ok(r) => r,
        Err(e) => {
            println!("[sw] FAIL: reader: {e:?}");
            return 1;
        }
    };
    let frames = Arc::new(AtomicUsize::new(0));
    {
        let counter = frames.clone();
        reader
            .set_image_listener(Box::new(move |_r| {
                counter.fetch_add(1, Ordering::SeqCst);
                while let Ok(ndk::media::image_reader::AcquireResult::Image(img)) =
                    _r.acquire_latest_image()
                {
                    drop(img);
                }
            }))
            .unwrap();
    }
    let res = SurfaceRegistry::global().reserve_native().unwrap();
    let window = reader.window().unwrap();
    let binding = res.commit(window);
    let backend = AndroidBackend::new();
    backend.set_video_surface(binding.clone()).unwrap();

    let clock = probe_clock();
    let _box_sink = backend.video(clock.clone());
    let sink_video: Arc<parking_lot::Mutex<player::android::AndroidVideoSink>> = backend
        .shared()
        .active_video
        .lock()
        .as_ref()
        .and_then(|w| w.upgrade())
        .expect("Backend::video registers the sink in active_video");
    let t0 = Instant::now();
    let producer = ProducerId(101);
    {
        let mut sink = sink_video.lock();
        if let Err(e) = reopen_mode(&mut *sink, Mode::Compressed, &params, producer, &clock) {
            println!("[sw] FAIL: open declined: {e:?}");
            return 1;
        }
    }
    let mut pushed = 0usize;
    for (pkt, random_access) in &packets {
        let pts = packet_media_time(pkt, time_base);
        let r = {
            let mut sink = sink_video.lock();
            push_packet(&mut *sink, pkt, pts, *random_access, producer)
        };
        match r {
            Ok(()) => pushed += 1,
            Err(e) => {
                println!("[sw] FAIL: push {pushed}: {e} after {:?}", t0.elapsed());
                return 1;
            }
        }
    }
    std::thread::sleep(Duration::from_millis(1500));
    let n = frames.load(Ordering::SeqCst);
    println!(
        "[sw] pushed {pushed}, frames received: {n} in {:?}",
        t0.elapsed()
    );

    let retirement = binding.retire();
    if let Err(e) = await_retirement(&retirement, Duration::from_secs(5)) {
        println!("[sw] FAIL: retirement incomplete: {e}");
        return 1;
    }
    drop(reader);

    if n >= 150 {
        println!("[sw] PASS");
        0
    } else {
        println!("[sw] FAIL: <150 frames");
        1
    }
}

fn nosurface_probe() -> i32 {
    println!("[nosurf] starting no-surface probe");
    let (packets, params, time_base) = match load_h264("/data/local/tmp/peartube_probe.mp4") {
        Ok(v) => v,
        Err(e) => {
            println!("[nosurf] FAIL: load: {e}");
            return 1;
        }
    };
    let mime = "video/avc";
    let codec = match ndk::media::media_codec::MediaCodec::from_codec_name("c2.android.avc.decoder")
    {
        Some(c) => c,
        None => {
            println!("[nosurf] FAIL: no codec");
            return 1;
        }
    };
    let mut format = ndk::media::media_format::MediaFormat::new();
    format.set_str("mime", mime);
    format.set_i32("width", params.width.unwrap_or(720) as i32);
    format.set_i32("height", params.height.unwrap_or(480) as i32);
    let (s0, s1, _) = crate_avcc_split(&params.extradata);
    if !s0.is_empty() {
        format.set_buffer("csd-0", &s0);
    }
    if !s1.is_empty() {
        format.set_buffer("csd-1", &s1);
    }
    codec
        .configure(&format, None, ndk::media::media_codec::MediaCodecDirection::Decoder)
        .map_err(|e| println!("[nosurf] configure err: {e:?}"))
        .unwrap();
    codec.start().map_err(|e| println!("[nosurf] start err: {e:?}")).unwrap();

    let mut out_count = 0usize;
    for (pkt, _) in packets.iter().take(60) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let idx = loop {
            match codec.dequeue_input_buffer(Duration::from_millis(20)) {
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::Buffer(b)) => break Some(b),
                Ok(_) => {
                    if std::time::Instant::now() > deadline {
                        break None;
                    }
                }
                Err(e) => {
                    println!("[nosurf] dequeue_input err: {e:?}");
                    break None;
                }
            }
        };
        if idx.is_none() {
            println!("[nosurf] FAIL: input stalled at packet {out_count}");
            return 1;
        }
        let mut buf = match idx {
            Some(b) => b,
            None => unreachable!(),
        };
        let dest = buf.buffer_mut();
        let data = &pkt.data;
        if dest.len() < data.len() {
            println!("[nosurf] FAIL: input buffer too small");
            return 1;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), dest.as_mut_ptr().cast(), data.len());
        }
        let pts = packet_media_time(pkt, time_base);
        codec
            .queue_input_buffer(buf, 0, data.len(), pts.as_micros() as u64, 0)
            .map_err(|e| println!("[nosurf] queue err: {e:?}"))
            .unwrap();
        loop {
            match codec.dequeue_output_buffer(Duration::from_millis(5)) {
                Ok(ndk::media::media_codec::DequeuedOutputBufferInfoResult::Buffer(b)) => {
                    out_count += 1;
                    let _ = codec.release_output_buffer(b, false);
                }
                Ok(_) => {}
                Err(e) => {
                    println!("[nosurf] dequeue_output err: {e:?}");
                    return 1;
                }
            }
            if out_count > 4000 {
                break;
            }
            static SPIN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = SPIN.fetch_add(1, Ordering::Relaxed) % 3;
            if n == 2 {
                break;
            }
        }
    }
    println!("[nosurf] output buffers decoded: {out_count}");
    if out_count >= 40 {
        println!("[nosurf] PASS");
        0
    } else {
        println!("[nosurf] FAIL");
        1
    }
}

fn crate_avcc_split(extra: &[u8]) -> (Vec<u8>, Vec<u8>, usize) {
    if extra.len() < 7 || extra[0] != 1 {
        return (Vec::new(), Vec::new(), 4);
    }
    let nls = ((extra[4] & 0x03) + 1) as usize;
    let num_sps = (extra[5] & 0x1F) as usize;
    let mut off = 6;
    let mut csd0 = Vec::new();
    for _ in 0..num_sps {
        if off + 2 > extra.len() {
            break;
        }
        let l = u16::from_be_bytes([extra[off], extra[off + 1]]) as usize;
        off += 2;
        if off + l > extra.len() {
            break;
        }
        csd0.extend_from_slice(&[0, 0, 0, 1]);
        csd0.extend_from_slice(&extra[off..off + l]);
        off += l;
    }
    let mut csd1 = Vec::new();
    if off < extra.len() {
        let num_pps = extra[off] as usize;
        off += 1;
        for _ in 0..num_pps {
            if off + 2 > extra.len() {
                break;
            }
            let l = u16::from_be_bytes([extra[off], extra[off + 1]]) as usize;
            off += 2;
            if off + l > extra.len() {
                break;
            }
            csd1.extend_from_slice(&[0, 0, 0, 1]);
            csd1.extend_from_slice(&extra[off..off + l]);
            off += l;
        }
    }
    (csd0, csd1, nls)
}

fn audio_probe() -> i32 {
    let backend = AndroidBackend::new();
    let mut audio = backend.audio();
    if let Err(e) = audio.open(48000, oxideav_core::ChannelLayout::Stereo) {
        println!("[audio] FAIL: open: {e}");
        return 1;
    }
    let mut pcm = vec![0.0f32; 48000 * 2];
    for i in 0..48000 {
        let t = i as f32 / 48000.0;
        let s = (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.2;
        pcm[i * 2] = s;
        pcm[i * 2 + 1] = s;
    }
    audio.play();
    let t0 = Instant::now();
    let _ = audio.write(&pcm, Duration::ZERO);
    let c0 = audio.clock().now();
    std::thread::sleep(Duration::from_millis(500));
    let c1 = audio.clock().now();
    let ok = match (c0, c1) {
        (Some(a), Some(b)) => b > a,
        _ => false,
    };
    if ok {
        println!("[audio] PASS: clock advanced from {c0:?} to {c1:?} in {:?}", t0.elapsed());
        0
    } else {
        println!("[audio] FAIL: clock did not advance (c0={c0:?}, c1={c1:?})");
        1
    }
}
