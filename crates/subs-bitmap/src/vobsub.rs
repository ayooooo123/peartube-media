//! Paired VobSub index/program-stream demuxer. Port of FFmpeg
//! libavformat/mpeg.c (vobsub_* and subtitle PES extraction) and
//! subtitles.c (queue ordering/durations), commit 2da55bf; both headers
//! verified LGPL-2.1-or-later. The caller supplies both byte sources;
//! an untrusted index can never select a filesystem or network path.

use std::io::{BufReader, Read, Seek, SeekFrom};
use oxideav_core::{CodecId, CodecParameters, Demuxer, Error, Packet, ReadSeek, Result, StreamInfo, TimeBase};

const MAX_INDEX: u64 = 8 << 20;
const MAX_PACKET: usize = 1 << 20;
const TB: TimeBase = TimeBase::new(1, 1000);
fn invalid() -> Error { Error::invalid("VobSub: malformed index or program-stream packet") }

#[derive(Clone, Copy)]
struct Entry { pts: i64, position: u64 }
struct Track { id: u8, entries: Vec<Entry>, next: usize }
struct VobSub { input: BufReader<Box<dyn ReadSeek>>, size: u64, streams: Vec<StreamInfo>, tracks: Vec<Track> }

fn timestamp(value: &str) -> Result<i64> {
    let mut parts = value.trim().split(':');
    let mut next = || parts.next().ok_or_else(invalid)?.parse::<i64>().map_err(|_| invalid());
    let (h, m, s, ms) = (next()?, next()?, next()?, next()?);
    if h < 0 || !(0..60).contains(&m) || !(0..60).contains(&s) || !(0..1000).contains(&ms) || parts.next().is_some() { return Err(invalid()); }
    h.checked_mul(3_600_000).and_then(|v| v.checked_add(m * 60_000 + s * 1000 + ms)).ok_or_else(invalid)
}

/// Opens a VobSub pair without guessing filenames or reading any path from
/// the index. Packet timestamps use milliseconds; split SPUs remain split
/// at the index's file positions, exactly as FFmpeg hands them to dvdsub.
pub fn open_vobsub(idx: Box<dyn ReadSeek>, mut sub: Box<dyn ReadSeek>) -> Result<Box<dyn Demuxer>> {
    let mut text = String::new();
    idx.take(MAX_INDEX + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_INDEX || !text.starts_with("# VobSub index file,") { return Err(invalid()); }
    let size = sub.seek(SeekFrom::End(0))?;
    let mut streams = Vec::new();
    let mut tracks: Vec<Track> = Vec::new();
    let mut header = String::new();
    let mut language = String::new();
    let mut stream_id = None;
    let mut delay = 0i64;
    let mut header_done = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("id:") {
            let (id, index) = value.split_once(", index:").ok_or_else(invalid)?;
            let index = index.trim().parse::<u8>().map_err(|_| invalid())?;
            if index >= 32 { return Err(invalid()); }
            stream_id = Some(index); language.clear(); language.push_str(id.trim()); header_done = true;
        } else if let Some(value) = line.strip_prefix("timestamp:") {
            let id = stream_id.ok_or_else(invalid)?;
            let (time, position) = value.split_once(", filepos:").ok_or_else(invalid)?;
            let pts = timestamp(time)?.checked_add(delay).ok_or_else(invalid)?;
            let position = u64::from_str_radix(position.trim(), 16).map_err(|_| invalid())?;
            if position > size { return Err(invalid()); }
            let track = if let Some(index) = tracks.iter().position(|track| track.id == id) { index } else {
                let index = tracks.len();
                streams.push(StreamInfo {
                    index: index as u32, time_base: TB, duration: None, start_time: None,
                    params: CodecParameters::subtitle(CodecId::new(crate::DVD_CODEC_ID)).with_language(language.clone()),
                });
                tracks.push(Track { id, entries: Vec::new(), next: 0 });
                index
            };
            tracks[track].entries.push(Entry { pts, position });
        } else if let Some(value) = line.strip_prefix("delay:") {
            let value = value.trim();
            let (sign, value) = if let Some(value) = value.strip_prefix('-') { (-1, value) } else { (1, value.trim_start_matches('+')) };
            delay = timestamp(value)?.checked_mul(sign).ok_or_else(invalid)?;
        } else if line.starts_with("alt:") {
            header_done = true;
        } else if !header_done && !line.is_empty() && !line.starts_with('#') && !line.starts_with("langidx:") {
            header.push_str(line); header.push('\n');
        }
    }
    if tracks.is_empty() { return Err(invalid()); }
    for (stream, track) in streams.iter_mut().zip(&mut tracks) {
        track.entries.sort_by_key(|entry| (entry.position, entry.pts));
        stream.params.extradata = header.as_bytes().to_vec();
        stream.start_time = track.entries.first().map(|entry| entry.pts);
    }
    Ok(Box::new(VobSub { input: BufReader::new(sub), size, streams, tracks }))
}

