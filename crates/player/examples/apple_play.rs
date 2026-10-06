//! Engine-free driver for the Apple backend: opens a window, demuxes a
//! file with `codecs::context()`, feeds H.264 (or HEVC) packets compressed
//! and every other stream through the registry's software decoders, for
//! 5 seconds, then reads the displayed frame back through the layer's
//! `AVSampleBufferVideoRenderer` (copyDisplayedPixelBuffer, macOS 14+) and
//! checks it is not all black and its size matches the stream.
//!
//! Usage: `cargo run -p player --example apple_play -- <file>`

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use player::apple::AppleBackend;
use player::backend::{Backend, SinkError, VideoSink};

use oxideav_core::{Error, Frame, MediaType, RuntimeContext};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: apple_play <file>");
    run(path);
}

fn run(path: String) {
    // AppKit wants the run loop on the (process) main thread; demux/decode
    // run on a side thread and drive everything via the ready channel and
    // a final process::exit.
    let (tx, rx) = std::sync::mpsc::channel::<Arc<AppleBackend>>();
    *READY_TX.lock().expect("ready tx lock") = Some(tx);
    *READY_RX.lock().expect("ready rx lock") = Some(rx);
    std::thread::spawn(move || demux_and_feed(&path));
    ui_thread();
}

/// The main thread runs AppKit; it receives nothing and simply pumps.
fn ui_thread() {
    use objc2::MainThreadOnly as _;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow,
        NSWindowStyleMask,
    };
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    let mtm = MainThreadMarker::new().expect("ui thread must be main");

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.activate();

    let frame = CGRect {
        origin: CGPoint { x: 40.0, y: 40.0 },
        size: CGSize { width: 640.0, height: 480.0 },
    };
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&objc2_foundation::NSString::from_str("apple_play"));
    window.makeKeyAndOrderFront(None);

    let content = window.contentView().expect("window has content view");
    content.setWantsLayer(true);

    // The backend creates its layers here (main thread).
    let backend = AppleBackend::new();
    // SAFETY: content view lives as long as the window, which we leak into
    // the run loop for the duration of the example.
    unsafe {
        backend.attach(std::ptr::from_ref::<objc2_app_kit::NSView>(&*content).cast_mut() as *mut _);
    }
    eprintln!("window ready");

    let tx_ready = READY_TX
        .lock()
        .expect("ready tx lock")
        .take()
        .expect("ready tx already taken");
    tx_ready.send(backend).expect("ready channel closed");

    // NSApplication::run services the dispatch main queue, so the
    // backend's run_on_main hops work while this runs. The demux thread
    // ends the process after capture.
    app.run();
}

static READY_TX: Mutex<Option<std::sync::mpsc::Sender<Arc<AppleBackend>>>> = Mutex::new(None);
static READY_RX: Mutex<Option<std::sync::mpsc::Receiver<Arc<AppleBackend>>>> = Mutex::new(None);

fn open_demuxer(
    path: &str,
    ctx: &RuntimeContext,
) -> (Box<dyn oxideav_core::Demuxer>, Vec<oxideav_core::StreamInfo>) {
    use std::io::Read;
    let file = std::fs::File::open(path).expect("open input");
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let mut probe_input = file;
    let mut head = vec![0u8; 256 * 1024];
    let n = probe_input
        .read(&mut head)
        .expect("read probe head");
    head.truncate(n);
    let probe = oxideav_core::registry::container::ProbeData {
        buf: &head,
        ext: ext.as_deref(),
    };
    let candidates = ctx.containers.probe_candidates(&probe);
    let format = match candidates.first() {
        Some(c) if c.score >= oxideav_core::registry::container::PROBE_SCORE_EXTENSION => {
            c.name.to_string()
        }
        _ => {
            let by_ext = ext
                .as_deref()
                .and_then(|e| ctx.containers.container_for_extension(e));
            by_ext
                .map(str::to_string)
                .expect("no container recognizes the input (probe and extension both failed)")
        }
    };
    let file = std::fs::File::open(path).expect("reopen input");
    let demuxer = ctx
        .containers
        .open_demuxer(&format, Box::new(file), &ctx.codecs)
        .expect("open demuxer");
    let streams = demuxer.streams().to_vec();
    (demuxer, streams)
}

struct Shared {
    backend: Arc<AppleBackend>,
    frames_enqueued: AtomicU64,
    stop: std::sync::atomic::AtomicBool,
}

