//! FATE/FFmpeg reference tests for the PearTube AAC fork.
//!
//! Samples decode through the fork behind OxideAV's mov/mp4/mpegts/adts
//! registries. The float reference floor is at least 90 dB, with exact
//! sample counts. Previously passing profiles retain their stronger floors.
//!
//! ELD and USAC cases with MP4 edit-list boundaries assert their *exact*
//! raw-output surplus separately and compare every presented sample, not an
//! arbitrary common prefix. These codec-fidelity tests do not claim the
//! missing container sample-trim propagation is implemented. USAC also checks
//! the ISO conformance S16 references and every FATE loudness target.
//!
//! The FATE suite must be present: `FATE_SUITE` (default
//! `~/projects/fate-suite`).

use check_aac::decoded_f32;

/// Samples the fork matches at ≥ 90 dB SNR against FFmpeg's float
/// decode: AAC LC / Main / SSR / LTP multichannel (including coupling
/// channels), HE-AAC v1 (SBR) stereo + 5.1, HE-AAC v2 (SBR + parametric
/// stereo) in every CT signalling variant, ER AAC LD and ER AAC ELD.
/// Floors are at least the pre-FFT fork `1c28f30` SNR minus 0.5 dB;
/// older, stronger floors are retained.
const PASSING: &[(&str, f64)] = &[
    // AAC LC mono/stereo/multichannel (al* series).
    ("aac/al04_44.mp4", 138.0),
    ("aac/al04sf_48.mp4", 137.289261),
    ("aac/al05_44.mp4", 138.0),
    ("aac/al06_44.mp4", 138.0),
    ("aac/al15_44.mp4", 107.0),
    ("aac/al17_44.mp4", 138.0),
    ("aac/am00_88.mp4", 138.248463),
    // AAC LC mono, PNS-heavy (FFmpeg's shared noise generator and the
    // sine-shaped block before the first frame).
    ("aac/al18_44.mp4", 137.496442),
    // AAC Main 5.1 (prediction before intensity stereo).
    ("aac/am05_44.mp4", 103.057012),
    // AAC LC 96 kHz 5.1 with a dependently switched coupling channel
    // (DPCM gains, sign split off the running sum as FFmpeg does).
    ("aac/al07_96.mp4", 137.756574),
    // AAC LTP stereo (LTP analysis windowed with the 256-point short
    // transform at block switches).
    ("aac/ap05_48.mp4", 135.091059),
    // HE-AAC v1 (SBR), stereo and 5.1, dual-rate and 96 kHz-core.
    ("aac/al_sbr_cm_48_2.mp4", 133.293771),
    ("aac/al_sbr_cm_48_5.1.mp4", 129.292669),
    ("aac/al_sbr_sr_48_2_fsaac48.mp4", 136.0),
    // HE-AAC v2 (SBR + PS): explicit, implicit and backward-compatible
    // signalling over MP4, 3GP and ADTS.
    ("aac/al_sbr_ps_04_new.mp4", 133.710337),
    ("aac/al_sbr_ps_06_new.mp4", 131.752459),
    ("aac/CT_DecoderCheck/sbr_i-ps_i.aac", 131.663250),
    ("aac/CT_DecoderCheck/sbr_bc-ps_i.mp4", 131.663250),
    ("aac/CT_DecoderCheck/sbr_bic-ps_i.3gp", 131.663250),
    ("aac/CT_DecoderCheck/sbr_bc-ps_bc.mp4", 131.663250),
    ("aac/CT_DecoderCheck/sbr_i-ps_bic.mp4", 131.663250),
    ("aac/CT_DecoderCheck/sbr_i-ps_i.mp4", 131.663250),
    ("aac/CT_DecoderCheck/sbr_bc-ps_i.3gp", 131.663250),
    // ER AAC LD 5.1 (ER tool order + LD TNS widths).
    ("aac/er_ad6000np_44_ep0.mp4", 138.191074),
    // ER AAC ELD 480-line stereo (low-delay filterbank).
    ("aac/er_eld2100np_48_ep0.mp4", 137.241180),
];

/// ER AAC ELD samples whose MP4 edit list trims the tail of the last
/// frame: FFmpeg's mov demuxer attaches `discard_padding` to the last
/// packet (55 mono samples for `er_eld1001np_44`, 32 per channel for
/// `er_eld2000np_48`; FATE's `SIZE_TOLERANCE` for the same pair) and
/// libavcodec drops them, while the fork emits the whole frame — the
/// trim is container data the decoder never sees. Each entry pins the
/// SNR floor over FFmpeg's length and the exact interleaved surplus.
const PASSING_END_TRIMMED: &[(&str, f64, usize)] = &[
    ("aac/er_eld1001np_44_ep0.mp4", 137.538027, 55),
    ("aac/er_eld2000np_48_ep0.mp4", 137.490163, 64),
];

/// Samples the fork decodes end-to-end whose SNR against FFmpeg's
/// float decode is still below the 90 dB floor. Each entry pins the
/// measured SNR: a change in either direction (a regression or the
/// gap being closed) fails the assert, so the table tracks progress.
const KNOWN_GAPS: &[(&str, f64)] = &[];


