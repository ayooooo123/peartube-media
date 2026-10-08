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

/// Bit-exact to the end. The file's last packet is cut (137 of 240 bytes):
/// FFmpeg's demuxer hands over the 137 bytes, flagged corrupt, its decoder
/// decodes 6 frames and refuses the 17-byte rest; nothing is zero-filled.
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
    assert_eq!(got.len(), ref_samples.len(), "453 whole packets and 6 frames of the cut one");
    assert_eq!(snr_db(&ref_samples, &got, 0), f64::INFINITY, "ra144 must be bit-exact");
}

#[test]
fn test_ra144_ra() {
    check_audio("realaudio/ra3.ra", f64::INFINITY, 0);
}

/// ra_288 and sipr are float decoders, bit-exact with FFmpeg 2da55bf once
/// the sums its build vectorizes round as it does (`codec_ra`'s `sums`).
#[test]
fn test_ra288_rm() {
    check_audio("real/ra_288.rm", f64::INFINITY, 0);
}

#[test]
fn test_ra288_ra() {
    check_audio("realaudio/ra4_288.ra", f64::INFINITY, 0);
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
    check_audio("sipr/sipr_5k0.rm", f64::INFINITY, 0);
}

#[test]
fn test_sipr_6k5() {
    check_audio("sipr/sipr_6k5.rm", f64::INFINITY, 0);
}

#[test]
fn test_sipr_8k5() {
    check_audio("sipr/sipr_8k5.rm", f64::INFINITY, 0);
}

#[test]
fn test_sipr_16k() {
    // The file is cut inside its last interleave block: the demuxer must
    // keep the bytes it has and zero only the missing tail, as FFmpeg does.
    let path = fate("sipr/sipr_16k.rm");
    let decoded = decode(
        &path,
        &[codec_ra::register, demux_rm::register],
        MediaType::Audio,
        0,
    );
    let got = interleaved_f32(&decoded);
    let ref_samples = ffmpeg_audio_f32(&path, 0);
    assert_eq!(got.len(), ref_samples.len(), "sipr/sipr_16k.rm: sample count");
    let snr = snr_db(&ref_samples, &got, 0);
    println!("sipr/sipr_16k.rm: SNR = {snr:.2} dB");
    assert_eq!(snr, f64::INFINITY, "sipr/sipr_16k.rm must be bit-exact");
}

#[test]
fn test_sipr_16k_ra() {
    check_audio("realaudio/RA5.0_16kbps_voice_wideband.ra", f64::INFINITY, 0);
}
