//! The player's fixed-point MP2 path against FFmpeg 2da55bf, fed the
//! packets FFmpeg's demuxer and MPEG-audio parser give its decoder.

use check_decoders::{decode_packets, ffmpeg_packets, tool};
use oxideav_core::{CodecId, CodecParameters, Frame, SampleFormat};

/// End of file cuts the last MP2 frame of both audio tracks of
/// `h264_intra_first-small.ts` (296 B and 176 B of a 672 B frame).
/// FFmpeg only clamps a buffer longer than the frame and decodes a
/// shorter one over the zero padding (mpegaudiodec_template.c:1599-1604),
/// so each track ends with that frame: 49,536 and 50,688 samples, the
/// last frame included. Every sample, including the zero-decoded cut tail,
/// must match FFmpeg's fixed-point synthesis exactly.
#[test]
fn a_frame_cut_by_the_end_of_file_decodes_like_ffmpeg() {
    let path = refcheck::fate("h264/h264_intra_first-small.ts");
    for (track, samples) in [(0, 49_536), (1, 50_688)] {
        let spec = format!("a:{track}");
        let packets = ffmpeg_packets(&path, &spec, None);
        let last = packets.last().expect("packets").data.len();
        assert!(last < 672, "track {track}: the last packet ({last} B) is no longer cut short");

        let mut params = CodecParameters::audio(CodecId::new("mp2"));
        params.sample_rate = Some(48_000);
        params.channels = Some(2);
        params.sample_format = Some(SampleFormat::S16);
        let (decoded, refused) = decode_packets(&[codec_mp2::register], &params, &packets);
        let ours: Vec<u8> = decoded
            .frames
            .iter()
            .flat_map(|frame| match frame {
                Frame::Audio(audio) => interleave_s16(&audio.data, audio.samples as usize),
                _ => Vec::new(),
            })
            .collect();
        let theirs = tool(
            refcheck::pinned_ffmpeg(),
            &["-v", "error", "-nostdin", "-i", path.to_str().unwrap(), "-map", &format!("0:{spec}"), "-f", "s16le", "-"],
        );
        assert_eq!(
            (ours.len() / 4, &refused[..]),
            (samples, &[][..]),
            "track {track}: samples per channel and refused packets (FFmpeg decodes {} samples)",
            theirs.len() / 4
        );
        assert_eq!(theirs.len() / 4, samples, "track {track}: FFmpeg's sample count changed");
        assert_pcm_exact(&format!("truncated-track-{track}"), &ours, &theirs, 2);
    }
}

/// Interleaves the decoder's planar S16 output (one plane per channel).
fn interleave_s16(planes: &[Vec<u8>], samples: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(planes.len() * samples * 2);
    for i in 0..samples {
        for plane in planes {
            out.extend_from_slice(&plane[2 * i..2 * i + 2]);
        }
    }
    out
}

fn assert_pcm_exact(label: &str, ours: &[u8], theirs: &[u8], channels: usize) {
    assert_eq!(ours.len(), theirs.len(), "{label}: PCM byte count");
    let samples = |bytes: &[u8]| -> Vec<f32> {
        bytes.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0).collect()
    };
    let ours_pcm = samples(ours);
    let reference = samples(theirs);
    let differences = ours_pcm.iter().zip(&reference).filter(|(a, b)| a != b).count();
    let snr = refcheck::snr_db(&reference, &ours_pcm, 0);
    eprintln!("{label}: samples/channel={}; differences={differences}; SNR={snr} dB; md5={}; reference={}",
        ours_pcm.len() / channels, refcheck::md5_hex(ours), refcheck::md5_hex(theirs));
    assert_eq!(differences, 0, "{label}: exact PCM required, SNR={snr}");
}
