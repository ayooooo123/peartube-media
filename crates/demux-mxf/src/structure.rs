// Ported from FFmpeg libavformat/mxfdec.c (commit 2da55bf): mxf_resolve_source_package,
// mxf_resolve_descriptor, mxf_resolve_sourceclip, mxf_add_metadata_stream,
// mxf_get_wrapping_kind, mxf_is_intra_only, is_pcm, mxf_parse_structural_metadata;
// av_get_bits_per_sample from libavcodec/utils.c.
// License: LGPL-2.1-or-later

//! The material package's tracks, resolved to their source tracks and
//! descriptors, as the streams FFmpeg creates. Stream metadata FFmpeg
//! only puts in dictionaries (UMIDs, timecodes, names, MCA labels, colour
//! and mastering properties) is not kept.

use oxideav_core::{Error, MediaType, Result};

use crate::index::{inv, rescale_q, Q};
use crate::sets::{Descriptor, EssenceGroup, Package, SetData, SetType, SourceClip, Store, Track};
use crate::types::*;
use crate::uls::*;

/// The track behind a stream (FFmpeg's st->priv_data).
#[derive(Clone, Debug, Default)]
pub struct StreamTrack {
    pub track_number: [u8; 4],
    pub edit_rate: Q,
    pub intra_only: bool,
    pub sample_count: i64,
    /// st->duration in SampleRate/EditRate units.
    pub original_duration: i64,
    pub index_sid: i32,
    pub body_sid: i32,
    pub wrapping: Wrapping,
    /// How many edit units to read at a time (PCM, clip-wrapped).
    pub edit_units_per_packet: i64,
}

/// What FFmpeg's demuxer layer parses a stream's packets for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Parsing {
    #[default]
    None,
    Headers,
    Full,
    Timestamps,
}

/// One stream as mxf_parse_structural_metadata creates it.
#[derive(Clone, Debug)]
pub struct Stream {
    pub media: MediaType,
    /// FFmpeg's codec name, "" for AV_CODEC_ID_NONE.
    pub codec: &'static str,
    pub time_base: Q,
    pub duration: Option<i64>,
    pub start_time: Option<i64>,
    pub width: i32,
    pub height: i32,
    pub sample_rate: i32,
    pub channels: i32,
    pub bits_per_coded_sample: i32,
    pub extradata: Option<Vec<u8>>,
    pub codec_tag: Option<[u8; 4]>,
    /// st->r_frame_rate where FFmpeg's demuxer sets it (SMPTE ST 422).
    pub r_frame_rate: Option<Q>,
    pub need_parsing: Parsing,
    /// None for the metadata-only streams of mxf_add_metadata_stream.
    pub track: Option<StreamTrack>,
}

impl Stream {
    fn new() -> Self {
        Self {
            media: MediaType::Data,
            codec: "",
            // avformat_new_stream's default time base.
            time_base: (1, 90000),
            duration: None,
            start_time: None,
            width: 0,
            height: 0,
            sample_rate: 0,
            channels: 0,
            bits_per_coded_sample: 0,
            extradata: None,
            codec_tag: None,
            r_frame_rate: None,
            need_parsing: Parsing::None,
            track: None,
        }
    }
}

fn track<'a>(store: &'a Store, uid: &Uid) -> Option<&'a Track> {
    match &store.resolve(uid, SetType::Track)?.data {
        SetData::Track(t) => Some(t),
        _ => None,
    }
}

fn package<'a>(store: &'a Store, uid: &Uid, ty: SetType) -> Option<&'a Package> {
    match &store.resolve(uid, ty)?.data {
        SetData::Package(p) => Some(p),
        _ => None,
    }
}

fn sequence<'a>(store: &'a Store, uid: &Uid) -> Option<&'a crate::sets::Sequence> {
    match &store.resolve(uid, SetType::Sequence)?.data {
        SetData::Sequence(s) => Some(s),
        _ => None,
    }
}

