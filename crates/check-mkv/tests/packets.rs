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
//! LZO1X) is undone, and keyframe flags follow FFmpeg's rule (the Block's
//! signal read through the codec's parser; intra-only codecs and subtitles
//! always keyframes).
//!
//! `CHECK_MKV_RECORD=1` prints the rows instead of asserting them.

use std::collections::BTreeSet;
use std::path::PathBuf;

use check_mkv::{Pkt, corpus_samples, ffprobe_packets, our_packets, FATE_SAMPLES};

/// `(sample, MD5 of our packet list, differences from ffprobe)`. Samples
/// are `fate:<path>` (FATE suite) or `gen:<file>` (generated corpus).
const EXPECTED: &[(&str, &str, &str)] = &[
    ("fate:audiomatch/tones_opus_48000_stereo.mka", "216e9abcbadc3727322ddf46a419f7f7", "s0: pts 101/101, dts 101/101"),
    ("fate:filter/242_4.mkv", "fe9870c78a57567b3481324d3473ce14", "s0: dts 156/207"),
    ("fate:filter/anim.mkv", "eca1c83f2ba1860d623ba712260ffd98", ""),
    ("fate:h264-high-depth/high-qp.mkv", "eda9bf16dd7b2e2ba27f6763af108607", "s0: dts 4/5"),
    ("fate:h264/H264_might_overflow.mkv", "1a5fe0ebc5bacb77531cf92f43974078", "s0: dts 5/5"),
    ("fate:h264/direct-bff.mkv", "62fcb3d28c660abce0dad88bb52b168a", "error: I/O error: EBML: short read (40783 of 72756 bytes); s0: dts 7/11"),
    ("fate:h264/dts_5frames.mkv", "3f65f4cc6406cd411df5c86081fad640", ""),
    ("fate:lcevc/L_AV1_854x480p_8bit8bit_2D_dd.mkv", "e85e69f88d9f26c37d412ed0b47317df", ""),
    ("fate:mkv/1242-small.mkv", "39b2bc46f73c7af5ff8a16429932d25f", "error: I/O error: EBML: short read (8172 of 10371 bytes); s0: pts 19/24, dts 19/24; s1: dts 9/12"),
    ("fate:mkv/codec_delay_opus.mkv", "069fb8a5a4442f91ed694c1eae72e0e2", "s0: pts 52/52, dts 52/52"),
    ("fate:mkv/dovi-p7-hvce.mkv", "20159b16147829d750641a51c565cec5", "s0: dts 1/1"),
    ("fate:mkv/flac_channel_layouts.mka", "dd94daf7eec1680477e4b4860f21826d", "s0: pts 9/12, dts 9/12; s1: pts 9/12, dts 9/12"),
    ("fate:mkv/h264_tta_undecodable.mkv", "50c45e3cf82d3d4c515bee1c1b34d07e", ""),
    ("fate:mkv/hdr10_plus_vp9_sample.webm", "9b688956626b0462d60a6bef82657fa0", ""),
    ("fate:mkv/hdr10tags-both.mkv", "95b1d18f160775dd999dc592bda59b0a", "s0: dts 7/10"),
    ("fate:mkv/lzo.mka", "d9853eb0b6cefb27b5f3dcb5b731c1d9", "s0: pts 3/4, dts 3/4"),
    ("fate:mkv/prores_bz2.mkv", "7939534cb32cb9689cec641efc97a16e", "s0: size 2/2, md5 2/2; s1: size 2/2, md5 2/2"),
    ("fate:mkv/prores_zlib.mkv", "83bed746f96749e5f529b9dfe99bfc27", ""),
    ("fate:mkv/spherical.mkv", "4cfc2b30079dbf1213efb236620bb3d1", "s0: dts 91/120"),
    ("fate:mkv/subtitle_zlib.mks", "22c21b4fc1438305ee004859b4ef14f0", ""),
    ("fate:mkv/test7_cut.mkv", "49c2e6a8a909cbfbbac3fddf1f152b75", "error: I/O error: failed to fill whole buffer; s0: count 24/72, dts 13/24; s1: count 48/143, pts 41/48, dts 41/48"),
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
    ("fate:vp3/coeff_level64.mkv", "09dff1d3741c3eacb9bf4253e7a79f72", "s0: pts 8/8, dts 8/8"),
    ("fate:vp8/RRSF49-short.webm", "a34a96f78dab2ca6bd7415120c1d18aa", "error: I/O error: EBML: short read (5068 of 39025 bytes)"),
    ("fate:vp8/dash_audio1.webm", "e86ab4a217f78e4588b80efa5990a249", ""),
    ("fate:vp8/dash_audio2.webm", "e86ab4a217f78e4588b80efa5990a249", ""),
    ("fate:vp8/dash_audio3.webm", "e86ab4a217f78e4588b80efa5990a249", ""),
    ("fate:vp8/dash_video1.webm", "15b53f6eeee222067ebefa2d8320b8fd", ""),
    ("fate:vp8/dash_video2.webm", "15b53f6eeee222067ebefa2d8320b8fd", ""),
    ("fate:vp8/dash_video3.webm", "15b53f6eeee222067ebefa2d8320b8fd", ""),
    ("fate:vp8/dash_video4.webm", "93bb333fb0549da36263f7fe47747bec", ""),
    ("fate:vp8/frame_size_change.webm", "bd72121bf420d0203f1e33d1eddf54d6", ""),
    ("fate:vp8_alpha/vp8_video_with_alpha.webm", "a1fc2c50567995d6be270892c689bf38", ""),
    ("fate:vp9-test-vectors/vp90-2-2pass-akiyo.webm", "8d54ea905bf2c415326ff99b6d75f32d", ""),
    ("fate:vp9-test-vectors/vp90-2-segmentation-aq-akiyo.webm", "3497fac02fbc0fb0159d7d095d2506e9", ""),
    ("fate:vp9-test-vectors/vp90-2-segmentation-sf-akiyo.webm", "b27bb3bece758f2f5a18ef39957b68c5", ""),
    ("fate:vp9-test-vectors/vp93-2-20-12bit-yuv422.webm", "337e1614dbdc0c57d045b5669e84b77c", ""),
    ("fate:wavpack/special/matroska_mode.mka", "ffde2e434fece23a3b5a9c62496f73a8", "s0: pts 14/22, dts 14/22, size 22/22, md5 22/22"),
    ("gen:av1_opus.mkv", "d1da5a6acbc0092f7d64087e5daac958", "s1: pts 301/301, dts 301/301"),
    ("gen:h264_aac.mkv", "d9f4c0fbfe0af5d349e64a7c8f8a770c", "s1: pts 283/283, dts 283/283"),
    ("gen:h264_aac_ass.mkv", "85f2616b489d5a56ea6bb2ab91a43ede", "s1: pts 283/283, dts 283/283"),
    ("gen:h264_aac_pgs.mkv", "fef76976a1a7558ec665abf9fff9689b", ""),
    ("gen:h264_aac_srt.mkv", "6ea833600905c10acd4cd4fbed7e49ef", "s1: pts 283/283, dts 283/283"),
    ("gen:h264_ac3.mkv", "5c0ff6f5b8f8b14c285bbb171ebf5f5a", "s1: pts 188/188, dts 188/188"),
    ("gen:h264_dts.mkv", "9404097b274a83f5da2385ac2e8c3c0b", ""),
    ("gen:h264_eac3.mkv", "4dcb9abbd5e83c175ce2b6f370b1dc45", "s1: pts 188/188, dts 188/188"),
    ("gen:h264_truehd.mkv", "0ccf0d52859df8b0bb3fa946707b1226", ""),
    ("gen:hevc10_eac3.mkv", "c5372d4047f21cee05e16c5661416e62", "s0: dts 113/150; s1: pts 188/188, dts 188/188"),
    ("gen:video_vp8.webm", "865477ebc559a6f59af86f9f401947ea", "s1: pts 283/283, dts 283/283"),
    ("gen:vp9_opus.webm", "2cb8034dbdfebd2b7cd2242e309d4634", "s1: pts 301/301, dts 301/301"),
    ("gen:vp9_opus_vtt.webm", "d16ec06e04f5760d1140262bc51b4071", "s1: pts 301/301, dts 301/301; s2: size 3/3, md5 3/3"),
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
