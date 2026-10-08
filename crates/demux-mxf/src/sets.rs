// Ported from FFmpeg libavformat/mxfdec.c (commit 2da55bf): the metadata set
// structures, mxf_read_primer_pack, mxf_read_partition_pack, partition_score,
// mxf_add_metadata_set, the set readers of mxf_metadata_read_table,
// mxf_read_local_tags and mxf_resolve_strong_ref.
// License: LGPL-2.1-or-later

//! Header metadata: partition packs and the local-tag sets FFmpeg reads.
//! Sets FFmpeg only turns into metadata dictionaries (identification,
//! preface, timecode, tagged values, MCA labels) are skipped; nothing a
//! packet or stream parameter depends on comes from them.

use oxideav_core::{Error, Result};

use crate::klv::{Io, Klv};
use crate::types::*;

/// MXFPartition.
#[derive(Clone, Copy, Debug, Default)]
pub struct Partition {
    pub closed: bool,
    pub complete: bool,
    pub ty: PartitionType,
    pub previous_partition: u64,
    pub index_sid: i32,
    pub body_sid: i32,
    pub essence_offset: i64,
    pub essence_length: i64,
    pub kag_size: i32,
    pub pack_ofs: i64,
    pub body_offset: i64,
    pub first_essence_klv: Klv,
}

/// partition_score: footers win over complete, closed, open partitions,
/// later partitions over earlier ones.
pub fn partition_score(p: Option<&Partition>) -> u64 {
    let Some(p) = p else { return 0 };
    let score: u64 = if p.ty == PartitionType::Footer {
        5
    } else if p.complete {
        4
    } else if p.closed {
        3
    } else {
        1
    };
    (score << 60) | ((p.pack_ofs as u64) >> 4)
}

/// MXFStructuralComponent (a SourceClip).
#[derive(Clone, Debug, Default)]
pub struct SourceClip {
    pub source_package_ul: Uid,
    pub source_package_uid: Uid,
    pub duration: i64,
    pub start_position: i64,
    pub source_track_id: i32,
}

/// MXFSequence.
#[derive(Clone, Debug, Default)]
pub struct Sequence {
    pub data_definition_ul: Uid,
    pub structural_components_refs: Vec<Uid>,
    pub duration: i64,
}

/// MXFEssenceGroup.
#[derive(Clone, Debug, Default)]
pub struct EssenceGroup {
    pub structural_components_refs: Vec<Uid>,
    pub duration: i64,
}

/// MXFTrack, as read.
#[derive(Clone, Debug, Default)]
pub struct Track {
    pub sequence_ref: Uid,
    pub track_id: i32,
    pub name: Option<String>,
    pub track_number: [u8; 4],
    pub edit_rate: (i32, i32),
    pub origin: i64,
}

/// MXFPackage.
#[derive(Clone, Debug, Default)]
pub struct Package {
    pub package_uid: Uid,
    pub package_ul: Uid,
    pub tracks_refs: Vec<Uid>,
    pub descriptor_ref: Uid,
    pub name: Option<String>,
}

/// MXFDescriptor, the fields stream parameters come from.
#[derive(Clone, Debug)]
pub struct Descriptor {
    pub essence_container_ul: Uid,
    pub essence_codec_ul: Uid,
    pub codec_ul: Uid,
    pub sample_rate: (i32, i32),
    pub aspect_ratio: (i32, i32),
    pub width: i32,
    pub height: i32,
    pub frame_layout: u8,
    pub video_line_map: [i32; 2],
    pub field_dominance: u8,
    pub channels: i32,
    pub bits_per_sample: i32,
    /// ContainerDuration, None for AV_NOPTS_VALUE.
    pub duration: Option<i64>,
    pub component_depth: u32,
    pub horiz_subsampling: u32,
    pub vert_subsampling: u32,
    pub file_descriptors_refs: Vec<Uid>,
    pub sub_descriptors_refs: Vec<Uid>,
    pub linked_track_id: i32,
    pub extradata: Option<Vec<u8>>,
}

