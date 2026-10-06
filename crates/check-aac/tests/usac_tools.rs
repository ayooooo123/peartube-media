//! USAC tool coverage over the conformance corpus, and AudioPreRoll priming
//! with real access units.
//!
//! Only `xhe_target_level` carries AudioPreRoll (AU0, one pre-roll AU). The
//! seek and reconfiguration cases rebuild real conformance AUs into AudioPreRoll
//! syntax: `Ext_2_c1_Ln_0x03` has no noise filling and every AU is
//! independent, so a fresh decoder primed with AU k-1 must reproduce
//! continuous decoding of AU k bit for bit.

use check_aac::{aac_decoder, decode_one, usac_packets, USAC_SAMPLES};
use oxideav_core::{Packet, TimeBase};

fn bits(data: &[u8]) -> Vec<bool> {
    data.iter().flat_map(|&b| (0..8).rev().map(move |i| b >> i & 1 == 1)).collect()
}

fn bytes(bits: &[bool]) -> Vec<u8> {
    bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |a, (i, &b)| a | (u8::from(b) << (7 - i)))).collect()
}

fn push(out: &mut Vec<bool>, value: u32, n: u32) {
    out.extend((0..n).rev().map(|i| value >> i & 1 == 1));
}

fn take(b: &[bool], pos: &mut usize, n: usize) -> u32 {
    let value = b[*pos..*pos + n].iter().fold(0, |a, &x| a << 1 | u32::from(x));
    *pos += n;
    value
}

fn push_escaped(out: &mut Vec<bool>, value: u32, a: u32, b: u32, c: u32) {
    let first = (1 << a) - 1;
    if value < first {
        return push(out, value, a);
    }
    push(out, first, a);
    let rest = value - first;
    let second = (1 << b) - 1;
    if c == 0 || rest < second {
        push(out, rest, b);
    } else {
        push(out, second, b);
        push(out, rest - second, c);
    }
}

fn take_escaped(b: &[bool], pos: &mut usize, a: usize, bb: usize, c: usize) -> u32 {
    let mut value = take(b, pos, a);
    if value == (1 << a) - 1 {
        let second = take(b, pos, bb);
        value += second;
        if c != 0 && second == (1 << bb) - 1 {
            value += take(b, pos, c);
        }
    }
    value
}

fn packet(data: Vec<u8>) -> Packet {
    Packet::new(0, TimeBase::new(1, 48000), data)
}

/// An AOT-42 ASC with an AudioPreRoll extension element inserted first, and
/// that UsacConfig() alone (the AudioPreRoll Config() payload).
fn with_preroll_element(asc: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let a = bits(asc);
    let mut p = 0;
    assert_eq!(take(&a, &mut p, 11), (31 << 6) | 10, "AOT 42");
    if take(&a, &mut p, 4) == 15 {
        p += 24;
    }
    p += 4;
    let usac_start = p;
    if take(&a, &mut p, 5) == 31 {
        p += 24;
    }
    p += 3;
    assert_ne!(take(&a, &mut p, 5), 0, "corpus layouts are indexed");
    let count_start = p;
    let count = take_escaped(&a, &mut p, 4, 8, 16);
    let mut usac = a[usac_start..count_start].to_vec();
    push_escaped(&mut usac, count + 1, 4, 8, 16);
    // usacElementType EXT, AudioPreRoll, no config, no default length, unfragmented.
    push(&mut usac, 3, 2);
    push_escaped(&mut usac, 3, 4, 8, 16);
    push_escaped(&mut usac, 0, 4, 8, 16);
    push(&mut usac, 0, 2);
    usac.extend_from_slice(&a[p..]);
    let mut full = a[..usac_start].to_vec();
    full.extend_from_slice(&usac);
    (bytes(&full), bytes(&usac))
}

/// An AU for the AudioPreRoll-first configuration, without a pre-roll payload.
fn absent(au: &[u8]) -> Vec<u8> {
    let b = bits(au);
    let mut out = vec![b[0], false];
    out.extend_from_slice(&b[1..]);
    bytes(&out)
}

