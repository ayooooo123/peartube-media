// Ported from FFmpeg libavformat/mxfdec.c (commit 2da55bf): mxf_read_header with
// mxf_read_random_index_pack, mxf_parse_klv, mxf_seek_to_previous_partition,
// mxf_parse_handle_essence, mxf_parse_handle_partition_or_eof,
// mxf_compute_essence_containers, mxf_handle_missing_index_segment,
// mxf_compute_edit_units_per_packet; mxf_read_packet with mxf_get_stream_index,
// find_body_sid_by_absolute_offset, mxf_get_d10_aes3_packet, mxf_decrypt_triplet,
// mxf_set_current_edit_unit, mxf_get_next_track_edit_unit,
// mxf_compute_sample_count, mxf_set_audio_pts, mxf_set_pts; mxf_read_seek with
// libavformat/seek.c ff_index_search_timestamp and avpriv_update_cur_dts.
// License: LGPL-2.1-or-later

use std::collections::VecDeque;

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, CodecTag, Demuxer, Error, MediaType, Packet, ProbeContext, ReadSeek, Rational, Result,
    SampleFormat, StreamInfo, TimeBase,
};

use crate::index::{self, edit_unit_absolute_offset, inv, rescale, rescale_q, IndexTable};
use crate::klv::{self, Io, Klv};
use crate::layer::StreamLayer;
use crate::sets::{self, IndexSegment, Partition, Reader, Set, SetData, SetType, Store};
use crate::structure::{self, is_pcm, Stream};
use crate::types::*;

/// A packet larger than this is refused (untrusted input; FFmpeg reads
/// any size).
const MAX_PACKET_BYTES: u64 = 256 << 20;

pub struct MxfDemuxer {
    io: Io,
    run_in: u64,
    op: Op,
    partitions: Vec<Partition>,
    streams: Vec<Stream>,
    infos: Vec<StreamInfo>,
    layers: Vec<StreamLayer>,
    index_tables: Vec<IndexTable>,
    /// current_klv_data, None for FFmpeg's zeroed one.
    current_klv: Option<Klv>,
    /// Streams the caller reads (AVDISCARD_ALL for the others).
    active: Vec<bool>,
    file_size: u64,
    duration_micros: Option<i64>,
    /// Packets the demuxer layer returned and the caller has yet to take
    /// (FFmpeg's parse queue).
    queue: VecDeque<Packet>,
    /// Reading ended (with the error or end of input to report once the
    /// queue is drained); the parsers have been flushed.
    ended: Option<Error>,
}

/// The header walk's state (MXFContext fields mxf_read_header uses).
struct Header {
    store: Store,
    partitions: Vec<Partition>,
    current: Option<usize>,
    last_forward_partition: usize,
    parsing_backward: bool,
    last_forward_tell: u64,
    footer_partition: u64,
    op: Op,
}

impl Header {
    /// mxf_read_partition_pack's bookkeeping: insert the pack in order.
    fn add_partition(&mut self, io: &mut Io, klv: &Klv, run_in: u64) -> Result<()> {
        if self.partitions.len() >= i32::MAX as usize / 2 {
            return Err(Error::invalid("mxf: too many partitions"));
        }
        let prev_forward = if !self.parsing_backward && self.last_forward_partition >= 1 {
            // The partition before this one, parsed forward.
            self.partitions.get(self.last_forward_partition.wrapping_sub(1)).map(|p| p.pack_ofs)
        } else {
            None
        };
        let pack = sets::read_partition_pack(io, klv, run_in, self.parsing_backward, prev_forward)?;
        let at = if self.parsing_backward {
            self.last_forward_partition
        } else {
            self.last_forward_partition += 1;
            self.partitions.len()
        };
        self.partitions.insert(at.min(self.partitions.len()), pack.partition);
        self.current = Some(at.min(self.partitions.len() - 1));
        // Some files don't have FooterPartition set in every partition.
        if pack.footer_partition != 0 && (self.footer_partition == 0 || self.footer_partition == pack.footer_partition) {
            self.footer_partition = pack.footer_partition;
        }
        self.op = pack.op;
        Ok(())
    }

    /// mxf_seek_to_previous_partition: whether to keep parsing.
    fn seek_to_previous_partition(&mut self, io: &mut Io, run_in: u64) -> Result<bool> {
        let Some(cur) = self.current else { return Ok(false) };
        let previous = self.partitions[cur].previous_partition;
        if run_in.wrapping_add(previous) <= self.last_forward_tell {
            return Ok(false); // all partitions parsed
        }
        let current_partition_ofs = self.partitions[cur].pack_ofs;
        io.seek(run_in + previous)?;
        self.current = None;
        let klv = klv::read_packet(io, run_in)?;
        if !is_partition_pack_key(&klv.key) {
            return Err(Error::invalid("mxf: PreviousPartition isn't a PartitionPack"));
        }
        // PreviousPartition can point to just before the current
        // partition, which klv_read_packet syncs back up to (deadlock3.mxf).
        if klv.offset as i64 >= current_partition_ofs {
            return Err(Error::invalid("mxf: PreviousPartition indirectly points to itself"));
        }
        let next = io.tell() + klv.length;
        self.add_partition(io, &klv, run_in)?;
        if io.tell() > next {
            return Err(Error::invalid("mxf: read past the end of a KLV"));
        }
        io.seek(next)?;
        Ok(true)
    }

