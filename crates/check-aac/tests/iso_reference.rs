//! ISO/IEC 23003-7 conformance PCM plus unmodified libxaac full raw PCM.
//! External, hash-pinned fixtures: data/iso-usac/README.md. Missing files fail.

use check_aac::{aac_decoder, channel_snr_db, decode_one, read_wav, usac_packets};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    std::env::var_os("ISO_USAC").map(PathBuf::from).unwrap_or_else(||
        PathBuf::from(std::env::var_os("HOME").unwrap()).join("projects/oracles/iso-usac"))
}

fn verified(path: &Path, md5: &str) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}; run tests/data/iso-usac/prepare.py", path.display()));
    assert_eq!(refcheck::md5_hex(&bytes), md5, "{}: immutable reference", path.display());
}

fn compare(label: &str, reference: &[f32], ours: &[f32], floors: &[f64]) {
    assert_eq!(reference.len(), ours.len(), "{label}: exact sample count");
    let values = channel_snr_db(reference, ours, 2, reference.len() / 2);
    eprintln!("{label}: {} frames, per-channel SNR {values:.6?} dB", reference.len() / 2);
    for (ch, (&actual, &floor)) in values.iter().zip(floors).enumerate() {
        assert!(actual >= floor, "{label} channel {ch}: {actual:.6} < {floor:.6} dB");
    }
}

#[test]
fn iso_fd_conformance() {
    // stem, AUs, edit-list skip, presented frames, MP4/ISO-WAV/native-WAV
    // MD5, followed by the two ISO and two native SNR floors.
    for line in include_str!("data/iso-usac/manifest.tsv").lines().filter(|s| !s.starts_with('#')) {
        let f: Vec<_> = line.split_whitespace().collect();
        let name = f[0];
        let aus: usize = f[1].parse().unwrap();
        let skip: usize = f[2].parse().unwrap();
        let frames: usize = f[3].parse().unwrap();
        let mp4 = root().join(format!("members/compressedMp4/{name}.mp4"));
        let iso = root().join(format!("members/referencesWav/{name}.wav"));
        let native = root().join(format!("libxaac-out/{name}.wav"));
        verified(&mp4, f[4]); verified(&iso, f[5]); verified(&native, f[6]);
        let (params, packets) = usac_packets(mp4.to_str().unwrap());
        assert_eq!(packets.len(), aus, "{name}: all untrimmed AUs");
        let counts = oxideav_aac::usac_tool_counts(&params, &packets).unwrap();
        eprintln!("{name}: {counts:?}");
        if name.contains("Cp_") {
            assert!(counts.complex_coef_frames > 0 && counts.imaginary_frames > 0 && counts.previous_frame_frames > 0, "{name}: actual complex prediction with nonzero imaginary coefficients and previous-frame MDST");
            if name.contains("Win") { assert!(counts.imaginary_short_frames > 0, "{name}: complex prediction in short windows"); }
        }
        let mut decoder = aac_decoder(&params);
        let ours: Vec<f32> = packets.iter().flat_map(|p| decode_one(&mut decoder, p).unwrap()).collect();
        assert_eq!(ours.len(), aus * 1024 * 2, "{name}: raw decoder output");
        let expected = read_wav(&iso);
        let native = read_wav(&native);
        let format = decoder.output_audio_format().unwrap();
        assert_eq!((expected.channels, expected.bits, expected.sample_rate), (2, 24, format.sample_rate));
        assert_eq!((native.channels, native.bits, native.sample_rate), (2, 24, format.sample_rate));
        assert_eq!((expected.frames, expected.declared_frames), (frames, frames));
        assert_eq!((native.frames, native.declared_frames), (aus * 1024, aus * 1024));
        let iso_floor = [f[7].parse().unwrap(), f[8].parse().unwrap()];
        let native_floor = [f[9].parse().unwrap(), f[10].parse().unwrap()];
        compare(&format!("{name} ISO presented"), &expected.samples, &ours[skip * 2..(skip + frames) * 2], &iso_floor);
        compare(&format!("{name} native raw"), &native.samples, &ours, &native_floor);
    }
}

#[test]
fn independent_window_tns_order() {
    // A separate equivalence vector, not a replacement for any canonical
    // input above. Native libxaac emits byte-identical WAVs for both forms.
    let name = "Fd_2_c1_WinTns_0x0c";
    let mp4 = root().join(format!("members/compressedMp4/{name}.mp4"));
    verified(&mp4, "f2d2e099426969624bfacfd14528bd85");
    let (params, original) = usac_packets(mp4.to_str().unwrap());
    let mut changed = original.clone();
    let mut flips = 0;
    for packet in &mut changed {
        let byte = &mut packet.data[0];
        assert_eq!(*byte & 0x60, 0, "two FD core_mode bits; this fixture has no extension element");
        if *byte & 0x18 == 0x10 { // tns_active=1, common_window=0
            assert_ne!(*byte & 4, 0, "original tns_on_lr=1");
            *byte ^= 4;
            flips += 1;
        }
    }
    assert_eq!(flips, 108);
    let counts = oxideav_aac::usac_tool_counts(&params, &changed).unwrap();
    assert_eq!(counts.tns_independent_not_on_lr, 130, "actually applied filters, not just a header bit");
    let native_path = root().join("libxaac-out/independent-tns.wav");
    let original_path = root().join(format!("libxaac-out/{name}.wav"));
    verified(&native_path, "45a2225bffabe7e07a44519d7ce62415");
    verified(&original_path, "45a2225bffabe7e07a44519d7ce62415");
    let native = read_wav(&native_path);
    let expected = read_wav(&original_path);
    assert_eq!(native.samples, expected.samples, "unmodified independent decoder: both syntax variants give identical PCM");
    let decode = |packets: &[oxideav_core::Packet]| {
        let mut decoder = aac_decoder(&params);
        packets.iter().flat_map(|p| decode_one(&mut decoder, p).unwrap()).collect::<Vec<_>>()
    };
    let ours = decode(&changed);
    assert_eq!(ours, decode(&original), "no stereo stage with independent windows: TNS still applies, regardless of tns_on_lr");
    compare("independent-window TNS raw", &native.samples, &ours, &[105.612463, 103.598594]);
}
