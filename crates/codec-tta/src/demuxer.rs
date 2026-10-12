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
const MAX_FRAME_MEMORY: u64 = 256 * 1024 * 1024;

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
    let bytes_per_sample = le16(8).div_ceil(8);
    if le16(4) > 2 || channels == 0 || channels > 16 || !(1..=3).contains(&bytes_per_sample) {
        return Err(Error::invalid("tta: invalid format, channels or sample width"));
    }
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

    // Rice unary codes can exceed the PCM size, so that is not a valid
    // compressed-size limit. Bound the packet by the contract's memory
    // budget after reserving the seek table and a full decoded frame:
    // interleaved i32 working samples plus U8, S16 or S32 output.
    let output_bytes = if bytes_per_sample == 3 { 4 } else { u64::from(bytes_per_sample) };
    let pcm_bytes = u64::from(frame_size) * u64::from(channels) * (4 + output_bytes);
    let table_bytes = total_frames * 4;
    let packet_limit = MAX_FRAME_MEMORY.checked_sub(pcm_bytes)
        .and_then(|n| n.checked_sub(table_bytes))
        .ok_or_else(|| Error::invalid("tta: declared frame and seek table exceed the memory budget"))?;

    // The seek table: each size read from the file, so a table longer than
    // the file stops at its end instead of being allocated up front.
    let mut sizes = Vec::new();
    let mut entry = [0u8; 4];
    for _ in 0..total_frames {
        input.read_exact(&mut entry).map_err(|_| Error::invalid("tta: truncated seek table"))?;
        let size = u32::from_le_bytes(entry);
        if u64::from(size) > packet_limit {
            return Err(Error::invalid("tta: seek-table frame exceeds the memory budget"));
        }
        // Grow only as bytes arrive, without rounding beyond the table's
        // declared size (already charged to the budget).
        if sizes.len() == sizes.capacity() {
            let capacity = (sizes.capacity() * 2).max(1024).min(total_frames as usize);
            sizes.reserve_exact(capacity - sizes.len());
        }
        sizes.push(size);
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
        // Sizes were validated at open. Read only that many bytes, growing
        // on demand without read_to_end's allocation rounding past the cap.
        let size = size as usize;
        let mut data = Vec::new();
        while data.len() < size {
            let start = data.len();
            let end = (start + 8192).min(size);
            if end > data.capacity() {
                let capacity = (data.capacity() * 2).max(end).min(size);
                data.reserve_exact(capacity - start);
            }
            data.resize(end, 0);
            match self.input.read(&mut data[start..]) {
                Ok(n) => {
                    data.truncate(start + n);
                    if n == 0 {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => data.truncate(start),
                Err(e) => return Err(e.into()),
            }
        }
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

#[cfg(test)]
#[path = "../tests/memory/mod.rs"]
mod memory;