    /// mxf_parse_handle_essence.
    fn handle_essence(&mut self, io: &mut Io, run_in: u64) -> Result<bool> {
        if self.parsing_backward {
            return self.seek_to_previous_partition(io, run_in);
        }
        if self.footer_partition == 0 {
            return Ok(false);
        }
        // Remember where we were so as not to seek further back than this.
        self.last_forward_tell = io.tell();
        io.seek(run_in + self.footer_partition)?;
        self.current = None;
        self.parsing_backward = true;
        Ok(true)
    }

    /// mxf_parse_handle_partition_or_eof.
    fn handle_partition_or_eof(&mut self, io: &mut Io, run_in: u64) -> Result<bool> {
        if self.parsing_backward { self.seek_to_previous_partition(io, run_in) } else { Ok(true) }
    }
}

/// A step's outcome where FFmpeg stops parsing on `<= 0`: errors stop
/// it too, they do not fail the header.
fn keep_going(step: Result<bool>) -> bool {
    step.unwrap_or(false)
}

/// mxf_read_random_index_pack: the footer partition the RIP names.
fn read_random_index_pack(io: &mut Io, run_in: u64, file_size: u64) -> u64 {
    let mut footer = 0;
    // S377m says to check the RIP length for "silly" values: a file of
    // nothing but 105-byte partition packs, 12 bytes per RIP entry, 28 of
    // header and footer.
    let max_rip_length = (((file_size.saturating_sub(run_in)) / 105) * 12 + 28).min(i32::MAX as u64);
    let min_rip_length = 16 + 1 + 24 + 4;
    if file_size >= 4 && io.seek(file_size - 4).is_ok() {
        let length = u64::from(io.rb32());
        if (min_rip_length..=max_rip_length).contains(&length) && io.seek(file_size - length).is_ok() {
            if let Ok(rip) = klv::read_packet(io, run_in) {
                if rip.key == RANDOM_INDEX_PACK_KEY && rip.next_klv == file_size && rip.length > 4 && (rip.length - 4) % 12 == 0 {
                    let _ = io.skip(rip.length as i64 - 12);
                    footer = io.rb64();
                    // Sanity check.
                    if run_in.saturating_add(footer) >= file_size {
                        footer = 0;
                    }
                }
            }
        }
    }
    let _ = io.seek(run_in);
    footer
}

pub fn open(input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(MxfDemuxer::open(input, codecs)?))
}

