//! Verdicts of one captured stream against FFmpeg's decode of it.

use serde::Serialize;

use crate::oracle::{AudioFrameInfo, Pcm};

/// A stream's verdict. `Decodes` is not a pass: FFmpeg cannot decode the
/// format, so the stream played to the end with output but nothing
/// verified it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Verdict {
    #[serde(rename = "PASS")]
    Pass,
    #[serde(rename = "DECODES")]
    Decodes,
    #[serde(rename = "FAIL")]
    Fail,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Decodes => "DECODES",
            Verdict::Fail => "FAIL",
        }
    }
}

/// One stream's comparison.
#[derive(Debug)]
pub struct Compare {
    pub verdict: Verdict,
    pub metric: String,
    pub error: Option<String>,
}

impl Compare {
    pub fn pass(metric: impl Into<String>) -> Self {
        Compare { verdict: Verdict::Pass, metric: metric.into(), error: None }
    }

    pub fn decodes(metric: impl Into<String>) -> Self {
        Compare { verdict: Verdict::Decodes, metric: metric.into(), error: None }
    }

    pub fn fail(metric: impl Into<String>, error: impl Into<String>) -> Self {
        Compare { verdict: Verdict::Fail, metric: metric.into(), error: Some(error.into()) }
    }
}

// ---------------------------------------------------------------- audio

/// One sample of `pcm` back in the canonical encoding FFmpeg writes for
/// `format`: the inverse of the engine's contracted conversion to f32
/// (`s16 / 32768`, `s32 / 2^31`, `(u8 - 128) / 128`, floats as is). `None`
/// when `x` is not on that format's grid, i.e. no sample of `format`
/// converts to it.
fn canonical_sample(x: f32, format: Pcm, out: &mut Vec<u8>) -> Option<()> {
    let on_grid = |v: f64, lo: f64, hi: f64| (v.fract() == 0.0 && (lo..=hi).contains(&v)).then_some(v);
    match format {
        Pcm::U8 => out.push(on_grid(x as f64 * 128.0 + 128.0, 0.0, 255.0)? as u8),
        Pcm::S16 => out.extend((on_grid(x as f64 * 32768.0, -32768.0, 32767.0)? as i16).to_le_bytes()),
        Pcm::S32 => {
            out.extend((on_grid(x as f64 * 2147483648.0, -2147483648.0, 2147483647.0)? as i32).to_le_bytes())
        }
        Pcm::F32 => out.extend(x.to_le_bytes()),
        Pcm::F64 => out.extend((x as f64).to_le_bytes()),
    }
    Some(())
}

/// Whether every sample of FFmpeg's canonical PCM survives the engine's
/// conversion to f32 unchanged, so that equal f32 output proves equal
/// integer output. 32-bit integers keep 24 significant bits in an f32, and
/// doubles lose their low mantissa bits.
fn representable_in_f32(reference: &[u8], format: Pcm) -> Result<(), String> {
    match format {
        Pcm::S32 => {
            for (i, b) in reference.chunks_exact(4).enumerate() {
                let v = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                if (v as f32) as f64 != v as f64 {
                    return Err(format!(
                        "FFmpeg's sample {i} ({v}) has more than 24 significant bits: the player's f32 output \
                         cannot carry it, so its exactness is unverifiable"
                    ));
                }
            }
        }
        Pcm::F64 => {
            for (i, b) in reference.chunks_exact(8).enumerate() {
                let v = f64::from_le_bytes(b.try_into().unwrap());
                if (v as f32) as f64 != v {
                    return Err(format!(
                        "FFmpeg's sample {i} ({v}) is not an f32: the player's f32 output cannot carry it"
                    ));
                }
            }
        }
        Pcm::U8 | Pcm::S16 | Pcm::F32 => {}
    }
    Ok(())
}

