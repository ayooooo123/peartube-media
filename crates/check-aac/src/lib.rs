//! Test-only helpers for the `check-aac` reference tests.
//!
//! The acceptance surface is the FATE/FFmpeg reference comparisons in
//! `tests/reference.rs`; this crate exists so those tests can run inside
//! the peartube-media workspace without shipping a decoder crate.

#![forbid(unsafe_code)]

use oxideav_core::{Frame, MediaType, SampleFormat};

/// Decode an `aac.mak` FATE sample through the fork behind OxideAV's
/// `mov` / `mp4` / `mpegts` containers and convert every audio frame to
/// interleaved f32. Each frame's channel count comes from its own byte
/// size: HE-AAC v2 streams go mono-until-PS, so the container's static
/// channel count can disagree with the decoded frames mid-stream.
pub fn decoded_f32(rel: &str) -> (Vec<f32>, std::path::PathBuf, u16) {
    let path = refcheck::fate(rel);
    let mut out = refcheck::decode(
        &path,
        &[
            oxideav_aac::__oxideav_entry,
            oxideav_mov::registry::register,
            oxideav_mp4::__oxideav_entry,
            oxideav_mpegts::__oxideav_entry,
        ],
        MediaType::Audio,
        0,
    );
    let mut channels = 1u16;
    let mut ours = Vec::new();
    for f in &out.frames {
        let Frame::Audio(a) = f else { continue };
        let bytes = a.data[0].len();
        let ch = (bytes / 4).checked_div(a.samples.max(1) as usize).unwrap_or(1);
        if (1..=8).contains(&ch) {
            channels = ch as u16;
        }
        for chunk in a.data[0].chunks_exact(4) {
            ours.push(f32::from_le_bytes(chunk.try_into().unwrap()));
        }
    }
    let _ = SampleFormat::F32;
    let _ = &mut out.params;
    (ours, path, channels)
}

/// The FATE samples the mutation test fuzzes: one per codec feature the
/// fork carries (LC, LC with the 960-line pulse family, HE-AAC v1, HE-AAC
/// v2, LATM-over-TS, ADTS), all real peer-deliverable streams.
pub const MUTATION_SAMPLES: &[&str] = &[
    "aac/al04_44.mp4",
    "aac/al04sf_48.mp4",
    "aac/al_sbr_cm_48_2.mp4",
    "aac/al_sbr_ps_04_new.mp4",
    "aac/latm_stereo_to_51.ts",
    "aac/CT_DecoderCheck/sbr_i-ps_i.aac",
];

/// USAC FD corpus and its MP4 edit-list trim, in samples per channel
/// `(path, initial_skip, final_padding)`. The raw decoder cannot apply
/// these until container-provided sample-trim metadata reaches the decode pipeline.
pub const USAC_SAMPLES: &[(&str, usize, usize)] = &[
    ("aac/Fd_2_c1_Ms_0x01.mp4", 2323, 763),
    ("aac/Fd_2_c1_Ms_0x04.mp4", 2220, 859),
    ("aac/usac/Fd_1_c1_0x03.mp4", 2220, 340),
    ("aac/usac/Fd_1_c1_0x04.mp4", 2220, 516),
    ("aac/usac/Fd_2_c1_0x03.mp4", 2220, 340),
    ("aac/usac/Fd_2_c1_0x05.mp4", 2220, 852),
    ("aac/usac/Fd_2_c1_Tns_0x04.mp4", 2220, 859),
    ("aac/usac/Ext_2_c1_Ln_0x03.mp4", 1600, 704),
    // FFmpeg omits the final whole AU outside the edit, then trims 128
    // samples from the preceding AU. OxideAV returns both raw AUs.
    ("aac/usac/xhe_target_level.m4a", 0, 1024 + 128),
];

/// Decode a USAC MP4 with FFmpeg-compatible optional loudness normalization.
/// The output remains untrimmed: the test asserts presentation bounds itself.
pub fn decoded_usac_target(rel: &str, target: i32) -> (Vec<f32>, std::path::PathBuf, u16) {
    use oxideav_core::{Error, RuntimeContext};
    let path = refcheck::fate(rel);
    let mut ctx = RuntimeContext::new();
    oxideav_aac::__oxideav_entry(&mut ctx);
    oxideav_mov::registry::register(&mut ctx);
    oxideav_mp4::__oxideav_entry(&mut ctx);
    let file = std::fs::File::open(&path).unwrap();
    let mut demuxer = ctx.containers.open_demuxer("mov", Box::new(file), &ctx.codecs).unwrap();
    let stream = demuxer.streams().iter().find(|s| s.params.media_type == MediaType::Audio).unwrap().clone();
    let mut params = stream.params;
    params.options.insert("target_level", target.to_string());
    let mut decoder = ctx.codecs.first_decoder(&params).unwrap();
    let mut frames = Vec::new();
    let drain = |decoder: &mut Box<dyn oxideav_core::Decoder>, frames: &mut Vec<Frame>| loop {
        match decoder.receive_frame() {
            Ok(frame) => frames.push(frame),
            Err(Error::NeedMore | Error::Eof) => break,
            Err(error) => panic!("{rel}: receive: {error}"),
        }
    };
    loop {
        match demuxer.next_packet() {
            Ok(packet) if packet.stream_index == stream.index => {
                decoder.send_packet(&packet).unwrap_or_else(|e| panic!("{rel}: packet {:?}: {e}", packet.pts));
                drain(&mut decoder, &mut frames);
            }
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(error) => panic!("{rel}: demux: {error}"),
        }
    }
    decoder.flush().unwrap();
    drain(&mut decoder, &mut frames);
    let format = decoder.output_audio_format().unwrap();
    let output = refcheck::Decoded { params, audio_format: Some(format), frames };
    (refcheck::interleaved_f32(&output), path, format.channels)
}
