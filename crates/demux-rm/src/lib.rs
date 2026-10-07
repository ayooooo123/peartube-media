// Ported from FFmpeg libavformat/rmdec.c, rmsipr.c, rm.c and the seek.c
// search rm_read_seek uses, commit 2da55bf
// License: GNU Lesser General Public License (LGPL) version 2.1 or later

#![forbid(unsafe_code)]

pub mod demuxer;
pub mod rm_tags;
pub mod rv34;
pub mod rmsipr;
mod seek;

pub use demuxer::{open, RmDemuxer};
pub use rm_tags::codec_id_from_rm_tag;

use oxideav_core::{ContainerRegistry, ProbeData, RuntimeContext};

/// Probe for RealMedia files.
pub fn probe(p: &ProbeData) -> u8 {
    if p.buf.len() >= 6
        && ((&p.buf[0..4] == b".RMF" || &p.buf[0..4] == b".RMP") && p.buf[4..6] == [0, 0])
    {
        100
    } else if p.buf.len() >= 4 && &p.buf[0..4] == b".ra\xfd" {
        100
    } else {
        0
    }
}

/// Register the RealMedia container demuxer into a ContainerRegistry.
pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("rm", demuxer::open);
    reg.register_extension("rm", "rm");
    reg.register_extension("rmvb", "rm");
    reg.register_extension("ra", "rm");
    reg.register_probe("rm", probe);
}

/// Register the RealMedia container demuxer into a RuntimeContext.
pub fn register(ctx: &mut RuntimeContext) {
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("demux-rm", register);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_installs_demuxer() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        assert_eq!(ctx.containers.container_for_extension("rm"), Some("rm"));
        assert_eq!(ctx.containers.container_for_extension("rmvb"), Some("rm"));
        assert_eq!(ctx.containers.container_for_extension("ra"), Some("rm"));
    }

    #[test]
    fn probe_detects_magic() {
        let rmf = b".RMF\0\0\0\0";
        assert_eq!(probe(&ProbeData { buf: rmf, ext: None }), 100);
        let ra = b".ra\xfd\0\0\0\0";
        assert_eq!(probe(&ProbeData { buf: ra, ext: None }), 100);
        let junk = b"RIFF\0\0\0\0";
        assert_eq!(probe(&ProbeData { buf: junk, ext: None }), 0);
    }
}
