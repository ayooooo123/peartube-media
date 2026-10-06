// Ported from FFmpeg libavformat/hevcdec.c (commit 2da55bf)
// License: LGPL-2.1-or-later

use std::io::Read;
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, ProbeData, ProbeScore, ReadSeek, Result, StreamInfo, TimeBase,
    PROBE_SCORE_EXTENSION,
};

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

pub struct HevcDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    buffer: Vec<u8>,
    pts: i64,
    eof_reached: bool,
    /// Byte offset where the next AU begins (already scanned), if found.
    pending_au: Option<usize>,
    /// Whether any VCL NAL of the current AU has been seen.
    has_current_slice: bool,
    /// Bytes of `buffer` already walked by `scan_aus`.
    scanned_upto: usize,
    /// Whether the current AU starts with an IRAP picture.
    au_keyframe: bool,
}

pub fn open_hevc(
    input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let params = CodecParameters::video(CodecId::new("hevc"));
    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::new(1, 1200000),
        duration: None,
        start_time: Some(0),
    };

    Ok(Box::new(HevcDemuxer {
        input,
        streams: vec![stream],
        buffer: Vec::with_capacity(64 * 1024),
        pts: 0,
        eof_reached: false,
        pending_au: None,
        has_current_slice: false,
        scanned_upto: 0,
        au_keyframe: false,
    }))
}

impl Demuxer for HevcDemuxer {
    fn format_name(&self) -> &str {
        "hevc"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    /// One packet = one access unit: NALs up to (but excluding) the next NAL
    /// that starts a new picture. FFmpeg's hevc parser flushes the pending AU
    /// at VPS/SPS/PPS/AUD (parameter sets) and at each new VCL slice
    /// (first_slice_segment_in_pic_flag == 1).
    fn next_packet(&mut self) -> Result<Packet> {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            if let Some(boundary) = self.pending_au.take() {
                let packet_data = self.buffer[..boundary].to_vec();
                self.buffer.drain(..boundary);
                self.scanned_upto = 0;
                let mut pkt = Packet {
                    stream_index: 0,
                    time_base: self.streams[0].time_base,
                    pts: Some(self.pts),
                    dts: Some(self.pts),
                    duration: Some(1),
                    flags: Default::default(),
                    data: packet_data,
                };
                pkt.flags.keyframe = self.au_keyframe;
                self.au_keyframe = false;
                self.has_current_slice = false;
                self.pts += 1;
                return Ok(pkt);
            }

            if self.eof_reached {
                self.scan_aus();
                if self.pending_au.is_some() {
                    continue;
                }
                if !self.buffer.is_empty() {
                    let packet_data = std::mem::take(&mut self.buffer);
                    self.scanned_upto = 0;
                    let mut pkt = Packet {
                        stream_index: 0,
                        time_base: self.streams[0].time_base,
                        pts: Some(self.pts),
                        dts: Some(self.pts),
                        duration: Some(1),
                        flags: Default::default(),
                        data: packet_data,
                    };
                    pkt.flags.keyframe = self.au_keyframe;
                    self.au_keyframe = false;
                    self.has_current_slice = false;
                    self.pts += 1;
                    return Ok(pkt);
                }
                return Err(Error::Eof);
            }

            let n = self.input.read(&mut chunk)?;
            if n == 0 {
                self.eof_reached = true;
                self.scan_aus();
                continue;
            }
            if self.buffer.len() + n > 32 * 1024 * 1024 {
                return Err(Error::invalid("hevc: packet size exceeded maximum"));
            }
            self.buffer.extend_from_slice(&chunk[..n]);
            self.scan_aus();
        }
    }
}

impl HevcDemuxer {
    /// Walk the unscanned tail of `buffer` and set `pending_au` when a NAL
    /// starts a new AU after the current one has slices.
    fn scan_aus(&mut self) {
        if self.pending_au.is_some() {
            return;
        }
        let mut have_slice = self.has_current_slice;
        let mut i = self.scanned_upto;
        let b = &self.buffer;
        while i + 5 < b.len() {
            let (sc_len, nal_byte_idx) = if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
                (3, i + 3)
            } else if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 0 && b[i + 3] == 1 {
                (4, i + 4)
            } else {
                i += 1;
                continue;
            };
            let nal_type = (b[nal_byte_idx] & 0x7E) >> 1;
            match nal_type {
                // VPS / SPS / PPS / AUD: parameter sets end the current AU.
                32..=35 => {
                    if i > 0 && have_slice {
                        self.pending_au = Some(i);
                        return;
                    }
                }
                // VCL: first_slice_segment_in_pic_flag is the first payload
                // bit (bit 7 of the second header byte).
                0..=31 => {
                    let first_slice = nal_byte_idx + 2 < b.len() && (b[nal_byte_idx + 2] & 0x80) != 0;
                    if i > 0 && have_slice && first_slice {
                        self.pending_au = Some(i);
                        return;
                    }
                    if (16..=23).contains(&nal_type) {
                        self.au_keyframe = true;
                    }
                    self.has_current_slice = true;
                    have_slice = true;
                }
                _ => {}
            }
            i += sc_len;
        }
        // A start code may straddle the buffer end. Rewind to the earliest
        // byte that could begin one so it is re-detected after the next fill.
        if b.is_empty() {
            return;
        }
        let start = i.saturating_sub(3);
        let mut rewind = 0usize;
        for k in (start..=i).rev() {
            if b[k] == 0 {
                rewind += 1;
            } else if b[k] == 1 && rewind >= 2 {
                rewind += 1;
                break;
            } else {
                break;
            }
        }
        self.scanned_upto = i + 1 - rewind.min(i + 1);
    }
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("hevc", open_hevc);
    reg.register_probe("hevc", probe_hevc);
    reg.register_extension("hevc", "hevc");
    reg.register_extension("h265", "hevc");
    reg.register_extension("265", "hevc");
    reg.register_extension("bit", "hevc");
}
