//! Reference tests: Speex through the production demuxers, compared with
//! FFmpeg 2da55bf's decode of the same file (`refcheck::pinned_ffmpeg`,
//! run with `-cpuflags 0`: its C code paths). Speex synthesizes in
//! floating point: SNR of at least 90 dB, sample counts within one packet.
//!
//! - Ogg: the FATE `speex/` samples (narrowband, narrowband with in-band
//!   stereo, wideband, ultra-wideband), header in the extradata.
//! - AVI: the ultra-wideband track of `vp5/potter512-400-partial.avi`
//!   (WAVE format 0xA109), header inside the extradata.
//! - FLV: `speex/wb_q8.spx` remuxed to FLV by FFmpeg; FLV carries no
//!   header, so the decoder takes the mode from the 16 kHz rate.
//! - Raw: each Ogg sample's packets with the extradata removed, so the
//!   decoder starts from the sample rate and channel count only. FFmpeg's
//!   decode of the Ogg file is the reference: for these files a decoder
//!   without the header ends every packet after its one frame too.

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{CodecId, Decoder, Frame, MediaType, RuntimeContext, SampleFormat};
use refcheck::{decode, fate, interleaved_f32, try_snr_db};

fn registrars() -> Vec<refcheck::Registrar> {
    vec![codec_speex::register, oxideav_ogg::register, oxideav_avi::__oxideav_entry, oxideav_flv::register]
}

fn run_pinned(args: &[&str]) -> Vec<u8> {
    let out = Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0"])
        .args(args)
        .output()
        .expect("the pinned FFmpeg runs");
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// FFmpeg's interleaved f32 decode of the first audio stream.
fn ffmpeg_f32(path: &Path) -> Vec<f32> {
    run_pinned(&["-i", path.to_str().unwrap(), "-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"])
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// SNR of `ours` against FFmpeg's decode of `path`, asserted >= 90 dB,
/// with lengths within `packet` samples per channel.
fn assert_snr(name: &str, path: &Path, ours: &[f32], channels: u16, packet: usize) {
    let theirs = ffmpeg_f32(path);
    let snr = try_snr_db(&theirs, ours, packet * usize::from(channels)).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert!(snr >= 90.0, "{name}: SNR {snr:.1} dB against FFmpeg");
    eprintln!("{name}: {} samples, SNR {snr:.1} dB", ours.len());
}

/// Decodes the first audio stream of `path` through the demuxer and
/// compares it with FFmpeg; `rate`, `channels` and `packet` (samples per
/// channel in one packet) are the stream's.
fn check(path: &Path, rate: u32, channels: u16, packet: usize) {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let decoded = decode(path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.codec_id, CodecId::new("speex"), "{name}: codec");
    let format = decoded.audio_format.expect("the decoder reports its layout");
    assert_eq!((format.sample_format, format.sample_rate, format.channels), (SampleFormat::F32, rate, channels), "{name}: layout");
    assert_snr(&name, path, &interleaved_f32(&decoded), channels, packet);
}

#[test]
fn ogg_narrowband() {
    check(&fate("speex/nb_q7.spx"), 8000, 1, 160);
}

#[test]
fn ogg_narrowband_stereo() {
    check(&fate("speex/stereo_q4.spx"), 8000, 2, 160);
}

#[test]
fn ogg_wideband() {
    check(&fate("speex/wb_q8.spx"), 16000, 1, 320);
}

#[test]
fn ogg_ultra_wideband() {
    check(&fate("speex/uwb_q4.spx"), 32000, 1, 640);
}

#[test]
fn avi_ultra_wideband() {
    check(&fate("vp5/potter512-400-partial.avi"), 32000, 1, 640);
}

#[test]
fn flv_wideband() {
    let dir = std::env::temp_dir().join(format!("codec-speex-ref-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let flv: PathBuf = dir.join("wb_q8.flv");
    let src = fate("speex/wb_q8.spx");
    run_pinned(&["-y", "-i", src.to_str().unwrap(), "-map", "0:a:0", "-c", "copy", "-f", "flv", flv.to_str().unwrap()]);
    check(&flv, 16000, 1, 320);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The Ogg sample's audio packets decoded without the extradata.
fn decode_raw(path: &Path) -> (Vec<f32>, u16) {
    let mut ctx = RuntimeContext::new();
    codec_speex::register(&mut ctx);
    oxideav_ogg::register(&mut ctx);
    let file = std::fs::File::open(path).unwrap();
    let mut demuxer = ctx.containers.open_demuxer("ogg", Box::new(file), &ctx.codecs).expect("open demuxer");
    let stream = demuxer.streams().iter().find(|s| s.params.media_type == MediaType::Audio).expect("audio").clone();
    let mut params = stream.params.clone();
    params.extradata.clear();
    let channels = params.channels.expect("the Ogg header gives the channel count");
    let mut decoder: Box<dyn Decoder> = ctx.codecs.first_decoder(&params).expect("decoder without extradata");
    let mut out = Vec::new();
    let mut take = |decoder: &mut Box<dyn Decoder>| {
        while let Ok(Frame::Audio(a)) = decoder.receive_frame() {
            out.extend(a.data[0].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
        }
    };
    while let Ok(packet) = demuxer.next_packet() {
        if packet.stream_index == stream.index {
            decoder.send_packet(&packet).expect("decode");
            take(&mut decoder);
        }
    }
    decoder.flush().unwrap();
    take(&mut decoder);
    (out, channels)
}

#[test]
fn raw_without_header() {
    for (sample, packet) in [("nb_q7.spx", 160), ("stereo_q4.spx", 160), ("wb_q8.spx", 320), ("uwb_q4.spx", 640)] {
        let path = fate(&format!("speex/{sample}"));
        let (ours, channels) = decode_raw(&path);
        assert_snr(&format!("raw {sample}"), &path, &ours, channels, packet);
    }
}
