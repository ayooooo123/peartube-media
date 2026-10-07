//! Fuzz the fork with mutated real FATE packets: every byte comes from
//! untrusted peers, so no mutation may panic the decoder — a malformed
//! input surfaces an error (or silence), never an abort.
//!
//! Mutations are deterministic: a fixed xorshift64* seed per mutation
//! index picks one packet of one reference sample, flips a bit range,
//! and the whole corpus decode runs under `catch_unwind`. A panic is a
//! test failure carrying the seed and the sample, so the input can be
//! reproduced with `MUTATION_SEED`.

use check_aac::{aac_decoder, decoded_f32, usac_packets, MUTATION_SAMPLES};
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
    exercise_mutations(MUTATION_SAMPLES, 0x5EED_AAC0_F00D_0001);
}

#[test]
fn usac_mutations_do_not_panic() {
    let samples: Vec<_> = check_aac::USAC_SAMPLES.iter().map(|s| s.0).collect();
    exercise_mutations(&samples, 0x5EED_AAC4_2000_0001);
}

fn exercise_mutations(samples: &[&str], seed: u64) {
    const RUNS: usize = 2000;

    let mut ctx = RuntimeContext::new();
    oxideav_aac::__oxideav_entry(&mut ctx);
    oxideav_mov::registry::register(&mut ctx);
    oxideav_mp4::__oxideav_entry(&mut ctx);
    oxideav_mpegts::__oxideav_entry(&mut ctx);

    // The packet bytes and the (optional) ASC of each sample, decoded
    // once unmutated so the mutations target real stream structure.
    let corpora: Vec<(String, Vec<Vec<u8>>, Option<Vec<u8>>)> = samples
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

    let mut state = seed;
    for run in 0..RUNS {
        let (rel, packets, asc) = &corpora[(xorshift(&mut state) as usize) % corpora.len()];
        let packet_idx = (xorshift(&mut state) as usize) % packets.len();
        let packet = &packets[packet_idx];
        if packet.is_empty() {
            continue;
        }
        let mut data = packet.clone();
        // Every odd run truncates: cut 1-64 bytes off the tail (packet
        // and header boundaries land inside the cut at random). Every
        // even run flips 1-8 bit positions.
        if run % 2 == 0 {
            let cut = 1 + (xorshift(&mut state) as usize) % 64;
            let cut = cut.min(data.len());
            data.truncate(data.len() - cut);
        } else {
            let flips = 1 + (xorshift(&mut state) as usize) % 8;
            for _ in 0..flips {
                let bit = (xorshift(&mut state) as usize) % (data.len() * 8);
                data[bit / 8] ^= 1 << (bit % 8);
            }
        }
        if data.is_empty() {
            continue;
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

/// One persistent decoder per run over a window of real USAC AUs starting at
/// an independent AU, with one to three AUs corrupted (bit flips, truncation,
/// emptied, dropped or duplicated). No panic; every emitted frame is 1024
/// finite samples per channel; a failed packet emits nothing. The noise-free,
/// all-independent stream resynchronizes bit-exactly two AUs after the last
/// corruption, and `reset()` restores fresh-decoder output exactly.
#[test]
fn usac_persistent_sequence_mutations() {
    use oxideav_core::{Error, Packet};
    const WINDOW: usize = 10;
    let mut samples = vec![
        "aac/Fd_2_c1_Ms_0x04.mp4",
        "aac/usac/Fd_1_c1_0x03.mp4",
        "aac/usac/Fd_2_c1_Tns_0x04.mp4",
        "aac/usac/Ext_2_c1_Ln_0x03.mp4",
        "aac/usac/xhe_target_level.m4a",
    ].into_iter().map(str::to_owned).collect::<Vec<_>>();
    let iso = std::env::var_os("ISO_USAC").map(std::path::PathBuf::from).unwrap_or_else(||
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join("projects/oracles/iso-usac"));
    for name in ["Fd_2_c1_WinCp_0x0c", "Fd_2_c1_WinTns_0x0c", "Fd_2_c1_Nf_0x0c"] {
        samples.push(iso.join(format!("members/compressedMp4/{name}.mp4")).to_str().unwrap().to_owned());
    }
    let corpora: Vec<_> = samples.iter().map(|rel| {
        let (params, packets) = usac_packets(rel);
        let starts: Vec<usize> = (0..packets.len().saturating_sub(WINDOW))
            .filter(|&i| packets[i].data[0] & 0x80 != 0)
            .collect();
        let channels = aac_decoder(&params).output_audio_format().unwrap().channels as usize;
        (rel.as_str(), params, packets, starts, channels)
    })
    .collect();
    let mut clean_cache = std::collections::HashMap::new();
    let mut state = 0x5EED_AAC4_2000_0002u64;
    for run in 0..2000 {
        let corpus = (xorshift(&mut state) as usize) % corpora.len();
        let (rel, params, packets, starts, channels) = &corpora[corpus];
        let start = starts[(xorshift(&mut state) as usize) % starts.len()];
        let window = &packets[start..start + WINDOW];
        let clean = clean_cache.entry((corpus, start)).or_insert_with(|| {
            let mut decoder = aac_decoder(params);
            window.iter().map(|p| check_aac::decode_one(&mut decoder, p).unwrap()).collect::<Vec<_>>()
        });
        // (original index, bytes) in sending order.
        let mut sequence: Vec<(usize, Vec<u8>, bool)> =
            window.iter().enumerate().map(|(i, p)| (i, p.data.clone(), false)).collect();
        let mut last = 0;
        for _ in 0..1 + (xorshift(&mut state) as usize) % 3 {
            let at = (xorshift(&mut state) as usize) % (WINDOW - 3);
            let position = sequence.iter().position(|s| s.0 == at).unwrap_or(0);
            last = last.max(at);
            let data = &mut sequence[position].1;
            match xorshift(&mut state) % 5 {
                0 => {
                    for _ in 0..1 + xorshift(&mut state) % 8 {
                        let bit = (xorshift(&mut state) as usize) % (data.len() * 8).max(1);
                        if !data.is_empty() {
                            data[bit / 8] ^= 1 << (bit % 8);
                        }
                    }
                }
                1 => {
                    let keep = (xorshift(&mut state) as usize) % data.len().max(1);
                    data.truncate(keep);
                }
                2 => data.clear(),
                3 => {
                    sequence.remove(position);
                }
                _ => {
                    let copy = sequence[position].clone();
                    sequence.insert(position, copy);
                }
            }
            let marked = position.min(sequence.len() - 1);
            sequence[marked].2 = true;
        }
        let seed = state;
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let mut decoder = aac_decoder(params);
            let mut frames: Vec<(usize, bool, Vec<f32>)> = Vec::new();
            for (index, data, mutated) in &sequence {
                let sent = decoder.send_packet(&Packet::new(0, oxideav_core::TimeBase::new(1, 48000), data.clone()));
                match decoder.receive_frame() {
                    Ok(Frame::Audio(audio)) => {
                        assert!(sent.is_ok() && !data.is_empty(), "frame without a decoded packet");
                        assert_eq!(audio.samples, 1024);
                        assert_eq!(audio.data[0].len(), 1024 * channels * 4);
                        let pcm: Vec<f32> =
                            audio.data[0].chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
                        assert!(pcm.iter().all(|v| v.is_finite()));
                        frames.push((*index, *mutated, pcm));
                        assert!(matches!(decoder.receive_frame(), Err(Error::NeedMore)));
                    }
                    Ok(_) => panic!("non-audio frame"),
                    Err(Error::NeedMore) => assert!(sent.is_err() || data.is_empty(), "decoded packet without a frame"),
                    Err(error) => panic!("receive_frame: {error}"),
                }
            }
            if rel.contains("Ext_") {
                for (index, mutated, pcm) in &frames {
                    if !mutated && *index >= last + 2 {
                        assert_eq!(pcm, &clean[*index], "AU {index} after resynchronization");
                    }
                }
            }
            decoder.reset().unwrap();
            for (i, packet) in window.iter().enumerate() {
                assert_eq!(check_aac::decode_one(&mut decoder, packet).unwrap(), clean[i], "AU {i} after reset");
            }
        }));
        if let Err(panic) = outcome {
            let message = panic.downcast_ref::<String>().cloned().unwrap_or_default();
            panic!("USAC sequence mutation run {run}, {rel} from AU {start}, seed {seed:#x}: {message}");
        }
    }
}