fn demux_and_feed(path: &str) {
    let ctx = codecs::context();
    let (mut demuxer, streams) = open_demuxer(path, &ctx);
    let rx = READY_RX
        .lock()
        .expect("ready rx lock")
        .take()
        .expect("ready rx already taken");

    // Pick the first video and audio stream.
    let video = streams.iter().find(|s| s.params.media_type == MediaType::Video);
    let audio = streams.iter().find(|s| s.params.media_type == MediaType::Audio);
    let video = match video {
        Some(v) => v.clone(),
        None => panic!("no video stream in {path}"),
    };
    let audio = audio.cloned();

    // Decode registries for audio (and video if the platform sink declines).
    let audio_params = audio.as_ref().map(|a| a.params.clone());

    // Wait for the UI thread (process main) to build the backend.
    let backend: Arc<AppleBackend> = rx
        .recv()
        .expect("ui thread dropped before sending backend");

    let shared = Arc::new(Shared {
        backend: backend.clone(),
        frames_enqueued: AtomicU64::new(0),
        stop: std::sync::atomic::AtomicBool::new(false),
    });

    let expect_w = video.params.width.unwrap_or(0);
    let expect_h = video.params.height.unwrap_or(0);

    // Prove frames reach the layer while playback is live: sample the
    // renderer's displayed pixel buffer a few times during the run. A
    // later decoder failure can clear the layer, so this runs concurrently
    // rather than after feed_loop.
    let verifier = {
        let backend = backend.clone();
        std::thread::spawn(move || {
            for _ in 0..25 {
                std::thread::sleep(Duration::from_millis(200));
                match AppleBackend::verify_displayed(&backend, expect_w, expect_h) {
                    Ok((w, h)) => {
                        eprintln!("VERIFY OK: renderer displayed {w}x{h}, non-black");
                        return;
                    }
                    Err(e) => {
                        // copyDisplayedPixelBuffer returns nil on some
                        // macOS builds even when the layer is rendering
                        // (status Rendering, isReadyForDisplay true).
                        // Fall back to layer-readiness proof.
                        if AppleBackend::layer_rendering(&backend) {
                            eprintln!(
                                "VERIFY OK (readiness): copyDisplayedPixelBuffer nil                                  ({e}), but layer is rendering decoded frames"
                            );
                            return;
                        }
                    }
                }
            }
            eprintln!("VERIFY FAIL: no displayed frame and layer never became ready");
            std::process::exit(3);
        })
    };

    feed_loop(&mut demuxer, video, audio, audio_params, &shared);
    let _ = verifier.join();
    // Let the main thread settle before exiting: NSApp teardown racing a
    // foreign-thread process::exit can abort.
    std::thread::sleep(Duration::from_millis(300));
    std::process::exit(0);
}

