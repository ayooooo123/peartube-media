//! The forked `oxideav-h263` decoder against FFmpeg 2da55bf, frame for
//! frame (framemd5 through its C IDCT, `-idct simple`), through the
//! player's registry:
//!
//! - FATE's H.263 tests, `fate-{vsynth1,vsynth2,vsynth_lena}-{h263,
//!   h263-obmc,h263p}` (vcodec.mak: baseline, advanced prediction, and
//!   H.263+ with advanced intra coding, unrestricted vectors, the
//!   alternative INTER VLC and GOB headers). FATE encodes these itself;
//!   the streams are regenerated with its exact command and checked
//!   against its stream MD5, and each decode also equals the MD5 FATE
//!   records for it. Two are also read as raw H.263, split into pieces.
//! - Intel H.263 (`I263`), which FATE does not test: both files of
//!   FFmpeg's sample archive (loop filter and custom size, one with long
//!   vectors; Intel's 8-byte dummy frames decode to nothing).
//! - QuickTime H.263 from the archive (long vectors) through the MOV
//!   demuxer.

mod support;

use support::*;

#[test]
fn fate_h263_encode_decode_tests() {
    for source in ["vsynth1", "vsynth2", "vsynth_lena"] {
        for test in ["h263", "h263-obmc", "h263p"] {
            let name = format!("{source}-{test}");
            let (path, fate_md5) = fate_vsynth(source, test);
            let frames = packed_frames(&decode(&path), &name);
            let ours: Vec<String> = frames.iter().map(|f| refcheck::md5_hex(f)).collect();
            assert_frames_equal(&name, &ours, &ffmpeg_md5s(&path, &[]));
            assert_eq!(refcheck::md5_hex(&frames.concat()), fate_md5, "{name}: FATE's decode MD5");
        }
    }
}

/// The player has no raw H.263 container; the decoder re-frames any
/// byte split of a stream on its picture start codes, including splits
/// inside a start code. FFmpeg's raw remuxes of two FATE streams, fed in
/// pieces, against FFmpeg reading them with its `h263` demuxer.
#[test]
fn raw_h263_streams_in_pieces() {
    for test in ["h263-obmc", "h263p"] {
        let (avi, _) = fate_vsynth("vsynth1", test);
        let raw = remux(&format!("vsynth1-{test}.h263"), &avi, &["-c", "copy", "-f", "h263"]);
        let theirs = ffmpeg_md5s(&raw, &["-f", "h263"]);
        for chunk in [4096, 997] {
            let ours: Vec<String> = decode_raw(&raw, "h263", chunk).into_iter().map(|(md5, _)| md5).collect();
            assert_frames_equal(&format!("vsynth1-{test}.h263 in {chunk}-byte pieces"), &ours, &theirs);
        }
    }
}

/// oxideav-avi refuses both archive files over their MP3 audio header
/// (`strh.dwSampleSize` 1 on a VBR codec), so the video is read from
/// FFmpeg's video-only remux, which FFmpeg decodes exactly as it does
/// the original.
#[test]
fn intel_h263_archive_files() {
    for file in ["V-codecs/I263/i263.avi", "V-codecs/I263/i263_2.avi"] {
        let original = archive(file);
        let name = original.file_stem().unwrap().to_str().unwrap();
        let video = remux(&format!("{name}-video.avi"), &original, &["-map", "0:v:0", "-c", "copy", "-f", "avi"]);
        let theirs = ffmpeg_md5s(&video, &[]);
        assert_eq!(theirs, ffmpeg_md5s(&original, &[]), "{file}: FFmpeg decodes the remux differently");
        let decoded = decode(&video);
        assert_eq!(decoded.params.codec_id.as_str(), "h263i", "{file}");
        assert_frames_equal(file, &frame_md5s(&decoded, file), &theirs);
    }
}

#[test]
fn quicktime_h263_archive_files() {
    for file in [
        "V-codecs/h263/baikonur_r7_overflight.mov",
        "V-codecs/h263/baikonur_r7_rollout.mov",
        "V-codecs/h263/iss_soyuztm32_launch.mov",
        "V-codecs/h263/pooch.mov",
        "V-codecs/h263/100374.mov",
    ] {
        let path = archive(file);
        assert_frames_equal(file, &frame_md5s(&decode(&path), file), &ffmpeg_md5s(&path, &[]));
    }
}
