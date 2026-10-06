// Registration for the WMA decoder family.
// Decoders ported from FFmpeg (commit 2da55bf): libavcodec/wmadec.c (wmav1,
// wmav2), wmaprodec.c (wmapro), wmalosslessdec.c (wmalossless), wmavoice.c
// (wmavoice). GNU Lesser General Public License 2.1 or later.

use crate::wma::WmaDecoder;
use crate::wmalossless::WmaLosslessDecoder;
use crate::wmapro::WmaProDecoder;
use crate::wmavoice::WmaVoiceDecoder;
use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Decoder,
    RuntimeContext,
};

fn make_wmav1(params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
    Ok(Box::new(WmaDecoder::new(params, 1)?))
}

fn make_wmav2(params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
    Ok(Box::new(WmaDecoder::new(params, 2)?))
}

fn make_wmapro(params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
    Ok(Box::new(WmaProDecoder::new(params)?))
}

fn make_wmalossless(params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
    Ok(Box::new(WmaLosslessDecoder::new(params)?))
}

fn make_wmavoice(params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
    Ok(Box::new(WmaVoiceDecoder::new(params)?))
}

/// Register the five WMA decoders with codec ids matching FFmpeg's decoder
/// names. Container tag claims (WAVEFORMATEX `wFormatTag`):
/// - `wmav1` = 0x0160, `wmav2` = 0x0161 (also claimed by oxideav-wma as
///   `wma1`/`wma2`; our registrations take precedence via priority 50)
/// - `wmapro` = 0x0162, `wmalossless` = 0x0163, `wmavoice` = 0x000A
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        CodecInfo::new(CodecId::new("wmav1"))
            .capabilities(
                CodecCapabilities::audio("wmav1_sw_dec")
                    .with_max_channels(2)
                    .with_priority(50),
            )
            .decoder(make_wmav1)
            .tag(CodecTag::wave_format(0x0160)),
    );
    reg.register(
        CodecInfo::new(CodecId::new("wmav2"))
            .capabilities(
                CodecCapabilities::audio("wmav2_sw_dec")
                    .with_max_channels(2)
                    .with_priority(50),
            )
            .decoder(make_wmav2)
            .tag(CodecTag::wave_format(0x0161)),
    );
    reg.register(
        CodecInfo::new(CodecId::new("wmapro"))
            .capabilities(
                CodecCapabilities::audio("wmapro_sw_dec")
                    .with_max_channels(8)
                    .with_priority(50),
            )
            .decoder(make_wmapro)
            .tag(CodecTag::wave_format(0x0162)),
    );
    reg.register(
        CodecInfo::new(CodecId::new("wmalossless"))
            .capabilities(
                CodecCapabilities::audio("wmalossless_sw_dec")
                    .with_lossless(true)
                    .with_max_channels(8)
                    .with_priority(50),
            )
            .decoder(make_wmalossless)
            .tag(CodecTag::wave_format(0x0163)),
    );
    reg.register(
        CodecInfo::new(CodecId::new("wmavoice"))
            .capabilities(
                CodecCapabilities::audio("wmavoice_sw_dec")
                    .with_max_channels(1)
                    .with_priority(50),
            )
            .decoder(make_wmavoice)
            .tag(CodecTag::wave_format(0x000A)),
    );
}