fn source_clip<'a>(store: &'a Store, uid: &Uid) -> Option<&'a SourceClip> {
    match &store.resolve(uid, SetType::SourceClip)?.data {
        SetData::SourceClip(c) => Some(c),
        _ => None,
    }
}

fn descriptor_of<'a>(store: &'a Store, uid: &Uid, ty: SetType) -> Option<&'a Descriptor> {
    match &store.resolve(uid, ty)?.data {
        SetData::Descriptor(d) => Some(d),
        _ => None,
    }
}

/// mxf_resolve_source_package: the source package with that UMID among
/// the content storage's packages.
fn resolve_source_package<'a>(store: &'a Store, ul: &Uid, uid: &Uid) -> Option<&'a Package> {
    store
        .storage
        .packages_refs
        .iter()
        .filter_map(|r| package(store, r, SetType::SourcePackage))
        .find(|p| &p.package_ul == ul && &p.package_uid == uid)
}

/// mxf_resolve_descriptor: a descriptor, or the file descriptor of a
/// multiple descriptor linked to `track_id`.
fn resolve_descriptor<'a>(store: &'a Store, uid: &Uid, track_id: i32) -> Option<&'a Descriptor> {
    if let Some(d) = descriptor_of(store, uid, SetType::Descriptor) {
        return Some(d);
    }
    let multiple = descriptor_of(store, uid, SetType::MultipleDescriptor)?;
    multiple
        .file_descriptors_refs
        .iter()
        .filter_map(|r| descriptor_of(store, r, SetType::Descriptor))
        .find(|d| d.linked_track_id == track_id)
}

/// mxf_resolve_sourceclip: a source clip, or the first clip of an
/// essence group whose package has a descriptor.
fn resolve_sourceclip<'a>(store: &'a Store, uid: &Uid) -> Option<&'a SourceClip> {
    if let Some(c) = source_clip(store, uid) {
        return Some(c);
    }
    let group: &EssenceGroup = match &store.resolve(uid, SetType::EssenceGroup)?.data {
        SetData::EssenceGroup(g) => g,
        _ => return None,
    };
    group.structural_components_refs.iter().filter_map(|r| source_clip(store, r)).find(|c| {
        resolve_source_package(store, &c.source_package_ul, &c.source_package_uid)
            .is_some_and(|p| descriptor_of(store, &p.descriptor_ref, SetType::Descriptor).is_some())
    })
}

/// mxf_get_wrapping_kind.
pub fn wrapping_kind(essence_container_ul: &Uid) -> Wrapping {
    let mut ul = get_codec_ul(PICTURE_ESSENCE_CONTAINER_ULS, essence_container_ul);
    if ul.uid[0] == 0 {
        ul = get_codec_ul(SOUND_ESSENCE_CONTAINER_ULS, essence_container_ul);
    }
    if ul.uid[0] == 0 {
        ul = get_codec_ul(DATA_ESSENCE_CONTAINER_ULS, essence_container_ul);
    }
    if ul.uid[0] == 0 || ul.wrapping_indicator_pos == 0 {
        return Wrapping::Unknown;
    }
    let mut val = essence_container_ul[ul.wrapping_indicator_pos.min(15)];
    match ul.wrapping_indicator_type {
        WrapType::RawVWrap => val %= 4,
        WrapType::RawAWrap => {
            if val == 0x03 || val == 0x04 {
                val -= 0x02;
            }
        }
        WrapType::D10D11Wrap => {
            if val == 0x02 {
                val = 0x01;
            }
        }
        WrapType::J2KWrap => {
            if val != 0x02 {
                val = 0x01;
            }
        }
        WrapType::NormalWrap => {}
    }
    match val {
        0x01 => Wrapping::Frame,
        0x02 => Wrapping::Clip,
        _ => Wrapping::Unknown,
    }
}

/// mxf_is_intra_only.
fn is_intra_only(d: &Descriptor) -> bool {
    !get_codec_ul(INTRA_ONLY_ESSENCE_CONTAINER_ULS, &d.essence_container_ul).id.is_empty()
        || !get_codec_ul(INTRA_ONLY_PICTURE_ESSENCE_CODING_ULS, &d.essence_codec_ul).id.is_empty()
}

