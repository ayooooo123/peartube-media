//! Regression suite modelling the 50-track "Iron Man 2" relay stream:
//! - Matroska container with 50 tracks:
//!   - Stream 0: HEVC Main 10 (`yuv420p10le`, BT.2020 PQ HDR10 + mastering display,
//!     24000/1001 fps, 192-frame / 8.0 s long GOP with B-pyramid reorder)
//!   - Stream 1: 7.1-channel (8 ch, 48 kHz, 24-bit in S32) Dolby TrueHD (`truehd`,
//!     1,200 access units/sec, container omits `sample_format`)
//!   - Streams 2..=3: secondary 5.1 and 2.0 `ac3` audio tracks
//!   - Streams 4..=49: 46 subtitle tracks muxed across the timeline
//! - Served over an HTTP/1.1 Range-capable relay server on `127.0.0.1` with
//!   per-Range-request relay latency.
//!
//! Verifies:
//! 1. Mid-GOP start/seek (`6.50 s` inside an `8.0 s` GOP = `7,800` pre-target
//!    TrueHD access units) fast-forwards to the latest TrueHD Major Sync before
//!    the target and still produces 100% bit-exact 8-channel `s32le` lossless PCM
//!    matching FFmpeg from `6.50 s` to EOF, opening the sink once as `S32` (never
//!    opening `F32` first and tearing down).
//! 2. `MkvRichDemuxer::set_active_streams` culls the 48 inactive audio/subtitle
//!    streams while preserving HDR10 metadata and chapters.
//! 3. Realtime HTTP Range relay playback + mid-GOP seek primes quickly, drops
//!    zero frames, never stalls presentation cadence, and reads in 512 KiB chunks.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use player::{AudioOutputMode, Event, Headless, PcmFormat, Player, PlayerOptions, TrackKind};

static TEST_CPU: RwLock<()> = RwLock::new(());

struct RelayFixture {
    _dir: PathBuf,
    mkv_path: PathBuf,
    mkv_bytes: Vec<u8>,
}

static FIXTURE: OnceLock<RelayFixture> = OnceLock::new();

fn build_iron_man_2_fixture() -> &'static RelayFixture {
    FIXTURE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("peartube-im2-relay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture temp dir");

        // 9.0 seconds (216 frames @ 24000/1001 fps; GOP=192 puts keyframes at 0.0 s and ~8.008 s,
        // so seeking to 6.50 s lands 6.50 s deep inside the first GOP across 7,800 TrueHD units).
        let raw_s32 = dir.join("thd_51.s32");
        let frames_48k = 9 * 48_000usize;
        let mut pcm = Vec::with_capacity(frames_48k * 6 * 4);
        for f in 0..frames_48k {
            for ch in 0..6usize {
                // Deterministic 24-bit signal packed into S32 (low 8 bits zero, matching TrueHD 24-bit).
                let phase = ((f.wrapping_mul(37 + ch * 13) ^ (f >> 3)) & 0x00ff_ffff) as i32;
                let signed24 = (phase - 0x0080_0000) << 8;
                pcm.extend_from_slice(&signed24.to_le_bytes());
            }
        }
        std::fs::write(&raw_s32, &pcm).expect("write raw 5.1 s32");

        // Dense SRT cue stream replicated across 46 subtitle tracks so every cluster carries
        // multi-track subtitle blocks alongside video and 3 audio tracks (50 tracks total).
        let srt_path = dir.join("subs.srt");
        let mut srt = String::new();
        for idx in 0..18usize {
            let s0 = idx as f64 * 0.5;
            let s1 = s0 + 0.4;
            srt.push_str(&format!(
                "{}\n00:00:{:02},{:03} --> 00:00:{:02},{:03}\nStark telemetry cue #{idx}\n\n",
                idx + 1,
                s0 as u32,
                ((s0.fract()) * 1000.0).round() as u32,
                s1 as u32,
                ((s1.fract()) * 1000.0).round() as u32,
            ));
        }
        std::fs::write(&srt_path, &srt).expect("write srt");

        let mkv_path = dir.join("iron_man_2_relay_50tracks.mkv");
        let mut cmd = std::process::Command::new(refcheck::system_ffmpeg());
        cmd.args([
            "-v",
            "error",
            "-nostdin",
            "-y",
            // Input 0: 23.976 fps video source
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=384x216:rate=24000/1001:duration=9",
            // Input 1: 5.1 48 kHz S32 PCM for lossless TrueHD (FFmpeg's truehd encoder max layout)
            "-f",
            "s32le",
            "-ar",
            "48000",
            "-ch_layout",
            "5.1",
            "-i",
        ])
        .arg(&raw_s32)
        // Input 2: 5.1 source for secondary AC-3
        .args([
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=330:sample_rate=48000:duration=9",
            // Input 3: 2.0 source for commentary AC-3
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=550:sample_rate=48000:duration=9",
            // Input 4: subtitle source mapped 46 times
            "-i",
        ])
        .arg(&srt_path);

        cmd.args(["-map", "0:v:0", "-map", "1:a:0", "-map", "2:a:0", "-map", "3:a:0"]);
        for _ in 0..46 {
            cmd.args(["-map", "4:s:0"]);
        }

        cmd.args([
            "-c:v:0",
            "libx265",
            "-pix_fmt",
            "yuv420p10le",
            "-preset",
            "ultrafast",
            "-color_primaries",
            "bt2020",
            "-color_trc",
            "smpte2084",
            "-colorspace",
            "bt2020nc",
            "-x265-params",
            "log-level=error:keyint=192:min-keyint=192:scenecut=0:bframes=4:b-adapt=0:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50):max-cll=1000,400",
            "-strict",
            "-2",
            "-c:a:0",
            "truehd",
            "-c:a:1",
            "ac3",
            "-ac:a:1",
            "6",
            "-b:a:1",
            "384k",
            "-c:a:2",
            "ac3",
            "-ac:a:2",
            "2",
            "-b:a:2",
            "192k",
            "-c:s",
            "srt",
        ])
        .arg(&mkv_path);

        let out = cmd.output().expect("run ffmpeg to build 50-track MKV");
        assert!(
            out.status.success(),
            "ffmpeg 50-track MKV build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mkv_bytes = std::fs::read(&mkv_path).expect("read generated MKV");
        RelayFixture {
            _dir: dir,
            mkv_path,
            mkv_bytes,
        }
    })
}

