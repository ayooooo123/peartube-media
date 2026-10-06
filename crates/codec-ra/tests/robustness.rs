//! Untrusted-input robustness test: decoders must never panic on truncated
//! or bit-flipped copies of packets, synthetic ones and the FATE reference
//! samples' own. Deterministic: fixed seed, 2000+ mutations per decoder.
use oxideav_core::{CodecId, CodecParameters, Decoder, MediaType, Packet, RuntimeContext, TimeBase};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_range(&mut self, max: usize) -> usize {
        if max == 0 {
            0
        } else {
            (self.next() as usize) % max
        }
    }
}

const MUTATIONS: usize = 2000;

fn fuzz_decoder(decoder: Box<dyn Decoder>, base_packet: &[u8], seed: u64) {
    fuzz_packets(decoder, &[base_packet.to_vec()], seed);
}

/// Feeds `MUTATIONS` mutated copies of packets picked at random from `base_packets`.
fn fuzz_packets(mut decoder: Box<dyn Decoder>, base_packets: &[Vec<u8>], seed: u64) {
    let mut rng = Rng::new(seed);
    for _ in 0..MUTATIONS {
        let mut data = base_packets[rng.next_range(base_packets.len())].clone();
        let mode = rng.next_range(3);
        match mode {
            0 => {
                // Truncate
                let new_len = rng.next_range(data.len() + 1);
                data.truncate(new_len);
            }
            1 => {
                // Bit flips
                let flips = 1 + rng.next_range(8);
                for _ in 0..flips {
                    if !data.is_empty() {
                        let idx = rng.next_range(data.len());
                        let bit = 1 << rng.next_range(8);
                        data[idx] ^= bit;
                    }
                }
            }
            _ => {
                // Truncate and flip
                let new_len = rng.next_range(data.len() + 1);
                data.truncate(new_len);
                if !data.is_empty() {
                    let idx = rng.next_range(data.len());
                    data[idx] ^= 1 << rng.next_range(8);
                }
            }
        }

        let pkt = Packet::new(0, TimeBase::new(1, 1000), data);
        let _ = decoder.send_packet(&pkt);
        while decoder.receive_frame().is_ok() {}
    }
}

/// The first audio stream of a FATE RealMedia sample and all its packets.
fn rm_audio_packets(sample: &str) -> (CodecParameters, Vec<Vec<u8>>) {
    let mut ctx = RuntimeContext::new();
    codec_ra::register(&mut ctx);
    demux_rm::register(&mut ctx);
    let file = std::fs::File::open(refcheck::fate(sample)).unwrap();
    let mut demuxer = ctx
        .containers
        .open_demuxer("rm", Box::new(file), &ctx.codecs)
        .unwrap_or_else(|e| panic!("{sample}: {e}"));
    let stream = demuxer
        .streams()
        .iter()
        .find(|s| s.params.media_type == MediaType::Audio)
        .unwrap_or_else(|| panic!("{sample}: no audio stream"))
        .clone();
    let mut packets = Vec::new();
    while let Ok(packet) = demuxer.next_packet() {
        if packet.stream_index == stream.index {
            packets.push(packet.data);
        }
    }
    assert!(!packets.is_empty(), "{sample}: no audio packets");
    (stream.params, packets)
}

/// Fuzzes the decoder the registry picks for `sample` with its own packets.
fn fuzz_sample(sample: &str, seed: u64) {
    let (params, packets) = rm_audio_packets(sample);
    let mut ctx = RuntimeContext::new();
    codec_ra::register(&mut ctx);
    let decoder = ctx
        .codecs
        .first_decoder(&params)
        .unwrap_or_else(|e| panic!("{sample}: no decoder: {e}"));
    fuzz_packets(decoder, &packets, seed);
}

#[test]
fn test_ra144_sample_robustness() {
    fuzz_sample("real/ra3_in_rm_file.rm", 0x144_0001);
    fuzz_sample("realaudio/ra3.ra", 0x144_0002);
}

