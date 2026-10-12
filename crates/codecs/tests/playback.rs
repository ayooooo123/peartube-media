//! Exercise the app's combined registry, not a codec's isolated registrar.
use oxideav_core::MediaType;

#[test]
fn wma2_playback_matches_ffmpeg_with_all_codecs_registered() {
    let path = refcheck::fate("cover_art/Californication_cover.wma");
    let decoded = refcheck::decode(&path, &[codecs::register_all], MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(2));
    assert_eq!(decoded.params.sample_rate, Some(44_100));
    let ours = refcheck::interleaved_f32(&decoded);
    let reference = refcheck::ffmpeg_audio_f32(&path, 0);
    // WMA2 at 44.1 kHz has 2048 samples/channel per frame.
    let snr = refcheck::try_snr_db(&reference, &ours, 2048 * 2)
        .expect("complete WMA2 playback within one codec frame");
    assert!(snr >= 90.0, "combined registry WMA2 SNR {snr} dB < 90 dB");
}

#[test]
fn wma_in_avi_matches_ffmpeg_through_production_registry() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../target/e2e/wma-avi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for codec in ["wmav1", "wmav2"] {
        let path = dir.join(format!("{codec}.avi"));
        let generated = std::process::Command::new(refcheck::system_ffmpeg())
            .args(["-v", "error", "-y", "-f", "lavfi", "-i",
                "sine=frequency=997:sample_rate=44100:duration=2",
                "-ac", "2", "-c:a", codec, "-b:a", "128k"])
            .arg(&path).output().unwrap();
        assert!(generated.status.success(), "fixture generation: {}",
            String::from_utf8_lossy(&generated.stderr));
        let mut bytes = std::fs::read(&path).unwrap();
        // The current AVI demuxer requires variable-size WMA chunks to have
        // dwSampleSize=0; FFmpeg writes a nonzero value. Preserve that existing
        // restriction while exercising the newly selected WMA decoder.
        let strh = bytes.windows(12)
            .position(|b| &b[..4] == b"strh" && &b[8..12] == b"auds")
            .expect("generated audio stream header");
        bytes[strh + 52..strh + 56].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        let decoded = refcheck::decode(&path, &[codecs::register_all], MediaType::Audio, 0);
        let ours = refcheck::interleaved_f32(&decoded);
        let reference = refcheck::ffmpeg_audio_f32(&path, 0);
        let snr = refcheck::try_snr_db(&reference, &ours, 2048 * 2)
            .expect("complete AVI WMA playback within one codec frame");
        assert!(snr >= 90.0, "{codec} AVI SNR {snr} dB < 90 dB");
    }
}
