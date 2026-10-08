// The `tta` demuxer.
//
// Ported from FFmpeg (commit 2da55bf) libavformat/tta.c, and the leading
// ID3v2 skip FFmpeg applies to it (FF_INFMT_FLAG_ID3V2_AUTO:
// libavformat/id3v2.c ff_id3v2_match, id3v2_read_internal's loop and
// id3v2_parse's end; av_probe_input_format3's skip, ff_id3v2_tag_len).
// Copyright (c) 2006 Alex Beregszaszi (tta.c); LGPL-2.1-or-later (see
// LICENSE).

//! A TTA file: optional ID3v2 tags, the 22-byte `TTA1` header, a seek
//! table of one 32-bit size per frame and its CRC, then the frames. Every
//! frame but the last holds `256 * rate / 245` samples. Packets are the
//! frames in table order; a seek lands on the frame starting at or before
//! the target. Tags (ID3v1, APE) are not read.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, Packet, ProbeData, ReadSeek, Result,
    StreamInfo, TimeBase,
};

/// `AVPROBE_SCORE_EXTENSION + 30` on FFmpeg's 100-point scale.
const PROBE_SCORE: u8 = 80;
const ID3V2_HEADER_SIZE: usize = 10;

/// `ff_id3v2_match` with the "ID3" magic.
fn id3v2_match(b: &[u8]) -> bool {
    b.len() >= ID3V2_HEADER_SIZE
        && &b[0..3] == b"ID3"
        && b[3] != 0xFF
        && b[4] != 0xFF
        && b[6..10].iter().all(|&x| x & 0x80 == 0)
}

/// The tag's size after its 10-byte header.
fn id3v2_len(b: &[u8]) -> u64 {
    b[6..10].iter().fold(0u64, |len, &x| len << 7 | u64::from(x & 0x7F))
}

/// `tta_probe`, after skipping a leading ID3v2 tag as FFmpeg's probe does
/// (`ff_id3v2_tag_len`: the footer counts for any version).
pub fn probe(p: &ProbeData) -> u8 {
    let mut b = p.buf;
    if b.len() > ID3V2_HEADER_SIZE && id3v2_match(b) {
        let mut len = ID3V2_HEADER_SIZE as u64 + id3v2_len(b);
        if b[5] & 0x10 != 0 {
            len += ID3V2_HEADER_SIZE as u64;
        }
        if (b.len() as u64) <= len + 16 {
            return 0;
        }
        b = &b[len as usize..];
    }
    if b.len() < 14 {
        return 0;
    }
    let le16 = |at: usize| u16::from_le_bytes([b[at], b[at + 1]]);
    let le32 = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    if &b[0..4] == b"TTA1" && (le16(4) == 1 || le16(4) == 2) && le16(6) > 0 && le16(8) > 0 && le32(10) > 0 {
        PROBE_SCORE
    } else {
        0
    }
}

/// Skips the ID3v2 tags at the start: `id3v2_read_internal` reads tags
/// while they follow one another, each ending after its size and, for
/// version 4 with the footer flag, 10 more bytes.
fn skip_id3v2(input: &mut Box<dyn ReadSeek>) -> Result<()> {
    loop {
        let start = input.stream_position()?;
        let mut head = Vec::with_capacity(ID3V2_HEADER_SIZE);
        input.take(ID3V2_HEADER_SIZE as u64).read_to_end(&mut head)?;
        if !id3v2_match(&head) {
            input.seek(SeekFrom::Start(start))?;
            return Ok(());
        }
        let mut end = start + ID3V2_HEADER_SIZE as u64 + id3v2_len(&head);
        if head[3] == 4 && head[5] & 0x10 != 0 {
            end += ID3V2_HEADER_SIZE as u64;
        }
        input.seek(SeekFrom::Start(end))?;
    }
}

