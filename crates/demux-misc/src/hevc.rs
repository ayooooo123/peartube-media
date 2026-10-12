// Ported from FFmpeg libavformat/hevcdec.c (commit 2da55bf); packets are
// the access units of FFmpeg's hevc parser (see rawvideo.rs).
// License: LGPL-2.1-or-later

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer,
    ProbeData, ProbeScore, ReadSeek, Result, StreamInfo, TimeBase,
    PROBE_SCORE_EXTENSION,
};

use crate::rawvideo::{Hevc, RawVideoDemuxer};

pub fn probe_hevc(probe: &ProbeData) -> ProbeScore {
    let p = probe.buf;
    if p.len() < 16 {
        return 0;
    }

    let mut code: u32 = 0xFFFFFFFF;
    let mut vps = 0;
    let mut sps = 0;
    let mut pps = 0;
    let mut irap = 0;

    let mut i = 0;
    while i < p.len() {
        code = (code << 8) | (p[i] as u32);
        if (code & 0xFFFFFF00) == 0x100
            && i + 1 < p.len() {
                let nal2 = p[i + 1];
                let nal_type = (code & 0x7E) >> 1;

                if (code & 0x81) != 0 {
                    return 0;
                }
                if (nal2 & 0xF8) != 0 {
                    return 0;
                }

                match nal_type {
                    32 => vps += 1,
                    33 => sps += 1,
                    34 => pps += 1,
                    16..=21 => irap += 1,
                    _ => {}
                }
            }
        i += 1;
    }

    if vps > 0 && sps > 0 && pps > 0 && irap > 0 {
        PROBE_SCORE_EXTENSION + 1
    } else if probe.ext.is_some_and(|e| e == "hevc" || e == "h265" || e == "265" || e == "bit") && (sps > 0 || vps > 0 || irap > 0) {
        PROBE_SCORE_EXTENSION
    } else {
        0
    }
}

/// ff_raw_video_read_header: time base 1/1200000.
pub fn open_hevc(
    input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let stream = StreamInfo {
        index: 0,
        params: CodecParameters::video(CodecId::new("hevc")),
        time_base: TimeBase::new(1, 1200000),
        duration: None,
        start_time: Some(0),
    };
    Ok(Box::new(RawVideoDemuxer::new("hevc", input, stream, Hevc::default())))
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("hevc", open_hevc);
    reg.register_probe("hevc", probe_hevc);
    reg.register_extension("hevc", "hevc");
    reg.register_extension("h265", "hevc");
    reg.register_extension("265", "hevc");
    reg.register_extension("bit", "hevc");
}
