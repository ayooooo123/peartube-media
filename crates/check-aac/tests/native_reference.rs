//! Independent libxaac references for paths where FFmpeg is not an oracle.
//! Reproduction, licensing and SHA-256 manifests: data/libxaac/README.md.

use check_aac::{aac_decoder, channel_snr_db, data_path, decode_one, read_wav, usac_packets};

fn assert_snr(label: &str, reference: &[f32], ours: &[f32], channels: usize, floors: &[f64]) {
    assert_eq!(ours.len(), reference.len(), "{label}: every sample");
    let snr = channel_snr_db(reference, ours, channels, reference.len() / channels);
    eprintln!("{label}: {} frames, per-channel SNR {snr:.6?} dB", reference.len() / channels);
    for (ch, (&actual, &floor)) in snr.iter().zip(floors).enumerate() {
        assert!(actual >= floor, "{label} channel {ch}: {actual:.6} < {floor:.6} dB");
    }
}

#[test]
fn canonical_xhe_native_reference() {
    let rel = "aac/usac/xhe_target_level.m4a";
    let path = refcheck::fate(rel);
    assert_eq!(refcheck::md5_hex(&std::fs::read(&path).unwrap()), "137894e06a6eb8637a67d503fe6cb06b", "unmodified canonical input");
    let (params, packets) = usac_packets(rel);
    assert_eq!(packets.len(), 48);
    let mut es = params.extradata.clone();
    for packet in &packets { es.extend_from_slice(&packet.data); }
    assert_eq!(refcheck::md5_hex(&es), "90e57f6ccfa8e8080e7d8926af3eb71c", "production demuxer emits the exact native-oracle ASC and AUs");

    // Floors: measured on predecessor 02ed7a8, minus 0.5 dB per channel.
    // No alignment search, gain fitting, pre-roll removal or startup exclusion.
    for (target, name, md5, raw_floor, presented_floor) in [
        (0, "xhe_target_level.wav", "97dd3365cf2ce89a73e2c60717d6abbd",
            [114.634525, 114.880771], [114.646389, 114.894712]),
        (-24, "xhe_target_level.t-24.wav", "c968666c639b5791b9c08cd659335705",
            [111.535669, 111.689522], [111.589333, 111.745927]),
    ] {
        let path = data_path(&format!("libxaac/{name}"));
        assert_eq!(refcheck::md5_hex(&std::fs::read(&path).unwrap()), md5, "immutable native PCM");
        let reference = read_wav(&path);
        assert_eq!((reference.channels, reference.sample_rate, reference.bits), (2, 48000, 24));
        assert_eq!((reference.frames, reference.declared_frames), (49152, 50176), "native WAV's overdeclared header is not zero padding");
        let mut configured = params.clone();
        configured.options.insert("target_level", target.to_string());
        let mut decoder = aac_decoder(&configured);
        let ours: Vec<f32> = packets.iter().flat_map(|p| decode_one(&mut decoder, p).unwrap()).collect();
        assert_eq!(ours.len(), 48 * 1024 * 2, "raw output retains the final AU outside the edit list");
        assert_snr(&format!("xhe target {target} raw"), &reference.samples, &ours, 2, &raw_floor);
        let presented = 48000 * 2; // elst media_time=0, duration=48000, timescale=48000.
        assert_eq!(ours.len() - presented, (1024 + 128) * 2);
        assert_snr(&format!("xhe target {target} presented"), &reference.samples[..presented], &ours[..presented], 2, &presented_floor);
    }
}

