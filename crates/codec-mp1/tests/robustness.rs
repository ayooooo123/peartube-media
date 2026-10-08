//! Untrusted input: truncated, bit-flipped and byte-smashed Layer I
//! packets, damaged headers (other layers, rates, channel counts, free
//! format), zero runs, ID3v1 tags and several frames in one packet must
//! never panic, and every frame out keeps the stream's format: 384 samples
//! in one 768-byte plane per declared channel. Deterministic: fixed seeds,
//! 2000 mutations per source.

mod common;

use common::{Rng, Spec, Version, frame};
use oxideav_core::{CodecId, CodecParameters, Decoder, Frame, Packet, TimeBase};

fn mutate(rng: &mut Rng, frames: &[Vec<u8>]) -> Vec<u8> {
    let mut d = frames[rng.below(frames.len() as u32) as usize].clone();
    match rng.below(7) {
        0 => d.truncate(rng.below(d.len() as u32 + 1) as usize),
        1 => {
            for _ in 0..1 + rng.below(8) {
                let i = rng.below(d.len() as u32) as usize;
                d[i] ^= 1 << rng.below(8);
            }
        }
        2 => {
            for _ in 0..1 + rng.below(16) {
                let i = rng.below(d.len() as u32) as usize;
                d[i] = rng.next() as u8;
            }
        }
        3 => {
            // The header and the allocations after it.
            for _ in 0..1 + rng.below(3) {
                let i = rng.below(8) as usize;
                d[i] ^= 1 << rng.below(8);
            }
        }
        4 => {
            // Several frames in one packet, the cut falling anywhere.
            for _ in 0..1 + rng.below(3) {
                d.extend_from_slice(&frames[rng.below(frames.len() as u32) as usize]);
            }
            let n = d.len() as u32;
            d.truncate(n as usize - rng.below(n / 2) as usize);
        }
        5 => {
            // Leading zero bytes (skipped), or a trailing ID3v1 tag.
            if rng.below(2) == 0 {
                let mut z = vec![0; 1 + rng.below(8) as usize];
                z.append(&mut d);
                d = z;
            } else {
                d.extend_from_slice(b"TAG");
                d.resize(d.len() + 125, b' ');
            }
        }
        _ => {
            let i = rng.below(d.len() as u32) as usize;
            d[i..].iter_mut().for_each(|b| *b = 0);
        }
    }
    d
}

fn check_frames(decoder: &mut dyn Decoder, channels: usize) {
    while let Ok(f) = decoder.receive_frame() {
        let Frame::Audio(a) = f else { panic!("not audio") };
        assert_eq!(a.samples, 384);
        assert_eq!(a.data.len(), channels);
        assert!(a.data.iter().all(|p| p.len() == 768));
    }
}

fn damaged(spec: Spec, seed: u64) {
    let mut rng = Rng(seed);
    let frames: Vec<Vec<u8>> = (0..24).map(|_| frame(&mut rng, &spec)).collect();
    let mut params = CodecParameters::audio(CodecId::new("mp1"));
    params.sample_rate = Some(spec.sample_rate());
    params.channels = Some(spec.channels());
    let channels = usize::from(spec.channels());
    let mut decoder = codec_mp1::Mp1Decoder::new(&params).expect("decoder");
    let tb = TimeBase::new(1, i64::from(spec.sample_rate()));
    for i in 0..2000 {
        let _ = decoder.send_packet(&Packet::new(0, tb, mutate(&mut rng, &frames)));
        check_frames(&mut decoder, channels);
        // Now and then a seek and a clean frame, which must decode.
        if i % 50 == 0 {
            decoder.reset().expect("reset");
            decoder.send_packet(&Packet::new(0, tb, frames[0].clone())).expect("clean frame");
            check_frames(&mut decoder, channels);
        }
    }
}

const BASE: Spec = Spec { version: Version::Mpeg1, rate_index: 1, bitrate_index: 14, mode: 0, crc: false, frames: 24, cut: 0 };

#[test]
fn damaged_stereo_frames() {
    damaged(BASE, 0xD001);
}

#[test]
fn damaged_joint_stereo_crc_frames() {
    damaged(Spec { mode: 1, crc: true, rate_index: 0, bitrate_index: 12, ..BASE }, 0xD002);
}

#[test]
fn damaged_mono_frames() {
    damaged(Spec { mode: 3, rate_index: 2, bitrate_index: 1, ..BASE }, 0xD003);
}

#[test]
fn damaged_mpeg25_frames() {
    damaged(Spec { version: Version::Mpeg25, mode: 1, crc: true, rate_index: 2, bitrate_index: 6, ..BASE }, 0xD004);
}

/// Parameters a container could hand over: none, three channels, zero.
#[test]
fn bad_parameters_are_refused() {
    let mut p = CodecParameters::audio(CodecId::new("mp1"));
    assert!(codec_mp1::Mp1Decoder::new(&p).is_err(), "no rate or channels");
    p.sample_rate = Some(44100);
    p.channels = Some(3);
    assert!(codec_mp1::Mp1Decoder::new(&p).is_err(), "three channels");
    p.channels = Some(0);
    assert!(codec_mp1::Mp1Decoder::new(&p).is_err(), "no channels");
}
