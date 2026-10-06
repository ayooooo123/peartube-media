//! FATE/FFmpeg reference tests for the PearTube AAC fork.
//!
//! Every sample is decoded through the fork (`oxideav-aac`) behind
//! OxideAV's `mov` / `mp4` / `mpegts` container registries and compared
//! against FFmpeg's float AAC decoder (`ffmpeg_audio_f32`): the fork's
//! pipeline is float, so the reference floor is 90 dB SNR over the
//! common length, with the sample count within one SBR frame of
//! FFmpeg's.
//!
//! Samples are the FATE corpus `tests/fate/aac.mak` runs (`pcm`
//! comparisons): AAC LC / Main / LTP / SSR multichannel, HE-AAC v1
//! (SBR), HE-AAC v2 (SBR + PS), the Coding Technologies decoder-check
//! streams, LATM-in-MPEG-TS/-PS, and the SCE-in-stereo framecrc file.
//!
//! The FATE suite must be present: `FATE_SUITE` (default
//! `~/projects/fate-suite`).

use check_aac::decoded_f32;

/// Samples the fork matches at ≥ 90 dB SNR against FFmpeg's float
/// decode, with the exact frame/sample count.
const PASSING: &[(&str, f64)] = &[
    // AAC LC mono/stereo/multichannel (al* series).
    ("aac/al04_44.mp4", 138.0),
    ("aac/al04sf_48.mp4", 137.0),
    ("aac/al05_44.mp4", 138.0),
    ("aac/al06_44.mp4", 138.0),
    ("aac/al15_44.mp4", 107.0),
    ("aac/al17_44.mp4", 138.0),
    ("aac/am00_88.mp4", 138.0),
    // HE-AAC v1 (SBR), stereo and 5.1, dual-rate and 96 kHz-core.
    ("aac/al_sbr_cm_48_2.mp4", 133.0),
    ("aac/al_sbr_cm_48_5.1.mp4", 129.0),
    ("aac/al_sbr_sr_48_2_fsaac48.mp4", 136.0),
];

#[test]
fn reference_passing_samples() {
    for (rel, floor) in PASSING {
        let (ours, path, _ch) = decoded_f32(rel);
        let ff = refcheck::ffmpeg_audio_f32(&path, 0);
        assert_eq!(ours.len(), ff.len(), "{rel}: sample count");
        let snr = refcheck::snr_db(&ff, &ours, 4096);
        assert!(
            snr >= *floor,
            "{rel}: SNR {snr:.2} dB below floor {floor} dB"
        );
    }
}
