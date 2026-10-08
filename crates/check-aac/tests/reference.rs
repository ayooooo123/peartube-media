//! FATE/FFmpeg reference tests for the PearTube AAC fork.
//!
//! Samples decode through the fork behind OxideAV's mov/mp4/mpegts/adts
//! registries. The float reference floor is at least 90 dB, with exact
//! sample counts. Previously passing profiles retain their stronger floors.
//!
//! MP4 edit-list and iTunSMPB trims (encoder delay, end padding) reach the
//! decode through `Demuxer::packet_metadata`, and `refcheck::decode` applies
//! them as the player does, so ELD and USAC samples match FFmpeg's sample
//! counts exactly; the USAC checks also hold the trims to the per-channel
//! FFprobe figures in `check_aac::USAC_SAMPLES` against the raw decode.
//! USAC also checks the ISO conformance S16 references and every FATE
//! loudness target.
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
    // al_sbr_cm_48_2 and al_sbr_cm_48_5.1 are 132.837847 and 129.001008 dB
    // from FFmpeg 2da55bf (133.793770 and 129.792666 from 9.0.2, where these
    // floors were first set); each floor is that less 0.5 dB.
    ("aac/al_sbr_cm_48_2.mp4", 132.337847),
    ("aac/al_sbr_cm_48_5.1.mp4", 128.501008),
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

/// ER AAC ELD samples whose last frame reaches past the track's duration:
/// FFmpeg's mov demuxer attaches `discard_padding` to the last packet (55
/// mono samples for `er_eld1001np_44`, 32 per channel for
/// `er_eld2000np_48`; FATE's `SIZE_TOLERANCE` for the same pair). The MP4
/// demuxer exposes the same trim, so the decode is FFmpeg's length; each
/// entry pins the SNR floor and the padding (interleaved samples) the
/// untrimmed frame carries.
const PASSING_END_TRIMMED: &[(&str, f64, usize)] = &[
    ("aac/er_eld1001np_44_ep0.mp4", 137.538027, 55),
    ("aac/er_eld2000np_48_ep0.mp4", 137.490163, 64),
];


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
    for &(rel, floor, padding) in PASSING_END_TRIMMED {
        let (ours, path, _ch) = decoded_f32(rel);
        let ff = refcheck::ffmpeg_audio_f32(&path, 0);
        assert_eq!(ours.len(), ff.len(), "{rel}: sample count");
        let snr = refcheck::snr_db(&ff, &ours, 0);
        eprintln!("{rel}: {} interleaved samples, SNR {snr:.6} dB", ours.len());
        assert!(
            snr >= floor,
            "{rel}: SNR {snr:.2} dB below floor {floor} dB"
        );
        // The trim is the container's: the raw packets decode to exactly
        // `padding` more samples, which are the ones dropped.
        let raw = check_aac::decoded_raw_f32(rel);
        assert_eq!(raw.len(), ours.len() + padding, "{rel}: raw sample count");
        assert_eq!(raw[..ours.len()], ours[..], "{rel}: the presented samples are the raw decode's");
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


/// The decode is FFmpeg's presentation exactly: the MP4 demuxer's trims are
/// the FFprobe skip/discard figures of `check_aac::USAC_SAMPLES` (per
/// channel), checked against the untrimmed decode of the same packets, and
/// every presented sample is compared, including the first block; no codec
/// startup region is omitted. xhe_target_level's content uses the
/// independent native oracle in native_reference.rs (FFmpeg is not one);
/// here only its trims are checked. The FFmpeg entries retain their
/// original floors.
#[test]
fn reference_usac_samples() {
    for &(rel, initial_skip, final_padding, floor) in check_aac::USAC_SAMPLES {
        let (ours, path, channels) = decoded_f32(rel);
        let start = initial_skip * channels as usize;
        let end_padding = final_padding * channels as usize;
        let raw = check_aac::decoded_raw_f32(rel);
        assert_eq!(raw.len(), ours.len() + start + end_padding, "{rel}: the trims are the FFprobe figures");
        assert_eq!(raw[start..raw.len() - end_padding], ours[..], "{rel}: the presented samples are the raw decode's");
        let Some(floor) = floor else { continue };
        let ff = refcheck::ffmpeg_audio_f32(&path, 0);
        assert_eq!(ours.len(), ff.len(), "{rel}: sample count");
        let snr = refcheck::snr_db(&ff, &ours, 0);
        eprintln!("{rel}: {} interleaved samples (skip {start}, tail {end_padding} raw), SNR {snr:.6} dB", ours.len());
        assert!(snr >= floor, "{rel}: USAC FD SNR {snr:.6} dB below floor {floor} dB");
        let stem = path.file_stem().unwrap().to_str().unwrap();
        if stem.starts_with("Fd_") {
            // The two older Ms references retain final padding; the newer
            // FD references cover exactly the presentation interval.
            let fate_pcm = if stem.starts_with("Fd_2_c1_Ms_") { &raw[start..] } else { &ours[..] };
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
    // Floors: the first measured SNR (fork e03fbe6) minus 0.5 dB.
    for (rel, target, golden, floor) in [
        ("aac/usac/Ext_2_c1_Ln_0x03.mp4", -16, "aac/usac/Ext_2_c1_Ln_0x03__Lou-16.s16", 139.252920),
        ("aac/usac/Ext_2_c1_Ln_0x03.mp4", -24, "aac/usac/Ext_2_c1_Ln_0x03__Lou-24.s16", 139.604440),
        ("aac/usac/Ext_2_c1_Ln_0x03.mp4", -31, "aac/usac/Ext_2_c1_Ln_0x03__Lou-31.s16", 139.325931),
    ] {
        let (ours, path, channels) = check_aac::decoded_usac_target(rel, target);
        let reference = std::process::Command::new(refcheck::pinned_ffmpeg())
            .args(["-v", "error", "-nostdin", "-target_level", &target.to_string(), "-i"])
            .arg(&path)
            .args(["-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"])
            .output().unwrap();
        assert!(reference.status.success(), "FFmpeg: {}", String::from_utf8_lossy(&reference.stderr));
        let ff: Vec<_> = reference.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        let &(_, initial_skip, final_padding, _) = check_aac::USAC_SAMPLES.iter().find(|s| s.0 == rel).unwrap();
        let start = initial_skip * channels as usize;
        let tail = final_padding * channels as usize;
        assert_eq!(ours.len(), ff.len() + start + tail, "{rel}: target {target} raw length");
        let presented = &ours[start..ours.len() - tail];
        let snr = refcheck::snr_db(&ff, presented, 0);
        eprintln!("{rel}: target {target}, SNR {snr:.6} dB");
        assert!(snr >= floor, "{rel}: target {target} SNR {snr:.6} dB below floor {floor} dB");
        assert_fate_pcm(&refcheck::fate(golden), presented);
    }
}
