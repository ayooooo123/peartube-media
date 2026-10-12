//! Damaged input. Every decoder gets packets and setup bytes from the
//! reference inputs, damaged at random from a fixed seed: it must return
//! errors rather than panic, and every frame it does return must hold
//! the samples its reported layout says.
//!
//! A failure names the input and the case; the same seed replays it.

mod support;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use oxideav_core::{Decoder, Error, Frame, RuntimeContext};
use refcheck::fate;
use support::{archive, audio_packets, track_caf};

/// Damaged packets per input, then damaged setups per input.
const CASES: usize = 2500;

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// 0 .. n (0 for n = 0).
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// `data` damaged one of five ways: bits flipped, cut short, a run of
/// random bytes, a run of 0x00 or 0xff, random bytes appended.
fn damage(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    let len = out.len();
    match rng.below(5) {
        0 if len > 0 => {
            for _ in 0..=rng.below(8) {
                let at = rng.below(len);
                out[at] ^= 1 << rng.below(8);
            }
        }
        1 => out.truncate(rng.below(len)),
        2 if len > 0 => {
            let at = rng.below(len);
            let n = 1 + rng.below(16.min(len - at));
            for b in &mut out[at..at + n] {
                *b = rng.next() as u8;
            }
        }
        3 if len > 0 => {
            let at = rng.below(len);
            let n = 1 + rng.below(64.min(len - at));
            out[at..at + n].fill(if rng.below(2) == 0 { 0 } else { 0xff });
        }
        _ => {
            for _ in 0..=rng.below(32) {
                out.push(rng.next() as u8);
            }
        }
    }
    out
}

/// Rewrites a damaged packet's check bytes so the damage gets past the
/// check into the parsers, as a peer that computes them would send it.
/// Given the frame size the setup bytes declare.
type Seal = fn(&mut [u8], usize);

/// The frame size (`checksum_size`) the `QDCA` atom of QDM2 and QDMC
/// setup bytes declares.
fn qdca_frame_size(extradata: &[u8]) -> usize {
    let at = extradata.windows(4).position(|w| w == b"QDCA").expect("a QDCA atom");
    u32::from_be_bytes(extradata[at + 28..at + 32].try_into().unwrap()) as usize
}

/// QDM2 superblocks of types 2, 4 and 5 open with two check bytes b0, b1
/// after their header: 257 * b0 + 2 * b1 equals the sum of the
/// superblock's bytes (mod 2^16), so 256 * b0 + b1 is the sum of the
/// other bytes.
fn seal_qdm2(data: &mut [u8], size: usize) {
    let Some(&kind) = data.first() else { return };
    let at = 2 + usize::from(kind & 0x80 != 0);
    if ![2, 4, 5].contains(&(kind & 0x7f)) || size < at + 2 || data.len() < size {
        return;
    }
    let sum = data[..size].iter().map(|&b| u32::from(b)).sum::<u32>() - u32::from(data[at]) - u32::from(data[at + 1]);
    data[at] = (sum >> 8) as u8;
    data[at + 1] = sum as u8;
}

/// QDMC frames open with the label `QMC\1` and a little-endian sum of the
/// bytes after it plus 226.
fn seal_qdmc(data: &mut [u8], size: usize) {
    if size < 6 || data.len() < size {
        return;
    }
    data[..4].copy_from_slice(b"QMC\x01");
    let sum = data[6..size].iter().fold(226u16, |s, &b| s.wrapping_add(u16::from(b)));
    data[4..6].copy_from_slice(&sum.to_le_bytes());
}

