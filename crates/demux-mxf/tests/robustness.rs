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

/// The value bytes of every local tag `tag` (8 bytes long) in `data`.
fn local_tag_values(data: &[u8], tag: u16) -> Vec<usize> {
    let pattern = [(tag >> 8) as u8, tag as u8, 0, 8];
    data.windows(4).enumerate().filter(|(_, w)| *w == pattern).map(|(i, _)| i + 4).collect()
}

/// The value offsets of the partition packs' 8-byte fields: this, previous
/// and footer partition, header and index byte counts, body offset.
fn partition_fields(data: &[u8]) -> Vec<Vec<usize>> {
    const KEY: [u8; 13] = [0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01];
    let mut fields = vec![Vec::new(); 6];
    for (i, _) in data.windows(13).enumerate().filter(|(_, w)| *w == KEY) {
        let Some(&first) = data.get(i + 16) else { continue };
        let (len_bytes, value) = if first & 0x80 != 0 { (usize::from(first & 0x7f), i + 17 + usize::from(first & 0x7f)) } else { (0, i + 17) };
        if len_bytes > 8 || value + 60 > data.len() {
            continue;
        }
        for (k, at) in [8, 16, 24, 32, 40, 52].into_iter().enumerate() {
            fields[k].push(value + at);
        }
    }
    fields
}

/// Index segments and partition packs claiming extreme 64-bit counts and
/// positions (all ones, the sign bit alone, values whose double overflows)
/// open or fail, read and seek without panicking.
#[test]
fn extreme_64_bit_index_and_partition_fields_never_panic() {
    let mut panicked = Vec::new();
    for sample in ["mxf/omneon_8.3.0.0_xdcam_startc_footer.mxf", "mxf/Avid-00005.mxf", "mxf/track_02_a01.mxf"] {
        let data = std::fs::read(fate(sample)).unwrap();
        let mut groups = vec![
            ("IndexStartPosition", local_tag_values(&data, 0x3F0C)),
            ("IndexDuration", local_tag_values(&data, 0x3F0D)),
        ];
        let names = ["ThisPartition", "PreviousPartition", "FooterPartition", "HeaderByteCount", "IndexByteCount", "BodyOffset"];
        groups.extend(names.into_iter().zip(partition_fields(&data)));
        assert!(!groups[1].1.is_empty(), "{sample}: an IndexDuration to patch");
        for (field, offsets) in &groups {
            for value in [u64::MAX, 1 << 63, (1 << 62) + 1, i64::MAX as u64] {
                let mut patched = data.clone();
                for &at in offsets {
                    patched[at..at + 8].copy_from_slice(&value.to_be_bytes());
                }
                if std::panic::catch_unwind(|| exercise(patched)).is_err() {
                    panicked.push(format!("{sample}: {field} = {value:#x}"));
                }
            }
        }
    }
    assert!(panicked.is_empty(), "panicked: {panicked:#?}");
}
