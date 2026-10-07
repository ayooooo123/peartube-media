// Untrusted-input robustness: truncated and bit-flipped copies of the
// reference samples must never panic — errors and empty output are fine.
// Deterministic: fixed seed LCG, 2000 mutations per format plus truncations.

use refcheck::fate;

/// Demux every packet of `data` through `format`; errors are fine, panics are not.
fn demux_all(format: &str, data: &[u8]) -> Result<(), oxideav_core::Error> {
    let mut ctx = oxideav_core::RuntimeContext::new();
    demux_misc::register(&mut ctx);
    let mut demuxer = ctx.containers.open_demuxer(
        format,
        Box::new(std::io::Cursor::new(data.to_vec())),
        &ctx.codecs,
    )?;
    loop {
        match demuxer.next_packet() {
            Ok(_) => {}
            Err(oxideav_core::Error::Eof) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*, fixed seed
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

/// Every format, including the previously missing multichannel and CAVS inputs.
const CASES: &[(&str, &str)] = &[
    ("ac3/monsters_inc_2.0_192_small.ac3", "ac3"),
    ("ac3/monsters_inc_5.1_448_small.ac3", "ac3"),
    ("eac3/csi_miami_5.1_256_spx_small.eac3", "eac3"),
    ("eac3/the_great_wall_7.1.eac3", "eac3"),
    ("mpeg2/sony-ct3.bs", "mpegvideo"),
    ("sub/Closedcaption_rollup.m2v", "mpegvideo"),
    ("h264/lossless.h264", "h264"),
    ("hevc-conformance/WPP_A_ericsson_MAIN_2.bit", "hevc"),
    ("cavs/cavs.mpg", "mpeg"),
    ("mpegps/pcm_aud.mpg", "mpeg"),
    ("mpeg2/dvd_single_frame.vob", "mpeg"),
    ("sub/vobsub.sub", "mpeg"),
    ("pva/PVA_test-partial.pva", "pva"),
    ("creative/BBC_2BIT.VOC", "voc"),
    ("caf/caf-pcm16.caf", "caf"),
    ("av1/seq_hdr_op_param_info.ivf", "ivf"),
];

#[test]
fn truncated_and_bit_flipped_files_never_panic() {
    for &(sample, format) in CASES {
        let data = std::fs::read(fate(sample)).unwrap();
        let mut rng = Rng(0x5EED_CAFE_F00D_0001);

        // Truncations: every 16th length from the full size down to 0.
        let mut len = data.len();
        while len > 0 {
            let _ = demux_all(format, &data[..len]);
            len = len.saturating_sub(data.len() / 16 + 1);
        }
        let _ = demux_all(format, &[]);

        // Bit flips: 2000 mutations per sample.
        for _ in 0..2000 {
            let mut mutated = data.clone();
            let mutations = 1 + (rng.next() % 8) as usize;
            for _ in 0..mutations {
                let pos = (rng.next() as usize) % mutated.len();
                mutated[pos] ^= (rng.next() & 0xFF) as u8 | 1;
            }
            let _ = demux_all(format, &mutated);
        }
    }
}

#[test]
fn nut_truncated_and_bit_flipped_never_panic() {
    // Mux a small NUT with FFmpeg (the reference muxer), then abuse it.
    let path = std::env::temp_dir().join("demux-misc-nut-fuzz.nut");
    let _ = std::fs::remove_file(&path);
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=1000:duration=0.3",
            "-f", "lavfi", "-i", "testsrc=duration=0.3:size=64x64:rate=10",
            "-c:a", "mp2", "-c:v", "mpeg2video", "-shortest",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(
        out.status.success(),
        "ffmpeg mux failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let data = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    let mut rng = Rng(0x5EED_CAFE_F00D_0002);
    let mut len = data.len();
    while len > 0 {
        let _ = demux_all("nut", &data[..len]);
        len = len.saturating_sub(data.len() / 16 + 1);
    }
    for _ in 0..2000 {
        let mut mutated = data.clone();
        let mutations = 1 + (rng.next() % 8) as usize;
        for _ in 0..mutations {
            let pos = (rng.next() as usize) % mutated.len();
            mutated[pos] ^= (rng.next() & 0xFF) as u8 | 1;
        }
        let _ = demux_all("nut", &mutated);
    }
}

#[test]
fn smf_truncated_and_bit_flipped_never_panic() {
    // Synthesize a minimal valid SMF: header + one track with a few events.
    let mut smf: Vec<u8> = Vec::new();
    smf.extend_from_slice(b"MThd");
    smf.extend_from_slice(&[0, 0, 0, 6]); // header length
    smf.extend_from_slice(&[0, 0]); // format 0
    smf.extend_from_slice(&[0, 1]); // 1 track
    smf.extend_from_slice(&[0, 96]); // division
    smf.extend_from_slice(b"MTrk");
    smf.extend_from_slice(&[0, 0, 0, 16]); // track length
    // note on / note off / end of track, with delta times
    smf.extend_from_slice(&[0, 0x90, 60, 64]);
    smf.extend_from_slice(&[32, 0x80, 60, 64]);
    smf.extend_from_slice(&[32, 0xFF, 0x2F, 0x00]);

    let mut rng = Rng(0x5EED_CAFE_F00D_0003);
    let mut len = smf.len();
    while len > 0 {
        let _ = demux_all("smf", &smf[..len]);
        len = len.saturating_sub(4);
    }
    for _ in 0..2000 {
        let mut mutated = smf.clone();
        let pos = (rng.next() as usize) % mutated.len();
        mutated[pos] ^= (rng.next() & 0xFF) as u8 | 1;
        let _ = demux_all("smf", &mutated);
    }
}

/// Open `data` as `format` through the player's registry, then seek it
/// the way the player does (its video stream, else audio, else stream 0)
/// to a few targets, reading a little after each: errors are fine, panics
/// are not. Returns what the first seek, to 1 s, returned.
fn seek_around(format: &str, data: &[u8]) -> oxideav_core::Result<i64> {
    let ctx = codecs::context();
    let mut demuxer = ctx.containers.open_demuxer(format, Box::new(std::io::Cursor::new(data.to_vec())), &ctx.codecs)?;
    let streams = demuxer.streams().to_vec();
    let Some(stream) = streams
        .iter()
        .find(|s| s.params.media_type == oxideav_core::MediaType::Video)
        .or_else(|| streams.iter().find(|s| s.params.media_type == oxideav_core::MediaType::Audio))
        .or(streams.first())
        .cloned()
    else {
        return Err(oxideav_core::Error::invalid("no stream"));
    };
    let tb = stream.time_base.as_rational();
    let ticks = |secs: i64| secs.saturating_mul(tb.den) / tb.num.max(1);
    let read = |demuxer: &mut Box<dyn oxideav_core::Demuxer>, n| {
        for _ in 0..n {
            if demuxer.next_packet().is_err() {
                break;
            }
        }
    };
    read(&mut demuxer, 2);
    let first = demuxer.seek_to(stream.index, ticks(1));
    read(&mut demuxer, 4);
    for target in [ticks(3600), 0, i64::MAX, -ticks(1)] {
        let _ = demuxer.seek_to(stream.index, target);
        read(&mut demuxer, 4);
    }
    first
}

/// Every seek path: generic index (raw AC-3/E-AC-3 and MPEG video, IVF),
/// read_seek (CAF, VOC, NUT with and without index) and timestamp
/// bisection (MPEG-PS, PVA) on truncated and bit-flipped copies: 2000
/// mutations per sample (fixed seed). Each seek reads at most up to the
/// input's end, so the whole run has a deadline; a hostile file that made
/// a seek loop would miss it.
#[test]
fn seeking_truncated_and_bit_flipped_files_never_panics_or_hangs() {
    let dir = std::env::temp_dir().join(format!("demux-misc-seek-fuzz-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cases: Vec<(String, &str, Vec<u8>)> = [
        ("ac3/monsters_inc_5.1_448_small.ac3", "ac3"),
        ("eac3/csi_miami_5.1_256_spx_small.eac3", "eac3"),
        ("mpeg2/sony-ct3.bs", "mpegvideo"),
        ("mpegps/pcm_aud.mpg", "mpeg"),
        ("mpeg2/dvd_single_frame.vob", "mpeg"),
        ("pva/PVA_test-partial.pva", "pva"),
        ("creative/BBC_2BIT.VOC", "voc"),
        ("caf/caf-pcm16.caf", "caf"),
        ("caf/aac.caf", "caf"),
        ("av1/seq_hdr_op_param_info.ivf", "ivf"),
    ]
    .into_iter()
    .map(|(sample, format)| (sample.to_string(), format, std::fs::read(fate(sample)).unwrap()))
    .collect();
    for (name, index) in [("indexed.nut", "1"), ("unindexed.nut", "0")] {
        let path = dir.join(name);
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=1000:duration=2", "-f", "lavfi", "-i"])
            .args(["testsrc=duration=2:size=64x64:rate=10", "-c:a", "mp2", "-c:v", "mpeg2video", "-g", "5", "-shortest"])
            .args(["-write_index", index])
            .arg(&path)
            .output()
            .expect("ffmpeg must be on PATH");
        assert!(out.status.success(), "ffmpeg mux failed: {}", String::from_utf8_lossy(&out.stderr));
        cases.push((name.to_string(), "nut", std::fs::read(&path).unwrap()));
    }
    let _ = std::fs::remove_dir_all(&dir);

    let total = cases.len();
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        for (name, format, data) in &cases {
            let mut rng = Rng(0x5EED_CAFE_F00D_0004);
            if let Err(e) = seek_around(format, data) {
                panic!("{name}: the intact sample does not seek: {e}");
            }
            let mut len = data.len();
            while len > 0 {
                let _ = seek_around(format, &data[..len]);
                len = len.saturating_sub(data.len() / 16 + 1);
            }
            for _ in 0..2000 {
                let mut mutated = data.clone();
                for _ in 0..1 + rng.next() % 8 {
                    let pos = (rng.next() as usize) % mutated.len();
                    mutated[pos] ^= (rng.next() & 0xFF) as u8 | 1;
                }
                let _ = seek_around(format, &mutated);
            }
            let _ = done.send(name.clone());
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
    let mut seen = 0;
    while seen < total {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match finished.recv_timeout(left) {
            Ok(_) => seen += 1,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("seeking mutated files missed the deadline after {seen} samples"),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    worker.join().expect("a seek panicked");
    assert_eq!(seen, total, "every sample seeked");
}