#[test]
fn reference_passing_samples() {
    for (rel, floor) in PASSING {
        let (ours, path, _ch) = decoded_f32(rel);
        let ff = refcheck::ffmpeg_audio_f32(&path, 0);
        assert_eq!(ours.len(), ff.len(), "{rel}: sample count");
        let snr = refcheck::snr_db(&ff, &ours, 4096);
        eprintln!("{rel}: {} interleaved samples, SNR {snr:.6} dB", ours.len());
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
        eprintln!("{rel}: {} interleaved samples (surplus {surplus}), SNR {snr:.6} dB", ours.len());
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

/// Keep the raw FD PCM and container presentation trim separate. These
/// assertions verify the exact untrimmed length and compare every presented
/// sample, including the first block; no codec startup region is omitted.
#[test]
fn reference_usac_samples() {
    for &(rel, initial_skip, final_padding) in check_aac::USAC_SAMPLES {
        let (ours, path, channels) = decoded_f32(rel);
        let ff = refcheck::ffmpeg_audio_f32(&path, 0);
        let start = initial_skip * channels as usize;
        let end_padding = final_padding * channels as usize;
        assert_eq!(ours.len(), ff.len() + start + end_padding, "{rel}: raw sample count");
        let presented = &ours[start..ours.len() - end_padding];
        let snr = refcheck::snr_db(&ff, presented, 0);
        eprintln!("{rel}: {} raw interleaved samples, skip {start}, tail {end_padding}, SNR {snr:.6} dB", ours.len());
        assert!(snr >= 90.0, "{rel}: USAC FD SNR {snr:.6} dB below 90 dB");
        let stem = path.file_stem().unwrap().to_str().unwrap();
        if stem.starts_with("Fd_") {
            // The two older Ms references retain final padding; the newer
            // FD references cover exactly the presentation interval.
            let fate_pcm = if stem.starts_with("Fd_2_c1_Ms_") { &ours[start..] } else { presented };
            assert_fate_pcm(&path.with_extension("s16"), fate_pcm);
        }
    }
}

fn assert_fate_pcm(path: &std::path::Path, pcm: &[f32]) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(bytes.len(), pcm.len() * 2, "{}: exact FATE PCM length", path.display());
    let maximum = bytes.chunks_exact(2).zip(pcm).map(|(b, &value)| {
        let reference = i16::from_le_bytes([b[0], b[1]]) as i32;
        let ours = (value * 32768.0).round_ties_even().clamp(-32768.0, 32767.0) as i32;
        (reference - ours).abs()
    }).max().unwrap();
    eprintln!("{}: S16 maximum error {maximum} LSB", path.display());
    // tests/fate/aac.mak uses CMP=oneoff, FUZZ=2.
    assert!(maximum <= 2, "{}: FATE PCM error {maximum} LSB", path.display());
}

#[test]
fn reference_usac_loudness_targets() {
    for (rel, target, golden) in [
        ("aac/usac/Ext_2_c1_Ln_0x03.mp4", -16, "aac/usac/Ext_2_c1_Ln_0x03__Lou-16.s16"),
        ("aac/usac/Ext_2_c1_Ln_0x03.mp4", -24, "aac/usac/Ext_2_c1_Ln_0x03__Lou-24.s16"),
        ("aac/usac/Ext_2_c1_Ln_0x03.mp4", -31, "aac/usac/Ext_2_c1_Ln_0x03__Lou-31.s16"),
        ("aac/usac/xhe_target_level.m4a", -24, "aac/usac/xhe_target_level.s16"),
    ] {
        let (ours, path, channels) = check_aac::decoded_usac_target(rel, target);
        let reference = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-target_level", &target.to_string(), "-i"])
            .arg(&path)
            .args(["-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"])
            .output().unwrap();
        assert!(reference.status.success(), "FFmpeg: {}", String::from_utf8_lossy(&reference.stderr));
        let ff: Vec<_> = reference.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        let &(_, initial_skip, final_padding) = check_aac::USAC_SAMPLES.iter().find(|s| s.0 == rel).unwrap();
        let start = initial_skip * channels as usize;
        let tail = final_padding * channels as usize;
        assert_eq!(ours.len(), ff.len() + start + tail, "{rel}: target {target} raw length");
        let presented = &ours[start..ours.len() - tail];
        let snr = refcheck::snr_db(&ff, presented, 0);
        eprintln!("{rel}: target {target}, SNR {snr:.6} dB");
        assert!(snr >= 90.0, "{rel}: target {target} SNR {snr:.6} dB");
        // xHE's S16 FATE reference retains 128/ch padding, but not the
        // whole final AU that is outside the edit-list presentation.
        let fate_pcm = if rel.ends_with("xhe_target_level.m4a") {
            &ours[..ours.len() - 1024 * channels as usize]
        } else { presented };
        assert_fate_pcm(&refcheck::fate(golden), fate_pcm);
    }
}