/// `tta_read_header`
pub fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    skip_id3v2(&mut input)?;
    let mut header = [0u8; 22];
    input.read_exact(&mut header).map_err(|_| Error::invalid("tta: truncated header"))?;
    if &header[0..4] != b"TTA1" {
        return Err(Error::invalid("tta: no TTA1 header"));
    }
    let le16 = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]);
    let le32 = |at: usize| u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]]);
    let channels = le16(6);
    let sample_rate = le32(10) as i32;
    if sample_rate <= 0 || sample_rate > 1_000_000 {
        return Err(Error::invalid("tta: nonsense samplerate"));
    }
    let nb_samples = le32(14);
    if nb_samples == 0 {
        return Err(Error::invalid("tta: invalid number of samples"));
    }
    let frame_size = sample_rate as u32 * 256 / 245;
    let mut last_frame_size = nb_samples % frame_size;
    if last_frame_size == 0 {
        last_frame_size = frame_size;
    }
    let total_frames = u64::from(nb_samples / frame_size) + u64::from(last_frame_size < frame_size);
    if total_frames >= (i32::MAX as u64 - 4) / 4 {
        return Err(Error::invalid(format!("tta: totalframes {total_frames} invalid")));
    }

    // The seek table: each size read from the file, so a table longer than
    // the file stops at its end instead of being allocated up front.
    let mut sizes = Vec::new();
    let mut entry = [0u8; 4];
    for _ in 0..total_frames {
        input.read_exact(&mut entry).map_err(|_| Error::invalid("tta: truncated seek table"))?;
        sizes.push(u32::from_le_bytes(entry));
    }
    // The table's CRC (checked only with -err_detect crccheck).
    let mut crc = Vec::with_capacity(4);
    (&mut input).take(4).read_to_end(&mut crc)?;
    let data_start = input.stream_position()?;

    let mut params = CodecParameters::audio(CodecId::new("tta"));
    params.channels = Some(channels);
    params.sample_rate = Some(sample_rate as u32);
    params.extradata = header.to_vec();
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, i64::from(sample_rate)),
        duration: Some(i64::from(nb_samples)),
        start_time: Some(0),
        params,
    };
    Ok(Box::new(TtaDemuxer {
        input,
        stream,
        sizes,
        frame_size,
        last_frame_size,
        data_start,
        current: 0,
    }))
}

struct TtaDemuxer {
    input: Box<dyn ReadSeek>,
    stream: StreamInfo,
    /// The seek table: each frame's size.
    sizes: Vec<u32>,
    frame_size: u32,
    last_frame_size: u32,
    /// Where the first frame starts.
    data_start: u64,
    /// The next frame.
    current: usize,
}

impl Demuxer for TtaDemuxer {
    fn format_name(&self) -> &str {
        "tta"
    }

    fn streams(&self) -> &[StreamInfo] {
        std::slice::from_ref(&self.stream)
    }

    /// `tta_read_packet`: the next frame, as many of its bytes as the file
    /// still has.
    fn next_packet(&mut self) -> Result<Packet> {
        let Some(&size) = self.sizes.get(self.current) else { return Err(Error::Eof) };
        let mut data = Vec::new();
        (&mut self.input).take(u64::from(size)).read_to_end(&mut data)?;
        if data.is_empty() && size > 0 {
            return Err(Error::Eof);
        }
        let pts = self.current as i64 * i64::from(self.frame_size);
        self.current += 1;
        let duration = if self.current == self.sizes.len() { self.last_frame_size } else { self.frame_size };
        let mut packet = Packet::new(0, self.stream.time_base, data);
        packet.pts = Some(pts);
        packet.dts = Some(pts);
        packet.duration = Some(i64::from(duration));
        packet.flags.keyframe = true;
        Ok(packet)
    }

    /// `tta_read_seek`: the frame starting at or before `pts` (the first
    /// one before the start).
    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        if self.sizes.is_empty() {
            return Err(Error::invalid("tta: no frames"));
        }
        let frame = (pts.max(0) / i64::from(self.frame_size)) as usize;
        let frame = frame.min(self.sizes.len() - 1);
        let pos = self.data_start + self.sizes[..frame].iter().map(|&s| u64::from(s)).sum::<u64>();
        self.input.seek(SeekFrom::Start(pos))?;
        self.current = frame;
        Ok(frame as i64 * i64::from(self.frame_size))
    }
}

/// Registers the `tta` demuxer with its `.tta` extension and probe.
pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("tta", open);
    reg.register_extension("tta", "tta");
    reg.register_probe("tta", probe);
}