impl Default for Descriptor {
    fn default() -> Self {
        // mxf_metadataset_init: duration AV_NOPTS_VALUE.
        Self {
            essence_container_ul: [0; 16],
            essence_codec_ul: [0; 16],
            codec_ul: [0; 16],
            sample_rate: (0, 0),
            aspect_ratio: (0, 0),
            width: 0,
            height: 0,
            frame_layout: 0,
            video_line_map: [0; 2],
            field_dominance: 0,
            channels: 0,
            bits_per_sample: 0,
            duration: None,
            component_depth: 0,
            horiz_subsampling: 0,
            vert_subsampling: 0,
            file_descriptors_refs: Vec::new(),
            sub_descriptors_refs: Vec::new(),
            linked_track_id: 0,
            extradata: None,
        }
    }
}

/// MXFIndexTableSegment.
#[derive(Clone, Debug, Default)]
pub struct IndexSegment {
    pub edit_unit_byte_count: u32,
    pub index_sid: i32,
    pub body_sid: i32,
    pub index_edit_rate: (i32, i32),
    pub index_start_position: u64,
    pub index_duration: u64,
    pub temporal_offset_entries: Vec<i8>,
    pub flag_entries: Vec<u8>,
    pub stream_offset_entries: Vec<u64>,
    /// Whether an IndexEntryArray was read (FFmpeg's non-NULL arrays).
    pub has_entries: bool,
    /// Where the segment's essence starts in its container
    /// (mxf_compute_index_tables fills it in).
    pub offset: i64,
}

impl IndexSegment {
    pub fn nb_index_entries(&self) -> usize {
        self.stream_offset_entries.len()
    }
}

/// MXFEssenceContainerData.
#[derive(Clone, Debug, Default)]
pub struct EssenceContainerData {
    pub package_uid: Uid,
    pub package_ul: Uid,
    pub index_sid: i32,
    pub body_sid: i32,
}

/// MXFMetadataSetType, the groups FFmpeg keeps sets in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetType {
    CryptoContext,
    SourceClip,
    Sequence,
    EssenceGroup,
    Track,
    MaterialPackage,
    SourcePackage,
    MultipleDescriptor,
    Descriptor,
    IndexTableSegment,
    EssenceContainerData,
    Ffv1SubDescriptor,
}

const SET_TYPES: usize = 12;

/// One set's contents.
#[derive(Clone, Debug)]
pub enum SetData {
    CryptoContext(Uid),
    SourceClip(SourceClip),
    Sequence(Sequence),
    EssenceGroup(EssenceGroup),
    Track(Track),
    Package(Package),
    Descriptor(Descriptor),
    IndexSegment(IndexSegment),
    EssenceContainerData(EssenceContainerData),
    Ffv1Extradata(Option<Vec<u8>>),
}

/// MXFMetadataSet: an instance UID, the score of the partition it came
/// from, its contents.
#[derive(Clone, Debug)]
pub struct Set {
    pub uid: Uid,
    pub partition_score: u64,
    pub data: SetData,
}

/// What the content storage set refers to.
#[derive(Clone, Debug, Default)]
pub struct ContentStorage {
    pub packages_refs: Vec<Uid>,
    pub essence_container_data_refs: Vec<Uid>,
}

/// The metadata set groups, the primer pack's local tags, the content
/// storage references.
#[derive(Debug, Default)]
pub struct Store {
    groups: [Vec<Set>; SET_TYPES],
    local_tags: Vec<(u16, Uid)>,
    pub storage: ContentStorage,
}

/// At most this many sets are kept per group, a bound FFmpeg does not
/// have (untrusted input).
const MAX_SETS: usize = 1 << 17;

impl Store {
    pub fn group(&self, ty: SetType) -> &[Set] {
        &self.groups[ty as usize]
    }

