//! Untrusted-input robustness tests for codec-ape:
//! - At least 2000 seeded truncated or bit-flipped mutants of packets (decoder)
//! - At least 2000 of file bytes (demuxer open and read to the end)
//! No panics, and every decoded frame matches the layout output_audio_format() reports.

use std::io::Cursor;

use oxideav_core::{CodecParameters, Frame, Packet, RuntimeContext};
use refcheck::fate;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    codec_ape::register(&mut ctx);
    ctx
}

fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut d = data.to_vec();
    match rng.below(5) {
        0 => {
            // Truncation
            d.truncate(rng.below(d.len() + 1));
        }
        1 => {
            // Bit flips
            let count = 1 + rng.below(16);
            for _ in 0..count {
                if !d.is_empty() {
                    let idx = rng.below(d.len());
                    d[idx] ^= 1 << rng.below(8);
                }
            }
        }
        2 => {
            // Byte overwrites
            let count = 1 + rng.below(16);
            for _ in 0..count {
                if !d.is_empty() {
                    let idx = rng.below(d.len());
                    d[idx] = rng.next() as u8;
                }
            }
        }
        3 => {
            // Zero a slice
            if !d.is_empty() {
                let start = rng.below(d.len());
                let len = 1 + rng.below(32);
                let end = (start + len).min(d.len());
                d[start..end].fill(0);
            }
        }
        _ => {
            // Truncate and append random bytes
            let cut = rng.below(d.len() + 1);
            d.truncate(cut);
            let add = rng.below(64);
            d.extend((0..add).map(|_| rng.next() as u8));
        }
    }
    d
}

#[test]
fn decoder_packet_mutants() {
    let ctx = context();
    let path = fate("lossless-audio/luckynight-partial.ape");
    let file = std::fs::File::open(&path).expect("open sample");
    let mut demuxer = codec_ape::open_ape(Box::new(file), &ctx.codecs).expect("open demuxer");

    let stream = demuxer.streams()[0].clone();
    let mut clean_packets = Vec::new();
    while let Ok(pkt) = demuxer.next_packet() {
        clean_packets.push(pkt);
    }
    assert!(!clean_packets.is_empty());

    let mut rng = Rng::new(0xDEAD_BEEF_CAFE_BABE);
    let mut decoder = ctx.codecs.first_decoder(&stream.params).expect("make decoder");

    for i in 0..2500 {
        if i % 100 == 0 {
            decoder.reset().unwrap_or(());
        }

        let base_pkt = &clean_packets[rng.below(clean_packets.len())];
        let mutated_data = mutate(&mut rng, &base_pkt.data);
        let mut pkt = Packet::new(0, stream.time_base, mutated_data);
        pkt.pts = base_pkt.pts;
        pkt.duration = base_pkt.duration;

        let _ = decoder.send_packet(&pkt);

        while let Ok(frame) = decoder.receive_frame() {
            let Frame::Audio(audio) = frame else { continue };
            let format = decoder.output_audio_format().expect("layout reported");
            assert_eq!(
                audio.data.len(),
                format.channels as usize,
                "frame channels mismatch"
            );
            let expected_plane_len =
                audio.samples as usize * format.sample_format.bytes_per_sample();
            for plane in &audio.data {
                assert_eq!(
                    plane.len(),
                    expected_plane_len,
                    "plane byte length must match layout"
                );
            }
        }
    }
}

#[test]
fn demuxer_file_mutants() {
    let ctx = context();
    let path = fate("lossless-audio/luckynight-partial.ape");
    let file_bytes = std::fs::read(&path).expect("read sample");

    let mut rng = Rng::new(0x1234_5678_9ABC_DEF0);

    for _ in 0..2500 {
        let mutated = mutate(&mut rng, &file_bytes);
        let cursor = Box::new(Cursor::new(mutated));

        if let Ok(mut demuxer) = codec_ape::open_ape(cursor, &ctx.codecs) {
            let mut guard = 0;
            while let Ok(_pkt) = demuxer.next_packet() {
                guard += 1;
                if guard > 200 {
                    break;
                }
            }
        }
    }
}

#[test]
fn corrupted_extradata_never_panics() {
    let mut rng = Rng::new(0x5555_AAAA_3333_CCCC);

    for _ in 0..1000 {
        let len = rng.below(32);
        let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let mut params = CodecParameters::audio(oxideav_core::CodecId::new("ape"));
        params.extradata = bytes;
        params.channels = Some(rng.below(8) as u16);
        params.sample_rate = Some(rng.below(192_000) as u32);
        let _ = codec_ape::make_decoder(&params);
    }
}
