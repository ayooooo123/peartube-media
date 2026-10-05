//! Untrusted-input robustness: the decoder and demuxers must never panic on
//! truncated or bit-flipped copies of the reference samples (every stream
//! byte comes from untrusted peers). Deterministic: fixed seed, 2000+
//! mutations per sample, no panic — errors are fine.

use refcheck::fate;
use oxideav_core::{Frame, ProbeData, RuntimeContext};
use std::fs::File;
use std::io::Read;

/// xorshift64* — deterministic, no external crates.
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

const MUTATIONS_PER_SAMPLE: usize = 2400;

/// Decode `data` with the raw demuxer + decoder, swallowing every error —
/// only a panic (or hang) fails the test.
fn feed_through(data: &[u8]) {
    let mut ctx = RuntimeContext::new();
    codec_mlp::register(&mut ctx);
    let probe = ProbeData {
        buf: &data[..data.len().min(256 * 1024)],
        ext: Some("thd"),
    };
    let candidates = ctx.containers.probe_candidates(&probe);
    let format = match candidates.first() {
        Some(c) if c.score >= oxideav_core::PROBE_SCORE_EXTENSION => c.name.to_string(),
        // Extension fallback mirrors the player's probe rule; for mlp data
        // use the mlp demuxer explicitly.
        _ => "truehd".to_string(),
    };
    let cursor = std::io::Cursor::new(data.to_vec());
    let Ok(mut demuxer) = ctx.containers.open_demuxer(&format, Box::new(cursor), &ctx.codecs)
    else {
        return;
    };
    let streams: Vec<_> = demuxer.streams().to_vec();
    let mut decoders: Vec<_> = streams
        .iter()
        .map(|s| ctx.codecs.first_decoder(&s.params))
        .collect();
    let mut budget = 4096usize;
    loop {
        budget -= 1;
        match demuxer.next_packet() {
            Ok(packet) => {
                if let Some(Ok(decoder)) = decoders.get_mut(packet.stream_index as usize) {
                    let _ = decoder.send_packet(&packet);
                    let mut guard = 0;
                    loop {
                        guard += 1;
                        match decoder.receive_frame() {
                            Ok(Frame::Audio(a)) => {
                                // Touch the data so a bad length panics here
                                // instead of downstream.
                                let _ = a.data.iter().map(|p| p.len()).sum::<usize>();
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                        if guard > 64 {
                            break;
                        }
                    }
                }
            }
            Err(_) => break,
        }
        if budget == 0 {
            break;
        }
    }
}

#[test]
fn no_panic_on_truncated_and_bit_flipped_truehd() {
    let path = fate("truehd/atmos.thd");
    let mut data = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut data).unwrap();

    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    for m in 0..MUTATIONS_PER_SAMPLE {
        let mut mutated = data.clone();
        match m % 3 {
            0 => {
                // Truncate at a pseudo-random length.
                let cut = 1 + (rng.next() as usize) % data.len();
                mutated.truncate(cut);
            }
            1 => {
                // Flip bits in a pseudo-random window.
                let flips = 1 + (rng.next() as usize) % 16;
                for _ in 0..flips {
                    let pos = (rng.next() as usize) % mutated.len();
                    let bit = 1u8 << ((rng.next() as u32) % 8);
                    mutated[pos] ^= bit;
                }
            }
            _ => {
                // Byte substitutions.
                let pos = (rng.next() as usize) % mutated.len();
                mutated[pos] = (rng.next() & 0xff) as u8;
            }
        }
        feed_through(&mutated);
    }
}

#[test]
fn no_panic_on_truncated_and_bit_flipped_mono() {
    let path = fate("truehd/ticket-1726-monocut.thd");
    let mut data = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut data).unwrap();

    let mut rng = Rng(0x5EED_5678_0000_0042);
    for m in 0..MUTATIONS_PER_SAMPLE {
        let mut mutated = data.clone();
        match m % 3 {
            0 => {
                let cut = 1 + (rng.next() as usize) % data.len();
                mutated.truncate(cut);
            }
            1 => {
                let flips = 1 + (rng.next() as usize) % 16;
                for _ in 0..flips {
                    let pos = (rng.next() as usize) % mutated.len();
                    let bit = 1u8 << ((rng.next() as u32) % 8);
                    mutated[pos] ^= bit;
                }
            }
            _ => {
                let pos = (rng.next() as usize) % mutated.len();
                mutated[pos] = (rng.next() & 0xff) as u8;
            }
        }
        feed_through(&mutated);
    }
}

#[test]
fn no_panic_on_truncated_and_bit_flipped_mlp() {
    let path = fate("lossless-audio/luckynight-partial.mlp");
    let mut data = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut data).unwrap();

    let mut rng = Rng(0x5EED_9ABC_DEF0_0003);
    for m in 0..MUTATIONS_PER_SAMPLE {
        let mut mutated = data.clone();
        match m % 3 {
            0 => {
                let cut = 1 + (rng.next() as usize) % data.len();
                mutated.truncate(cut);
            }
            1 => {
                let flips = 1 + (rng.next() as usize) % 16;
                for _ in 0..flips {
                    let pos = (rng.next() as usize) % mutated.len();
                    let bit = 1u8 << ((rng.next() as u32) % 8);
                    mutated[pos] ^= bit;
                }
            }
            _ => {
                let pos = (rng.next() as usize) % mutated.len();
                mutated[pos] = (rng.next() & 0xff) as u8;
            }
        }
        // The MLP file's probe scores 100 through the mlp demuxer; feed it
        // through the same pipeline (format resolved inside).
        feed_through_any(&mutated, "mlp");
    }
}

fn feed_through_any(data: &[u8], fallback_format: &str) {
    let mut ctx = RuntimeContext::new();
    codec_mlp::register(&mut ctx);
    let probe = ProbeData {
        buf: &data[..data.len().min(256 * 1024)],
        ext: Some("mlp"),
    };
    let candidates = ctx.containers.probe_candidates(&probe);
    let format = match candidates.first() {
        Some(c) if c.score >= oxideav_core::PROBE_SCORE_EXTENSION => c.name.to_string(),
        _ => fallback_format.to_string(),
    };
    let cursor = std::io::Cursor::new(data.to_vec());
    let Ok(mut demuxer) = ctx.containers.open_demuxer(&format, Box::new(cursor), &ctx.codecs)
    else {
        return;
    };
    let streams: Vec<_> = demuxer.streams().to_vec();
    let mut decoders: Vec<_> = streams
        .iter()
        .map(|s| ctx.codecs.first_decoder(&s.params))
        .collect();
    let mut budget = 4096usize;
    loop {
        budget -= 1;
        match demuxer.next_packet() {
            Ok(packet) => {
                if let Some(Ok(decoder)) = decoders.get_mut(packet.stream_index as usize) {
                    let _ = decoder.send_packet(&packet);
                    let mut guard = 0;
                    loop {
                        guard += 1;
                        match decoder.receive_frame() {
                            Ok(Frame::Audio(a)) => {
                                let _ = a.data.iter().map(|p| p.len()).sum::<usize>();
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                        if guard > 64 {
                            break;
                        }
                    }
                }
            }
            Err(_) => break,
        }
        if budget == 0 {
            break;
        }
    }
}
