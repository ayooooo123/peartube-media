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