/// An immediate-playout AU: `au` with AudioPreRoll(config, units).
fn ipf(au: &[u8], config: &[u8], units: &[&[u8]]) -> Vec<u8> {
    let mut payload = Vec::new();
    push_escaped(&mut payload, config.len() as u32, 4, 4, 8);
    config.iter().for_each(|&byte| push(&mut payload, byte.into(), 8));
    push(&mut payload, 0, 2); // applyCrossfade, reserved
    push_escaped(&mut payload, units.len() as u32, 2, 4, 0);
    for unit in units {
        push_escaped(&mut payload, unit.len() as u32, 16, 16, 0);
        unit.iter().for_each(|&byte| push(&mut payload, byte.into(), 8));
    }
    let payload = bytes(&payload);
    let b = bits(au);
    let mut out = vec![b[0], true, false];
    if payload.len() < 255 {
        push(&mut out, payload.len() as u32, 8);
    } else {
        push(&mut out, 255, 8);
        push(&mut out, payload.len() as u32 - 253, 16);
    }
    payload.iter().for_each(|&byte| push(&mut out, byte.into(), 8));
    out.extend_from_slice(&b[1..]);
    bytes(&out)
}

fn decode_all(params: &oxideav_core::CodecParameters, units: &[Vec<u8>]) -> Vec<Vec<f32>> {
    let mut decoder = aac_decoder(params);
    units.iter().map(|u| decode_one(&mut decoder, &packet(u.clone())).unwrap()).collect()
}

fn max_error(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
}

