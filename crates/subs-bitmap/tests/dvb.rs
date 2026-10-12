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

/// CRC-32/MPEG-2 of a PSI section.
fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

/// `payload` on `pid` as 188-byte transport packets; the first starts the
/// unit. Bytes past a PES's declared length are stuffing, not data.
fn ts_unit(out: &mut Vec<u8>, pid: u16, continuity: &mut u8, payload: &[u8], section: bool) {
    let mut unit = Vec::new();
    if section {
        unit.push(0);
    }
    unit.extend_from_slice(payload);
    for (index, chunk) in unit.chunks(184).enumerate() {
        out.extend_from_slice(&[0x47, if index == 0 { 0x40 } else { 0 } | (pid >> 8) as u8, pid as u8, 0x10 | *continuity]);
        *continuity = (*continuity + 1) & 15;
        out.extend_from_slice(chunk);
        out.resize(out.len() + 184 - chunk.len(), 0xff);
    }
}

/// A transport stream with one DVB subtitle PID (0x101) whose subtitling
/// descriptor (0x59) holds `services`, and one PES per `(pts, segments)`
/// with its data identifier, stream id and end marker.
fn subtitle_ts(path: &Path, services: &[u8], units: &[(u64, Vec<u8>)]) {
    let mut out = Vec::new();
    let mut pat = vec![0x00, 0xb0, 0x0d, 0, 1, 0xc1, 0, 0, 0, 1, 0xe1, 0x00];
    pat.extend_from_slice(&crc32_mpeg2(&pat).to_be_bytes());
    let mut pmt = vec![0x02, 0xb0, 0, 0, 1, 0xc1, 0, 0, 0xe1, 0x01, 0xf0, 0, 0x06, 0xe1, 0x01, 0xf0, 2 + services.len() as u8, 0x59, services.len() as u8];
    pmt.extend_from_slice(services);
    pmt[2] = (pmt.len() - 3 + 4) as u8;
    pmt.extend_from_slice(&crc32_mpeg2(&pmt).to_be_bytes());
    let (mut pat_cc, mut pmt_cc, mut sub_cc) = (0, 0, 0);
    ts_unit(&mut out, 0, &mut pat_cc, &pat, true);
    ts_unit(&mut out, 0x100, &mut pmt_cc, &pmt, true);
    // Null packets: FFmpeg's probe wants more transport packets than a few
    // short subtitles fill.
    for _ in 0..32 {
        out.extend_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
        out.resize(out.len() + 184, 0xff);
    }
    for &(pts, ref segments) in units {
        let payload = [&[0x20u8, 0x00][..], segments.as_slice(), &[0xff]].concat();
        let mut pes = vec![0, 0, 1, 0xbd];
        pes.extend_from_slice(&((3 + 5 + payload.len()) as u16).to_be_bytes());
        pes.extend_from_slice(&[0x81, 0x80, 5]);
        pes.extend_from_slice(&[
            0x21 | ((pts >> 29) & 0x0e) as u8,
            (pts >> 22) as u8,
            ((pts >> 14) & 0xfe) as u8 | 1,
            (pts >> 7) as u8,
            ((pts << 1) & 0xfe) as u8 | 1,
        ]);
        pes.extend_from_slice(&payload);
        ts_unit(&mut out, 0x101, &mut sub_cc, &pes, false);
    }
    std::fs::write(path, out).unwrap();
}

/// A transport stream whose one subtitle PID carries two DVB services:
/// the original FATE pages (composition page 1, ancillary page 0x152,
/// declared first) and a second service on page 7 that re-composes the
/// same regions 16 pixels lower and to the right, in every packet. Only
/// the declared first service may reach the screen, as VLC's decoder
/// filters by the service's page ids (dvbsub.c); FFmpeg decodes every
/// page unless told a substream (dvbsubdec.c).
fn two_services_on_one_pid(source: &Path, path: &Path) {
    let (_, packets) = packets(source, "mpegts");
    let mut units = Vec::new();
    for packet in &packets {
        let data = &packet.data;
        let mut segments = Vec::new();
        let mut at = 2;
        while at + 6 <= data.len() && data[at] == 0x0f {
            let len = usize::from(u16::from_be_bytes([data[at + 4], data[at + 5]]));
            let segment = &data[at..at + 6 + len];
            segments.extend_from_slice(segment);
            if segment[1] == 0x10 && segment[2..4] == [0, 1] {
                // The same composition on page 7: a newer version, every
                // region moved by (16, 16).
                let mut page = segment.to_vec();
                page[2..4].copy_from_slice(&[0, 7]);
                page[7] = (page[7] & 0x0f) | (page[7].wrapping_add(0x80) & 0xf0);
                for region in page[8..].chunks_exact_mut(6) {
                    let x = u16::from_be_bytes([region[2], region[3]]) + 16;
                    let y = u16::from_be_bytes([region[4], region[5]]) + 16;
                    region[2..4].copy_from_slice(&x.to_be_bytes());
                    region[4..6].copy_from_slice(&y.to_be_bytes());
                }
                segments.extend_from_slice(&page);
            }
            at += 6 + len;
        }
        units.push((packet.pts.expect("subtitle PTS") as u64, segments));
    }
    assert!(units.iter().any(|(_, segments)| segments.windows(4).any(|w| w == [0x0f, 0x10, 0, 7])), "page-7 twins");
    let services = [b'e', b'n', b'g', 0x10, 0x00, 0x01, 0x01, 0x52, b'f', b'r', b'a', 0x10, 0x00, 0x07, 0x00, 0x07];
    subtitle_ts(path, &services, &units);
}

