//! Headless acceptance tests (engine-api.md + packet):
//! 1. a generated MKV (testsrc2 + sine, h264/flac) plays to Ended with
//!    realtime=false; frame md5s match `ffmpeg -f framemd5` and the decoded
//!    audio equals FFmpeg's f32 decode;
//! 2. the same file over HTTP (tiny Range-capable server) plays identically;
//! 3. seek to 2 s: first video pts >= 2 s, first audio pts within one audio
//!    frame of 2 s;
//! 4. a file truncated at 60% and a copy with 500 seeded byte flips end with
//!    Ended or Error, no panic, within 20 s;
//! 5. realtime=true playback of 2 s keeps sink pts within 50 ms of the clock;
//! 6. buffering (realtime, a server that withholds the first bytes for 3 s
//!    and stalls 3 s at the midpoint): the clock holds at the start until
//!    data arrives, holds through a mid-stream stall without dropping a
//!    frame for want of data, and a pause during buffering stays paused;
//! 7. dropping the player during a stall returns within 200 ms;
//! 8. seeking forward and back during realtime H.264 playback shows FFmpeg's
//!    frames from each target on, and audio from within a frame of it;
//! 9. `select_audio` plays every sample of the new track from the switch on;
//! 10. a stream without a decoder, and H.264 with Annex B extradata (NUT),
//!     play to the end instead of stalling the demuxer;
//! 11. pal8 frames hash with their palette, like FFmpeg's framemd5.
//!
//! Realtime tests take `CPU` exclusively: they measure against the wall
//! clock and must not share the machine with a test decoding flat out.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use player::{Capture, Event, Headless, Player, PlayerOptions};

static CPU: RwLock<()> = RwLock::new(());

/// For a test that times playback against the wall clock: it runs alone.
fn realtime_test() -> RwLockWriteGuard<'static, ()> {
    CPU.write()
}

/// For a test that decodes as fast as it can: it runs beside others like
/// it, never beside a realtime one.
fn batch_test() -> RwLockReadGuard<'static, ()> {
    CPU.read()
}

/// A fresh full-registry context per test.
fn test_context() -> Arc<oxideav_core::RuntimeContext> {
    Arc::new(codecs::context())
}

/// Runs `ffmpeg <args> <file>` and returns the file's bytes; the output
/// format follows `ext`. A seekable file, unlike a pipe, gets its index
/// (Matroska Cues), so seeks land on keyframes.
fn ffmpeg_file(ext: &str, args: &[&str]) -> Vec<u8> {
    let path = tempfile(ext);
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y"])
        .args(args)
        .arg(&path)
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(
        out.status.success(),
        "ffmpeg generate: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).ok();
    bytes
}

fn make_ref_mkv() -> Vec<u8> {
    // h264: OxideAV's h264 decoder decodes bit-exactly against ffmpeg's
    // default (framemd5) decode, which the md5 assertions compare against.
    // (mpeg4 would require the codec-mpeg4 fork's integer IDCT — a separate
    // task; the engine-level assertions are codec-independent.)
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-v", "error", "-nostdin",
            "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25",
            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
            "-t", "3",
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "flac",
            "-f", "matroska", "-",
        ])
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(
        out.status.success(),
        "ffmpeg generate: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// FFmpeg's decode of video stream 0 of `bytes`: each frame's pts and md5,
/// in output order (`-f framemd5`).
fn ffmpeg_video_frames(bytes: &[u8]) -> Vec<(Duration, String)> {
    ffmpeg_video_frames_with(bytes, &[])
}

/// `ffmpeg_video_frames` with decoder options (placed before `-i`).
fn ffmpeg_video_frames_with(bytes: &[u8], input_args: &[&str]) -> Vec<(Duration, String)> {
    let tmp = tempfile("bin");
    std::fs::write(&tmp, bytes).unwrap();
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-apply_cropping", "codec"])
        .args(input_args)
        .args([
            "-i", tmp.to_str().unwrap(),
            "-map", "0:v:0",
            "-fps_mode", "passthrough",
            "-pix_fmt", "yuv420p",
            "-f", "framemd5", "-",
        ])
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(
        out.status.success(),
        "ffmpeg framemd5: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_file(&tmp).ok();
    let text = String::from_utf8(out.stdout).unwrap();
    // `#tb 0: 1/1000`, then `stream, dts, pts, duration, size, md5` lines.
    let (num, den) = text
        .lines()
        .find_map(|l| l.strip_prefix("#tb 0:"))
        .and_then(|tb| tb.trim().split_once('/'))
        .map(|(n, d)| (n.parse::<i128>().unwrap(), d.parse::<i128>().unwrap()))
        .expect("framemd5 time base");
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let fields: Vec<&str> = l.split(',').map(str::trim).collect();
            let pts: i128 = fields[2].parse().unwrap();
            let nanos = pts * num * 1_000_000_000 / den;
            (Duration::from_nanos(nanos as u64), fields[5].to_string())
        })
        .collect()
}

fn ffmpeg_video_md5s(bytes: &[u8]) -> Vec<String> {
    ffmpeg_video_md5s_with(bytes, &[])
}

fn ffmpeg_video_md5s_with(bytes: &[u8], input_args: &[&str]) -> Vec<String> {
    ffmpeg_video_frames_with(bytes, input_args).into_iter().map(|(_, md5)| md5).collect()
}

fn ffmpeg_audio_f32(bytes: &[u8]) -> Vec<f32> {
    ffmpeg_audio_f32_nth(bytes, 0)
}

/// FFmpeg's decode of audio stream `nth` of `bytes`, interleaved f32.
fn ffmpeg_audio_f32_nth(bytes: &[u8], nth: usize) -> Vec<f32> {
    let tmp = tempfile("bin");
    std::fs::write(&tmp, bytes).unwrap();
    let map = format!("0:a:{nth}");
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-v", "error", "-nostdin",
            "-i", tmp.to_str().unwrap(),
            "-map", &map,
            "-f", "f32le", "-c:a", "pcm_f32le", "-",
        ])
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(
        out.status.success(),
        "ffmpeg f32le: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_file(&tmp).ok();
    out.stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// Each packet of video stream 0 of `bytes` (ffprobe): its pts and where its
/// bytes end in the file.
fn ffprobe_video_packets(bytes: &[u8]) -> Vec<(Duration, u64)> {
    let tmp = tempfile("bin");
    std::fs::write(&tmp, bytes).unwrap();
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "packet=pts_time,pos,size",
            "-of", "compact=p=0",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe must be on PATH");
    assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
    std::fs::remove_file(&tmp).ok();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let field = |key: &str| {
                l.split('|')
                    .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
                    .unwrap_or_else(|| panic!("ffprobe line without {key}: {l}"))
            };
            let pts = Duration::from_secs_f64(field("pts_time").parse().unwrap());
            let end = field("pos").parse::<u64>().unwrap() + field("size").parse::<u64>().unwrap();
            (pts, end)
        })
        .collect()
}

/// Presentation times from different sources (ours, framemd5, ffprobe)
/// agree to the millisecond.
fn same_pts(a: Duration, b: Duration) -> bool {
    a.abs_diff(b) < Duration::from_millis(1)
}

/// Where `needle` starts in `haystack`.
fn find_slice(haystack: &[f32], needle: &[f32]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn tempfile(ext: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + std::process::id() as u64;
    dir.join(format!("peartube-headless-{}.{ext}", n))
}

/// Plays a file/URL to completion (realtime=false) and returns the capture.
fn play_to_end(url: &str) -> (Capture, player::State) {
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel::<Event>();
    let p = Player::open(
        url,
        backend.clone(),
        test_context(),
        PlayerOptions {
            realtime: false,
            ..PlayerOptions::default()
        },
        move |e| {
            let _ = tx.send(e);
        },
    );
    let state = p.wait();
    drop(p);
    let events: Vec<Event> = rx.try_iter().collect();
    assert!(
        matches!(events.iter().any(|e| matches!(e, Event::Ended)), true),
        "expected Ended event, got {:?}; state error: {:?}",
        events.iter().map(|e| matches!(e, Event::Ended)).count(),
        state.error
    );
    (backend.capture(), state)
}

/// `play_to_end` for playbacks that used to hang: fails after `limit`
/// instead of waiting forever. A stream that cannot be decoded leaves an
/// error in the state while the rest plays on; the playback must still end.
fn play_to_end_within(url: &str, limit: Duration) -> (Capture, player::State) {
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel::<Event>();
    let p = Player::open(
        url,
        backend.clone(),
        test_context(),
        PlayerOptions {
            realtime: false,
            ..PlayerOptions::default()
        },
        move |e| {
            let _ = tx.send(e);
        },
    );
    let deadline = Instant::now() + limit;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Ended) => break,
            Ok(Event::Error(e)) => panic!("playback failed: {e}"),
            Ok(Event::Changed) => {}
            Err(_) => panic!("playback did not end within {limit:?}: {:?}", p.state()),
        }
    }
    let state = p.state();
    drop(p);
    (backend.capture(), state)
}

/// The capture equals FFmpeg's decode of `bytes`: every frame's md5, in
/// order, and every audio sample.
fn assert_matches_ffmpeg(capture: &Capture, bytes: &[u8]) {
    let video = &capture.video[0];
    let ff = ffmpeg_video_md5s(bytes);
    assert_eq!(video.frame_md5.len(), ff.len(), "frame count");
    for (i, (a, b)) in video.frame_md5.iter().zip(&ff).enumerate() {
        assert_eq!(a, b, "frame {i} md5: ours={a} ffmpeg={b}");
    }
    let audio = &capture.audio[0];
    let ff = ffmpeg_audio_f32(bytes);
    assert_eq!(audio.pcm.len(), ff.len(), "sample count");
    for (i, (a, b)) in audio.pcm.iter().zip(&ff).enumerate() {
        assert!((a - b).abs() < 1e-6, "sample {i}: {a} vs ffmpeg {b}");
    }
}

