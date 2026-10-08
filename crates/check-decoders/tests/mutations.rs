//! Hostile input on the changed paths: truncated and bit-flipped copies
//! of FFmpeg's packets of FATE samples go through the decoders, which
//! must return errors, never panic (fixed seed, 2,000 mutants each).

use check_decoders::{decode_packets, ffmpeg_packets};
use oxideav_core::{CodecId, CodecParameters, Frame, Packet, SampleFormat};

/// xorshift64*: a fixed sequence, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// `count` mutants of `packets`: in each, one packet is cut short, or
/// has 1 to 8 bits flipped, or both.
fn mutants(packets: &[Packet], count: usize, seed: u64) -> impl Iterator<Item = Vec<Packet>> + '_ {
    let mut rng = Rng(seed);
    (0..count).map(move |_| {
        let mut mutant = packets.to_vec();
        let victim = rng.below(mutant.len());
        let data = &mut mutant[victim].data;
        let kind = rng.below(3);
        if kind != 1 && !data.is_empty() {
            let flips = 1 + rng.below(8);
            for _ in 0..flips {
                let bit = rng.below(data.len() * 8);
                data[bit / 8] ^= 1 << (bit % 8);
            }
        }
        if kind != 0 {
            let keep = rng.below(data.len() + 1);
            data.truncate(keep);
        }
        mutant
    })
}

#[test]
fn mp2_survives_truncated_and_flipped_packets() {
    let path = refcheck::fate("h264/h264_intra_first-small.ts");
    let packets = ffmpeg_packets(&path, "a:0", None);
    let mut params = CodecParameters::audio(CodecId::new("mp2"));
    params.sample_rate = Some(48_000);
    params.channels = Some(2);
    params.sample_format = Some(SampleFormat::S16);
    for mutant in mutants(&packets, 2_000, 0x6d70_3221) {
        let _ = decode_packets(&[oxideav_mp2::register], &params, &mutant);
    }
}

#[test]
fn h264_recovery_survives_truncated_and_flipped_packets() {
    // A recovery point SEI (recovery_frame_cnt 21) before a P picture:
    // the SEI, recovery and output-gate paths, at 320x240.
    let path = refcheck::fate("h264/intra_refresh.h264");
    let packets: Vec<Packet> = ffmpeg_packets(&path, "v:0", None).into_iter().take(40).collect();
    let mut params = CodecParameters::video(CodecId::new("h264"));
    params.options.insert("video_delay", "1");
    for mutant in mutants(&packets, 2_000, 0x6832_3634) {
        let _ = decode_packets(&[oxideav_h264::register], &params, &mutant);
    }
}

#[test]
fn ac3_survives_truncated_and_flipped_packets() {
    // AC-3 5.1 with coupling and block switching; E-AC-3 5.1 with
    // spectral extension; E-AC-3 7.1 with dependent substreams.
    for (sample, codec, seed) in [
        ("ac3/monsters_inc_5.1_448_small.ac3", "ac3", 0x6163_3301),
        ("eac3/csi_miami_5.1_256_spx_small.eac3", "eac3", 0x6563_3302),
        ("eac3/the_great_wall_7.1.eac3", "eac3", 0x6563_3303),
    ] {
        let path = refcheck::fate(sample);
        let packets: Vec<Packet> = ffmpeg_packets(&path, "a:0", None).into_iter().take(12).collect();
        let params = CodecParameters::audio(CodecId::new(codec));
        for mutant in mutants(&packets, 2_000, seed) {
            let (decoded, _) = decode_packets(&[oxideav_ac3::register], &params, &mutant);
            // Every decode call consumes input, and a frame is at most 6
            // blocks of 16 channels.
            assert!(decoded.frames.len() <= mutant.iter().map(|p| p.data.len()).sum::<usize>());
            for frame in &decoded.frames {
                let Frame::Audio(audio) = frame else { panic!("{sample}: not an audio frame") };
                assert!(audio.samples <= 1536 && audio.data.len() <= 16, "{sample}: {} x {}", audio.samples, audio.data.len());
            }
        }
    }
}