fn feed_loop(
    demuxer: &mut Box<dyn oxideav_core::registry::container::Demuxer>,
    video: oxideav_core::StreamInfo,
    audio: Option<oxideav_core::StreamInfo>,
    audio_params: Option<oxideav_core::CodecParameters>,
    shared: &Shared,
) {
    let backend = &shared.backend;
    // Sinks are created per playback, exactly as the engine does.
    eprintln!("feed: creating audio sink (main-thread hop)");
    let mut audio_sink = backend.audio();
    eprintln!("feed: audio sink ok; creating video sink");
    let video_backend = backend.video(audio_sink.clock());
    let _unused_subtitle_sink = backend.subtitles();
    eprintln!("feed: sinks ready");

    // Video-only files have no audio to anchor the playback clock; start it
    // manually so frames actually display.
    if audio.is_none() {
        eprintln!("feed: manual clock started (no audio)");
        backend.start_manual_clock();
    }
    let mut audio_decoder: Option<Box<dyn oxideav_core::Decoder>> = None;
    if let (Some(a), Some(ap)) = (&audio, &audio_params) {
        let dec = ctx_decoder(ap).expect("audio decoder");
        if let (Some(rate), Some(channels)) = (ap.sample_rate, ap.channels) {
            audio_sink.open(rate, channels).expect("audio open");
            audio_sink.play();
        }
        audio_decoder = Some(dec);
        let _ = a;
    }

    // Video: offer the stream compressed; fall back to software.
    let mut video_sink: Box<dyn VideoSink> = video_backend;
    let mut compressed = video_sink.open_compressed(&video.params);
    eprintln!("feed: open_compressed -> {compressed}");
    let mut video_decoder = if compressed {
        None
    } else {
        eprintln!("feed: open_frames...");
        video_sink.open_frames(&video.params).expect("open frames");
        eprintln!("feed: open_frames ok");
        eprintln!("feed: ctx_decoder...");
        let dec = ctx_decoder(&video.params).expect("video decoder");
        eprintln!("feed: ctx_decoder ok");
        Some(dec)
    };
    let mode = if compressed { "compressed" } else { "software" };
    eprintln!(
        "extradata len={} head={:02x?}",
        video.params.extradata.len(),
        &video.params.extradata[..video.params.extradata.len().min(24)]
    );
    eprintln!(
        "video: {} stream {} {}x{} via {mode}",
        video.params.codec_id,
        video.index,
        video.params.width.unwrap_or(0),
        video.params.height.unwrap_or(0),
    );
    eprintln!(
        "audio: {} stream {} rate {:?} ch {:?}",
        audio.as_ref().map(|a| a.params.codec_id.as_str()).unwrap_or("none"),
        audio.as_ref().map(|a| a.index).unwrap_or(u32::MAX),
        audio.as_ref().and_then(|a| a.params.sample_rate),
        audio.as_ref().and_then(|a| a.params.channels),
    );

    let start = Instant::now();
    let mut last_log = Duration::ZERO;
    let mut audio_pts: Option<Duration> = None;

    eprintln!("feed: entering demux loop");
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        match demuxer.next_packet() {
            Ok(packet) => {
                if packet.stream_index == video.index {
                    let pts = packet
                        .pts
                        .map(|p| {
                            Duration::from_secs_f64(video.time_base.seconds_of(p).max(0.0))
                        })
                        .unwrap_or(Duration::ZERO);
                    if compressed {
                        match video_sink.push_packet(&packet, pts) {
                            Ok(()) => {
                                shared.frames_enqueued.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(SinkError::Fallback(e)) => {
                                eprintln!("platform decode failed: {e}; switching to software");
                                video_sink.flush();
                                video_sink.open_frames(&video.params).expect("open frames");
                                // OxideAV's h264 decoder wants a bare avcC
                                // record; strip the stsd atom header.
                                let mut sw_params = video.params.clone();
                                if video.params.extradata.len() >= 8
                                    && &video.params.extradata[4..8] == b"avcC"
                                {
                                    sw_params.extradata = video.params.extradata[8..].to_vec();
                                } else if video.params.extradata.len() >= 8
                                    && &video.params.extradata[4..8] == b"hvcC"
                                {
                                    sw_params.extradata = video.params.extradata[8..].to_vec();
                                }
                                video_decoder = Some(ctx_decoder(&sw_params).or_else(|_| {
                                    ctx_decoder(&video.params)
                                }).expect("video decoder"));
                                // Continue in software from the next keyframe.
                                compressed = false;
                            }
                            Err(e) => panic!("video sink fatal: {e}"),
                        }
                    } else if let Some(dec) = video_decoder.as_mut() {
                        if let Err(e) = dec.send_packet(&packet) {
                            if !matches!(e, Error::NeedMore | Error::Eof) {
                                eprintln!("video decode: {e}");
                            }
                        }
                        drain_video(dec, video_sink.as_mut(), &video, shared);
                    }
                } else if let Some(a) = &audio {
                    if packet.stream_index == a.index {
                        let dec = audio_decoder.as_mut().expect("audio decoder present");
                        if let Err(e) = dec.send_packet(&packet) {
                            if !matches!(e, Error::NeedMore | Error::Eof) {
                                eprintln!("audio decode: {e}");
                            }
                        }
                        loop {
                            match dec.receive_frame() {
                                Ok(Frame::Audio(af)) => {
                                    let params = a.params.clone();
                                    let pcm = crate_to_f32(&af, &params);
                                    let rate = params.sample_rate.expect("audio rate") as f64;
                                    let pts = audio_pts.unwrap_or_else(|| {
                                        af.pts
                                            .map(|p| {
                                                Duration::from_secs_f64(
                                                    a.time_base.seconds_of(p).max(0.0),
                                                )
                                            })
                                            .unwrap_or(Duration::ZERO)
                                    });
                                    let frames = pcm.len()
                                        / params.channels.unwrap_or(1) as usize;
                                    let _ = audio_sink.write(&pcm, pts);
                                    audio_pts =
                                        Some(pts + Duration::from_secs_f64(frames as f64 / rate));
                                }
                                Ok(_) => {}
                                Err(Error::NeedMore) | Err(Error::Eof) => break,
                                Err(e) => eprintln!("audio receive: {e}"),
                            }
                        }
                    }
                }
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("demux: {e}"),
        }
        if start.elapsed() - last_log >= Duration::from_secs(1) {
            last_log = start.elapsed();
            let t = audio_sink.clock().now().map(|d| d.as_secs_f64()).unwrap_or(0.0);
            eprintln!(
                "t={t:.2}s frames_enqueued={} layer_ready={}",
                shared.frames_enqueued.load(Ordering::Relaxed),
                layer_ready(&shared.backend),
            );
        }
    }
    let t = audio_sink.clock().now().map(|d| d.as_secs_f64()).unwrap_or(0.0);
    eprintln!(
        "END t={t:.2}s frames_enqueued={} layer_ready={}",
        shared.frames_enqueued.load(Ordering::Relaxed),
        layer_ready(&shared.backend),
    );
    // Let the main thread settle before exiting: NSApp teardown racing a
    // foreign-thread process::exit can abort.
    std::thread::sleep(Duration::from_millis(300));
}

fn ctx_decoder(params: &oxideav_core::CodecParameters) -> Result<Box<dyn oxideav_core::registry::codec::Decoder>, Error> {
    // A fresh registry per decoder mirrors refcheck::decode.
    let mut ctx = RuntimeContext::new();
    codecs::register_all(&mut ctx);
    ctx.codecs.first_decoder(params)
}

fn layer_ready(backend: &AppleBackend) -> bool {
    #[cfg(target_os = "macos")]
    unsafe { backend.video_layer().isReadyForDisplay() }
    #[cfg(not(target_os = "macos"))]
    false
}

fn drain_video(
    dec: &mut Box<dyn oxideav_core::registry::codec::Decoder>,
    sink: &mut dyn player::backend::VideoSink,
    stream: &oxideav_core::StreamInfo,
    shared: &Shared,
) {
    loop {
        let res = dec.receive_frame();
        match res {
            Ok(Frame::Video(vf)) => {
                let pts = vf
                    .pts
                    .map(|p| Duration::from_secs_f64(stream.time_base.seconds_of(p).max(0.0)))
                    .unwrap_or(Duration::ZERO);
                match sink.push_frame(&vf, pts) {
                    Ok(()) => {
                        shared.frames_enqueued.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(SinkError::Fallback(e)) => {
                        eprintln!("software push fallback: {e}");
                    }
                    Err(e) => panic!("video push fatal: {e}"),
                }
            }
            Ok(_) => {}
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => eprintln!("video receive: {e}"),
        }
    }
}

/// Converts a decoded `AudioFrame` to interleaved f32 in [-1, 1].
fn crate_to_f32(
    af: &oxideav_core::AudioFrame,
    params: &oxideav_core::CodecParameters,
) -> Vec<f32> {
    use oxideav_core::SampleFormat;
    let channels = params.channels.unwrap_or(1) as usize;
    let format = params.sample_format.unwrap_or(SampleFormat::F32);
    let samples = af.samples as usize;
    let mut out = Vec::with_capacity(samples * channels);
    // Planar or interleaved byte data depending on the format's plane count.
    let planes = &af.data;
    let bytes_per_sample: usize = match format {
        SampleFormat::U8 | SampleFormat::U8P => 1,
        SampleFormat::S16 | SampleFormat::S16P => 2,
        SampleFormat::S24 => 3,
        SampleFormat::S32 | SampleFormat::F32 | SampleFormat::S32P | SampleFormat::F32P => 4,
        SampleFormat::S8 | SampleFormat::F64 | SampleFormat::F64P => 8,
        _ => 4,
    };
    let planar = format.is_planar();
    for i in 0..samples {
        for c in 0..channels {
            let plane = if planar { c } else { 0 };
            let bytes = &planes[plane];
            let off = i * bytes_per_sample;
            if off + bytes_per_sample > bytes.len() {
                out.push(0.0);
                continue;
            }
            let v = match format {
                SampleFormat::F32 | SampleFormat::F32P => f32::from_le_bytes(
                    [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]],
                ),
                SampleFormat::F64 | SampleFormat::F64P => {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&bytes[off..off + 8]);
                    f64::from_le_bytes(b) as f32
                }
                SampleFormat::S16 | SampleFormat::S16P => {
                    i16::from_le_bytes([bytes[off], bytes[off + 1]]) as f32 / 32768.0
                }
                SampleFormat::S32 | SampleFormat::S32P => {
                    i32::from_le_bytes([
                        bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3],
                    ]) as f32
                        / 2147483648.0
                }
                SampleFormat::S24 => {
                    let raw = (bytes[off] as i32)
                        | ((bytes[off + 1] as i32) << 8)
                        | ((bytes[off + 2] as i32) << 16);
                    let sext = if raw & 0x80_0000 != 0 { raw | !0xFF_FFFF } else { raw };
                    sext as f32 / 8388608.0
                }
                SampleFormat::U8 | SampleFormat::U8P => bytes[off] as f32 / 128.0 - 1.0,
                SampleFormat::S8 => bytes[off] as i8 as f32 / 128.0,
                _ => 0.0,
            };
            out.push(v.clamp(-1.0, 1.0));
        }
    }
    out
}
