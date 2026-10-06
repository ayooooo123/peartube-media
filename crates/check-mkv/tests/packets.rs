//! Packet equality with FFmpeg: for every FATE Matroska / WebM sample and
//! every Matroska / WebM file of the generated corpus, our packets — stream,
//! pts, dts, size, keyframe flag and data MD5 — are compared with
//! `ffprobe -show_packets -show_data_hash md5`.
//!
//! Every sample has one row in `EXPECTED`: the MD5 of our whole packet list
//! and the differences from `ffprobe`, per stream and field ("" = every
//! packet equal on every field). A row changes only when the fork's output
//! does; a difference is never re-pinned to a value FFmpeg doesn't produce.
//! The differences still listed, all in packet assembly inherited from
//! upstream oxideav-mkv c0966a6:
//!
//! * `key`: every `SimpleBlock` packet is flagged a keyframe, whatever the
//!   Block's keyframe bit (FFmpeg follows the bit; TrueHD's are set by its
//!   parser).
//! * `dts`: always equal to pts; FFmpeg derives dts for reordered video.
//! * `pts`: FFmpeg subtracts a track's `CodecDelay` (Opus, AAC, AC-3,
//!   E-AC-3), spreads the frames of a laced Block over its duration, applies
//!   `TrackTimestampScale` (`tts10.mkv`) and drops negative Block
//!   timestamps (`coeff_level64.mkv`).
//! * `size` / `md5`: FFmpeg rebuilds WavPack block headers, restores
//!   ProRes frame headers and moves WebVTT cue settings to side data.
//! * `error` / `count`: a truncated sample ends the strict demux with an
//!   error where FFmpeg returns the Blocks that fit; `zero_length_block.mks`
//!   doesn't open.
//!
//! Fixed by the fork so far: `ContentEncodings` compression (zlib, bzip2,
//! LZO1X) is undone.
//!
//! `CHECK_MKV_RECORD=1` prints the rows instead of asserting them.

use std::collections::BTreeSet;
use std::path::PathBuf;

use check_mkv::{Pkt, corpus_samples, ffprobe_packets, our_packets, FATE_SAMPLES};