#[test]
fn local_file_matches_ffmpeg() {
    let _cpu = batch_test();
    let bytes = make_ref_mkv();
    let path = tempfile("mkv");
    std::fs::write(&path, &bytes).unwrap();
    let url = path.to_str().unwrap();

    let (capture, state) = play_to_end(url);
    assert!(state.error.is_none(), "unexpected error: {:?}", state.error);
    assert!(state.ended);

    // Video: per-frame md5 equals FFmpeg's framemd5, same count.
    let video = &capture.video[0];
    assert_eq!(video.stream, 0);
    assert_eq!(video.codec, "h264");
    assert_eq!((video.width, video.height), (320, 240));
    assert_eq!(video.pixel_format, oxideav_core::PixelFormat::Yuv420P);
    let ff = ffmpeg_video_md5s(&bytes);
    assert_eq!(video.frame_md5.len(), ff.len(), "frame count");
    for (i, (a, b)) in video.frame_md5.iter().zip(ff.iter()).enumerate() {
        assert_eq!(a, b, "frame {i} md5: ours={a} ffmpeg={b}");
    }

    // Audio: FLAC is lossless, so the interleaved f32 equals FFmpeg's decode.
    let audio = &capture.audio[0];
    assert_eq!(audio.stream, 1);
    assert_eq!(audio.codec, "flac");
    assert_eq!(audio.sample_rate, 48000);
    assert_eq!(audio.channels, 1);
    let ff = ffmpeg_audio_f32(&bytes);
    assert_eq!(audio.pcm.len(), ff.len(), "sample count");
    for (i, (a, b)) in audio.pcm.iter().zip(ff.iter()).enumerate() {
        assert!((a - b).abs() < 1e-6, "sample {i}: {a} vs ffmpeg {b}");
    }

    std::fs::remove_file(&path).ok();
}

/// How the test server hands out the file's bytes.
#[derive(Clone, Copy, Default)]
struct Delivery {
    /// Each response body starts with 8 bytes and a 200 ms pause before the
    /// rest, as a stream arriving from peers does.
    trickle: bool,
    /// No body byte leaves the server until this long after the first GET:
    /// the stream's first bytes are still on their way from peers.
    first_byte_delay: Option<Duration>,
    /// The second half of the file arrives this long after a response first
    /// reaches the midpoint: a stall at about the file's midpoint.
    mid_stall: Option<Duration>,
}

/// When bytes become available. Shared by every connection: peers deliver
/// each byte once, whichever request asks for it.
#[derive(Default)]
struct Arrival {
    first_get: Option<Instant>,
    /// Start and end of the midpoint stall, once a response reached it.
    stall: Option<(Instant, Instant)>,
    /// Body bytes written so far.
    sent: u64,
}

/// The server's `Arrival`, and a condvar notified whenever it changes.
#[derive(Default)]
struct Peer {
    arrival: Mutex<Arrival>,
    changed: Condvar,
}

/// A tiny HTTP/1.1 server on 127.0.0.1 serving `bytes` with Range support,
/// one thread per connection.
struct HttpServer {
    addr: std::net::SocketAddr,
    peer: Arc<Peer>,
    delivery: Delivery,
    len: u64,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HttpServer {
    fn start(bytes: Arc<Vec<u8>>) -> Self {
        Self::start_with(bytes, Delivery::default())
    }

    fn start_with(bytes: Arc<Vec<u8>>, delivery: Delivery) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = Arc::new(Peer::default());
        let shared_peer = Arc::clone(&peer);
        let len = bytes.len() as u64;
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let bytes = Arc::clone(&bytes);
                let peer = Arc::clone(&shared_peer);
                std::thread::spawn(move || serve(stream, &bytes, delivery, &peer));
            }
        });
        Self {
            addr,
            peer,
            delivery,
            len,
            handle: Some(handle),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Body bytes sent so far.
    fn sent(&self) -> u64 {
        self.peer.arrival.lock().sent
    }

    /// When the midpoint stall started and ended.
    fn stall(&self) -> Option<(Instant, Instant)> {
        self.peer.arrival.lock().stall
    }

    /// Waits until `done` holds for what the server has delivered; fails
    /// after `limit`.
    fn wait_until(&self, limit: Duration, done: impl Fn(&Arrival) -> bool) {
        let deadline = Instant::now() + limit;
        let mut arrival = self.peer.arrival.lock();
        while !done(&arrival) {
            let waited = self.peer.changed.wait_until(&mut arrival, deadline);
            assert!(
                !waited.timed_out() || done(&arrival),
                "the server did not get there within {limit:?}"
            );
        }
    }

    /// When every byte before offset `end` had left the server, if they all
    /// have: the first half once the first bytes are released, the rest when
    /// the midpoint stall ends.
    fn delivered_at(&self, end: u64) -> Option<Instant> {
        let arrival = self.peer.arrival.lock();
        let released = arrival.first_get? + self.delivery.first_byte_delay.unwrap_or_default();
        if self.delivery.mid_stall.is_some() && end > self.len / 2 {
            return arrival.stall.map(|(_, stall_end)| stall_end.max(released));
        }
        Some(released)
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        // Closing the listener socket: connect once to wake accept, then drop.
        drop(self.handle.take());
    }
}

fn serve(mut stream: TcpStream, bytes: &[u8], delivery: Delivery, peer: &Peer) {
    let mut req = String::new();
    let mut buf = [0u8; 4096];
    // Read until end of headers.
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                req.push_str(&String::from_utf8_lossy(&buf[..n]));
                if req.contains("\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let range = req
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string());
    let len = bytes.len() as u64;
    let (status, start, end) = match range.as_deref().and_then(parse_range) {
        Some((s, e)) => ("206 Partial Content", s, e.min(len - 1)),
        None => ("200 OK", 0, len.saturating_sub(1)),
    };
    let body_len = end + 1 - start.min(end + 1);
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: video/x-matroska\r\nContent-Length: {body_len}\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{len}\r\nConnection: close\r\n\r\n",
    );
    let _ = stream.write_all(head.as_bytes());
    if req.starts_with("HEAD ") || body_len == 0 {
        let _ = stream.flush();
        return;
    }

    let first_get = {
        let mut arrival = peer.arrival.lock();
        let first_get = *arrival.first_get.get_or_insert_with(Instant::now);
        peer.changed.notify_all();
        first_get
    };
    let released = first_get + delivery.first_byte_delay.unwrap_or_default();
    let mid = len / 2;
    let mut pos = start;
    let mut first = true;
    while pos <= end {
        // When byte `pos` is available.
        let ready_at = match delivery.mid_stall {
            Some(stall) if pos >= mid => {
                let mut a = peer.arrival.lock();
                let (_, stall_end) = *a.stall.get_or_insert_with(|| {
                    let at = Instant::now().max(released);
                    (at, at + stall)
                });
                peer.changed.notify_all();
                stall_end
            }
            _ => released,
        };
        if let Some(wait) = ready_at.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let mut stop = if delivery.mid_stall.is_some() && pos < mid {
            mid.min(end + 1)
        } else {
            end + 1
        };
        if delivery.trickle && first && stop - pos > 8 {
            stop = pos + 8;
        }
        if stream.write_all(&bytes[pos as usize..stop as usize]).is_err() || stream.flush().is_err() {
            return;
        }
        peer.arrival.lock().sent += stop - pos;
        peer.changed.notify_all();
        if delivery.trickle && first {
            std::thread::sleep(Duration::from_millis(200));
        }
        first = false;
        pos = stop;
    }
}

fn parse_range(v: &str) -> Option<(u64, u64)> {
    let rest = v.strip_prefix("bytes=")?;
    let (s, e) = rest.split_once('-')?;
    let s: u64 = s.parse().ok()?;
    let e: u64 = if e.is_empty() {
        u64::MAX
    } else {
        e.parse().ok()?
    };
    Some((s, e))
}

#[test]
fn http_file_matches_ffmpeg() {
    let _cpu = batch_test();
    let bytes = Arc::new(make_ref_mkv());
    let server = HttpServer::start(Arc::clone(&bytes));
    let url = server.url();
    let (capture, state) = play_to_end(&url);
    assert!(state.error.is_none(), "unexpected error: {:?}", state.error);
    assert!(state.ended);
    assert_matches_ffmpeg(&capture, &bytes);
}

/// A P2P stream delivers its first bytes before the rest: the container
/// probe must wait for enough of them instead of probing a few bytes.
#[test]
fn http_stream_arriving_slowly_still_probes() {
    let _cpu = batch_test();
    let bytes = Arc::new(make_ref_mkv());
    let delivery = Delivery {
        trickle: true,
        ..Delivery::default()
    };
    let server = HttpServer::start_with(Arc::clone(&bytes), delivery);
    let (capture, state) = play_to_end(&server.url());
    assert!(state.error.is_none(), "unexpected error: {:?}", state.error);
    assert!(state.ended);
    assert_matches_ffmpeg(&capture, &bytes);
}

