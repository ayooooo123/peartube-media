//! Hostile input: cut and bit-flipped copies of FATE packets and file
//! headers go through every decoder and both demuxers, which must return
//! errors, never panic, and keep their output within the format's bounds
//! (fixed seeds, 2,000 mutants each).

use std::io::Cursor;

use check_decoders::{decode_packets, pinned_ffmpeg_packets};
use oxideav_core::{CodecId, CodecParameters, Error, Frame, Packet, RuntimeContext};

/// xorshift64*: a fixed sequence, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Flips 1 to 8 bits of `data`, or cuts it, or both.
fn mutate(rng: &mut Rng, data: &mut Vec<u8>) {
    let kind = rng.below(3);
    if kind != 1 && !data.is_empty() {
        for _ in 0..1 + rng.below(8) {
            let bit = rng.below(data.len() * 8);
            data[bit / 8] ^= 1 << (bit % 8);
        }
    }
    if kind != 0 {
        let keep = rng.below(data.len() + 1);
        data.truncate(keep);
    }
}

fn params(codec: &str, channels: u16, block_align: usize, extradata: &[u8]) -> CodecParameters {
    let mut p = CodecParameters::audio(CodecId::new(codec));
    p.sample_rate = Some(44_100);
    p.channels = Some(channels);
    p.extradata = extradata.to_vec();
    p.options.insert("block_align", block_align.to_string());
    p
}

/// The 14-byte ATRAC3 extradata of a WAV file (joint stereo or not).
fn atrac3_extradata(joint: bool) -> Vec<u8> {
    let mode = u8::from(joint);
    vec![1, 0, 0x44, 0xAC, 0, 0, mode, 0, mode, 0, 1, 0, 0, 0]
}

#[test]
fn decoders_survive_cut_and_flipped_packets() {
    let cases: [(&str, CodecParameters, u64); 6] = [
        (
            "atrac1/test_tones_small.aea",
            params("atrac1", 2, 424, &[]),
            0x6174_3101,
        ),
        (
            "atrac3/mc_sich_at3_066_small.wav",
            params("atrac3", 2, 192, &atrac3_extradata(true)),
            0x6174_3302,
        ),
        (
            "atrac3/mc_sich_at3_132_small.wav",
            params("atrac3", 2, 384, &atrac3_extradata(false)),
            0x6174_3303,
        ),
        (
            "atrac3/mc_sich_at3_066_small.wav",
            params("atrac3al", 2, 192, &[]),
            0x6174_3304,
        ),
        (
            "atrac3p/at3p_sample1.oma",
            params("atrac3plus", 2, 1488, &[]),
            0x6174_3305,
        ),
        (
            "atrac3p/at3p_sample1.oma",
            params("atrac3plusal", 2, 1488, &[]),
            0x6174_3306,
        ),
    ];
    for (sample, params, seed) in cases {
        let packets: Vec<Packet> = pinned_ffmpeg_packets(&refcheck::fate(sample), "a:0")
            .into_iter()
            .take(16)
            .collect();
        let max_samples = 2048;
        let mut rng = Rng(seed);
        for _ in 0..2_000 {
            let mut mutant = packets.clone();
            let victim = rng.below(mutant.len());
            mutate(&mut rng, &mut mutant[victim].data);
            let (decoded, _) = decode_packets(&[codec_atrac::register], &params, &mutant);
            let bytes: usize = mutant.iter().map(|p| p.data.len()).sum();
            assert!(
                decoded.frames.len() <= bytes,
                "{sample}: more frames than input bytes"
            );
            for frame in &decoded.frames {
                let Frame::Audio(a) = frame else {
                    panic!("{sample}: not audio")
                };
                assert!(
                    a.samples <= max_samples && a.data.len() == 2,
                    "{sample}: {} x {}",
                    a.samples,
                    a.data.len()
                );
            }
        }
    }
}

#[test]
fn demuxers_survive_cut_and_flipped_headers() {
    let mut ctx = RuntimeContext::new();
    codec_atrac::register(&mut ctx);
    for (sample, format, seed) in [
        ("aea/chirp.aea", "aea", 0x6165_6101u64),
        ("atrac3p/sonateno14op27-2-cut.aa3", "oma", 0x6f6d_6102),
        ("oma/01-Untitled-partial.oma", "oma", 0x6f6d_6103),
    ] {
        let file = std::fs::read(refcheck::fate(sample)).unwrap();
        let head = &file[..file.len().min(16 * 1024)];
        let mut rng = Rng(seed);
        for _ in 0..2_000 {
            let mut mutant = head.to_vec();
            // the damage lands in the headers or the first packets
            let span = mutant.len().min(4096);
            let mut part = mutant[..span].to_vec();
            mutate(&mut rng, &mut part);
            mutant.splice(..span, part);
            let Ok(mut demuxer) =
                ctx.containers
                    .open_demuxer(format, Box::new(Cursor::new(mutant)), &ctx.codecs)
            else {
                continue;
            };
            for _ in 0..64 {
                match demuxer.next_packet() {
                    // frames are at most 0x3FF * 8 + 8 bytes; AL blocks 65,535
                    Ok(p) => assert!(
                        p.data.len() <= 65_535,
                        "{sample}: packet of {} bytes",
                        p.data.len()
                    ),
                    Err(Error::Eof) => break,
                    Err(_) => break,
                }
            }
            let _ = demuxer.seek_to(0, 1_000);
        }
    }
}
