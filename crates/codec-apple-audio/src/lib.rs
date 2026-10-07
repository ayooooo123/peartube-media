//! Apple audio decoders, ported from FFmpeg (commit 2da55bf,
//! LGPL-2.1-or-later):
//!
//! | Codec id | Ported from | Output | MP4/MOV sample entry | Matroska | CAF |
//! |---|---|---|---|---|---|
//! | `alac` | `alac.c`, `alacdsp.c`, `alac_data.c` | S16P / S32P, bit-exact | `alac` | `A_ALAC` | `alac` |
//! | `qdm2` | `qdm2.c`, `qdm2_tablegen.h`, `qdm2data.h`, mpegaudio synthesis, RDFT | float | `QDM2` | | `QDM2` |
//! | `qdmc` | `qdmc.c` | float | `QDMC` | | `QDMC` |
//! | `mace3` | `mace.c` | S16P, bit-exact | `MAC3` | | `MAC3` |
//! | `mace6` | `mace.c` | S16P, bit-exact | `MAC6` | | `MAC6` |
//!
//! Codec ids are FFmpeg's decoder names; CAF, AIFF-C and the QuickTime
//! demuxers name these codecs the same way. Every byte comes from
//! untrusted peers: reads are bounds-checked, allocations are capped, and
//! malformed packets give `Error::InvalidData`, never a panic.
#![forbid(unsafe_code)]

mod alac;
mod bits;
mod getbits;
mod mace;
mod qdm2;
mod qdmc;

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, RuntimeContext};

/// The codec ids this crate decodes.
pub const CODEC_IDS: [&str; 5] = ["alac", "qdm2", "qdmc", "mace3", "mace6"];

fn audio_info(id: &str, caps: CodecCapabilities) -> CodecInfo {
    CodecInfo::new(CodecId::new(id))
        .capabilities(caps.with_intra_only(true).with_priority(50))
        .with_resolution_priority(50)
}

/// Registers the five decoders and the container tags they travel under.
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        audio_info(
            "alac",
            CodecCapabilities::audio("alac_sw").with_lossless(true).with_max_channels(8),
        )
        .decoder(alac::make_decoder)
        .tags([CodecTag::fourcc(b"alac"), CodecTag::matroska("A_ALAC")]),
    );
    reg.register(
        audio_info("qdm2", CodecCapabilities::audio("qdm2_sw").with_lossy(true).with_max_channels(2))
            .decoder(qdm2::make_decoder)
            .tags([CodecTag::fourcc(b"QDM2")]),
    );
    reg.register(
        audio_info("qdmc", CodecCapabilities::audio("qdmc_sw").with_lossy(true).with_max_channels(2))
            .decoder(qdmc::make_decoder)
            .tags([CodecTag::fourcc(b"QDMC")]),
    );
    reg.register(
        audio_info("mace3", CodecCapabilities::audio("mace3_sw").with_lossy(true).with_max_channels(2))
            .decoder(mace::make_decoder)
            .tags([CodecTag::fourcc(b"MAC3")]),
    );
    reg.register(
        audio_info("mace6", CodecCapabilities::audio("mace6_sw").with_lossy(true).with_max_channels(2))
            .decoder(mace::make_decoder)
            .tags([CodecTag::fourcc(b"MAC6")]),
    );
}

/// Unified registration entry point.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

oxideav_core::register!("codec-apple-audio", register);
