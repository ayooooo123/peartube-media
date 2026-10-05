//! Pure-Rust ASF (Advanced Systems Format) container demuxer.
//! Ported from FFmpeg commit 2da55bf (libavformat/asfdec_f.c, asf.c, asf.h, asf_tags.c).
//! Licensed under LGPL-2.1-or-later.

#![forbid(unsafe_code)]

pub mod demuxer;
pub mod guid;

use oxideav_core::{ContainerRegistry, ProbeData};

pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("asf", demuxer::AsfDemuxer::open);
    reg.register_extension("asf", "asf");
    reg.register_extension("wmv", "asf");
    reg.register_extension("wma", "asf");
    reg.register_probe("asf", probe);
}

pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("demux-asf", register);

fn probe(p: &ProbeData) -> u8 {
    if p.buf.len() >= 16 && p.buf[0..16] == guid::ASF_HEADER {
        100
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registration() {
        let mut ctx = oxideav_core::RuntimeContext::new();
        register(&mut ctx);
        assert_eq!(ctx.containers.container_for_extension("asf"), Some("asf"));
        assert_eq!(ctx.containers.container_for_extension("wmv"), Some("asf"));
        assert_eq!(ctx.containers.container_for_extension("wma"), Some("asf"));
    }

    #[test]
    fn test_probe() {
        let mut data = vec![0u8; 32];
        data[0..16].copy_from_slice(&guid::ASF_HEADER);
        let pd = ProbeData {
            buf: &data,
            ext: Some("asf"),
        };
        assert_eq!(probe(&pd), 100);

        let pd_none = ProbeData {
            buf: &[0u8; 16],
            ext: None,
        };
        assert_eq!(probe(&pd_none), 0);
    }
}