/// Which USAC tools each conformance file exercises, pinned so the README's
/// coverage statements stay true. Columns: frames, AudioPreRoll payloads,
/// pre-roll AUs decoded, payloads skipped, config changes, short-window
/// channels, noise-filled channels, TNS channels, unapplied TNS channels,
/// M/S pairs, complex-prediction pairs, `complex_coef = 1` pairs and
/// `use_prev_frame = 1` pairs. No file exercises `complex_coef = 1`,
/// `use_prev_frame = 1` or TNS with `common_window = 0` and `tns_on_lr = 0`:
/// those FFmpeg-mirrored paths have no oracle here.
#[test]
fn usac_tool_coverage() {
    let expected: [[u64; 13]; 9] = [
        [2587, 0, 0, 0, 0, 0, 0, 0, 0, 2528, 0, 0, 0],
        [1295, 0, 0, 0, 0, 0, 0, 0, 0, 1270, 0, 0, 0],
        [940, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [864, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [940, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [628, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [1295, 0, 0, 0, 0, 0, 0, 1814, 0, 0, 0, 0, 0],
        [96, 0, 0, 0, 0, 190, 0, 0, 0, 0, 0, 0, 0],
        [48, 1, 1, 0, 0, 4, 98, 7, 0, 0, 2, 0, 0],
    ];
    for (&(rel, ..), expected) in USAC_SAMPLES.iter().zip(expected) {
        let (params, packets) = usac_packets(rel);
        let c = oxideav_aac::usac_tool_counts(&params, &packets).unwrap();
        eprintln!("{rel}: {c:?}");
        let actual = [
            c.frames, c.preroll_payloads, c.preroll_decoded, c.preroll_skipped, c.config_changes,
            c.short_window_channels, c.noise_filled_channels, c.tns_channels, c.tns_unapplied,
            c.ms_frames, c.prediction_frames, c.complex_coef_frames, c.previous_frame_frames,
        ];
        assert_eq!(actual, expected, "{rel}");
        assert_eq!(c.frames, packets.len() as u64, "{rel}");
    }
}

/// Priming mechanics on the only real AudioPreRoll stream: production output
/// must equal continuous decoding of the encoder's pre-roll AU followed by the
/// stream. This checks our decoder against itself; it is not an oracle for
/// standards-correct output. The SNR against FFmpeg (which never primes) is
/// printed for the record; the unmodified comparison in `reference.rs` is the
/// acceptance check.
#[test]
fn usac_xhe_primed_production_output() {
    let rel = "aac/usac/xhe_target_level.m4a";
    let (params, packets) = usac_packets(rel);
    let mut production = aac_decoder(&params);
    let primed: Vec<Vec<f32>> = packets.iter().map(|p| decode_one(&mut production, p).unwrap()).collect();
    let b = bits(&packets[0].data);
    let mut p = 3;
    let length = take(&b, &mut p, 8) as usize;
    let payload = &b[p..p + length * 8];
    let mut q = 0;
    let config_length = take_escaped(payload, &mut q, 4, 4, 8) as usize;
    q += config_length * 8 + 2;
    assert_eq!(take_escaped(payload, &mut q, 2, 4, 0), 1);
    let unit_length = take_escaped(payload, &mut q, 16, 16, 0) as usize;
    let mut continuous = aac_decoder(&params);
    decode_one(&mut continuous, &packet(bytes(&payload[q..q + unit_length * 8]))).unwrap();
    for (i, frame) in primed.iter().enumerate() {
        assert_eq!(&decode_one(&mut continuous, &packets[i]).unwrap(), frame, "AU {i}");
    }
    let ours: Vec<f32> = primed.concat();
    let ff = refcheck::ffmpeg_audio_f32(&refcheck::fate(rel), 0);
    let presented = &ours[..ff.len()];
    let snr = refcheck::snr_db(&ff, presented, 0);
    let first = refcheck::snr_db(&ff[..2048], &presented[..2048], 0);
    eprintln!("{rel}: primed production vs FFmpeg (unprimed): SNR {snr:.6} dB, AU0 {first:.6} dB");
}

#[test]
fn usac_preroll_priming() {
    // Real stream: xhe AU0 is an IPF with one pre-roll AU.
    let (xhe_params, xhe) = usac_packets("aac/usac/xhe_target_level.m4a");
    let counts = oxideav_aac::usac_tool_counts(&xhe_params, &xhe).unwrap();
    assert_eq!((counts.preroll_payloads, counts.preroll_decoded, counts.preroll_skipped), (1, 1, 0));
    let au0 = bits(&xhe[0].data);
    let mut p = 0;
    assert_eq!(take(&au0, &mut p, 3), 0b110, "independent, pre-roll present, explicit length");
    let length = take(&au0, &mut p, 8) as usize;
    assert!(length < 255);
    let payload = &au0[p..p + length * 8];
    let mut q = 0;
    let config_length = take_escaped(payload, &mut q, 4, 4, 8) as usize;
    q += config_length * 8 + 2;
    assert_eq!(take_escaped(payload, &mut q, 2, 4, 0), 1);
    let unit_length = take_escaped(payload, &mut q, 16, 16, 0) as usize;
    let unit = bytes(&payload[q..q + unit_length * 8]);
    // Continuous decoding of [pre-roll AU, AU0] skips AU0's payload, so its
    // second frame is the fresh, primed AU0 frame.
    let primed = decode_one(&mut aac_decoder(&xhe_params), &xhe[0]).unwrap();
    let mut continuous = aac_decoder(&xhe_params);
    let priming = decode_one(&mut continuous, &packet(unit.clone())).unwrap();
    assert_eq!(decode_one(&mut continuous, &xhe[0]).unwrap(), primed);
    let peak = priming.iter().fold(0f32, |m, v| m.max(v.abs()));
    eprintln!("xhe AU0 pre-roll AU: {unit_length} bytes, decoded peak {peak:e}");

    // Seeks into a real stream rebuilt with AudioPreRoll.
    let (params, packets) = usac_packets("aac/usac/Ext_2_c1_Ln_0x03.mp4");
    let (asc, usac) = with_preroll_element(&params.extradata);
    let mut preroll_params = params.clone();
    preroll_params.extradata = asc;
    let original: Vec<Vec<u8>> = packets.iter().map(|p| p.data.clone()).collect();
    let rewritten: Vec<Vec<u8>> = original.iter().map(|au| absent(au)).collect();
    let continuous = decode_all(&preroll_params, &rewritten);
    assert_eq!(continuous, decode_all(&params, &original), "an absent pre-roll changes nothing");
    for k in [17, 48, 80] {
        let seek = ipf(&original[k], &usac, &[&rewritten[k - 1]]);
        let mut fresh = aac_decoder(&preroll_params);
        let primed = decode_one(&mut fresh, &packet(seek.clone())).unwrap();
        assert_eq!(primed, continuous[k], "seek to AU {k}: primed frame");
        assert_eq!(decode_one(&mut fresh, &packet(rewritten[k + 1].clone())).unwrap(), continuous[k + 1]);
        let unprimed = decode_one(&mut aac_decoder(&preroll_params), &packet(rewritten[k].clone())).unwrap();
        let error = max_error(&unprimed, &continuous[k]);
        assert!(error > 1e-3, "AU {k}: priming must be audible, unprimed error {error:e}");
        fresh.reset().unwrap();
        assert_eq!(decode_one(&mut fresh, &packet(seek.clone())).unwrap(), primed, "AU {k}: after reset");
        // Continuous decoding skips the same payload (7.18.3.3).
        let mut stream = aac_decoder(&preroll_params);
        for au in &rewritten[..k] {
            decode_one(&mut stream, &packet(au.clone())).unwrap();
        }
        assert_eq!(decode_one(&mut stream, &packet(seek.clone())).unwrap(), continuous[k], "AU {k}: continuous");
        let units: Vec<Packet> = rewritten[..k].iter().cloned().chain([seek]).map(packet).collect();
        let counts = oxideav_aac::usac_tool_counts(&preroll_params, &units).unwrap();
        assert_eq!((counts.preroll_payloads, counts.preroll_decoded, counts.preroll_skipped), (1, 0, 1));
    }

    // A pre-roll configuration change: mono Fd_1_c1_0x03 switches to stereo.
    let (mono_params, mono) = usac_packets("aac/usac/Fd_1_c1_0x03.mp4");
    let mut start = mono_params.clone();
    start.extradata = with_preroll_element(&mono_params.extradata).0;
    let mut decoder = aac_decoder(&start);
    for au in &mono[..3] {
        assert_eq!(decode_one(&mut decoder, &packet(absent(&au.data))).unwrap().len(), 1024);
    }
    let k = 48;
    let switch = ipf(&original[k], &usac, &[&rewritten[k - 1]]);
    assert_eq!(decode_one(&mut decoder, &packet(switch.clone())).unwrap(), continuous[k]);
    let format = decoder.output_audio_format().unwrap();
    assert_eq!((format.channels, format.sample_rate), (2, 48000));
    assert_eq!(decode_one(&mut decoder, &packet(rewritten[k + 1].clone())).unwrap(), continuous[k + 1]);
    let units: Vec<Packet> = mono[..3].iter().map(|p| absent(&p.data)).chain([switch]).map(packet).collect();
    let counts = oxideav_aac::usac_tool_counts(&start, &units).unwrap();
    assert_eq!((counts.config_changes, counts.preroll_decoded), (1, 1));

    // An unsupported embedded configuration is rejected and emits nothing.
    let mut bad = bits(&usac);
    bad[5..8].copy_from_slice(&[false, true, false]); // coreSbrFrameLengthIndex 2 (eSBR)
    let mut decoder = aac_decoder(&preroll_params);
    let error = decoder.send_packet(&packet(ipf(&original[k], &bytes(&bad), &[&rewritten[k - 1]]))).unwrap_err();
    assert!(error.to_string().contains("1024-line"), "{error}");
    assert!(matches!(decoder.receive_frame(), Err(oxideav_core::Error::NeedMore)));
}
