//! Stateful robustness: 2400 deterministic truncations/bit mutations per
//! real packet source (SUP and both Matroska remux layouts). Each mutation
//! is surrounded by the real preceding/following packets, then the decoder
//! is reset and its entire valid decode is compared again with FFmpeg.

mod support;

use std::panic::AssertUnwindSafe;
use std::path::Path;
use parking_lot::Mutex;

use oxideav_core::{Decoder, Error, Frame, Packet, RuntimeContext, TimeBase};
use support::{Reference, ffmpeg_reference, remux, to_us};

const CASES: usize = 2400;
const SEED: u64 = 0x5047_535f_4d55_5441;
// Keep large mutated canvases sequential even with the default test runner.
static SERIAL: Mutex<()> = Mutex::new(());

fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn mutate(packet: &Packet, case: usize, state: &mut u64) -> Packet {
    let mut packet = packet.clone();
    let original = packet.data.clone();
    let len = packet.data.len();
    assert!(len > 0, "reference packet must contain a PGS segment");
    match case % 4 {
        0 => packet.data.truncate(random(state) as usize % len),
        1 => {
            let index = random(state) as usize % len;
            packet.data[index] ^= 1 << (random(state) % 8);
        }
        2 => {
            // Target segment headers, dimensions, object references and
            // palette ids, not just the large RLE payloads.
            let index = random(state) as usize % len.min(32);
            packet.data[index] ^= 1 << (random(state) % 8);
        }
        _ => {
            let flips = 2 + random(state) as usize % 7;
            for _ in 0..flips {
                let index = random(state) as usize % len;
                packet.data[index] ^= 1 << (random(state) % 8);
            }
            if packet.data == original {
                packet.data[0] ^= 1;
            }
        }
    }
    assert_ne!(packet.data, original, "case {case} must actually mutate its packet");
    packet
}

fn drain(decoder: &mut dyn Decoder, mut receive: impl FnMut(Frame)) {
    for _ in 0..64 {
        match decoder.receive_frame() {
            Ok(frame) => receive(frame),
            Err(_) => return,
        }
    }
    panic!("PGS receive_frame never reached NeedMore/Eof/error");
}

fn bounded(frame: Frame) {
    let Frame::Video(frame) = frame else { panic!("PGS returned a non-bitmap frame") };
    let planes = frame.image_planes();
    assert_eq!(planes.len(), 1);
    let plane = &planes[0];
    assert!(plane.stride > 0 && plane.stride % 4 == 0);
    assert!(plane.stride / 4 <= 16384);
    assert!(plane.data.len() / plane.stride <= 16384);
    assert!(plane.data.len() <= 256 << 20, "mutated canvas exceeds the allocation cap");
}

/// The first END segment in the actual packets, extracted even when it is
/// inside a merged Matroska display-set block. Replaying it after the
/// truncated second display set makes mutations in that set reach the RLE
/// decoder too, rather than sitting unrendered in its cache.
fn end_segment(packets: &[Packet]) -> Packet {
    for packet in packets {
        let mut at = 0;
        while at + 3 <= packet.data.len() {
            let len = usize::from(u16::from_be_bytes([packet.data[at + 1], packet.data[at + 2]]));
            let end = at + 3 + len;
            assert!(end <= packet.data.len());
            if packet.data[at] == 0x80 {
                let mut marker = packet.clone();
                marker.data = packet.data[at..end].to_vec();
                return marker;
            }
            at = end;
        }
    }
    panic!("reference has no END segment");
}

fn valid_decode_matches_ffmpeg(decoder: &mut dyn Decoder, packets: &[Packet], time_base: TimeBase, reference: &Reference) {
    let mut index = 0;
    let mut compare = |frame| {
        let Frame::Video(frame) = frame else { panic!("PGS returned a non-bitmap frame") };
        let expected = reference.cues.get(index).expect("extra valid-decode frame after reset");
        assert_eq!(to_us(frame.pts.expect("PGS pts"), time_base), expected.sub.start_us());
        assert_eq!(frame.display_duration().map(|d| d.as_micros() as i64), expected.sub.end_us().map(|end| end - expected.sub.start_us()));
        let plane = &frame.image_planes()[0];
        assert_eq!(plane.stride, reference.width * 4);
        assert!(plane.data == expected.canvas, "reset decode frame {index} differs from FFmpeg's complete canvas");
        index += 1;
    };
    for packet in packets {
        decoder.send_packet(packet).expect("original packet after reset");
        drain(decoder, &mut compare);
    }
    decoder.flush().unwrap();
    drain(decoder, &mut compare);
    assert_eq!(index, reference.cues.len(), "all valid-decode frames after reset");
}

fn exercise(path: &Path, container: &str) {
    let reference = ffmpeg_reference(path, 0);
    assert!(!reference.cues.is_empty());
    let mut ctx = RuntimeContext::new();
    oxideav_mkv::__oxideav_entry(&mut ctx);
    subs_bitmap::register(&mut ctx);
    let mut demux = ctx.containers.open_demuxer(container, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap();
    let stream = demux.streams()[0].clone();
    let mut packets = Vec::new();
    loop {
        match demux.next_packet() {
            Ok(packet) => packets.push(packet),
            Err(Error::Eof) => break,
            Err(error) => panic!("{}: demux {error}", path.display()),
        }
    }
    assert!(!packets.is_empty());
    let marker = end_segment(&packets);
    let mut seed = SEED;
    for case in 0..CASES {
        // Every packet sees all four mutation modes, including tiny END
        // packets and each of the large ODS packets.
        let target = (case / 4) % packets.len();
        let mutated = mutate(&packets[target], case, &mut seed);
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut decoder = ctx.codecs.first_decoder(&stream.params).unwrap();
            for (index, packet) in packets.iter().enumerate() {
                let packet = if index == target { &mutated } else { packet };
                let _ = decoder.send_packet(packet);
                drain(decoder.as_mut(), bounded);
            }
            let _ = decoder.send_packet(&marker);
            drain(decoder.as_mut(), bounded);
            let _ = decoder.flush();
            drain(decoder.as_mut(), bounded);
            decoder.reset().unwrap();
            valid_decode_matches_ffmpeg(decoder.as_mut(), &packets, stream.time_base, &reference);
        }));
        assert!(result.is_ok(), "{}: seed={SEED:#x}, case={case}, packet={target}, mode={}, original={} bytes, mutated={} bytes", path.display(), case % 4, packets[target].data.len(), mutated.data.len());
    }
}

#[test]
fn pgs_sup_2400_stateful_mutations() {
    let _serial = SERIAL.lock();
    exercise(&refcheck::fate("sub/pgs_sub.sup"), "sup");
}

#[test]
fn pgs_matroska_segments_2400_stateful_mutations() {
    let _serial = SERIAL.lock();
    let path = remux(&refcheck::fate("sub/pgs_sub.sup"), 0, "pgs_mutations_segments.mks", &["-f", "matroska"]);
    exercise(&path, "matroska");
    std::fs::remove_file(path).unwrap();
}

#[test]
fn pgs_matroska_display_sets_2400_stateful_mutations() {
    let _serial = SERIAL.lock();
    let path = remux(&refcheck::fate("sub/pgs_sub.sup"), 0, "pgs_mutations_sets.mks", &["-bsf:s", "pgs_frame_merge", "-f", "matroska"]);
    exercise(&path, "matroska");
    std::fs::remove_file(path).unwrap();
}