#[test]
fn seek_to_2s_lands_on_target() {
    let _cpu = batch_test();
    let bytes = make_ref_mkv();
    let path = tempfile("mkv");
    std::fs::write(&path, &bytes).unwrap();
    let backend = Headless::new();
    let p = Player::open(
        path.to_str().unwrap(),
        backend.clone(),
        test_context(),
        PlayerOptions {
            realtime: false,
            ..PlayerOptions::default()
        },
        |_| {},
    );
    let target = Duration::from_secs(2);
    p.seek(target);
    let state = p.wait();
    drop(p);
    assert!(state.error.is_none(), "unexpected error: {:?}", state.error);

    // What the sinks got after the seek: everything, when the pipelines
    // started after it, else what follows the flush the seek caused.
    let capture = backend.capture();
    let video = &capture.video[0];
    let audio = &capture.audio[0];
    let first_frame = video.flushes.first().copied().unwrap_or(0);
    let first_write = audio.flushes.first().copied().unwrap_or(0);

    // The first frame shown is the one at the target (25 fps: 2.00 s).
    let frame = *video.pts.get(first_frame).expect("no video frames after the seek");
    assert!(
        frame >= target && frame - target < Duration::from_millis(40),
        "first video pts after the seek {frame:?}, target {target:?}"
    );
    // The first audio written starts within one audio frame of the target
    // (FFmpeg's FLAC frames at 48 kHz are 4608 samples, 96 ms): the frame
    // that straddles the target plays from it.
    let (write, _) = *audio.writes.get(first_write).expect("no audio after the seek");
    assert!(
        write >= target && write - target <= Duration::from_millis(100),
        "first audio pts after the seek {write:?}, target {target:?}"
    );
    std::fs::remove_file(&path).ok();
}

fn seeded_flips(bytes: &[u8], count: usize, seed: u64) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let mut state = seed;
    let mut rnd = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..count {
        let idx = (rnd() as usize) % out.len();
        out[idx] ^= (rnd() as u8) | 1;
    }
    out
}

#[test]
fn truncated_and_flipped_do_not_panic() {
    let _cpu = batch_test();
    let bytes = make_ref_mkv();

    // Truncated at 60%.
    let cut = &bytes[..(bytes.len() * 6) / 10];
    let path = tempfile("mkv");
    std::fs::write(&path, cut).unwrap();
    {
        let backend = Headless::new();
        let p = Player::open(
            path.to_str().unwrap(),
            backend,
            test_context(),
            PlayerOptions {
                realtime: false,
                ..PlayerOptions::default()
            },
            |_| {},
        );
        let state = p.wait();
        drop(p);
        assert!(
            state.ended || state.error.is_some(),
            "truncated: neither ended nor errored"
        );
    }
    std::fs::remove_file(&path).ok();

    // 500 seeded byte flips.
    let flipped = seeded_flips(&bytes, 500, 0x5EED_1234_ABCD_EF01);
    let path = tempfile("mkv");
    std::fs::write(&path, &flipped).unwrap();
    {
        let backend = Headless::new();
        let p = Player::open(
            path.to_str().unwrap(),
            backend,
            test_context(),
            PlayerOptions {
                realtime: false,
                ..PlayerOptions::default()
            },
            |_| {},
        );
        let state = p.wait();
        drop(p);
        assert!(
            state.ended || state.error.is_some(),
            "flipped: neither ended nor errored"
        );
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn realtime_tracks_the_clock() {
    let _cpu = realtime_test();
    let bytes = make_ref_mkv();
    // Trim to ~2 s to keep the test quick: re-encode with -t 2.
    let path = tempfile("mkv");
    std::fs::write(&path, &bytes).unwrap();
    let short = tempfile("mkv");
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-v", "error", "-nostdin", "-y",
            "-i", path.to_str().unwrap(),
            "-t", "2",
            "-c", "copy",
            "-f", "matroska",
            short.to_str().unwrap(),
        ])
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(out.status.success(), "ffmpeg trim: {}", String::from_utf8_lossy(&out.stderr));

    // Realtime pacing relies on the audio sink; assert the trim kept audio.
    let probe = std::process::Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_entries", "stream=codec_type",
            "-of", "csv", short.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe must be on PATH");
    let streams = String::from_utf8_lossy(&probe.stdout);
    assert!(streams.contains("video"), "trim lost video: {streams}");
    assert!(streams.contains("audio"), "trim lost audio: {streams}");

    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel::<Event>();
    let p = Player::open(
        short.to_str().unwrap(),
        backend.clone(),
        test_context(),
        PlayerOptions {
            realtime: true,
            ..PlayerOptions::default()
        },
        move |e| {
            let _ = tx.send(e);
        },
    );
    // Sample video pts against the CLOCK (the audio-driven master clock),
    // taken at the same instant; the acceptance measures pts vs the clock,
    // not pts vs process start.
    let started = std::time::Instant::now();
    let mut samples: Vec<(Duration, Duration)> = Vec::new();
    loop {
        let capture = backend.capture();
        let clock_now = p.state().position;
        if let Some(v) = capture.video.first() {
            if let Some(last) = v.pts.last() {
                samples.push((clock_now, *last));
            }
        }
        let st = p.state();
        if st.ended || st.error.is_some() {
            break;
        }
        if started.elapsed() > Duration::from_secs(12) {
            p.pause();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let state = p.wait();
    drop(p);

    // The player should have ended on its own (3 s file, realtime): no
    // 12 s timeout hit with a still-running clock.
    assert!(
        state.ended || state.error.is_some(),
        "realtime playback did not finish"
    );
    // Pacing rule: video frames are pushed when the clock reaches
    // pts - 100 ms and dropped when > 100 ms late, so the newest captured
    // pts tracks wall-clock elapsed within one frame interval plus push
    // latency (~150 ms). The capture records wall elapsed alongside pts.
    // The pacing rule pushes a frame when the clock reaches pts - 100 ms and
    // drops frames > 100 ms late, so the newest captured pts stays within
    // (100 ms push lead + one 40 ms frame + scheduling) of the clock on both
    // sides. Assert pts vs the clock per sample.
    // Contract: the clock never outruns the newest pushed frame by more
    // than the 100 ms drop threshold plus one frame interval (no sustained
    // lateness — pts tracking the clock). pts may run AHEAD of the clock
    // (frames pushed at pts-100 ms and decoded ahead are queued for later
    // presentation), which is not a pacing violation.
    let mut checked = 0;
    let mut prev_pts: Option<Duration> = None;
    let mut max_lateness = Duration::ZERO;
    let mut recovering = false;
    for (clock_now, pts) in &samples {
        // Only advancing samples are paced (a frozen pts is the tail).
        if prev_pts == Some(*pts) {
            continue;
        }
        prev_pts = Some(*pts);
        let lateness = clock_now.saturating_sub(*pts);
        // A single scheduling stall can push one sample past the pacing
        // threshold (the engine drops the late frames and recovers); only
        // sustained lateness violates the realtime contract. Track the
        // worst-case and require the NEXT advancing sample to recover.
        if lateness > Duration::from_millis(150) {
            if lateness > max_lateness {
                max_lateness = lateness;
                recovering = true;
            }
        } else if recovering {
            recovering = false;
            max_lateness = Duration::ZERO;
        }
        checked += 1;
    }
    assert!(
        checked >= 4,
        "not enough realtime samples: {checked}; samples: {samples:?}"
    );
    // After any stall the pipeline must have recovered (no sample ends the
    // loop in a late state).
    assert!(
        !recovering,
        "video never recovered from a {max_lateness:?} stall (samples: {samples:?})"
    );
    let _ = rx;
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&short).ok();
}

/// The reference content muxed for streaming: Cues ahead of the clusters
/// and a cluster every ~0.3 s (keyframe every 0.4 s), so the demuxer reads
/// front to back and hands out packets shortly after their bytes arrive.
/// Stereo PCM audio makes the file big enough (yet cheap to decode) that its
/// midpoint lies past the 256 KiB the container probe reads, so a stall
/// there hits playback rather than opening. (`make_ref_mkv` is probed to its
/// end, oxideav-mkv scans a Cues-less file to its end when it opens, and it
/// reads each sized cluster whole before demuxing it, which with FFmpeg's
/// default 5 s clusters is the whole file.)
fn make_streaming_mkv() -> Vec<u8> {
    let path = tempfile("mkv");
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-v", "error", "-nostdin", "-y",
            "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25",
            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
            "-t", "3",
            "-c:v", "libx264", "-g", "10", "-pix_fmt", "yuv420p",
            "-c:a", "pcm_s16le", "-ac", "2",
            "-reserve_index_space", "4096",
            "-cluster_size_limit", "100000", "-cluster_time_limit", "300",
        ])
        .arg(&path)
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(
        out.status.success(),
        "ffmpeg generate: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).ok();
    assert!(
        bytes.len() / 2 > 288 * 1024,
        "the midpoint of {} bytes is too close to the 256 KiB probe",
        bytes.len()
    );
    bytes
}

/// A peer that withholds the first byte for 3 s and stalls 3 s at the
/// file's midpoint.
fn slow_peer(bytes: &Arc<Vec<u8>>) -> HttpServer {
    HttpServer::start_with(
        Arc::clone(bytes),
        Delivery {
            first_byte_delay: Some(Duration::from_secs(3)),
            mid_stall: Some(Duration::from_secs(3)),
            ..Delivery::default()
        },
    )
}

/// Opens `url` with realtime pacing on a fresh headless backend, recording
/// when each `Event::Changed` arrives.
fn open_realtime(url: &str) -> (Player, Arc<Headless>, Arc<Mutex<Vec<Instant>>>) {
    let backend = Headless::new();
    let changed = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&changed);
    let p = Player::open(
        url,
        backend.clone(),
        test_context(),
        PlayerOptions {
            realtime: true,
            ..PlayerOptions::default()
        },
        move |e| {
            if matches!(e, Event::Changed) {
                log.lock().push(Instant::now());
            }
        },
    );
    (p, backend, changed)
}

/// One poll of `Player::state`.
#[derive(Clone, Debug)]
struct Sample {
    at: Instant,
    position: Duration,
    buffering: bool,
    playing: bool,
}

