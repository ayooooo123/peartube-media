//! oxideav-ac3 against the FFmpeg its decoder is a port of (commit 2da55bf,
//! `refcheck::pinned_ffmpeg`, C code paths via `-cpuflags 0`), fed the
//! packets that FFmpeg's demuxer and parser give its own decoder.

use check_decoders::{decode_packets, pinned_ffmpeg_packets, tool};
use oxideav_core::{AudioFormat, CodecId, CodecParameters, Frame, SampleFormat};

/// Every AC-3 / E-AC-3 stream of the FATE suite's `ac3/` and `eac3/`
/// samples.
const SAMPLES: &[(&str, &str)] = &[
    ("ac3/millers_crossing_4.0.ac3", "a:0"),
    ("ac3/monsters_inc_2.0_192_small.ac3", "a:0"),
    ("ac3/monsters_inc_5.1_448_small.ac3", "a:0"),
    ("ac3/diatonis_invisible_order_anfos_ac3-small.wav", "a:0"),
    ("ac3/mp3ac325-4864-small.ts", "a:0"),
    ("ac3/mp3ac325-4864-small.ts", "a:1"),
    ("eac3/csi_miami_5.1_256_spx_small.eac3", "a:0"),
    ("eac3/csi_miami_stereo_128_spx.eac3", "a:0"),
    ("eac3/csi_miami_stereo_128_spx_small.eac3", "a:0"),
    ("eac3/matrix2_commentary1_stereo_192_small.eac3", "a:0"),
    ("eac3/serenity_english_5.1_1536_small.eac3", "a:0"),
    ("eac3/the_great_wall_7.1.eac3", "a:0"),
];

/// FFmpeg's frames for stream `spec`: (samples per channel, channels).
fn pinned_frames(path: &str, spec: &str) -> Vec<(u32, u16)> {
    let ffprobe = refcheck::pinned_ffmpeg().with_file_name("ffprobe");
    let out = tool(
        &ffprobe,
        &["-v", "error", "-cpuflags", "0", "-select_streams", spec, "-show_entries", "frame=nb_samples,channels", "-of", "csv=p=0", path],
    );
    String::from_utf8(out)
        .expect("ffprobe output is UTF-8")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            // nb_samples,channels, then an empty side-data column
            let mut fields = l.split(',').map(str::trim);
            let mut next =
                |what| fields.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or_else(|| panic!("{what} in {l:?}"));
            (next("nb_samples"), next("channels") as u16)
        })
        .collect()
}

/// FFmpeg's decode of stream `spec`, interleaved f32.
fn pinned_pcm(path: &str, spec: &str) -> Vec<f32> {
    let map = format!("0:{spec}");
    let out = tool(
        refcheck::pinned_ffmpeg(),
        &["-v", "error", "-nostdin", "-cpuflags", "0", "-i", path, "-map", &map, "-f", "f32le", "-c:a", "pcm_f32le", "-"],
    );
    out.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

/// How many packets of stream `spec` FFmpeg's decoder refuses.
fn pinned_decode_errors(path: &str, spec: &str) -> usize {
    let map = format!("0:{spec}");
    let out = std::process::Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0", "-i", path, "-map", &map, "-f", "null", "-"])
        .output()
        .expect("pinned ffmpeg runs");
    assert!(out.status.success(), "{path}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stderr).matches("Error submitting packet to decoder").count()
}

/// Every frame interleaved, read in the layout the decoder reported for it
/// (planar float).
fn interleave(frames: &[Frame], formats: &[Option<AudioFormat>]) -> Vec<f32> {
    let mut pcm = Vec::new();
    for (frame, format) in frames.iter().zip(formats) {
        let Frame::Audio(audio) = frame else { panic!("not an audio frame") };
        let format = format.expect("a frame without a reported layout");
        assert_eq!(format.sample_format, SampleFormat::F32P);
        let planes: Vec<Vec<f32>> = audio
            .data
            .iter()
            .map(|p| p.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
            .collect();
        assert_eq!(planes.len(), usize::from(format.channels));
        for i in 0..audio.samples as usize {
            pcm.extend(planes.iter().map(|p| p[i]));
        }
    }
    pcm
}

/// Each stream decodes to FFmpeg's frames (same count, sizes and channel
/// counts), refuses as many packets as FFmpeg's decoder does
/// (`csi_miami_stereo_128_spx` opens with a packet holding no sync word),
/// and decodes to FFmpeg's samples within 90 dB SNR. The old decoder
/// (OxideAV 9922883) refused `monsters_inc_2.0`'s cut last frame and was
/// 34 dB from FFmpeg on `millers_crossing` (S16 output, its own dither).
#[test]
fn fate_streams_decode_to_ffmpegs_frames_and_samples() {
    let mut failures = Vec::new();
    for &(sample, spec) in SAMPLES {
        let path = refcheck::fate(sample);
        let p = path.to_str().expect("UTF-8 path");
        let packets = pinned_ffmpeg_packets(&path, spec);
        let codec = if sample.starts_with("eac3/") { "eac3" } else { "ac3" };
        let params = CodecParameters::audio(CodecId::new(codec));
        let (decoded, errors) = decode_packets(&[oxideav_ac3::register], &params, &packets);

        let ours: Vec<(u32, u16)> = decoded
            .frames
            .iter()
            .zip(&decoded.frame_formats)
            .map(|(f, fmt)| match f {
                Frame::Audio(a) => (a.samples, fmt.map_or(0, |fmt| fmt.channels)),
                _ => (0, 0),
            })
            .collect();
        let theirs = pinned_frames(p, spec);
        if ours != theirs || errors.len() != pinned_decode_errors(p, spec) {
            let first = ours.iter().zip(&theirs).position(|(a, b)| a != b);
            failures.push(format!(
                "{sample} {spec}: {} frames vs FFmpeg {}, first differing frame {first:?}, errors {errors:?}",
                ours.len(),
                theirs.len()
            ));
            continue;
        }
        let snr = refcheck::snr_db(&pinned_pcm(p, spec), &interleave(&decoded.frames, &decoded.frame_formats), 0);
        eprintln!("{sample} {spec}: {} frames, SNR {snr:.1} dB", ours.len());
        if snr < 90.0 {
            failures.push(format!("{sample} {spec}: SNR {snr:.1} dB < 90"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
