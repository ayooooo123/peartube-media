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
