//! Untrusted-input robustness: the decoder and demuxers must never panic
//! on truncated or bit-flipped copies of the reference samples (every
//! stream byte comes from untrusted peers). Deterministic: fixed seed,
//! 2000+ mutations per sample — errors are fine, panics are not.

use oxideav_core::{Frame, ProbeData, RuntimeContext};
use refcheck::fate;
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

const MUTATIONS_PER_SAMPLE: usize = 2000;

/// Decode `data` with the `dtshd`/`dts` demuxer + decoder, swallowing every
/// error — only a panic (or hang) fails the test.
fn feed_through(data: &[u8], ext: &str, fallback_format: &str) {
    let mut ctx = RuntimeContext::new();
    codec_dca::register(&mut ctx);
    let probe = ProbeData {
        buf: &data[..data.len().min(256 * 1024)],
        ext: Some(ext),
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
    let Ok(mut decoder) = ctx.codecs.first_decoder(&streams[0].params) else {
        return;
    };
    let mut budget = 4096usize;
    loop {
        budget -= 1;
        match demuxer.next_packet() {
            Ok(packet) => {
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
            Err(_) => break,
        }
        if budget == 0 {
            break;
        }
    }
}

fn mutate(data: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut mutated = data.to_vec();
    match rng.next() % 3 {
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
    mutated
}

#[test]
fn no_panic_on_truncated_and_bit_flipped_dtshd() {
    let path = fate("dts/dcadec-suite/xll_51_24_48_768.dtshd");
    let mut data = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut data).unwrap();

    let mut rng = Rng(0x5EED_DCA1_0000_0001);
    for _ in 0..MUTATIONS_PER_SAMPLE {
        let mutated = mutate(&data, &mut rng);
        feed_through(&mutated, "dtshd", "dtshd");
    }
}

#[test]
fn no_panic_on_truncated_and_bit_flipped_core() {
    let path = fate("dts/dcadec-suite/core_51_24_48_768_0.dtshd");
    let mut data = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut data).unwrap();

    let mut rng = Rng(0x5EED_DCA2_0000_0002);
    for _ in 0..MUTATIONS_PER_SAMPLE {
        let mutated = mutate(&data, &mut rng);
        feed_through(&mutated, "dtshd", "dtshd");
    }
}

#[test]
fn no_panic_on_truncated_and_bit_flipped_raw_dts() {
    let path = fate("dts/master_audio_7.1_24bit.dts");
    let mut data = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut data).unwrap();
    // The raw demuxer rescans from the head on every sync loss; keep the
    // mutation corpus bounded so the 2000 runs stay fast.
    data.truncate(256 * 1024);

    let mut rng = Rng(0x5EED_DCA3_0000_0003);
    for _ in 0..MUTATIONS_PER_SAMPLE {
        let mutated = mutate(&data, &mut rng);
        feed_through(&mutated, "dts", "dts");
    }
}
