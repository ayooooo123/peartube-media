// Ported from FFmpeg libavformat/mpegvideodec.c (commit 2da55bf)
// License: LGPL-2.1-or-later

use std::io::{Read, Seek, SeekFrom};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, ProbeData, ProbeScore, ReadSeek, Result, StreamInfo, TimeBase,
    PROBE_SCORE_EXTENSION,
};

const SEQ_START_CODE: u32 = 0x000001B3;
const PICTURE_START_CODE: u32 = 0x00000100;
const SLICE_START_CODE_MIN: u32 = 0x00000101;
const SLICE_START_CODE_MAX: u32 = 0x000001AF;
const PACK_START_CODE: u32 = 0x000001BA;
const VIDEO_ID: u32 = 0x000001E0;
const AUDIO_ID: u32 = 0x000001C0;

const FRAME_RATES: [(u32, u32); 9] = [
    (0, 0),
    (24000, 1001),
    (24, 1),
    (25, 1),
    (30000, 1001),
    (30, 1),
    (50, 1),
    (60000, 1001),
    (60, 1),
];

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

pub struct MpegVideoDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    buffer: Vec<u8>,
    pts: i64,
    eof_reached: bool,
    /// Whether the first picture (with any leading sequence header) shipped.
    first_picture_emitted: bool,
}

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
    let mut fps_num = 25;
    let mut fps_den = 1;

    for i in 0..n.saturating_sub(7) {
        if head[i] == 0 && head[i + 1] == 0 && head[i + 2] == 1 && head[i + 3] == 0xB3 {
            seq_offset = Some(i);
            width = ((head[i + 4] as u32) << 4) | ((head[i + 5] as u32) >> 4);
            height = (((head[i + 5] as u32) & 0x0F) << 8) | (head[i + 6] as u32);
            let fps_idx = (head[i + 7] & 0x0F) as usize;
            if fps_idx > 0 && fps_idx < FRAME_RATES.len() {
                fps_num = FRAME_RATES[fps_idx].0;
                fps_den = FRAME_RATES[fps_idx].1;
            }
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

    input.seek(SeekFrom::Start(seq_pos as u64))?;

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
        time_base: TimeBase::new(fps_den as i64, fps_num as i64),
        duration: None,
        start_time: Some(0),
    };

    Ok(Box::new(MpegVideoDemuxer {
        input,
        streams: vec![stream],
        buffer: Vec::with_capacity(64 * 1024),
        pts: 0,
        eof_reached: false,
        first_picture_emitted: false,
    }))
}

impl Demuxer for MpegVideoDemuxer {
    fn format_name(&self) -> &str {
        "mpegvideo"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let mut chunk = [0u8; 16 * 1024];

        // We want to return one picture per packet.
        // A picture packet starts at picture_start_code (or sequence_start_code on the first frame)
        // and ends before the NEXT picture_start_code or EOF.
        loop {
            // Check if we have a full picture in buffer
            if self.buffer.len() >= 4 {
                // Find where the first picture start code is
                let first_pic = self.find_picture_start(0);
                if let Some(first_pos) = first_pic {
                    // Look for the NEXT picture start code after first_pos + 4
                    if let Some(second_pos) = self.find_picture_start(first_pos + 4) {
                        // Any leading sequence/GOP header before the first
                        // picture groups with the first picture's packet (the
                        // way FFmpeg's parser groups it): when nothing has been
                        // emitted yet the packet starts at 0 and must reach the
                        // picture AFTER the headers, i.e. end at the second
                        // picture start.
                        let (start, end) = if !self.first_picture_emitted {
                            let end = self
                                .find_picture_start(second_pos + 4)
                                .unwrap_or(self.buffer.len());
                            (0, end)
                        } else {
                            (first_pos, second_pos)
                        };
                        self.first_picture_emitted = true;
                        let packet_data = self.buffer[start..end.max(start + 1)].to_vec();
                        self.buffer.drain(..end.max(start + 1));
                        let mut pkt = Packet {
                            stream_index: 0,
                            time_base: self.streams[0].time_base,
                            pts: Some(self.pts),
                            dts: Some(self.pts),
                            duration: Some(1),
                            flags: Default::default(),
                            data: packet_data,
                        };
                        pkt.flags.keyframe = true;
                        self.pts += 1;
                        return Ok(pkt);
                    }
                }
            }

            if self.eof_reached {
                // Flush the remainder as the final packet only when it holds
                // actual picture data; trailing header/end codes (sequence
                // end, padding) attach to nothing and are dropped.
                let has_picture = self.find_picture_start(0).is_some();
                if !self.buffer.is_empty() && has_picture {
                    let packet_data = std::mem::take(&mut self.buffer);
                    let mut pkt = Packet {
                        stream_index: 0,
                        time_base: self.streams[0].time_base,
                        pts: Some(self.pts),
                        dts: Some(self.pts),
                        duration: Some(1),
                        flags: Default::default(),
                        data: packet_data,
                    };
                    pkt.flags.keyframe = true;
                    self.pts += 1;
                    return Ok(pkt);
                }
                return Err(Error::Eof);
            }

            let n = self.input.read(&mut chunk)?;
            if n == 0 {
                self.eof_reached = true;
            } else {
                if self.buffer.len() + n > 16 * 1024 * 1024 {
                    return Err(Error::invalid("mpegvideo: frame size exceeded maximum"));
                }
                self.buffer.extend_from_slice(&chunk[..n]);
            }
        }
    }
}

impl MpegVideoDemuxer {
    fn find_picture_start(&self, from: usize) -> Option<usize> {
        if self.buffer.len() < from + 4 {
            return None;
        }
        (from..self.buffer.len() - 3).find(|&i| {
            self.buffer[i] == 0
                && self.buffer[i + 1] == 0
                && self.buffer[i + 2] == 1
                && (self.buffer[i + 3] == 0x00 || (from == 0 && self.buffer[i + 3] == 0xB3))
        })
    }
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("mpegvideo", open_mpegvideo);
    reg.register_probe("mpegvideo", probe_mpegvideo);
    reg.register_extension("m1v", "mpegvideo");
    reg.register_extension("m2v", "mpegvideo");
    reg.register_extension("bs", "mpegvideo");
}