    /// mxf_add_metadata_set: a set whose instance UID a set from a better
    /// partition already has is dropped; index table segments are all
    /// kept.
    pub fn add(&mut self, ty: SetType, set: Set) -> Result<()> {
        let group = &mut self.groups[ty as usize];
        if ty != SetType::IndexTableSegment
            && group.iter().any(|old| old.uid == set.uid && old.partition_score > set.partition_score)
        {
            return Ok(());
        }
        if group.len() >= MAX_SETS {
            return Err(Error::invalid("mxf: too many metadata sets"));
        }
        group.push(set);
        Ok(())
    }

    /// mxf_resolve_strong_ref: the last set of the group with that UID.
    pub fn resolve(&self, uid: &Uid, ty: SetType) -> Option<&Set> {
        self.groups[ty as usize].iter().rev().find(|s| &s.uid == uid)
    }

    /// mxf_read_primer_pack.
    pub fn read_primer_pack(&mut self, io: &mut Io) -> Result<()> {
        let item_num = io.rb32() as i32;
        let item_len = io.rb32() as i32;
        if item_len != 18 {
            return Err(Error::unsupported(format!("mxf: primer pack item length {item_len}")));
        }
        if !(0..=65536).contains(&item_num) {
            return Err(Error::invalid("mxf: primer pack item count too large"));
        }
        let mut items = vec![0u8; item_num as usize * 18];
        let got = io.read(&mut items);
        // avio_read short at the end leaves the rest zeroed (av_calloc).
        items[got..].fill(0);
        self.local_tags = items.chunks_exact(18).map(|c| (u16::from_be_bytes([c[0], c[1]]), c[2..18].try_into().unwrap())).collect();
        Ok(())
    }

    /// The UID of dynamic local tag `tag`, all-zero when the primer pack
    /// has none (the last matching item wins).
    fn dynamic_uid(&self, tag: u16) -> Uid {
        self.local_tags.iter().rev().find(|(t, _)| *t == tag).map_or([0; 16], |(_, uid)| *uid)
    }
}

/// mxf_read_partition_pack's results for the context.
pub struct PartitionPack {
    pub partition: Partition,
    pub footer_partition: u64,
    pub op: Op,
}

/// mxf_read_partition_pack, `io` after the KLV's length. `parsing_backward`
/// and `prev_forward` (the pack_ofs of the partition before the last one
/// parsed forward) feed FFmpeg's PreviousPartition override.
pub fn read_partition_pack(
    io: &mut Io,
    klv: &Klv,
    run_in: u64,
    parsing_backward: bool,
    prev_forward: Option<i64>,
) -> Result<PartitionPack> {
    let mut p = Partition { pack_ofs: klv.offset as i64, ..Default::default() };
    p.ty = match klv.key[13] {
        2 => PartitionType::Header,
        3 => PartitionType::BodyPartition,
        4 => PartitionType::Footer,
        _ => return Err(Error::invalid("mxf: unknown partition type")),
    };
    // Both footers count as closed (there is only Footer and CompleteFooter).
    p.closed = p.ty == PartitionType::Footer || klv.key[14] & 1 == 0;
    p.complete = klv.key[14] > 2;
    io.skip(4)?;
    p.kag_size = io.rb32() as i32;
    let this_partition = io.rb64();
    if this_partition != klv.offset - run_in {
        return Err(Error::invalid("mxf: ThisPartition mismatches the pack's position"));
    }
    p.previous_partition = io.rb64();
    let footer_partition = io.rb64();
    let _header_byte_count = io.rb64();
    let _index_byte_count = io.rb64();
    p.index_sid = io.rb32() as i32;
    p.body_offset = io.rb64() as i64;
    p.body_sid = io.rb32() as i32;
    if p.body_offset < 0 {
        return Err(Error::invalid("mxf: negative BodyOffset"));
    }
    let mut op = [0u8; 16];
    io.read_exact(&mut op)?;
    let nb_essence_containers = io.rb32();

    if this_partition != 0 && p.previous_partition == this_partition {
        // Override with the actual previous partition offset.
        if let (false, Some(prev)) = (parsing_backward, prev_forward) {
            p.previous_partition = (prev as u64).wrapping_sub(run_in);
        }
        // Without a previous body partition, point to the header partition.
        if p.previous_partition == this_partition {
            p.previous_partition = 0;
        }
    }
    // Sanity check PreviousPartition if set.
    if p.previous_partition != 0 && run_in.wrapping_add(p.previous_partition) >= klv.offset {
        return Err(Error::invalid("mxf: PreviousPartition points to this partition or forward"));
    }

    let op = match (op[12], op[13]) {
        (1, 1) => Op::Op1a,
        (1, 2) => Op::Op1b,
        (1, 3) => Op::Op1c,
        (2, 1) => Op::Op2a,
        (2, 2) => Op::Op2b,
        (2, 3) => Op::Op2c,
        (3, 1) => Op::Op3a,
        (3, 2) => Op::Op3b,
        (3, 3) => Op::Op3c,
        (64, 1) => Op::OpSonyOpt,
        (0x10, _) => {
            // SMPTE 390m: "There shall be exactly one essence container";
            // files that violate this are taken as OP1a (some) or OPAtom
            // (none).
            if nb_essence_containers != 1 {
                if nb_essence_containers != 0 { Op::Op1a } else { Op::OpAtom }
            } else {
                Op::OpAtom
            }
        }
        // Unknown operational pattern: guessing OP1a.
        _ => Op::Op1a,
    };
    if p.kag_size <= 0 || p.kag_size > 1 << 20 {
        p.kag_size = if op == Op::OpSonyOpt { 512 } else { 1 };
    }
    Ok(PartitionPack { partition: p, footer_partition, op })
}

