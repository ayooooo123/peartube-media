//! Miscellaneous container demuxers for PearTube media: raw AC-3/E-AC-3,
//! raw MPEG video, raw H.264 and HEVC Annex B, MPEG-1/2 program streams,
//! TechnoTrend PVA, NUT, Creative VOC, Core Audio Format (CAF), On2 IVF
//! and Standard MIDI Files. Ported from FFmpeg's libavformat (commit
//! 2da55bf), with the libavcodec parsers that cut its packets, except
//! SMF, which follows the MIDI 1.0 spec.
//!
//! Stream discovery finishes at open. Program streams retain PES video,
//! parse MPEG audio / AC-3 into decoder-sized frames, and assemble DVD
//! subpictures separately. Raw E-AC-3 units include dependent substreams.
//! PVA and NUT expose only the timestamps their containers carry; VOC
//! leaves later ADPCM timestamps unknown when no duration is available.
//!
//! Reference tests require FATE_SUITE, FFmpeg/ffprobe on PATH, and the
//! FFmpeg source with its built ffmpeg and ffprobe at FFMPEG_SRC
//! (revision 2da55bf). The inventory expands tests/fate/*.mak and rejects
//! missing inputs. Raw video compares complete access units and key flags;
//! raw MPEG-1/2 also FFmpeg's timestamps and durations. Raw AVC/HEVC
//! timing is unchanged: units are numbered, not timed as FFmpeg does.
#![forbid(unsafe_code)]

mod ac3;
mod av1_cbs;
mod h264;
mod hevc;
mod ivf;
mod mpegps;
mod mpegvideo;
mod nut;
mod parser;
mod pva;
mod rawvideo;
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
