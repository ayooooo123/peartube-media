//! CEA-708 output equals VLC's: the unmodified `modules/codec/cea708.c`
//! of the source this crate ports (vlc-src, `VLC_SRC`, default
//! `~/projects/vlc-src`) is built with stub VLC headers (`tests/vlc708/`)
//! into a harness that drives it as `modules/codec/cc.c` does. Both get the
//! same triplets — our extraction and timeline over each input — and every
//! subpicture must match: start and stop, each window's region (origin,
//! grid flags, alignment) and each text run with its style.
//!
//! Inputs: the rollup FATE sample (MPEG-2, A/53 Part 4 user data carrying
//! CEA-708 service 1) and the same captions re-encoded into H.264 in
//! Matroska and HEVC in TS.

mod support;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use subs_cc::cea708::{Cea708, Output};
use subs_cc::eia608::ticks_to_us;
use support::{generated, our_captions, process_dir, rollup};

fn vlc_src() -> PathBuf {
    let root = std::env::var_os("VLC_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("projects/vlc-src"));
    let file = root.join("modules/codec/cea708.c");
    assert!(file.is_file(), "missing {} (set VLC_SRC to a VLC source checkout)", file.display());
    root
}

/// The harness binary, built once per process.
fn harness() -> &'static Path {
    static HARNESS: LazyLock<PathBuf> = LazyLock::new(|| {
        let dir = process_dir().join("vlc708");
        std::fs::create_dir_all(&dir).unwrap();
        let stubs = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vlc708");
        for entry in std::fs::read_dir(&stubs).unwrap() {
            let path = entry.unwrap().path();
            std::fs::copy(&path, dir.join(path.file_name().unwrap())).unwrap();
        }
        let codec = vlc_src().join("modules/codec");
        for name in ["cea708.c", "cea708.h"] {
            std::fs::copy(codec.join(name), dir.join(name)).unwrap();
        }
        let binary = dir.join("harness");
        let out = Command::new("cc")
            .args(["-std=gnu11", "-O1", "-DNDEBUG", "-w", "-I"])
            .arg(&dir)
            .arg("-o")
            .arg(&binary)
            .arg(dir.join("harness.c"))
            .arg(dir.join("cea708.c"))
            .output()
            .expect("run cc");
        assert!(out.status.success(), "building the VLC harness: {}", String::from_utf8_lossy(&out.stderr));
        binary
    });
    &HARNESS
}

/// The timed pictures of `path` as (microseconds, triplet bytes).
fn input(path: &Path) -> Vec<(i64, Vec<u8>)> {
    let captions = our_captions(path);
    let (num, den) = captions.time_base;
    captions
        .pictures
        .iter()
        .filter_map(|(ts, triplets)| {
            let us = ts.and_then(|ts| ticks_to_us(ts, num, den))?;
            Some((us, triplets.iter().flatten().copied().collect()))
        })
        .collect()
}

/// VLC's output for `records`. Input and output go through files: a pipe
/// in each direction deadlocks once the harness writes more than a pipe
/// holds before it has read everything.
fn vlc_lines(records: &[(i64, Vec<u8>)]) -> Vec<String> {
    static RUN: AtomicUsize = AtomicUsize::new(0);
    let run = RUN.fetch_add(1, Ordering::SeqCst);
    let (input, output) = (process_dir().join(format!("vlc708-in-{run}")), process_dir().join(format!("vlc708-out-{run}")));
    let mut bytes = Vec::new();
    for (pts, data) in records {
        bytes.extend_from_slice(&pts.to_le_bytes());
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
    }
    std::fs::write(&input, bytes).unwrap();
    let mut child = Command::new(harness())
        .stdin(File::open(&input).unwrap())
        .stdout(File::create(&output).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the VLC harness ran past 120 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "VLC harness failed: {status}");
    std::fs::read_to_string(&output).unwrap().lines().map(str::to_string).collect()
}

fn format(output: &Output, lines: &mut Vec<String>) {
    lines.push(format!("OUT {} {} 1", output.start, output.stop));
    for r in &output.regions {
        lines.push(format!(
            "REGION {:08x} {:08x} {} {} {}",
            r.origin_x.to_bits(),
            r.origin_y.to_bits(),
            r.flags,
            r.align,
            r.inner_align
        ));
        for s in &r.segments {
            let hex: String = s.text.iter().map(|b| format!("{b:02x}")).collect();
            let st = &s.style;
            lines.push(format!(
                "SEG {hex} {} {} {:06x} {} {:06x} {} {:08x}",
                st.style_flags,
                st.features,
                st.font_color,
                st.font_alpha,
                st.background_color,
                st.background_alpha,
                st.font_relsize.to_bits()
            ));
        }
    }
    lines.push("END".to_string());
}

fn our_lines(records: &[(i64, Vec<u8>)]) -> (Vec<String>, usize, usize) {
    let mut decoder = Cea708::new(1);
    let mut lines = Vec::new();
    let (mut outputs, mut with_text) = (0, 0);
    for (pts, data) in records {
        for output in decoder.decode(data, *pts) {
            outputs += 1;
            with_text += usize::from(!output.is_empty());
            format(&output, &mut lines);
        }
    }
    (lines, outputs, with_text)
}