/// is_pcm: AV_CODEC_ID_PCM_S16LE up to before AV_CODEC_ID_PCM_S24DAUD.
pub fn is_pcm(codec: &str) -> bool {
    matches!(
        codec,
        "pcm_s16le" | "pcm_s16be" | "pcm_u16le" | "pcm_u16be" | "pcm_s8" | "pcm_u8" | "pcm_mulaw" | "pcm_alaw"
            | "pcm_s32le" | "pcm_s32be" | "pcm_u32le" | "pcm_u32be" | "pcm_s24le" | "pcm_s24be" | "pcm_u24le"
            | "pcm_u24be"
    )
}

/// av_get_bits_per_sample, for the codecs MXF maps.
pub fn bits_per_sample(codec: &str) -> i32 {
    match codec {
        "pcm_s8" | "pcm_u8" | "pcm_alaw" | "pcm_mulaw" => 8,
        "pcm_s16le" | "pcm_s16be" | "pcm_u16le" | "pcm_u16be" => 16,
        "pcm_s24le" | "pcm_s24be" | "pcm_u24le" | "pcm_u24be" | "pcm_s24daud" => 24,
        "pcm_s32le" | "pcm_s32be" | "pcm_u32le" | "pcm_u32be" | "pcm_f32le" | "pcm_f32be" => 32,
        "pcm_f64le" | "pcm_f64be" => 64,
        _ => 0,
    }
}

/// mxf_add_metadata_stream: a data stream for a material track without
/// essence, where its sequence has a source clip.
fn metadata_stream(store: &Store, seq: &crate::sets::Sequence) -> Option<Stream> {
    seq.structural_components_refs.iter().find_map(|r| resolve_sourceclip(store, r))?;
    Some(Stream::new())
}

