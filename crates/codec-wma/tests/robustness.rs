// Untrusted-input robustness: the WMA decoders must never panic on truncated
// or bit-flipped copies of the reference samples (every stream byte comes
// from untrusted peers). Deterministic: fixed seed, 2000+ mutations per
// sample, no panic — errors are fine.
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

const MUTATIONS_PER_SAMPLE: usize = 2400;

/// Decode `data` with the asf demuxer + the WMA decoders, swallowing every
/// error — only a panic (or hang) fails the test.
fn feed_through(data: &[u8]) {
    let mut ctx = RuntimeContext::new();
    codec_wma::register(&mut ctx);
    demux_asf::register(&mut ctx);
    let probe = ProbeData {
        buf: &data[..data.len().min(256 * 1024)],
        ext: Some("asf"),
    };
    let candidates = ctx.containers.probe_candidates(&probe);
    let format = match candidates.first() {
        Some(c) if c.score >= oxideav_core::PROBE_SCORE_EXTENSION => c.name.to_string(),
        _ => return,
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

fn mutate_and_feed(sample: &str, seed: u64) {
    let path = fate(sample);
    let mut data = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut data).unwrap();

    let mut rng = Rng(seed);
    for m in 0..MUTATIONS_PER_SAMPLE {
        let mut mutated = data.clone();
        match m % 3 {
            0 => {
                // Truncate at a pseudo-random length.
                let cut = 1 + (rng.next() as usize) % data.len();
                mutated.truncate(cut);
            }
            1 => {
                // Flip bits in the first 64 KiB (headers + early packets).
                let pos = (rng.next() as usize) % mutated.len().min(64 * 1024);
                let bit = (rng.next() % 8) as u32;
                mutated[pos] ^= 1 << bit;
            }
            _ => {
                // Both: truncate then flip.
                let cut = 1 + (rng.next() as usize) % data.len();
                mutated.truncate(cut);
                if !mutated.is_empty() {
                    let pos = (rng.next() as usize) % mutated.len();
                    mutated[pos] ^= 1 << (rng.next() % 8);
                }
            }
        }
        feed_through(&mutated);
    }
}

#[test]
fn no_panic_on_truncated_and_bit_flipped_wmalossless() {
    mutate_and_feed("lossless-audio/luckynight-partial.wma", 0x5EED_1234_ABCD_0001);
}

/// The 24-bit path, whose last frame runs past its saved bits.
#[test]
fn no_panic_on_truncated_and_bit_flipped_wmalossless_24bit() {
    mutate_and_feed("lossless-audio/Mega_Weird_Audio_Test_24bit.wma", 0x5EED_1234_ABCD_0024);
}

#[test]
fn no_panic_on_truncated_and_bit_flipped_wmapro() {
    mutate_and_feed("wmapro/Beethovens_9th-1_small.wma", 0x5EED_5678_0000_0042);
}

#[test]
fn no_panic_on_truncated_and_bit_flipped_wmavoice() {
    mutate_and_feed("wmavoice/streaming_CBR-7K.wma", 0x5EED_9ABC_DEAD_BEEF);
}

/// 16 kHz with 16 LSPs: other pitch ranges and LSP tables than 7K.
#[test]
fn no_panic_on_truncated_and_bit_flipped_wmavoice_19k() {
    mutate_and_feed("wmavoice/streaming_CBR-19K.wma", 0x5EED_9ABC_DEAD_0019);
}

#[test]
fn no_panic_on_truncated_and_bit_flipped_wmav2() {
    mutate_and_feed("cover_art/Californication_cover.wma", 0x5EED_0F0F_1234_5678);
}

#[test]
fn zero_audio_dimensions_are_rejected_at_open() {
    let mut ctx = RuntimeContext::new();
    codec_wma::register(&mut ctx);
    for codec in ["wmav1", "wmav2"] {
        for (sample_rate, channels) in [(44_100, 0), (0, 2)] {
            let mut params = oxideav_core::CodecParameters::audio(oxideav_core::CodecId::new(codec));
            params.sample_rate = Some(sample_rate);
            params.channels = Some(channels);
            params.bit_rate = Some(128_000);
            params.options.insert("block_align", "512");
            params.extradata = vec![0; 10];
            params.extradata[if codec == "wmav1" { 2 } else { 4 }] = 0x0f;
            assert!(ctx.codecs.first_decoder(&params).is_err(),
                "{codec} accepted sample_rate={sample_rate}, channels={channels}");
        }
    }
}