/// The metadata set readers of mxf_metadata_read_table, by key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reader {
    PrimerPack,
    PartitionPack,
    ContentStorage,
    Package(SetType),
    Sequence,
    EssenceGroup,
    SourceClip,
    Descriptor(SetType),
    Ffv1SubDescriptor,
    Track,
    CryptoContext,
    IndexTableSegment,
    EssenceContainerData,
    /// A set FFmpeg reads for metadata only, or KLV fill: skipped.
    Skip,
}

const fn set_key(b14: u8, b15: u8) -> Uid {
    [0x06, 0x0e, 0x2b, 0x34, 0x02, 0x53, 0x01, 0x01, 0x0d, 0x01, 0x01, 0x01, 0x01, 0x01, b14, b15]
}

const fn pack_key(b13: u8, b14: u8) -> Uid {
    [0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, b13, b14, 0x00]
}

/// mxf_metadata_read_table.
const READ_TABLE: &[(Uid, Reader)] = &[
    (pack_key(0x05, 0x01), Reader::PrimerPack),
    (pack_key(0x02, 0x01), Reader::PartitionPack),
    (pack_key(0x02, 0x02), Reader::PartitionPack),
    (pack_key(0x02, 0x03), Reader::PartitionPack),
    (pack_key(0x02, 0x04), Reader::PartitionPack),
    (pack_key(0x03, 0x01), Reader::PartitionPack),
    (pack_key(0x03, 0x02), Reader::PartitionPack),
    (pack_key(0x03, 0x03), Reader::PartitionPack),
    (pack_key(0x03, 0x04), Reader::PartitionPack),
    (pack_key(0x04, 0x02), Reader::PartitionPack),
    (pack_key(0x04, 0x04), Reader::PartitionPack),
    (set_key(0x2f, 0x00), Reader::Skip), // preface metadata
    (set_key(0x30, 0x00), Reader::Skip), // identification metadata
    (set_key(0x18, 0x00), Reader::ContentStorage),
    (set_key(0x37, 0x00), Reader::Package(SetType::SourcePackage)),
    (set_key(0x36, 0x00), Reader::Package(SetType::MaterialPackage)),
    (set_key(0x0f, 0x00), Reader::Sequence),
    (set_key(0x05, 0x00), Reader::EssenceGroup),
    (set_key(0x11, 0x00), Reader::SourceClip),
    (set_key(0x3f, 0x00), Reader::Skip), // tagged value
    (set_key(0x44, 0x00), Reader::Descriptor(SetType::MultipleDescriptor)),
    (set_key(0x42, 0x00), Reader::Descriptor(SetType::Descriptor)), // Generic Sound
    (set_key(0x28, 0x00), Reader::Descriptor(SetType::Descriptor)), // CDCI
    (set_key(0x29, 0x00), Reader::Descriptor(SetType::Descriptor)), // RGBA
    (set_key(0x48, 0x00), Reader::Descriptor(SetType::Descriptor)), // Wave
    (set_key(0x47, 0x00), Reader::Descriptor(SetType::Descriptor)), // AES3
    (set_key(0x51, 0x00), Reader::Descriptor(SetType::Descriptor)), // MPEG2VideoDescriptor
    (set_key(0x5b, 0x00), Reader::Descriptor(SetType::Descriptor)), // VBI - SMPTE 436M
    (set_key(0x5c, 0x00), Reader::Descriptor(SetType::Descriptor)), // VANC/VBI - SMPTE 436M
    (set_key(0x5e, 0x00), Reader::Descriptor(SetType::Descriptor)), // MPEG2AudioDescriptor
    (set_key(0x64, 0x00), Reader::Descriptor(SetType::Descriptor)), // DC Timed Text Descriptor
    (set_key(0x6b, 0x00), Reader::Skip), // MCA sub-descriptors
    (set_key(0x6c, 0x00), Reader::Skip),
    (set_key(0x6d, 0x00), Reader::Skip),
    (set_key(0x81, 0x03), Reader::Ffv1SubDescriptor),
    (set_key(0x3a, 0x00), Reader::Track), // Static Track
    (set_key(0x3b, 0x00), Reader::Track), // Generic Track
    (set_key(0x14, 0x00), Reader::Skip), // timecode component
    (set_key(0x0c, 0x00), Reader::Skip), // pulldown component
    ([0x06, 0x0e, 0x2b, 0x34, 0x02, 0x53, 0x01, 0x01, 0x0d, 0x01, 0x04, 0x01, 0x02, 0x02, 0x00, 0x00], Reader::CryptoContext),
    ([0x06, 0x0e, 0x2b, 0x34, 0x02, 0x53, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, 0x10, 0x01, 0x00], Reader::IndexTableSegment),
    (set_key(0x23, 0x00), Reader::EssenceContainerData),
    ([0x06, 0x0e, 0x2b, 0x34, 0x01, 0x01, 0x01, 0x02, 0x03, 0x01, 0x02, 0x10, 0x01, 0x00, 0x00, 0x00], Reader::Skip), // KLV fill
];

