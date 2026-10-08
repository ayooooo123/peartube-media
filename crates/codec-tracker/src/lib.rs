//! Tracker modules (MOD, S3M, XM, IT, MTM, 669, ULT, STM), ported from
//! libopenmpt 0.8.9, which VLC's mod module plays them with. The module
//! file is the stream: one crate loads it and renders it to stereo float
//! PCM as libopenmpt does with openmpt123's defaults (48 kHz, 8-tap
//! polyphase sinc, 100% stereo separation, default ramping).
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).
//!
//! Every byte comes from untrusted peers: reads are bounds-checked, sizes
//! are capped, and malformed input returns an error, never a panic.

#![forbid(unsafe_code)]

mod channel;
mod command;
mod defs;
mod ext;
mod fx;
mod instrument;
mod io;
mod length;
mod load_it;
mod load_misc;
mod load_mod;
mod load_s3m;
mod load_xm;
mod midimacro;
mod mixer;
mod player;
mod registry;
mod render;
mod rowvisitor;
mod sample;
mod sndfile;
mod sndmix;
mod tables;
mod upgrade;
mod version;

use sndfile::Module;

pub use length::RowPos;
pub use registry::{make_decoder, register_codecs, register_containers};
pub use render::{READ_FRAMES, Renderer, SAMPLE_RATE, Song};

/// Unified container and decoder registration.
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("codec-tracker", register);

/// Module formats this crate loads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Mod,
    S3m,
    Xm,
    It,
    Mtm,
    Six69,
    Ult,
    Stm,
}

impl Format {
    /// Container name and file extension.
    pub fn name(self) -> &'static str {
        match self {
            Format::Mod => "mod",
            Format::S3m => "s3m",
            Format::Xm => "xm",
            Format::It => "it",
            Format::Mtm => "mtm",
            Format::Six69 => "669",
            Format::Ult => "ult",
            Format::Stm => "stm",
        }
    }
}

/// Probe order of libopenmpt's loader table (`Sndfile.cpp`), restricted to
/// the formats this crate ports.
const LOADERS: [(Format, fn(&[u8]) -> bool, fn(&[u8]) -> Option<Module>); 8] = [
    (Format::Xm, load_xm::probe, load_xm::read),
    (Format::It, load_it::probe, load_it::read),
    (Format::S3m, load_s3m::probe, load_s3m::read),
    (Format::Stm, load_misc::probe_stm, load_misc::read_stm),
    (Format::Mtm, load_misc::probe_mtm, load_misc::read_mtm),
    (Format::Ult, load_misc::probe_ult, load_misc::read_ult),
    (Format::Mod, load_mod::probe, load_mod::read),
    (Format::Six69, load_misc::probe_669, load_misc::read_669),
];

/// The format whose header `data` starts with.
pub fn probe(data: &[u8]) -> Option<Format> {
    LOADERS.iter().find(|(_, probe, _)| probe(data)).map(|(f, _, _)| *f)
}

/// Loads a module: the first loader in libopenmpt's order that accepts the
/// file wins.
fn load(data: &[u8]) -> Option<(Format, Module)> {
    if data.len() as u64 > registry::MAX_FILE_BYTES {
        return None;
    }
    for (format, probe, read) in LOADERS {
        if probe(data) {
            io::reset_sample_budget();
            if let Some(mut m) = read(data) {
                if io::sample_budget_exceeded() {
                    return None;
                }
                // There is no OPL synthesizer in this crate. Refuse AdLib
                // instruments rather than return silent replacement audio.
                if m.samples.iter().any(|s| s.u_flags & defs::CHN_ADLIB != 0) {
                    return None;
                }
                // A short pattern can be referenced by thousands of orders.
                // Bound the row visitor independently of pattern storage.
                let visits: usize = m.order.iter().map(|&p| {
                    m.patterns.get(p as usize).map_or(1, |p| p.rows as usize)
                }).sum();
                if visits > load_xm::MAX_PATTERN_CELLS {
                    return None;
                }
                upgrade::upgrade(&mut m);
                m.finish_load();
                return Some((format, m));
            }
        }
    }
    None
}
