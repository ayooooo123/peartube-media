//! Musepack (SV7 and SV8) demuxers and decoders, ported from FFmpeg
//! (commit 2da55bf), LGPL-2.1-or-later:
//!
//! - Demuxers: `mpc` (SV7, `MP+`) and `mpc8` (SV8, `MPCK`), ported from
//!   `libavformat/mpc.c`, `libavformat/mpc8.c`, and `libavformat/apetag.c`.
//!   Copyright (c) 2006, 2007 Konstantin Shishkov; Copyright (c) 2007 Benjamin Zores.
//! - Decoders: `musepack7` and `musepack8`, ported from `libavcodec/mpc7.c`,
//!   `libavcodec/mpc8.c`, `libavcodec/mpc.c`, `libavcodec/mpc.h`,
//!   `libavcodec/mpcdata.h`, `libavcodec/mpc7data.h`, `libavcodec/mpc8data.h`,
//!   `libavcodec/mpc8huff.h`, and `libavcodec/bswapdsp.c`.
//!   Copyright (c) 2006, 2007 Konstantin Shishkov.
//! - Fixed-point synthesis and DCT32: ported from `libavcodec/mpegaudiodsp.c`,
//!   `libavcodec/mpegaudiodsp_template.c`, `libavcodec/dct32_template.c`,
//!   and `libavcodec/mpegaudiodsp_data.c`.
//!   Copyright (c) 2001, 2002 Fabrice Bellard; Copyright (c) 2011 Mans Rullgard.
//! - PRNG: ported from `libavutil/lfg.c` and `libavutil/lfg.h`.
//!   Copyright (c) 2008 Michael Niedermayer.
//! - Bit reader and VLC tables: ported from `libavcodec/get_bits.h`,
//!   `libavcodec/unary.h`, and `libavcodec/vlc.c`.
//!   Copyright (c) 2003-2023 FFmpeg developers.
//!
//! Output format is S16P, reported through `Decoder::output_audio_format()`.
//! Every byte comes from untrusted peers: reads are bounds-checked, allocations
//! are capped, and malformed input returns an error, never a panic.

#![forbid(unsafe_code)]

mod bits;
mod containers;
mod mpc;
mod mpc7_data;
mod mpc7_dec;
mod mpc8_data;
mod mpc8_dec;
mod mpc8_huff;
mod mpc_data;
mod synth;
mod vlc;

pub use containers::{mpc8_probe, mpc_probe, open_mpc, open_mpc8};
pub use mpc7_dec::make_decoder as make_mpc7_decoder;
pub use mpc8_dec::make_decoder as make_mpc8_decoder;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecRegistry, ContainerRegistry, RuntimeContext,
};

/// The codec ids this crate decodes.
pub const CODEC_IDS: [&str; 2] = ["musepack7", "musepack8"];

/// The container names this crate demuxes.
pub const CONTAINER_NAMES: [&str; 2] = ["mpc", "mpc8"];

fn mpc_info(id: &str, implementation: &str, max_channels: u16, max_rate: u32) -> CodecInfo {
    CodecInfo::new(CodecId::new(id))
        .capabilities(
            CodecCapabilities::audio(implementation)
                .with_lossy(true)
                .with_intra_only(true)
                .with_max_channels(max_channels)
                .with_max_sample_rate(max_rate)
                .with_priority(50),
        )
        .with_resolution_priority(50)
}

/// Registers the `musepack7` and `musepack8` decoders.
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        mpc_info("musepack7", "mpc7_sw", 2, 48_000)
            .decoder(mpc7_dec::make_decoder),
    );
    reg.register(
        mpc_info("musepack8", "mpc8_sw", 2, 48_000)
            .decoder(mpc8_dec::make_decoder),
    );
}

/// Registers the `mpc` and `mpc8` demuxers.
pub fn register_containers(reg: &mut ContainerRegistry) {
    containers::register(reg);
}

/// Unified registration entry point.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("codec-musepack", register);
