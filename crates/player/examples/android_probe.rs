//! On-device probe for the Android backend, run over adb as a plain
//! executable (API 29+).
//!
//! 1. Video: opens an `AImageReader`, takes its window as the video surface,
//!    demuxes an H.264 MP4 with oxideav-mp4, and pushes the packets through
//!    `Backend::video` + `open_compressed`/`push_packet`, counting the
//!    frames the reader receives. Midway through the stream it swaps to a
//!    second `AImageReader`: `set_video_window(None)` (the integrator's
//!    contract: this blocks until the codec no longer touches the old
//!    window), then `set_video_window(Some(new))`, which rebuilds the codec
//!    on the new window and resumes from the next keyframe. Both readers'
//!    frame counts are reported; the test passes when the second reader
//!    receives frames after the swap and the combined total reaches the
//!    sample's frame count (± slack for the drain).
//! 2. Audio: plays 3 s of a 440 Hz sine (48 kHz, stereo, f32) through
//!    `Backend::audio`, sampling the sink's `Clock` before and after and
//!    checking the media clock advances ~3 s.
//!
//! Exit status 0 = both probes passed. Counts and clock readings go to
//! stdout.

use oxideav_core::{MediaType, Packet, RuntimeContext, TimeBase};
use player::backend::{Backend, Clock, SinkError};
use player::AndroidBackend;
use std::fs::File;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let mut code = 0;
    code |= video_probe();
    code |= audio_probe();
    std::process::exit(code);
}

// ---------------------------------------------------------------- video

/// Sample duration ~3.2 s at 59.94 fps = 192 frames; keyframes every 30.
const EXPECTED_FRAMES: usize = 192;

fn video_probe() -> i32 {
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

    // Two image readers: the first window, and the one we swap to
    // mid-decode. Both count the frames they receive.
    let readers = make_readers();
    let frames_a = readers[0].1.clone();
    let frames_b = readers[1].1.clone();
    let window_a = readers[0]
        .0
        .window()
        .map_err(|e| format!("reader A window: {e:?}"))
        .unwrap();
    let window_b = readers[1]
        .0
        .window()
        .map_err(|e| format!("reader B window: {e:?}"))
        .unwrap();

    let backend = AndroidBackend::new();
    backend.set_video_window(Some(window_a));

    let clock = Arc::new(NullClock);
    let sink_video = Arc::new(parking_lot::Mutex::new(backend.video(clock)));
    let packets = Arc::new(packets);

    // Swap after roughly half the packets have been pushed (the queue
    // thread tracks its position); the two-step None -> Some exercises
    // the blocking release and the rebuild-on-new-window path.
    let swap_at = packets.len() / 2;
    let swap_state = Arc::new(parking_lot::Mutex::new(Swap::Pending));

    let push = {
        let sink = sink_video.clone();
        let packets = packets.clone();
        let swap_state = swap_state.clone();
        let backend = backend.clone();
        let window_b = window_b.clone();
        std::thread::spawn(move || -> Result<(), SinkError> {
            let mut pushed = 0usize;
            for pkt in packets.iter() {
                if pushed == swap_at {
                    println!(
                        "[video] swap: set_video_window(None) at packet {pushed}/{}",
                        packets.len()
                    );
                    let t0 = Instant::now();
                    backend.set_video_window(None);
                    println!(
                        "[video] set_video_window(None) blocked for {:?}",
                        t0.elapsed()
                    );
                    *swap_state.lock() = Swap::WindowCleared;
                }
                let pts = packet_media_time(pkt, time_base);
                let r = {
                    let mut sink = sink.lock();
                    if pushed == 0 {
                        if !sink.open_compressed(&params) {
                            return Err(SinkError::Fatal(
                                "open_compressed declined".into(),
                            ));
                        }
                        println!("[video] open_compressed accepted");
                    }
                    sink.push_packet(pkt, pts)
                };
                match r {
                    Ok(()) => {}
                    // Unavailable right after the None: expected between
                    // clearing and restoring; retry after the new window.
                    Err(SinkError::Unavailable) if pushed >= swap_at => {
                        if matches!(*swap_state.lock(), Swap::WindowCleared) {
                            backend.set_video_window(Some(window_b.clone()));
                            *swap_state.lock() = Swap::NewWindowSet;
                            println!("[video] swap: new window set at packet {pushed}");
                            let mut sink = sink.lock();
                            sink.push_packet(pkt, pts)?;
                        } else {
                            return Err(SinkError::Unavailable);
                        }
                    }
                    Err(e) => return Err(e),
                }
                pushed += 1;
            }
            // Let the decoder drain what it has queued.
            std::thread::sleep(Duration::from_millis(1500));
            let mut sink = sink.lock();
            sink.flush();
            Ok(())
        })
    };

    let started = Instant::now();
    let mut result: Result<(), String> = Ok(());
    let mut joined = false;
    while started.elapsed() < Duration::from_secs(40) {
        if push.is_finished() {
            result = match push.join() {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(format!("push failed: {e}")),
                Err(_) => Err("push thread panicked".into()),
            };
            joined = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !joined {
        println!("[video] FAIL: push thread still running after 40 s");
        backend.suspend();
        return 1;
    }
    if let Err(e) = &result {
        println!("[video] push error: {e}");
        backend.suspend();
        return 1;
    }

    let a = frames_a.load(Ordering::SeqCst);
    let b = frames_b.load(Ordering::SeqCst);
    println!("[video] frames on window A (before swap): {a}");
    println!("[video] frames on window B (after swap):  {b}");
    let total = a + b;
    println!("[video] total frames received: {total}");

    // Both windows must have received frames, and the total must cover
    // the sample (the drain sleep covers the last frames).
    let ok = a > 0 && b > 0 && total >= EXPECTED_FRAMES * 8 / 10;
    if ok {
        println!("[video] PASS (window swap resumed on new surface)");
        0
    } else {
        println!("[video] FAIL: expected >0 frames on both windows and >= {}", EXPECTED_FRAMES * 8 / 10);
        1
    }
}

enum Swap {
    Pending,
    WindowCleared,
    NewWindowSet,
}

fn make_readers() -> [(ndk::media::image_reader::ImageReader, Arc<AtomicUsize>); 2] {
    use ndk::media::image_reader::{ImageFormat, ImageReader};
    let mut out = Vec::new();
    for _ in 0..2 {
        let mut reader = ImageReader::new(720, 480, ImageFormat::RGBA_8888, 8)
            .expect("ImageReader::new");
        let frames = Arc::new(AtomicUsize::new(0));
        let counter = frames.clone();
        reader
            .set_image_listener(Box::new(move |_reader| {
                counter.fetch_add(1, Ordering::SeqCst);
            }))
            .expect("set image listener");
        out.push((reader, frames));
    }
    [out.remove(0), out.remove(0)]
}

/// Demuxes the video track of an MP4 with oxideav-mp4.
fn load_h264(
    path: &str,
) -> Result<(Vec<Packet>, oxideav_core::CodecParameters, TimeBase), String> {
    let mut ctx = RuntimeContext::new();
    oxideav_mp4::__oxideav_entry(&mut ctx);
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut demuxer = ctx
        .containers
        .open_demuxer("mp4", Box::new(file), &ctx.codecs)
        .map_err(|e| e.to_string())?;
    let video = demuxer
        .streams()
        .iter()
        .find(|s| s.params.media_type == MediaType::Video)
        .ok_or("no video stream")?
        .clone();
    let mut packets = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) if p.stream_index == video.index => packets.push(p),
            Ok(_) => {}
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => return Err(format!("demux: {e}")),
        }
    }
    Ok((packets, video.params, video.time_base))
}

