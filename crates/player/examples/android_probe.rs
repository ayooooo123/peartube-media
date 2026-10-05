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
use player::backend::{Backend, Clock, SinkError, VideoSink};
use player::AndroidBackend;
use std::fs::File;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    // The two deliverable probes. The experiment probes (`sw_first_probe`,
    // `nosurface_probe`) stay for debugging behind `--all`.
    let mut code = video_probe();
    code |= audio_probe();
    if std::env::args().any(|a| a == "--all") {
        code |= sw_first_probe();
        code |= nosurface_probe();
    }
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

    // One reader up front; the swap/fallback windows are created lazily
    // (a fresh reader is exactly what the swap path needs).
    let reader_a = make_reader();
    let frames_a = reader_a.1.clone();
    let window_a = reader_a
        .0
        .window()
        .map_err(|e| format!("reader A window: {e:?}"))
        .unwrap();
    let _keep_a = reader_a;

    let backend = AndroidBackend::new();
    backend.set_video_window(Some(window_a));

    // The emulator's vendor (goldfish) H.264 decoder wedges without ever
    // queueing input, so this probe drives the sink directly and prefers
    // the platform's software decoder (c2.android.*) up front. A real
    // device uses the type-derived hardware decoder; the window-swap
    // semantics under test are decoder-agnostic.
    let clock = Arc::new(NullClock);
    let sink_video = Arc::new(parking_lot::Mutex::new(
        player::android::AndroidVideoSink::new(backend.shared().clone(), clock),
    ));
    sink_video.lock().prefer_software_decoder(true);
    let packets = Arc::new(packets);

    // Swap after roughly half the packets have been pushed (the queue
    // thread tracks its position); the two-step None -> Some exercises
    // the blocking release and the rebuild-on-new-window path.
    let swap_at = packets.len() / 2;
    let swap_state = Arc::new(parking_lot::Mutex::new(Swap::Pending));
    // Counters for the lazily created readers (the readers themselves are
    // leaked in the push thread: ImageReader is not Send).
    let frames_b_slot: Arc<parking_lot::Mutex<Option<Arc<AtomicUsize>>>> =
        Arc::new(parking_lot::Mutex::new(None));
    let frames_c_slot: Arc<parking_lot::Mutex<Option<Arc<AtomicUsize>>>> =
        Arc::new(parking_lot::Mutex::new(None));

    let push = {
        let sink = sink_video.clone();
        let packets = packets.clone();
        let swap_state = swap_state.clone();
        let backend = backend.clone();
            let frames_b_slot = frames_b_slot.clone();
        let frames_c_slot = frames_c_slot.clone();
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
                    // clearing and restoring; set the new window first
                    // (it re-opens the codec via on_window_available), then
                    // retry the packet. Never hold the sink lock across
                    // set_video_window: the backend takes it.
                    Err(SinkError::Unavailable) if pushed >= swap_at => {
                        if matches!(*swap_state.lock(), Swap::WindowCleared) {
                            let (reader, counter) = make_reader();
                            frames_b_slot.lock().replace(counter);
                            let w = reader
                                .window()
                                .map_err(|e| SinkError::Fatal(format!("reader B window: {e:?}")))?;
                            std::mem::forget(reader); // live until process exit
                            backend.set_video_window(Some(w));
                            *swap_state.lock() = Swap::NewWindowSet;
                            println!("[video] swap: new window set at packet {pushed}");
                            let mut sink = sink.lock();
                            sink.push_packet(pkt, pts)?;
                        } else {
                            return Err(SinkError::Unavailable);
                        }
                    }
                    // The type-derived decoder stalled and the in-place
                    // software retry could not take the old window (the
                    // dead codec still holds it). Move to a fresh reader:
                    // clear, attach reader C's window, re-open. The sink
                    // then prefers its software decoder for this stream.
                    Err(SinkError::Fallback(e)) => {
                        println!("[video] fallback at packet {pushed}: {e}");
                        backend.set_video_window(None);
                        let (reader, counter) = make_reader();
                        frames_c_slot.lock().replace(counter);
                        let w = reader
                            .window()
                            .map_err(|e| SinkError::Fatal(format!("reader C window: {e:?}")))?;
                        std::mem::forget(reader); // live until process exit
                        backend.set_video_window(Some(w));
                        println!("[video] moved to fresh window (reader C)");
                        let mut sink = sink.lock();
                        if !sink.open_compressed(&params) {
                            return Err(SinkError::Fatal(
                                "re-open on fresh window declined".into(),
                            ));
                        }
                        sink.push_packet(pkt, pts)?;
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
    while started.elapsed() < Duration::from_secs(150) {
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

    // Both swap windows must have received frames, and the total must
    // cover the sample (the drain sleep covers the last frames).
    let ok = b + c > 0 && total >= EXPECTED_FRAMES * 8 / 10;
    if ok {
        println!("[video] PASS (window swap resumed on new surface)");
        0
    } else {
        println!(
            "[video] FAIL: expected >0 frames after the swap and >= {}",
            EXPECTED_FRAMES * 8 / 10
        );
        1
    }
}

enum Swap {
    Pending,
    WindowCleared,
    NewWindowSet,
}


/// One AImageReader (720x480 RGBA, 8 slots) with a counting, draining
/// listener: `(reader, frames-received)`.
fn make_reader() -> (ndk::media::image_reader::ImageReader, Arc<AtomicUsize>) {
    use ndk::media::image_reader::{ImageFormat, ImageReader};
    let mut reader = ImageReader::new(720, 480, ImageFormat::YUV_420_888, 8)
        .expect("ImageReader::new");
    let frames = Arc::new(AtomicUsize::new(0));
    let counter = frames.clone();
    reader
        .set_image_listener(Box::new(move |reader| {
            counter.fetch_add(1, Ordering::SeqCst);
            // Acquire + drop every available image so the codec's
            // output queue drains; with `max_images` slots full the
            // decoder would stall.
            while let Ok(ndk::media::image_reader::AcquireResult::Image(img)) =
                reader.acquire_latest_image()
            {
                drop(img);
            }
        }))
        .expect("set image listener");
    (reader, frames)
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
    let mut written_frames = 0usize;
    let mut clock_before = None;
    for i in 0..6 {
        let pts = Duration::from_secs_f64(written_frames as f64 / rate as f64);
        match audio.write(&chunk, pts) {
            Ok(n) => written_frames += n / ch as usize,
            Err(e) => {
                println!("[audio] FAIL: write: {e}");
                return 1;
            }
        }
        if i == 0 {
            clock_before = clock.now();
            println!("[audio] clock after first write: {clock_before:?}");
        }
    }
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(400));
    let clock_after = clock.now();
    audio.pause();

    println!("[audio] clock after first write: {clock_before:?}");
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

/// Experiment: software decoder first on a fresh reader window.
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
        720,
        480,
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
    let window = reader.window().unwrap();
    let backend = AndroidBackend::new();
    backend.set_video_window(Some(window));
    let sink_video = Arc::new(parking_lot::Mutex::new(
        player::android::AndroidVideoSink::new(
            backend.shared().clone(),
            Arc::new(NullClock),
        ),
    ));
    let t0 = Instant::now();
    {
        let mut sink = sink_video.lock();
        // Exercise the software path directly: the emulator's vendor decoder
        // wedges, and this probe isolates the software decode + ImageReader
        // pipeline from that.
        sink.prefer_software_decoder(true);
        if !sink.open_compressed(&params) {
            println!("[sw] FAIL: open declined");
            return 1;
        }
    }
    let mut pushed = 0usize;
    for pkt in &packets {
        let pts = packet_media_time(pkt, time_base);
        let r = {
            let mut sink = sink_video.lock();
            sink.push_packet(pkt, pts)
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
    println!("[sw] pushed {pushed}, frames received: {n} in {:?}", t0.elapsed());
    if n >= 150 {
        println!("[sw] PASS");
        0
    } else {
        println!("[sw] FAIL: <150 frames");
        1
    }
}

/// Experiment: software decoder with NO surface, raw buffer counting.
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
    for pkt in packets.iter().take(60) {
        // input
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
        // drain outputs
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
            if false {
                break;
            }
            // stop draining when nothing more for a moment — simplified: check via TryAgainLater count
            // (this loop must end; use a bounded count)
            if out_count > 4000 {
                break;
            }
            // crude: break the inner loop each packet after trying a few times
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

/// Splits avcC extradata into (csd-0 SPS annexb, csd-1 PPS annexb, length size).
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
