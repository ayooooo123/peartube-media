//! Shorten: the `shorten` decoder (8-bit unsigned and 16-bit streams, up to
//! 8 channels, versions 0 to 3, the embedded WAVE or AIFF/AIFC header) and
//! the raw `shn` demuxer.
//!
//! Packets remain compressed until `receive_frame` decodes one block.
//! `flush` only signals the end: subsequent receives finish pending packets
//! and drain buffered blocks, preserving the bit position between calls.
//! Repeated flush/drain calls do not duplicate output; `reset` drops it.
//!
//! Ported from FFmpeg (commit 2da55bf): libavcodec/shorten.c and the
//! readers it uses (get_bits.h, golomb.h, bytestream.h), libavformat/
//! shortendec.c and rawdec.c. Licensed under LGPL-2.1-or-later (see
//! LICENSE).

#![forbid(unsafe_code)]

mod bits;
mod decoder;
mod demuxer;

pub use decoder::{ShortenDecoder, StreamHeader, parse_stream_header};

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, ContainerRegistry, RuntimeContext};

/// Capability priority, ahead of any OxideAV claimant.
pub const RESOLUTION_PRIORITY: i32 = 50;

/// Registers `shorten`. No container tag maps to it outside the `shn`
/// demuxer (FFmpeg's NIST SPHERE reader also carries it; that container is
/// not one the player opens).
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        CodecInfo::new(CodecId::new("shorten"))
            .capabilities(
                CodecCapabilities::audio("shorten_sw")
                    .with_lossless(true)
                    .with_intra_only(false)
                    .with_max_channels(8)
                    .with_priority(RESOLUTION_PRIORITY),
            )
            .decoder(|params| Ok(Box::new(ShortenDecoder::new(params)?)))
            .with_resolution_priority(RESOLUTION_PRIORITY),
    );
}

/// Registers the `shn` demuxer.
pub fn register_containers(reg: &mut ContainerRegistry) {
    demuxer::register(reg);
}

pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("codec-shorten", register);
