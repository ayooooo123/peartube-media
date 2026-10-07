//! oxideav-mp2 against FFmpeg's mp2 decoder, fed the packets FFmpeg's
//! demuxer and mpegaudio parser give its own decoder.

use check_decoders::{decode_packets, ffmpeg_packets, tool};
use oxideav_core::{CodecId, CodecParameters, Frame, SampleFormat};

/// End of file cuts the last MP2 frame of both audio tracks of
/// `h264_intra_first-small.ts` (296 B and 176 B of a 672 B frame).
/// FFmpeg only clamps a buffer longer than the frame and decodes a
/// shorter one over the zero padding (mpegaudiodec_template.c:1599-1604),
/// so each track ends with that frame: 49,536 and 50,688 samples, the
/// last frame included. The complete frames are PCM-identical to
/// FFmpeg's; in the cut frame's zero-decoded tail, where samples sit at
/// ±0.5 LSB, FFmpeg's fixed-point synthesis rounds some ties the other
/// way (78 samples on track 0), so that frame is held to 1 LSB.
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
        let (decoded, refused) = decode_packets(&[oxideav_mp2::register], &params, &packets);
        let ours: Vec<u8> = decoded
            .frames
            .iter()
            .flat_map(|frame| match frame {
                Frame::Audio(audio) => interleave_s16(&audio.data, audio.samples as usize),
                _ => Vec::new(),
            })
            .collect();
        let theirs = tool(
            "ffmpeg",
            &["-v", "error", "-nostdin", "-i", path.to_str().unwrap(), "-map", &format!("0:{spec}"), "-f", "s16le", "-"],
        );
        assert_eq!(
            (ours.len() / 4, &refused[..]),
            (samples, &[][..]),
            "track {track}: samples per channel and refused packets (FFmpeg decodes {} samples)",
            theirs.len() / 4
        );
        assert_eq!(theirs.len() / 4, samples, "track {track}: FFmpeg's sample count changed");
        let cut = (samples - 1152) * 4;
        let first = ours[..cut].chunks(2).zip(theirs[..cut].chunks(2)).position(|(a, b)| a != b);
        assert!(first.is_none(), "track {track}: complete frames differ from FFmpeg's from interleaved sample {first:?}");
        let worst = ours[cut..]
            .chunks(2)
            .zip(theirs[cut..].chunks(2))
            .map(|(a, b)| (i16::from_le_bytes([a[0], a[1]]) as i32 - i16::from_le_bytes([b[0], b[1]]) as i32).abs())
            .max()
            .unwrap_or(0);
        assert!(worst <= 1, "track {track}: the cut frame is {worst} LSB from FFmpeg's");
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
