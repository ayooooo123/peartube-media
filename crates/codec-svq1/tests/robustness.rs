//! Untrusted input: truncated, bit-flipped and overwritten copies of the
//! packets of both FATE SVQ1 samples (the plain header and the swapped
//! one with its embedded message and 12-bit size) go through the decoder,
//! in order, with a fixed seed: no panic, nothing slow, and every frame
//! that comes out has the planes of the size the decoder reports.

use std::time::{Duration, Instant};

use oxideav_core::{CodecParameters, Error, Frame, MediaType, Packet, RuntimeContext};
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
    codec_svq1::register(&mut ctx);
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
        // The frame header: code, type, size, the swapped words.
        1 => {
            for _ in 0..1 + rng.below(3) {
                let at = rng.below(out.len().min(40));
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
    let samples = [("svq1/marymary-shackles.mov", 200), ("svq1/ct_ending_cut.mov", 20)];
    let mut rng = Rng(0x5EED_5A91_0000_0001);
    let mut mutations = 0;
    for (sample, n) in samples {
        let (params, packets) = packets(sample, n);
        assert!(!packets.is_empty(), "{sample}: packets");
        let mut ctx = RuntimeContext::new();
        codec_svq1::register(&mut ctx);
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
                    let (w, h) = decoder.output_video_dimensions().unwrap();
                    let (w, h) = (w as usize, h as usize);
                    while let Ok(Frame::Video(f)) = decoder.receive_frame() {
                        let sizes: Vec<usize> = f.planes.iter().map(|p| p.data.len()).collect();
                        let c = w.div_ceil(4) * h.div_ceil(4);
                        assert_eq!(sizes, [w * h, c, c], "{sample}: planes of a {w}x{h} frame");
                    }
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
