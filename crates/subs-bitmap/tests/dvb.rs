//! DVB subtitles through the real MPEG-TS and Matroska demuxers: every
//! subtitle's full canvas and display interval must equal FFmpeg, including
//! blank states. Mutation inputs are those same real transport packets.

mod support;

use std::panic::AssertUnwindSafe;
use std::path::Path;

use oxideav_core::{Decoder, Error, Frame, MediaType, Packet, RuntimeContext, StreamInfo};
use parking_lot::Mutex;
use refcheck::{Registrar, fate};
use support::{Match, cue_diffs, decode_subtitles, decoded_cues, ffmpeg_reference, ffprobe_packets, reference_cues, remux};

static SERIAL: Mutex<()> = Mutex::new(());
const REGISTRARS: &[Registrar] = &[subs_bitmap::register, oxideav_mpegts::__oxideav_entry, oxideav_mkv::__oxideav_entry];
const CASES: usize = 2400;
const SEED: u64 = 0x4456_425f_4d55_5441;

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    for register in REGISTRARS {
        register(&mut ctx);
    }
    ctx
}

fn packets(path: &Path, container: &str) -> (StreamInfo, Vec<Packet>) {
    let ctx = context();
    let mut demux = ctx.containers.open_demuxer(container, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap();
    let stream = demux.streams().iter().find(|s| s.params.media_type == MediaType::Subtitle).expect("DVB subtitle stream").clone();
    assert_eq!(stream.params.codec_id.as_str(), subs_bitmap::DVB_CODEC_ID);
    let mut packets = Vec::new();
    loop {
        match demux.next_packet() {
            Ok(packet) if packet.stream_index == stream.index => packets.push(packet),
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(error) => panic!("{}: demux: {error}", path.display()),
        }
    }
    assert!(!packets.is_empty());
    (stream, packets)
}

fn assert_matches_ffmpeg(path: &Path) {
    let reference = ffmpeg_reference(path, 0);
    assert!(!reference.cues.is_empty());
    assert!(reference.cues.iter().any(|cue| cue.sub.num_rects == 0), "fixture must exercise blank states");
    assert!(reference.cues.iter().any(|cue| cue.sub.num_rects > 1), "fixture must exercise multiple rectangles");
    let decoded = decode_subtitles(path, REGISTRARS, 0);
    assert!(decoded.errors.is_empty(), "{}: {:?}", path.display(), decoded.errors);
    let want = reference_cues(&reference);
    let got = decoded_cues(&decoded.frames, decoded.stream.time_base, reference.width, reference.height);
    let diffs = cue_diffs(&want, &got, reference.width, Match::Exact);
    assert!(diffs.is_empty(), "{}:\n{}", path.display(), diffs.join("\n"));
    eprintln!("{}: {} DVB states, complete {}x{} canvases and intervals equal FFmpeg", path.display(), got.len(), reference.width, reference.height);
}

#[test]
fn dvb_ts_stream_and_packets_match_ffprobe() {
    let _serial = SERIAL.lock();
    let path = fate("sub/dvbsubtest_filter.ts");
    let (stream, packets) = packets(&path, "mpegts");
    assert_eq!(stream.params.language.as_deref(), Some("eng"));
    assert_eq!(stream.params.extradata, [0, 1, 0x01, 0x52, 0x10]);
    let reference = ffprobe_packets(&path, 0);
    assert_eq!(packets.len(), reference.len());
    for (index, (packet, expected)) in packets.iter().zip(&reference).enumerate() {
        assert_eq!(packet.pts, expected.pts, "packet {index}: pts");
        assert_eq!(packet.dts.or(packet.pts), expected.dts, "packet {index}: dts");
        assert_eq!(&packet.data[..2], &[0x20, 0x00], "packet {index}: DVB private-PES prefix");
        assert_eq!(packet.data.last(), Some(&0xff), "packet {index}: DVB end marker");
        // FFmpeg's DVB parser removes precisely this standard PES framing.
        // Compare every byte of the remaining segments, not merely lengths.
        let segments = &packet.data[2..packet.data.len() - 1];
        assert_eq!(segments.len(), expected.size, "packet {index}: payload size");
        assert_eq!(refcheck::md5_hex(segments), expected.md5, "packet {index}: payload bytes");
    }
}

#[test]
fn dvb_ts_matches_ffmpeg() {
    let _serial = SERIAL.lock();
    assert_matches_ffmpeg(&fate("sub/dvbsubtest_filter.ts"));
}

#[test]
fn dvb_matroska_matches_ffmpeg() {
    let _serial = SERIAL.lock();
    let path = remux(&fate("sub/dvbsubtest_filter.ts"), 0, "dvb_reference.mks", &["-f", "matroska"]);
    assert_matches_ffmpeg(&path);
    std::fs::remove_file(path).unwrap();
}

fn random(state: &mut u64) -> usize {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state as usize
}

fn drain_bounded(decoder: &mut dyn Decoder) {
    for _ in 0..64 {
        match decoder.receive_frame() {
            Ok(Frame::Video(frame)) => {
                let planes = frame.image_planes();
                assert_eq!(planes.len(), 1);
                let plane = &planes[0];
                assert!(plane.stride > 0 && plane.stride % 4 == 0);
                assert!(plane.stride / 4 <= 16384);
                assert!(plane.data.len() / plane.stride <= 16384);
                assert!(plane.data.len() <= 256 << 20);
            }
            Ok(_) => panic!("DVB decoder returned a non-bitmap frame"),
            Err(_) => return,
        }
    }
    panic!("DVB decoder did not stop yielding frames");
}

/// Last real acquisition/mode-change packet preceding each target. Starting
/// there reproduces its actual decoder epoch without repeatedly decoding
/// unrelated earlier epochs for every mutation.
fn epoch_starts(packets: &[Packet]) -> Vec<usize> {
    let mut start = 0;
    packets.iter().enumerate().map(|(index, packet)| {
        let mut data = packet.data.as_slice();
        if data.starts_with(&[0x20, 0]) { data = &data[2..]; }
        while data.len() >= 6 && data[0] == 0x0f {
            let len = usize::from(u16::from_be_bytes([data[4], data[5]]));
            if len > data.len() - 6 { break; }
            if data[1] == 0x10 && len >= 2 && matches!((data[7] >> 2) & 3, 1 | 2) {
                start = index;
            }
            data = &data[6 + len..];
        }
        start
    }).collect()
}

fn mutations(path: &Path, container: &str) {
    let (stream, packets) = packets(path, container);
    let starts = epoch_starts(&packets);
    let ctx = context();
    let mut seed = SEED;
    for case in 0..CASES {
        let target = (case / 4) % packets.len();
        let original = &packets[target];
        let mut mutant = original.clone();
        let len = mutant.data.len();
        match case % 4 {
            0 => mutant.data.truncate(random(&mut seed) % len),
            1 => {
                let at = random(&mut seed) % len;
                mutant.data[at] ^= 1 << (random(&mut seed) % 8);
            }
            2 => {
                let at = random(&mut seed) % len.min(48);
                mutant.data[at] ^= 1 << (random(&mut seed) % 8);
            }
            _ => {
                for _ in 0..2 + random(&mut seed) % 7 {
                    let at = random(&mut seed) % len;
                    mutant.data[at] ^= 1 << (random(&mut seed) % 8);
                }
                if mutant.data == original.data { mutant.data[0] ^= 1; }
            }
        }
        assert_ne!(mutant.data, original.data);
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut decoder = ctx.codecs.first_decoder(&stream.params).unwrap();
            for packet in &packets[starts[target]..target] {
                decoder.send_packet(packet).expect("real epoch prefix");
                drain_bounded(decoder.as_mut());
            }
            let _ = decoder.send_packet(&mutant);
            drain_bounded(decoder.as_mut());
            if let Some(next) = packets.get(target + 1) {
                let _ = decoder.send_packet(next);
                drain_bounded(decoder.as_mut());
            }
            decoder.flush().unwrap();
            drain_bounded(decoder.as_mut());
            decoder.reset().unwrap();
            drain_bounded(decoder.as_mut());
        }));
        assert!(result.is_ok(), "{}: seed={SEED:#x}, case={case}, packet={target}, mode={}", path.display(), case % 4);
    }
}

#[test]
fn dvb_ts_and_matroska_4800_stateful_mutations() {
    let _serial = SERIAL.lock();
    let source = fate("sub/dvbsubtest_filter.ts");
    mutations(&source, "mpegts");
    let mkv = remux(&source, 0, "dvb_mutations.mks", &["-f", "matroska"]);
    mutations(&mkv, "matroska");
    std::fs::remove_file(mkv).unwrap();
}