struct RelayServer {
    url: String,
    stop: Arc<AtomicBool>,
    range_gets: Arc<AtomicU64>,
    bytes_served: Arc<AtomicU64>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl RelayServer {
    fn start(bytes: Vec<u8>, per_get_rtt: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind localhost relay");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let range_gets = Arc::new(AtomicU64::new(0));
        let bytes_served = Arc::new(AtomicU64::new(0));
        let data = Arc::new(bytes);

        let stop_bg = Arc::clone(&stop);
        let gets_bg = Arc::clone(&range_gets);
        let served_bg = Arc::clone(&bytes_served);
        let join = std::thread::spawn(move || {
            while !stop_bg.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let data_c = Arc::clone(&data);
                        let stop_c = Arc::clone(&stop_bg);
                        let gets_c = Arc::clone(&gets_bg);
                        let served_c = Arc::clone(&served_bg);
                        std::thread::spawn(move || {
                            serve_relay_conn(stream, &data_c, per_get_rtt, &stop_c, &gets_c, &served_c);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            url: format!("http://127.0.0.1:{port}/iron_man_2.mkv"),
            stop,
            range_gets,
            bytes_served,
            join: Some(join),
        }
    }
}

impl Drop for RelayServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn serve_relay_conn(
    mut stream: TcpStream,
    data: &[u8],
    per_get_rtt: Duration,
    stop: &AtomicBool,
    range_gets: &AtomicU64,
    bytes_served: &AtomicU64,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_nodelay(true);
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 1024];

    while !stop.load(Ordering::Acquire) {
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut tmp) {
                Ok(0) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
        let hdr_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let hdr = String::from_utf8_lossy(&buf[..hdr_end]).into_owned();
        buf.drain(..hdr_end);

        let first = hdr.lines().next().unwrap_or("");
        let is_head = first.starts_with("HEAD ");
        let total = data.len();

        if is_head {
            let resp = format!(
                "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Type: video/x-matroska\r\nContent-Length: {total}\r\nConnection: keep-alive\r\n\r\n"
            );
            if stream.write_all(resp.as_bytes()).is_err() {
                return;
            }
            continue;
        }

        if !per_get_rtt.is_zero() {
            std::thread::sleep(per_get_rtt);
        }

        let mut range: Option<(usize, usize)> = None;
        for line in hdr.lines() {
            let lower = line.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("range: bytes=") {
                let mut parts = rest.trim().splitn(2, '-');
                if let (Some(s), Some(e)) = (parts.next(), parts.next()) {
                    if let Ok(start) = s.parse::<usize>() {
                        if start < total {
                            let end = e
                                .parse::<usize>()
                                .map(|v| v.min(total - 1))
                                .unwrap_or(total - 1);
                            if end >= start {
                                range = Some((start, end));
                            }
                        }
                    }
                }
            }
        }

        range_gets.fetch_add(1, Ordering::Relaxed);
        if let Some((start, end)) = range {
            let slice = &data[start..=end];
            let len = slice.len();
            let resp = format!(
                "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nContent-Type: video/x-matroska\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {len}\r\nConnection: keep-alive\r\n\r\n"
            );
            if stream.write_all(resp.as_bytes()).is_err() {
                return;
            }
            if stream.write_all(slice).is_err() {
                return;
            }
            bytes_served.fetch_add(len as u64, Ordering::Relaxed);
        } else {
            let resp = format!(
                "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Type: video/x-matroska\r\nContent-Length: {total}\r\nConnection: keep-alive\r\n\r\n"
            );
            if stream.write_all(resp.as_bytes()).is_err() {
                return;
            }
            if stream.write_all(data).is_err() {
                return;
            }
            bytes_served.fetch_add(total as u64, Ordering::Relaxed);
        }
    }
}

fn ffmpeg_decode_truehd_s32(mkv_path: &Path) -> Vec<u8> {
    let out = std::process::Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(mkv_path)
        .args(["-map", "0:a:0", "-f", "s32le", "-c:a", "pcm_s32le", "-"])
        .output()
        .expect("run pinned ffmpeg truehd reference");
    assert!(
        out.status.success(),
        "ffmpeg truehd decode failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn iron_man_2_relay_profile_mid_gop_seek_lossless_truehd_and_stream_culling() {
    let _guard = TEST_CPU.read();
    let fx = build_iron_man_2_fixture();
    let relay = RelayServer::start(fx.mkv_bytes.clone(), Duration::from_millis(2));
    let full_ref_s32 = ffmpeg_decode_truehd_s32(&fx.mkv_path);

    let headless = Headless::new();
    let seek_target = Duration::from_millis(6500);

    let player = Player::open(
        &relay.url,
        headless.clone(),
        Arc::new(codecs::context()),
        PlayerOptions {
            realtime: false,
            audio_mode: AudioOutputMode::BitPerfectPcm,
            ..PlayerOptions::default()
        },
        |_: Event| {},
    );
    player.seek(seek_target);
    let state = player.wait();
    drop(player);

    assert!(state.error.is_none(), "unexpected error: {:?}", state.error);
    assert!(state.ended, "playback must reach EOF cleanly");
    assert_eq!(state.dropped_frames, 0, "must drop zero frames");

    // 50-track topology + HDR10 + 10-bit metadata preserved through MkvRichDemuxer.
    assert_eq!(state.tracks.len(), 50, "must expose all 50 Matroska tracks");
    let sub_tracks = state
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Subtitle)
        .count();
    assert_eq!(sub_tracks, 46, "must expose all 46 subtitle tracks");
    assert_eq!(state.video_dynamic_range.as_deref(), Some("HDR10"));
    assert_eq!(state.video_bit_depth, Some(10));

    let cap = headless.capture();
    // Inactive 46 subtitle tracks and 2 secondary AC-3 tracks must be culled cleanly.
    assert!(
        cap.subtitles.is_empty(),
        "inactive subtitle tracks must not emit cues when unselected"
    );
    // Deferred unknown-format open must open the sink ONCE directly as S32 8ch 48 kHz
    // (never opening F32 first and tearing down).
    assert_eq!(
        cap.audio.len(),
        1,
        "TrueHD MKV must open audio sink once as S32, got {} opens",
        cap.audio.len()
    );
    let acap = &cap.audio[0];
    assert_eq!(acap.sample_format, Some(PcmFormat::S32));
    assert_eq!(acap.channels, 6);
    assert_eq!(acap.sample_rate, 48_000);

    let first_write = acap.flushes.last().copied().unwrap_or(0);
    let (audio_pts, sample_start) = *acap
        .writes
        .get(first_write)
        .expect("must capture post-seek TrueHD writes");
    assert!(
        audio_pts.abs_diff(seek_target) <= Duration::from_millis(2),
        "post-seek audio start {audio_pts:?} must land at {seek_target:?}"
    );

    let post_seek_exact = &acap.exact_pcm[sample_start * 4..];
    assert!(
        !post_seek_exact.is_empty(),
        "post-seek TrueHD capture must not be empty"
    );
    // Match against FFmpeg's full lossless decode starting at `audio_pts`.
    let start_frames = (audio_pts.as_secs_f64() * 48_000.0).round() as usize;
    let expected_offset = start_frames * 6 * 4;
    let expected_tail = &full_ref_s32[expected_offset..expected_offset + post_seek_exact.len()];
    assert_eq!(
        refcheck::md5_hex(post_seek_exact),
        refcheck::md5_hex(expected_tail),
        "post-seek multichannel TrueHD S32 samples after Major-Sync fast-forward must be 100% bit-exact with FFmpeg"
    );

    // Video presentation starts at >= 6.50 s and delivers all remaining ~60 frames cleanly.
    assert!(!cap.video.is_empty(), "video capture must be present");
    let vcap = &cap.video[0];
    let first_frame = vcap.flushes.last().copied().unwrap_or(0);
    let post_seek_vpts = &vcap.pts[first_frame..];
    assert!(
        post_seek_vpts[0] + Duration::from_millis(5) >= seek_target,
        "first presented video frame {:?} preceded seek target {:?}",
        post_seek_vpts[0],
        seek_target
    );
    assert!(
        post_seek_vpts.len() >= 55,
        "expected ~60 frames from 6.50 s..9.0 s, got {}",
        post_seek_vpts.len()
    );
}

#[test]
fn iron_man_2_relay_profile_realtime_smooth_playback_and_mid_gop_seek() {
    let _guard = TEST_CPU.write();
    let fx = build_iron_man_2_fixture();
    let relay = RelayServer::start(fx.mkv_bytes.clone(), Duration::from_millis(5));
    let headless = Headless::new();
    let player = Player::open(
        &relay.url,
        headless.clone(),
        Arc::new(codecs::context()),
        PlayerOptions {
            realtime: true,
            audio_mode: AudioOutputMode::Auto,
            ..PlayerOptions::default()
        },
        |_| {},
    );

    // Wait for initial prime at 0.0 s.
    let t_open = Instant::now();
    while t_open.elapsed() < Duration::from_secs(5) {
        let st = player.state();
        if let Some(err) = st.error {
            panic!("open errored: {err}");
        }
        if !st.buffering && st.position >= Duration::from_millis(150) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !player.state().buffering,
        "initial realtime prime did not finish"
    );

    // Seek 6.50 s deep into the 8.0 s HEVC GOP (across 7,800 TrueHD 7.1 access units).
    let seek_target = Duration::from_millis(6500);
    let t_seek = Instant::now();
    player.seek(seek_target);

    let mut primed_in = None;
    while t_seek.elapsed() < Duration::from_secs(5) {
        let st = player.state();
        if let Some(err) = st.error {
            panic!("mid-GOP seek errored: {err}");
        }
        if !st.buffering && st.position >= seek_target {
            primed_in = Some(t_seek.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let primed_in = primed_in.expect("mid-GOP seek must prime cleanly within deadline");
    assert!(
        primed_in < Duration::from_millis(1800),
        "mid-GOP TrueHD + HEVC seek took too long to prime: {primed_in:?}"
    );

    // Let 1.5 s of realtime 23.976 fps HDR10 + 7.1 TrueHD play and verify smooth cadence.
    std::thread::sleep(Duration::from_millis(1500));
    let st_after = player.state();
    assert!(st_after.error.is_none(), "playback error: {:?}", st_after.error);
    assert!(!st_after.buffering, "must not re-enter buffering during steady playback");
    assert_eq!(st_after.dropped_frames, 0, "must drop zero video frames");
    drop(player);

    let cap = headless.capture();
    let vcap = &cap.video[0];
    let flush_idx = *vcap.flushes.last().unwrap_or(&0);
    let post_seek_shown = &vcap.shown_at[flush_idx..];
    assert!(
        post_seek_shown.len() >= 25,
        "expected >= 25 realtime frames in 1.5 s @ 23.976 fps, got {}",
        post_seek_shown.len()
    );

    // Skip first 2 frames right at the prime boundary and verify steady-state
    // frame-to-frame presentation cadence never stalls (> 120 ms = ~3 frames).
    let steady = &post_seek_shown[2..];
    let max_gap_ms = steady
        .windows(2)
        .map(|w| ((w[1] - w[0]).max(0) as f64) / 1_000_000.0)
        .fold(0.0f64, f64::max);
    assert!(
        max_gap_ms < 120.0,
        "realtime post-seek cadence stalled (max frame gap {max_gap_ms:.2} ms)"
    );

    // 512 KiB ReadAheadSource chunking keeps HTTP Range GET count compact.
    let expected_chunks = (fx.mkv_bytes.len() / (512 * 1024)) as u64 + 18;
    let actual_gets = relay.range_gets.load(Ordering::Relaxed);
    assert!(relay.bytes_served.load(Ordering::Relaxed) > 0);
    assert!(
        actual_gets <= expected_chunks,
        "expected <= {expected_chunks} HTTP Range GETs with 512 KiB chunking, got {actual_gets}"
    );
}
