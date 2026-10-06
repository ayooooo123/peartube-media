//! Untrusted-input robustness: demuxers and decoders must never panic on
//! mutated or truncated input.

use refcheck::fate;
use oxideav_core::RuntimeContext;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

const MUTATIONS_PER_SAMPLE: usize = 2000;

fn feed_through(data: &[u8], _ext: &str, format: &str) {
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);

    let cursor = std::io::Cursor::new(data.to_vec());
    let Ok(mut demuxer) = ctx.containers.open_demuxer(format, Box::new(cursor), &ctx.codecs) else {
        return;
    };

    let streams = demuxer.streams().to_vec();
    let mut decoders: Vec<_> = streams
        .iter()
        .map(|s| ctx.codecs.first_decoder(&s.params))
        .collect();

    let mut budget = 256;
    while budget > 0 {
        budget -= 1;
        match demuxer.next_packet() {
            Ok(packet) => {
                if let Some(Ok(decoder)) = decoders.get_mut(packet.stream_index as usize) {
                    let _ = decoder.send_packet(&packet);
                    let mut guard = 0;
                    while guard < 8 {
                        guard += 1;
                        if decoder.receive_frame().is_err() {
                            break;
                        }
                    }
                }
            }
            _ => break,
        }
    }
}

fn mutate(rng: &mut Rng, original: &[u8]) -> Vec<u8> {
    let mut data = original.to_vec();
    let mode = (rng.next() % 3) as u8;
    match mode {
        0 => {
            // Truncate
            let new_len = (rng.next() as usize) % (data.len().max(1));
            data.truncate(new_len);
        }
        1 => {
            // Bit flip
            let flips = 1 + (rng.next() % 8) as usize;
            for _ in 0..flips {
                if !data.is_empty() {
                    let idx = (rng.next() as usize) % data.len();
                    let bit = 1 << ((rng.next() % 8) as u8);
                    data[idx] ^= bit;
                }
            }
        }
        _ => {
            // Byte overwrite
            let count = 1 + (rng.next() % 16) as usize;
            for _ in 0..count {
                if !data.is_empty() {
                    let idx = (rng.next() as usize) % data.len();
                    data[idx] = (rng.next() % 256) as u8;
                }
            }
        }
    }
    data
}

#[test]
fn test_robustness_smm0005_rcv() {
    let path = fate("vc1/SMM0005.rcv");
    let original = std::fs::read(&path).expect("read SMM0005.rcv");
    let mut rng = Rng(0x1234_5678_9ABC_DEF0);

    for _ in 0..MUTATIONS_PER_SAMPLE {
        let mutated = mutate(&mut rng, &original);
        feed_through(&mutated, "rcv", "vc1test");
    }
}

#[test]
fn test_robustness_sa00040_vc1() {
    let path = fate("vc1/SA00040.vc1");
    let original = std::fs::read(&path).expect("read SA00040.vc1");
    let mut rng = Rng(0xCAFE_BABE_DEAD_BEEF);

    for _ in 0..MUTATIONS_PER_SAMPLE {
        let mutated = mutate(&mut rng, &original);
        feed_through(&mutated, "vc1", "vc1");
    }
}
