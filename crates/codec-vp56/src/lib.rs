// Ported from FFmpeg (commit 2da55bf); see each module for its files.
// License: LGPL-2.1-or-later

//! On2 VP5 and VP6 for PearTube media, ported from FFmpeg: the `vp5`,
//! `vp6` (AVI, EA), `vp6f` (Flash: FLV, AVI) and `vp6a` (Flash with alpha:
//! FLV, MOV) decoders of libavcodec/vp5.c, vp6.c and vp56.c, with the VP3
//! DSP they use. They replace OxideAV's VP6 decoder, so `codecs` registers
//! them before it.
#![forbid(unsafe_code)]

mod decoder;
mod dsp;
mod huffman;
mod rac;
mod tables;
mod vp56;

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Decoder, PixelFormat, Result, RuntimeContext};

pub use decoder::Vp56Decoder;

/// Capability and tag-resolution priority, as the other PearTube decoders.
const PRIORITY: i32 = 50;

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(Vp56Decoder::new(params)?))
}

/// Registers the four decoders under FFmpeg's ids and the FourCCs
/// libavformat/riff.c maps to them (MOV uses the same for VP6A). FLV's
/// codec ids 4 and 5 need no tag: OxideAV's FLV demuxer names `vp6f` and
/// `vp6a` itself.
pub fn register_codecs(reg: &mut CodecRegistry) {
    let codecs: [(&str, &[&[u8; 4]], PixelFormat); 4] = [
        ("vp5", &[b"VP50"], PixelFormat::Yuv420P),
        ("vp6", &[b"VP60", b"VP61", b"VP62"], PixelFormat::Yuv420P),
        ("vp6f", &[b"VP6F", b"FLV4"], PixelFormat::Yuv420P),
        ("vp6a", &[b"VP6A"], PixelFormat::Yuva420P),
    ];
    for (id, fourccs, pix) in codecs {
        let mut caps = CodecCapabilities::video(format!("{id}_pear_sw"))
            .with_lossy(true)
            .with_intra_only(false)
            .with_priority(PRIORITY)
            .with_max_size(4080, 4080);
        caps.accepted_pixel_formats = vec![pix];
        reg.register(
            CodecInfo::new(CodecId::new(id))
                .capabilities(caps)
                .with_resolution_priority(PRIORITY)
                .decoder(make_decoder)
                .tags(fourccs.iter().map(|f| CodecTag::fourcc(f))),
        );
    }
}

/// Registers the decoders.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

oxideav_core::register!("codec-vp56", register);
