// Ported from FFmpeg libavformat/h264dec.c (commit 2da55bf)
// License: LGPL-2.1-or-later

use std::io::Read;
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, ProbeData, ProbeScore, ReadSeek, Result, StreamInfo, TimeBase,
    PROBE_SCORE_EXTENSION,
};

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

pub struct H264Demuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    buffer: Vec<u8>,
    pts: i64,
    eof_reached: bool,
    /// Byte offset where the next AU begins (already scanned), if found.
    pending_au: Option<usize>,
    /// Whether the current (not yet emitted) AU starts with a keyframe.
    au_keyframe: bool,
    /// Whether any VCL NAL of the current AU has been seen.
    has_current_slice: bool,
    /// Bytes of `buffer` already walked by `scan_aus`.
    scanned_upto: usize,
    /// Last first_mb_in_slice in the current AU (h264 parser's monotonic rule).
    last_first_mb: u32,
}

pub fn open_h264(
    input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let params = CodecParameters::video(CodecId::new("h264"));
    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::new(1, 1200000),
        duration: None,
        start_time: Some(0),
    };

    Ok(Box::new(H264Demuxer {
        input,
        streams: vec![stream],
        buffer: Vec::with_capacity(64 * 1024),
        pts: 0,
        eof_reached: false,
        pending_au: None,
        au_keyframe: false,
        has_current_slice: false,
        scanned_upto: 0,
        last_first_mb: 0,
    }))
}

impl Demuxer for H264Demuxer {
    fn format_name(&self) -> &str {
        "h264"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    /// One packet = one access unit: NALs up to (but excluding) the next
    /// NAL that starts a new picture. FFmpeg's h264 parser flushes the
    /// pending AU when it sees AUD, SPS, or a VCL NAL with
    /// first_mb_in_slice == 0 while slices of the current AU were seen.
    fn next_packet(&mut self) -> Result<Packet> {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            // If we hold a complete AU (we saw its slice(s) and then a
            // boundary), emit it.
            if let Some(boundary) = self.pending_au.take() {
                let packet_data = self.buffer[..boundary].to_vec();
                self.buffer.drain(..boundary);
                self.scanned_upto = 0;
                self.last_first_mb = 0;
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
                // The tail may hold unemitted AUs; scan once more before
                // flushing the remainder.
                self.scan_aus();
                if self.pending_au.is_some() {
                    continue;
                }
                if !self.buffer.is_empty() {
                    let packet_data = std::mem::take(&mut self.buffer);
                    self.scanned_upto = 0;
                    self.last_first_mb = 0;
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
                // The tail may still contain unemitted AUs.
                self.scan_aus();
                continue;
            }
            if self.buffer.len() + n > 32 * 1024 * 1024 {
                return Err(Error::invalid("h264: packet size exceeded maximum"));
            }
            self.buffer.extend_from_slice(&chunk[..n]);
            self.scan_aus();
        }
    }
}

/// Read a ue(v) Exp-Golomb value starting at byte `pos`, stopping when the
/// value cannot complete in the buffer; None = not decodable yet.
fn read_ue_golomb(b: &[u8], pos: usize) -> Option<u32> {
    let mut leading_zeros = 0u32;
    let mut i = pos;
    loop {
        if i >= b.len() {
            return None;
        }
        let byte = b[i];
        if byte == 0 {
            leading_zeros += 8;
            i += 1;
            if leading_zeros > 32 {
                return None;
            }
            continue;
        }
        // first set bit
        let lz = byte.leading_zeros();
        leading_zeros += lz;
        // value = 2^lz - 1 + read(lz bits after the leading 1)
        if lz == 8 {
            leading_zeros += 0;
        }
        let mut value: u32 = 0;
        let mut bits_needed = lz; // bits after the marker bit
        // consume marker bit's remaining bits in this byte
        let mut bitpos = lz + 1;
        while bits_needed > 0 {
            if bitpos >= 8 {
                i += 1;
                if i >= b.len() {
                    return None;
                }
                bitpos = 0;
                continue;
            }
            let bit = (b[i] >> (7 - bitpos)) & 1;
            value = (value << 1) | u32::from(bit);
            bits_needed -= 1;
            bitpos += 1;
        }
        let _ = leading_zeros;
        return Some((1u32 << lz).wrapping_sub(1).wrapping_add(value));
    }
}

impl H264Demuxer {
    /// Walk the unscanned tail of `buffer` and set `pending_au` when a NAL
    /// starts a new AU after the current one has slices.
    fn scan_aus(&mut self) {
        if self.pending_au.is_some() {
            return;
        }
        let mut have_slice = self.has_current_slice;
        let mut i = self.scanned_upto;
        let b = &self.buffer;
        while i + 4 < b.len() {
            let (sc_len, nal_byte_idx) = if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
                (3, i + 3)
            } else if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 0 && b[i + 3] == 1 {
                (4, i + 4)
            } else {
                i += 1;
                continue;
            };
            let nal_type = b[nal_byte_idx] & 0x1F;
            if nal_type == 5 {
                self.au_keyframe = true;
            }
            match nal_type {
                // SEI / SPS / PPS / AUD: parameter sets end the current AU;
                // everything from here on belongs to the next picture.
                6..=9 => {
                    if i > 0 && have_slice {
                        self.pending_au = Some(i);
                        return;
                    }
                }
                // VCL: first_mb_in_slice ue(v); a new picture starts when it
                // is <= the previous slice's (monotonic within an AU), like
                // FFmpeg's h264 parser.
                1 | 5 => {
                    let first_mb = read_ue_golomb(b, nal_byte_idx + 1);
                    if let Some(mb) = first_mb {
                        if i > 0 && have_slice && mb <= self.last_first_mb {
                            self.pending_au = Some(i);
                            self.last_first_mb = mb;
                            return;
                        }
                        self.last_first_mb = mb;
                    }
                    self.has_current_slice = true;
                    have_slice = true;
                }
                _ => {}
            }
            i += sc_len;
        }
        // A start code may straddle the buffer end. Rewind to the earliest
        // byte that could begin one: a run of up to 3 trailing zeros, or a
        // '01' byte preceded by two zeros (i.e. we stopped right at the third
        // byte of 00 00 01). No NAL decision is replayed — the walk resumes
        // only on undetected start codes.
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
    reg.register_demuxer("h264", open_h264);
    reg.register_probe("h264", probe_h264);
    reg.register_extension("h264", "h264");
    reg.register_extension("264", "h264");
    reg.register_extension("avc", "h264");
}
