//! Untrusted input: truncated, bit-flipped and overwritten copies of the
//! packets of every codec and packaging (VP5 and VP6 in AVI, VP6F and VP6A
//! in FLV, VP6A in MOV) go through one decoder per stream, in order, with
//! a fixed seed: no panic, nothing slow.

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
fn packets(sample: &str, format: &str, n: usize) -> (CodecParameters, Vec<Packet>) {
    let mut ctx = RuntimeContext::new();
    codec_vp56::register(&mut ctx);
    oxideav_avi::__oxideav_entry(&mut ctx);
    oxideav_mov::registry::register(&mut ctx);
    oxideav_flv::register(&mut ctx);
    let mut d = ctx.containers.open_demuxer(format, Box::new(std::fs::File::open(fate(sample)).unwrap()), &ctx.codecs).unwrap();
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
        // The headers: the first bytes of the frame (and VP6A's offset).
        1 => {
            for _ in 0..1 + rng.below(3) {
                let at = rng.below(out.len().min(12));
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
    let samples = [
        ("vp5/potter512-400-partial.avi", "avi", 60),
        ("vp6/interlaced32x64.avi", "avi", 60),
        ("flash-vp6/clip1024.flv", "flv", 60),
        ("flash-vp6/300x180-Scr-f8-056alpha.flv", "flv", 60),
        ("flash-vp6/300x180-Scr-f8-056alpha.mov", "mov", 60),
    ];
    let mut rng = Rng(0x5EED_0F56_0000_0001);
    let mut mutations = 0;
    for (sample, format, n) in samples {
        let (params, packets) = packets(sample, format, n);
        assert!(!packets.is_empty(), "{sample}: packets");
        let mut ctx = RuntimeContext::new();
        codec_vp56::register(&mut ctx);
        let mut decoder = ctx.codecs.first_decoder(&params).unwrap();
        let start = Instant::now();
        for round in 0..9 {
            for p in &packets {
                // Every other round leaves some packets whole, so damage
                // meets decoded reference frames.
                let mut damaged = p.clone();
                if round % 2 == 0 || rng.below(3) != 0 {
                    damaged.data = mutate(&mut rng, &p.data);
                    mutations += 1;
                }
                if decoder.send_packet(&damaged).is_ok() {
                    while decoder.receive_frame().is_ok() {}
                }
            }
            if round == 4 {
                decoder.reset().unwrap();
            }
        }
        assert!(start.elapsed() < Duration::from_secs(120), "{sample}: decoding damaged packets is slow");
    }
    assert!(mutations >= 2000, "{mutations} mutations");
}
