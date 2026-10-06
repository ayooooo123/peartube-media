use check_aac::decoded_f32;

#[test]
fn snr_report() {
    for rel in [
        "aac/latm_000000001180bc60.mpg", "aac/latm_stereo_to_51.ts",
        ] {
        let (ours, path, ch) = decoded_f32(rel);
        let ff = refcheck::ffmpeg_audio_f32(&path, 0);
        let outcome = if ff.is_empty() || ours.is_empty() {
            "empty".to_string()
        } else if ff.len().abs_diff(ours.len()) <= 4096 {
            format!("{:.2} dB", refcheck::snr_db(&ff, &ours, 4096))
        } else {
            let n = ff.len().min(ours.len());
            format!(
                "len-mismatch ours={} ff={} prefix-snr={:.2}",
                ours.len(),
                ff.len(),
                refcheck::snr_db(&ff[..n], &ours[..n], 4096)
            )
        };
        println!("SNR {rel}: ch={ch} {outcome}");
    }
}
