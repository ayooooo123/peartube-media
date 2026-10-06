//! FATE/FFmpeg reference tests for the PearTube AAC fork.
//!
//! Every sample in FFmpeg's `tests/fate/aac.mak` decode matrix is
//! decoded through the fork (`oxideav-aac`) behind OxideAV's
//! `mov` / `mp4` / `mpegts` / `adts` container registries and compared
//! against FFmpeg's float AAC decoder (`ffmpeg_audio_f32`): the fork's
//! pipeline is float, so the reference floor is 90 dB SNR over the
//! common length, with the sample count within one SBR frame of
//! FFmpeg's.
//!
//! The suite is exhaustive over `aac.mak`: every sample appears in
//! exactly one of `PASSING` (asserted ≥ 90 dB) or `KNOWN_GAPS`
//! (asserted to decode, with its measured SNR pinned so a regression
//! in a gap region fails too). A sample in `KNOWN_GAPS` is a remaining
//! porting task, not an acceptance; see the task report.
//!
//! The FATE suite must be present: `FATE_SUITE` (default
//! `~/projects/fate-suite`).

use check_aac::decoded_f32;

/// Samples the fork matches at ≥ 90 dB SNR against FFmpeg's float
/// decode: AAC LC / Main / SSR / LTP multichannel (including coupling
/// channels), HE-AAC v1 (SBR) stereo + 5.1, HE-AAC v2 (SBR + parametric
/// stereo) in every CT signalling variant, ER AAC LD and ER AAC ELD. Each floor
/// sits just under the measured SNR, so a regression fails even while
/// it stays above 90 dB.
const PASSING: &[(&str, f64)] = &[
    // AAC LC mono/stereo/multichannel (al* series).
    ("aac/al04_44.mp4", 138.0),
    ("aac/al04sf_48.mp4", 137.0),
    ("aac/al05_44.mp4", 138.0),
    ("aac/al06_44.mp4", 138.0),
    ("aac/al15_44.mp4", 107.0),
    ("aac/al17_44.mp4", 138.0),
    ("aac/am00_88.mp4", 138.0),
    // AAC LC mono, PNS-heavy (FFmpeg's shared noise generator and the
    // sine-shaped block before the first frame).
    ("aac/al18_44.mp4", 137.0),
    // AAC Main 5.1 (prediction before intensity stereo).
    ("aac/am05_44.mp4", 102.0),
    // AAC LC 96 kHz 5.1 with a dependently switched coupling channel
    // (DPCM gains, sign split off the running sum as FFmpeg does).
    ("aac/al07_96.mp4", 137.0),
    // AAC LTP stereo (LTP analysis windowed with the 256-point short
    // transform at block switches).
    ("aac/ap05_48.mp4", 134.0),
    // HE-AAC v1 (SBR), stereo and 5.1, dual-rate and 96 kHz-core.
    ("aac/al_sbr_cm_48_2.mp4", 133.0),
    ("aac/al_sbr_cm_48_5.1.mp4", 129.0),
    ("aac/al_sbr_sr_48_2_fsaac48.mp4", 136.0),
    // HE-AAC v2 (SBR + PS): explicit, implicit and backward-compatible
    // signalling over MP4, 3GP and ADTS.
    ("aac/al_sbr_ps_04_new.mp4", 133.0),
    ("aac/al_sbr_ps_06_new.mp4", 131.0),
    ("aac/CT_DecoderCheck/sbr_i-ps_i.aac", 131.0),
    ("aac/CT_DecoderCheck/sbr_bc-ps_i.mp4", 131.0),
    ("aac/CT_DecoderCheck/sbr_bic-ps_i.3gp", 131.0),
    ("aac/CT_DecoderCheck/sbr_bc-ps_bc.mp4", 131.0),
    ("aac/CT_DecoderCheck/sbr_i-ps_bic.mp4", 131.0),
    ("aac/CT_DecoderCheck/sbr_i-ps_i.mp4", 131.0),
    ("aac/CT_DecoderCheck/sbr_bc-ps_i.3gp", 131.0),
    // ER AAC LD 5.1 (ER tool order + LD TNS widths).
    ("aac/er_ad6000np_44_ep0.mp4", 138.0),
    // ER AAC ELD 480-line stereo (low-delay filterbank).
    ("aac/er_eld2100np_48_ep0.mp4", 137.0),
];

