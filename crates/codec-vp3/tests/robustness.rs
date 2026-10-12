//! Untrusted-input robustness: truncated, bit-flipped, byte-smashed and
//! reordered copies of each FATE sample's packets (and corrupted frame
//! sizes) must never panic the decoders. Deterministic: fixed seeds,
//! 2000 mutations per codec.

use oxideav_core::{CodecParameters, Decoder, MediaType, Packet, RuntimeContext};
use refcheck::fate;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    codec_vp3::register(&mut ctx);
    oxideav_avi::__oxideav_entry(&mut ctx);
    ctx
}

/// The parameters and packets of video stream 0 of `sample`.
fn packets(ctx: &RuntimeContext, sample: &str) -> (CodecParameters, Vec<Packet>) {
    let path = fate(sample);
    let file = std::fs::File::open(&path).expect("open sample");
    let mut demuxer = ctx.containers.open_demuxer("avi", Box::new(file), &ctx.codecs).expect("open demuxer");
    let stream = demuxer.streams().iter().find(|s| s.params.media_type == MediaType::Video).expect("video stream").clone();
    let mut out = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) if p.stream_index == stream.index => out.push(p),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(!out.is_empty(), "{sample}: no video packets");
    (stream.params, out)
}

fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut d = data.to_vec();
    match rng.below(7) {
        0 => d.truncate(rng.below(d.len() + 1)),
        1 => {
            for _ in 0..1 + rng.below(8) {
                if !d.is_empty() {
                    let i = rng.below(d.len());
                    d[i] ^= 1 << rng.below(8);
                }
            }
        }
        2 => {
            for _ in 0..1 + rng.below(16) {
                if !d.is_empty() {
                    let i = rng.below(d.len());
                    d[i] = rng.next() as u8;
                }
            }
        }
        3 => {
            // Frame header, superblock and mode data.
            let n = d.len().min(24);
            for _ in 0..1 + rng.below(4) {
                if n > 0 {
                    let i = rng.below(n);
                    d[i] = rng.next() as u8;
                }
            }
        }
        4 => {
            let n = rng.below(64);
            d = (0..n).map(|_| rng.next() as u8).collect();
        }
        5 => {
            // Bit flips in the coefficient data only.
            for _ in 0..1 + rng.below(4) {
                if d.len() > 64 {
                    let i = 64 + rng.below(d.len() - 64);
                    d[i] ^= 1 << rng.below(8);
                }
            }
        }
        _ => {
            if d.len() > 2 {
                let cut = rng.below(d.len());
                d.truncate(cut.max(1));
                d.extend((0..rng.below(32)).map(|_| rng.next() as u8));
            }
        }
    }
    d
}

fn drain(dec: &mut Box<dyn Decoder>) {
    while dec.receive_frame().is_ok() {}
}

fn fuzz(sample: &str, seed: u64) {
    let ctx = context();
    let (params, pkts) = packets(&ctx, sample);
    let mut rng = Rng(seed);
    let mut dec: Option<Box<dyn Decoder>> = None;
    // CODEC_VP3_FUZZ_ITERS raises the mutation count for longer local runs.
    let iters = std::env::var("CODEC_VP3_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000usize).max(2000);
    for i in 0..iters {
        if dec.is_none() || i % 97 == 0 {
            let mut p = params.clone();
            if rng.below(8) == 0 {
                // Corrupted frame size or container tag.
                p.width = Some(rng.below(1024) as u32);
                p.height = Some(rng.below(1024) as u32);
                if rng.below(2) == 0 {
                    p.tag = None;
                }
            }
            dec = ctx.codecs.first_decoder(&p).ok();
            if dec.is_none() {
                dec = Some(ctx.codecs.first_decoder(&params).expect("decoder"));
            }
        }
        let d = dec.as_mut().unwrap();
        // A short run of packets from a random position, one or more
        // mutated, sometimes out of order.
        let start = rng.below(pkts.len());
        let run = 1 + rng.below(4);
        for k in 0..run {
            let idx = if rng.below(10) == 0 { rng.below(pkts.len()) } else { (start + k) % pkts.len() };
            let mut pkt = pkts[idx].clone();
            if k == 0 || rng.below(3) == 0 {
                pkt.data = mutate(&mut rng, &pkt.data);
            }
            let _ = d.send_packet(&pkt);
            drain(d);
        }
        if rng.below(20) == 0 {
            let _ = d.flush();
            drain(d);
        }
        if rng.below(50) == 0 {
            let _ = d.reset();
        }
    }
}

#[test]
fn vp31_mutations_do_not_panic() {
    fuzz("vp3/vp31.avi", 0x5631_DEAD_BEEF_0031);
}

#[test]
fn vp40_mutations_do_not_panic() {
    fuzz("vp4/KTkvw8dg1J8.avi", 0x5640_DEAD_BEEF_0040);
}
