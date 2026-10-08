//! Speech decoders and their raw containers, ported from FFmpeg (commit
//! 2da55bf, LGPL-2.1-or-later):
//!
//! | Codec id | Ported from | Output | MP4/MOV | WAVEFORMATEX |
//! |---|---|---|---|---|
//! | `amr_nb` | `amrnbdec.c`, `amrnbdata.h` | F32P, 8 kHz | `samr` | 0x0038, 0x0057 |
//! | `amr_wb` | `amrwbdec.c`, `amrwbdata.h` | F32P, 16 kHz | `sawb` | 0x0058 |
//! | `qcelp` | `qcelpdec.c`, `qcelpdata.h` | F32, 8 kHz | `Qclp`, `Qclq`, `sqcp` | |
//!
//! The decoders share the CELP routines of `celp_math.c`,
//! `celp_filters.c`, `acelp_filters.c`, `acelp_vectors.c`,
//! `acelp_pitch_delay.c`, `lsp.c` and libavutil's
//! `float_scalarproduct.c`; AMR-WB's noise comes from `lfg.c`.
//! Containers: `amr` (`amr.c`, framed as `amr_parser.c` does) and `qcp`
//! (`qcp.c`, which also names EVRC, SMV and 4GV streams). The float
//! arithmetic follows what FFmpeg's compiled C does on arm64 (see `celp`),
//! so the output matches FFmpeg's (`-cpuflags 0`) bit for bit there.
//!
//! The ids are those OxideAV and FFmpeg's demuxers give these codecs; the
//! tags are FFmpeg's (`isom_tags.c`, `riff.c`; it maps none in Matroska
//! but through `A_MS/ACM`). Every byte comes from untrusted peers: reads
//! are bounds-checked, allocations are capped, and malformed input gives
//! an error, never a panic.
#![forbid(unsafe_code)]

mod amrnb_data;
mod amrnb_dec;
mod amrwb_data;
mod amrwb_dec;
mod celp;
mod containers;
mod qcelp_data;
mod qcelp_dec;

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, ContainerRegistry, RuntimeContext};

/// The codec ids this crate decodes.
pub const CODEC_IDS: [&str; 3] = ["amr_nb", "amr_wb", "qcelp"];

fn speech_info(id: &str, implementation: &str, max_channels: u16, max_rate: u32) -> CodecInfo {
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

/// Registers the three decoders and the tags they travel under.
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        speech_info("amr_nb", "amrnb_sw", 2, 8_000)
            .decoder(amrnb_dec::make_decoder)
            .tags([CodecTag::fourcc(b"samr"), CodecTag::wave_format(0x0038), CodecTag::wave_format(0x0057)]),
    );
    reg.register(
        speech_info("amr_wb", "amrwb_sw", 2, 16_000)
            .decoder(amrwb_dec::make_decoder)
            .tags([CodecTag::fourcc(b"sawb"), CodecTag::wave_format(0x0058)]),
    );
    reg.register(
        speech_info("qcelp", "qcelp_sw", 1, 8_000)
            .decoder(qcelp_dec::make_decoder)
            .tags([CodecTag::fourcc(b"Qclp"), CodecTag::fourcc(b"Qclq"), CodecTag::fourcc(b"sqcp")]),
    );
}

/// Registers the `amr` and `qcp` demuxers.
pub fn register_containers(reg: &mut ContainerRegistry) {
    containers::register(reg);
}

/// Unified registration entry point.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("codec-speech", register);
