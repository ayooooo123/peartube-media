// Ported from FFmpeg tests/fate/wavpack.mak (commit 2da55bf)
// License: LGPL-2.1-or-later

#![forbid(unsafe_code)]

use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use oxideav_core::{CodecId, CodecParameters, Decoder, Demuxer, Frame, Packet};
use refcheck::fate;

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 { 0x1234_5678_9ABC_DEF0 } else { seed })
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, upper: usize) -> usize {
        if upper == 0 {
            0
        } else {
            (self.next() as usize) % upper
        }
    }
}

fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut d = data.to_vec();
    match rng.below(6) {
        0 => {
            let len = rng.below(d.len() + 1);
            d.truncate(len);
        }
        1 => {
            let count = 1 + rng.below(16);
            for _ in 0..count {
                if !d.is_empty() {
                    let idx = rng.below(d.len());
                    d[idx] ^= 1 << rng.below(8);
                }
            }
        }
        2 => {
            let count = 1 + rng.below(16);
            for _ in 0..count {
                if !d.is_empty() {
                    let idx = rng.below(d.len());
                    d[idx] = rng.next() as u8;
                }
            }
        }
        3 => {
            let n = d.len().min(32);
            if n > 0 {
                let idx = rng.below(n);
                d[idx] = rng.next() as u8;
            }
        }
        4 => {
            if d.len() > 4 {
                let cut = rng.below(d.len());
                d.truncate(cut.max(1));
                let extra = rng.below(64);
                d.extend((0..extra).map(|_| rng.next() as u8));
            }
        }
        _ => {
            let start = rng.below(d.len() + 1);
            let end = (start + rng.below(32)).min(d.len());
            let fill = if rng.below(2) == 0 { 0x00 } else { 0xff };
            d[start..end].fill(fill);
        }
    }
    d
}

fn drain_and_verify(dec: &mut Box<dyn Decoder>) {
    while let Ok(frame) = dec.receive_frame() {
        let format = dec
            .output_audio_format()
            .expect("every decoded frame must have a reported output format");
        let Frame::Audio(audio) = frame else {
            panic!("expected audio frame");
        };
        assert_eq!(
            audio.data.len(),
            format.channels as usize,
            "plane count must equal channel count"
        );
        let bpp = format.sample_format.bytes_per_sample();
        let expected_bytes = audio.samples as usize * bpp;
        for (c, plane) in audio.data.iter().enumerate() {
            assert_eq!(
                plane.len(),
                expected_bytes,
                "plane {c} size must equal samples * bpp"
            );
        }
    }
}

fn load_packets(path: &Path) -> Vec<Packet> {
    let file = File::open(path).expect("open file");
    let ctx = oxideav_core::RuntimeContext::new();
    let mut demuxer =
        codec_wavpack::demuxer::RawWvDemuxer::open(Box::new(file), &ctx.codecs).expect("open demuxer");
    let mut pkts = Vec::new();
    while let Ok(pkt) = demuxer.next_packet() {
        pkts.push(pkt);
    }
    pkts
}

fn fuzz_decoder(rel_path: &str, seed: u64, mutations: usize) {
    let path = fate(rel_path);
    let original_pkts = load_packets(&path);
    assert!(!original_pkts.is_empty(), "must have packets to mutate");

    let params = CodecParameters::audio(CodecId::new("wavpack"));
    let mut rng = Rng::new(seed);

    for i in 0..mutations {
        let mut dec = codec_wavpack::decoder::make_decoder(&params).expect("make decoder");
        let pkt_idx = rng.below(original_pkts.len());
        let orig = &original_pkts[pkt_idx];

        let mutated_data = mutate(&mut rng, &orig.data);
        let mutated_pkt = Packet::new(0, orig.time_base, mutated_data)
            .with_pts(orig.pts.unwrap_or(0))
            .with_dts(orig.dts.unwrap_or(0))
            .with_keyframe(true);

        let _ = dec.send_packet(&mutated_pkt);
        drain_and_verify(&mut dec);
        let _ = dec.flush();
        drain_and_verify(&mut dec);

        // Also test sending multiple packets sequentially with mutations
        if i % 10 == 0 && original_pkts.len() > 1 {
            let next_orig = &original_pkts[(pkt_idx + 1) % original_pkts.len()];
            let mut_data2 = mutate(&mut rng, &next_orig.data);
            let mut_pkt2 = Packet::new(0, next_orig.time_base, mut_data2)
                .with_pts(next_orig.pts.unwrap_or(0))
                .with_dts(next_orig.dts.unwrap_or(0))
                .with_keyframe(true);
            let _ = dec.send_packet(&mut_pkt2);
            drain_and_verify(&mut dec);
            let _ = dec.flush();
            drain_and_verify(&mut dec);
        }
    }

    eprintln!("{rel_path}: {mutations} decoder packet mutations survived without panic");
}

