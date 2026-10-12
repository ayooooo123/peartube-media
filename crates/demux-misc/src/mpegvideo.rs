// Ported from FFmpeg libavformat/mpegvideodec.c (commit 2da55bf); packets
// are the frames of FFmpeg's mpegvideo parser (see rawvideo.rs).
// License: LGPL-2.1-or-later

use std::io::{Read, Seek, SeekFrom};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    ProbeData, ProbeScore, ReadSeek, Result, StreamInfo, TimeBase,
    PROBE_SCORE_EXTENSION,
};

use crate::rawvideo::{MpegVideo, RawVideoDemuxer, RAW_VIDEO_CLOCK};

const SEQ_START_CODE: u32 = 0x000001B3;
const PICTURE_START_CODE: u32 = 0x00000100;
const SLICE_START_CODE_MIN: u32 = 0x00000101;
const SLICE_START_CODE_MAX: u32 = 0x000001AF;
const PACK_START_CODE: u32 = 0x000001BA;
const VIDEO_ID: u32 = 0x000001E0;
const AUDIO_ID: u32 = 0x000001C0;

pub fn probe_mpegvideo(probe: &ProbeData) -> ProbeScore {
    let p = probe.buf;
    if p.len() < 16 {
        return 0;
    }

    let mut code: u32 = 0xFFFFFFFF;
    let mut pic = 0;
    let mut seq = 0;
    let mut slice = 0;
    let mut pspack = 0;
    let mut vpes = 0;
    let mut apes = 0;
    let mut res = 0;
    let mut sicle = 0;
    let mut last = 0;

    let mut i = 0;
    while i < p.len() {
        code = (code << 8) | (p[i] as u32);
        if (code & 0xFFFFFF00) == 0x100 {
            match code {
                SEQ_START_CODE => {
                    if i + 4 < p.len() && (p[i] & 0x20) != 0 {
                        seq += 1;
                    }
                }
                PICTURE_START_CODE => pic += 1,
                PACK_START_CODE => pspack += 1,
                0x1B6 => res += 1,
                _ => {}
            }
            if (SLICE_START_CODE_MIN..=SLICE_START_CODE_MAX).contains(&code) {
                if (SLICE_START_CODE_MIN..=SLICE_START_CODE_MAX).contains(&last) {
                    if code >= last {
                        slice += 1;
                    } else {
                        sicle += 1;
                    }
                } else if code == SLICE_START_CODE_MIN {
                    slice += 1;
                } else {
                    sicle += 1;
                }
            }
            if (code & 0x1F0) == VIDEO_ID {
                vpes += 1;
            } else if (code & 0x1E0) == AUDIO_ID {
                apes += 1;
            }
            last = code;
        }
        i += 1;
    }

    if seq > 0 && seq * 9 <= pic * 10 && pic * 9 <= slice * 10 && pspack == 0 && apes == 0 && res == 0 && slice > sicle {
        if vpes > 0 {
            PROBE_SCORE_EXTENSION / 4
        } else if pic > 1 {
            PROBE_SCORE_EXTENSION + 1
        } else {
            PROBE_SCORE_EXTENSION / 2
        }
    } else if probe.ext.is_some_and(|e| e == "m1v" || e == "m2v") && (seq > 0 || pic > 0) {
        PROBE_SCORE_EXTENSION
    } else {
        0
    }
}

/// ff_raw_video_read_header, with the stream parameters FFmpeg's parser
/// reports: MPEG-2 once a sequence extension follows the first sequence
/// header, and its size. Time base 1/1200000, as FFmpeg's: packets are
/// timed in it as FFmpeg times them (see rawvideo.rs), in fields, which
/// a repeated field can make one and a half frames.
pub fn open_mpegvideo(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let mut head = vec![0u8; 64 * 1024];
    let n = input.read(&mut head)?;
    if n < 8 {
        return Err(Error::invalid("mpegvideo: input too short"));
    }

    // Find SEQ_START_CODE
    let mut seq_offset = None;
    let mut is_mpeg2 = false;
    let mut width = 0;
    let mut height = 0;

    for i in 0..n.saturating_sub(7) {
        if head[i] == 0 && head[i + 1] == 0 && head[i + 2] == 1 && head[i + 3] == 0xB3 {
            seq_offset = Some(i);
            width = ((head[i + 4] as u32) << 4) | ((head[i + 5] as u32) >> 4);
            height = (((head[i + 5] as u32) & 0x0F) << 8) | (head[i + 6] as u32);
            break;
        }
    }

    let seq_pos = seq_offset.ok_or_else(|| Error::invalid("mpegvideo: no sequence header found"))?;

    // Check for sequence extension 0x000001B5
    for i in seq_pos..n.saturating_sub(4) {
        if head[i] == 0 && head[i + 1] == 0 && head[i + 2] == 1 && head[i + 3] == 0xB5 {
            let ext_id = (head[i + 4] >> 4) & 0x0F;
            if ext_id == 1 {
                is_mpeg2 = true;
                break;
            }
        }
    }

    input.seek(SeekFrom::Start(0))?;

    let codec_id = if is_mpeg2 {
        CodecId::new("mpeg2video")
    } else {
        CodecId::new("mpeg1video")
    };

    let mut params = CodecParameters::video(codec_id);
    params.width = Some(width);
    params.height = Some(height);

    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::new(1, RAW_VIDEO_CLOCK),
        duration: None,
        start_time: Some(0),
    };
    Ok(Box::new(RawVideoDemuxer::new("mpegvideo", input, stream, MpegVideo::default())))
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("mpegvideo", open_mpegvideo);
    reg.register_probe("mpegvideo", probe_mpegvideo);
    reg.register_extension("m1v", "mpegvideo");
    reg.register_extension("m2v", "mpegvideo");
    reg.register_extension("bs", "mpegvideo");
}
