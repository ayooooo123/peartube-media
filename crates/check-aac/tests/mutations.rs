//! Fuzz the fork with mutated real FATE packets: every byte comes from
//! untrusted peers, so no mutation may panic the decoder — a malformed
//! input surfaces an error (or silence), never an abort.
//!
//! Mutations are deterministic: a fixed xorshift64* seed per mutation
//! index picks one packet of one reference sample, flips a bit range,
//! and the whole corpus decode runs under `catch_unwind`. A panic is a
//! test failure carrying the seed and the sample, so the input can be
//! reproduced with `MUTATION_SEED`.

use check_aac::{decoded_f32, MUTATION_SAMPLES};
use oxideav_core::{Frame, RuntimeContext};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// The container name a refcheck-style probe picks for this sample.
fn container_name(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "ts" | "mpg" | "mpeg" => "mpegts",
        "aac" | "adts" => "adts",
        _ => "mov",
    }
}

/// xorshift64* — deterministic, tiny, no external crate.
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Decode a raw packet byte buffer the way the transport layer would:
/// raw AAC access units through the fork's decoder factory.
fn decode_buffer(ctx: &RuntimeContext, data: &[u8], asc_hint: Option<&[u8]>) {
    let mut params = oxideav_core::CodecParameters::audio(oxideav_core::CodecId::new("aac"));
    params.sample_rate = Some(44_100);
    params.channels = Some(2);
    if let Some(asc) = asc_hint {
        params.extradata = asc.to_vec();
    }
    let Ok(mut dec) = ctx.codecs.first_decoder(&params) else {
        return;
    };
    let mut pkt = oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, 44_100), data.to_vec());
    pkt.pts = Some(0);
    // Any error is fine (invalid data is expected); a panic is not.
    let _ = dec.send_packet(&pkt);
    while let Ok(frame) = dec.receive_frame() {
        if let Frame::Audio(a) = frame {
            // Touch the PCM so UB (if any) surfaces inside the test.
            let _ = a.data[0].len();
        }
    }
    let _ = dec.flush();
}

#[test]
fn mutations_do_not_panic() {
    const SEED: u64 = 0x5EED_AAC0_F00D_0001;
    const RUNS: usize = 2000;

    let mut ctx = RuntimeContext::new();
    oxideav_aac::__oxideav_entry(&mut ctx);
    oxideav_mov::registry::register(&mut ctx);
    oxideav_mp4::__oxideav_entry(&mut ctx);
    oxideav_mpegts::__oxideav_entry(&mut ctx);

    // The packet bytes and the (optional) ASC of each sample, decoded
    // once unmutated so the mutations target real stream structure.
    let corpora: Vec<(String, Vec<Vec<u8>>, Option<Vec<u8>>)> = MUTATION_SAMPLES
        .iter()
        .map(|rel| {
            let (_pcm, path, _ch) = decoded_f32(rel);
            let mut ctx = RuntimeContext::new();
            oxideav_aac::__oxideav_entry(&mut ctx);
            oxideav_mov::registry::register(&mut ctx);
            oxideav_mp4::__oxideav_entry(&mut ctx);
            oxideav_mpegts::__oxideav_entry(&mut ctx);
            let file = std::fs::File::open(&path).unwrap();
            let mut demuxer = ctx
                .containers
                .open_demuxer(container_name(&path), Box::new(file), &ctx.codecs)
                .unwrap_or_else(|e| panic!("{rel}: open demuxer: {e}"));
            let asc = demuxer
                .streams()
                .first()
                .filter(|s| !s.params.extradata.is_empty())
                .map(|s| s.params.extradata.clone());
            let mut packets = Vec::new();
            while let Ok(p) = demuxer.next_packet() {
                packets.push(p.data);
            }
            assert!(!packets.is_empty(), "{rel}: no packets");
            (rel.to_string(), packets, asc)
        })
        .collect();

    let mut state = SEED;
    for run in 0..RUNS {
        let (rel, packets, asc) = &corpora[(xorshift(&mut state) as usize) % corpora.len()];
        let packet_idx = (xorshift(&mut state) as usize) % packets.len();
        let packet = &packets[packet_idx];
        if packet.is_empty() {
            continue;
        }
        let mut data = packet.clone();
        // Flip 1-8 bit positions picked from the stream state.
        let flips = 1 + (xorshift(&mut state) as usize) % 8;
        for _ in 0..flips {
            let bit = (xorshift(&mut state) as usize) % (data.len() * 8);
            data[bit / 8] ^= 1 << (bit % 8);
        }

        let seed_for_report = state;
        let result = catch_unwind(AssertUnwindSafe(|| decode_buffer(&ctx, &data, asc.as_deref())));
        if result.is_err() {
            panic!(
                "mutation panic: run {run} sample {rel} packet {packet_idx} seed {seed_for_report:#x}"
            );
        }
    }
}