fn assert_same_as_vlc(path: &Path, what: &str) {
    let records = input(path);
    let theirs = vlc_lines(&records);
    let (ours, outputs, with_text) = our_lines(&records);
    println!("{what}: {outputs} outputs ({with_text} with text), {} lines", ours.len());
    assert!(with_text > 0, "{what}: no CEA-708 text");
    for (i, (a, b)) in ours.iter().zip(&theirs).enumerate() {
        assert_eq!(a, b, "{what}: line {i} differs from VLC's");
    }
    assert_eq!(ours.len(), theirs.len(), "{what}: line count");
}

#[test]
fn rollup_service_1_from_mpeg2() {
    assert_same_as_vlc(&rollup(), "Closedcaption_rollup.m2v");
}

#[test]
fn rollup_service_1_from_h264_and_hevc() {
    assert_same_as_vlc(&generated("h264.mkv"), "H.264 in Matroska");
    assert_same_as_vlc(&generated("hevc.ts"), "HEVC in TS");
}

/// xorshift64*: a fixed-seed generator for the random services.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

/// Random service data: every command with arguments, text in G0, G1,
/// G2/G3 and P16. Window definitions keep at most 15 rows: VLC indexes
/// past its row array with 16, which C does not define.
fn random_service(rng: &mut Rng, len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while out.len() < len {
        match rng.below(24) {
            0..=2 => {
                out.push(0x98 + rng.below(8) as u8);
                out.extend([rng.byte(), rng.byte(), rng.byte()]);
                out.push((rng.byte() & 0xf0) | rng.below(15) as u8);
                out.extend([rng.byte(), rng.byte()]);
            }
            3 => out.push(0x80 + rng.below(8) as u8),
            4..=6 => out.extend([0x88 + rng.below(5) as u8, rng.byte()]),
            7 => out.extend([0x90, rng.byte(), rng.byte()]),
            8 => out.extend([0x91, rng.byte(), rng.byte(), rng.byte()]),
            9 => out.extend([0x92, rng.byte(), rng.byte()]),
            10 => out.extend([0x97, rng.byte(), rng.byte(), rng.byte(), rng.byte()]),
            11 => match rng.below(4) {
                0 => out.extend([0x8d, rng.below(4) as u8]),
                1 => out.push(0x8e),
                2 => out.push(0x8f),
                _ => out.push(0x93 + rng.below(4) as u8),
            },
            12 | 13 => out.push([0x00, 0x03, 0x08, 0x0c, 0x0d, 0x0e, 0x01, 0x11][rng.below(8) as usize]),
            14 => {
                out.push(0x10);
                out.push(rng.byte());
                for _ in 0..rng.below(6) {
                    out.push(rng.byte());
                }
            }
            15 => out.extend([0x18, rng.byte(), rng.byte()]),
            16 => out.push(0xa0 + rng.below(0x60) as u8),
            _ => {
                for _ in 0..1 + rng.below(8) {
                    out.push(0x20 + rng.below(0x60) as u8);
                }
            }
        }
    }
    out
}

/// Packs service data into DTVCC packets and those into triplets, with
/// service 2 blocks, 608 and invalid triplets mixed in, and the odd lost
/// packet; then into pictures of up to 20 triplets, 1/30 s apart.
fn random_pictures(seed: u64, pictures: usize) -> Vec<(i64, Vec<u8>)> {
    let mut rng = Rng(seed);
    let mut triplets: Vec<[u8; 3]> = Vec::new();
    let mut sequence = 0u8;
    while triplets.len() < pictures * 12 {
        let size_code = 1 + rng.below(63) as usize;
        let data_len = size_code * 2 - 1;
        let mut data = Vec::new();
        while data.len() + 2 <= data_len {
            let block_len = (1 + rng.below(31) as usize).min(data_len - data.len() - 1);
            let sid = if rng.below(8) == 0 { 2 } else { 1 };
            data.push((sid << 5) | block_len as u8);
            data.extend(random_service(&mut rng, block_len).into_iter().take(block_len));
            if rng.below(3) == 0 {
                break;
            }
        }
        data.resize(data_len, 0);
        if rng.below(40) == 0 {
            sequence = sequence.wrapping_add(2); // a lost packet
        }
        triplets.push([0xff, ((sequence & 3) << 6) | size_code as u8, data[0]]);
        sequence = sequence.wrapping_add(1);
        for pair in data[1..].chunks(2) {
            triplets.push([0xfe, pair[0], pair[1]]);
            match rng.below(16) {
                0 => triplets.push([0xfc, 0x94, 0x2c]),
                1 => triplets.push([0xfa, rng.byte(), rng.byte()]),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    let mut at = 0usize;
    let mut pts = 1_000_000i64;
    while at < triplets.len() {
        let n = (1 + rng.below(20) as usize).min(triplets.len() - at);
        out.push((pts, triplets[at..at + n].iter().flatten().copied().collect()));
        at += n;
        pts += 33_367;
    }
    out
}

/// Every command, direction, window and character set, in random streams
/// (fixed seeds): the port and VLC queue the same subpictures.
#[test]
fn random_services_match_vlc() {
    for seed in [0x708, 0xcea708, 0xdcc_0001] {
        let records = random_pictures(seed, 3000);
        let theirs = vlc_lines(&records);
        let (ours, outputs, with_text) = our_lines(&records);
        println!("seed {seed:#x}: {outputs} outputs ({with_text} with text)");
        assert!(with_text > 100, "seed {seed:#x}: too little text to compare");
        for (i, (a, b)) in ours.iter().zip(&theirs).enumerate() {
            assert_eq!(a, b, "seed {seed:#x}: line {i} differs from VLC's");
        }
        assert_eq!(ours.len(), theirs.len(), "seed {seed:#x}: line count");
    }
}
