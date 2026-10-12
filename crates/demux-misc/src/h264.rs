// Ported from FFmpeg libavformat/h264dec.c (commit 2da55bf); packets are
// the access units of FFmpeg's h264 parser (see rawvideo.rs).
// License: LGPL-2.1-or-later

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer,
    ProbeData, ProbeScore, ReadSeek, Result, StreamInfo, TimeBase,
    PROBE_SCORE_EXTENSION,
};

use crate::rawvideo::{RawVideoDemuxer, H264, RAW_VIDEO_CLOCK};

pub fn probe_h264(probe: &ProbeData) -> ProbeScore {
    let p = probe.buf;
    if p.len() < 16 {
        return 0;
    }

    let mut code: u32 = 0xFFFFFFFF;
    let mut sps = 0;
    let mut pps = 0;
    let mut idr = 0;
    let mut res = 0;
    let mut sli = 0;

    let mut i = 0;
    while i < p.len() {
        code = (code << 8) | (p[i] as u32);
        if (code & 0xFFFFFF00) == 0x100 {
            let ref_idc = (code >> 5) & 3;
            let nal_type = (code & 0x1F) as usize;

            if (code & 0x80) != 0 {
                // forbidden_zero_bit must be 0
                return 0;
            }

            match nal_type {
                1 => {
                    sli += 1;
                }
                5 => {
                    if ref_idc == 0 {
                        return 0;
                    }
                    idr += 1;
                }
                7 => {
                    if ref_idc == 0 {
                        return 0;
                    }
                    sps += 1;
                }
                8 => {
                    if ref_idc == 0 {
                        return 0;
                    }
                    pps += 1;
                }
                _ => {
                    if nal_type > 23 {
                        res += 1;
                    }
                }
            }
        }
        i += 1;
    }

    if sps > 0 && pps > 0 && (idr > 0 || sli > 3) && res < (sps + pps + idr) {
        PROBE_SCORE_EXTENSION + 1
    } else if probe.ext.is_some_and(|e| e == "h264" || e == "264" || e == "avc") && (sps > 0 || idr > 0 || sli > 0) {
        PROBE_SCORE_EXTENSION
    } else {
        0
    }
}

/// ff_raw_video_read_header: time base 1/1200000.
pub fn open_h264(
    input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let stream = StreamInfo {
        index: 0,
        params: CodecParameters::video(CodecId::new("h264")),
        time_base: TimeBase::new(1, 1200000),
        duration: None,
        start_time: Some(0),
    };
    Ok(Box::new(RawVideoDemuxer::new("h264", input, stream, H264::new((1, RAW_VIDEO_CLOCK)))))
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("h264", open_h264);
    reg.register_probe("h264", probe_h264);
    reg.register_extension("h264", "h264");
    reg.register_extension("264", "h264");
    reg.register_extension("avc", "h264");
}
