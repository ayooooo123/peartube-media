//! Untrusted input: truncated, bit-flipped and byte-smashed TTA frames,
//! damaged 22-byte stream headers (any format, channel count, bit depth,
//! rate and length; encrypted streams with any password), and damaged
//! whole files read to the end and seeked, must never panic.
//! Deterministic: fixed seeds, at least 2000 mutations per source.

use oxideav_core::{CodecId, CodecParameters, Decoder, Demuxer, Packet, RuntimeContext, TimeBase};
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
            // A run of ones: long unary codes and large Rice parameters.
            if !d.is_empty() {
                let at = rng.below(d.len());
                let end = (at + 1 + rng.below(64)).min(d.len());
                d[at..end].fill(0xFF);
            }
        }
    }
    d
}

fn open(ctx: &RuntimeContext, data: Vec<u8>) -> oxideav_core::Result<Box<dyn Demuxer>> {
    ctx.containers.open_demuxer("tta", Box::new(std::io::Cursor::new(data)), &ctx.codecs)
}

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    codec_tta::register(&mut ctx);
    ctx
}

/// The stream parameters and the first `n` frames of `sample`.
fn frames(sample: &str, n: usize) -> (CodecParameters, Vec<Packet>) {
    let ctx = context();
    let mut demuxer = open(&ctx, std::fs::read(fate(sample)).expect("read")).expect("open");
    let params = demuxer.streams()[0].params.clone();
    let mut out = Vec::new();
    while let Ok(p) = demuxer.next_packet() {
        out.push(p);
        if out.len() == n {
            break;
        }
    }
    (params, out)
}

fn drain(decoder: &mut dyn Decoder) {
    while decoder.receive_frame().is_ok() {}
}

#[test]
fn damaged_frames_never_panic() {
    let (params, packets) = frames("lossless-audio/inside.tta", 12);
    let mut decoder = codec_tta::TtaDecoder::new(&params).expect("decoder");
    let mut rng = Rng(0x7A7A);
    for _ in 0..2000 {
        let p = &packets[rng.below(packets.len())];
        let mut q = p.clone();
        q.data = mutate(&mut rng, &p.data);
        let _ = decoder.send_packet(&q);
        drain(&mut decoder);
    }
}

/// Headers with damaged fields, each decoder fed damaged frames.
#[test]
fn damaged_headers_never_panic() {
    let (params, packets) = frames("lossless-audio/inside.tta", 4);
    let mut rng = Rng(0x4EAD);
    for i in 0..3000 {
        let mut p = params.clone();
        let e = &mut p.extradata;
        match i % 4 {
            // Random values in the format, channel, depth, rate and length fields.
            0 => {
                let at = 4 + rng.below(14);
                e[at] = rng.next() as u8;
            }
            1 => {
                let at = 4 + 2 * rng.below(3);
                e[at..at + 2].copy_from_slice(&(rng.next() as u16).to_le_bytes());
            }
            2 => {
                let at = 10 + 4 * rng.below(2);
                e[at..at + 4].copy_from_slice(&(rng.next() as u32).to_le_bytes());
            }
            _ => {
                // Encrypted, with a random password.
                e[4] = 2;
                let pass: String = (0..rng.below(12)).map(|_| char::from(b'a' + rng.below(26) as u8)).collect();
                p.options.insert("password", pass);
            }
        }
        if rng.below(8) == 0 {
            p.extradata.truncate(rng.below(22));
        }
        let Ok(mut decoder) = codec_tta::TtaDecoder::new(&p) else { continue };
        for _ in 0..2 {
            let q = &packets[rng.below(packets.len())];
            let data = if rng.below(2) == 0 { q.data.clone() } else { mutate(&mut rng, &q.data) };
            let _ = decoder.send_packet(&Packet::new(0, TimeBase::new(1, 44100), data));
            drain(&mut decoder);
        }
    }
    // A header with no TTA1 magic is refused.
    let mut p = CodecParameters::audio(CodecId::new("tta"));
    p.extradata = vec![0; 22];
    assert!(codec_tta::TtaDecoder::new(&p).is_err());
}

/// Damaged files: open, read to the end, seek anywhere, read on.
#[test]
fn damaged_files_never_panic() {
    let file = std::fs::read(fate("lossless-audio/luckynight-partial.tta")).expect("read");
    let ctx = context();
    let mut rng = Rng(0xF11E);
    for i in 0..2000 {
        let mut data = file.clone();
        // Most damage lands in the header and seek table, the rest anywhere.
        if i % 2 == 0 {
            let head = data.len().min(22 + 4 * 12);
            let mut front = mutate(&mut rng, &data[..head]);
            front.extend_from_slice(&data[head..]);
            data = front;
        } else {
            data = mutate(&mut rng, &data);
        }
        let Ok(mut demuxer) = open(&ctx, data) else { continue };
        let params = demuxer.streams()[0].params.clone();
        let Ok(mut decoder) = codec_tta::TtaDecoder::new(&params) else { continue };
        // Every packet read; two decoded (the frames' damage has its own test).
        let mut count = 0;
        while let Ok(p) = demuxer.next_packet() {
            if count < 2 {
                let _ = decoder.send_packet(&p);
                drain(&mut decoder);
            }
            count += 1;
        }
        let _ = demuxer.seek_to(0, rng.next() as i64);
        if let Ok(p) = demuxer.next_packet() {
            let _ = decoder.send_packet(&p);
            drain(&mut decoder);
        }
    }
}
