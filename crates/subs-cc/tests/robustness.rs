//! Truncated and bit-flipped copies of real caption-carrying packets never
//! panic: the video packets of every caption input through extraction and
//! the timeline, and the caption triplets through both decoders (buffered
//! and real time EIA-608, CEA-708) and their `Decoder` implementations.
//! Fixed seed, at least 2000 mutations per stage and input.

mod support;

use oxideav_core::{CodecId, CodecParameters, Decoder, Packet, TimeBase};
use subs_cc::cea708::Cea708;
use subs_cc::eia608::Cc608;
use subs_cc::{CaptionTimeline, CcExtractor};
use support::{generated, our_captions, rollup, scte20, video_packets};

const MUTATIONS: usize = 2000;

/// xorshift64*
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

/// One mutation of `data`: truncated, bits flipped, bytes overwritten with
/// boundary values, or a span duplicated.
fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    match rng.below(4) {
        0 => out.truncate(rng.below(data.len() + 1)),
        1 => {
            for _ in 0..1 + rng.below(8) {
                if !out.is_empty() {
                    let at = rng.below(out.len());
                    out[at] ^= 1 << rng.below(8);
                }
            }
        }
        2 => {
            for _ in 0..1 + rng.below(4) {
                if !out.is_empty() {
                    let at = rng.below(out.len());
                    out[at] = [0x00, 0x01, 0x03, 0x7f, 0x80, 0xff, 0xfc, 0xfd, 0xfe][rng.below(9)];
                }
            }
        }
        _ => {
            if !out.is_empty() {
                let from = rng.below(out.len());
                let len = rng.below(out.len() - from + 1);
                let span = out[from..from + len].to_vec();
                let at = rng.below(out.len() + 1);
                out.splice(at..at, span);
            }
        }
    }
    out
}

/// Mutated video packets through a stream's extractor and timeline.
fn extraction_survives(path: &std::path::Path, seed: u64) {
    let (stream, packets) = video_packets(path);
    let codec = stream.params.codec_id.as_str().to_string();
    let mut rng = Rng(seed);
    let mut extractor = CcExtractor::new(&codec, &stream.params.extradata).unwrap();
    let mut timeline = CaptionTimeline::new();
    let mut mutated = 0;
    while mutated < MUTATIONS {
        for packet in &packets {
            let data = if rng.below(3) == 0 {
                mutated += 1;
                mutate(&mut rng, &packet.data)
            } else {
                packet.data.clone()
            };
            let triplets = extractor.extract(&data);
            let pts = packet.pts.map(|p| if rng.below(50) == 0 { p ^ (rng.next() as i64) } else { p });
            let _ = timeline.push(pts, packet.dts, triplets);
            if rng.below(500) == 0 {
                extractor.reset();
                timeline.reset();
            }
        }
        let _ = extractor.finish();
        let _ = timeline.finish();
        // A fresh extractor with mutated extradata (avcC/hvcC length size).
        let extradata = mutate(&mut rng, &stream.params.extradata);
        extractor = CcExtractor::new(&codec, &extradata).unwrap();
        mutated += 1;
    }
    println!("{}: {mutated} mutated video packets", path.display());
}

#[test]
fn mutated_video_packets() {
    for (path, seed) in [
        (rollup(), 1),
        (scte20(), 2),
        (generated("h264.mkv"), 3),
        (generated("h264.ts"), 4),
        (generated("hevc.mkv"), 5),
        (generated("hevc.ts"), 6),
    ] {
        extraction_survives(&path, seed);
    }
}

/// The timed caption packets of the inputs, as the player feeds them.
fn caption_packets() -> Vec<(i64, Vec<u8>)> {
    let mut out = Vec::new();
    for path in [rollup(), scte20(), generated("h264.mkv")] {
        let captions = our_captions(&path);
        let (num, den) = captions.time_base;
        for (ts, triplets) in captions.pictures {
            let us = ts.and_then(|t| subs_cc::eia608::ticks_to_us(t, num, den)).unwrap_or(0);
            out.push((us, triplets.into_iter().flatten().collect()));
        }
    }
    out
}

fn packet(us: i64, data: Vec<u8>) -> Packet {
    let mut packet = Packet::new(0, TimeBase::new(1, 1_000_000), data);
    packet.pts = Some(us);
    packet
}

fn drain(decoder: &mut dyn Decoder) {
    while decoder.receive_frame().is_ok() {}
}

#[test]
fn mutated_caption_packets() {
    let packets = caption_packets();
    assert!(packets.len() > 100, "{} caption packets", packets.len());
    let mut rng = Rng(0x608_708);
    let params = CodecParameters::subtitle(CodecId::new("eia_608"));
    let mut cc608 = Cc608::new();
    let mut live608 = Cc608::real_time();
    let mut cea708 = Cea708::new(1);
    let mut dec608 = subs_cc::eia608::make_decoder(&params).unwrap();
    let mut dec708 = subs_cc::cea708::make_decoder(&params).unwrap();
    let mut mutated = 0;
    while mutated < MUTATIONS * 2 {
        for (us, data) in &packets {
            let data = if rng.below(2) == 0 {
                mutated += 1;
                mutate(&mut rng, data)
            } else {
                data.clone()
            };
            let us = if rng.below(40) == 0 { rng.next() as i64 } else { *us };
            let whole = data.len() - data.len() % 3;
            let _ = cc608.decode(&data[..whole], Some(us));
            let _ = live608.decode(&data[..whole], if rng.below(20) == 0 { None } else { Some(us) });
            let _ = cea708.decode(&data[..whole], us);
            let _ = dec608.send_packet(&packet(us, data.clone()));
            drain(&mut *dec608);
            let _ = dec708.send_packet(&packet(us, data.clone()));
            drain(&mut *dec708);
            if rng.below(300) == 0 {
                cc608.flush();
                live608.flush();
                cea708.flush();
                let _ = dec608.reset();
                let _ = dec708.reset();
            }
        }
        let _ = cc608.finish();
        let _ = dec608.flush();
        drain(&mut *dec608);
        let _ = dec708.flush();
        drain(&mut *dec708);
        let _ = dec608.reset();
        let _ = dec708.reset();
    }
    println!("{mutated} mutated caption packets");
}
