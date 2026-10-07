// Standard MIDI File demuxer — no FFmpeg code: libavformat has no SMF
// demuxer, so this follows the MIDI 1.0 spec (SMF, RP-001/v95.1).
// License: MIT (workspace).
//
// The whole file is emitted as one packet for codec `midi`, which is how
// OxideAV's midi decoder consumes SMF blobs (one send_packet per song).
// Header must be "MThd" with a 6-byte length; every subsequent chunk is
// "MTrk" (kept) or an unknown chunk (skipped, per spec).

use std::io::Read;
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    MediaType, Packet, ProbeData, ProbeScore, ReadSeek, Result, StreamInfo,
    TimeBase, MAX_PROBE_SCORE,
};

pub fn probe_smf(probe: &ProbeData) -> ProbeScore {
    let p = probe.buf;
    if p.len() < 14 {
        return 0;
    }
    if &p[0..4] == b"MThd" && u32::from_be_bytes([p[4], p[5], p[6], p[7]]) == 6 {
        let format = u16::from_be_bytes([p[8], p[9]]);
        if format > 2 {
            return 0;
        }
        MAX_PROBE_SCORE
    } else {
        0
    }
}

struct SmfDemuxer {
    data: Vec<u8>,
    stream: StreamInfo,
    sent: bool,
}

pub fn open_smf(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let mut data = Vec::new();
    input.read_to_end(&mut data)?;
    if data.len() < 14 {
        return Err(Error::invalid("smf: file too short"));
    }
    if &data[0..4] != b"MThd" {
        return Err(Error::invalid("smf: bad magic"));
    }
    let mthd_len = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
    if mthd_len != 6 || data.len() < 8 + mthd_len {
        return Err(Error::invalid("smf: bad header length"));
    }
    let format = u16::from_be_bytes([data[8], data[9]]);
    let ntrks = u16::from_be_bytes([data[10], data[11]]) as usize;
    if format > 2 || ntrks == 0 {
        return Err(Error::invalid("smf: unsupported format or track count"));
    }

    // Validate chunk structure per spec; cap total size for untrusted input.
    if data.len() > 256 * 1024 * 1024 {
        return Err(Error::invalid("smf: file too large"));
    }
    let mut pos = 8 + mthd_len;
    let mut tracks = 0usize;
    while pos + 8 <= data.len() {
        let len = u32::from_be_bytes([
            data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7],
        ]) as usize;
        let chunk_type = &data[pos..pos + 4];
        if chunk_type == b"MTrk" {
            tracks += 1;
        }
        pos += 8 + len;
        if pos > data.len() {
            // A truncated final chunk: keep what we have, the decoder can
            // still play the tracks before it.
            break;
        }
    }
    if tracks == 0 {
        return Err(Error::invalid("smf: no MTrk chunks"));
    }

    let mut params = CodecParameters::data(CodecId::new("midi"));
    params.media_type = MediaType::Data;
    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::new(1, 44100),
        duration: None,
        start_time: Some(0),
    };

    Ok(Box::new(SmfDemuxer {
        data,
        stream,
        sent: false,
    }))
}

impl Demuxer for SmfDemuxer {
    fn format_name(&self) -> &str {
        "smf"
    }

    fn streams(&self) -> &[StreamInfo] {
        std::slice::from_ref(&self.stream)
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if self.sent {
            return Err(Error::Eof);
        }
        self.sent = true;
        // Kept for a later seek: the song is a few kilobytes as a rule.
        let mut pkt = Packet::new(0, self.stream.time_base, self.data.clone());
        pkt.pts = Some(0);
        pkt.dts = Some(0);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }

    /// libavformat has no SMF demuxer, so no FFmpeg landing to follow:
    /// the song is one packet, its only random access point, and any seek
    /// hands it out again from its start (the decoder renders up to the
    /// target, which the player then drops).
    fn seek_to(&mut self, _stream_index: u32, _pts: i64) -> Result<i64> {
        self.sent = false;
        Ok(0)
    }
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("smf", open_smf);
    reg.register_probe("smf", probe_smf);
    reg.register_extension("mid", "smf");
    reg.register_extension("midi", "smf");
    reg.register_extension("smf", "smf");
}
