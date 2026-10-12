//! The registered fixed decoder against FFmpeg 2da55bf's C MP2 path.
//! Full PCM equality, including synthesis startup, channel ordering,
//! fragmentation, repeated EOF and reset. No LSB tolerance or alignment slack.

use std::{path::PathBuf, process::Command};
use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, RuntimeContext, TimeBase};
use oxideav_mp2::{encode_all_frames, header::{Emphasis, Mode, ModeExtension}, FrameHeader};

fn drain(decoder: &mut dyn Decoder, pcm: &mut Vec<u8>) {
    for _ in 0..32 {
        match decoder.receive_frame() {
            Ok(Frame::Audio(frame)) => {
                assert_eq!(frame.samples, 1152);
                for sample in 0..frame.samples as usize {
                    for plane in &frame.data {
                        pcm.extend_from_slice(&plane[sample * 2..sample * 2 + 2]);
                    }
                }
            }
            Ok(_) => panic!("expected audio"),
            Err(Error::NeedMore | Error::Eof) => return,
            Err(error) => panic!("decode: {error}"),
        }
    }
    panic!("bounded drain did not finish");
}

fn compare(label: &str, pcm: &[u8], reference: &[u8], channels: usize) {
    assert_eq!(pcm.len(), reference.len(), "{label}: byte count");
    let to_float = |bytes: &[u8]| -> Vec<f32> {
        bytes.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0).collect()
    };
    let ours = to_float(pcm);
    let theirs = to_float(reference);
    let differences = ours.iter().zip(&theirs).filter(|(a, b)| a != b).count();
    let snr = refcheck::snr_db(&theirs, &ours, 0);
    eprintln!("{label}: samples/channel={}; differing={differences}; SNR={snr} dB; md5={}; reference={}",
        ours.len() / channels, refcheck::md5_hex(pcm), refcheck::md5_hex(reference));
    assert_eq!(differences, 0, "{label}: PCM must be bit-exact");
}

#[test]
fn channel_modes_rates_crc_and_packet_boundaries_are_bit_exact() {
    use Mode::{DualChannel, JointStereo, SingleChannel, Stereo};
    use ModeExtension::{Bound4, Bound8, Bound12, Bound16};
    let cases = [
        (48000, 192000, Stereo, Bound4),
        (44100, 128000, DualChannel, Bound4),
        (32000, 96000, SingleChannel, Bound4),
        (48000, 48000, SingleChannel, Bound4),
        (44100, 64000, Stereo, Bound4),
        (32000, 64000, Stereo, Bound4),
        (48000, 192000, JointStereo, Bound4),
        (44100, 192000, JointStereo, Bound8),
        (32000, 192000, JointStereo, Bound12),
        (48000, 192000, JointStereo, Bound16),
        (24000, 64000, JointStereo, Bound4),
        (22050, 96000, Stereo, Bound4),
        (16000, 32000, SingleChannel, Bound4),
    ];
    for (index, (rate, bitrate, mode, bound)) in cases.into_iter().enumerate() {
        let header = FrameHeader {
            lsf: rate < 32000, protection_bit: index % 2 == 0,
            bit_rate: bitrate, sample_rate: rate, padding: false, private_bit: false,
            mode, mode_extension: bound, copyright: false, original: true, emphasis: Emphasis::None,
        };
        let input: Vec<Vec<f64>> = (0..header.channels()).map(|ch| {
            (0..1152 * 6).map(|i| {
                let t = i as f64 / rate as f64;
                [311.0, 1373.0, 4507.0, 6901.0].into_iter().enumerate().map(|(band, f)| {
                    0.13 * (std::f64::consts::TAU * (f + ch as f64 * 79.0) * t + band as f64).sin()
                }).sum()
            }).collect()
        }).collect();
        let encoded = encode_all_frames(&header, &input, 0).unwrap();
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("fixed-mp2-{index}.mp2"));
        std::fs::write(&path, &encoded).unwrap();
        let output = Command::new(refcheck::pinned_ffmpeg())
            .args(["-v", "error", "-nostdin", "-cpuflags", "0", "-c:a", "mp2", "-i"])
            .arg(&path).args(["-map", "0:a:0", "-f", "s16le", "-"]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(output.stdout.len(), 6 * 1152 * header.channels() * 2);
        let mut params = CodecParameters::audio(CodecId::new("mp2"));
        params.sample_rate = Some(rate);
        params.channels = Some(header.channels() as u16);
        let mut context = RuntimeContext::new();
        codec_mp2::register(&mut context);
        let mut decoder = context.codecs.first_decoder(&params).unwrap();
        for chunk_size in [encoded.len(), 997, 3] {
            decoder.reset().unwrap();
            let mut actual = Vec::new();
            for chunk in encoded.chunks(chunk_size) {
                decoder.send_packet(&Packet::new(0, TimeBase::new(1, i64::from(rate)), chunk.to_vec())).unwrap();
                drain(decoder.as_mut(), &mut actual);
            }
            decoder.flush().unwrap();
            drain(decoder.as_mut(), &mut actual);
            assert!(matches!(decoder.receive_frame(), Err(Error::Eof)));
            compare(&format!("case-{index} {rate}Hz {mode:?} {bound:?} chunk={chunk_size}"),
                &actual, &output.stdout, header.channels());
        }
        std::fs::remove_file(path).unwrap();
    }
}