/// Packet pts in its stream time base, as a media `Duration`.
fn packet_media_time(pkt: &Packet, time_base: TimeBase) -> Duration {
    let pts = pkt.pts.unwrap_or(0);
    let num = time_base.0.num;
    let den = time_base.0.den;
    let micros = pts as i128 * 1_000_000 * num as i128 / den as i128;
    Duration::from_micros(micros.max(0) as u64)
}

// ---------------------------------------------------------------- audio

fn audio_probe() -> i32 {
    println!("[audio] starting 3 s sine probe");
    let backend = AndroidBackend::new();
    let mut audio = backend.audio();
    let (rate, ch) = (48000u32, 2u16);

    if let Err(e) = audio.open(rate, ch) {
        println!("[audio] FAIL: open: {e}");
        return 1;
    }

    // 500 ms of sine, written in chunks.
    let chunk_frames = (rate as usize / 2) * ch as usize;
    let mut chunk = Vec::with_capacity(chunk_frames);
    for f in 0..rate as usize / 2 {
        let t = f as f64 / rate as f64;
        let s = (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32 * 0.25;
        chunk.push(s);
        chunk.push(s);
    }

    let clock = audio.clock();
    audio.play();
    let clock_before = clock.now();
    let t0 = Instant::now();
    let mut written_frames = 0usize;
    for _ in 0..6 {
        let pts = Duration::from_secs_f64(written_frames as f64 / rate as f64);
        match audio.write(&chunk, pts) {
            Ok(n) => written_frames += n / ch as usize,
            Err(e) => {
                println!("[audio] FAIL: write: {e}");
                return 1;
            }
        }
    }
    std::thread::sleep(Duration::from_millis(400));
    let clock_after = clock.now();
    audio.pause();

    println!("[audio] clock before writes: {clock_before:?}");
    println!("[audio] clock after drain:    {clock_after:?}");
    audio.flush();

    let (Some(before), Some(after)) = (clock_before, clock_after) else {
        println!("[audio] FAIL: clock never became valid");
        return 1;
    };
    let advanced = (after - before).as_secs_f64();
    println!(
        "[audio] clock advanced: {advanced:.3} s over {:.2} s wall",
        t0.elapsed().as_secs_f64() + 0.4
    );
    if (2.5..3.6).contains(&advanced) {
        println!("[audio] PASS");
        0
    } else {
        println!("[audio] FAIL: expected ~3 s of clock advance");
        1
    }
}

/// Placeholder clock for the video probe; real sync comes from the audio
/// clock in the engine. Frames are presented immediately.
struct NullClock;
impl Clock for NullClock {
    fn now(&self) -> Option<Duration> {
        None
    }
    fn monotonic_ns_at(&self, _at: Duration) -> Option<i64> {
        None
    }
}
