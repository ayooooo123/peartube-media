//! FATE's PVA sample decoded through the player's registry against FFmpeg
//! 2da55bf (`-idct simple`, as fate-pva-demux). The file is cut short
//! inside its last coded picture, a B-picture: FFmpeg conceals the 510
//! macroblocks it lacks ("ac-tex damaged at 7 21") and shows it, then the
//! anchor it held. The MPEG-1/2 decoder conceals as FFmpeg does, so every
//! frame comes out equal to FFmpeg's, the concealed one included.

use oxideav_core::{Frame, MediaType};

#[test]
fn pva_video_equals_ffmpeg_with_the_cut_off_picture_concealed() {
    let path = refcheck::fate("pva/PVA_test-partial.pva");
    let decoded = refcheck::decode(&path, &[codecs::register_all], MediaType::Video, 0);
    let dims = [(544, 576), (272, 288), (272, 288)];
    let ours: Vec<String> = decoded
        .frames
        .iter()
        .map(|f| {
            let Frame::Video(vf) = f else { panic!("not a video frame") };
            refcheck::md5_hex(&refcheck::pack(vf, &dims))
        })
        .collect();
    let args = refcheck::ffmpeg_video_md5_args(&path, "0:v:0", "yuv420p", &["-idct", "simple"]);
    let out = std::process::Command::new(refcheck::pinned_ffmpeg()).args(["-v", "error", "-nostdin"]).args(&args).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let theirs = refcheck::parse_framemd5(std::str::from_utf8(&out.stdout).unwrap());
    assert_eq!(theirs.len(), 37, "FFmpeg's frames");
    assert_eq!(ours.len(), theirs.len(), "frames");
    let differ: Vec<usize> = ours.iter().zip(&theirs).enumerate().filter(|(_, (o, t))| o != t).map(|(i, _)| i).collect();
    if !differ.is_empty() {
        let dir = std::path::PathBuf::from(std::env::var_os("CARGO_TARGET_DIR").expect("CARGO_TARGET_DIR")).join("evidence");
        std::fs::create_dir_all(&dir).unwrap();
        let pixels: Vec<u8> = decoded.frames.iter().flat_map(|frame| {
            let Frame::Video(vf) = frame else { unreachable!() };
            refcheck::pack(vf, &dims)
        }).collect();
        std::fs::write(dir.join("pva-actual.yuv"), pixels).unwrap();
        std::fs::write(dir.join("pva-expected.framemd5"), out.stdout).unwrap();
    }
    assert!(differ.is_empty(), "frames {differ:?} differ from FFmpeg's");
}

fn audio_pcm(path: &std::path::Path) -> Vec<f32> {
    let decoded = refcheck::decode(path, &[codecs::register_all], MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(2));
    assert_eq!(decoded.params.sample_rate, Some(48000));
    refcheck::interleaved_f32(&decoded)
}

#[test]
fn pva_and_grouped_mp2_in_wav_keep_every_sample() {
    let pva = refcheck::fate("pva/PVA_test-partial.pva");
    let dir = std::path::PathBuf::from(std::env::var_os("CARGO_TARGET_DIR").expect("CARGO_TARGET_DIR")).join("evidence");
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("pva-mp2.wav");
    let out = std::process::Command::new(refcheck::pinned_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y", "-i"]).arg(&pva)
        .args(["-map", "0:a:0", "-c:a", "copy"]).arg(&wav).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let pva_pcm = audio_pcm(&pva);
    let wav_pcm = audio_pcm(&wav);
    assert_eq!(pva_pcm.len(), 96768 * 2, "all 84 MP2 frames, not 21 PES packets");
    assert_eq!(wav_pcm.len(), 96768 * 2, "WAV must retain all frames in each byte chunk");
    assert_eq!(wav_pcm, pva_pcm, "WAV byte chunks must preserve the same continuous synthesis state");
    for (path, pcm) in [(&pva, &pva_pcm), (&wav, &wav_pcm)] {
        let reference = refcheck::ffmpeg_audio_f32(path, 0);
        assert_eq!(pcm.len(), reference.len(), "{}: FFmpeg sample count", path.display());
        let different = pcm.iter().zip(&reference).filter(|(a, b)| a != b).count();
        let snr = refcheck::snr_db(&reference, pcm, 0);
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| ((*s * 32768.0) as i16).to_le_bytes()).collect();
        let reference_bytes: Vec<u8> = reference.iter().flat_map(|s| ((*s * 32768.0) as i16).to_le_bytes()).collect();
        eprintln!("{}: samples/channel=96768; differing samples={different}; SNR={snr} dB; md5={}; reference={}",
            path.display(), refcheck::md5_hex(&bytes), refcheck::md5_hex(&reference_bytes));
        assert_eq!(different, 0, "MP2 must be bit-exact, not merely within one LSB");
    }
}
