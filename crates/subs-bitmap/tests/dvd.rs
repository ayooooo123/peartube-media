//! Complete FFmpeg comparison for paired VobSub, embedded Matroska (also
//! zlib-compressed blocks) and MPEG-PS, including split SPUs and stop times.
mod support;

use std::io::Cursor;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;
use oxideav_core::{Demuxer, Error, Frame, MediaType, Packet, RuntimeContext, StreamInfo};
use parking_lot::Mutex;
use support::{Match, cue_diffs, decoded_cues, ffmpeg_reference, ffprobe_packets, reference_cues};

static SERIAL: Mutex<()> = Mutex::new(());
fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    subs_bitmap::register(&mut ctx);
    oxideav_mkv::__oxideav_entry(&mut ctx);
    demux_misc::register(&mut ctx);
    ctx
}
fn open(path: &Path) -> Box<dyn Demuxer> {
    if path.extension().and_then(|e| e.to_str()) == Some("idx") {
        subs_bitmap::open_vobsub(Box::new(std::fs::File::open(path).unwrap()), Box::new(std::fs::File::open(path.with_extension("sub")).unwrap())).unwrap()
    } else {
        let ctx = context();
        let format = if path.extension().and_then(|e| e.to_str()) == Some("sub") { "mpeg" } else { "matroska" };
        ctx.containers.open_demuxer(format, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap()
    }
}
fn packets(path: &Path, nth: usize) -> (StreamInfo, Vec<Packet>) {
    let mut demux = open(path);
    let mut packets = Vec::new();
    loop {
        match demux.next_packet() {
            Ok(packet) => packets.push(packet),
            Err(Error::Eof) => break,
            Err(error) => panic!("{}: {error}", path.display()),
        }
    }
    // MPEG-PS discovers streams while reading PES packets, unlike Matroska.
    let stream = demux.streams().iter().filter(|s| s.params.media_type == MediaType::Subtitle).nth(nth).expect("DVD subtitle stream").clone();
    packets.retain(|packet| packet.stream_index == stream.index);
    (stream, packets)
}
fn compare(path: &Path, nth: usize) {
    let reference = ffmpeg_reference(path, nth);
    assert!(!reference.cues.is_empty(), "{} stream {nth}", path.display());
    let (stream, packets) = packets(path, nth);
    let ctx = context();
    let mut decoder = ctx.codecs.first_decoder(&stream.params).unwrap();
    let mut frames = Vec::new();
    for (index, packet) in packets.iter().enumerate() {
        decoder.send_packet(packet).unwrap_or_else(|error| panic!("{} stream {nth} packet {index}: pts={:?} size={} prefix={:02x?}: {error}", path.display(), packet.pts, packet.data.len(), &packet.data[..packet.data.len().min(12)]));
        while let Ok(frame) = decoder.receive_frame() { frames.push(frame); }
    }
    decoder.flush().unwrap();
    while let Ok(frame) = decoder.receive_frame() { frames.push(frame); }
    let expected = reference_cues(&reference);
    let actual = decoded_cues(&frames, stream.time_base, reference.width, reference.height);
    let diffs = cue_diffs(&expected, &actual, reference.width, Match::Exact);
    assert!(diffs.is_empty(), "{} stream {nth}:\n{}", path.display(), diffs.join("\n"));
    eprintln!("{} stream {nth}: {} complete {}x{} DVD canvases and intervals exactly match FFmpeg", path.display(), actual.len(), reference.width, reference.height);
}
#[test]
fn paired_vobsub_packets_match_every_ffprobe_field() {
    let _serial = SERIAL.lock();
    let path = refcheck::fate("sub/vobsub.idx");
    let (stream, packets) = packets(&path, 0);
    let expected = ffprobe_packets(&path, 0);
    assert_eq!(stream.time_base, oxideav_core::TimeBase::new(1, 1000));
    assert_eq!(packets.len(), expected.len());
    for (i, (packet, reference)) in packets.iter().zip(expected).enumerate() {
        assert_eq!(packet.pts, reference.pts, "{i} pts");
        assert_eq!(packet.dts.or(packet.pts), reference.dts, "{i} dts");
        assert_eq!(packet.duration, reference.duration, "{i} duration");
        assert_eq!(packet.data.len(), reference.size, "{i} size");
        assert_eq!(refcheck::md5_hex(&packet.data), reference.md5, "{i} bytes");
    }
    let mut demux = open(&path);
    let landed = demux.seek_to(0, 200_000).unwrap();
    assert_eq!(landed, 199_724);
    let packet = demux.next_packet().unwrap();
    assert_eq!(packet.pts, Some(landed));
    assert_eq!(packet.data, packets.iter().find(|p| p.pts == Some(landed)).unwrap().data);
}
#[test]
fn paired_vobsub_matches_ffmpeg() { let _serial = SERIAL.lock(); compare(&refcheck::fate("sub/vobsub.idx"), 0); }
#[test]
fn matroska_dvd_all_tracks_match_ffmpeg() {
    let _serial = SERIAL.lock();
    for nth in 0..3 { compare(&refcheck::fate("filter/242_4.mkv"), nth); }
    compare(&refcheck::fate("mkv/subtitle_zlib.mks"), 0);
}
#[test]
fn mpeg_program_stream_dvd_matches_ffmpeg() { let _serial = SERIAL.lock(); compare(&refcheck::fate("sub/vobsub.sub"), 0); }

fn random(state: &mut u64) -> usize { *state ^= *state << 13; *state ^= *state >> 7; *state ^= *state << 17; *state as usize }
fn drain(decoder: &mut dyn oxideav_core::Decoder) {
    for _ in 0..64 {
        match decoder.receive_frame() {
            Ok(Frame::Video(frame)) => {
                let planes = frame.image_planes(); assert_eq!(planes.len(), 1);
                assert!(planes[0].stride > 0 && planes[0].stride <= 16384 * 4);
                assert!(planes[0].data.len() <= 256 << 20);
            }
            Ok(_) => panic!("non-bitmap DVD frame"),
            Err(_) => return,
        }
    }
    panic!("unbounded DVD output");
}
#[test]
fn dvd_4800_real_packet_mutations_and_reset_recovery() {
    let _serial = SERIAL.lock();
    let ctx = context();
    let mut seed = 0x4456_445f_4d55_5441;
    for sample in ["sub/vobsub.idx", "mkv/subtitle_zlib.mks"] {
        let reference = ffmpeg_reference(&refcheck::fate(sample), 0);
        let (stream, packets) = packets(&refcheck::fate(sample), 0);
        for case in 0..2400 {
            let target = (case / 4) % packets.len();
            let mut mutant = packets[target].clone();
            let len = mutant.data.len();
            assert!(len > 0);
            if case % 4 == 0 { mutant.data.truncate(random(&mut seed) % len); }
            else {
                let flips = if case % 4 == 3 { 2 + random(&mut seed) % 7 } else { 1 };
                for _ in 0..flips {
                    let at = random(&mut seed) % if case % 4 == 2 { len.min(32) } else { len };
                    mutant.data[at] ^= 1 << (random(&mut seed) % 8);
                }
                if mutant.data == packets[target].data { mutant.data[0] ^= 1; }
            }
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let mut decoder = ctx.codecs.first_decoder(&stream.params).unwrap();
                for packet in &packets[..target] { let _ = decoder.send_packet(packet); drain(decoder.as_mut()); }
                let _ = decoder.send_packet(&mutant); drain(decoder.as_mut());
                if let Some(packet) = packets.get(target + 1) { let _ = decoder.send_packet(packet); drain(decoder.as_mut()); }
                decoder.reset().unwrap();
                let mut cue_index = 0;
                for packet in &packets {
                    decoder.send_packet(packet).unwrap();
                    loop {
                        match decoder.receive_frame() {
                            Ok(Frame::Video(frame)) => {
                                let expected = &reference.cues[cue_index];
                                assert_eq!(support::to_us(frame.pts.unwrap(), stream.time_base), expected.sub.start_us());
                                assert_eq!(frame.display_duration().map(|d| d.as_micros() as i64),
                                    expected.sub.end_us().map(|end| (end - expected.sub.start_us()).max(0)));
                                assert_eq!(frame.image_planes()[0].stride, reference.width * 4);
                                assert_eq!(frame.image_planes()[0].data, expected.canvas);
                                cue_index += 1;
                            }
                            Err(Error::NeedMore) => break,
                            other => panic!("reset recovery: {other:?}"),
                        }
                    }
                }
                assert_eq!(cue_index, reference.cues.len());
            }));
            assert!(result.is_ok(), "{sample} case={case} target={target}");
        }
    }
}
#[test]
fn paired_index_2000_mutations_are_bounded() {
    let _serial = SERIAL.lock();
    let index = std::fs::read(refcheck::fate("sub/vobsub.idx")).unwrap();
    let sub: Arc<[u8]> = std::fs::read(refcheck::fate("sub/vobsub.sub")).unwrap().into();
    let mut seed = 0x4944_585f_4d55_5441;
    for case in 0..2000 {
        let mut bytes = index.clone();
        if case % 2 == 0 { bytes.truncate(random(&mut seed) % bytes.len()); }
        else { let at = random(&mut seed) % bytes.len(); bytes[at] ^= 1 << (random(&mut seed) % 8); }
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            if let Ok(mut demux) = subs_bitmap::open_vobsub(Box::new(Cursor::new(bytes)), Box::new(Cursor::new(sub.clone()))) {
                assert!(demux.streams().len() <= 32);
                for _ in 0..1000 {
                    match demux.next_packet() { Ok(packet) => assert!(packet.data.len() <= 1 << 20), Err(_) => return }
                }
                panic!("mutated index yielded unbounded packets");
            }
        }));
        assert!(result.is_ok(), "index mutation {case}");
    }
}