/// `(sample, MD5 of our packet list, differences from ffprobe)`. Samples
/// are `fate:<path>` (FATE suite) or `gen:<file>` (generated corpus).
const EXPECTED: &[(&str, &str, &str)] = &[
    ("fate:audiomatch/tones_opus_48000_stereo.mka", "216e9abcbadc3727322ddf46a419f7f7", "s0: pts 101/101, dts 101/101"),
    ("fate:filter/242_4.mkv", "3cfc5a1b98bdb5797a19555169d0fd36", "s0: dts 156/207, key 205/207"),
    ("fate:filter/anim.mkv", "441606bf5f2ba654cd807310e45cddea", "s0: key 69/71"),
    ("fate:h264-high-depth/high-qp.mkv", "0ca405b1f9896fdeeebd3ea84534d5c1", "s0: dts 4/5, key 4/5"),
    ("fate:h264/H264_might_overflow.mkv", "de6b5e3247775b3b09190657111ad6f1", "s0: dts 5/5, key 4/5"),
    ("fate:h264/direct-bff.mkv", "fb9c049d144b89176fadf7e2a549edd8", "error: I/O error: EBML: short read (40783 of 72756 bytes); s0: dts 7/11, key 10/11"),
    ("fate:h264/dts_5frames.mkv", "9ab5a4245f0a62f75305414016f4b525", "s0: key 4/5"),
    ("fate:lcevc/L_AV1_854x480p_8bit8bit_2D_dd.mkv", "e85e69f88d9f26c37d412ed0b47317df", ""),
    ("fate:mkv/1242-small.mkv", "29588a8acb94cdf8c12f1fdae23117e1", "error: I/O error: EBML: short read (8172 of 10371 bytes); s0: pts 19/24, dts 19/24; s1: dts 9/12, key 9/12"),
    ("fate:mkv/codec_delay_opus.mkv", "069fb8a5a4442f91ed694c1eae72e0e2", "s0: pts 52/52, dts 52/52"),
    ("fate:mkv/dovi-p7-hvce.mkv", "20159b16147829d750641a51c565cec5", "s0: dts 1/1"),
    ("fate:mkv/flac_channel_layouts.mka", "dd94daf7eec1680477e4b4860f21826d", "s0: pts 9/12, dts 9/12; s1: pts 9/12, dts 9/12"),
    ("fate:mkv/h264_tta_undecodable.mkv", "50c45e3cf82d3d4c515bee1c1b34d07e", ""),
    ("fate:mkv/hdr10_plus_vp9_sample.webm", "9b688956626b0462d60a6bef82657fa0", ""),
    ("fate:mkv/hdr10tags-both.mkv", "ceabb755a3c50dddcef12f4eaa5cddd4", "s0: dts 7/10, key 9/10"),
    ("fate:mkv/lzo.mka", "d9853eb0b6cefb27b5f3dcb5b731c1d9", "s0: pts 3/4, dts 3/4"),
    ("fate:mkv/prores_bz2.mkv", "7939534cb32cb9689cec641efc97a16e", "s0: size 2/2, md5 2/2; s1: size 2/2, md5 2/2"),
    ("fate:mkv/prores_zlib.mkv", "83bed746f96749e5f529b9dfe99bfc27", ""),
    ("fate:mkv/spherical.mkv", "bf372a12d060c6acd0842d83f8aadb75", "s0: dts 91/120, key 119/120"),
    ("fate:mkv/subtitle_zlib.mks", "22c21b4fc1438305ee004859b4ef14f0", ""),
    ("fate:mkv/test7_cut.mkv", "59a1988a3deb05e21790768d2e49a95f", "error: I/O error: failed to fill whole buffer; s0: count 24/72, dts 13/24, key 23/24; s1: count 48/143, pts 41/48, dts 41/48"),
    ("fate:mkv/tts10.mkv", "bd610379cac4715f9b94c46229bb5c5a", "s0: pts 2/5, dts 2/5"),
    ("fate:mkv/wavpack_missing_codecprivate.mka", "9d041a3294ae1117849c718ba9eae665", "s0: pts 1/2, dts 1/2, size 2/2, md5 2/2"),
    ("fate:mkv/xiph_lacing.mka", "21e599087b1a6520f7d5a59c05df81fd", "s0: pts 72/84, dts 72/84"),
    ("fate:mkv/zero_length_block.mks", "b25dbbdf3c2d9065ab72d092c162437d", "error: open: invalid data: MKV: no tracks found; s0: count 0/2"),
    ("fate:opus/silk-lbrr-mono.mka", "307e28a2b4172e25f06c8129a1e12281", "s0: pts 46/46, dts 46/46"),
    ("fate:opus/silk-lbrr.mka", "c3ebe61c0130359f044f078ed33fe916", ""),
    ("fate:opus/testvector01.mka", "ffe4254e6656c498f2f1639c305176ff", ""),
    ("fate:opus/testvector02.mka", "18de2e0e4df5950646a43034826ca5ee", ""),
    ("fate:opus/testvector03.mka", "6c9d53ba6e2faf891c5d830b38196653", ""),
    ("fate:opus/testvector04.mka", "d2e2b19fcf38ab3f1d087d14f6f37f56", ""),
    ("fate:opus/testvector05.mka", "2576a1fa581d6941a2a714c2be700a15", ""),
    ("fate:opus/testvector06.mka", "db9d3107c41536012a996a83981f6759", ""),
    ("fate:opus/testvector07.mka", "180ffe4c92e1488550f8cc07f0f794ba", ""),
    ("fate:opus/testvector08.mka", "cfb7419019470a1b8a1a6eee750380c5", ""),
    ("fate:opus/testvector09.mka", "52dd0f84152a680cc38d4800d1dde882", ""),
    ("fate:opus/testvector10.mka", "9fea45c10780ca82ba8e154a55bae2be", ""),
    ("fate:opus/testvector11.mka", "0dd5f9308d4ab3123a9daf3c269b03fd", ""),
    ("fate:opus/testvector12.mka", "5e652b60e6c18586c7a123499c36f3dc", ""),
    ("fate:opus/tron.6ch.tinypkts.mka", "87f9969dd823c8c8251a86f84f5f66ab", ""),
    ("fate:vp3/coeff_level64.mkv", "e1d293613980bc1e1607217b247fe07c", "s0: pts 8/8, dts 8/8, key 7/8"),
    ("fate:vp8/RRSF49-short.webm", "63c5612afd028e71f2e9b266f8ba4a28", "error: I/O error: EBML: short read (5068 of 39025 bytes); s0: key 110/111"),
    ("fate:vp8/dash_audio1.webm", "e86ab4a217f78e4588b80efa5990a249", ""),
    ("fate:vp8/dash_audio2.webm", "e86ab4a217f78e4588b80efa5990a249", ""),
    ("fate:vp8/dash_audio3.webm", "e86ab4a217f78e4588b80efa5990a249", ""),
    ("fate:vp8/dash_video1.webm", "3d79284b65f3fa4ff564e3bfaa637dcb", "s0: key 806/812"),
    ("fate:vp8/dash_video2.webm", "3d79284b65f3fa4ff564e3bfaa637dcb", "s0: key 806/812"),
    ("fate:vp8/dash_video3.webm", "3d79284b65f3fa4ff564e3bfaa637dcb", "s0: key 806/812"),
    ("fate:vp8/dash_video4.webm", "83fe9134854f57a53dae6e9f4aa65232", "s0: key 785/812"),
    ("fate:vp8/frame_size_change.webm", "6d301109508e6b601730d5cefb5c4f74", "s0: key 200/300"),
    ("fate:vp8_alpha/vp8_video_with_alpha.webm", "5e2df636a13041edebab19c508e09e26", "s0: key 119/120"),
    ("fate:vp9-test-vectors/vp90-2-2pass-akiyo.webm", "d87bd07c3bd1de527b088ac10714baed", "s0: key 49/50"),
    ("fate:vp9-test-vectors/vp90-2-segmentation-aq-akiyo.webm", "ccf4d356d6a3a31b5a84fc7a975c2458", "s0: key 24/25"),
    ("fate:vp9-test-vectors/vp90-2-segmentation-sf-akiyo.webm", "2f1b43a51834c90f5581f78542ea7601", "s0: key 24/25"),
    ("fate:vp9-test-vectors/vp93-2-20-12bit-yuv422.webm", "15d67b4904374106ff0d292ebfc60614", "s0: key 9/10"),
    ("fate:wavpack/special/matroska_mode.mka", "ffde2e434fece23a3b5a9c62496f73a8", "s0: pts 14/22, dts 14/22, size 22/22, md5 22/22"),
    ("gen:av1_opus.mkv", "6d77990ceac0b1144647bf367bbc63d9", "s0: key 149/150; s1: pts 301/301, dts 301/301"),
    ("gen:h264_aac.mkv", "c2361518122a672e3e346e5bab5dd825", "s0: key 149/150; s1: pts 283/283, dts 283/283"),
    ("gen:h264_aac_ass.mkv", "1e530e4088ca123b9e061fb43f31ba46", "s0: key 149/150; s1: pts 283/283, dts 283/283"),
    ("gen:h264_aac_pgs.mkv", "4e21e93a4a355cdbf269546211ce1b77", "s0: key 149/150"),
    ("gen:h264_aac_srt.mkv", "c204facb2a77f01fd8d1949a9fe4370c", "s0: key 149/150; s1: pts 283/283, dts 283/283"),
    ("gen:h264_ac3.mkv", "28eef6c0adadbaa2fd9779f6ec3f3b72", "s0: key 149/150; s1: pts 188/188, dts 188/188"),
    ("gen:h264_dts.mkv", "94d6052e12f61b7234ce880df5c40788", "s0: key 149/150"),
    ("gen:h264_eac3.mkv", "e608d12fbfa4f9c2b233beea69d36537", "s0: key 149/150; s1: pts 188/188, dts 188/188"),
    ("gen:h264_truehd.mkv", "9a7d4d0e0f41887ac840b7ae9ab87ead", "s0: key 149/150; s1: key 6750/7200"),
    ("gen:hevc10_eac3.mkv", "f80437b2268a16336d1cdd9f8f0f19ff", "s0: dts 113/150, key 149/150; s1: pts 188/188, dts 188/188"),
    ("gen:video_vp8.webm", "129dbe713882bbcfe67d5361b882fe6c", "s0: key 148/150; s1: pts 283/283, dts 283/283"),
    ("gen:vp9_opus.webm", "3b1b9b9c8af6e205de4a7c5ef9b75b0e", "s0: key 148/150; s1: pts 301/301, dts 301/301"),
    ("gen:vp9_opus_vtt.webm", "befc1dadcdfff062d830e98b8ab06127", "s0: key 148/150; s1: pts 301/301, dts 301/301; s2: size 3/3, md5 3/3"),
];

