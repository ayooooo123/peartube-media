//! Container mutations: 2000 deterministic mutations of a real file (the
//! corpus `h264_aac.mkv` — FFmpeg's muxing with a SeekHead, CRC-32 in every
//! Cluster and Cues) and 2000 of its Cues-less pipe remux — bit flips, byte
//! rewrites (VINT markers forge huge sizes), truncations and splices. Every
//! byte comes from untrusted peers: no mutation may panic the demuxer, make
//! it allocate beyond a bound, or keep it from ending. Each run opens the
//! file strictly (the player's path) or resiliently, demuxes it to its end,
//! seeks into it and demuxes again.
//!
//! The heap is measured by a counting global allocator; a panic reports the
//! run and seed so the input can be rebuilt.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use check_mkv::{Generated, corpus_dir, generated};
use oxideav_core::{Demuxer, Error, ReadSeek};

/// Counts live heap bytes and their peak.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let live = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: every call forwards to `System` with the caller's arguments; the
// counters only observe the sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size > layout.size() {
                grew(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static HEAP: Counting = Counting;

const RUNS: usize = 2000;
const SEED: u64 = 0x5EED_3A7B_10C0_0001;
/// No mutated file of this size holds anywhere near this many packets.
const MAX_CALLS: usize = 1_000_000;

/// xorshift64* — deterministic, tiny, no external crate.
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn mutate(src: &[u8], state: &mut u64) -> Vec<u8> {
    let mut data = src.to_vec();
    let at = |state: &mut u64, len: usize| (xorshift(state) as usize) % len.max(1);
    match xorshift(state) % 4 {
        0 => {
            for _ in 0..1 + xorshift(state) % 8 {
                let bit = at(state, data.len() * 8);
                data[bit / 8] ^= 1 << (bit % 8);
            }
        }
        1 => {
            // VINT length markers turn a size into a huge or unknown one.
            const VALUES: [u8; 6] = [0x00, 0x01, 0x7F, 0x80, 0xFF, 0x08];
            for _ in 0..1 + xorshift(state) % 4 {
                let i = at(state, data.len());
                let pick = xorshift(state) as usize;
                data[i] = if pick % 8 < 6 { VALUES[pick % 6] } else { (pick >> 8) as u8 };
            }
        }
        2 => {
            let len = at(state, data.len());
            data.truncate(len);
        }
        _ => {
            let n = 1 + at(state, 256);
            let from = at(state, data.len());
            let to = at(state, data.len());
            let chunk: Vec<u8> = src[from..(from + n).min(src.len())].to_vec();
            let end = (to + chunk.len()).min(data.len());
            data[to..end].copy_from_slice(&chunk[..end - to]);
        }
    }
    data
}

/// Calls `next_packet` until the end, tolerating a few consecutive errors
/// (a caller may retry); `Err` when the demuxer never ends.
fn drain(dmx: &mut dyn Demuxer) -> Result<(), String> {
    let mut errors = 0;
    for _ in 0..MAX_CALLS {
        match dmx.next_packet() {
            Ok(_) => errors = 0,
            Err(Error::Eof) => return Ok(()),
            Err(_) => {
                errors += 1;
                if errors == 16 {
                    return Ok(());
                }
            }
        }
    }
    Err(format!("no end after {MAX_CALLS} next_packet calls"))
}

fn exercise(data: Vec<u8>, resilient: bool, target: i64) -> Result<(), String> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(data));
    let opened = if resilient {
        oxideav_mkv::demux::open_resilient(rs, &oxideav_core::NullCodecResolver)
    } else {
        check_mkv::open(rs)
    };
    let Ok(mut dmx) = opened else {
        return Ok(());
    };
    drain(&mut *dmx)?;
    if !dmx.streams().is_empty() {
        let _ = dmx.seek_to(0, target);
        drain(&mut *dmx)?;
    }
    Ok(())
}

fn fuzz(name: &str, path: &Path, seed: u64) {
    let src = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    // A demux holds a Block and its de-laced frames at a time, each at most
    // the input's size; a size field can't make it hold more than the input.
    let bound = 8 * src.len() + (16 << 20);
    let mut state = seed;
    let mut failures = Vec::new();
    let mut worst = 0;
    for run in 0..RUNS {
        let run_seed = state;
        let data = mutate(&src, &mut state);
        let resilient = run % 2 == 1;
        let target = (xorshift(&mut state) % 20_000) as i64;
        let base = LIVE.load(Ordering::Relaxed);
        PEAK.store(base, Ordering::Relaxed);
        let outcome = catch_unwind(AssertUnwindSafe(|| exercise(data, resilient, target)));
        let extra = PEAK.load(Ordering::Relaxed).saturating_sub(base);
        worst = worst.max(extra);
        let mode = if resilient { "resilient" } else { "strict" };
        match outcome {
            Err(_) => failures.push(format!("{name} run {run} ({mode}, seed {run_seed:#x}): panic")),
            Ok(Err(e)) => failures.push(format!("{name} run {run} ({mode}, seed {run_seed:#x}): {e}")),
            Ok(Ok(())) => {}
        }
        if extra > bound {
            failures.push(format!(
                "{name} run {run} ({mode}, seed {run_seed:#x}): {extra} heap bytes for a {}-byte file",
                src.len()
            ));
        }
    }
    println!(
        "{name}: {RUNS} mutations of {} bytes, peak extra heap {worst} bytes (bound {bound})",
        src.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn mutations_neither_panic_nor_grow_the_heap() {
    fuzz("h264_aac.mkv", &corpus_dir().join("h264_aac.mkv"), SEED);
    let cues_less = generated(Path::new(env!("CARGO_TARGET_TMPDIR")), Generated::SmallNoCues);
    fuzz("h264_aac.mkv piped (no Cues)", &cues_less, SEED ^ 0xA5A5);
}
