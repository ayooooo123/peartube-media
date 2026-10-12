// Ported from FFmpeg libavcodec/wavpack.c, libavformat/wvdec.c (commit 2da55bf)
// Copyright (c) 2006, 2011 Konstantin Shishkov
// License: LGPL-2.1-or-later

#![forbid(unsafe_code)]

pub mod bitreader;
pub mod common;
pub mod decoder;
pub mod demuxer;
pub mod dsd;

use common::WV_MAX_CHANNELS;
use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, ContainerRegistry,
    RuntimeContext,
};

pub const CODEC_IDS: [&str; 1] = ["wavpack"];
pub const CONTAINER_NAMES: [&str; 1] = ["wv"];

pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        CodecInfo::new(CodecId::new("wavpack"))
            .capabilities(
                CodecCapabilities::audio("wavpack_sw")
                    .with_lossless(true)
                    .with_intra_only(true)
                    .with_max_channels(WV_MAX_CHANNELS as u16)
                    .with_priority(50),
            )
            .with_resolution_priority(50)
            .decoder(decoder::make_decoder)
            .tags([
                CodecTag::fourcc(b"wvpk"),
                CodecTag::fourcc(b"WVPK"),
                CodecTag::matroska("A_WAVPACK4"),
                CodecTag::wave_format(0x5756),
            ]),
    );
}

pub fn register_containers(reg: &mut ContainerRegistry) {
    demuxer::register_containers(reg);
}

pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("codec-wavpack", register);
