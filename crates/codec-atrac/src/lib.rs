//! ATRAC audio for the PearTube player: the ATRAC1, ATRAC3 (and ATRAC3 AL)
//! and ATRAC3+ (and ATRAC3+ AL) decoders, and the AEA (MD STUDIO) and OMA
//! (Sony OpenMG) demuxers, ported from FFmpeg (commit
//! `2da55bf59a68801a8157ab141a487196ce3416a8`), whose files all carry the
//! "GNU Lesser General Public License" header; so does this crate
//! (LGPL-2.1-or-later).
//!
//! | module | ported from |
//! |---|---|
//! | `common` | `libavcodec/atrac.c`, `atrac.h` |
//! | `atrac1` | `libavcodec/atrac1.c`, `atrac1data.h` |
//! | `atrac3` | `libavcodec/atrac3.c`, `atrac3data.h` |
//! | `atrac3p` | `libavcodec/atrac3plusdec.c`, `atrac3plus.c`, `atrac3plus.h`, `atrac3plusdsp.c`, `atrac3plus_data.h` |
//! | `tx` | `libavutil/tx_template.c` (the inverse MDCT's definition) |
//! | `vlc`, `bits` | `libavcodec/vlc.c`, `get_bits.h` |
//! | `frames` | `libavcodec/decode.c` (the decode loop) |
//! | `aea` | `libavformat/aeadec.c` |
//! | `oma` | `libavformat/omadec.c`, `oma.c`, `oma.h` |
//!
//! Every decoder outputs planar float (`F32P`), FFmpeg's `fltp`, and
//! reports it through `Decoder::output_audio_format`. The inverse MDCT
//! computes av_tx's definition in double precision, not with its float
//! codelets, so the output is within float rounding of FFmpeg's, not
//! bit-identical.
//!
//! Every byte comes from untrusted peers: no `unsafe`, bounded reads,
//! `Error::InvalidData` on malformed input, never a panic.

#![forbid(unsafe_code)]
// The ports keep FFmpeg's index loops and its constants digit for digit.
#![allow(clippy::needless_range_loop, clippy::excessive_precision)]

mod aea;
mod atrac1;
mod atrac3;
#[rustfmt::skip]
mod atrac3_tables;
mod atrac3p;
mod bits;
mod common;
mod demux;
mod frames;
mod oma;
mod tx;
mod vlc;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, Decoder, Result, RuntimeContext,
};

pub const CODEC_ID_ATRAC1: &str = "atrac1";
pub const CODEC_ID_ATRAC3: &str = "atrac3";
pub const CODEC_ID_ATRAC3AL: &str = "atrac3al";
/// FFmpeg's decoder name for `AV_CODEC_ID_ATRAC3P`.
pub const CODEC_ID_ATRAC3P: &str = "atrac3plus";
/// FFmpeg's decoder name for `AV_CODEC_ID_ATRAC3PAL`.
pub const CODEC_ID_ATRAC3PAL: &str = "atrac3plusal";

/// WAVEFORMATEX tag of ATRAC3 (`libavformat/riff.c`).
pub const WAVE_FORMAT_ATRAC3: u16 = 0x0270;

/// The codec id the OxideAV WAV demuxer gives an unmapped
/// WAVE_FORMAT_EXTENSIBLE SubFormat, for FFmpeg's ATRAC3+ GUID
/// `BF AA 23 E9 58 CB 71 44 A1 19 FF FA 01 E4 CE 62`.
pub const WAV_GUID_ATRAC3P: &str = "wav:guid_E923AABF-CB58-4471-A119-FFFA01E4CE62";

type MakeDecoder = fn(&oxideav_core::CodecParameters) -> Result<Box<dyn Decoder>>;

fn info(id: &str, make: MakeDecoder) -> CodecInfo {
    CodecInfo::new(CodecId::new(id))
        .capabilities(
            CodecCapabilities::audio(id)
                .with_lossy(true)
                .with_max_channels(8)
                .with_priority(50),
        )
        .with_resolution_priority(50)
        .decoder(make)
}

/// Registers the decoders: under FFmpeg's decoder names, with the codec
/// names FFmpeg's descriptors use (`atrac3p`, `atrac3pal`) as aliases, and
/// claiming the container tags each format travels under. `CodecTag` has
/// no 128-bit form, so the WAVE_FORMAT_EXTENSIBLE GUID FFmpeg maps to
/// ATRAC3+ (`ff_codec_wav_guids`) is claimed as the codec id the WAV
/// demuxer synthesizes for it.
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        info(CODEC_ID_ATRAC1, atrac1::make_decoder).tag(CodecTag::matroska("A_ATRAC/AT1")),
    );
    reg.register(
        info(CODEC_ID_ATRAC3, atrac3::make_decoder)
            .tag(CodecTag::wave_format(WAVE_FORMAT_ATRAC3))
            .tag(CodecTag::matroska("A_REAL/ATRC"))
            .tag(CodecTag::fourcc(b"atrc")),
    );
    reg.register(info(CODEC_ID_ATRAC3AL, atrac3::make_al_decoder));
    reg.register(info(CODEC_ID_ATRAC3P, atrac3p::make_decoder));
    reg.register(info("atrac3p", atrac3p::make_decoder));
    reg.register(info(CODEC_ID_ATRAC3PAL, atrac3p::make_al_decoder));
    reg.register(info("atrac3pal", atrac3p::make_al_decoder));
    reg.register(info(WAV_GUID_ATRAC3P, atrac3p::make_decoder));
}

/// Registers the AEA and OMA demuxers.
pub fn register_containers(reg: &mut oxideav_core::ContainerRegistry) {
    aea::register(reg);
    oma::register(reg);
}

/// Installs the decoders and demuxers into a runtime context.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("codec-atrac", register);