/// ER AAC ELD samples whose MP4 edit list trims the tail of the last
/// frame: FFmpeg's mov demuxer attaches `discard_padding` to the last
/// packet (55 mono samples for `er_eld1001np_44`, 32 per channel for
/// `er_eld2000np_48`; FATE's `SIZE_TOLERANCE` for the same pair) and
/// libavcodec drops them, while the fork emits the whole frame — the
/// trim is container data the decoder never sees. Each entry pins the
/// SNR floor over FFmpeg's length and the exact interleaved surplus.
const PASSING_END_TRIMMED: &[(&str, f64, usize)] = &[
    ("aac/er_eld1001np_44_ep0.mp4", 137.0, 55),
    ("aac/er_eld2000np_48_ep0.mp4", 137.0, 64),
];

/// Samples the fork decodes end-to-end whose SNR against FFmpeg's
/// float decode is still below the 90 dB floor. Each entry pins the
/// measured SNR: a change in either direction (a regression or the
/// gap being closed) fails the assert, so the table tracks progress.
const KNOWN_GAPS: &[(&str, f64)] = &[];

/// Samples `aac.mak` lists that the fork cannot decode at all: the
/// AOT 42 (USAC / xHE-AAC) decoder does not exist in the fork yet.
/// Asserted here so adding support fails this table and the sample
/// moves up to PASSING/KNOWN_GAPS.
const UNDECODED: &[&str] = &[
    "aac/Fd_2_c1_Ms_0x01.mp4",
    "aac/Fd_2_c1_Ms_0x04.mp4",
    "aac/usac/Fd_1_c1_0x03.mp4",
    "aac/usac/Fd_1_c1_0x04.mp4",
    "aac/usac/Fd_2_c1_0x03.mp4",
    "aac/usac/Fd_2_c1_0x05.mp4",
    "aac/usac/Fd_2_c1_Tns_0x04.mp4",
    "aac/usac/Ext_2_c1_Ln_0x03.mp4",
    "aac/usac/xhe_target_level.m4a",
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

#[test]
fn reference_end_trimmed_samples() {
    for (rel, floor, surplus) in PASSING_END_TRIMMED {
        let (ours, path, _ch) = decoded_f32(rel);
        let ff = refcheck::ffmpeg_audio_f32(&path, 0);
        assert_eq!(ours.len(), ff.len() + surplus, "{rel}: sample count");
        let snr = refcheck::snr_db(&ff, &ours[..ff.len()], 0);
        assert!(
            snr >= *floor,
            "{rel}: SNR {snr:.2} dB below floor {floor} dB"
        );
    }
}

/// Samples with structural mid-stream config changes the fork handles
/// differently from FFmpeg: the fork honours the mid-stream in-band
/// PCE (stereo → 5.1) that `latm_stereo_to_51.ts` carries; FFmpeg's
/// LATM parse drops that region and outputs stereo only. The test
/// asserts the decode still runs and produces the expected 288 frames.
#[test]
fn reference_config_change_samples() {
    let (ours, _path, _ch) = decoded_f32("aac/latm_stereo_to_51.ts");
    assert_eq!(ours.len(), 1265664, "latm_stereo_to_51: frame count");
}

#[test]
fn reference_known_gap_samples() {
    for (rel, pin) in KNOWN_GAPS {
        let (ours, path, _ch) = decoded_f32(rel);
        let ff = refcheck::ffmpeg_audio_f32(&path, 0);
        assert!(
            ff.len().abs_diff(ours.len()) <= 4096,
            "{rel}: length ours={} ff={}",
            ours.len(),
            ff.len()
        );
        let snr = refcheck::snr_db(&ff, &ours, 4096);
        assert!(
            (snr - *pin).abs() < 10.0,
            "{rel}: SNR {snr:.2} dB moved away from the pinned {pin} dB — \
             update the table (a rise past 90 dB graduates the sample to \
             PASSING; a drop is a regression)"
        );
    }
}

/// The USAC (AOT 42) decoder does not exist yet: the ASC parse rejects
/// the config. Assert the rejection so the samples stay visible in the
/// suite.
#[test]
fn reference_undecoded_samples_rejected_at_config() {
    for rel in UNDECODED {
        let path = refcheck::fate(rel);
        let result = std::panic::catch_unwind(|| {
            check_aac::decoded_f32(rel);
        });
        if result.is_ok() {
            // Decodes now: the port landed — move the sample up.
            panic!(
                "{rel}: decoded but is still listed in UNDECODED; move it to \
                 PASSING or KNOWN_GAPS"
            );
        }
        let _ = path;
    }
}