#[test]
fn test_ra288_sample_robustness() {
    fuzz_sample("real/ra_288.rm", 0x288_0001);
    fuzz_sample("realaudio/ra4_288.ra", 0x288_0002);
}

#[test]
fn test_ralf_sample_robustness() {
    fuzz_sample("lossless-audio/luckynight-partial.rmvb", 0x1A1F_0001);
}

#[test]
fn test_cook_sample_robustness() {
    fuzz_sample("real/ra_cook.rm", 0xC00C_0001);
}

#[test]
fn test_sipr_robustness() {
    for (sample, seed) in [
        ("sipr/sipr_5k0.rm", 0x5150_0500),
        ("sipr/sipr_6k5.rm", 0x5150_0605),
        ("sipr/sipr_8k5.rm", 0x5150_0805),
        ("sipr/sipr_16k.rm", 0x5150_1600),
        ("realaudio/RA5.0_16kbps_voice_wideband.ra", 0x5150_1601),
    ] {
        fuzz_sample(sample, seed);
        // Without a bit rate or sample rate the mode follows the packet length.
        let (_, packets) = rm_audio_packets(sample);
        let bare = CodecParameters::audio(CodecId::new("sipr"));
        fuzz_packets(codec_ra::sipr::make_decoder(&bare).unwrap(), &packets, !seed);
    }
}

#[test]
fn test_ra144_robustness() {
    let params = CodecParameters::audio(CodecId::new("ra_144"));
    let decoder = codec_ra::ra144::make_decoder(&params).unwrap();
    let base = vec![0x55u8; 240];
    fuzz_decoder(decoder, &base, 0x144_144);
}

#[test]
fn test_ra288_robustness() {
    let params = CodecParameters::audio(CodecId::new("ra_288"));
    let decoder = codec_ra::ra288::make_decoder(&params).unwrap();
    let base = vec![0xAAu8; 38 * 4];
    fuzz_decoder(decoder, &base, 0x288_288);
}

#[test]
fn test_ralf_robustness() {
    // Valid minimal ralf extradata
    let mut extradata = vec![0u8; 24];
    extradata[..4].copy_from_slice(b"LSD:");
    extradata[4..6].copy_from_slice(&0x103u16.to_be_bytes());
    extradata[8..10].copy_from_slice(&2u16.to_be_bytes()); // channels = 2
    extradata[12..16].copy_from_slice(&44100u32.to_be_bytes()); // rate = 44100
    extradata[16..20].copy_from_slice(&4096u32.to_be_bytes()); // max_frame_size = 4096

    let mut params = CodecParameters::audio(CodecId::new("ralf"));
    params.extradata = extradata;
    let decoder = codec_ra::ralf::make_decoder(&params).unwrap();
    let base = vec![0x33u8; 512];
    fuzz_decoder(decoder, &base, 0x1A1F);
}

#[test]
fn test_cook_robustness() {
    let mut extradata = vec![0u8; 16];
    extradata[..4].copy_from_slice(&0x01000003u32.to_be_bytes()); // JOINT_STEREO
    extradata[4..6].copy_from_slice(&2048u16.to_be_bytes()); // samples_per_frame = 2048
    extradata[6..8].copy_from_slice(&37u16.to_be_bytes()); // subbands = 37
    extradata[12..14].copy_from_slice(&6u16.to_be_bytes()); // js_subband_start = 6
    extradata[14..16].copy_from_slice(&5u16.to_be_bytes()); // js_vlc_bits = 5

    let mut params = CodecParameters::audio(CodecId::new("cook"));
    params.channels = Some(2);
    params.sample_rate = Some(44100);
    params.extradata = extradata;
    let mut decoder = codec_ra::cook::make_decoder(&params).unwrap();
    let base = vec![0x5Au8; 240];
    let _ = decoder.send_packet(&Packet::new(0, TimeBase::new(1, 1000), base.clone()));
    fuzz_decoder(decoder, &base, 0xC00C);
}