/// `audio:md5`: the player's PCM, back in FFmpeg's canonical encoding of the
/// decoder's sample format, must be byte-identical to FFmpeg's, with the same
/// sample count. Returns the metric, or why it differs.
pub fn exact_pcm(ours: &[f32], reference: &[u8], format: Pcm, channels: usize) -> Result<String, String> {
    let w = format.bytes();
    if reference.is_empty() {
        return Err("FFmpeg decoded no samples".into());
    }
    if reference.len() % w != 0 {
        return Err(format!("FFmpeg's PCM is {} bytes, not whole {w}-byte samples", reference.len()));
    }
    representable_in_f32(reference, format)?;
    let mut bytes = Vec::with_capacity(ours.len() * w);
    for (i, &x) in ours.iter().enumerate() {
        if canonical_sample(x, format, &mut bytes).is_none() {
            return Err(format!("sample {i} ({x}) is not a {format:?} value: the decode is not exact"));
        }
    }
    let per_frame = channels.max(1);
    let (got, want) = (ours.len() / per_frame, reference.len() / w / per_frame);
    let (md5_got, md5_want) = (refcheck::md5_hex(&bytes), refcheck::md5_hex(reference));
    if bytes == reference {
        return Ok(format!("samples={got} md5={md5_got}"));
    }
    let mismatch = bytes.chunks(w).zip(reference.chunks(w)).position(|(a, b)| a != b);
    Err(match mismatch {
        Some(i) => format!(
            "md5 {md5_got} vs FFmpeg {md5_want}: first difference at sample {} (channel {}); {got} vs {want} samples",
            i / per_frame,
            i % per_frame
        ),
        None => format!("md5 {md5_got} vs FFmpeg {md5_want}: {got} samples vs FFmpeg {want} (the common part is equal)"),
    })
}

/// How many interleaved samples a lossy decode may run long or short: one
/// frame of FFmpeg's decoder for this stream (its largest), times the
/// channel count.
pub fn lossy_slack(frames: &[AudioFrameInfo], channels: u16) -> Result<usize, String> {
    let largest = frames.iter().map(|f| f.nb_samples).max().ok_or("FFmpeg decoded no frames")?;
    if let Some(f) = frames.iter().find(|f| f.channels != channels) {
        return Err(format!("FFmpeg decodes {} channels, the player {channels}", f.channels));
    }
    Ok(largest as usize * channels as usize)
}

