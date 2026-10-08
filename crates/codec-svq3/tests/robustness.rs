//! Untrusted input: truncated, bit-flipped and overwritten copies of the
//! packets of every FATE SVQ3 sample (B pictures, third-pel motion, the
//! watermark) and of their SEQH headers go through the decoder, in order,
//! with a fixed seed: no panic, nothing slow.

use std::time::{Duration, Instant};

use oxideav_core::{CodecParameters, Error, MediaType, Packet, RuntimeContext};
use refcheck::fate;

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// The video stream's parameters and its first `n` packets.
fn packets(sample: &str, n: usize) -> (CodecParameters, Vec<Packet>) {
    let mut ctx = RuntimeContext::new();
    codec_svq3::register(&mut ctx);
    oxideav_mov::registry::register(&mut ctx);
    let mut d = ctx.containers.open_demuxer("mov", Box::new(std::fs::File::open(fate(sample)).unwrap()), &ctx.codecs).unwrap();
    let stream = d.streams().iter().find(|s| s.params.media_type == MediaType::Video).unwrap().clone();
    let mut out = Vec::new();
    while out.len() < n {
        match d.next_packet() {
            Ok(p) if p.stream_index == stream.index => out.push(p),
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("{sample}: {e}"),
        }
    }
    (stream.params, out)
}

fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    if out.is_empty() {
        return out;
    }
    match rng.below(4) {
        0 => out.truncate(rng.below(out.len())),
        // The slice header and the first macroblocks.
        1 => {
            for _ in 0..1 + rng.below(3) {
                let at = rng.below(out.len().min(16));
                out[at] ^= 1 << rng.below(8);
            }
        }
        2 => {
            for _ in 0..1 + rng.below(16) {
                let at = rng.below(out.len());
                out[at] ^= 1 << rng.below(8);
            }
        }
        _ => {
            let at = rng.below(out.len());
            let len = 1 + rng.below(16);
            for b in out.iter_mut().skip(at).take(len) {
                *b = rng.next() as u8;
            }
        }
    }
    out
}

#[test]
fn damaged_packets_never_panic() {
    let samples = [("svq3/Vertical400kbit.sorenson3.mov", 200), ("svq3/svq3_watermark.mov", 10), ("svq3/svq3_decoding_regression.mov", 20)];
    let mut rng = Rng(0x5EED_5A93_0000_0001);
    let mut mutations = 0;
    for (sample, n) in samples {
        let (params, packets) = packets(sample, n);
        assert!(!packets.is_empty(), "{sample}: packets");
        let mut ctx = RuntimeContext::new();
        codec_svq3::register(&mut ctx);
        let mut decoder = ctx.codecs.first_decoder(&params).unwrap();
        let start = Instant::now();
        for round in 0..12 {
            for p in &packets {
                // Every other round leaves some packets whole, so damage
                // meets decoded reference pictures.
                let mut damaged = p.clone();
                if round % 2 == 0 || rng.below(3) != 0 {
                    damaged.data = mutate(&mut rng, &p.data);
                    mutations += 1;
                }
                if decoder.send_packet(&damaged).is_ok() {
                    while decoder.receive_frame().is_ok() {}
                }
            }
            if round == 6 {
                decoder.reset().unwrap();
            }
        }
        assert!(start.elapsed() < Duration::from_secs(120), "{sample}: decoding damaged packets is slow");
    }
    assert!(mutations >= 2000, "{mutations} mutations");
}

/// The SEQH header (sizes, flags, the watermark's zlib logo) damaged: the
/// decoder is made or refused, never a panic or a huge allocation.
#[test]
fn damaged_headers_never_panic() {
    let mut rng = Rng(0x5EED_5A93_0000_0002);
    for sample in ["svq3/Vertical400kbit.sorenson3.mov", "svq3/svq3_watermark.mov"] {
        let (params, packets) = packets(sample, 3);
        let mut ctx = RuntimeContext::new();
        codec_svq3::register(&mut ctx);
        for _ in 0..1000 {
            let mut p = params.clone();
            p.extradata = mutate(&mut rng, &params.extradata);
            if let Ok(mut decoder) = ctx.codecs.first_decoder(&p) {
                for pkt in &packets {
                    if decoder.send_packet(pkt).is_ok() {
                        while decoder.receive_frame().is_ok() {}
                    }
                }
            }
        }
    }
}