/// The table entry for `key` (IS_KLV_KEY over all 16 bytes).
pub fn reader_for(key: &Uid) -> Option<Reader> {
    READ_TABLE.iter().find(|(k, _)| k == key).map(|&(_, r)| r)
}

/// mxf_read_strong_ref_array, within `left` bytes of the set.
fn read_strong_ref_array(io: &mut Io, left: u64) -> Result<Vec<Uid>> {
    let count = io.rb32();
    if count > i32::MAX as u32 / 16 {
        return Err(Error::unsupported("mxf: strong reference array too large"));
    }
    io.skip(4)?; // size of the objects, always 16
    // FFmpeg reads count UIDs wherever they end; past the set's end it
    // then fails the set. Refuse those counts before allocating.
    if u64::from(count) * 16 > left.saturating_sub(8) {
        return Err(Error::invalid("mxf: strong reference array past its set"));
    }
    // Grown as the UIDs are read: a count the input does not back costs
    // no memory.
    let mut refs = Vec::new();
    for _ in 0..count {
        refs.push(io.uid());
        if io.feof() {
            return Err(Error::invalid("mxf: strong reference array cut short"));
        }
    }
    Ok(refs)
}

/// mxf_read_utf16be_string: `size` bytes of UTF-16BE, to the first NUL.
fn read_utf16be(io: &mut Io, size: u64) -> String {
    let mut units = Vec::new();
    let mut left = size;
    while left >= 2 {
        let u = io.rb16();
        left -= 2;
        if u == 0 {
            break;
        }
        units.push(u);
    }
    if left > 0 {
        let _ = io.skip(left as i64);
    }
    String::from_utf16_lossy(&units)
}