fn fuzz_demuxer(rel_path: &str, seed: u64, mutations: usize) {
    let path = fate(rel_path);
    let mut file = File::open(&path).expect("open file");
    let mut original_bytes = Vec::new();
    file.read_to_end(&mut original_bytes).expect("read file");

    let ctx = oxideav_core::RuntimeContext::new();
    let mut rng = Rng::new(seed);

    for _ in 0..mutations {
        let mutated_bytes = mutate(&mut rng, &original_bytes);
        let cursor = Cursor::new(mutated_bytes);
        if let Ok(mut demuxer) =
            codec_wavpack::demuxer::RawWvDemuxer::open(Box::new(cursor), &ctx.codecs)
        {
            while let Ok(_) = demuxer.next_packet() {}
        }
    }

    eprintln!("{rel_path}: {mutations} demuxer file mutations survived without panic");
}

// ───────────────────────── Decoder Robustness (≥ 2000 mutants total) ─────────────────────────

#[test]
fn decoder_robustness_multichannel() {
    // 5.1 multichannel 16-bit
    fuzz_decoder(
        "wavpack/num_channels/panslab_sample_5.1_16bit-partial.wv",
        0x5756_0001_CAFE_0001,
        500,
    );
}

#[test]
fn decoder_robustness_float() {
    // 32-bit float
    fuzz_decoder(
        "wavpack/lossless/32bit_float-partial.wv",
        0x5756_0002_CAFE_0002,
        500,
    );
}

#[test]
fn decoder_robustness_hybrid() {
    // 4.0 hybrid lossy 16-bit
    fuzz_decoder(
        "wavpack/lossy/4.0_16-bit.wv",
        0x5756_0003_CAFE_0003,
        500,
    );
}

#[test]
fn decoder_robustness_dsd() {
    // DSD
    fuzz_decoder(
        "wavpack/lossless/dsd.wv",
        0x5756_0004_CAFE_0004,
        500,
    );
}

#[test]
fn decoder_robustness_lossless_stereo() {
    // 16-bit stereo lossless
    fuzz_decoder(
        "wavpack/lossless/16bit-partial.wv",
        0x5756_0005_CAFE_0005,
        500,
    );
}

// ───────────────────────── Demuxer Robustness (≥ 2000 mutants total) ─────────────────────────

#[test]
fn demuxer_robustness_multichannel() {
    fuzz_demuxer(
        "wavpack/num_channels/panslab_sample_5.1_16bit-partial.wv",
        0x5756_0006_BEEF_0001,
        500,
    );
}

#[test]
fn demuxer_robustness_float() {
    fuzz_demuxer(
        "wavpack/lossless/32bit_float-partial.wv",
        0x5756_0007_BEEF_0002,
        500,
    );
}

#[test]
fn demuxer_robustness_hybrid() {
    fuzz_demuxer(
        "wavpack/lossy/4.0_16-bit.wv",
        0x5756_0008_BEEF_0003,
        500,
    );
}

#[test]
fn demuxer_robustness_dsd() {
    fuzz_demuxer(
        "wavpack/lossless/dsd.wv",
        0x5756_0009_BEEF_0004,
        500,
    );
}

#[test]
fn demuxer_robustness_lossless_stereo() {
    fuzz_demuxer(
        "wavpack/lossless/16bit-partial.wv",
        0x5756_000A_BEEF_0005,
        500,
    );
}
