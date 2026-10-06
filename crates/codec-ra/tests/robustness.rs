//! Untrusted-input robustness test: decoders must never panic on truncated
//! or bit-flipped copies of packets. Deterministic: fixed seed, 2000+ mutations
//! per codec.
use oxideav_core::{CodecId, CodecParameters, Decoder, Packet, TimeBase};

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

fn fuzz_decoder(mut decoder: Box<dyn Decoder>, base_packet: &[u8], seed: u64) {
    let mut rng = Rng::new(seed);
    for _ in 0..MUTATIONS {
        let mut data = base_packet.to_vec();
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