/// mxf_read_index_entry_array.
fn read_index_entry_array(io: &mut Io, segment: &mut IndexSegment, left: u64) -> Result<()> {
    if segment.has_entries {
        return Err(Error::invalid("mxf: second IndexEntryArray"));
    }
    let nb_index_entries = io.rb32();
    if nb_index_entries > i32::MAX as u32 {
        return Err(Error::invalid("mxf: too many index entries"));
    }
    let length = io.rb32() as i32;
    if nb_index_entries != 0 && length < 11 {
        return Err(Error::invalid("mxf: index entries shorter than 11 bytes"));
    }
    // The entries must fit the set (FFmpeg reads on into the next KLV and
    // then fails the set); checked before allocating.
    if u64::from(nb_index_entries) * length.max(0) as u64 > left.saturating_sub(8) {
        return Err(Error::invalid("mxf: index entries past their set"));
    }
    // Grown as the entries are read: a count the input does not back
    // costs no memory.
    let n = nb_index_entries as usize;
    segment.has_entries = true;
    segment.temporal_offset_entries = Vec::new();
    segment.flag_entries = Vec::new();
    segment.stream_offset_entries = Vec::new();
    for _ in 0..n {
        if io.feof() {
            return Err(Error::invalid("mxf: index entries cut short"));
        }
        segment.temporal_offset_entries.push(io.r8() as i8);
        io.r8(); // KeyFrameOffset
        segment.flag_entries.push(io.r8());
        segment.stream_offset_entries.push(io.rb64());
        io.skip(i64::from(length) - 11)?;
    }
    Ok(())
}