impl MxfDemuxer {
    /// mxf_read_header. Codec ids are FFmpeg's, but for those the codec
    /// registry knows by a container tag under another id.
    pub fn open(input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<Self> {
        let mut io = Io::new(input);
        let file_size = io.size()?;
        if !klv::read_sync(&mut io, &HEADER_PARTITION_PACK_KEY) {
            return Err(Error::invalid("mxf: no header partition pack key"));
        }
        let run_in = io.tell() - 14;
        io.seek(run_in)?;
        if run_in > RUN_IN_MAX {
            return Err(Error::invalid("mxf: run-in too long"));
        }
        let mut h = Header {
            store: Store::default(),
            partitions: Vec::new(),
            current: None,
            last_forward_partition: 0,
            parsing_backward: false,
            last_forward_tell: i64::MAX as u64,
            footer_partition: 0,
            op: Op::Unset,
        };
        h.footer_partition = read_random_index_pack(&mut io, run_in, file_size);
        let mut essence_offset: u64 = 0;
        while !io.feof() {
            let klv = match klv::read_packet(&mut io, run_in) {
                Ok(klv) if klv.key != RANDOM_INDEX_PACK_KEY => klv,
                _ => {
                    // The end, or the RIP: seek to the previous partition or stop.
                    if !keep_going(h.handle_partition_or_eof(&mut io, run_in)) {
                        break;
                    }
                    continue;
                }
            };
            if match_uid(&klv.key, &ENCRYPTED_TRIPLET_KEY, 16)
                || is_essence_element_key(&klv.key)
                || is_klv_key(&klv.key, &SYSTEM_ITEM_KEY_CP)
                || is_klv_key(&klv.key, &SYSTEM_ITEM_KEY_GC)
            {
                let Some(cur) = h.current else {
                    return Err(Error::invalid("mxf: essence before the first PartitionPack"));
                };
                if h.partitions[cur].first_essence_klv.offset == 0 {
                    h.partitions[cur].first_essence_klv = klv;
                }
                if essence_offset == 0 {
                    essence_offset = klv.offset;
                }
                // Seek to the footer, the previous partition, or stop.
                if !keep_going(h.handle_essence(&mut io, run_in)) {
                    break;
                }
                continue;
            } else if is_partition_pack_key(&klv.key) && h.current.is_some() {
                // The next partition pack: keep going, or seek to the
                // previous partition, or stop.
                if !keep_going(h.handle_partition_or_eof(&mut io, run_in)) {
                    break;
                } else if h.parsing_backward {
                    continue;
                }
                // Still parsing forward: this partition pack is read below.
            }
            match sets::reader_for(&klv.key) {
                Some(Reader::Skip) | None => {
                    io.skip(klv.length as i64)?;
                }
                Some(reader) => parse_klv(&mut h, &mut io, &klv, reader, run_in)?,
            }
        }
        if essence_offset == 0 {
            return Err(Error::invalid("mxf: no essence"));
        }
        io.seek(essence_offset)?;

        // Before the index tables, so zero IndexDurations take the
        // stream's duration.
        let mut streams = structure::parse(&h.store, h.op)?;
        for st in &mut streams {
            handle_missing_index_segment(&mut h.store, &h.partitions, st);
        }
        let segments: Vec<IndexSegment> = h
            .store
            .group(SetType::IndexTableSegment)
            .iter()
            .filter_map(|s| match &s.data {
                SetData::IndexSegment(seg) => Some(seg.clone()),
                _ => None,
            })
            .collect();
        let index_tables = index::compute_index_tables(&segments, |index_sid| {
            streams.iter().filter_map(|s| s.track.as_ref()).find(|t| t.index_sid == index_sid).map(|t| (t.edit_rate, t.original_duration))
        })?;
        let mut partitions = h.partitions;
        compute_essence_containers(&mut partitions, h.op, &streams, run_in);
        for st in &mut streams {
            compute_edit_units_per_packet(&index_tables, st);
        }

        let infos: Vec<StreamInfo> = streams.iter().enumerate().map(|(i, s)| stream_info(i, s, codecs)).collect();
        let layers = streams
            .iter()
            .map(|s| StreamLayer::new(s.media, s.codec, s.need_parsing, s.time_base, s.r_frame_rate, s.sample_rate, s.channels, s.extradata.as_deref()))
            .collect();
        let duration_micros = infos
            .iter()
            .filter_map(|s| {
                let tb = s.time_base.as_rational();
                s.duration.filter(|&d| d > 0 && tb.num > 0 && tb.den > 0).map(|d| rescale(d, tb.num * 1_000_000, tb.den))
            })
            .max();
        let active = vec![true; streams.len()];
        Ok(Self {
            io,
            run_in,
            op: h.op,
            partitions,
            streams,
            infos,
            layers,
            index_tables,
            current_klv: None,
            active,
            file_size,
            duration_micros,
            queue: VecDeque::new(),
            ended: None,
        })
    }

    fn find_index_table(&self, index_sid: i32) -> Option<usize> {
        self.index_tables.iter().position(|t| t.index_sid == index_sid)
    }

    /// mxf_get_stream_index (SMPTE 379M 7.3).
    fn stream_index(&self, key: &Uid, body_sid: i32) -> Option<usize> {
        for (i, s) in self.streams.iter().enumerate() {
            let Some(t) = &s.track else { continue };
            if (body_sid == 0 || t.body_sid == 0 || t.body_sid == body_sid) && key[12..16] == t.track_number {
                return Some(i);
            }
        }
        // One stream: OPAtom files with 0 as the track number.
        (self.streams.len() == 1 && self.streams[0].track.is_some()).then_some(0)
    }

    /// find_body_sid_by_absolute_offset.
    fn body_sid_at(&self, offset: u64) -> i32 {
        let (mut a, mut b): (isize, isize) = (-1, self.partitions.len() as isize);
        while b - a > 1 {
            let m = (a + b) >> 1;
            if self.partitions[m as usize].pack_ofs <= offset as i64 {
                a = m;
            } else {
                b = m;
            }
        }
        if a == -1 { 0 } else { self.partitions[a as usize].body_sid }
    }

    /// mxf_compute_sample_count.
    fn sample_count_of(&self, st: usize, edit_unit: i64) -> i64 {
        let s = &self.streams[st];
        let Some(t) = &s.track else { return edit_unit };
        if s.media != MediaType::Audio {
            return edit_unit;
        }
        rescale_q(edit_unit, inv(s.time_base), t.edit_rate)
    }

    /// mxf_get_next_track_edit_unit: the first edit unit of stream `st`
    /// at or after `current_offset` (original_duration past the last).
    fn next_track_edit_unit(&self, st: usize, current_offset: i64) -> Option<i64> {
        let t = self.streams[st].track.as_ref()?;
        let table = &self.index_tables[self.find_index_table(t.index_sid)?];
        if t.original_duration <= 0 {
            return None;
        }
        let (mut a, mut b): (i64, i64) = (-1, t.original_duration);
        while b - 1 > a {
            let m = ((a as u64).wrapping_add(b as u64) >> 1) as i64;
            let (_, offset, _) = edit_unit_absolute_offset(&self.partitions, table, m, t.edit_rate).ok()?;
            if offset < current_offset {
                a = m;
            } else {
                b = m;
            }
        }
        Some(b)
    }

    /// mxf_set_current_edit_unit: the offset of stream `st`'s next edit
    /// unit (or packet) after `current_offset`, resyncing its sample count
    /// when it lost track; None where FFmpeg returns < 0.
    fn set_current_edit_unit(&mut self, st: usize, current_offset: i64, resync: bool) -> Option<i64> {
        let s = &self.streams[st];
        let t = s.track.as_ref()?;
        let edit_unit = rescale_q(t.sample_count, s.time_base, inv(t.edit_rate));
        let ti = self.find_index_table(t.index_sid)?;
        if t.wrapping == Wrapping::Unknown || edit_unit > i64::MAX - t.edit_units_per_packet {
            return None;
        }
        let next_ofs = match edit_unit_absolute_offset(&self.partitions, &self.index_tables[ti], edit_unit + t.edit_units_per_packet, t.edit_rate) {
            Ok((_, ofs, _)) => ofs,
            Err(_) => {
                let end = index::essence_container_end(&self.partitions, self.index_tables[ti].body_sid);
                if end <= 0 {
                    return None; // unable to compute the size of the last packet
                }
                end
            }
        };
        // Whether the next edit unit starts ahead of current_offset.
        if next_ofs > current_offset {
            return Some(next_ofs);
        }
        if !resync {
            return None; // cannot find the current edit unit: invalid index?
        }
        let new_edit_unit = self.next_track_edit_unit(st, current_offset + 1).filter(|&e| e > 0)?;
        let sample_count = self.sample_count_of(st, new_edit_unit - 1);
        if let Some(t) = self.streams[st].track.as_mut() {
            t.sample_count = sample_count;
        }
        self.set_current_edit_unit(st, current_offset, false)
    }

    /// mxf_set_pts (with mxf_set_audio_pts): the demuxer's pts, dts and
    /// duration for a packet of `size` bytes of stream `st`.
    fn set_pts(&mut self, st: usize, size: usize) -> (Option<i64>, Option<i64>, Option<i64>) {
        let media = self.streams[st].media;
        let (channels, bits_per_sample, tb) = (self.streams[st].channels, self.streams[st].bits_per_coded_sample, self.streams[st].time_base);
        let table = self.streams[st].track.as_ref().and_then(|t| self.find_index_table(t.index_sid));
        let next_audio_count = {
            let t = self.streams[st].track.as_ref();
            // FFmpeg's int64_t sample counts advance with plain (wrapping)
            // additions; an index can land them anywhere.
            t.map(|t| {
                if channels <= 0 || bits_per_sample <= 0 || i64::from(channels) * i64::from(bits_per_sample) < 8 {
                    let eu = rescale_q(t.sample_count, tb, inv(t.edit_rate));
                    self.sample_count_of(st, eu.wrapping_add(1))
                } else {
                    t.sample_count.wrapping_add(size as i64 / (i64::from(channels) * i64::from(bits_per_sample) / 8))
                }
            })
        };
        let Some(t) = self.streams[st].track.as_mut() else { return (None, None, None) };
        match media {
            MediaType::Video => {
                let mut out = (None, None, None);
                match table.map(|i| &self.index_tables[i]) {
                    Some(table) if (t.sample_count as u64) < table.nb_ptses as u64 => {
                        out.1 = Some(t.sample_count + table.first_dts);
                        out.0 = table.ptses[t.sample_count as usize];
                    }
                    // Intra-only: PTS = EditUnit; FFmpeg's demuxer layer
                    // works out the DTS.
                    _ if t.intra_only => out.0 = Some(t.sample_count),
                    _ => {}
                }
                t.sample_count = t.sample_count.wrapping_add(1);
                out
            }
            MediaType::Audio => {
                let pts = Some(t.sample_count);
                if let Some(n) = next_audio_count {
                    t.sample_count = n;
                }
                (pts, None, None)
            }
            _ => {
                let ts = Some(t.sample_count);
                t.sample_count = t.sample_count.wrapping_add(1);
                (ts, ts, Some(1))
            }
        }
    }

    /// mxf_read_packet: the next packet as the MXF demuxer returns it,
    /// before the demuxer layer.
    fn read_packet(&mut self) -> Result<(usize, Vec<u8>, u64)> {
        loop {
            let mut pos = self.io.tell();
            let mut klv;
            let max_data_size;
            match self.current_klv {
                Some(cur) if pos >= cur.next_klv - cur.length && pos < cur.next_klv => {
                    klv = cur;
                    max_data_size = klv.next_klv - pos;
                }
                _ => {
                    self.current_klv = None;
                    klv = match klv::read_packet(&mut self.io, self.run_in) {
                        Ok(k) => k,
                        Err(e) => return Err(if self.io.feof() { self.io.take_error().unwrap_or(Error::Eof) } else { e }),
                    };
                    max_data_size = klv.length;
                    pos = klv.next_klv - klv.length;
                    if match_uid(&klv.key, &ENCRYPTED_TRIPLET_KEY, 16) {
                        return self.decrypt_triplet(&klv);
                    }
                }
            }
            if !is_essence_element_key(&klv.key) {
                self.io.skip(max_data_size as i64)?;
                self.current_klv = None;
                continue;
            }
            let body_sid = self.body_sid_at(klv.offset);
            let Some(st) = self.stream_index(&klv.key, body_sid).filter(|&i| self.active[i]) else {
                self.io.skip(max_data_size as i64)?;
                self.current_klv = None;
                continue;
            };
            let next_ofs = self.set_current_edit_unit(st, pos as i64, true);
            let wrapping = self.streams[st].track.as_ref().map_or(Wrapping::Unknown, |t| t.wrapping);
            if wrapping != Wrapping::Frame {
                let size = match next_ofs {
                    // No way to packetize the data: chunks of it.
                    None => max_data_size.min(MXF_MAX_CHUNK_SIZE),
                    Some(next_ofs) => {
                        let size = next_ofs - pos as i64;
                        if size <= 0 {
                            self.current_klv = None;
                            return Err(Error::invalid("mxf: bad packet size"));
                        }
                        // Not past the KLV: the next edit unit might be in another.
                        (size as u64).min(max_data_size)
                    }
                };
                self.current_klv = Some(klv);
                klv.offset = pos;
                klv.length = size;
                klv.next_klv = klv.offset + klv.length;
            }
            // 8 channels AES3 element.
            let data = if klv.key[12] == 0x06 && klv.key[13] == 0x01 && klv.key[14] == 0x10 {
                match self.d10_aes3_packet(st, klv.length) {
                    Ok(data) => data,
                    Err(e) => {
                        self.current_klv = None;
                        return Err(e);
                    }
                }
            } else {
                if klv.length > MAX_PACKET_BYTES {
                    self.current_klv = None;
                    return Err(Error::invalid("mxf: essence element too large"));
                }
                match self.io.get_packet(klv.length) {
                    Ok(data) => data,
                    Err(e) => {
                        self.current_klv = None;
                        return Err(e);
                    }
                }
            };
            // Seek for truncated packets.
            self.io.seek(klv.next_klv)?;
            return Ok((st, data, klv.offset));
        }
    }

    /// mxf_get_d10_aes3_packet: SMPTE 331M, 8 channels of 32-bit words,
    /// to little-endian samples of the stream's channels.
    fn d10_aes3_packet(&mut self, st: usize, length: u64) -> Result<Vec<u8>> {
        // Worst case PAL: 1920 samples of 8 channels.
        if length > 61444 {
            return Err(Error::invalid("mxf: D-10 AES3 element too large"));
        }
        let raw = self.io.get_packet(length)?;
        let channels = self.streams[st].channels;
        if channels > 8 {
            return Err(Error::invalid("mxf: D-10 AES3 with more than 8 channels"));
        }
        let channels = channels.max(0) as usize;
        let bits = self.streams[st].bits_per_coded_sample;
        let mut out = Vec::with_capacity(raw.len());
        let mut p = 4; // skip the SMPTE 331M header
        while raw.len().saturating_sub(p) >= channels * 4 && channels > 0 {
            for _ in 0..channels {
                let sample = u32::from_le_bytes(raw[p..p + 4].try_into().unwrap());
                p += 4;
                if bits == 24 {
                    out.extend_from_slice(&((sample >> 4) & 0xFF_FFFF).to_le_bytes()[..3]);
                } else {
                    out.extend_from_slice(&(((sample >> 12) & 0xFFFF) as u16).to_le_bytes());
                }
            }
            // Always 8 channels stored (SMPTE 331M).
            p += 32 - channels * 4;
        }
        Ok(out)
    }

    /// mxf_decrypt_triplet without a key: the triplet's encrypted value,
    /// its plaintext part in the clear, cut to the source size.
    fn decrypt_triplet(&mut self, klv: &Klv) -> Result<(usize, Vec<u8>, u64)> {
        let end = self.io.tell() + klv.length;
        // Cryptographic context.
        let (size, _) = klv::decode_ber_length(&mut self.io)?;
        self.io.skip(size as i64)?;
        // Plaintext offset.
        klv::decode_ber_length(&mut self.io)?;
        let plaintext_size = self.io.rb64();
        // Source KLV key.
        klv::decode_ber_length(&mut self.io)?;
        let key = self.io.uid();
        if !is_klv_key(&key, &ESSENCE_ELEMENT_KEY) {
            return Err(Error::invalid("mxf: encrypted triplet of no essence element"));
        }
        let body_sid = self.body_sid_at(klv.offset);
        let st = self.stream_index(&key, body_sid).ok_or_else(|| Error::invalid("mxf: encrypted triplet of no stream"))?;
        // Source size.
        klv::decode_ber_length(&mut self.io)?;
        let orig_size = self.io.rb64();
        if orig_size < plaintext_size {
            return Err(Error::invalid("mxf: encrypted triplet smaller than its plaintext"));
        }
        // Encrypted code: IV, check value, the data.
        let (size, _) = klv::decode_ber_length(&mut self.io)?;
        if size < 32 || size - 32 < orig_size || orig_size > i32::MAX as u64 {
            return Err(Error::invalid("mxf: invalid encrypted triplet"));
        }
        self.io.skip(32)?;
        let mut data = self.io.get_packet(size - 32)?;
        if (data.len() as u64) < plaintext_size {
            return Err(Error::invalid("mxf: encrypted triplet cut short"));
        }
        data.truncate(orig_size as usize);
        let at = self.io.tell();
        self.io.skip(end as i64 - at as i64)?;
        Ok((st, data, klv.offset))
    }

    /// mxf_read_seek, `sample_time` in stream `stream`'s time base: the
    /// landing in that time base.
    fn read_seek(&mut self, stream: usize, mut sample_time: i64) -> Result<i64> {
        let Some(source_track) = self.streams[stream].track.clone() else {
            return Ok(sample_time);
        };
        let mut st = stream;
        let mut source_track = source_track;
        // Audio: truncate sample_time to the edit rate.
        if self.streams[st].media == MediaType::Audio {
            sample_time = rescale_q(sample_time, self.streams[st].time_base, inv(source_track.edit_rate));
        }
        let (seekpos, klv, landed) = if self.index_tables.is_empty() {
            // Without an index: the byte the average bit rate puts the time at.
            let duration = self.duration_micros.filter(|&d| d > 0).ok_or_else(|| Error::invalid("mxf: no bit rate to seek by"))?;
            let bit_rate = rescale(self.file_size as i64, 8 * 1_000_000, duration);
            if bit_rate <= 0 {
                return Err(Error::invalid("mxf: no bit rate to seek by"));
            }
            sample_time = sample_time.max(0);
            let tb = self.streams[st].time_base;
            let seconds = rescale(sample_time, i64::from(tb.0), i64::from(tb.1));
            (((bit_rate as i128 * i128::from(seconds)) >> 3).clamp(0, i128::from(i64::MAX)) as i64, None, sample_time)
        } else {
            let t = &self.index_tables[0];
            if t.index_sid != source_track.index_sid {
                // The first index table does not belong to the stream: a
                // stream that does belongs to it.
                let Some(i) = self.streams.iter().position(|s| s.track.as_ref().is_some_and(|tr| tr.index_sid == t.index_sid)) else {
                    return Err(Error::invalid("mxf: no stream for the index table"));
                };
                let new_track = self.streams[i].track.clone().unwrap_or_default();
                sample_time = rescale_q(sample_time, new_track.edit_rate, source_track.edit_rate);
                source_track = new_track;
                st = i;
            }
            // Clamp above zero: seeking before the start is allowed.
            sample_time = sample_time.max(0);
            if t.nb_ptses > 0 {
                // The first frames may not be key frames in presentation
                // order: advance the target to find the first one backwards.
                if let Some(first) = t.ptses[0] {
                    if sample_time < first && t.fake_index_key.get(first as usize).copied().unwrap_or(false) {
                        sample_time = first;
                    }
                }
                sample_time = index_search_timestamp(&t.fake_index_key, sample_time, true);
                if sample_time < 0 {
                    return Err(Error::invalid("mxf: no key frame to seek to"));
                }
                // Display order to stored order.
                sample_time += i64::from(t.offsets[sample_time as usize]);
            } else {
                // No IndexEntryArray: don't seek past the end.
                sample_time = sample_time.min(source_track.original_duration - 1);
            }
            let (landed, seekpos, partition) = edit_unit_absolute_offset(&self.partitions, t, sample_time, source_track.edit_rate)?;
            let klv = if source_track.wrapping == Wrapping::Clip {
                let klv = self.partitions[partition].first_essence_klv;
                if seekpos < (klv.next_klv - klv.length) as i64 || seekpos >= klv.next_klv as i64 {
                    return Err(Error::invalid("mxf: seek out of the clip-wrapped KLV"));
                }
                Some(klv)
            } else {
                None
            };
            (seekpos, klv, landed)
        };
        // The reposition first: reading is left as it was if it fails.
        self.io.seek(seekpos.max(0) as u64)?;
        self.current_klv = klv;
        self.queue.clear();
        self.ended = None;
        // avpriv_update_cur_dts, after ff_read_frame_flush.
        let ref_tb = self.streams[st].time_base;
        for (s, layer) in self.streams.iter().zip(self.layers.iter_mut()) {
            let cur = rescale(landed, i64::from(s.time_base.1) * i64::from(ref_tb.0), i64::from(s.time_base.0) * i64::from(ref_tb.1));
            layer.flush(cur, s.extradata.as_deref());
        }
        // Update all tracks' sample counts.
        for i in 0..self.streams.len() {
            if self.streams[i].track.is_none() {
                continue;
            }
            let mut edit_unit = landed;
            if i != st {
                if let Some(e) = self.next_track_edit_unit(i, seekpos) {
                    edit_unit = e;
                }
            }
            let count = self.sample_count_of(i, edit_unit);
            if let Some(t) = self.streams[i].track.as_mut() {
                t.sample_count = count;
            }
        }
        let tb = self.streams[st].time_base;
        let req_tb = self.streams[stream].time_base;
        Ok(rescale(landed, i64::from(tb.0) * i64::from(req_tb.1), i64::from(tb.1) * i64::from(req_tb.0)))
    }

    /// The packets the demuxer layer gave for stream `st`, queued.
    fn queue_outs(&mut self, st: usize, outs: Vec<crate::layer::Out>) {
        let tb = self.infos[st].time_base;
        for out in outs {
            let mut pkt = Packet::new(st as u32, tb, out.data);
            pkt.pts = out.pts;
            pkt.dts = out.dts;
            pkt.duration = out.duration;
            pkt.flags.keyframe = out.key;
            self.queue.push_back(pkt);
        }
    }

    /// The operational pattern of the file.
    pub fn operational_pattern(&self) -> Op {
        self.op
    }
}

/// ff_index_search_timestamp over the fake index (timestamps 0..n).
fn index_search_timestamp(keys: &[bool], wanted: i64, backward: bool) -> i64 {
    let n = keys.len() as i64;
    let (mut a, mut b) = (-1i64, n);
    if b > 0 && b - 1 < wanted {
        a = b - 1;
    }
    while b - a > 1 {
        let m = (a + b) >> 1;
        if m >= wanted {
            b = m;
        }
        if m <= wanted {
            a = m;
        }
    }
    let mut m = if backward { a } else { b };
    while m >= 0 && m < n && !keys[m as usize] {
        m += if backward { -1 } else { 1 };
    }
    if m == n { -1 } else { m }
}

/// mxf_parse_klv for a set in the read table, `io` after the KLV's length.
fn parse_klv(h: &mut Header, io: &mut Io, klv: &Klv, reader: Reader, run_in: u64) -> Result<()> {
    if klv.key[5] == 0x53 {
        let current = h.current.map(|i| h.partitions[i]);
        return sets::read_local_tags(&mut h.store, io, klv, reader, current.as_ref());
    }
    let next = io.tell() + klv.length;
    match reader {
        Reader::PrimerPack => h.store.read_primer_pack(io)?,
        Reader::PartitionPack => h.add_partition(io, klv, run_in)?,
        _ => {}
    }
    // Only seek forward, else this can loop for a long time.
    if io.tell() > next {
        return Err(Error::invalid("mxf: read past the end of a KLV"));
    }
    io.seek(next)?;
    Ok(())
}

/// mxf_handle_missing_index_segment: a clip-wrapped track in a single
/// essence partition without index segments gets one of its sample size.
fn handle_missing_index_segment(store: &mut Store, partitions: &[Partition], st: &mut Stream) {
    let Some(track) = st.track.as_mut() else { return };
    if track.wrapping != Wrapping::Clip {
        return;
    }
    let has_segment = store
        .group(SetType::IndexTableSegment)
        .iter()
        .any(|s| matches!(&s.data, SetData::IndexSegment(seg) if seg.body_sid == track.body_sid));
    if has_segment {
        return;
    }
    let essence: Vec<&Partition> = partitions.iter().filter(|p| p.body_sid == track.body_sid).collect();
    // Only files with a single essence partition.
    let [p] = essence[..] else { return };
    let duration = st.duration.unwrap_or(-1);
    let edit_unit_byte_count = if st.media == MediaType::Audio && is_pcm(st.codec) {
        (structure::bits_per_sample(st.codec) * st.channels) >> 3
    } else if duration > 0 && p.first_essence_klv.length > 0 && p.first_essence_klv.length % duration as u64 == 0 {
        (p.first_essence_klv.length / duration as u64) as i32
    } else {
        0
    };
    if edit_unit_byte_count <= 0 {
        return;
    }
    // A nonzero unique IndexSID: an index SID equal to a body SID is
    // forbidden in MXF, so the BodySID will do.
    if track.index_sid == 0 {
        track.index_sid = track.body_sid;
    }
    let segment = IndexSegment {
        edit_unit_byte_count: edit_unit_byte_count as u32,
        index_sid: track.index_sid,
        body_sid: p.body_sid,
        index_edit_rate: inv(st.time_base),
        index_start_position: 0,
        index_duration: duration as u64,
        ..Default::default()
    };
    let _ = store.add(SetType::IndexTableSegment, Set { uid: [0; 16], partition_score: 0, data: SetData::IndexSegment(segment) });
}

/// mxf_compute_essence_containers: each partition's essence offset and
/// length (clip-wrapped: the value of its first essence KLV).
fn compute_essence_containers(partitions: &mut [Partition], op: Op, streams: &[Stream], run_in: u64) {
    for x in 0..partitions.len() {
        let body_sid = partitions[x].body_sid;
        if body_sid == 0 {
            continue; // BodySID 0: no essence
        }
        let wrapping = if op == Op::OpAtom {
            Wrapping::Clip
        } else {
            // mxf_get_wrapping_by_body_sid.
            streams
                .iter()
                .filter_map(|s| s.track.as_ref())
                .find(|t| t.body_sid == body_sid && t.wrapping != Wrapping::Unknown)
                .map_or(Wrapping::Unknown, |t| t.wrapping)
        };
        let first = partitions[x].first_essence_klv;
        if wrapping == Wrapping::Clip {
            partitions[x].essence_offset = (first.next_klv - first.length) as i64;
            partitions[x].essence_length = first.length as i64;
        } else {
            partitions[x].essence_offset = first.offset as i64;
            // The essence container spans to the next partition.
            if x + 1 < partitions.len() {
                partitions[x].essence_length = partitions[x + 1].pack_ofs - run_in as i64 - partitions[x].essence_offset;
            }
            if partitions[x].essence_length < 0 {
                // Next ThisPartition < essence_offset.
                partitions[x].essence_length = 0;
            }
        }
    }
}

/// mxf_compute_edit_units_per_packet: clip-wrapped PCM with one index
/// segment of fewer than 32 bytes per edit unit reads 1/25 s at a time.
fn compute_edit_units_per_packet(tables: &[IndexTable], st: &mut Stream) {
    let (media, codec) = (st.media, st.codec);
    let Some(track) = st.track.as_mut() else { return };
    track.edit_units_per_packet = 1;
    if track.wrapping != Wrapping::Clip {
        return;
    }
    let Some(t) = tables.iter().find(|t| t.index_sid == track.index_sid) else { return };
    if media != MediaType::Audio || !is_pcm(codec) || t.segments.len() != 1 || t.segments[0].edit_unit_byte_count >= 32 {
        return;
    }
    // Arbitrarily 48 kHz PAL audio frame size.
    track.edit_units_per_packet = i64::from((track.edit_rate.0 / track.edit_rate.1.max(1) / 25).max(1));
}

/// The stream as the engine sees it.
fn stream_info(index: usize, s: &Stream, codecs: &dyn CodecResolver) -> StreamInfo {
    // MPEG-4 Part 2 is "mpeg4" to FFmpeg; the registry may know it by the
    // MP4 object type of MPEG-4 Visual under another id.
    let mpeg4 = (s.codec == "mpeg4").then(|| codecs.resolve_tag(&ProbeContext::new(&CodecTag::mp4_object_type(0x20)))).flatten();
    let id = mpeg4.unwrap_or_else(|| CodecId::new(if s.codec.is_empty() { "none" } else { s.codec }));
    let mut params = match s.media {
        MediaType::Video => {
            let mut p = CodecParameters::video(id);
            p.width = u32::try_from(s.width).ok().filter(|&w| w > 0);
            p.height = u32::try_from(s.height).ok().filter(|&h| h > 0);
            let rate = s.r_frame_rate.unwrap_or((s.time_base.1, s.time_base.0));
            p.frame_rate = (rate.0 > 0 && rate.1 > 0).then(|| Rational::new(i64::from(rate.0), i64::from(rate.1)));
            p
        }
        MediaType::Audio => {
            let mut p = CodecParameters::audio(id);
            p.sample_rate = u32::try_from(s.sample_rate).ok().filter(|&r| r > 0);
            p.channels = u16::try_from(s.channels).ok().filter(|&c| c > 0);
            p.sample_format = match s.codec {
                "pcm_u8" => Some(SampleFormat::U8),
                "pcm_s8" => Some(SampleFormat::S8),
                "pcm_s16le" | "pcm_s16be" => Some(SampleFormat::S16),
                "pcm_s24le" | "pcm_s24be" => Some(SampleFormat::S24),
                "pcm_s32le" | "pcm_s32be" => Some(SampleFormat::S32),
                _ => None,
            };
            p
        }
        MediaType::Subtitle => CodecParameters::subtitle(id),
        _ => CodecParameters::data(id),
    };
    if let Some(e) = &s.extradata {
        params.extradata = e.clone();
    }
    if let Some(tag) = &s.codec_tag {
        params.tag = Some(CodecTag::fourcc(tag));
    }
    StreamInfo {
        index: index as u32,
        params,
        time_base: TimeBase::new(i64::from(s.time_base.0), i64::from(s.time_base.1)),
        duration: s.duration,
        start_time: s.start_time,
    }
}

impl Demuxer for MxfDemuxer {
    fn format_name(&self) -> &str {
        "mxf"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.infos
    }

