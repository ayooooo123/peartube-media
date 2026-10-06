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

/// The formats and one FATE sample each.
const CASES: &[(&str, &str)] = &[
    ("ac3/monsters_inc_2.0_192_small.ac3", "ac3"),
    ("eac3/csi_miami_5.1_256_spx_small.eac3", "eac3"),
    ("mpeg2/sony-ct3.bs", "mpegvideo"),
    ("sub/Closedcaption_rollup.m2v", "mpegvideo"),
    ("h264/lossless.h264", "h264"),
    ("hevc-conformance/WPP_A_ericsson_MAIN_2.bit", "hevc"),
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
