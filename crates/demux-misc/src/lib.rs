//! Miscellaneous container demuxers for PearTube media: raw AC-3/E-AC-3,
//! raw MPEG video, raw H.264 Annex B (probe only — OxideAV's `h264` stays
//! the primary), raw HEVC, MPEG-1/2 program streams, TechnoTrend PVA,
//! NUT, Creative VOC, Core Audio Format (CAF), On2 IVF and Standard MIDI
//! Files. Ported from FFmpeg's libavformat (commit 2da55bf) except SMF,
//! which follows the MIDI 1.0 spec.
#![forbid(unsafe_code)]

mod ac3;
mod h264;
mod hevc;
mod ivf;
mod mpegps;
mod mpegvideo;
mod nut;
mod parser;
mod pva;
mod smf;
mod voc;
mod caf;

use oxideav_core::RuntimeContext;

pub fn register(ctx: &mut RuntimeContext) {
    ac3::register(&mut ctx.containers);
    h264::register(&mut ctx.containers);
    hevc::register(&mut ctx.containers);
    ivf::register(&mut ctx.containers);
    mpegps::register(&mut ctx.containers);
    mpegvideo::register(&mut ctx.containers);
    nut::register(&mut ctx.containers);
    pva::register(&mut ctx.containers);
    smf::register(&mut ctx.containers);
    voc::register(&mut ctx.containers);
    caf::register(&mut ctx.containers);
}

oxideav_core::register!("demux-misc", register);