    fn set_active_streams(&mut self, indices: &[u32]) {
        for (i, a) in self.active.iter_mut().enumerate() {
            *a = indices.contains(&(i as u32));
        }
    }

    fn duration_micros(&self) -> Option<i64> {
        self.duration_micros
    }

    /// av_read_frame: the next packet of the MXF demuxer through the
    /// demuxer layer; at the end of the input, or a read error, the
    /// frames the parsers still hold come first.
    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            if let Some(pkt) = self.queue.pop_front() {
                return Ok(pkt);
            }
            if let Some(e) = self.ended.take() {
                // The end stays the end; another error is reported once,
                // reading going on after it as av_read_frame's does.
                if matches!(e, Error::Eof) {
                    self.ended = Some(Error::Eof);
                }
                return Err(e);
            }
            match self.read_packet() {
                Ok((st, data, pos)) => {
                    let (pts, dts, duration) = self.set_pts(st, data.len());
                    let outs = self.layers[st].packets(data, pts, dts, duration, pos as i64);
                    self.queue_outs(st, outs);
                }
                Err(e) => {
                    // Flush the parsers: what they hold is the last frames.
                    for st in 0..self.layers.len() {
                        let outs = self.layers[st].finish();
                        self.queue_outs(st, outs);
                    }
                    self.ended = Some(e);
                }
            }
        }
    }

    /// mxf_read_seek with AVSEEK_FLAG_BACKWARD: by the index tables (the
    /// first one's, its fake index for key frames where it has temporal
    /// offsets), else by the bit rate. A seek reads nothing; one that
    /// fails leaves reading where it was.
    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        let stream = stream_index as usize;
        if stream >= self.streams.len() {
            return Err(Error::invalid("mxf: no such stream to seek"));
        }
        self.read_seek(stream, pts)
    }
}