/// `audio:snr:<floor>`: SNR over the common length, lengths within `slack`
/// samples, accepted only when `snr >= floor` (+infinity passes, -infinity
/// and NaN do not).
pub fn snr_pcm(ours: &[f32], reference: &[f32], slack: usize, floor: f64) -> Compare {
    match refcheck::try_snr_db(reference, ours, slack) {
        Ok(snr) if snr >= floor => Compare::pass(format!("snr={snr:.1} dB")),
        Ok(snr) => Compare::fail(format!("snr={snr:.1} dB"), format!("SNR {snr:.1} dB below the {floor} dB floor")),
        Err(e) => Compare::fail("snr=n/a", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s16(samples: &[i16]) -> (Vec<f32>, Vec<u8>) {
        let f = samples.iter().map(|&s| s as f32 / 32768.0).collect();
        let b = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        (f, b)
    }

    fn tone(n: usize) -> Vec<i16> {
        (0..n).map(|i| ((i as f64 * 0.05).sin() * 20000.0) as i16).collect()
    }

    #[test]
    fn exact_pcm_accepts_identical_samples() {
        let (ours, reference) = s16(&tone(48000));
        assert!(exact_pcm(&ours, &reference, Pcm::S16, 1).is_ok());
    }

    #[test]
    fn one_lsb_error_passes_the_old_120_db_floor_but_not_md5() {
        let samples = tone(48000);
        let (_, reference) = s16(&samples);
        let mut wrong = samples.clone();
        wrong[1000] += 1;
        let (ours, _) = s16(&wrong);
        let (ref_f32, _) = s16(&samples);
        let snr = refcheck::try_snr_db(&ref_f32, &ours, 0).unwrap();
        assert!(snr >= 120.0, "the old audio:md5 stand-in accepted this: {snr} dB");
        let err = exact_pcm(&ours, &reference, Pcm::S16, 1).unwrap_err();
        assert!(err.contains("first difference at sample 1000"), "{err}");
    }

    #[test]
    fn missing_tail_samples_fail_md5() {
        let samples = tone(48000);
        let (ours, reference) = s16(&samples);
        // The old slack (rate / 10 + 2048 = 6848 samples) scored only the
        // common prefix, so this passed as +infinity.
        let short = &ours[..48000 - 6000];
        let (ref_f32, _) = s16(&samples);
        assert_eq!(refcheck::try_snr_db(&ref_f32, short, 48000 / 10 + 2048), Ok(f64::INFINITY));
        let err = exact_pcm(short, &reference, Pcm::S16, 1).unwrap_err();
        assert!(err.contains("42000 samples vs FFmpeg 48000"), "{err}");
    }

    #[test]
    fn exact_pcm_rejects_output_off_the_integer_grid() {
        let (mut ours, reference) = s16(&tone(100));
        ours[3] += 1e-6;
        let err = exact_pcm(&ours, &reference, Pcm::S16, 1).unwrap_err();
        assert!(err.contains("sample 3"), "{err}");
    }

    #[test]
    fn exact_pcm_refuses_32_bit_samples_an_f32_cannot_carry() {
        // 24-bit content in s32 (TrueHD, FLAC-24) is exact in f32 ...
        let v24: Vec<i32> = vec![0x7fff_ff00, i32::MIN, 0x1234_5600];
        let bytes: Vec<u8> = v24.iter().flat_map(|v| v.to_le_bytes()).collect();
        let ours: Vec<f32> = v24.iter().map(|&v| v as f32 / 2147483648.0).collect();
        assert!(exact_pcm(&ours, &bytes, Pcm::S32, 1).is_ok());
        // ... a 32-bit sample is not: f32 equality would not prove it.
        let v32 = [0x1234_5679i32];
        let bytes: Vec<u8> = v32.iter().flat_map(|v| v.to_le_bytes()).collect();
        let ours: Vec<f32> = v32.iter().map(|&v| v as f32 / 2147483648.0).collect();
        let err = exact_pcm(&ours, &bytes, Pcm::S32, 1).unwrap_err();
        assert!(err.contains("unverifiable"), "{err}");
    }

    #[test]
    fn exact_pcm_needs_a_reference() {
        assert!(exact_pcm(&[0.0], &[], Pcm::S16, 1).is_err());
    }

    #[test]
    fn snr_pcm_accepts_only_snr_at_or_above_the_floor() {
        let silence = [0.0f32; 64];
        let noise: Vec<f32> = (0..64).map(|i| if i % 2 == 0 { 0.01 } else { -0.01 }).collect();
        // A silent reference against noise scores -infinity: the old
        // `snr.is_infinite() || snr >= floor` accepted it.
        assert_eq!(snr_pcm(&noise, &silence, 0, 90.0).verdict, Verdict::Fail);
        assert_eq!(snr_pcm(&silence, &silence, 0, 90.0).verdict, Verdict::Pass);
        assert_eq!(snr_pcm(&[], &silence, 64, 90.0).verdict, Verdict::Fail, "empty decode");
        assert_eq!(snr_pcm(&silence, &[], 64, 90.0).verdict, Verdict::Fail, "empty reference");
    }

    #[test]
    fn lossy_slack_is_one_decoder_frame_of_every_channel() {
        let frames = vec![AudioFrameInfo { nb_samples: 1024, channels: 1 }; 10];
        // 48 kHz mono AAC: the old allowance was 48000 / 10 + 2048 = 6848.
        assert_eq!(lossy_slack(&frames, 1), Ok(1024));
        let stereo = vec![AudioFrameInfo { nb_samples: 1152, channels: 2 }; 3];
        assert_eq!(lossy_slack(&stereo, 2), Ok(2304));
        assert!(lossy_slack(&stereo, 1).is_err(), "channel count disagrees");
        assert!(lossy_slack(&[], 2).is_err());
    }
}
