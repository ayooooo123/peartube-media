use oxideav_core::MediaType;
use refcheck::{decode, fate, ffmpeg_audio_f32, interleaved_f32, snr_db};

fn check_audio(sample: &str, min_snr: f64, max_slack: usize) {
    let path = fate(sample);
    let decoded = decode(
        &path,
        &[codec_ra::register, demux_rm::register],
        MediaType::Audio,
        0,
    );
    let got = interleaved_f32(&decoded);
    let ref_samples = ffmpeg_audio_f32(&path, 0);

    assert!(
        !got.is_empty(),
        "{sample}: decoder produced no audio samples"
    );
    let len_diff = got.len().abs_diff(ref_samples.len());
    println!(
        "{sample}: got {} samples, ref has {} samples (diff {len_diff})",
        got.len(),
        ref_samples.len()
    );

    let snr = snr_db(&ref_samples, &got, max_slack);
    println!("{sample}: SNR = {snr:.2} dB");
    assert!(
        snr >= min_snr,
        "{sample}: SNR {snr:.2} dB below minimum {min_snr} dB"
    );
}

#[test]
fn test_ra144_rm() {
    let path = fate("real/ra3_in_rm_file.rm");
    let decoded = decode(
        &path,
        &[codec_ra::register, demux_rm::register],
        MediaType::Audio,
        0,
    );
    let got = interleaved_f32(&decoded);
    let ref_samples = ffmpeg_audio_f32(&path, 0);
    println!("got len: {}, ref len: {}", got.len(), ref_samples.len());
    let prefix = 453 * 1920; // 869760 samples (first 453 packets before corrupt EOF packet)
    let snr = snr_db(&ref_samples[..prefix], &got[..prefix], 0);
    println!("SNR over {prefix} valid samples: {snr:.2} dB");
    assert_eq!(snr, f64::INFINITY, "ra144 must be bit-exact on valid packets");
}

#[test]
fn test_ra144_ra() {
    check_audio("realaudio/ra3.ra", f64::INFINITY, 0);
}

#[test]
fn test_ra288_rm() {
    // ra_288 is float -> >= 90 dB
    check_audio("real/ra_288.rm", 90.0, 160);
}

#[test]
fn test_ra288_ra() {
    check_audio("realaudio/ra4_288.ra", 90.0, 160);
}

#[test]
fn test_ralf() {
    // ralf is lossless -> bit exact (infinity), slack 2048 allows the final truncated frame
    check_audio("lossless-audio/luckynight-partial.rmvb", f64::INFINITY, 2048);
}

#[test]
fn test_cook() {
    // cook is float -> >= 90 dB
    check_audio("real/ra_cook.rm", 90.0, 1024);
}

#[test]
fn test_sipr_5k0() {
    // sipr is float -> >= 90 dB
    check_audio("sipr/sipr_5k0.rm", 90.0, 480);
}

#[test]
fn test_sipr_6k5() {
    check_audio("sipr/sipr_6k5.rm", 90.0, 288);
}

#[test]
fn test_sipr_8k5() {
    check_audio("sipr/sipr_8k5.rm", 90.0, 144);
}

#[test]
fn test_sipr_16k() {
    let path = fate("sipr/sipr_16k.rm");
    let decoded = decode(
        &path,
        &[codec_ra::register, demux_rm::register],
        MediaType::Audio,
        0,
    );
    let got = interleaved_f32(&decoded);
    let ref_samples = ffmpeg_audio_f32(&path, 0);
    let limit = 3250 * 160;
    let snr = snr_db(&ref_samples[..limit], &got[..limit], 0);
    println!("sipr_16k (first 3250 frames): SNR = {snr:.2} dB");
    assert!(snr >= 90.0, "sipr_16k must have >= 90 dB SNR over valid frames: {snr}");
}

#[test]
fn test_sipr_16k_ra() {
    check_audio("realaudio/RA5.0_16kbps_voice_wideband.ra", 90.0, 160);
}
