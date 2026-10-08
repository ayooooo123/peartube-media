//! Untrusted input: truncated, bit-flipped and byte-smashed TAK frames,
//! damaged stream info, and damaged whole files read to the end through the
//! demuxer's framing and seeked, must never panic. Deterministic: fixed
//! seeds, at least 2000 mutations per source.

use oxideav_core::{Decoder, Demuxer, Packet, RuntimeContext};
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

fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut d = data.to_vec();
    match rng.below(4) {
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
        _ => {
            // Damage in the frame header and the first channel's parameters.
            let n = d.len().min(48);
            for _ in 0..1 + rng.below(4) {
                if n > 0 {
                    let i = rng.below(n);
                    d[i] = rng.next() as u8;
                }
            }
        }
    }
    d
}

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    codec_tak::register(&mut ctx);
    ctx
}

fn open(ctx: &RuntimeContext, data: Vec<u8>) -> oxideav_core::Result<Box<dyn Demuxer>> {
    ctx.containers.open_demuxer("tak", Box::new(std::io::Cursor::new(data)), &ctx.codecs)
}

fn drain(decoder: &mut dyn Decoder) {
    while decoder.receive_frame().is_ok() {}
}

#[test]
fn damaged_frames_never_panic() {
    let ctx = context();
    let file = std::fs::read(fate("lossless-audio/luckynight-partial.tak")).expect("read");
    let mut demuxer = open(&ctx, file).expect("open");
    let params = demuxer.streams()[0].params.clone();
    let mut packets = Vec::new();
    while let Ok(p) = demuxer.next_packet() {
        packets.push(p);
    }
    let mut decoder = codec_tak::TakDecoder::new(&params).expect("decoder");
    let mut rng = Rng(0x7A4B);
    for _ in 0..3000 {
        let p = &packets[rng.below(packets.len())];
        let mut q: Packet = p.clone();
        q.data = mutate(&mut rng, &p.data);
        let _ = decoder.send_packet(&q);
        drain(&mut decoder);
    }
}

/// Damaged stream info: the decoder's setup, then frames.
#[test]
fn damaged_stream_info_never_panics() {
    let ctx = context();
    let file = std::fs::read(fate("lossless-audio/luckynight-partial.tak")).expect("read");
    let mut demuxer = open(&ctx, file).expect("open");
    let params = demuxer.streams()[0].params.clone();
    let packets: Vec<Packet> = std::iter::from_fn(|| demuxer.next_packet().ok()).take(4).collect();
    let mut rng = Rng(0x1F0);
    for _ in 0..2000 {
        let mut p = params.clone();
        p.extradata = mutate(&mut rng, &p.extradata);
        let Ok(mut decoder) = codec_tak::TakDecoder::new(&p) else { continue };
        for q in &packets {
            let _ = decoder.send_packet(q);
            drain(&mut decoder);
        }
    }
}

/// Damaged files: metadata blocks and data, read to the end (the framing
/// scans every byte), seeked, and a few frames decoded.
#[test]
fn damaged_files_never_panic() {
    let ctx = context();
    let file = std::fs::read(fate("lossless-audio/luckynight-partial.tak")).expect("read");
    // A shorter copy keeps each mutant quick: the header and the first frames.
    let file = &file[..file.len().min(96 * 1024)];
    let mut rng = Rng(0xF11E);
    for i in 0..2000 {
        let data = if i % 2 == 0 {
            let head = file.len().min(128);
            let mut front = mutate(&mut rng, &file[..head]);
            front.extend_from_slice(&file[head..]);
            front
        } else {
            mutate(&mut rng, file)
        };
        let Ok(mut demuxer) = open(&ctx, data) else { continue };
        let params = demuxer.streams()[0].params.clone();
        let mut decoder = codec_tak::TakDecoder::new(&params).ok();
        let mut count = 0;
        while let Ok(p) = demuxer.next_packet() {
            if let Some(d) = decoder.as_mut().filter(|_| count < 2) {
                let _ = d.send_packet(&p);
                drain(d);
            }
            count += 1;
        }
        let _ = demuxer.seek_to(0, rng.next() as i64 >> 40);
        if let (Ok(p), Some(d)) = (demuxer.next_packet(), decoder.as_mut()) {
            let _ = d.send_packet(&p);
            drain(d);
        }
    }
}