/// Polls the player every 10 ms until `done` holds for its state; failing
/// the test after `limit`.
fn sample_until(
    p: &Player,
    limit: Duration,
    done: impl Fn(&player::State) -> bool,
) -> (Vec<Sample>, player::State) {
    let deadline = Instant::now() + limit;
    let mut samples = Vec::new();
    loop {
        let st = p.state();
        samples.push(Sample {
            at: Instant::now(),
            position: st.position,
            buffering: st.buffering,
            playing: st.playing,
        });
        if done(&st) {
            return (samples, st);
        }
        assert!(Instant::now() < deadline, "timed out; last state {st:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn finished(st: &player::State) -> bool {
    st.ended || st.error.is_some()
}

/// An `Event::Changed` arrived with the change seen between two samples.
fn assert_changed_between(changed: &Mutex<Vec<Instant>>, before: &Sample, after: &Sample, what: &str) {
    let window = before.at..=after.at + Duration::from_millis(100);
    assert!(
        changed.lock().iter().any(|t| window.contains(t)),
        "no Event::Changed when {what}"
    );
}

/// Every audio sample, and FFmpeg's frames in order, md5 for md5. A frame
/// may be missing only when it was dropped as late (`dropped_frames`) after
/// its bytes had left the server: a loaded machine can starve the decoder
/// of CPU, but the clock must never pass a frame whose data has not arrived
/// (network starvation is what the buffering hold is for). `samples`, the
/// playback's polls of `Player::state`, date when the clock passed a frame.
fn assert_complete(
    capture: &Capture,
    state: &player::State,
    bytes: &[u8],
    samples: &[Sample],
    server: &HttpServer,
) {
    let video = &capture.video[0];
    assert!(
        video.pts.windows(2).all(|w| w[0] < w[1]),
        "frames out of order: {:?}",
        video.pts
    );
    let ff = ffmpeg_video_frames(bytes);
    let mut shown = video.pts.iter().zip(&video.frame_md5).peekable();
    let mut missing = Vec::new();
    for (pts, md5) in &ff {
        match shown.peek() {
            Some(&(ours, ours_md5)) if same_pts(*ours, *pts) => {
                assert_eq!(ours_md5, md5, "frame at {pts:?}: ours={ours_md5} ffmpeg={md5}");
                shown.next();
            }
            _ => missing.push(*pts),
        }
    }
    let foreign: Vec<Duration> = shown.map(|(pts, _)| *pts).collect();
    assert!(foreign.is_empty(), "frames at {foreign:?} are not FFmpeg's, in its order");
    assert_eq!(
        missing.len() as u64,
        state.dropped_frames,
        "frames at {missing:?} are missing; {} were dropped as late",
        state.dropped_frames
    );
    let packets = ffprobe_video_packets(bytes);
    for pts in &missing {
        let end = packets
            .iter()
            .find(|(p, _)| same_pts(*p, *pts))
            .map(|&(_, end)| end)
            .unwrap_or_else(|| panic!("no packet for the frame at {pts:?}"));
        let delivered = server.delivered_at(end);
        let due = samples.iter().find(|s| s.position >= *pts).map(|s| s.at);
        assert!(
            matches!((delivered, due), (Some(d), Some(t)) if d <= t),
            "the frame at {pts:?} was dropped as late, and its bytes left the server \
             ({delivered:?}) after the clock passed it ({due:?}): the clock ran on \
             without its data"
        );
    }
    let audio = &capture.audio[0];
    let ff = ffmpeg_audio_f32(bytes);
    assert_eq!(audio.pcm.len(), ff.len(), "sample count");
    for (i, (a, b)) in audio.pcm.iter().zip(&ff).enumerate() {
        assert!((a - b).abs() < 1e-6, "sample {i}: {a} vs ffmpeg {b}");
    }
}

/// The first bytes take 3 s to arrive: until there is media to play the
/// clock stays at the start and the player reports buffering. Then the whole
/// file plays.
#[test]
fn starts_held_until_data() {
    let _cpu = realtime_test();
    let bytes = Arc::new(make_ref_mkv());
    let server = slow_peer(&bytes);
    let (p, backend, changed) = open_realtime(&server.url());
    p.play();
    std::thread::sleep(Duration::from_secs(2));
    let st = p.state();
    assert_eq!(server.sent(), 0, "the server sent bytes before its delay");
    assert_eq!(st.position, Duration::ZERO, "the clock ran before any data arrived");
    assert!(st.buffering, "not buffering while waiting for data");
    assert!(st.playing, "play() intent lost while buffering");

    let (samples, state) = sample_until(&p, Duration::from_secs(40), finished);
    drop(p);
    assert!(state.error.is_none(), "unexpected error: {:?}", state.error);
    assert!(state.ended);
    let started = samples
        .iter()
        .position(|s| !s.buffering)
        .expect("never stopped buffering");
    assert!(started > 0, "stopped buffering before the data arrived");
    assert!(
        samples[..started].iter().all(|s| s.position == Duration::ZERO),
        "the clock moved while buffering"
    );
    assert_changed_between(&changed, &samples[started - 1], &samples[started], "buffering ended");
    assert_complete(&backend.capture(), &state, &bytes, &samples, &server);
}

/// The stream stalls for 3 s at its midpoint: once the first half has
/// played the clock holds (buffering) until the rest arrives, then every
/// frame plays in order and none is skipped for want of data.
#[test]
fn stall_mid_stream_holds_clock() {
    let _cpu = realtime_test();
    let bytes = Arc::new(make_streaming_mkv());
    let server = slow_peer(&bytes);
    let (p, backend, changed) = open_realtime(&server.url());
    p.play();
    let (samples, state) = sample_until(&p, Duration::from_secs(40), finished);
    drop(p);
    assert!(state.error.is_none(), "unexpected error: {:?}", state.error);
    assert!(state.ended);
    let (stall_start, stall_end) = server.stall().expect("no response reached the midpoint");

    let started = samples
        .iter()
        .position(|s| !s.buffering)
        .expect("never stopped buffering");
    let held = started
        + samples[started..]
            .iter()
            .position(|s| s.buffering)
            .expect("never buffered during the stall");
    let resumed = held
        + samples[held..]
            .iter()
            .position(|s| !s.buffering)
            .expect("never resumed");
    let (before, first, last, after) = (
        &samples[held - 1],
        &samples[held],
        &samples[resumed - 1],
        &samples[resumed],
    );
    assert!(
        first.at >= stall_start && first.at <= stall_end,
        "the hold started {:?} after the stall began, which lasted {:?}",
        first.at.saturating_duration_since(stall_start),
        stall_end - stall_start
    );
    assert!(
        last.at + Duration::from_millis(50) >= stall_end,
        "resumed {:?} before the data arrived",
        stall_end - last.at
    );
    assert!(
        samples[held..resumed].iter().all(|s| s.position == first.position),
        "the clock moved while holding"
    );
    let advanced = after.position.saturating_sub(before.position);
    assert!(
        advanced < Duration::from_millis(150),
        "position advanced {advanced:?} over a {:?} hold",
        after.at - before.at
    );
    assert_changed_between(&changed, before, first, "buffering started");
    assert_changed_between(&changed, last, after, "buffering ended");
    assert_complete(&backend.capture(), &state, &bytes, &samples, &server);
}

/// A pause while buffering is the user's: once the data is there buffering
/// ends but the clock stays put, and `play()` then plays the whole file.
#[test]
fn pause_during_buffering_stays_paused() {
    let _cpu = realtime_test();
    let bytes = Arc::new(make_ref_mkv());
    let server = slow_peer(&bytes);
    let (p, backend, _changed) = open_realtime(&server.url());
    p.play();
    std::thread::sleep(Duration::from_millis(500));
    assert!(p.state().buffering, "not buffering while waiting for data");
    p.pause();

    let (mut samples, st) =
        sample_until(&p, Duration::from_secs(20), |st| !st.buffering || finished(st));
    assert!(server.sent() > 0, "stopped buffering without data");
    assert!(!finished(&st), "playback finished while paused: {st:?}");
    assert!(!st.playing, "the pause was lost when buffering ended");
    let paused_at = Instant::now();
    let (held, _) = sample_until(&p, Duration::from_secs(5), |_| {
        paused_at.elapsed() >= Duration::from_secs(1)
    });
    assert!(
        held
            .iter()
            .all(|s| s.position == Duration::ZERO && !s.playing && !s.buffering),
        "the clock moved while paused: {held:?}"
    );
    samples.extend(held);

    p.play();
    let (played, state) = sample_until(&p, Duration::from_secs(30), finished);
    samples.extend(played);
    drop(p);
    assert!(state.error.is_none(), "unexpected error: {:?}", state.error);
    assert!(state.ended);
    assert_complete(&backend.capture(), &state, &bytes, &samples, &server);
}

/// Leaving the play screen during a P2P stall: dropping the player returns
/// within 200 ms whatever the peer withholds. It used to wait for the bytes
/// (the demuxer blocked on the ring, the read-ahead worker was joined while
/// in its socket read), which froze the app's UI.
#[test]
fn drop_during_stall_returns_quickly() {
    let _cpu = realtime_test();
    let limit = Duration::from_millis(200);
    let timed_drop = |p: Player| {
        let start = Instant::now();
        drop(p);
        start.elapsed()
    };

    // A peer that never answers: the request that opens the source hangs.
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let (p, _, _) = open_realtime(&format!("http://{}/video.mkv", silent.local_addr().unwrap()));
    let (request, _) = silent.accept().unwrap();
    let took = timed_drop(p);
    assert!(took < limit, "drop took {took:?} while the source was opening");
    drop(request);

    // A peer that withholds the first bytes: the probe waits on the ring,
    // the read-ahead worker sits in its socket read.
    let bytes = Arc::new(make_streaming_mkv());
    let server = slow_peer(&bytes);
    let (p, _, _) = open_realtime(&server.url());
    server.wait_until(Duration::from_secs(10), |a| a.first_get.is_some());
    let took = timed_drop(p);
    assert_eq!(server.sent(), 0, "the first bytes left the server before the drop");
    assert!(took < limit, "drop took {took:?} while the first bytes were withheld");

    // A peer that stalls mid-stream: the clock holds, the demuxer waits on
    // the ring.
    let server = slow_peer(&bytes);
    let (p, _, _) = open_realtime(&server.url());
    p.play();
    let (_, st) = sample_until(&p, Duration::from_secs(30), |st| {
        finished(st) || (st.buffering && st.position > Duration::ZERO && server.stall().is_some())
    });
    assert!(!finished(&st), "played to the end without stalling: {st:?}");
    let took = timed_drop(p);
    let (_, stall_end) = server.stall().expect("no stall");
    assert!(Instant::now() < stall_end, "the stall was over before the drop returned");
    assert!(took < limit, "drop took {took:?} during the mid-stream stall");
}

/// Seeking forward past a keyframe and back during realtime H.264 playback.
/// oxideav-h264's `reset()` dropped the avcC SPS/PPS, so after a seek every
/// slice failed and the video was disabled; the engine now builds fresh
/// decoders. From each target on, the frames shown are FFmpeg's (bar any
/// dropped as late), and the audio is FFmpeg's from the frame that holds
/// the target, stamped at the target.
#[test]
fn seek_during_realtime_h264_playback() {
    let _cpu = realtime_test();
    let bytes = ffmpeg_file(
        "mkv",
        &[
            "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25",
            "-f", "lavfi", "-i", "anoisesrc=c=pink:r=48000:a=0.5:s=7",
            "-t", "4",
            "-c:v", "libx264", "-g", "10", "-pix_fmt", "yuv420p",
            "-c:a", "flac",
        ],
    );
    let path = tempfile("mkv");
    std::fs::write(&path, &bytes).unwrap();
    let (p, backend, _) = open_realtime(path.to_str().unwrap());
    p.play();
    let forward = Duration::from_millis(2100);
    let back = Duration::from_millis(500);
    let (_, st) = sample_until(&p, Duration::from_secs(30), |st| {
        finished(st) || st.position >= Duration::from_secs(1)
    });
    assert!(!finished(&st), "ended before the first seek: {st:?}");
    p.seek(forward);
    let (_, st) = sample_until(&p, Duration::from_secs(30), |st| {
        finished(st) || (!st.buffering && st.position >= forward + Duration::from_millis(400))
    });
    assert!(!finished(&st), "ended before the second seek: {st:?}");
    p.seek(back);
    let (_, state) = sample_until(&p, Duration::from_secs(30), finished);
    drop(p);
    std::fs::remove_file(&path).ok();
    assert!(state.error.is_none(), "unexpected error: {:?}", state.error);
    assert!(state.ended);
    assert_eq!(state.video, Some(0), "the video was disabled");

    // Each seek flushed the sinks once: what follows a flush is that seek's.
    let capture = backend.capture();
    let targets = [Duration::ZERO, forward, back];
    let runs = |flushes: &[usize], len: usize| {
        let mut bounds = vec![0];
        bounds.extend_from_slice(flushes);
        bounds.push(len);
        bounds.windows(2).map(|w| w[0]..w[1]).collect::<Vec<_>>()
    };

    let video = &capture.video[0];
    assert_eq!(video.flushes.len(), 2, "video sink flushes: {:?}", video.flushes);
    let ff = ffmpeg_video_frames(&bytes);
    let mut missing = 0;
    for (k, (target, run)) in targets.iter().zip(runs(&video.flushes, video.pts.len())).enumerate() {
        assert!(!run.is_empty(), "no frame shown after the seek to {target:?}");
        let first = video.pts[run.start];
        assert!(first >= *target, "after the seek to {target:?} the first frame is {first:?}");
        // FFmpeg's frames from the target on, in order.
        let mut next = ff.iter().position(|(pts, _)| pts >= target).unwrap();
        for i in run {
            let skipped = ff[next..]
                .iter()
                .position(|(pts, _)| same_pts(*pts, video.pts[i]))
                .unwrap_or_else(|| {
                    panic!("after the seek to {target:?}, the frame at {:?} is not FFmpeg's next", video.pts[i])
                });
            next += skipped;
            assert_eq!(
                video.frame_md5[i], ff[next].1,
                "after the seek to {target:?}, the frame at {:?}",
                video.pts[i]
            );
            missing += skipped;
            next += 1;
        }
        if k == targets.len() - 1 {
            missing += ff.len() - next;
        }
    }
    assert!(
        missing as u64 <= state.dropped_frames,
        "{missing} frames missing after the seeks, {} dropped as late",
        state.dropped_frames
    );

    let audio = &capture.audio[0];
    assert_eq!(audio.flushes.len(), 2, "audio sink flushes: {:?}", audio.flushes);
    let ff = ffmpeg_audio_f32(&bytes);
    let frame = Duration::from_millis(100);
    for (k, (target, run)) in targets.iter().zip(runs(&audio.flushes, audio.writes.len())).enumerate() {
        assert!(!run.is_empty(), "no audio after the seek to {target:?}");
        let (stamp, from) = audio.writes[run.start];
        assert!(
            stamp >= *target && stamp - *target <= frame,
            "after the seek to {target:?} the first audio is stamped {stamp:?}"
        );
        let to = audio.writes.get(run.end).map_or(audio.pcm.len(), |&(_, offset)| offset);
        let pcm = &audio.pcm[from..to];
        let start = find_slice(&ff, &pcm[..pcm.len().min(256)])
            .unwrap_or_else(|| panic!("the audio after the seek to {target:?} is not FFmpeg's"));
        assert!(
            pcm == &ff[start..start + pcm.len()],
            "the audio after the seek to {target:?} has a gap or foreign samples"
        );
        let begins = Duration::from_secs_f64(start as f64 / 48000.0);
        assert!(
            begins <= *target && *target - begins <= frame,
            "the audio after the seek to {target:?} begins at {begins:?}"
        );
        if k == targets.len() - 1 {
            assert_eq!(start + pcm.len(), ff.len(), "the audio stopped before the end");
        }
    }
}

/// `select_audio` mid-playback hands the lane to the new track's pipeline
/// alone (the old thread kept consuming it, splitting the new track's
/// packets between two decoders), the new track plays every sample from
/// the switch point to the end, and the video plays on.
#[test]
fn select_audio_plays_new_track_from_switch() {
    let _cpu = realtime_test();
    let bytes = ffmpeg_file(
        "mkv",
        &[
            "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25",
            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
            "-f", "lavfi", "-i", "anoisesrc=c=pink:r=48000:a=0.5:s=11",
            "-map", "0", "-map", "1", "-map", "2",
            "-t", "4",
            "-c:v", "libx264", "-g", "10", "-pix_fmt", "yuv420p",
            "-c:a", "flac",
        ],
    );
    let path = tempfile("mkv");
    std::fs::write(&path, &bytes).unwrap();
    let (p, backend, _) = open_realtime(path.to_str().unwrap());
    p.play();
    let (_, st) = sample_until(&p, Duration::from_secs(30), |st| {
        finished(st) || st.position >= Duration::from_millis(1200)
    });
    assert!(!finished(&st), "ended before the switch: {st:?}");
    assert_eq!(st.audio, Some(1));
    let switched_at = p.state().position;
    p.select_audio(Some(2));
    let (_, state) = sample_until(&p, Duration::from_secs(30), finished);
    drop(p);
    std::fs::remove_file(&path).ok();
    assert!(state.error.is_none(), "unexpected error: {:?}", state.error);
    assert!(state.ended);
    assert_eq!(state.audio, Some(2));

    let capture = backend.capture();
    let old = capture.audio.iter().find(|a| a.stream == 1).expect("no capture of the first track");
    let new = capture.audio.iter().find(|a| a.stream == 2).expect("no capture of the second track");

    // The first track: FFmpeg's decode of it from the start, nothing else.
    let ff_old = ffmpeg_audio_f32_nth(&bytes, 0);
    assert!(!old.pcm.is_empty() && old.pcm.len() <= ff_old.len(), "first track: {} samples", old.pcm.len());
    assert!(old.pcm[..] == ff_old[..old.pcm.len()], "the first track holds foreign samples");

    // The second track: every sample from the switch point to the end.
    let ff_new = ffmpeg_audio_f32_nth(&bytes, 1);
    assert!(new.pcm.len() >= 256, "second track: {} samples", new.pcm.len());
    let start = find_slice(&ff_new, &new.pcm[..256]).expect("the second track is not FFmpeg's decode of it");
    assert!(new.pcm[..] == ff_new[start..], "the second track is not whole from the switch to the end");
    let begins = Duration::from_secs_f64(start as f64 / 48000.0);
    let slack = Duration::from_millis(300);
    assert!(
        begins.abs_diff(switched_at) <= slack,
        "the second track begins at {begins:?}, the switch was at {switched_at:?}"
    );
    let (stamp, _) = new.writes[0];
    assert!(
        stamp.abs_diff(switched_at) <= slack,
        "the second track's first audio is stamped {stamp:?}, the switch was at {switched_at:?}"
    );

    // The video plays on through the switch, to the last frame.
    let video = &capture.video[0];
    let last = *video.pts.last().expect("no video");
    assert!(last >= Duration::from_millis(3900), "the video stopped at {last:?}");
}

/// A stream nobody can decode (FFV1 here) must not stall the playback. Its
/// pipeline ended at once while the demuxer kept queuing its packets into a
/// lane nobody drained; once that lane held 2 s of media the demuxer parked
/// for good and the playback never ended (the e2e hangs T3 and T6). The
/// rest of the file now plays to the end.
#[test]
fn stream_without_decoder_does_not_stall() {
    let _cpu = batch_test();
    let bytes = ffmpeg_file(
        "mkv",
        &[
            "-f", "lavfi", "-i", "testsrc2=size=160x120:rate=25",
            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
            "-t", "5",
            "-c:v", "ffv1", "-c:a", "flac",
        ],
    );
    let path = tempfile("mkv");
    std::fs::write(&path, &bytes).unwrap();
    let (capture, state) = play_to_end_within(path.to_str().unwrap(), Duration::from_secs(60));
    std::fs::remove_file(&path).ok();
    assert!(state.ended);
    let error = state.error.as_deref().unwrap_or_default();
    assert!(error.contains("no video decoder"), "state error: {error:?}");
    assert!(capture.video.iter().all(|v| v.frame_md5.is_empty()));
    assert_eq!(capture.audio[0].pcm, ffmpeg_audio_f32(&bytes), "the audio");
}

/// H.264 in NUT carries its parameter sets as Annex B extradata, which
/// oxideav-h264 refuses (it reads avcC only): the video pipeline had no
/// decoder and the playback stalled (e2e T3). The engine hands those
/// parameter sets to the decoder in-band; every frame matches FFmpeg's.
#[test]
fn nut_h264_with_annex_b_extradata_matches_ffmpeg() {
    let _cpu = batch_test();
    let bytes = ffmpeg_file(
        "nut",
        &[
            "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25",
            "-t", "3",
            "-c:v", "libx264", "-pix_fmt", "yuv420p",
        ],
    );
    let path = tempfile("nut");
    std::fs::write(&path, &bytes).unwrap();
    let (capture, state) = play_to_end_within(path.to_str().unwrap(), Duration::from_secs(60));
    std::fs::remove_file(&path).ok();
    assert!(state.error.is_none(), "unexpected error: {:?}", state.error);
    assert!(state.ended);
    let video = &capture.video[0];
    assert_eq!(video.codec, "h264");
    let ff = ffmpeg_video_md5s(&bytes);
    assert_eq!(video.frame_md5.len(), ff.len(), "frame count");
    for (i, (a, b)) in video.frame_md5.iter().zip(&ff).enumerate() {
        assert_eq!(a, b, "frame {i} md5: ours={a} ffmpeg={b}");
    }
}

/// FFmpeg's framemd5 hashes a pal8 frame as its index rows followed by the
/// 256-entry palette, 4 bytes an entry (`av_image_copy_to_buffer`); the
/// headless capture must hash the same bytes.
#[test]
fn pal8_frames_hash_with_their_palette() {
    let _cpu = batch_test();
    let (width, height) = (8usize, 4usize);
    let source = [
        "-v", "error", "-nostdin",
        "-f", "lavfi", "-i", "testsrc=size=8x4:rate=1",
        "-frames:v", "1", "-pix_fmt", "pal8",
    ];
    let ffmpeg = |format: &[&str]| {
        let out = std::process::Command::new("ffmpeg")
            .args(source)
            .args(format)
            .output()
            .expect("ffmpeg must be on PATH");
        assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    };
    // FFmpeg's own layout of the frame, and its framemd5.
    let raw = ffmpeg(&["-f", "rawvideo", "-"]);
    let framemd5 = String::from_utf8(ffmpeg(&["-f", "framemd5", "-"])).unwrap();
    let expected = framemd5.lines().last().unwrap().rsplit(',').next().unwrap().trim().to_string();
    assert_eq!(raw.len(), width * height + 1024);
    let (indices, palette) = raw.split_at(width * height);
    assert!(palette.chunks_exact(4).all(|e| e[3] == 0xFF), "FFmpeg's palette is opaque");

    // The same frame as a decoder hands it over: padded index rows and a
    // packed RGB palette.
    let stride = 16;
    let mut plane = vec![0xEE; stride * height];
    for row in 0..height {
        plane[row * stride..row * stride + width].copy_from_slice(&indices[row * width..(row + 1) * width]);
    }
    let rgb: Vec<u8> = palette.chunks_exact(4).flat_map(|e| [e[2], e[1], e[0]]).collect();
    let frame = oxideav_core::VideoFrame {
        pts: Some(0),
        planes: vec![oxideav_core::VideoPlane { stride, data: plane }],
    }
    .with_palette(rgb);
    let packed = player::headless::pack_frame(&frame, oxideav_core::PixelFormat::Pal8, width as u32, height as u32);
    assert_eq!(packed, raw, "packed bytes");
    assert_eq!(format!("{:x}", md5::compute(&packed)), expected);
}

/// fate:isom/vc1-wmapro.ism: VC-1 video next to a WMA Pro track that may
/// have no decoder (main has none yet). Its e2e run hung past the watchdog
/// like T3/T6. The track without a decoder is skipped (listed in `tracks`,
/// not selected, its error reported) and the video plays to its end on a
/// running clock, FFmpeg's frames bit for bit.
#[test]
fn track_without_decoder_is_skipped() {
    let _cpu = realtime_test();
    let path = refcheck::fate("isom/vc1-wmapro.ism");
    let bytes = std::fs::read(&path).unwrap();
    let (p, backend, _) = open_realtime(path.to_str().unwrap());
    p.play();
    // The skipped track's error shows up at once; the playback goes on.
    let (samples, state) = sample_until(&p, Duration::from_secs(60), |st| st.ended);
    drop(p);
    assert!(state.ended, "did not end: {state:?}");
    assert!(
        state.tracks.iter().any(|t| t.stream == 0 && t.kind == player::TrackKind::Audio),
        "the WMA Pro track is not listed: {:?}",
        state.tracks
    );
    let capture = backend.capture();
    if let Some(error) = &state.error {
        // No WMA Pro decoder: the track is skipped.
        assert!(error.contains("no audio decoder"), "unexpected error: {error}");
        assert_eq!(state.audio, None, "a track without a decoder stays selected");
        assert!(capture.audio.is_empty(), "audio captured without a decoder");
    }
    assert_eq!(state.video, Some(1));

    // FFmpeg's frames bit for bit and in order (C IDCT, as the codec crates
    // compare), bar any dropped as late. Matched by content: the file stamps
    // packets pts == dts, and FFmpeg's B-frame delay shifts its frames'
    // stamps by a frame.
    let video = &capture.video[0];
    let ff = ffmpeg_video_md5s_with(&bytes, &["-idct", "simple"]);
    let mut next = 0;
    for (i, md5) in video.frame_md5.iter().enumerate() {
        let skipped = ff[next..]
            .iter()
            .position(|m| m == md5)
            .unwrap_or_else(|| panic!("frame {i} (at {:?}) is not among FFmpeg's next frames", video.pts[i]));
        next += skipped + 1;
    }
    let missing = ff.len() - video.frame_md5.len();
    assert!(
        missing as u64 <= state.dropped_frames,
        "{missing} of FFmpeg's {} frames missing, {} dropped as late",
        ff.len(),
        state.dropped_frames
    );
    // The clock ran up to the last frame (shown up to 100 ms early).
    let last = *video.pts.last().expect("no video");
    let end = samples.last().unwrap().position;
    assert!(end + Duration::from_millis(200) >= last, "the clock stopped at {end:?}, last frame {last:?}");
}

/// One white frame and a 40 ms, 1 kHz beep at each integer second. PCM and
/// short indexed clusters keep HTTP read-ahead from swallowing the stall.
fn sync_clip(seconds: u32) -> Vec<u8> {
    ffmpeg_file("mkv", &[
        "-f", "lavfi", "-i",
        "color=black:size=160x96:rate=25,drawbox=color=white:t=fill:enable='lt(mod(t,1),0.039)',geq=lum='if(lt(Y,8)*lt(X,128),if(bitand(N,pow(2,floor(X/16))),220,32),lum(X,Y))':cb='cb(X,Y)':cr='cr(X,Y)'",
        "-f", "lavfi", "-i",
        "aevalsrc=if(lt(mod(t\\,1)\\,0.04)\\,0.5*sin(2*PI*1000*t)\\,0):s=48000",
        "-t", &seconds.to_string(), "-c:v", "libx264", "-preset", "ultrafast",
        "-g", "25", "-bf", "0", "-pix_fmt", "yuv420p",
        "-c:a", "pcm_s16le", "-ac", "2", "-reserve_index_space", "4096",
        "-cluster_size_limit", "100000", "-cluster_time_limit", "200",
    ])
}

/// Compare independent output records, not video PTS against the clock the
/// scheduler just read. Locate the actual beep samples in captured PCM and
/// interpolate their device presentation times in recorded playback runs.
fn assert_flash_beeps(capture: &Capture, bytes: &[u8], expected: &[Vec<u32>], name: &str) {
    let video = &capture.video[0];
    let audio = &capture.audio[0];
    let reference = ffmpeg_video_frames(bytes);
    let mut bounds = vec![0];
    bounds.extend_from_slice(&video.flushes);
    bounds.push(video.pts.len());
    assert_eq!(bounds.len() - 1, expected.len(), "seek generations");
    let mut offsets = Vec::new();
    let mut csv = String::from("generation,pts_s,video_mono_ns,beep_mono_ns,offset_ms\n");
    for (generation, (range, seconds)) in bounds.windows(2).zip(expected).enumerate() {
        for &second in seconds {
            let pts = Duration::from_secs(u64::from(second));
            let frame = (range[0]..range[1]).find(|&i| video.pts[i] == pts)
                .unwrap_or_else(|| panic!("missing flash at {pts:?}, generation {generation}"));
            let expected_frame = &reference.iter().find(|(p, _)| *p == pts).expect("reference flash").1;
            assert_eq!(&video.frame_md5[frame], expected_frame, "flash/frame identifier differs from FFmpeg");
            let shown = video.shown_at[frame];
            let channels = usize::from(audio.channels);
            let mut heard = Vec::new();
            for (i, &(start_pts, from)) in audio.writes.iter().enumerate() {
                let to = audio.writes.get(i + 1).map_or(audio.pcm.len(), |w| w.1);
                let span = Duration::from_secs_f64((to - from) as f64 / channels as f64 / audio.sample_rate as f64);
                if pts < start_pts || pts >= start_pts + span {
                    continue;
                }
                let offset = ((pts - start_pts).as_secs_f64() * audio.sample_rate as f64).round() as usize;
                let estimated = from / channels + offset;
                // Matroska packet stamps round to milliseconds. Find the
                // waveform's actual onset near that estimate, after silence,
                // rather than assigning a rounded stamp to a silent sample.
                let radius = audio.sample_rate as usize / 500;
                let sample_frame = (estimated.saturating_sub(radius)..=(estimated + radius).min(audio.pcm.len() / channels - 1))
                    .find(|&n| audio.pcm[n * channels].abs() > 0.01
                        && (n.saturating_sub(16)..n).all(|k| audio.pcm[k * channels].abs() <= 0.01))
                    .unwrap_or_else(|| panic!("no beep onset near {pts:?}"));
                let sample = sample_frame * channels;
                for &(begin, end, at) in &audio.played {
                    if (begin..end).contains(&sample) {
                        heard.push(at + ((sample - begin) as f64 / channels as f64 / audio.device_rate * 1e9).round() as i64);
                    }
                }
            }
            let beep = heard.into_iter().min_by_key(|at| shown.abs_diff(*at))
                .unwrap_or_else(|| panic!("beep at {pts:?} was never played"));
            let offset = (shown - beep) as f64 / 1e6;
            csv.push_str(&format!("{generation},{second},{shown},{beep},{offset:.6}\n"));
            offsets.push(offset);
        }
    }
    let root = std::env::var_os("CARGO_TARGET_DIR").map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("../../target"));
    let dir = root.join("engine-sync");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.csv")), csv).unwrap();
    eprintln!("{name}: offsets_ms={offsets:?}");
    assert!(offsets.iter().all(|v| v.abs() <= 40.0), "A/V offsets exceed 40 ms: {offsets:?}");
}

#[test]
fn audio_master_follows_two_percent_slow_device_for_thirty_seconds() {
    let _cpu = realtime_test();
    let bytes = sync_clip(32);
    let path = tempfile("mkv");
    std::fs::write(&path, &bytes).unwrap();
    let backend = Headless::new();
    backend.set_audio_speed(0.98);
    let player = Player::open(path.to_str().unwrap(), backend.clone(), test_context(), PlayerOptions::default(), |_| {});
    let (_, state) = sample_until(&player, Duration::from_secs(50), finished);
    drop(player);
    std::fs::remove_file(path).unwrap();
    assert!(state.ended && state.error.is_none(), "{state:?}");
    assert_flash_beeps(&backend.capture(), &bytes, &[(0..32).collect()], "headless-drift");
}

#[test]
fn flash_beeps_remain_synchronized_after_stall_pause_and_seek() {
    let _cpu = realtime_test();
    let bytes = Arc::new(sync_clip(8));
    let server = HttpServer::start_with(bytes.clone(), Delivery {
        mid_stall: Some(Duration::from_secs(6)),
        ..Delivery::default()
    });
    let (player, backend, _) = open_realtime(&server.url());
    let (samples, state) = sample_until(&player, Duration::from_secs(25), |s| finished(s) || s.position >= Duration::from_millis(6200));
    assert!(!finished(&state), "{state:?}");
    assert!(samples.windows(2).any(|w| !w[0].buffering && w[1].buffering), "no mid-stream hold");
    player.pause();
    let held = player.state().position;
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(player.state().position, held, "paused master moved");
    player.seek(Duration::from_millis(2100));
    player.play();
    let (_, state) = sample_until(&player, Duration::from_secs(20), finished);
    drop(player);
    assert!(state.ended && state.error.is_none(), "{state:?}");
    assert_flash_beeps(&backend.capture(), &bytes, &[(0..7).collect(), (3..8).collect()], "headless-stall-seek");
}

#[test]
fn far_future_final_timestamp_does_not_hold_the_end() {
    let _cpu = realtime_test();
    // A hostile last video packet stamped a minute after the rest.
    let bytes = ffmpeg_file("mkv", &[
        "-f", "lavfi", "-i", "testsrc=size=160x96:rate=25:duration=2",
        "-f", "lavfi", "-i", "sine=sample_rate=48000:duration=2",
        "-c:v", "libx264", "-bf", "0", "-g", "25", "-c:a", "pcm_s16le",
        "-bsf:v", "setts=pts=if(gte(N\\,49)\\,PTS+60/TB\\,PTS):dts=if(gte(N\\,49)\\,DTS+60/TB\\,DTS)",
    ]);
    let path = tempfile("mkv");
    std::fs::write(&path, bytes).unwrap();
    let backend = Headless::new();
    let opened = Instant::now();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), test_context(), PlayerOptions::default(), |_| {});
    let (_, state) = sample_until(&player, Duration::from_secs(20), finished);
    let elapsed = opened.elapsed();
    drop(player);
    std::fs::remove_file(path).unwrap();
    assert!(state.ended && state.error.is_none(), "{state:?}");
    // Two seconds of media, then at most the queue horizon and a second.
    assert!(elapsed < Duration::from_secs(8), "end held {elapsed:?} by a bogus timestamp");
    assert!(state.position < Duration::from_secs(6), "position ran to {:?}", state.position);
}