fn samples() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> =
        FATE_SAMPLES.iter().map(|rel| (format!("fate:{rel}"), refcheck::fate(rel))).collect();
    for path in corpus_samples() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        out.push((format!("gen:{name}"), path));
    }
    out
}

/// MD5 of the packet list (and the error that ended the demux, if any).
fn digest(packets: &[Pkt], error: &Option<String>) -> String {
    let mut text = String::new();
    for p in packets {
        text.push_str(&format!(
            "{} {:?} {:?} {} {} {}\n",
            p.stream, p.pts, p.dts, p.size, p.keyframe, p.md5
        ));
    }
    if let Some(e) = error {
        text.push_str(&format!("error {e}\n"));
    }
    refcheck::md5_hex(text.as_bytes())
}

/// The differences between our packets and ffprobe's, per stream: packet
/// counts, then the number of packets (in stream order) whose field
/// differs; "order" when only the interleaving of the streams differs.
fn differences(ours: &[Pkt], theirs: &[Pkt], error: &Option<String>) -> String {
    let mut parts = Vec::new();
    if let Some(e) = error {
        parts.push(format!("error: {e}"));
    }
    let streams: BTreeSet<u32> = ours.iter().chain(theirs).map(|p| p.stream).collect();
    for s in streams {
        let a: Vec<&Pkt> = ours.iter().filter(|p| p.stream == s).collect();
        let b: Vec<&Pkt> = theirs.iter().filter(|p| p.stream == s).collect();
        let mut diffs = Vec::new();
        if a.len() != b.len() {
            diffs.push(format!("count {}/{}", a.len(), b.len()));
        }
        let n = a.len().min(b.len());
        let fields: [(&str, fn(&Pkt, &Pkt) -> bool); 5] = [
            ("pts", |x, y| x.pts == y.pts),
            ("dts", |x, y| x.dts == y.dts),
            ("size", |x, y| x.size == y.size),
            ("key", |x, y| x.keyframe == y.keyframe),
            ("md5", |x, y| x.md5 == y.md5),
        ];
        for (name, same) in fields {
            let k = (0..n).filter(|&i| !same(a[i], b[i])).count();
            if k > 0 {
                diffs.push(format!("{name} {k}/{n}"));
            }
        }
        if !diffs.is_empty() {
            parts.push(format!("s{s}: {}", diffs.join(", ")));
        }
    }
    if parts.is_empty() && ours.iter().map(|p| p.stream).ne(theirs.iter().map(|p| p.stream)) {
        parts.push("order".into());
    }
    parts.join("; ")
}