/// mxf_parse_structural_metadata: the streams of the first material
/// package, in its track order.
pub fn parse(store: &Store, op: Op) -> Result<Vec<Stream>> {
    let material_package = store
        .storage
        .packages_refs
        .iter()
        .find_map(|r| package(store, r, SetType::MaterialPackage))
        .ok_or_else(|| Error::invalid("mxf: no material package found"))?;
    let mut streams: Vec<Stream> = Vec::new();
    for track_ref in &material_package.tracks_refs {
        let Some(material_track) = track(store, track_ref) else { continue };
        let Some(material_seq) = sequence(store, &material_track.sequence_ref) else { continue };

        // The first source clip whose source package resolves: the source
        // track it names, or no essence where that package lacks it.
        let mut found: Option<(&SourceClip, &Package, Track)> = None;
        for r in &material_seq.structural_components_refs {
            let Some(c) = resolve_sourceclip(store, r) else { continue };
            let Some(p) = resolve_source_package(store, &c.source_package_ul, &c.source_package_uid) else { continue };
            let mut source_track: Option<Track> = None;
            for t in &p.tracks_refs {
                let Some(t) = track(store, t) else {
                    return Err(Error::invalid("mxf: could not resolve a source package track"));
                };
                if t.track_id == c.source_track_id {
                    source_track = Some(t.clone());
                    break;
                }
            }
            if let Some(st) = source_track {
                found = Some((c, p, st));
            }
            break;
        }
        let Some((component, source_package, source_track)) = found else {
            if let Some(s) = metadata_stream(store, material_seq) {
                streams.push(s);
            }
            continue;
        };
        // The essence container data linked to the source package.
        let (mut body_sid, mut index_sid) = (0, 0);
        for r in &store.storage.essence_container_data_refs {
            let Some(SetData::EssenceContainerData(e)) = store.resolve(r, SetType::EssenceContainerData).map(|s| &s.data) else {
                continue;
            };
            if e.package_ul == component.source_package_ul && e.package_uid == component.source_package_uid {
                body_sid = e.body_sid;
                index_sid = e.index_sid;
                break;
            }
        }
        let Some(source_seq) = sequence(store, &source_track.sequence_ref) else {
            return Err(Error::invalid("mxf: could not resolve a source track's sequence"));
        };
        // Two files sharing a SourcePackageID (0001GL00.MXF.A1 and
        // 0001GL.MXF.V1): their data definitions tell them apart.
        if material_seq.data_definition_ul != source_seq.data_definition_ul {
            continue;
        }
        let mut st = Stream::new();
        let descriptor = resolve_descriptor(store, &source_package.descriptor_ref, source_track.track_id);
        // A SourceClip from an EssenceGroup may be a single frame repeated:
        // the descriptor's duration is then the real one.
        let original_duration = match descriptor.and_then(|d| d.duration) {
            Some(d) => d.min(component.duration),
            None => component.duration,
        };
        st.duration = (original_duration != -1).then_some(original_duration);
        st.start_time = Some(component.start_position);
        let mut edit_rate = material_track.edit_rate;
        if edit_rate.0 <= 0 || edit_rate.1 <= 0 {
            edit_rate = (25, 1);
        }
        st.time_base = pts_info(edit_rate.1, edit_rate.0);
        let mut t = StreamTrack {
            track_number: source_track.track_number,
            edit_rate,
            original_duration,
            index_sid,
            body_sid,
            edit_units_per_packet: 1,
            ..Default::default()
        };
        st.media = get_codec_ul(DATA_DEFINITION_ULS, &source_seq.data_definition_ul).id;
        let Some(descriptor) = descriptor else {
            st.track = Some(t);
            streams.push(st);
            continue;
        };
        let mut essence_container_ul = descriptor.essence_container_ul;
        t.wrapping = if op == Op::OpAtom { Wrapping::Clip } else { wrapping_kind(&essence_container_ul) };
        // Replacing the key with mxf_encrypted_essence_container is not
        // allowed (s429-6); the crypto context may still name it.
        if essence_container_ul == ENCRYPTED_ESSENCE_CONTAINER {
            if let Some(SetData::CryptoContext(ul)) = store.group(SetType::CryptoContext).first().map(|s| &s.data) {
                essence_container_ul = *ul;
            }
        }
        st.codec = get_codec_ul(CODEC_ULS, &descriptor.essence_codec_ul).id;
        if st.codec.is_empty() {
            st.codec = get_codec_ul(CODEC_ULS, &descriptor.codec_ul).id;
        }
        match st.media {
            MediaType::Video => {
                t.intra_only = is_intra_only(descriptor);
                let container = get_codec_ul(PICTURE_ESSENCE_CONTAINER_ULS, &essence_container_ul);
                if st.codec.is_empty() {
                    st.codec = container.id;
                }
                st.width = descriptor.width;
                st.height = descriptor.height; // field height, not frame height
                // SegmentedFrame (3) and SeparateFields (1): frame height.
                if matches!(descriptor.frame_layout, 1 | 3) {
                    st.height = st.height.wrapping_mul(2);
                }
                if essence_container_ul[..14] == ST_422_ESSENCE_CONTAINER_UL {
                    st.r_frame_rate = match essence_container_ul[14] {
                        2 | 3 | 4 | 6 => Some(edit_rate),
                        5 => Some((edit_rate.0.wrapping_mul(2), edit_rate.1)),
                        _ => None,
                    };
                }
                if st.codec == "prores" {
                    st.codec_tag = match descriptor.essence_codec_ul[14] {
                        1 => Some(*b"apco"),
                        2 => Some(*b"apcs"),
                        3 => Some(*b"apcn"),
                        4 => Some(*b"apch"),
                        5 => Some(*b"ap4h"),
                        6 => Some(*b"ap4x"),
                        _ => None,
                    };
                }
                st.need_parsing = Parsing::Headers;
            }
            MediaType::Audio => {
                let container = get_codec_ul(SOUND_ESSENCE_CONTAINER_ULS, &essence_container_ul);
                // Only overwrite an unset codec or A-law, the default per RP 224.
                if st.codec.is_empty() || (st.codec == "pcm_alaw" && !container.id.is_empty()) {
                    st.codec = container.id;
                }
                st.channels = descriptor.channels;
                if descriptor.sample_rate.1 > 0 {
                    st.sample_rate = descriptor.sample_rate.0 / descriptor.sample_rate.1;
                    st.time_base = pts_info(descriptor.sample_rate.1, descriptor.sample_rate.0);
                } else {
                    st.time_base = (1, 48000);
                }
                if let Some(d) = st.duration {
                    st.duration = Some(rescale_q(d, inv(edit_rate), st.time_base));
                }
                match st.codec {
                    "pcm_s16le" if descriptor.bits_per_sample > 16 && descriptor.bits_per_sample <= 24 => st.codec = "pcm_s24le",
                    "pcm_s16le" if descriptor.bits_per_sample == 32 => st.codec = "pcm_s32le",
                    "pcm_s16be" if descriptor.bits_per_sample > 16 && descriptor.bits_per_sample <= 24 => st.codec = "pcm_s24be",
                    "pcm_s16be" if descriptor.bits_per_sample == 32 => st.codec = "pcm_s32be",
                    "mp2" | "aac" => st.need_parsing = Parsing::Full,
                    _ => {}
                }
                st.bits_per_coded_sample = bits_per_sample(st.codec);
                if descriptor.channels <= 0 || descriptor.channels >= SANE_NB_CHANNELS {
                    return Err(Error::invalid("mxf: invalid number of channels"));
                }
            }
            MediaType::Data => {
                let container = get_codec_ul(DATA_ESSENCE_CONTAINER_ULS, &essence_container_ul);
                if st.codec.is_empty() {
                    st.codec = container.id;
                }
                // avcodec_get_type: the subtitle codecs MXF maps.
                if st.codec == "ttml" {
                    st.media = MediaType::Subtitle;
                }
            }
            _ => {}
        }
        st.extradata = descriptor.extradata.clone().or_else(|| ffv1_extradata(store, descriptor));
        if st.extradata.is_none() && st.codec == "h264" && t.intra_only {
            let coded_width = get_codec_ul(INTRA_ONLY_PICTURE_CODED_WIDTH, &descriptor.essence_codec_ul).id;
            if coded_width != 0 {
                st.width = coded_width as i32;
            }
        }
        if st.media != MediaType::Data && t.wrapping != Wrapping::Frame {
            st.need_parsing = Parsing::Timestamps;
        }
        st.track = Some(t);
        streams.push(st);
    }
    // Streams sharing a BodySID share a wrapping where one of them is unknown.
    for i in 0..streams.len() {
        for j in i + 1..streams.len() {
            let (a, b) = streams.split_at_mut(j);
            let (Some(t1), Some(t2)) = (a[i].track.as_mut(), b[0].track.as_mut()) else { continue };
            if t1.body_sid != 0 && t1.body_sid == t2.body_sid && t1.wrapping != t2.wrapping {
                if t1.wrapping == Wrapping::Unknown {
                    t1.wrapping = t2.wrapping;
                } else if t2.wrapping == Wrapping::Unknown {
                    t2.wrapping = t1.wrapping;
                }
            }
        }
    }
    Ok(streams)
}

/// parse_ffv1_sub_descriptor: the FFV1 initialization metadata of the
/// descriptor's sub-descriptors.
fn ffv1_extradata(store: &Store, d: &Descriptor) -> Option<Vec<u8>> {
    d.sub_descriptors_refs.iter().find_map(|r| match &store.resolve(r, SetType::Ffv1SubDescriptor)?.data {
        SetData::Ffv1Extradata(e) => e.clone(),
        _ => None,
    })
}

/// avpriv_set_pts_info's time base: num/den reduced.
fn pts_info(num: i32, den: i32) -> Q {
    fn gcd(a: i64, b: i64) -> i64 {
        if b == 0 { a.abs() } else { gcd(b, a % b) }
    }
    let g = gcd(i64::from(num), i64::from(den));
    if g <= 1 { (num, den) } else { ((i64::from(num) / g) as i32, (i64::from(den) / g) as i32) }
}
