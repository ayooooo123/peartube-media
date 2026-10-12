//! A/53 caption data taken from video packets equals what FFmpeg exports
//! as `AV_FRAME_DATA_A53_CC` (`-a53cc 1`), read back through its lavfi
//! `movie=…[out0+subcc]` source: picture by picture, every triplet, at
//! the same presentation time.
//!
//! Inputs: the two FATE caption samples FFmpeg's own tests use
//! (`sub/Closedcaption_rollup.m2v`, raw MPEG-2 with A/53 Part 4 user data
//! and an open first GOP; `sub/scte20.ts`, SCTE-20 user data in TS), and
//! the rollup sample re-encoded by libx264 (B-frames) and libx265, its
//! captions carried in SEI, in Matroska (length-prefixed NAL units) and TS
//! (Annex B). For H.264 and HEVC, `extract_a53` on each packet alone,
//! without the extradata, takes what the stream's extractor takes.

mod support;

use subs_cc::{extract_a53, CcExtractor};
use support::{ffmpeg_captions, generated, hex, our_captions, rollup, same_time, scte20, video_packets};

fn assert_same_as_ffmpeg(path: &std::path::Path, what: &str) {
    let theirs = ffmpeg_captions(path);
    let ours = our_captions(path);
    let triplets = |c: &support::Captions| c.pictures.iter().map(|(_, t)| t.len()).sum::<usize>();
    println!(
        "{what}: FFmpeg {} pictures / {} triplets, ours {} / {}",
        theirs.pictures.len(),
        triplets(&theirs),
        ours.pictures.len(),
        triplets(&ours)
    );
    assert!(!theirs.pictures.is_empty(), "{what}: FFmpeg found no captions");
    for (i, (a, b)) in ours.pictures.iter().zip(&theirs.pictures).enumerate() {
        assert!(
            same_time(a.0, ours.time_base, b.0, theirs.time_base) && a.1 == b.1,
            "{what}: picture {i} differs:\n  ours   {} @ {:?}/{:?}\n  FFmpeg {} @ {:?}/{:?}",
            hex(&a.1),
            a.0,
            ours.time_base,
            hex(&b.1),
            b.0,
            theirs.time_base,
        );
    }
    assert_eq!(ours.pictures.len(), theirs.pictures.len(), "{what}: picture count");
}

#[test]
fn rollup_mpeg2_user_data() {
    assert_same_as_ffmpeg(&rollup(), "Closedcaption_rollup.m2v");
}

#[test]
fn scte20_user_data_in_ts() {
    assert_same_as_ffmpeg(&scte20(), "scte20.ts");
}

/// `extract_a53` on each packet of `path` (H.264/HEVC: every access unit
/// stands alone) equals the stream extractor's take from it.
fn assert_alone_as_in_stream(path: &std::path::Path, what: &str) {
    let (stream, packets) = video_packets(path);
    let codec = stream.params.codec_id.as_str();
    let mut extractor = CcExtractor::new(codec, &stream.params.extradata).unwrap();
    let mut found = 0;
    for (i, packet) in packets.iter().enumerate() {
        let in_stream = extractor.extract(&packet.data);
        assert_eq!(extract_a53(codec, &packet.data), in_stream, "{what}: packet {i} alone");
        found += in_stream.len();
    }
    assert!(found > 0, "{what}: no captions");
}

#[test]
fn h264_sei_in_matroska_and_ts() {
    for (name, what) in [("h264.mkv", "H.264 (B-frames) in Matroska"), ("h264.ts", "H.264 (B-frames) in TS")] {
        assert_same_as_ffmpeg(&generated(name), what);
        assert_alone_as_in_stream(&generated(name), what);
    }
}

#[test]
fn hevc_sei_in_matroska_and_ts() {
    for (name, what) in [("hevc.mkv", "HEVC in Matroska"), ("hevc.ts", "HEVC in TS")] {
        assert_same_as_ffmpeg(&generated(name), what);
        assert_alone_as_in_stream(&generated(name), what);
    }
}