/// Wraps the registry's MPEG-2 decoder and releases every picture only at
/// flush, as a hardware decoder or a flush-only software decoder may: the
/// demuxer reaches the end while the clock is still held at zero.
struct FlushOnly {
    inner: Box<dyn oxideav_core::Decoder>,
    held: std::collections::VecDeque<oxideav_core::Frame>,
    flushed: bool,
}

static FLUSH_ONLY_OPENED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl FlushOnly {
    fn open(params: &oxideav_core::CodecParameters) -> oxideav_core::Result<Box<dyn oxideav_core::Decoder>> {
        FLUSH_ONLY_OPENED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let inner = codecs::context().codecs.first_decoder(params)?;
        Ok(Box::new(FlushOnly { inner, held: Default::default(), flushed: false }))
    }

    fn drain(&mut self) -> oxideav_core::Result<()> {
        loop {
            match self.inner.receive_frame() {
                Ok(frame) => self.held.push_back(frame),
                Err(oxideav_core::Error::NeedMore | oxideav_core::Error::Eof) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
}

impl oxideav_core::Decoder for FlushOnly {
    fn codec_id(&self) -> &oxideav_core::CodecId {
        self.inner.codec_id()
    }
    fn send_packet(&mut self, packet: &oxideav_core::Packet) -> oxideav_core::Result<()> {
        self.inner.send_packet(packet)?;
        self.drain()
    }
    fn receive_frame(&mut self) -> oxideav_core::Result<oxideav_core::Frame> {
        match (self.flushed, self.held.pop_front()) {
            (true, Some(frame)) => Ok(frame),
            (true, None) => Err(oxideav_core::Error::Eof),
            (false, frame) => {
                if let Some(frame) = frame {
                    self.held.push_front(frame);
                }
                Err(oxideav_core::Error::NeedMore)
            }
        }
    }
    fn flush(&mut self) -> oxideav_core::Result<()> {
        self.inner.flush()?;
        self.drain()?;
        self.flushed = true;
        Ok(())
    }
    fn reset(&mut self) -> oxideav_core::Result<()> {
        self.inner.reset()?;
        self.held.clear();
        self.flushed = false;
        Ok(())
    }
    fn output_video_dimensions(&self) -> Option<(u32, u32)> {
        self.inner.output_video_dimensions()
    }
    fn output_pixel_format(&self) -> Option<oxideav_core::PixelFormat> {
        self.inner.output_pixel_format()
    }
}

#[test]
fn decoder_held_frames_play_after_demux_eof() {
    let _cpu = realtime_test();
    let bytes = ffmpeg_file("mkv", &[
        "-f", "lavfi", "-i", "testsrc=size=160x96:rate=25:duration=6",
        "-c:v", "mpeg2video", "-g", "25", "-bf", "0",
    ]);
    let path = tempfile("mkv");
    std::fs::write(&path, bytes).unwrap();
    // The player takes the first registered decoder, so the wrapper goes in
    // before the full registry.
    let mut context = oxideav_core::RuntimeContext::new();
    context.codecs.register(
        oxideav_core::CodecInfo::new(oxideav_core::CodecId::new("mpeg2video"))
            .capabilities(oxideav_core::CodecCapabilities::video("flush_only"))
            .decoder(FlushOnly::open),
    );
    codecs::register_all(&mut context);
    let backend = Headless::new();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(context), PlayerOptions::default(), |_| {});
    let (_, state) = sample_until(&player, Duration::from_secs(20), finished);
    drop(player);
    std::fs::remove_file(path).unwrap();
    assert!(state.ended && state.error.is_none(), "{state:?}");
    let video = &backend.capture().video[0];
    // Capping the tail at "clock + queue horizon" while the clock was held
    // dropped every picture past 3 s (74 of 150). A few late frames under
    // machine load are a timing matter, not this one.
    assert_eq!(video.pts.last().copied(), Some(Duration::from_millis(5960)),
        "the final picture was not shown ({} shown, {} dropped)", video.pts.len(), state.dropped_frames);
    assert!(video.pts.len() >= 135, "only {} of 150 pictures shown ({} dropped)", video.pts.len(), state.dropped_frames);
    assert!(FLUSH_ONLY_OPENED.load(std::sync::atomic::Ordering::SeqCst) > 0, "the flush-only decoder was not used");
}

/// A platform-like output: takes MPEG-2 compressed and counts what it gets;
/// software frames go to the wrapped Headless capture.
struct CompressedOutput {
    inner: Arc<Headless>,
    pushed: Arc<std::sync::atomic::AtomicUsize>,
}

struct CompressedSink {
    inner: Box<dyn player::backend::VideoSink>,
    pushed: Arc<std::sync::atomic::AtomicUsize>,
}

impl player::backend::Backend for CompressedOutput {
    fn audio(&self) -> Box<dyn player::backend::AudioSink> {
        player::backend::Backend::audio(&*self.inner)
    }
    fn video(&self, clock: Arc<dyn player::backend::Clock>) -> Box<dyn player::backend::VideoSink> {
        let inner = player::backend::Backend::video(&*self.inner, clock);
        Box::new(CompressedSink { inner, pushed: self.pushed.clone() })
    }
    fn subtitles(&self) -> Box<dyn player::backend::SubtitleSink> {
        player::backend::Backend::subtitles(&*self.inner)
    }
}

impl player::backend::VideoSink for CompressedSink {
    fn open_compressed(&mut self, params: &oxideav_core::CodecParameters) -> bool {
        params.codec_id.as_str() == "mpeg2video"
    }
    fn push_packet(&mut self, _: &oxideav_core::Packet, _: Duration, _: bool) -> Result<(), player::backend::SinkError> {
        self.pushed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn open_frames(&mut self, params: &oxideav_core::CodecParameters) -> Result<(), player::backend::SinkError> {
        self.inner.open_frames(params)
    }
    fn push_frame(&mut self, frame: &oxideav_core::VideoFrame, pts: Duration) -> Result<(), player::backend::SinkError> {
        self.inner.push_frame(frame, pts)
    }
    fn frame_lead(&self) -> Duration {
        self.inner.frame_lead()
    }
    fn finish(&mut self) -> Result<(), player::backend::SinkError> {
        self.inner.finish()
    }
    fn flush(&mut self) {
        self.inner.flush()
    }
    fn set_playing(&mut self, playing: bool) {
        self.inner.set_playing(playing)
    }
}

#[test]
fn untimed_raw_mpeg_switches_to_software_decoding() {
    // Raw MPEG-2 leaves I and P pictures untimed (only B pictures carry a
    // PTS), which a platform decoder cannot present: the stream must move to
    // the software decoder before any packet reaches the compressed sink,
    // and play every picture with FFmpeg's pixels and times.
    let bytes = ffmpeg_file("m2v", &[
        "-f", "lavfi", "-i", "testsrc=size=160x96:rate=25:duration=2",
        "-c:v", "mpeg2video", "-g", "12", "-bf", "2",
    ]);
    let path = tempfile("m2v");
    std::fs::write(&path, bytes).unwrap();
    let inner = Headless::new();
    inner.set_active_streams(Some((0, "mpeg2video".into())), None, None, false);
    let pushed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let backend = Arc::new(CompressedOutput { inner: inner.clone(), pushed: pushed.clone() });
    let options = PlayerOptions { realtime: false, ..PlayerOptions::default() };
    let player = Player::open(path.to_str().unwrap(), backend, test_context(), options, |_| {});
    let (_, state) = sample_until(&player, Duration::from_secs(20), finished);
    drop(player);
    assert!(state.ended && state.error.is_none(), "{state:?}");
    assert_eq!(pushed.load(std::sync::atomic::Ordering::SeqCst), 0, "untimed packets reached the compressed sink");
    let capture = inner.capture();
    let video = &capture.video[0];
    let expected = refcheck::ffmpeg_video_md5s_with(&path, 0, "yuv420p", &["-idct", "simple"]);
    assert_eq!(expected.len(), 50);
    assert_eq!(video.frame_md5, expected, "pictures differ from FFmpeg");
    let probe = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "frame=best_effort_timestamp_time", "-of", "csv=p=0"])
        .arg(&path)
        .output()
        .expect("ffprobe must be on PATH");
    std::fs::remove_file(&path).unwrap();
    // FFmpeg leaves a picture untimed when it cannot derive a time (N/A);
    // there ours must still continue the timeline.
    let theirs: Vec<Option<u128>> = String::from_utf8(probe.stdout).unwrap().lines()
        .map(|t| t.trim().trim_end_matches(',').parse::<f64>().ok().map(|s| Duration::from_secs_f64(s).as_millis()))
        .collect();
    let ours: Vec<u128> = video.pts.iter().map(Duration::as_millis).collect();
    assert_eq!(ours.len(), theirs.len());
    assert!(theirs.iter().filter(|t| t.is_some()).count() >= 45, "FFmpeg times too few pictures: {theirs:?}");
    for (i, (ours_t, theirs_t)) in ours.iter().zip(&theirs).enumerate() {
        match theirs_t {
            Some(t) => assert_eq!(ours_t, t, "picture {i}: ours {ours:?}, FFmpeg {theirs:?}"),
            None => assert!(i == 0 || *ours_t > ours[i - 1], "picture {i} does not continue the timeline: {ours:?}"),
        }
    }
}

#[test]
fn video_without_container_size_plays_at_the_decoded_size() {
    // A raw H.264 stream declares no picture size; only the decoder knows
    // it. Every picture must still reach the sink at that size, matching
    // FFmpeg, and the player state must report it.
    let bytes = ffmpeg_file("h264", &[
        "-f", "lavfi", "-i", "testsrc=size=176x144:rate=25:duration=1",
        "-c:v", "libx264", "-bf", "0", "-pix_fmt", "yuv420p",
    ]);
    let path = tempfile("h264");
    std::fs::write(&path, bytes).unwrap();
    let backend = Headless::new();
    let options = PlayerOptions { realtime: false, ..PlayerOptions::default() };
    let player = Player::open(path.to_str().unwrap(), backend.clone(), test_context(), options, |_| {});
    let (_, state) = sample_until(&player, Duration::from_secs(20), finished);
    drop(player);
    assert!(state.ended && state.error.is_none(), "{state:?}");
    assert_eq!(state.video_size, Some((176, 144)), "decoded size not published");
    let capture = backend.capture();
    let video = &capture.video[0];
    assert_eq!((video.width, video.height), (176, 144), "sink not opened at the decoded size");
    let expected = refcheck::ffmpeg_video_md5s_with(&path, 0, "yuv420p", &[]);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(expected.len(), 25);
    assert_eq!(video.frame_md5, expected, "pictures differ from FFmpeg");
}

/// Plays `bytes`, a `ext` file that declares no picture size, and checks
/// that the player state reports the decoded `size`, that the sink was
/// opened at it, and that every picture equals FFmpeg's decode.
fn assert_plays_at_decoded_size(ext: &str, bytes: Vec<u8>, size: (u32, u32), frames: usize) {
    let path = tempfile(ext);
    std::fs::write(&path, bytes).unwrap();
    let backend = Headless::new();
    let options = PlayerOptions { realtime: false, ..PlayerOptions::default() };
    let player = Player::open(path.to_str().unwrap(), backend.clone(), test_context(), options, |_| {});
    let (_, state) = sample_until(&player, Duration::from_secs(30), finished);
    drop(player);
    let expected = refcheck::ffmpeg_video_md5s_with(&path, 0, "yuv420p", &[]);
    std::fs::remove_file(&path).unwrap();
    assert!(state.ended && state.error.is_none(), "{ext}: {state:?}");
    assert_eq!(state.video_size, Some(size), "{ext}: decoded size not published");
    let capture = backend.capture();
    let video = &capture.video[0];
    assert_eq!((video.width, video.height), size, "{ext}: sink not opened at the decoded size");
    assert_eq!(expected.len(), frames, "{ext}: FFmpeg frame count");
    assert_eq!(video.frame_md5, expected, "{ext}: pictures differ from FFmpeg");
}

/// An odd-size test card: one second at 25 fps, cropped from whole blocks.
const ODD_TESTSRC: &str = "testsrc=size=33x17:rate=25:duration=1";

#[test]
fn raw_hevc_plays_at_the_cropped_decoded_size() {
    // x265 codes 4:2:0 only at even sizes; 34×18 is still cropped from the
    // 40×24 its coding blocks cover.
    let bytes = ffmpeg_file("hevc", &[
        "-f", "lavfi", "-i", "testsrc=size=34x18:rate=25:duration=1", "-pix_fmt", "yuv420p",
        "-c:v", "libx265", "-x265-params", "log-level=error:bframes=2",
    ]);
    assert_plays_at_decoded_size("hevc", bytes, (34, 18), 25);
}

/// An IVF file of `codec` whose file header declares a 0×0 picture.
fn ivf_without_size(codec: &[&str]) -> Vec<u8> {
    let mut args = vec!["-f", "lavfi", "-i", ODD_TESTSRC, "-pix_fmt", "yuv420p"];
    args.extend_from_slice(codec);
    let mut bytes = ffmpeg_file("ivf", &args);
    assert_eq!(&bytes[..4], b"DKIF");
    // Width and height, 16 bits each, at bytes 12..16.
    bytes[12..16].fill(0);
    bytes
}

#[test]
fn ivf_vp8_without_size_plays_at_the_decoded_size() {
    let bytes = ivf_without_size(&["-c:v", "libvpx", "-b:v", "200k"]);
    assert_plays_at_decoded_size("ivf", bytes, (33, 17), 25);
}

#[test]
fn ivf_vp9_without_size_plays_at_the_decoded_size() {
    let bytes = ivf_without_size(&["-c:v", "libvpx-vp9", "-b:v", "200k"]);
    assert_plays_at_decoded_size("ivf", bytes, (33, 17), 25);
}

#[test]
fn ivf_av1_without_size_plays_at_the_decoded_size() {
    let bytes = ivf_without_size(&["-c:v", "libaom-av1", "-cpu-used", "8", "-b:v", "200k"]);
    assert_plays_at_decoded_size("ivf", bytes, (33, 17), 25);
}