/// One local tag of a set: `size` bytes at the reader, `uid` its dynamic
/// UID (all-zero for a static tag), `left` the bytes left in the set.
fn read_tag(data: &mut SetData, io: &mut Io, tag: u16, size: u64, uid: &Uid, left: u64) -> Result<()> {
    match data {
        SetData::CryptoContext(source_container_ul) => {
            if size != 16 {
                return Err(Error::invalid("mxf: cryptographic context of the wrong size"));
            }
            if uid == &CRYPTO_SOURCE_CONTAINER_UL {
                *source_container_ul = io.uid();
            }
        }
        SetData::SourceClip(c) => match tag {
            0x0202 => c.duration = io.rb64() as i64,
            0x1201 => c.start_position = io.rb64() as i64,
            0x1101 => {
                // UMID: only the last 16 bytes... FFmpeg keeps both halves.
                c.source_package_ul = io.uid();
                c.source_package_uid = io.uid();
            }
            0x1102 => c.source_track_id = io.rb32() as i32,
            _ => {}
        },
        SetData::Sequence(s) => match tag {
            0x0202 => s.duration = io.rb64() as i64,
            0x0201 => s.data_definition_ul = io.uid(),
            0x1001 => s.structural_components_refs = read_strong_ref_array(io, left)?,
            _ => {}
        },
        SetData::EssenceGroup(g) => match tag {
            0x0202 => g.duration = io.rb64() as i64,
            0x0501 => g.structural_components_refs = read_strong_ref_array(io, left)?,
            _ => {}
        },
        SetData::Track(t) => match tag {
            0x4801 => t.track_id = io.rb32() as i32,
            0x4804 => {
                let mut n = [0u8; 4];
                io.read(&mut n);
                t.track_number = n;
            }
            0x4802 => t.name = Some(read_utf16be(io, size)),
            0x4b01 => t.edit_rate = (io.rb32() as i32, io.rb32() as i32),
            0x4b02 => t.origin = io.rb64() as i64,
            0x4803 => t.sequence_ref = io.uid(),
            _ => {}
        },
        SetData::Package(p) => match tag {
            0x4403 => p.tracks_refs = read_strong_ref_array(io, left)?,
            0x4401 => {
                p.package_ul = io.uid();
                p.package_uid = io.uid();
            }
            0x4701 => p.descriptor_ref = io.uid(),
            0x4402 => p.name = Some(read_utf16be(io, size)),
            _ => {}
        },
        SetData::EssenceContainerData(e) => match tag {
            0x2701 => {
                e.package_ul = io.uid();
                e.package_uid = io.uid();
            }
            0x3f06 => e.index_sid = io.rb32() as i32,
            0x3f07 => e.body_sid = io.rb32() as i32,
            _ => {}
        },
        SetData::IndexSegment(s) => match tag {
            0x3F05 => s.edit_unit_byte_count = io.rb32(),
            0x3F06 => s.index_sid = io.rb32() as i32,
            0x3F07 => s.body_sid = io.rb32() as i32,
            0x3F0A => read_index_entry_array(io, s, left)?,
            0x3F0B => {
                s.index_edit_rate = (io.rb32() as i32, io.rb32() as i32);
                if s.index_edit_rate.0 <= 0 || s.index_edit_rate.1 <= 0 {
                    return Err(Error::invalid("mxf: invalid IndexEditRate"));
                }
            }
            0x3F0C => s.index_start_position = io.rb64(),
            0x3F0D => s.index_duration = io.rb64(),
            _ => {}
        },
        SetData::Descriptor(d) => read_descriptor_tag(d, io, tag, size, uid, left)?,
        SetData::Ffv1Extradata(extradata) => {
            if uid == &FFV1_EXTRADATA {
                *extradata = Some(read_bytes(io, size, left)?);
            }
        }
    }
    Ok(())
}

fn read_bytes(io: &mut Io, size: u64, left: u64) -> Result<Vec<u8>> {
    if size > left {
        return Err(Error::invalid("mxf: tag past its set"));
    }
    let mut v = vec![0u8; size as usize];
    let got = io.read(&mut v);
    v[got..].fill(0);
    Ok(v)
}

/// mxf_read_generic_descriptor, for the fields FFmpeg's streams take.
fn read_descriptor_tag(d: &mut Descriptor, io: &mut Io, tag: u16, size: u64, uid: &Uid, left: u64) -> Result<()> {
    match tag {
        0x3F01 => d.file_descriptors_refs = read_strong_ref_array(io, left)?,
        0x3002 => d.duration = Some(io.rb64() as i64), // ContainerDuration
        0x3004 => d.essence_container_ul = io.uid(),
        0x3005 => d.codec_ul = io.uid(),
        0x3006 => d.linked_track_id = io.rb32() as i32,
        0x3201 => d.essence_codec_ul = io.uid(), // PictureEssenceCoding
        0x3203 => d.width = io.rb32() as i32,
        0x3202 => d.height = io.rb32() as i32,
        0x320C => d.frame_layout = io.r8(),
        0x320D => {
            let entry_count = io.rb32() as i32;
            let entry_size = io.rb32() as i32;
            if entry_size == 4 {
                d.video_line_map[0] = if entry_count > 0 { io.rb32() as i32 } else { 0 };
                d.video_line_map[1] = if entry_count > 1 { io.rb32() as i32 } else { 0 };
            }
        }
        0x320E => d.aspect_ratio = (io.rb32() as i32, io.rb32() as i32),
        0x3212 => d.field_dominance = io.r8(),
        0x3301 => d.component_depth = io.rb32(),
        0x3302 => d.horiz_subsampling = io.rb32(),
        0x3308 => d.vert_subsampling = io.rb32(),
        0x3D03 => d.sample_rate = (io.rb32() as i32, io.rb32() as i32),
        0x3D06 => d.essence_codec_ul = io.uid(), // SoundEssenceCompression
        0x3D07 => d.channels = io.rb32() as i32,
        0x3D01 => d.bits_per_sample = io.rb32() as i32,
        _ => {
            if uid == &SONY_MPEG4_EXTRADATA {
                d.extradata = Some(read_bytes(io, size, left)?);
            }
            if uid == &SUB_DESCRIPTOR {
                d.sub_descriptors_refs = read_strong_ref_array(io, left)?;
            }
        }
    }
    Ok(())
}

