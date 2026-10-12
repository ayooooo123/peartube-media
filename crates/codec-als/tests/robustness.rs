//! Untrusted input: truncated, bit-flipped and byte-smashed ALS frames from
//! the FATE files (Rice and BGMC coding, LTP, joint stereo, multichannel
//! coding, block switching, float with MLZ), and damaged
//! AudioSpecificConfig / ALSSpecificConfig, must never panic or hang.
//! Deterministic: fixed seeds, 2000 mutations per source.

use oxideav_core::{CodecParameters, Decoder, MediaType, Packet, RuntimeContext};
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

    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut d = data.to_vec();
    match rng.below(4) {
        0 => d.truncate(rng.below(d.len() + 1)),
        1 => {
            for _ in 0..1 + rng.below(8) {
                if !d.is_empty() {
                    let i = rng.below(d.len());
                    d[i] ^= 1 << rng.below(8);
                }
            }
        }
        2 => {
            for _ in 0..1 + rng.below(16) {
                if !d.is_empty() {
                    let i = rng.below(d.len());
                    d[i] = rng.next() as u8;
                }
            }
        }
        _ => {
            // The block headers at the start of the frame.
            let n = d.len().min(24);
            for _ in 0..1 + rng.below(4) {
                if n > 0 {
                    let i = rng.below(n);
                    d[i] = rng.next() as u8;
                }
            }
        }
    }
    d
}

/// The stream parameters and the first `n` packets of `sample`.
fn packets(sample: &str, n: usize) -> (CodecParameters, Vec<Packet>) {
    let mut ctx = RuntimeContext::new();
    codec_als::register(&mut ctx);
    oxideav_mp4::__oxideav_entry(&mut ctx);
    let path = fate(sample);
    let format = refcheck::probe_container(&ctx, &path).expect("probe");
    let file = std::fs::File::open(&path).expect("open");
    let mut demuxer = ctx.containers.open_demuxer(&format, Box::new(file), &ctx.codecs).expect("open demuxer");
    let stream = demuxer.streams().iter().find(|s| s.params.media_type == MediaType::Audio).expect("audio").clone();
    let mut out = Vec::new();
    while let Ok(p) = demuxer.next_packet() {
        if p.stream_index == stream.index {
            out.push(p);
            if out.len() == n {
                break;
            }
        }
    }
    (stream.params, out)
}

fn drain(decoder: &mut dyn Decoder) {
    while decoder.receive_frame().is_ok() {}
}

/// Damaged packets of `sample`. Most of the conformance files are one MP4
/// sample holding every frame (the decoder takes them one after another,
/// as decode.c calls alsdec.c until the packet is used up), so each mutant
/// is at most the first 16 KiB of its packet: the first few frames.
fn damaged_frames(sample: &str, seed: u64) {
    let (params, packets) = packets(sample, 24);
    let mut decoder = codec_als::AlsDecoder::new(&params).expect("decoder");
    let mut rng = Rng(seed);
    for i in 0..2000 {
        let p = &packets[rng.below(packets.len())];
        let mut q = p.clone();
        q.data = mutate(&mut rng, &p.data[..p.data.len().min(16 * 1024)]);
        let _ = decoder.send_packet(&q);
        drain(&mut decoder);
        // Now and then a seek and clean frames, so damage meets fresh state.
        if i % 50 == 0 {
            let _ = decoder.reset();
            let mut clean = packets[0].clone();
            clean.data.truncate(16 * 1024);
            let _ = decoder.send_packet(&clean);
            drain(&mut decoder);
        }
    }
}

#[test]
fn damaged_rice_frames_never_panic() {
    damaged_frames("lossless-audio/als_00_2ch48k16b.mp4", 0xA100);
}

#[test]
fn damaged_block_switching_joint_stereo_frames_never_panic() {
    damaged_frames("lossless-audio/als_01_2ch48k16b.mp4", 0xA101);
}

#[test]
fn damaged_ltp_frames_never_panic() {
    damaged_frames("lossless-audio/als_02_2ch48k16b.mp4", 0xA102);
}

#[test]
fn damaged_multichannel_frames_never_panic() {
    damaged_frames("lossless-audio/als_04_2ch48k16b.mp4", 0xA104);
}

#[test]
fn damaged_bgmc_frames_never_panic() {
    damaged_frames("lossless-audio/als_05_2ch48k16b.mp4", 0xA105);
}

#[test]
fn damaged_float_frames_never_panic() {
    damaged_frames("lossless-audio/als_07_2ch192k32bF.mp4", 0xA107);
}

/// Damaged configs: every field of the ALSSpecificConfig, then a few
/// frames through each decoder that accepts its config.
#[test]
fn damaged_configs_never_panic() {
    let mut rng = Rng(0xC0F1);
    let sources: Vec<_> = ["lossless-audio/als_00_2ch48k16b.mp4", "lossless-audio/als_04_2ch48k16b.mp4", "lossless-audio/als_07_2ch192k32bF.mp4"]
        .iter()
        .map(|s| packets(s, 3))
        .collect();
    for i in 0..3000 {
        let (params, packets) = &sources[i % sources.len()];
        let mut p = params.clone();
        // Most of the damage lands in the config's fixed fields.
        let n = p.extradata.len().min(32);
        if rng.below(4) == 0 {
            p.extradata = mutate(&mut rng, &p.extradata);
        } else {
            for _ in 0..1 + rng.below(3) {
                let at = rng.below(n);
                p.extradata[at] = rng.next() as u8;
            }
        }
        let Ok(mut decoder) = codec_als::AlsDecoder::new(&p) else { continue };
        for q in packets {
            let mut q = q.clone();
            q.data.truncate(16 * 1024);
            let _ = decoder.send_packet(&q);
            drain(&mut decoder);
        }
    }
}
