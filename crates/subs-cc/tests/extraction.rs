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
//! (Annex B).

mod support;

use support::{ffmpeg_captions, generated, hex, our_captions, rollup, same_time, scte20};

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

#[test]
fn h264_sei_in_matroska_and_ts() {
    assert_same_as_ffmpeg(&generated("h264.mkv"), "H.264 (B-frames) in Matroska");
    assert_same_as_ffmpeg(&generated("h264.ts"), "H.264 (B-frames) in TS");
}

#[test]
fn hevc_sei_in_matroska_and_ts() {
    assert_same_as_ffmpeg(&generated("hevc.mkv"), "HEVC in Matroska");
    assert_same_as_ffmpeg(&generated("hevc.ts"), "HEVC in TS");
}
