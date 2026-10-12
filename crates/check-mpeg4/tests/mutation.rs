//! Untrusted-input mutation tests: truncated and bit-flipped copies of the
//! reference corpus go through the player's decode path (the product
//! registry: FFmpeg's m4v demuxer port for the raw `.m4v` and `.h263`
//! streams, AVI for the packed B-frames, the forked MPEG-4 Part 2 decoder)
//! and must never panic or hang. Deterministic: a fixed-seed xorshift picks
//! every mutation.
//!
//! Only an input whose demuxer opened and handed the MPEG-4 decoder at
//! least one packet counts as a decoder trial: every unmodified seed must
//! be one, every truncated raw stream that still holds a VOP must be one,
//! and the bit-flip hammer runs until 2000 mutations have reached the
//! decoder. Inputs no demuxer takes are counted apart.

use oxideav_core::{Error, MediaType, ProbeData, RuntimeContext, PROBE_SCORE_EXTENSION};

/// One corpus stream: its name, file extension and bytes.
struct Seed {
    name: String,
    ext: &'static str,
    data: Vec<u8>,
}

fn corpus() -> Vec<Seed> {
    let mut out = Vec::new();
    for (name, ext) in [
        ("demo.m4v", "m4v"),
        ("xvid_vlc_trac7411.h263", "h263"),
        ("packed_bframes.avi", "avi"),
        ("resize_down-down.h263", "h263"),
        ("resize_up-up.h263", "h263"),
    ] {
        let data = std::fs::read(refcheck::fate(&format!("mpeg4/{name}"))).expect("FATE sample");
        out.push(Seed { name: format!("fate/{name}"), ext, data });
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for name in [
        "ipb_bf2_64x64.m4v",
        "qpel_64x64.m4v",
        "qp_rd_64x64.m4v",
        "ilaced_64x64.m4v",
        "ipb_aic_qpel_64x64.m4v",
        "qpel_mv4_bf2_84x76.m4v",
    ] {
        let data = std::fs::read(dir.join(name)).expect("generated fixture");
        out.push(Seed { name: format!("fixtures/{name}"), ext: "m4v", data });
    }
    out
}

/// What one input did on the decode path.
#[derive(Debug, Default)]
struct Trial {
    /// A demuxer took the input.
    opened: bool,
    /// Packets the MPEG-4 decoder was handed.
    packets: usize,
    frames: usize,
    /// Errors the demuxer or decoder returned.
    errors: usize,
}

impl Trial {
    fn reached_decoder(&self) -> bool {
        self.packets > 0
    }
}

/// The player's probe rule (engine-api.md): the best content probe over
/// the first 256 KiB when it scores at least an extension match, else the
/// container registered for the extension.
fn container(ctx: &RuntimeContext, ext: &str, data: &[u8]) -> Option<String> {
    let probe = ProbeData { buf: &data[..data.len().min(256 * 1024)], ext: Some(ext) };
    match ctx.containers.probe_candidates(&probe).first() {
        Some(c) if c.score >= PROBE_SCORE_EXTENSION => Some(c.name.to_string()),
        _ => ctx.containers.container_for_extension(ext).map(str::to_string),
    }
}

/// Demux `data` and decode its first video stream when that is MPEG-4
/// Part 2, draining every frame; errors are counted, not raised: the
/// assertion is that nothing panics or hangs.
fn try_decode(ctx: &RuntimeContext, ext: &str, data: &[u8]) -> Trial {
    let mut trial = Trial::default();
    let Some(format) = container(ctx, ext, data) else { return trial };
    let Ok(mut demuxer) = ctx.containers.open_demuxer(&format, Box::new(std::io::Cursor::new(data.to_vec())), &ctx.codecs) else {
        return trial;
    };
    trial.opened = true;
    let Some(stream) = demuxer.streams().iter().find(|s| s.params.media_type == MediaType::Video).cloned() else {
        return trial;
    };
    if !matches!(stream.params.codec_id.as_str(), "mpeg4" | "mpeg4video") {
        return trial;
    }
    let Ok(mut decoder) = ctx.codecs.first_decoder(&stream.params) else { return trial };
    let drain = |decoder: &mut Box<dyn oxideav_core::Decoder>, trial: &mut Trial| loop {
        match decoder.receive_frame() {
            Ok(_) => trial.frames += 1,
            Err(Error::NeedMore | Error::Eof) => break,
            Err(_) => {
                trial.errors += 1;
                break;
            }
        }
    };
    loop {
        match demuxer.next_packet() {
            Ok(packet) if packet.stream_index == stream.index => {
                trial.packets += 1;
                if decoder.send_packet(&packet).is_err() {
                    trial.errors += 1;
                }
                drain(&mut decoder, &mut trial);
            }
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(_) => {
                trial.errors += 1;
                break;
            }
        }
    }
    if decoder.flush().is_err() {
        trial.errors += 1;
    }
    drain(&mut decoder, &mut trial);
    trial
}

/// Every unmodified seed opens, reaches the decoder and decodes without
/// an error: the mutations below start from inputs the decoder takes.
#[test]
fn every_seed_reaches_the_decoder() {
    let ctx = codecs::context();
    for seed in corpus() {
        let trial = try_decode(&ctx, seed.ext, &seed.data);
        assert!(
            trial.reached_decoder() && trial.frames > 0 && trial.errors == 0,
            "{}: {trial:?}",
            seed.name
        );
    }
}

/// Truncation ladder: each stream cut at 24 lengths spread over all of
/// it. A `.m4v` stream cut after its first VOP start code still reaches
/// the decoder (a cut `.h263` stream may hold too few start codes for the
/// probe to claim it, and no container owns the extension).
#[test]
fn truncated_corpus_never_panics() {
    let ctx = codecs::context();
    let (mut trials, mut rejected) = (0usize, 0usize);
    for seed in corpus() {
        let first_vop = seed.data.windows(4).position(|w| w == [0, 0, 1, 0xB6]);
        for k in 0..24usize {
            let len = seed.data.len() * (k + 1) / 25;
            let trial = try_decode(&ctx, seed.ext, &seed.data[..len]);
            if trial.reached_decoder() {
                trials += 1;
            } else {
                rejected += 1;
            }
            if seed.ext == "m4v" && first_vop.is_some_and(|at| at + 4 < len) {
                assert!(trial.reached_decoder(), "{} cut at {len}: {trial:?}", seed.name);
            }
        }
    }
    println!("truncation: {trials} decoder trials, {rejected} inputs no demuxer took to the decoder (no panics)");
}

/// Bit-flip hammer: deterministic single-byte XOR mutations (one to eight
/// per input) until 2000 of them reached the decoder.
#[test]
fn bitflip_corpus_never_panics() {
    const TRIALS: usize = 2000;
    let ctx = codecs::context();
    let corpus = corpus();
    let mut state: u32 = 0x2545_F491;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    let (mut trials, mut rejected) = (0usize, 0usize);
    while trials < TRIALS && trials + rejected < 10 * TRIALS {
        let seed = &corpus[(next() as usize) % corpus.len()];
        let mut mutated = seed.data.clone();
        for _ in 0..1 + (next() as usize) % 8 {
            let at = (next() as usize) % mutated.len();
            mutated[at] ^= (next() & 0xFF) as u8;
        }
        if try_decode(&ctx, seed.ext, &mutated).reached_decoder() {
            trials += 1;
        } else {
            rejected += 1;
        }
    }
    println!("bitflip: {trials} decoder trials, {rejected} inputs no demuxer took to the decoder (no panics)");
    assert_eq!(trials, TRIALS, "only {trials} of {} mutations reached the decoder", trials + rejected);
}
