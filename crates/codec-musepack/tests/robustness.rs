use std::io::Cursor;

use oxideav_core::{Decoder, Frame, Packet, SampleFormat};
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

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u32() as usize) % bound
        }
    }
}

fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut d = data.to_vec();
    match rng.below(6) {
        0 => {
            // Truncation
            let new_len = rng.below(d.len() + 1);
            d.truncate(new_len);
        }
        1 => {
            // Single bit flip
            if !d.is_empty() {
                let idx = rng.below(d.len());
                let bit = rng.below(8);
                d[idx] ^= 1 << bit;
            }
        }
        2 => {
            // Multiple bit flips
            let count = 1 + rng.below(8);
            for _ in 0..count {
                if !d.is_empty() {
                    let idx = rng.below(d.len());
                    let bit = rng.below(8);
                    d[idx] ^= 1 << bit;
                }
            }
        }
        3 => {
            // Random byte overwrite
            let count = 1 + rng.below(16);
            for _ in 0..count {
                if !d.is_empty() {
                    let idx = rng.below(d.len());
                    d[idx] = rng.next_u32() as u8;
                }
            }
        }
        4 => {
            // Insertion of random bytes
            let insert_len = 1 + rng.below(32);
            let idx = rng.below(d.len() + 1);
            let bytes: Vec<u8> = (0..insert_len).map(|_| rng.next_u32() as u8).collect();
            d.splice(idx..idx, bytes);
        }
        _ => {
            // Fill slice with 0x00 or 0xFF
            if !d.is_empty() {
                let val = if rng.below(2) == 0 { 0x00 } else { 0xFF };
                let start = rng.below(d.len());
                let len = 1 + rng.below(d.len() - start);
                d[start..start + len].fill(val);
            }
        }
    }
    d
}

fn drain_and_verify(dec: &mut Box<dyn Decoder>) {
    let layout = dec.output_audio_format();
    while let Ok(frame) = dec.receive_frame() {
        if let Frame::Audio(audio) = frame {
            if let Some(expected) = layout {
                assert_eq!(expected.sample_format, SampleFormat::S16P);
                assert_eq!(audio.data.len(), expected.channels as usize);
                for plane in &audio.data {
                    assert!(
                        plane.len() >= audio.samples as usize * 2,
                        "plane bytes must fit samples"
                    );
                }
            }
        }
    }
}

#[test]
fn test_mpc7_decoder_robustness() {
    let path = fate("musepack/inside-mp7.mpc");
    let file_bytes = std::fs::read(&path).expect("read file");
    let cursor = Box::new(Cursor::new(file_bytes));
    let mut demuxer = codec_musepack::open_mpc(cursor, &oxideav_core::NullCodecResolver).expect("open mpc");

    let params = demuxer.streams()[0].params.clone();
    let mut packets = Vec::new();
    while let Ok(pkt) = demuxer.next_packet() {
        packets.push(pkt);
        if packets.len() >= 20 {
            break;
        }
    }
    assert!(!packets.is_empty());

    let mut rng = Rng::new(0x4D50_4337_0000_0001);
    let mut dec = codec_musepack::make_mpc7_decoder(&params).expect("make mpc7 decoder");

    for iter in 0..2000 {
        let base_pkt = &packets[rng.below(packets.len())];
        let mutated_data = mutate(&mut rng, &base_pkt.data);
        let mut pkt = Packet::new(base_pkt.stream_index, base_pkt.time_base, mutated_data);
        pkt.pts = base_pkt.pts;
        pkt.duration = base_pkt.duration;

        let _ = dec.send_packet(&pkt);
        drain_and_verify(&mut dec);

        if iter % 100 == 0 {
            let _ = dec.reset();
        }
    }
}

#[test]
fn test_mpc8_decoder_robustness() {
    let path = fate("musepack/inside-mp8.mpc");
    let file_bytes = std::fs::read(&path).expect("read file");
    let cursor = Box::new(Cursor::new(file_bytes));
    let mut demuxer = codec_musepack::open_mpc8(cursor, &oxideav_core::NullCodecResolver).expect("open mpc8");

    let params = demuxer.streams()[0].params.clone();
    let mut packets = Vec::new();
    while let Ok(pkt) = demuxer.next_packet() {
        packets.push(pkt);
    }
    assert!(!packets.is_empty());

    let mut rng = Rng::new(0x4D50_4338_0000_0002);
    let mut dec = codec_musepack::make_mpc8_decoder(&params).expect("make mpc8 decoder");

    for iter in 0..2000 {
        let base_pkt = &packets[rng.below(packets.len())];
        let mutated_data = mutate(&mut rng, &base_pkt.data);
        let mut pkt = Packet::new(base_pkt.stream_index, base_pkt.time_base, mutated_data);
        pkt.pts = base_pkt.pts;
        pkt.duration = base_pkt.duration;

        let _ = dec.send_packet(&pkt);
        drain_and_verify(&mut dec);

        if iter % 50 == 0 {
            let _ = dec.reset();
        }
    }
}

#[test]
fn test_mpc_demuxer_robustness() {
    let path = fate("musepack/inside-mp7.mpc");
    let orig_bytes = std::fs::read(&path).expect("read file");

    let mut rng = Rng::new(0x4D50_4337_0000_0003);

    for _ in 0..2000 {
        let mutated = mutate(&mut rng, &orig_bytes);
        let cursor = Box::new(Cursor::new(mutated));
        if let Ok(mut dmx) = codec_musepack::open_mpc(cursor, &oxideav_core::NullCodecResolver) {
            let mut count = 0;
            while let Ok(_pkt) = dmx.next_packet() {
                count += 1;
                if count >= 100 {
                    break;
                }
            }
        }
    }
}

#[test]
fn test_mpc8_demuxer_robustness() {
    let path = fate("musepack/inside-mp8.mpc");
    let orig_bytes = std::fs::read(&path).expect("read file");

    let mut rng = Rng::new(0x4D50_4338_0000_0004);

    for _ in 0..2000 {
        let mutated = mutate(&mut rng, &orig_bytes);
        let cursor = Box::new(Cursor::new(mutated));
        if let Ok(mut dmx) = codec_musepack::open_mpc8(cursor, &oxideav_core::NullCodecResolver) {
            let mut count = 0;
            while let Ok(_pkt) = dmx.next_packet() {
                count += 1;
                if count >= 50 {
                    break;
                }
            }
        }
    }
}
