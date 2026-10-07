// Ported from FFmpeg libavformat/mxfdec.c, mxf.c, mxf.h (commit 2da55bf)
// License: LGPL-2.1-or-later

//! The `mxf` demuxer: SMPTE 377M Material eXchange Format, ported from
//! FFmpeg's mxfdec.c. OP1a and OPAtom, frame- and clip-wrapped essence,
//! index tables (CBR and VBR, temporal offsets), D-10 AES3 audio; codec
//! ids from FFmpeg's UL tables (MPEG-2, H.264, DV, JPEG 2000, DNxHD,
//! ProRes, PCM, ...). Packets carry the timestamps, durations and key
//! flags FFmpeg's demuxer layer gives them (see `layer`).

#![forbid(unsafe_code)]

mod demuxer;
mod index;
mod klv;
mod layer;
mod sets;
mod structure;
mod types;
mod uls;

pub use demuxer::MxfDemuxer;
pub use types::Op;

use oxideav_core::{ContainerRegistry, ProbeData, ProbeScore, MAX_PROBE_SCORE};

use types::{HEADER_PARTITION_PACK_KEY, RUN_IN_MAX};

/// mxf_probe: the header partition pack key within the run-in, at the
/// start for the full score.
pub fn probe(p: &ProbeData) -> ProbeScore {
    let buf = p.buf;
    let key = &HEADER_PARTITION_PACK_KEY;
    if buf.len() < key.len() {
        return 0;
    }
    let end = buf.len().min(RUN_IN_MAX as usize + 1 + key.len()) - key.len();
    let mut i = 0;
    // Skip the run-in and look for the key (SMPTE 377M 5.5); bytes whose
    // byte 13 cannot be the key's skip ahead 10, as FFmpeg's scan does.
    while i < end {
        if buf[i + 13].wrapping_sub(1) & 0xF2 == 0 {
            if buf[i..i + 14] == key[..] {
                return if i == 0 { MAX_PROBE_SCORE } else { MAX_PROBE_SCORE - 1 };
            }
            i += 1;
        } else {
            i += 10;
        }
    }
    0
}

pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("mxf", demuxer::open);
    reg.register_probe("mxf", probe);
    reg.register_extension("mxf", "mxf");
}

pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("demux-mxf", register);
