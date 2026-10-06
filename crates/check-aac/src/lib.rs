//! Test-only helpers for the `check-aac` reference tests.
//!
//! The acceptance surface is the FATE/FFmpeg reference comparisons in
//! `tests/reference.rs`; this crate exists so those tests can run inside
//! the peartube-media workspace without shipping a decoder crate.

#![forbid(unsafe_code)]

use oxideav_core::{CodecParameters, Decoder, Frame, MediaType, Packet, SampleFormat};

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

/// USAC FD corpus: `(path, initial_skip, final_padding, snr_floor_db)`.
/// Skip and padding are the MP4 edit-list trim in samples per channel. The
/// raw decoder cannot apply them until container-provided sample-trim
/// metadata reaches the decode pipeline. Floors are the first measured SNR
/// (fork `e03fbe6`) minus 0.5 dB.
pub const USAC_SAMPLES: &[(&str, usize, usize, f64)] = &[
    ("aac/Fd_2_c1_Ms_0x01.mp4", 2323, 763, 138.962070),
    ("aac/Fd_2_c1_Ms_0x04.mp4", 2220, 859, 138.573987),
    ("aac/usac/Fd_1_c1_0x03.mp4", 2220, 340, 138.753700),
    ("aac/usac/Fd_1_c1_0x04.mp4", 2220, 516, 138.715224),
    ("aac/usac/Fd_2_c1_0x03.mp4", 2220, 340, 138.742638),
    ("aac/usac/Fd_2_c1_0x05.mp4", 2220, 852, 138.611948),
    ("aac/usac/Fd_2_c1_Tns_0x04.mp4", 2220, 859, 137.773368),
    ("aac/usac/Ext_2_c1_Ln_0x03.mp4", 1600, 704, 139.604440),
    // FFmpeg omits the final whole AU outside the edit, then trims 128
    // samples from the preceding AU. OxideAV returns both raw AUs.
    ("aac/usac/xhe_target_level.m4a", 0, 1024 + 128, 138.516260),
];

/// The first audio stream's parameters and all of its packets, demuxed by
/// OxideAV's mov/mp4 registry.
pub fn usac_packets(rel: &str) -> (CodecParameters, Vec<Packet>) {
    use oxideav_core::{Error, RuntimeContext};
    let path = refcheck::fate(rel);
    let mut ctx = RuntimeContext::new();
    oxideav_aac::__oxideav_entry(&mut ctx);
    oxideav_mov::registry::register(&mut ctx);
    oxideav_mp4::__oxideav_entry(&mut ctx);
    let file = std::fs::File::open(&path).unwrap();
    let mut demuxer = ctx.containers.open_demuxer("mov", Box::new(file), &ctx.codecs).unwrap();
    let stream = demuxer.streams().iter().find(|s| s.params.media_type == MediaType::Audio).unwrap().clone();
    let mut packets = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(packet) if packet.stream_index == stream.index => packets.push(packet),
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(error) => panic!("{rel}: demux: {error}"),
        }
    }
    (stream.params, packets)
}

/// A fresh decoder for `params` from the fork's codec registration.
pub fn aac_decoder(params: &CodecParameters) -> Box<dyn Decoder> {
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_aac::__oxideav_entry(&mut ctx);
    ctx.codecs.first_decoder(params).unwrap()
}

/// Send one access unit and return its frame as interleaved f32 PCM.
pub fn decode_one(decoder: &mut Box<dyn Decoder>, packet: &Packet) -> oxideav_core::Result<Vec<f32>> {
    decoder.send_packet(packet)?;
    match decoder.receive_frame()? {
        Frame::Audio(audio) => {
            Ok(audio.data[0].chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect())
        }
        _ => panic!("AAC decoder returned a non-audio frame"),
    }
}

/// Decode a USAC MP4's unmodified packets with FFmpeg-compatible optional
/// loudness normalization (`target` 0 is off). The output remains untrimmed:
/// tests assert presentation bounds themselves.
pub fn decoded_usac_target(rel: &str, target: i32) -> (Vec<f32>, std::path::PathBuf, u16) {
    let (mut params, packets) = usac_packets(rel);
    params.options.insert("target_level", target.to_string());
    let mut decoder = aac_decoder(&params);
    let mut pcm = Vec::new();
    for packet in &packets {
        pcm.extend(decode_one(&mut decoder, packet).unwrap_or_else(|e| panic!("{rel}: packet {:?}: {e}", packet.pts)));
    }
    let channels = decoder.output_audio_format().unwrap().channels;
    (pcm, refcheck::fate(rel), channels)
}
