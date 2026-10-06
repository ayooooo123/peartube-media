//! Untrusted input robustness tests for RealVideo decoders.
//! Asserts no panics on truncated and bit-flipped inputs (2000 mutations).

use oxideav_core::{Packet, RuntimeContext};
use refcheck::fate;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.0 >> 32) as u32
    }

    fn next_range(&mut self, max: usize) -> usize {
        if max == 0 {
            0
        } else {
            (self.next_u32() as usize) % max
        }
    }
}

#[test]
fn test_robustness_no_panic() {
    let mut ctx = RuntimeContext::new();
    codec_rv::register(&mut ctx);
    demux_rm::register(&mut ctx);

    let path = fate("real/G2_with_SVT_320_240.rm");
    let file = std::fs::File::open(&path).expect("open test sample");
    let mut demuxer = ctx
        .containers
        .open_demuxer("rm", Box::new(file), &ctx.codecs)
        .expect("open demuxer");

    let video_stream = demuxer
        .streams()
        .iter()
        .find(|s| s.params.media_type == oxideav_core::MediaType::Video)
        .expect("video stream")
        .clone();

    let mut original_packets = Vec::new();
    while let Ok(pkt) = demuxer.next_packet() {
        if pkt.stream_index == video_stream.index {
            original_packets.push(pkt);
            if original_packets.len() >= 5 {
                break;
            }
        }
    }

    assert!(!original_packets.is_empty(), "collected packets for mutation");

    let mut rng = Rng::new(0xDEAD_BEEF_1234_5678);

    for i in 0..2000 {
        let pkt_idx = rng.next_range(original_packets.len());
        let orig = &original_packets[pkt_idx];
        let mut data = orig.data.clone();

        if data.is_empty() {
            continue;
        }

        let mode = rng.next_range(3);
        match mode {
            0 => {
                // Truncation
                let cut = rng.next_range(data.len());
                data.truncate(cut);
            }
            1 => {
                // Single bit flip
                let byte_idx = rng.next_range(data.len());
                let bit_idx = rng.next_range(8);
                data[byte_idx] ^= 1 << bit_idx;
            }
            _ => {
                // Multiple byte corruptions
                let count = 1 + rng.next_range(16);
                for _ in 0..count {
                    let byte_idx = rng.next_range(data.len());
                    data[byte_idx] = rng.next_u32() as u8;
                }
            }
        }

        let mut decoder = ctx
            .codecs
            .first_decoder(&video_stream.params)
            .expect("first_decoder");

        let mut pkt = orig.clone();
        pkt.data = data;

        // Must not panic!
        let _ = decoder.send_packet(&pkt);
        let _ = decoder.receive_frame();
        let _ = decoder.flush();
    }
}
