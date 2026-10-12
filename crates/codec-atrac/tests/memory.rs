//! A packet costs at most one decoded frame of memory, whatever its size,
//! `block_align` or channel count: decoding happens on demand, one frame per
//! `receive_frame`, as FFmpeg's decode loop does.
//!
//! The probes, frames of a few bytes each: an ATRAC3+ byte 0x60 is a start
//! bit then the terminator unit, a whole frame of 2,048 samples per channel
//! (`block_align` 1); RealMedia ATRAC3 descrambles `F3 7F 61 03` to
//! `A0 00 00 00`, an empty sound unit of 1,024 samples per channel; an
//! all-zero ATRAC1 block decodes to 512, moving `block_align` bytes on.
//! Decoding such a packet in full up front takes 64 KiB of PCM per input
//! byte at 8 channels.
//!
//! This file holds one test, so no other test allocates while it measures.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use oxideav_core::{CodecId, CodecParameters, Frame, Packet, TimeBase};

/// Counts live heap bytes and their peak.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Bytes the decoder may hold beyond the input packet: a few frames.
const BUDGET: usize = 2 << 20;

fn params(codec: &str, channels: u16, block_align: usize) -> CodecParameters {
    let mut p = CodecParameters::audio(CodecId::new(codec));
    p.sample_rate = Some(44_100);
    p.channels = Some(channels);
    p.options.insert("block_align", block_align.to_string());
    if codec == "atrac3" {
        // RealMedia: version 4, 1,024 samples per channel, delay 0x88E,
        // single channels
        let samples = 1024 * channels;
        p.extradata = [
            &[0, 0, 0, 4][..],
            &samples.to_be_bytes(),
            &[0x08, 0x8E, 0, 2],
        ]
        .concat();
    }
    p
}

#[test]
fn a_packet_of_tiny_frames_costs_one_frame_of_memory() {
    let mut ctx = oxideav_core::RuntimeContext::new();
    codec_atrac::register(&mut ctx);
    let mut failures = Vec::new();
    const AT3: &[u8] = &[0xF3, 0x7F, 0x61, 0x03];
    for (codec, channels, block_align, pattern, samples) in [
        ("atrac3plus", 1u16, 1usize, &[0x60u8][..], 2048u32),
        ("atrac3plus", 2, 1, &[0x60], 2048),
        ("atrac3plus", 8, 1, &[0x60], 2048),
        ("atrac3plus", 8, 3, &[0x60], 2048),
        ("atrac1", 2, 1, &[0x00], 512),
        ("atrac1", 8, 5, &[0x00], 512),
        ("atrac3", 1, 4, AT3, 1024),
        ("atrac3", 2, 8, AT3, 1024),
    ] {
        let params = params(codec, channels, block_align);
        let mut decoder = ctx
            .codecs
            .first_decoder(&params)
            .unwrap_or_else(|e| panic!("{codec} {channels} ch: {e}"));
        let data: Vec<u8> = pattern.iter().copied().cycle().take(4096).collect();
        let packet = Packet::new(0, TimeBase::new(1, 44_100), data);
        // the decoder and its tables exist before the measurement starts
        let base = LIVE.load(Ordering::Relaxed);
        PEAK.store(base, Ordering::Relaxed);

        decoder.send_packet(&packet).unwrap();
        for _ in 0..3 {
            let Frame::Audio(a) = decoder.receive_frame().unwrap() else {
                panic!("not audio")
            };
            assert_eq!(
                (a.samples, a.data.len()),
                (samples, usize::from(channels)),
                "{codec} {channels} ch"
            );
        }
        let used = PEAK.load(Ordering::Relaxed) - base;
        if used > BUDGET {
            failures.push(format!("{codec}, {channels} ch, block_align {block_align}: {used} bytes for 3 frames of a 4 KiB packet"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