/// Receives every ready frame and checks it against the reported layout.
fn drain(decoder: &mut dyn Decoder) {
    loop {
        let frame = match decoder.receive_frame() {
            Ok(Frame::Audio(frame)) => frame,
            Ok(_) => panic!("a frame that is not audio"),
            Err(Error::NeedMore) => return,
            Err(e) => panic!("receive_frame: {e}"),
        };
        let format = decoder.output_audio_format().expect("a layout for every frame");
        let channels = usize::from(format.channels);
        let plane = frame.samples as usize * format.sample_format.bytes_per_sample();
        if format.sample_format.is_planar() {
            assert_eq!(frame.data.len(), channels, "planes");
            assert!(frame.data.iter().all(|p| p.len() == plane), "plane sizes");
        } else {
            assert_eq!(frame.data.len(), 1, "planes");
            assert_eq!(frame.data[0].len(), plane * channels, "interleaved size");
        }
    }
}

/// `CASES` damaged packets into one decoder, a clean packet now and then
/// so it runs in a realistic state, most damaged ones resealed where the
/// codec checks its frames; then `CASES` decoders made from damaged setup
/// bytes and stream parameters, each fed three packets.
fn survive(path: &Path, seed: u64, seal: Option<Seal>) {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let (params, packets) = audio_packets(path);
    assert!(!packets.is_empty(), "{name}: no packets");
    let frame_size = if seal.is_some() { qdca_frame_size(&params.extradata) } else { 0 };
    let mut ctx = RuntimeContext::new();
    codec_apple_audio::register(&mut ctx);
    let mut rng = Rng(seed);

    let mut decoder = ctx.codecs.first_decoder(&params).unwrap_or_else(|e| panic!("{name}: {e}"));
    for case in 0..CASES {
        let mut packet = packets[rng.below(packets.len())].clone();
        if rng.below(8) != 0 {
            packet.data = damage(&mut rng, &packet.data);
            if let Some(seal) = seal.filter(|_| rng.below(4) != 0) {
                seal(&mut packet.data, frame_size);
            }
        }
        let run = catch_unwind(AssertUnwindSafe(|| {
            let _ = decoder.send_packet(&packet);
            drain(decoder.as_mut());
        }));
        assert!(run.is_ok(), "{name}: packet case {case} (seed {seed:#x}) panicked");
    }

    for case in 0..CASES {
        let mut setup = params.clone();
        setup.extradata = damage(&mut rng, &params.extradata);
        if rng.below(4) == 0 {
            setup.channels = Some(rng.below(70) as u16);
        }
        if rng.below(4) == 0 {
            setup.sample_rate = Some(rng.next() as u32);
        }
        let picks: Vec<usize> = (0..3).map(|_| rng.below(packets.len())).collect();
        let run = catch_unwind(AssertUnwindSafe(|| {
            if let Ok(mut decoder) = ctx.codecs.first_decoder(&setup) {
                for &p in &picks {
                    let _ = decoder.send_packet(&packets[p]);
                    drain(decoder.as_mut());
                }
            }
        }));
        assert!(run.is_ok(), "{name}: setup case {case} (seed {seed:#x}) panicked");
    }
}

#[test]
fn alac_survives_damage() {
    survive(&fate("lossless-audio/inside.m4a"), 0xA1AC, None);
}

#[test]
fn qdm2_survives_damage() {
    survive(&track_caf(&fate("qt-surge-suite/surge-2-16-B-QDM2.mov")), 0x0D32, Some(seal_qdm2));
    survive(&track_caf(&archive("A-codecs/QDM2/sweep/0-22050HzSweep8kb.mov")), 0x0D33, Some(seal_qdm2));
    survive(&track_caf(&archive("A-codecs/QDM2/fft8/resurrection.mov")), 0x0D34, Some(seal_qdm2));
}

#[test]
fn qdmc_survives_damage() {
    survive(&track_caf(&archive("A-codecs/QDMC/rumcoke.mov")), 0x0DC1, Some(seal_qdmc));
    survive(&track_caf(&archive("A-codecs/QDMC/tidemo1-24bit-rle.mov")), 0x0DC2, Some(seal_qdmc));
}

#[test]
fn mace_survives_damage() {
    survive(&track_caf(&fate("qt-surge-suite/surge-2-8-MAC3.mov")), 0x3AC3, None);
    survive(&track_caf(&fate("qt-surge-suite/surge-1-8-MAC6.mov")), 0x3AC6, None);
}