#[test]
fn dvb_pid_with_two_services_shows_only_the_declared_first() {
    let _serial = SERIAL.lock();
    let source = fate("sub/dvbsubtest_filter.ts");
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dvb-two-services-{}.ts", std::process::id()));
    two_services_on_one_pid(&source, &path);
    let (stream, _) = packets(&path, "mpegts");
    assert_eq!(stream.params.extradata, [0, 1, 0x01, 0x52, 0x10, 0, 7, 0, 7, 0x10]);
    let reference = ffmpeg_reference(&source, 0);
    let decoded = decode_subtitles(&path, REGISTRARS, 0);
    assert!(decoded.errors.is_empty(), "{:?}", decoded.errors);
    let want = reference_cues(&reference);
    let got = decoded_cues(&decoded.frames, decoded.stream.time_base, reference.width, reference.height);
    let diffs = cue_diffs(&want, &got, reference.width, Match::Exact);
    assert!(diffs.is_empty(), "page 7 reached the screen:\n{}", diffs.join("\n"));
    std::fs::remove_file(path).unwrap();
}

/// The FATE stream with its service's resources on the ancillary page: its
/// CLUT and object segments move from page 1 to the declared ancillary
/// page 0x152, and after every page-1 composition comes a newer one on
/// 0x152 that moves every region by (16, 16) and starts a new epoch (a
/// mode change, which drops regions and objects). VLC skips page
/// compositions on an ancillary page that differs from the composition
/// page and keeps its other segments (dvbsub.c), so the stream must decode
/// exactly as FFmpeg decodes the original.
fn ancillary_page_compositions(source: &Path, path: &Path) {
    let (_, packets) = packets(source, "mpegts");
    let mut units = Vec::new();
    let mut moved = 0;
    for packet in &packets {
        let data = &packet.data;
        let mut segments = Vec::new();
        let mut at = 2;
        while at + 6 <= data.len() && data[at] == 0x0f {
            let len = usize::from(u16::from_be_bytes([data[at + 4], data[at + 5]]));
            let mut segment = data[at..at + 6 + len].to_vec();
            if matches!(segment[1], 0x12 | 0x13) && segment[2..4] == [0, 1] {
                segment[2..4].copy_from_slice(&[0x01, 0x52]);
                moved += 1;
            }
            segments.extend_from_slice(&segment);
            if segment[1] == 0x10 && segment[2..4] == [0, 1] {
                let mut page = segment.clone();
                page[2..4].copy_from_slice(&[0x01, 0x52]);
                // A newer version, as a mode change.
                page[7] = (page[7].wrapping_add(0x80) & 0xf0) | (2 << 2) | (page[7] & 3);
                for region in page[8..].chunks_exact_mut(6) {
                    let x = u16::from_be_bytes([region[2], region[3]]) + 16;
                    let y = u16::from_be_bytes([region[4], region[5]]) + 16;
                    region[2..4].copy_from_slice(&x.to_be_bytes());
                    region[4..6].copy_from_slice(&y.to_be_bytes());
                }
                segments.extend_from_slice(&page);
            }
            at += 6 + len;
        }
        units.push((packet.pts.expect("subtitle PTS") as u64, segments));
    }
    assert!(moved > 0, "resources moved to the ancillary page");
    assert!(units.iter().any(|(_, segments)| segments.windows(4).any(|w| w == [0x0f, 0x10, 0x01, 0x52])), "ancillary compositions");
    subtitle_ts(path, &[b'e', b'n', b'g', 0x10, 0x00, 0x01, 0x01, 0x52], &units);
}

#[test]
fn dvb_ancillary_page_keeps_its_resources_but_not_its_compositions() {
    let _serial = SERIAL.lock();
    let source = fate("sub/dvbsubtest_filter.ts");
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dvb-ancillary-{}.ts", std::process::id()));
    ancillary_page_compositions(&source, &path);
    let (stream, _) = packets(&path, "mpegts");
    assert_eq!(stream.params.extradata, [0, 1, 0x01, 0x52, 0x10]);
    let reference = ffmpeg_reference(&source, 0);
    let decoded = decode_subtitles(&path, REGISTRARS, 0);
    assert!(decoded.errors.is_empty(), "{:?}", decoded.errors);
    let want = reference_cues(&reference);
    let got = decoded_cues(&decoded.frames, decoded.stream.time_base, reference.width, reference.height);
    let diffs = cue_diffs(&want, &got, reference.width, Match::Exact);
    assert!(diffs.is_empty(), "an ancillary page composition reached the screen:\n{}", diffs.join("\n"));
    std::fs::remove_file(path).unwrap();
}

