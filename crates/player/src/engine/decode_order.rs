//! Repair decode-order timestamps for H.264 and HEVC video streams.
//!
//! Containers such as MP4 occasionally lack a `ctts` (composition time to
//! sample) atom while carrying B-frame video. In such files every packet's
//! presentation timestamp equals its decode timestamp (`pts == dts`),
//! delivering decode-order timestamps to down-stream decoders. Because modern
//! video decoders emit pictures in display order with each packet's attached
//! timestamp, timestamps go backward and display sinks drop or freeze frames.
//!
//! [`wrap`] observes pictures in decode order, tracks POC (picture order count)
//! to re-establish presentation order across a bounded reordering window `N`,
//! detects whether the container stamps are in decode order, and rewrites each
//! picture packet's `pts` to its presentation time before sinks or decoders see
//! it. Streams that already carry presentation times or have no reordering pass
//! through untouched.

use std::collections::{HashMap, VecDeque};

use oxideav_core::{
    AttachedPicture, Attachment, Chapter, Demuxer, Error, MediaType, Packet, PacketMetadata,
    Result, StreamInfo,
};

use crate::engine::entry;

/// Wraps a demuxer so H.264/HEVC video packets stamped in decode order leave it
/// stamped with their presentation times. Every other packet passes through as read.
pub(super) fn wrap(inner: Box<dyn Demuxer>) -> Box<dyn Demuxer> {
    let mut streams = HashMap::new();
    for s in inner.streams() {
        if s.params.media_type == MediaType::Video {
            let codec_kind = match s.params.codec_id.as_str() {
                "h264" => Some(CodecKind::H264),
                "hevc" | "h265" => Some(CodecKind::Hevc),
                _ => None,
            };
            if let Some(codec) = codec_kind {
                let framing = match codec {
                    CodecKind::H264 => entry::h264_framing(&s.params.extradata),
                    CodecKind::Hevc => entry::hevc_framing(&s.params.extradata),
                };

                let mut hevc_sps = HashMap::new();
                let mut hevc_pps = HashMap::new();
                let mut h264_sps = HashMap::new();
                let mut h264_pps = HashMap::new();

                match codec {
                    CodecKind::Hevc => {
                        load_hevc_extradata(&s.params.extradata, &mut hevc_sps, &mut hevc_pps);
                    }
                    CodecKind::H264 => {
                        load_h264_extradata(&s.params.extradata, &mut h264_sps, &mut h264_pps);
                    }
                }

                streams.insert(
                    s.index,
                    StreamState {
                        codec,
                        framing,
                        reorder: Reorder::new(),
                        next_packet_id: 0,
                        hevc_sps,
                        hevc_pps,
                        h264_sps,
                        h264_pps,
                        held_packets: VecDeque::new(),
                        first_pic_since_reset: true,
                        hevc_poc_state: oxideav_h265::poc::PocState::new(),
                        h264_poc_state: oxideav_h264::poc::PocState::default(),
                    },
                );
            }
        }
    }

    Box::new(DecodeOrderDemuxer {
        inner,
        streams,
        ready_queue: VecDeque::new(),
        last_metadata: PacketMetadata::default(),
        eof: false,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodecKind {
    H264,
    Hevc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Undecided,
    Repair,
    Trust,
}

#[derive(Debug, PartialEq, Eq)]
struct Release {
    packet_id: usize,
    repaired_pts: Option<i64>,
}

enum InputPacket {
    Picture {
        poc: i32,
        is_new_poc_period: bool,
        reorder_n: usize,
        container_stamp: i64,
        duration: Option<i64>,
        bytes: usize,
    },
    NonPicture {
        bytes: usize,
    },
}

struct WaitingPic {
    poc: i32,
    packet_id: usize,
}

struct ReorderHeldPic {
    id: usize,
    is_picture: bool,
    display_index: Option<usize>,
    bytes: usize,
}

/// Standalone reordering and slot state machine.
///
/// Tracks POC progression within POC periods to determine whether container
/// stamps follow decode order (Repair) or presentation order (Trust). Bumps
/// waiting pictures in display order and assigns presentation timestamps from
/// a sliding window of decode-order container stamps.
struct Reorder {
    mode: Mode,
    shift_d: usize,
    max_poc_in_period: Option<(i32, i64)>,
    waiting: Vec<WaitingPic>,
    held_pictures: VecDeque<ReorderHeldPic>,
    slots: VecDeque<i64>,
    slots_base: usize,
    last_slot_step: Option<i64>,
    last_packet_duration: Option<i64>,
    display_count: usize,
    held_bytes: usize,
}

impl Reorder {
    fn new() -> Self {
        Self {
            mode: Mode::Undecided,
            shift_d: 0,
            max_poc_in_period: None,
            waiting: Vec::new(),
            held_pictures: VecDeque::new(),
            slots: VecDeque::new(),
            slots_base: 0,
            last_slot_step: None,
            last_packet_duration: None,
            display_count: 0,
            held_bytes: 0,
        }
    }

    fn reset_for_seek(&mut self) {
        self.waiting.clear();
        self.held_pictures.clear();
        self.slots.clear();
        self.slots_base = 0;
        self.last_slot_step = None;
        self.last_packet_duration = None;
        self.display_count = 0;
        self.max_poc_in_period = None;
        self.held_bytes = 0;
        // mode and shift_d survive seek
    }

    fn become_trust(&mut self, releases: &mut Vec<Release>) {
        self.mode = Mode::Trust;
        self.held_bytes = 0;
        for held in self.held_pictures.drain(..) {
            releases.push(Release {
                packet_id: held.id,
                repaired_pts: None,
            });
        }
        self.waiting.clear();
        self.slots.clear();
    }

    fn push(&mut self, packet_id: usize, input: InputPacket, releases: &mut Vec<Release>) {
        if self.mode == Mode::Trust {
            releases.push(Release {
                packet_id,
                repaired_pts: None,
            });
            return;
        }

        let (is_picture, bytes) = match &input {
            InputPacket::Picture { bytes, .. } => (true, *bytes),
            InputPacket::NonPicture { bytes } => (false, *bytes),
        };
        self.held_bytes += bytes;
        self.held_pictures.push_back(ReorderHeldPic {
            id: packet_id,
            is_picture,
            display_index: None,
            bytes,
        });

        match input {
            InputPacket::Picture {
                poc,
                is_new_poc_period,
                reorder_n,
                container_stamp,
                duration,
                ..
            } => {
                // Step 4: Decision while Undecided.
                // Only compare against prior pictures in the same POC period.
                if self.mode == Mode::Undecided {
                    self.shift_d = reorder_n;
                    if !is_new_poc_period {
                        if let Some((max_poc, max_stamp)) = self.max_poc_in_period {
                            if poc < max_poc {
                                if container_stamp > max_stamp {
                                    self.mode = Mode::Repair;
                                    self.shift_d = reorder_n;
                                } else if container_stamp < max_stamp {
                                    self.become_trust(releases);
                                    return;
                                }
                            }
                        }
                    }
                } else if self.mode == Mode::Repair {
                    // Contradiction fallback: if a stream decided Repair later shows a
                    // picture whose container stamp decreases when its POC decreases,
                    // the stamps contradict decode order (a presentation-stamped stream
                    // falsely flipped to Repair by a corrupt stamp). Recover by switching
                    // to Trust: held packets leave with original stamps, later ones pass through.
                    if !is_new_poc_period {
                        if let Some((max_poc, max_stamp)) = self.max_poc_in_period {
                            if poc < max_poc && container_stamp < max_stamp {
                                self.become_trust(releases);
                                return;
                            }
                        }
                    }
                }

                if is_new_poc_period {
                    self.max_poc_in_period = Some((poc, container_stamp));
                } else {
                    match self.max_poc_in_period {
                        Some((max_poc, _)) if poc > max_poc => {
                            self.max_poc_in_period = Some((poc, container_stamp));
                        }
                        None => {
                            self.max_poc_in_period = Some((poc, container_stamp));
                        }
                        _ => {}
                    }
                }

                // Step 2: Display order (output bumping)
                if is_new_poc_period {
                    self.waiting.sort_by_key(|p| p.poc);
                    for p in self.waiting.drain(..) {
                        let k = self.display_count;
                        self.display_count += 1;
                        if let Some(held) = self.held_pictures.iter_mut().find(|h| h.id == p.packet_id) {
                            held.display_index = Some(k);
                        }
                    }
                }

                self.waiting.push(WaitingPic {
                    poc,
                    packet_id,
                });

                while self.waiting.len() > reorder_n {
                    let min_idx = self
                        .waiting
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, p)| p.poc)
                        .map(|(i, _)| i)
                        .unwrap();
                    let p = self.waiting.remove(min_idx);
                    let k = self.display_count;
                    self.display_count += 1;
                    if let Some(held) = self.held_pictures.iter_mut().find(|h| h.id == p.packet_id) {
                        held.display_index = Some(k);
                    }
                }

                // Step 3: Slots
                if let Some(prev) = self.slots.back() {
                    self.last_slot_step = Some(container_stamp.wrapping_sub(*prev));
                }
                self.slots.push_back(container_stamp);
                if let Some(dur) = duration {
                    self.last_packet_duration = Some(dur);
                }
            }
            InputPacket::NonPicture { .. } => {}
        }

        // Rule 6: cap at 64 held packets or VIDEO_MAX_BYTES per stream
        if self.held_pictures.len() > 64 || self.held_bytes > super::VIDEO_MAX_BYTES {
            self.become_trust(releases);
            return;
        }

        self.try_release(releases);
    }

    /// Releases packets in decode order as their display slots become known.
    ///
    /// Pictures released while Undecided keep their container stamps: a
    /// decode-order stream whose first reordered picture comes late (N = 1,
    /// or a P-only opening GOP) plays its first pictures up to N frames early
    /// and jumps N frames when Repair is decided. That is accepted: holding
    /// until a decision would delay every non-reordering stream's first picture
    /// by up to the 64-packet cap.
    fn try_release(&mut self, releases: &mut Vec<Release>) {
        while let Some(front) = self.held_pictures.front() {
            if !front.is_picture {
                let held = self.held_pictures.pop_front().unwrap();
                self.held_bytes = self.held_bytes.saturating_sub(held.bytes);
                releases.push(Release {
                    packet_id: held.id,
                    repaired_pts: None,
                });
            } else {
                let k = match front.display_index {
                    Some(k) => k,
                    None => break,
                };
                let d = self.shift_d;
                let target_slot = k + d;
                if target_slot < self.slots_base || target_slot >= self.slots_base + self.slots.len() {
                    break;
                }
                let slot_val = self.slots[target_slot - self.slots_base];
                let held = self.held_pictures.pop_front().unwrap();
                self.held_bytes = self.held_bytes.saturating_sub(held.bytes);
                let repaired = if self.mode == Mode::Repair {
                    Some(slot_val)
                } else {
                    None
                };
                releases.push(Release {
                    packet_id: held.id,
                    repaired_pts: repaired,
                });
            }
        }
        self.prune_slots();
    }

    fn prune_slots(&mut self) {
        let min_needed_k = self
            .held_pictures
            .iter()
            .filter(|h| h.is_picture)
            .map(|h| h.display_index.unwrap_or(self.display_count))
            .min()
            .unwrap_or(self.display_count);
        let min_needed_slot = min_needed_k;
        while self.slots_base < min_needed_slot && self.slots.len() > 2 {
            self.slots.pop_front();
            self.slots_base += 1;
        }
    }

    fn handle_eof(&mut self, releases: &mut Vec<Release>) {
        if self.mode == Mode::Trust {
            return;
        }

        self.waiting.sort_by_key(|p| p.poc);
        for p in self.waiting.drain(..) {
            let k = self.display_count;
            self.display_count += 1;
            if let Some(held) = self.held_pictures.iter_mut().find(|h| h.id == p.packet_id) {
                held.display_index = Some(k);
            }
        }

        let max_k_plus_d = self
            .held_pictures
            .iter()
            .filter(|h| h.is_picture)
            .filter_map(|h| h.display_index.map(|k| k + self.shift_d))
            .max();

        if let Some(max_needed) = max_k_plus_d {
            let current_max_slot = self.slots_base + self.slots.len();
            if max_needed >= current_max_slot {
                let step = self.last_slot_step.unwrap_or_else(|| {
                    self.last_packet_duration.unwrap_or(0)
                });
                while self.slots_base + self.slots.len() <= max_needed {
                    let next = self.slots.back().copied().unwrap_or(0).saturating_add(step);
                    self.slots.push_back(next);
                }
            }
        }

        while let Some(held) = self.held_pictures.pop_front() {
            self.held_bytes = self.held_bytes.saturating_sub(held.bytes);
            let repaired = if held.is_picture && self.mode == Mode::Repair {
                if let Some(k) = held.display_index {
                    let target_slot = k + self.shift_d;
                    if target_slot >= self.slots_base
                        && target_slot < self.slots_base + self.slots.len()
                    {
                        Some(self.slots[target_slot - self.slots_base])
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            releases.push(Release {
                packet_id: held.id,
                repaired_pts: repaired,
            });
        }
        self.slots.clear();
    }
}

struct StreamState {
    codec: CodecKind,
    framing: entry::Framing,
    reorder: Reorder,
    next_packet_id: usize,

    // Tables preserved across seeks:
    hevc_sps: HashMap<u8, oxideav_h265::sps::SeqParameterSet>,
    hevc_pps: HashMap<u8, oxideav_h265::pps::PicParameterSet>,
    h264_sps: HashMap<u32, oxideav_h264::sps::Sps>,
    h264_pps: HashMap<u32, oxideav_h264::pps::Pps>,

    // Per-seek packet queue (drain in FIFO release order):
    held_packets: VecDeque<(usize, Packet, PacketMetadata)>,

    first_pic_since_reset: bool,
    hevc_poc_state: oxideav_h265::poc::PocState,
    h264_poc_state: oxideav_h264::poc::PocState,
}

impl StreamState {
    fn reset_for_seek(&mut self) {
        self.reorder.reset_for_seek();
        self.held_packets.clear();
        self.first_pic_since_reset = true;
        self.hevc_poc_state = oxideav_h265::poc::PocState::new();
        self.h264_poc_state = oxideav_h264::poc::PocState::default();
    }

    fn process_packet(
        &mut self,
        packet: Packet,
        metadata: PacketMetadata,
        ready_queue: &mut VecDeque<(Packet, PacketMetadata)>,
    ) {
        if self.reorder.mode == Mode::Trust {
            ready_queue.push_back((packet, metadata));
            return;
        }

        let packet_id = self.next_packet_id;
        self.next_packet_id += 1;

        let parse_result = match self.codec {
            CodecKind::Hevc => parse_hevc_packet(
                &packet.data,
                self.framing,
                &mut self.hevc_sps,
                &mut self.hevc_pps,
                &mut self.hevc_poc_state,
                &mut self.first_pic_since_reset,
            ),
            CodecKind::H264 => parse_h264_packet(
                &packet.data,
                self.framing,
                &mut self.h264_sps,
                &mut self.h264_pps,
                &mut self.h264_poc_state,
            ),
        };

        let slice_info = match parse_result {
            Ok(info) => info,
            Err(()) => {
                let mut releases = Vec::new();
                self.reorder.become_trust(&mut releases);
                self.dispatch_releases(&releases, ready_queue);
                ready_queue.push_back((packet, metadata));
                return;
            }
        };

        let charge = super::packet_charge(&packet, &metadata);
        let input = match slice_info {
            Some(info) => {
                let container_stamp = match packet.pts.or(packet.dts) {
                    Some(s) => s,
                    None => {
                        let mut releases = Vec::new();
                        self.reorder.become_trust(&mut releases);
                        self.dispatch_releases(&releases, ready_queue);
                        ready_queue.push_back((packet, metadata));
                        return;
                    }
                };
                InputPacket::Picture {
                    poc: info.poc,
                    is_new_poc_period: info.is_new_poc_period,
                    reorder_n: info.reorder_n,
                    container_stamp,
                    duration: packet.duration,
                    bytes: charge,
                }
            }
            None => InputPacket::NonPicture { bytes: charge },
        };

        self.held_packets.push_back((packet_id, packet, metadata));

        let mut releases = Vec::new();
        self.reorder.push(packet_id, input, &mut releases);
        self.dispatch_releases(&releases, ready_queue);
    }

    fn dispatch_releases(
        &mut self,
        releases: &[Release],
        ready_queue: &mut VecDeque<(Packet, PacketMetadata)>,
    ) {
        for rel in releases {
            if let Some((id, mut pkt, meta)) = self.held_packets.pop_front() {
                debug_assert_eq!(id, rel.packet_id);
                if let Some(new_pts) = rel.repaired_pts {
                    pkt.pts = Some(new_pts);
                }
                ready_queue.push_back((pkt, meta));
            }
        }
    }

    fn handle_eof(&mut self, ready_queue: &mut VecDeque<(Packet, PacketMetadata)>) {
        let mut releases = Vec::new();
        self.reorder.handle_eof(&mut releases);
        self.dispatch_releases(&releases, ready_queue);
    }
}

struct DecodeOrderDemuxer {
    inner: Box<dyn Demuxer>,
    streams: HashMap<u32, StreamState>,
    ready_queue: VecDeque<(Packet, PacketMetadata)>,
    last_metadata: PacketMetadata,
    eof: bool,
}

impl Demuxer for DecodeOrderDemuxer {
    fn format_name(&self) -> &str {
        self.inner.format_name()
    }

    fn streams(&self) -> &[StreamInfo] {
        self.inner.streams()
    }

    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            if let Some((packet, metadata)) = self.ready_queue.pop_front() {
                self.last_metadata = metadata;
                return Ok(packet);
            }

            if self.eof {
                return Err(Error::Eof);
            }

            match self.inner.next_packet() {
                Ok(packet) => {
                    let metadata = self.inner.packet_metadata();
                    if let Some(stream_state) = self.streams.get_mut(&packet.stream_index) {
                        stream_state.process_packet(packet, metadata, &mut self.ready_queue);
                    } else {
                        self.last_metadata = metadata;
                        return Ok(packet);
                    }
                }
                Err(Error::Eof) => {
                    self.eof = true;
                    for stream in self.streams.values_mut() {
                        stream.handle_eof(&mut self.ready_queue);
                    }
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn packet_metadata(&self) -> PacketMetadata {
        self.last_metadata.clone()
    }

    fn set_active_streams(&mut self, indices: &[u32]) {
        for (&idx, stream) in self.streams.iter_mut() {
            if !indices.contains(&idx) {
                stream.reset_for_seek();
            }
        }
        self.inner.set_active_streams(indices);
    }

    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        self.ready_queue.clear();
        self.last_metadata = PacketMetadata::default();
        self.eof = false;
        for stream in self.streams.values_mut() {
            stream.reset_for_seek();
        }
        self.inner.seek_to(stream_index, pts)
    }

    fn metadata(&self) -> &[(String, String)] {
        self.inner.metadata()
    }

    fn duration_micros(&self) -> Option<i64> {
        self.inner.duration_micros()
    }

    fn attached_pictures(&self) -> &[AttachedPicture] {
        self.inner.attached_pictures()
    }

    fn chapters(&self) -> &[Chapter] {
        self.inner.chapters()
    }

    fn attachments(&self) -> &[Attachment] {
        self.inner.attachments()
    }
}

struct ParsedSlice {
    poc: i32,
    is_new_poc_period: bool,
    reorder_n: usize,
}

fn load_hevc_extradata(
    extradata: &[u8],
    sps_map: &mut HashMap<u8, oxideav_h265::sps::SeqParameterSet>,
    pps_map: &mut HashMap<u8, oxideav_h265::pps::PicParameterSet>,
) {
    if extradata.is_empty() {
        return;
    }
    let framing = entry::hevc_framing(extradata);
    if let entry::Framing::Length(_) = framing {
        if let Ok(rec) = oxideav_h265::hvcc::parse_hvcc(extradata) {
            for u in rec.nal_units {
                if u.header.nal_unit_type == 33 {
                    if let Ok(sps) = oxideav_h265::sps::SeqParameterSet::parse(&u.rbsp) {
                        sps_map.insert(sps.sps_id, sps);
                    }
                } else if u.header.nal_unit_type == 34 {
                    if let Ok(pps) = oxideav_h265::pps::PicParameterSet::parse(&u.rbsp) {
                        pps_map.insert(pps.pps_id, pps);
                    }
                }
            }
            return;
        }
    }
    for nal in entry::nals(extradata, framing) {
        if nal.len() >= 2 {
            if let Ok(h) = oxideav_h265::nal::NalHeader::parse(nal) {
                let rbsp = entry::rbsp(&nal[2..]);
                if h.nal_unit_type == 33 {
                    if let Ok(sps) = oxideav_h265::sps::SeqParameterSet::parse(&rbsp) {
                        sps_map.insert(sps.sps_id, sps);
                    }
                } else if h.nal_unit_type == 34 {
                    if let Ok(pps) = oxideav_h265::pps::PicParameterSet::parse(&rbsp) {
                        pps_map.insert(pps.pps_id, pps);
                    }
                }
            }
        }
    }
}

fn load_h264_extradata(
    extradata: &[u8],
    sps_map: &mut HashMap<u32, oxideav_h264::sps::Sps>,
    pps_map: &mut HashMap<u32, oxideav_h264::pps::Pps>,
) {
    if extradata.is_empty() {
        return;
    }
    let framing = entry::h264_framing(extradata);
    if extradata.len() >= 7 && extradata[0] == 1 {
        let num_sps = (extradata[5] & 0x1f) as usize;
        let mut pos = 6;
        for _ in 0..num_sps {
            if pos + 2 > extradata.len() {
                break;
            }
            let len = u16::from_be_bytes([extradata[pos], extradata[pos + 1]]) as usize;
            pos += 2;
            if pos + len > extradata.len() {
                break;
            }
            let nal = &extradata[pos..pos + len];
            pos += len;
            if !nal.is_empty() {
                let rbsp = entry::rbsp(&nal[1..]);
                if let Ok(sps) = oxideav_h264::sps::Sps::parse(&rbsp) {
                    sps_map.insert(sps.seq_parameter_set_id, sps);
                }
            }
        }
        if pos < extradata.len() {
            let num_pps = extradata[pos] as usize;
            pos += 1;
            for _ in 0..num_pps {
                if pos + 2 > extradata.len() {
                    break;
                }
                let len = u16::from_be_bytes([extradata[pos], extradata[pos + 1]]) as usize;
                pos += 2;
                if pos + len > extradata.len() {
                    break;
                }
                let nal = &extradata[pos..pos + len];
                pos += len;
                if !nal.is_empty() {
                    let rbsp = entry::rbsp(&nal[1..]);
                    if let Ok(pps) = oxideav_h264::pps::Pps::parse(&rbsp) {
                        pps_map.insert(pps.pic_parameter_set_id, pps);
                    }
                }
            }
        }
        return;
    }
    for nal in entry::nals(extradata, framing) {
        if !nal.is_empty() {
            let nal_type = nal[0] & 0x1f;
            let rbsp = entry::rbsp(&nal[1..]);
            if nal_type == 7 {
                if let Ok(sps) = oxideav_h264::sps::Sps::parse(&rbsp) {
                    sps_map.insert(sps.seq_parameter_set_id, sps);
                }
            } else if nal_type == 8 {
                if let Ok(pps) = oxideav_h264::pps::Pps::parse(&rbsp) {
                    pps_map.insert(pps.pic_parameter_set_id, pps);
                }
            }
        }
    }
}

fn parse_hevc_packet(
    packet_data: &[u8],
    framing: entry::Framing,
    sps_map: &mut HashMap<u8, oxideav_h265::sps::SeqParameterSet>,
    pps_map: &mut HashMap<u8, oxideav_h265::pps::PicParameterSet>,
    poc_state: &mut oxideav_h265::poc::PocState,
    first_pic_since_reset: &mut bool,
) -> std::result::Result<Option<ParsedSlice>, ()> {
    let nals = entry::nals(packet_data, framing);
    let mut has_eos = false;

    // 1. In-band parameter sets and EOS detection
    for &nal in &nals {
        if nal.len() < 2 {
            continue;
        }
        let Ok(header) = oxideav_h265::nal::NalHeader::parse(nal) else {
            continue;
        };
        if header.nuh_layer_id != 0 {
            continue;
        }
        if header.nal_unit_type == 33 {
            let rbsp = entry::rbsp(&nal[2..]);
            if let Ok(sps) = oxideav_h265::sps::SeqParameterSet::parse(&rbsp) {
                sps_map.insert(sps.sps_id, sps);
            }
        } else if header.nal_unit_type == 34 {
            let rbsp = entry::rbsp(&nal[2..]);
            if let Ok(pps) = oxideav_h265::pps::PicParameterSet::parse(&rbsp) {
                pps_map.insert(pps.pps_id, pps);
            }
        } else if header.nal_unit_type == 36 {
            has_eos = true;
        }
    }

    // 2. Picture's first slice
    let mut parsed_slice = None;
    for &nal in &nals {
        if nal.len() < 2 {
            continue;
        }
        let Ok(header) = oxideav_h265::nal::NalHeader::parse(nal) else {
            return Err(());
        };
        if header.nuh_layer_id != 0 || header.nal_unit_type > 31 {
            continue;
        }
        // Bound the RBSP conversion to at most first 4096 bytes of slice payload
        let payload = &nal[2..nal.len().min(4096 + 2)];
        let rbsp = entry::rbsp(payload);
        if rbsp.first().map_or(false, |b| b & 0x80 != 0) {
            let mut br = oxideav_h265::bitreader::BitReader::new(&rbsp);
            let _first = br.u1().map_err(|_| ())?;
            if (16..=23).contains(&header.nal_unit_type) {
                let _ = br.u1().map_err(|_| ())?;
            }
            let pps_id = br.ue().map_err(|_| ())?;
            if pps_id > 63 {
                return Err(());
            }
            let pps = pps_map.get(&(pps_id as u8)).ok_or(())?;
            let sps = sps_map.get(&pps.sps_id).ok_or(())?;

            let sh = oxideav_h265::slice::SliceSegmentHeader::parse(
                &rbsp,
                header.nal_unit_type,
                sps,
                pps,
            )
            .map_err(|_| ())?;

            let nal_kind = oxideav_h265::poc::NalKind::new(header.nal_unit_type);
            let is_idr = nal_kind.is_idr();
            let is_bla = nal_kind.is_bla();
            let is_cra = nal_kind.is_cra();
            let no_rasl_output = is_idr || is_bla || (is_cra && *first_pic_since_reset);
            *first_pic_since_reset = false;

            let max_sub_layers = sps.max_sub_layers_minus1 as usize;
            let reorder_n = sps.sub_layer_ordering_info[max_sub_layers].max_num_reorder_pics as usize;

            let max_poc_lsb = 1u32 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
            let poc_lsb = sh.slice_pic_order_cnt_lsb.unwrap_or(0);
            let poc_res = poc_state.decode_picture_poc(
                nal_kind,
                header.temporal_id,
                no_rasl_output,
                poc_lsb,
                max_poc_lsb,
            );

            let is_new_poc_period = nal_kind.is_irap() && no_rasl_output;

            parsed_slice = Some(ParsedSlice {
                poc: poc_res.val,
                is_new_poc_period,
                reorder_n,
            });
            break;
        }
    }

    // NAL 36 sets no_rasl_output for the NEXT picture
    if has_eos {
        *first_pic_since_reset = true;
    }

    Ok(parsed_slice)
}

fn parse_h264_packet(
    packet_data: &[u8],
    framing: entry::Framing,
    sps_map: &mut HashMap<u32, oxideav_h264::sps::Sps>,
    pps_map: &mut HashMap<u32, oxideav_h264::pps::Pps>,
    poc_state: &mut oxideav_h264::poc::PocState,
) -> std::result::Result<Option<ParsedSlice>, ()> {
    let nals = entry::nals(packet_data, framing);

    // 1. In-band parameter sets
    for &nal in &nals {
        if nal.is_empty() {
            continue;
        }
        let nal_type = nal[0] & 0x1f;
        if nal_type == 7 {
            let rbsp = entry::rbsp(&nal[1..]);
            if let Ok(sps) = oxideav_h264::sps::Sps::parse(&rbsp) {
                sps_map.insert(sps.seq_parameter_set_id, sps);
            }
        } else if nal_type == 8 {
            let rbsp = entry::rbsp(&nal[1..]);
            if let Ok(pps) = oxideav_h264::pps::Pps::parse(&rbsp) {
                pps_map.insert(pps.pic_parameter_set_id, pps);
            }
        }
    }

    // 2. Picture's first slice
    for &nal in &nals {
        if nal.is_empty() {
            continue;
        }
        let nal_type = nal[0] & 0x1f;
        let nal_ref_idc = (nal[0] >> 5) & 3;
        if !(1..=5).contains(&nal_type) {
            continue;
        }
        // Bound the RBSP conversion to at most first 4096 bytes of slice payload
        let payload = &nal[1..nal.len().min(4096 + 1)];
        let rbsp = entry::rbsp(payload);
        if rbsp.first().map_or(false, |b| b & 0x80 != 0) {
            // first_mb_in_slice == 0
            let mut br = oxideav_h265::bitreader::BitReader::new(&rbsp);
            let first_mb = br.ue().map_err(|_| ())?;
            if first_mb != 0 {
                continue;
            }
            let _slice_type = br.ue().map_err(|_| ())?;
            let pps_id = br.ue().map_err(|_| ())?;
            if pps_id > 255 {
                return Err(());
            }
            let pps = pps_map.get(&pps_id).ok_or(())?;
            let sps = sps_map.get(&pps.seq_parameter_set_id).ok_or(())?;

            let bitstream_restriction = sps
                .vui
                .as_ref()
                .and_then(|v| v.bitstream_restriction.as_ref())
                .ok_or(())?;
            let reorder_n = bitstream_restriction.max_num_reorder_frames as usize;

            let nal_header = oxideav_h264::nal::NalHeader {
                forbidden_zero_bit: (nal[0] >> 7) & 1,
                nal_ref_idc,
                nal_unit_type: oxideav_h264::nal::NalUnitType::from_u8(nal_type),
            };

            let sh = oxideav_h264::slice_header::SliceHeader::parse(
                &rbsp,
                sps,
                pps,
                &nal_header,
            )
            .map_err(|_| ())?;

            if sh.field_pic_flag {
                return Err(());
            }

            let has_mmco5 = sh
                .dec_ref_pic_marking
                .as_ref()
                .and_then(|m| m.adaptive_marking.as_ref())
                .map_or(false, |ops| {
                    ops.iter()
                        .any(|op| matches!(op, oxideav_h264::slice_header::MmcoOp::MarkAllUnused))
                });
            if has_mmco5 {
                return Err(());
            }

            let poc_sps = oxideav_h264::poc::PocSps {
                pic_order_cnt_type: sps.pic_order_cnt_type,
                log2_max_frame_num_minus4: sps.log2_max_frame_num_minus4,
                log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
                delta_pic_order_always_zero_flag: sps.delta_pic_order_always_zero_flag,
                offset_for_non_ref_pic: sps.offset_for_non_ref_pic,
                offset_for_top_to_bottom_field: sps.offset_for_top_to_bottom_field,
                num_ref_frames_in_pic_order_cnt_cycle: sps.num_ref_frames_in_pic_order_cnt_cycle,
                offset_for_ref_frame: sps.offset_for_ref_frame.clone(),
                frame_mbs_only_flag: sps.frame_mbs_only_flag,
            };

            let is_idr = nal_type == 5;
            let is_reference = nal_ref_idc != 0;

            let poc_slice = oxideav_h264::poc::PocSlice {
                is_reference,
                is_idr,
                frame_num: sh.frame_num,
                field_pic_flag: sh.field_pic_flag,
                bottom_field_flag: sh.bottom_field_flag,
                pic_order_cnt_lsb: sh.pic_order_cnt_lsb,
                delta_pic_order_cnt_bottom: sh.delta_pic_order_cnt_bottom,
                delta_pic_order_cnt: sh.delta_pic_order_cnt,
                prev_had_mmco5: false,
                prev_reference_top_foc_for_mmco5: 0,
            };

            let poc_res = oxideav_h264::poc::derive_poc(&poc_sps, &poc_slice, poc_state).map_err(|_| ())?;

            return Ok(Some(ParsedSlice {
                poc: poc_res.pic_order_cnt,
                is_new_poc_period: is_idr,
                reorder_n,
            }));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic sequence unit tests for `Reorder`.
    //
    // These streams cannot be generated by FFmpeg's encoders on demand:
    //
    // - `presentation_order_not_switched_on_idr_without_reordering`: Catches
    //   false-positive `Repair` transition when an IDR picture (POC 0) follows
    //   a POC period that had no reordering, comparing across POC period boundaries
    //   instead of gating on `!is_new_poc_period`. When broken, the stream switches
    //   to Repair and delays every subsequent picture by D slots.
    //
    // - `decode_order_stamps_repaired_and_extrapolated_at_eof`: Catches failure
    //   to detect decode-order timestamps, failure to bump pictures by POC,
    //   failure to shift by `D = N`, or failure to extrapolate the final `D`
    //   slots at inner EOF using the last slot step.
    //
    // - `seek_reset_clears_max_poc_so_non_idr_landing_does_not_flip_to_repair`: Catches
    //   failure to clear `max_poc_in_period` on seek, where a non-IDR landing with
    //   a smaller POC compared against the pre-seek max POC flips a correctly stamped
    //   stream to `Repair`.
    //
    // - `presentation_order_recovers_to_trust_on_contradictory_smaller_stamp`: Catches
    //   failure to fall back to Trust when a stream flipped to Repair by a corrupt stamp
    //   subsequently contradicts decode order (a smaller POC picture carrying a smaller
    //   container stamp within the same POC period).
    //
    // - `held_bytes_budget_exceeded_flips_to_trust_and_flushes_all`: Catches failure
    //   to enforce VIDEO_MAX_BYTES budget on held packets, allowing untrusted peers
    //   to exhaust memory before the engine's lane byte gate.

    #[test]
    fn presentation_order_not_switched_on_idr_without_reordering() {
        let mut reorder = Reorder::new();
        let mut releases = Vec::new();

        // GOP 1 (POC period 1): no reordering, strictly increasing POC and stamps
        // Pic 0: IDR, POC 0, stamp 0
        reorder.push(
            0,
            InputPacket::Picture {
                poc: 0,
                is_new_poc_period: true,
                reorder_n: 2,
                container_stamp: 0,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );
        // Pic 1: P, POC 1, stamp 10
        reorder.push(
            1,
            InputPacket::Picture {
                poc: 1,
                is_new_poc_period: false,
                reorder_n: 2,
                container_stamp: 10,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );
        // Pic 2: P, POC 2, stamp 20
        reorder.push(
            2,
            InputPacket::Picture {
                poc: 2,
                is_new_poc_period: false,
                reorder_n: 2,
                container_stamp: 20,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );

        // GOP 2 (POC period 2): IDR arrives with POC 0 and stamp 30.
        // If compared against GOP 1's max POC (2) with stamp 20, POC 0 < POC 2 and stamp 30 > 20
        // would falsely trigger Repair mode.
        reorder.push(
            3,
            InputPacket::Picture {
                poc: 0,
                is_new_poc_period: true,
                reorder_n: 2,
                container_stamp: 30,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );

        // Pic 4: P, POC 2, stamp 50
        reorder.push(
            4,
            InputPacket::Picture {
                poc: 2,
                is_new_poc_period: false,
                reorder_n: 2,
                container_stamp: 50,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );
        // Pic 5: B, POC 1, stamp 40.
        // Reordering in GOP 2: POC 1 < POC 2, and stamp 40 < 50 => switches to Trust!
        reorder.push(
            5,
            InputPacket::Picture {
                poc: 1,
                is_new_poc_period: false,
                reorder_n: 2,
                container_stamp: 40,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );

        reorder.handle_eof(&mut releases);

        // Mode must be Trust, and NO release may have a repaired PTS (all None).
        assert_eq!(reorder.mode, Mode::Trust);
        for rel in &releases {
            assert_eq!(
                rel.repaired_pts, None,
                "packet {} must not have its PTS rewritten",
                rel.packet_id
            );
        }
    }

    #[test]
    fn decode_order_stamps_repaired_and_extrapolated_at_eof() {
        let mut reorder = Reorder::new();
        let mut releases = Vec::new();

        // N = 2, decode order:
        // Pic 0: IDR, POC 0, stamp 0
        reorder.push(
            0,
            InputPacket::Picture {
                poc: 0,
                is_new_poc_period: true,
                reorder_n: 2,
                container_stamp: 0,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );
        // Pic 1: P, POC 2, stamp 10
        reorder.push(
            1,
            InputPacket::Picture {
                poc: 2,
                is_new_poc_period: false,
                reorder_n: 2,
                container_stamp: 10,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );
        // Pic 2: B, POC 1, stamp 20.
        // POC 1 < POC 2 and stamp 20 > 10 => switches to Repair! D = 2.
        reorder.push(
            2,
            InputPacket::Picture {
                poc: 1,
                is_new_poc_period: false,
                reorder_n: 2,
                container_stamp: 20,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );
        // Pic 3: P, POC 4, stamp 30
        reorder.push(
            3,
            InputPacket::Picture {
                poc: 4,
                is_new_poc_period: false,
                reorder_n: 2,
                container_stamp: 30,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );
        // Pic 4: B, POC 3, stamp 40
        reorder.push(
            4,
            InputPacket::Picture {
                poc: 3,
                is_new_poc_period: false,
                reorder_n: 2,
                container_stamp: 40,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );

        reorder.handle_eof(&mut releases);

        assert_eq!(reorder.mode, Mode::Repair);
        // Packets leave in decode order: 0, 1, 2, 3, 4
        assert_eq!(releases.len(), 5);
        for (i, rel) in releases.iter().enumerate() {
            assert_eq!(rel.packet_id, i);
        }

        // Display placement:
        // Pic 0 (POC 0) is display 0 -> slot 0 + 2 = slot[2] = 20
        // Pic 2 (POC 1) is display 1 -> slot 1 + 2 = slot[3] = 30
        // Pic 1 (POC 2) is display 2 -> slot 2 + 2 = slot[4] = 40
        // Pic 4 (POC 3) is display 3 -> slot 3 + 2 = slot[5] = 50 (extrapolated)
        // Pic 3 (POC 4) is display 4 -> slot 4 + 2 = slot[6] = 60 (extrapolated)
        assert_eq!(releases[0].repaired_pts, Some(20));
        assert_eq!(releases[1].repaired_pts, Some(40));
        assert_eq!(releases[2].repaired_pts, Some(30));
        assert_eq!(releases[3].repaired_pts, Some(60));
        assert_eq!(releases[4].repaired_pts, Some(50));
    }

    #[test]
    fn seek_reset_clears_max_poc_so_non_idr_landing_does_not_flip_to_repair() {
        let mut reorder = Reorder::new();
        let mut releases = Vec::new();

        for i in 0..=10 {
            reorder.push(
                i as usize,
                InputPacket::Picture {
                    poc: i,
                    is_new_poc_period: i == 0,
                    reorder_n: 2,
                    container_stamp: (i * 10) as i64,
                    duration: Some(10),
                    bytes: 100,
                },
                &mut releases,
            );
        }
        assert_eq!(reorder.mode, Mode::Undecided);

        reorder.reset_for_seek();
        releases.clear();

        for i in 4..=8 {
            reorder.push(
                100 + i as usize,
                InputPacket::Picture {
                    poc: i,
                    is_new_poc_period: false,
                    reorder_n: 2,
                    container_stamp: (200 + (i - 4) * 10) as i64,
                    duration: Some(10),
                    bytes: 100,
                },
                &mut releases,
            );
        }

        reorder.handle_eof(&mut releases);

        assert_ne!(reorder.mode, Mode::Repair, "seek reset must not allow pre-seek POC to trigger Repair");
        for rel in &releases {
            assert_eq!(
                rel.repaired_pts, None,
                "packet {} must not carry a repaired pts",
                rel.packet_id
            );
        }
    }

    #[test]
    fn presentation_order_recovers_to_trust_on_contradictory_smaller_stamp() {
        let mut reorder = Reorder::new();
        let mut releases = Vec::new();

        reorder.push(
            0,
            InputPacket::Picture {
                poc: 0,
                is_new_poc_period: true,
                reorder_n: 2,
                container_stamp: 0,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );
        reorder.push(
            1,
            InputPacket::Picture {
                poc: 4,
                is_new_poc_period: false,
                reorder_n: 2,
                container_stamp: 40,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );
        // Corrupt stamp 50 > 40 on POC 2 < 4 triggers Repair
        reorder.push(
            2,
            InputPacket::Picture {
                poc: 2,
                is_new_poc_period: false,
                reorder_n: 2,
                container_stamp: 50,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );
        assert_eq!(reorder.mode, Mode::Repair);

        reorder.push(
            3,
            InputPacket::Picture {
                poc: 6,
                is_new_poc_period: false,
                reorder_n: 2,
                container_stamp: 60,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );
        // Normal smaller stamp 45 < 60 on POC 5 < 6 contradicts decode order
        reorder.push(
            4,
            InputPacket::Picture {
                poc: 5,
                is_new_poc_period: false,
                reorder_n: 2,
                container_stamp: 45,
                duration: Some(10),
                bytes: 100,
            },
            &mut releases,
        );

        reorder.handle_eof(&mut releases);

        assert_eq!(reorder.mode, Mode::Trust);
        for rel in releases.iter().filter(|r| r.packet_id >= 2) {
            assert_eq!(
                rel.repaired_pts, None,
                "packet {} after contradiction fallback must not have repaired pts",
                rel.packet_id
            );
        }
    }

    #[test]
    fn held_bytes_budget_exceeded_flips_to_trust_and_flushes_all() {
        let mut reorder = Reorder::new();
        let mut releases = Vec::new();

        // 4 pictures with N = 4 (none released by bumping), each charging 10 MiB.
        // Sum = 40 MiB > 32 MiB VIDEO_MAX_BYTES.
        let large_bytes = 10 * 1024 * 1024;
        for i in 0..4 {
            reorder.push(
                i,
                InputPacket::Picture {
                    poc: (i * 2) as i32,
                    is_new_poc_period: i == 0,
                    reorder_n: 4,
                    container_stamp: (i * 10) as i64,
                    duration: Some(10),
                    bytes: large_bytes,
                },
                &mut releases,
            );
        }

        assert_eq!(reorder.mode, Mode::Trust, "exceeding held byte budget must switch mode to Trust");
        assert_eq!(reorder.held_pictures.len(), 0, "nothing must remain held after budget breach");
        assert_eq!(releases.len(), 4, "all held pictures must be released");
        for (i, rel) in releases.iter().enumerate() {
            assert_eq!(rel.packet_id, i, "releases must be in decode order");
            assert_eq!(rel.repaired_pts, None, "releases after Trust must have no repaired pts");
        }
    }
}
