//! Untrusted input: truncated and bit-flipped copies of FATE samples
//! (fixed seed, 2400 mutations and 256 truncations) open or fail, read
//! and seek without panicking, each within a time bound.

use std::io::Cursor;
use std::time::{Duration, Instant};

use oxideav_core::{Error, RuntimeContext};
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
}

/// Open `data`, read up to 4000 packets, seek three times reading a few
/// packets after each. Errors are fine; panics are not.
fn exercise(data: Vec<u8>) {
    let mut ctx = RuntimeContext::new();
    demux_mxf::register(&mut ctx);
    let Ok(mut d) = ctx.containers.open_demuxer("mxf", Box::new(Cursor::new(data)), &ctx.codecs) else { return };
    let nb = d.streams().len();
    for _ in 0..4000 {
        match d.next_packet() {
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(_) => {}
        }
    }
    for (i, target) in [0i64, 7, 1 << 40].into_iter().enumerate() {
        if nb == 0 {
            break;
        }
        let _ = d.seek_to((i % nb) as u32, target);
        for _ in 0..5 {
            let _ = d.next_packet();
        }
    }
}

fn bounded(what: String, data: Vec<u8>) {
    let start = Instant::now();
    let worker = std::thread::spawn(move || exercise(data));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(worker.join().is_ok());
    });
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(true) => {}
        Ok(false) => panic!("{what}: panicked"),
        Err(_) => panic!("{what}: did not end within 60 s ({:?})", start.elapsed()),
    }
}

const SAMPLES: [&str; 4] = [
    "mxf/opatom_missing_index.mxf",
    "mxf/track_02_a01.mxf",
    "imf/countdown/countdown-small.mxf",
    "mxf/C0023S01.mxf",
];

#[test]
fn truncated_copies_never_panic() {
    let mut rng = Rng(0x4D58_465F_5452_554E);
    for sample in SAMPLES {
        let data = std::fs::read(fate(sample)).unwrap();
        let data = &data[..data.len().min(384 * 1024)];
        for _ in 0..64 {
            let len = (rng.next() % data.len() as u64) as usize;
            bounded(format!("{sample} cut to {len}"), data[..len].to_vec());
        }
    }
}

#[test]
fn bit_flipped_copies_never_panic() {
    let mut rng = Rng(0x4D58_465F_464C_4950);
    for sample in SAMPLES {
        let data = std::fs::read(fate(sample)).unwrap();
        let data = &data[..data.len().min(384 * 1024)];
        for n in 0..600 {
            let mut copy = data.to_vec();
            // Most flips in the header metadata, where the structure is.
            let span = if n % 2 == 0 { copy.len().min(32 * 1024) } else { copy.len() };
            for _ in 0..1 + rng.next() % 16 {
                let at = (rng.next() % span as u64) as usize;
                copy[at] ^= 1 << (rng.next() % 8);
            }
            bounded(format!("{sample} mutation {n}"), copy);
        }
    }
}