/// One DVB subtitling segment of page 1.
fn page_segment(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![0x0f, kind, 0, 1];
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// Malformed input where FFmpeg's choices are cheap to share, against
/// FFmpeg itself: a display definition whose window is cut short still
/// records its version (the next one of that version is skipped,
/// dvbsubdec.c:1415-1428); a region 20000 pixels wide passes, as FFmpeg
/// checks only its area (1191-1199); and an object whose pixel data ends
/// in a cut-off map table still makes the computed CLUT recompute (961-986).
#[test]
fn dvb_malformed_segments_follow_ffmpeg() {
    let _serial = SERIAL.lock();
    let seg = page_segment;
    let end = || seg(0x80, &[]);
    // Region 0 (64x8, CLUT 0) at (16, 16) on a mode-change page, painted
    // with two rows of colour 2; then a display definition (720x576) whose
    // window is too short: FFmpeg fails the packet after recording it.
    let first = [
        seg(0x10, &[10, 0x08, 0, 0xff, 0x00, 0x10, 0x00, 0x10]),
        seg(0x11, &[0, 0x08, 0x00, 0x40, 0x00, 0x08, 0x08, 0, 0x00, 0x10, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]),
        seg(0x12, &[0, 0x00, 1, 0x41, 0x80, 0x80, 0x80, 0x00, 2, 0x41, 0xc0, 0x60, 0x60, 0x00]),
        seg(0x13, &[0x00, 0x01, 0x00, 0x00, 7, 0x00, 0x00, 0x11, 0x22, 0x22, 0x22, 0x22, 0x00, 0xf0]),
        seg(0x14, &[0x08, 0x02, 0xcf, 0x02, 0x3f]),
        end(),
    ];
    // The same definition version, now with a window at (100, 100).
    let definition = [
        seg(0x14, &[0x08, 0x02, 0xcf, 0x02, 0x3f, 0x00, 0x64, 0x02, 0x6b, 0x00, 0x64, 0x01, 0xdb]),
        seg(0x10, &[10, 0x10, 0, 0xff, 0x00, 0x10, 0x00, 0x10]),
        end(),
    ];
    // Region 3, 20000x10, joins the page.
    let wide = [
        seg(0x10, &[10, 0x20, 0, 0xff, 0x00, 0x10, 0x00, 0x10, 3, 0xff, 0x00, 0x00, 0x00, 0x40]),
        seg(0x11, &[3, 0x00, 0x4e, 0x20, 0x00, 0x0a, 0x08, 0, 0, 0]),
        end(),
    ];
    // A new epoch: region 1 (32x4) with CLUT 5, which never arrives, so its
    // palette is computed from colours 1 and 2.
    let computed = [
        seg(0x10, &[10, 0x38, 1, 0xff, 0x00, 0x00, 0x00, 0x40]),
        seg(0x11, &[1, 0x08, 0x00, 0x20, 0x00, 0x04, 0x08, 5, 0, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00]),
        seg(0x13, &[0x00, 0x02, 0x00, 0x00, 7, 0x00, 0x00, 0x11, 0x11, 0x22, 0x11, 0x22, 0x00, 0xf0]),
        end(),
    ];
    // Colour 3 painted over it, then a 2-to-4 map table cut off by the end.
    let recomputed = [
        seg(0x13, &[0x00, 0x02, 0x00, 0x00, 6, 0x00, 0x00, 0x11, 0x33, 0x33, 0x00, 0x20, 0x34]),
        seg(0x10, &[10, 0x40, 1, 0xff, 0x00, 0x00, 0x00, 0x40]),
        end(),
    ];
    // FFmpeg's DVB parser has no timestamp yet for a stream's first PES and
    // passes it on unparsed, which its decoder refuses; an empty first PES
    // keeps the cases off that path. Both decoders refuse the empty one.
    let units: Vec<(u64, Vec<u8>)> = [&[][..], &first[..], &definition[..], &wide[..], &computed[..], &recomputed[..]]
        .iter()
        .enumerate()
        .map(|(second, segments)| (90_000 * second as u64, segments.concat()))
        .collect();
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dvb-malformed-{}.ts", std::process::id()));
    subtitle_ts(&path, &[b'e', b'n', b'g', 0x10, 0x00, 0x01, 0x00, 0x01], &units);
    let reference = ffmpeg_reference(&path, 0);
    assert_eq!(reference.cues.len(), 4, "FFmpeg fails the empty packet and the cut-off window's");
    let decoded = decode_subtitles(&path, REGISTRARS, 0);
    let want = reference_cues(&reference);
    let got = decoded_cues(&decoded.frames, decoded.stream.time_base, reference.width, reference.height);
    let diffs = cue_diffs(&want, &got, reference.width, Match::Exact);
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
    std::fs::remove_file(path).unwrap();
}