#[test]
fn packets_equal_ffprobe() {
    let record = std::env::var_os("CHECK_MKV_RECORD").is_some();
    let samples = samples();
    let mut failures = Vec::new();
    let (mut exact, mut known) = (0, 0);
    for (name, path) in &samples {
        let ours = our_packets(path);
        let theirs = ffprobe_packets(path, &[]);
        let got_digest = digest(&ours.packets, &ours.error);
        let got_diff = differences(&ours.packets, &theirs, &ours.error);
        if record {
            println!("    (\"{name}\", \"{got_digest}\", \"{got_diff}\"),");
            continue;
        }
        let Some(&(_, digest, diff)) = EXPECTED.iter().find(|e| e.0 == name) else {
            failures.push(format!("{name}: no EXPECTED row (digest {got_digest}, differences \"{got_diff}\")"));
            continue;
        };
        if got_digest != digest {
            failures.push(format!("{name}: packets changed (digest {got_digest}, want {digest})"));
        }
        if got_diff != diff {
            failures.push(format!("{name}: differences from ffprobe \"{got_diff}\", want \"{diff}\""));
        }
        if diff.is_empty() {
            exact += 1;
        } else {
            known += 1;
        }
    }
    if record {
        return;
    }
    for (name, _, _) in EXPECTED {
        if !samples.iter().any(|s| s.0 == *name) {
            failures.push(format!("{name}: EXPECTED row without a sample"));
        }
    }
    println!(
        "{} samples: {exact} equal ffprobe on every packet, {known} with differences inherited from upstream",
        samples.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