/// The empty set a reader fills, None for readers that fill the context.
fn new_set_data(reader: Reader) -> Option<(SetType, SetData)> {
    Some(match reader {
        Reader::Package(ty) => (ty, SetData::Package(Package::default())),
        Reader::Sequence => (SetType::Sequence, SetData::Sequence(Sequence::default())),
        Reader::EssenceGroup => (SetType::EssenceGroup, SetData::EssenceGroup(EssenceGroup::default())),
        Reader::SourceClip => (SetType::SourceClip, SetData::SourceClip(SourceClip::default())),
        Reader::Descriptor(ty) => (ty, SetData::Descriptor(Descriptor::default())),
        Reader::Ffv1SubDescriptor => (SetType::Ffv1SubDescriptor, SetData::Ffv1Extradata(None)),
        Reader::Track => (SetType::Track, SetData::Track(Track::default())),
        Reader::CryptoContext => (SetType::CryptoContext, SetData::CryptoContext([0; 16])),
        Reader::IndexTableSegment => (SetType::IndexTableSegment, SetData::IndexSegment(IndexSegment::default())),
        Reader::EssenceContainerData => {
            (SetType::EssenceContainerData, SetData::EssenceContainerData(EssenceContainerData::default()))
        }
        Reader::PrimerPack | Reader::PartitionPack | Reader::ContentStorage | Reader::Skip => return None,
    })
}

/// mxf_read_local_tags for a set (or the content storage), `io` after
/// the KLV's length; leaves `io` where FFmpeg does (at most the KLV's end).
pub fn read_local_tags(store: &mut Store, io: &mut Io, klv: &Klv, reader: Reader, current: Option<&Partition>) -> Result<()> {
    let klv_end = io.tell().saturating_add(klv.length);
    let mut set = new_set_data(reader).map(|(ty, data)| (ty, Set { uid: [0; 16], partition_score: partition_score(current), data }));
    while io.tell() + 4 < klv_end && !io.feof() {
        let tag = io.rb16();
        let size = u64::from(io.rb16());
        let next = io.tell() + size;
        if size == 0 {
            // Ignore empty tags (some files have an empty UMID tag).
            continue;
        }
        let uid = if tag > 0x7FFF { store.dynamic_uid(tag) } else { [0; 16] };
        let left = klv_end.saturating_sub(io.tell());
        match &mut set {
            Some((_, s)) if tag == 0x3C0A => s.uid = io.uid(),
            Some((_, s)) => read_tag(&mut s.data, io, tag, size, &uid, left)?,
            None => {
                if reader == Reader::ContentStorage {
                    match tag {
                        0x1901 => store.storage.packages_refs = read_strong_ref_array(io, left)?,
                        0x1902 => store.storage.essence_container_data_refs = read_strong_ref_array(io, left)?,
                        _ => {}
                    }
                }
            }
        }
        // Accept the 64k local set limit being exceeded (Avid), not a tag
        // extending past the end of the KLV.
        if io.tell() > klv_end {
            return Err(Error::invalid("mxf: local tag extends past the end of its set"));
        } else if io.tell() <= next {
            // Only seek forward, else this can loop for a long time.
            io.seek(next)?;
        }
    }
    if let Some((ty, set)) = set {
        store.add(ty, set)?;
    }
    Ok(())
}
