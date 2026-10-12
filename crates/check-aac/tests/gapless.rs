//! FFmpeg's gapless AAC samples: tests/fate/gapless.mak's audiomatch MP4/M4A
//! files from afconvert, Nero, fdk-aac, Dolby and QuickTime (iTunSMPB and
//! edit-list priming, end padding from iTunSMPB, the edit list or the last
//! sample's duration, HE-AAC v1/v2 whose counts follow the SBR output rate)
//! and the iTunes `gaplessinfo` samples. Decoded through the MP4 demuxer's
//! trims, each has exactly FFmpeg's sample count and is aligned with its
//! samples (≥ 90 dB; a trim one sample off scores near 0 dB).
//!
//! The Dolby stereo HE-AAC and LC files carry no trims (FFmpeg applies
//! none either): their 80.6 / 84.9 dB are the decoder's, the same with or
//! without this change, so they are held to the length only.
//!
//! Not here: the `faac` files, whose priming only FFmpeg's decoder knows
//! (it recognizes libfaac's fill element and skips 1024 samples itself; the
//! container declares none).

use check_aac::decoded_f32;

/// Untrimmed files whose decode is below 90 dB for reasons of the decoder.
const LENGTH_ONLY: &[&str] =
    &["audiomatch/tones_dolby_44100_stereo_aac_he.mp4", "audiomatch/tones_dolby_44100_stereo_aac_lc.mp4"];

const SAMPLES: &[&str] = &[
    "audiomatch/square3.m4a",
    "audiomatch/tones_afconvert_16000_mono_aac_he.m4a",
    "audiomatch/tones_afconvert_16000_mono_aac_lc.m4a",
    "audiomatch/tones_afconvert_16000_stereo_aac_he.m4a",
    "audiomatch/tones_afconvert_16000_stereo_aac_he2.m4a",
    "audiomatch/tones_afconvert_16000_stereo_aac_lc.m4a",
    "audiomatch/tones_afconvert_44100_mono_aac_he.m4a",
    "audiomatch/tones_afconvert_44100_mono_aac_lc.m4a",
    "audiomatch/tones_afconvert_44100_stereo_aac_he.m4a",
    "audiomatch/tones_afconvert_44100_stereo_aac_he2.m4a",
    "audiomatch/tones_afconvert_44100_stereo_aac_lc.m4a",
    "audiomatch/tones_dolby_44100_mono_aac_he.mp4",
    "audiomatch/tones_dolby_44100_mono_aac_lc.mp4",
    "audiomatch/tones_dolby_44100_stereo_aac_he.mp4",
    "audiomatch/tones_dolby_44100_stereo_aac_he2.mp4",
    "audiomatch/tones_dolby_44100_stereo_aac_lc.mp4",
    "audiomatch/tones_fdkaac_44100_stereo_aac_he.m4a",
    "audiomatch/tones_fdkaac_44100_stereo_aac_he2.m4a",
    "audiomatch/tones_fdkaac_44100_stereo_aac_lc.m4a",
    "audiomatch/tones_nero_16000_mono_aac_he.m4a",
    "audiomatch/tones_nero_16000_mono_aac_lc.m4a",
    "audiomatch/tones_nero_16000_stereo_aac_he.m4a",
    "audiomatch/tones_nero_16000_stereo_aac_he2.m4a",
    "audiomatch/tones_nero_16000_stereo_aac_lc.m4a",
    "audiomatch/tones_nero_44100_mono_aac_he.m4a",
    "audiomatch/tones_nero_44100_mono_aac_lc.m4a",
    "audiomatch/tones_nero_44100_stereo_aac_he.m4a",
    "audiomatch/tones_nero_44100_stereo_aac_he2.m4a",
    "audiomatch/tones_nero_44100_stereo_aac_lc.m4a",
    "audiomatch/tones_quicktime7_44100_stereo_aac_lc.mp4",
    "audiomatch/tones_quicktimeX_44100_stereo_aac_lc.m4a",
    "gapless/102400samples_qt-lc-aac.m4a",
    "cover_art/Owner-iTunes_9.0.3.15.m4a",
];

#[test]
fn gapless_samples_have_ffmpegs_length_and_samples() {
    let mut failures = Vec::new();
    for rel in SAMPLES {
        let (ours, path, _channels) = decoded_f32(rel);
        let ff = refcheck::ffmpeg_audio_f32(&path, 0);
        let snr = refcheck::try_snr_db(&ff, &ours, usize::MAX);
        eprintln!("{rel}: {} interleaved samples, FFmpeg {}, SNR {snr:.3?} dB", ours.len(), ff.len());
        if ours.len() != ff.len() {
            failures.push(format!("{rel}: {} samples, FFmpeg {}", ours.len(), ff.len()));
        } else if !LENGTH_ONLY.contains(rel) && !snr.as_ref().is_ok_and(|s| *s >= 90.0) {
            failures.push(format!("{rel}: SNR {snr:?} below 90 dB"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
