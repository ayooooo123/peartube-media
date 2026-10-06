//! Untrusted-input mutation test: truncated and bit-flipped copies of the
//! reference corpus's packets must never panic the decoder (deterministic:
//! fixed-seed LCG, ≥ 2000 mutations per run).

use oxideav_core::RuntimeContext;

fn ctx() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_mpeg4video::__oxideav_entry(&mut ctx);
    oxideav_avi::__oxideav_entry(&mut ctx);
    oxideav_mkv::__oxideav_entry(&mut ctx);
    oxideav_mp4::__oxideav_entry(&mut ctx);
    oxideav_mpegts::__oxideav_entry(&mut ctx);
    ctx
}

/// Every corpus stream we mutate: the FATE samples (via `refcheck::fate`)
/// plus the generated fixtures.
fn corpus() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for name in [
        "demo.m4v",
        "xvid_vlc_trac7411.h263",
        "packed_bframes.avi",
        "resize_down-down.h263",
        "resize_up-up.h263",
    ] {
        out.push((format!("fate/{name}"), std::fs::read(refcheck::fate(&format!("mpeg4/{name}"))).expect("FATE sample")));
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for name in [
        "ipb_bf2_64x64.m4v",
        "qpel_64x64.m4v",
        "qp_rd_64x64.m4v",
        "ilaced_64x64.m4v",
        "ipb_aic_qpel_64x64.m4v",
    ] {
        out.push((
            format!("fixtures/{name}"),
            std::fs::read(dir.join(name)).expect("generated fixture"),
        ));
    }
    out
}

/// Decode `data` as every container input, swallowing per-decode errors —
/// the assertion is "no panic", which this function's return expresses.
fn try_decode(name: &str, data: &[u8]) -> Result<(), String> {
    let ctx = ctx();
    let mut head = vec![0u8; 256 * 1024];
    let n = head.len().min(data.len());
    head[..n].copy_from_slice(&data[..n]);
    let probe = oxideav_core::ProbeData {
        buf: &head[..n],
        ext: Some("m4v"),
    };
    let candidates = ctx.containers.probe_candidates(&probe);
    let format = match candidates.first() {
        Some(c) if c.score >= oxideav_core::PROBE_SCORE_EXTENSION => c.name.to_string(),
        _ => "mp4".to_string(),
    };
    let mut demuxer = ctx
        .containers
        .open_demuxer(&format, Box::new(std::io::Cursor::new(data.to_vec())), &ctx.codecs)
        .map_err(|e| format!("{name}: open {format}: {e}"))?;
    let streams: Vec<_> = demuxer
        .streams()
        .iter()
        .filter(|s| s.params.media_type == oxideav_core::MediaType::Video)
        .cloned()
        .collect();
    for stream in &streams {
        let mut decoder = ctx
            .codecs
            .first_decoder(&stream.params)
            .map_err(|e| format!("{name}: no decoder: {e}"))?;
            loop {
            match demuxer.next_packet() {
                Ok(packet) if packet.stream_index == stream.index => {
                    if let Err(e) = decoder.send_packet(&packet) {
                        return Err(format!("{name}: send: {e}"));
                    }
                    loop {
                        match decoder.receive_frame() {
                            Ok(_) => {}
                            Err(oxideav_core::Error::NeedMore) => break,
                            Err(oxideav_core::Error::Eof) => break,
                            Err(e) => return Err(format!("{name}: recv: {e}")),
                        }
                    }
                }
                Ok(_) => {}
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => return Err(format!("{name}: demux: {e}")),
            }
        }
        let _ = decoder.flush();
    }
    Ok(())
}

/// Truncation ladder: for each corpus stream, cut the file at 24 lengths
/// spread over the first ~16 KiB and decode each prefix.
#[test]
fn truncated_corpus_never_panics() {
    let corpus = corpus();
    let mut decoded_ok = 0usize;
    let mut errored = 0usize;
    for (name, data) in &corpus {
        for k in 0..24usize {
            let len = (data.len() * (k + 1)) / 25;
            match try_decode(name, &data[..len]) {
                Ok(()) => decoded_ok += 1,
                Err(_) => errored += 1,
            }
        }
    }
    println!("truncation: {decoded_ok} decoded, {errored} errored (no panics)");
}

/// Bit-flip hammer: ≥ 2000 deterministic single-byte XOR mutations across
/// the corpus (fixed-seed LCG picks stream, offset, and value).
#[test]
fn bitflip_corpus_never_panics() {
    let corpus = corpus();
    let mut state: u32 = 0x2545_F491; // fixed seed
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    let mut decoded_ok = 0usize;
    let mut errored = 0usize;
    for _ in 0..2000 {
        let (name, data) = {
            let i = (next() as usize) % corpus.len();
            (corpus[i].0.clone(), corpus[i].1.clone())
        };
        let mut mutated = data;
        let flips = 1 + (next() as usize) % 8;
        for _ in 0..flips {
            let off = (next() as usize) % mutated.len();
            mutated[off] ^= (next() & 0xFF) as u8;
        }
        match try_decode(&name, &mutated) {
            Ok(()) => decoded_ok += 1,
            Err(_) => errored += 1,
        }
    }
    println!("bitflip: {decoded_ok} decoded, {errored} errored (no panics)");
}
