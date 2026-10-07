//! Packet equality against FFmpeg 9: every packet's stream, PTS, DTS,
//! size, keyframe flag and data MD5 must match `ffprobe -show_packets
//! -show_data_hash md5`. A sample is exact only when every field matches.
//!
//! No known-wrong output is pinned or exempted. Outstanding CodecDelay
//! differences are an external AudioTrim dependency and keep this test red.
//! The per-sample diagnostics distinguish that dependency from regressions
//! in packet counts, payload reconstruction, lacing and video timestamps.

use std::path::PathBuf;

use check_mkv::{corpus_samples, differences, ffprobe_packets, our_packets, FATE_SAMPLES};

/// Coverage inventory, not expected packet values. Missing samples fail too.
const EXPECTED_SAMPLES: &[&str] = &[
    "fate:audiomatch/tones_opus_48000_stereo.mka",
    "fate:filter/242_4.mkv",
    "fate:filter/anim.mkv",
    "fate:h264-high-depth/high-qp.mkv",
    "fate:h264/H264_might_overflow.mkv",
    "fate:h264/direct-bff.mkv",
    "fate:h264/dts_5frames.mkv",
    "fate:lcevc/L_AV1_854x480p_8bit8bit_2D_dd.mkv",
    "fate:mkv/1242-small.mkv",
    "fate:mkv/codec_delay_opus.mkv",
    "fate:mkv/dovi-p7-hvce.mkv",
    "fate:mkv/flac_channel_layouts.mka",
    "fate:mkv/h264_tta_undecodable.mkv",
    "fate:mkv/hdr10_plus_vp9_sample.webm",
    "fate:mkv/hdr10tags-both.mkv",
    "fate:mkv/lzo.mka",
    "fate:mkv/prores_bz2.mkv",
    "fate:mkv/prores_zlib.mkv",
    "fate:mkv/spherical.mkv",
    "fate:mkv/subtitle_zlib.mks",
    "fate:mkv/test7_cut.mkv",
    "fate:mkv/tts10.mkv",
    "fate:mkv/wavpack_missing_codecprivate.mka",
    "fate:mkv/xiph_lacing.mka",
    "fate:mkv/zero_length_block.mks",
    "fate:opus/silk-lbrr-mono.mka",
    "fate:opus/silk-lbrr.mka",
    "fate:opus/testvector01.mka",
    "fate:opus/testvector02.mka",
    "fate:opus/testvector03.mka",
    "fate:opus/testvector04.mka",
    "fate:opus/testvector05.mka",
    "fate:opus/testvector06.mka",
    "fate:opus/testvector07.mka",
    "fate:opus/testvector08.mka",
    "fate:opus/testvector09.mka",
    "fate:opus/testvector10.mka",
    "fate:opus/testvector11.mka",
    "fate:opus/testvector12.mka",
    "fate:opus/tron.6ch.tinypkts.mka",
    "fate:vp3/coeff_level64.mkv",
    "fate:vp8/RRSF49-short.webm",
    "fate:vp8/dash_audio1.webm",
    "fate:vp8/dash_audio2.webm",
    "fate:vp8/dash_audio3.webm",
    "fate:vp8/dash_video1.webm",
    "fate:vp8/dash_video2.webm",
    "fate:vp8/dash_video3.webm",
    "fate:vp8/dash_video4.webm",
    "fate:vp8/frame_size_change.webm",
    "fate:vp8_alpha/vp8_video_with_alpha.webm",
    "fate:vp9-test-vectors/vp90-2-2pass-akiyo.webm",
    "fate:vp9-test-vectors/vp90-2-segmentation-aq-akiyo.webm",
    "fate:vp9-test-vectors/vp90-2-segmentation-sf-akiyo.webm",
    "fate:vp9-test-vectors/vp93-2-20-12bit-yuv422.webm",
    "fate:wavpack/special/matroska_mode.mka",
    "gen:av1_opus.mkv",
    "gen:h264_aac.mkv",
    "gen:h264_aac_ass.mkv",
    "gen:h264_aac_pgs.mkv",
    "gen:h264_aac_srt.mkv",
    "gen:h264_ac3.mkv",
    "gen:h264_dts.mkv",
    "gen:h264_eac3.mkv",
    "gen:h264_truehd.mkv",
    "gen:hevc10_eac3.mkv",
    "gen:video_vp8.webm",
    "gen:vp9_opus.webm",
    "gen:vp9_opus_vtt.webm",
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

#[test]
fn packets_equal_ffprobe() {
    let samples = samples();
    let mut failures = Vec::new();
    let mut exact = 0;
    for (name, path) in &samples {
        let ours = our_packets(path);
        let theirs = ffprobe_packets(path, &[]);
        let diff = differences(&ours.packets, &theirs, &ours.error);
        if diff.is_empty() {
            exact += 1;
            println!("{name}: EXACT");
        } else {
            println!("{name}: {diff}");
            failures.push(format!("{name}: {diff}"));
        }
        if !EXPECTED_SAMPLES.contains(&name.as_str()) {
            failures.push(format!("{name}: missing coverage inventory entry"));
        }
    }
    for name in EXPECTED_SAMPLES {
        if !samples.iter().any(|s| s.0 == *name) {
            failures.push(format!("{name}: inventory entry without a sample"));
        }
    }
    println!("{} samples: {exact} exact on every packet field, {} non-exact",
        samples.len(), samples.len() - exact);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