/// Offset of the elementary payload in one MPEG-1/2 PES body (the two-byte
/// PES length is outside `body`). Timestamp and extension bytes are skipped,
/// since the index, not the PES clock, supplies paired VobSub timestamps.
fn payload_offset(body: &[u8]) -> Result<usize> {
    let mut at = 0;
    while body.get(at) == Some(&0xff) { at += 1; }
    let mut flags = *body.get(at).ok_or_else(invalid)?;
    if flags & 0xc0 == 0x40 { at += 2; flags = *body.get(at).ok_or_else(invalid)?; }
    if flags & 0xe0 == 0x20 {
        at += if flags & 0x10 != 0 { 10 } else { 5 };
    } else if flags & 0xc0 == 0x80 {
        at += 3 + usize::from(*body.get(at + 2).ok_or_else(invalid)?);
    } else if flags == 0x0f { at += 1; } else { return Err(invalid()); }
    if at >= body.len() { return Err(invalid()); }
    Ok(at)
}

impl VobSub {
    fn payload(&mut self, start: u64, end: u64, id: u8) -> Result<Vec<u8>> {
        self.input.seek(SeekFrom::Start(start))?;
        let mut out = Vec::new();
        let mut state = u32::MAX;
        let mut position = start;
        let mut body = Vec::new();
        while position < end {
            let mut byte = [0];
            if self.input.read(&mut byte)? == 0 { break; }
            position += 1;
            state = (state << 8) | u32::from(byte[0]);
            if state & 0xffffff00 != 0x00000100 { continue; }
            let code = state as u8;
            if matches!(code, 0xba | 0xbb) { continue; }
            if !(0xbc..=0xff).contains(&code) { continue; }
            if end - position < 2 { break; }
            let mut length = [0; 2]; self.input.read_exact(&mut length)?; position += 2;
            let length = usize::from(u16::from_be_bytes(length));
            if length as u64 > end - position { break; }
            body.resize(length, 0);
            self.input.read_exact(&mut body)?; position += length as u64;
            state = u32::MAX;
            if code != 0xbd { continue; }
            let at = payload_offset(&body)?;
            if body[at] & 0x1f != id { break; }
            let payload = &body[at + 1..];
            if payload.len() > MAX_PACKET - out.len() { return Err(invalid()); }
            out.extend_from_slice(payload);
        }
        // FFmpeg raises EOF when an index entry points beyond the final PES,
        // rather than returning a synthetic empty subtitle packet.
        if out.is_empty() && position >= self.size { return Err(Error::Eof); }
        Ok(out)
    }
}

impl Demuxer for VobSub {
    fn format_name(&self) -> &str { "vobsub" }
    fn streams(&self) -> &[StreamInfo] { &self.streams }
    fn next_packet(&mut self) -> Result<Packet> {
        let index = self.tracks.iter().enumerate().filter_map(|(i, track)| track.entries.get(track.next).map(|entry| (i, entry.pts))).min_by_key(|&(i, pts)| (pts, i)).map(|(i, _)| i).ok_or(Error::Eof)?;
        let track = &mut self.tracks[index];
        let entry = track.entries[track.next]; track.next += 1;
        let next = track.entries.get(track.next);
        let end = next.map_or(self.size, |entry| entry.position);
        let id = track.id;
        let data = self.payload(entry.position, end, id)?;
        // Index spacing is not a display duration. VobSub queue entries start
        // with duration zero; the DVD stop command supplies the actual end.
        Ok(Packet::new(index as u32, TB, data).with_pts(entry.pts).with_dts(entry.pts).with_keyframe(true))
    }
    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        let index = stream_index as usize;
        let selected = self.tracks.get(index).ok_or_else(invalid)?;
        let target = selected.entries.iter().enumerate().filter(|(_, entry)| entry.pts <= pts).max_by_key(|(_, entry)| entry.pts).map(|(i, _)| i).unwrap_or(0);
        let actual = selected.entries[target].pts;
        for (i, track) in self.tracks.iter_mut().enumerate() {
            track.next = if i == index { target } else {
                track.entries.iter().enumerate().filter(|(_, entry)| entry.pts <= actual).max_by_key(|(_, entry)| entry.pts).map(|(i, _)| i).unwrap_or(0)
            };
        }
        Ok(actual)
    }
}
