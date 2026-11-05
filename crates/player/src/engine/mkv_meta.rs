//! Matroska (`oxideav-mkv`) rich metadata bridge (`#![forbid(unsafe_code)]`).
//!
//! `oxideav_mkv::demux::MkvDemuxer` parses `BlockAdditionMapping` (`dvcC` /
//! `dvvC` Dolby Vision records, ITU-T T.35 `HDR10+`), `VideoColour` (`Colour`
//! H.273 code points, `MasteringMetadata` SMPTE ST 2086, `MaxCLL` / `MaxFALL`),
//! and `Chapters` (`Edition` / `ChapterAtom`) on its typed surface without
//! attaching them to `StreamInfo::params`. Opening Matroska/WebM through
//! [`open_mkv_enriched`] preserves full `Demuxer` behavior while surfacing
//! container HDR10, Dolby Vision, HDR10+ side-data, and chapters to the engine
//! and platform video sinks.

#![forbid(unsafe_code)]

use std::time::Duration;

use oxideav_core::{
    CodecParameters, CodecResolver, ColorPrimaries, ColorRange, ColorSignal, Demuxer,
    MatrixCoefficients, Packet, PacketMetadata, ReadSeek, Result, StreamInfo,
    TransferCharacteristics,
};
use oxideav_mkv::demux::{MasteringMetadata, MkvDemuxer, VideoColour};

use crate::cue::{Chapter, finalize_chapter_ends};

pub const OPT_CONTAINER_DOVI: &str = "container.dovi_record";
pub const OPT_CONTAINER_MDCV: &str = "container.hdr_mdcv";
pub const OPT_CONTAINER_CLLI: &str = "container.hdr_clli";

const ADD_ID_TYPE_DVCC: u64 = 0x6476_6343; // "dvcC"
const ADD_ID_TYPE_DVVC: u64 = 0x6476_7643; // "dvvC"
const ADD_ID_TYPE_ITU_T_T35: u64 = 4; // Matroska IANA ITU-T T.35 (HDR10+ ST 2094-40)

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

pub(crate) fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for pair in bytes.chunks_exact(2) {
        let hi = match pair[0] {
            b'0'..=b'9' => pair[0] - b'0',
            b'a'..=b'f' => pair[0] - b'a' + 10,
            b'A'..=b'F' => pair[0] - b'A' + 10,
            _ => return None,
        };
        let lo = match pair[1] {
            b'0'..=b'9' => pair[1] - b'0',
            b'a'..=b'f' => pair[1] - b'a' + 10,
            b'A'..=b'F' => pair[1] - b'A' + 10,
            _ => return None,
        };
        out.push((hi << 4) | lo);
    }
    Some(out)
}

/// Encodes Matroska `MasteringMetadata` into the canonical 24-byte `mdcv`
/// (SMPTE ST 2086 / ISO 14496-12) big-endian layout: G, B, R, white point in
/// `0.00002` units (`u16`), followed by `max_luminance` and `min_luminance` in
/// `0.0001 cd/m²` units (`u32`).
fn encode_mdcv(m: &MasteringMetadata) -> Option<[u8; 24]> {
    let gx = m.primary_g_chromaticity_x()?;
    let gy = m.primary_g_chromaticity_y()?;
    let bx = m.primary_b_chromaticity_x()?;
    let by = m.primary_b_chromaticity_y()?;
    let rx = m.primary_r_chromaticity_x()?;
    let ry = m.primary_r_chromaticity_y()?;
    let wx = m.white_point_chromaticity_x()?;
    let wy = m.white_point_chromaticity_y()?;
    let max_lum = m.luminance_max()?;
    let min_lum = m.luminance_min()?;

    let q_chroma = |v: f64| -> Option<u16> {
        if !v.is_finite() || !(0.0..=1.0).contains(&v) {
            return None;
        }
        Some((v * 50_000.0).round().clamp(0.0, 50_000.0) as u16)
    };
    let q_lum = |v: f64| -> Option<u32> {
        if !v.is_finite() || v < 0.0 {
            return None;
        }
        Some((v * 10_000.0).round().clamp(0.0, u32::MAX as f64) as u32)
    };

    let mut out = [0u8; 24];
    let coords = [
        q_chroma(gx)?,
        q_chroma(gy)?,
        q_chroma(bx)?,
        q_chroma(by)?,
        q_chroma(rx)?,
        q_chroma(ry)?,
        q_chroma(wx)?,
        q_chroma(wy)?,
    ];
    for (idx, c) in coords.into_iter().enumerate() {
        out[idx * 2..idx * 2 + 2].copy_from_slice(&c.to_be_bytes());
    }
    out[16..20].copy_from_slice(&q_lum(max_lum)?.to_be_bytes());
    out[20..24].copy_from_slice(&q_lum(min_lum)?.to_be_bytes());
    Some(out)
}

fn apply_mkv_colour(params: &mut CodecParameters, vc: &VideoColour) {
    let mut sig: ColorSignal = params.color_signal;
    let prim_raw = vc.primaries().to_raw();
    if sig.primaries.is_unspecified() && prim_raw != 2 && prim_raw <= u8::MAX as u64 {
        sig.primaries = ColorPrimaries(prim_raw as u8);
    }
    let trc_raw = vc.transfer_characteristics().to_raw();
    if sig.transfer.is_unspecified() && trc_raw != 2 && trc_raw <= u8::MAX as u64 {
        sig.transfer = TransferCharacteristics(trc_raw as u8);
    }
    let mat_raw = vc.matrix_coefficients().to_raw();
    if sig.matrix.is_unspecified() && mat_raw != 2 && mat_raw <= u8::MAX as u64 {
        sig.matrix = MatrixCoefficients(mat_raw as u8);
    }
    if sig.range.is_unspecified() {
        sig.range = match vc.range().to_raw() {
            1 => ColorRange::Limited,
            2 => ColorRange::Full,
            _ => ColorRange::Unspecified,
        };
    }
    params.color_signal = sig;

    if let Some(mdcv) = vc.mastering_metadata().and_then(encode_mdcv) {
        params.options.insert(OPT_CONTAINER_MDCV, hex_encode(&mdcv));
    }
    let cll = vc.max_cll().unwrap_or(0).min(u16::MAX as u64) as u16;
    let fall = vc.max_fall().unwrap_or(0).min(u16::MAX as u64) as u16;
    if cll > 0 || fall > 0 {
        let mut clli = [0u8; 4];
        clli[0..2].copy_from_slice(&cll.to_be_bytes());
        clli[2..4].copy_from_slice(&fall.to_be_bytes());
        params.options.insert(OPT_CONTAINER_CLLI, hex_encode(&clli));
    }
}

fn extract_mkv_chapters(dmx: &MkvDemuxer, duration: Option<Duration>) -> Vec<Chapter> {
    let editions = dmx.chapters();
    let Some(ed) = editions
        .iter()
        .find(|e| e.default && !e.hidden)
        .or_else(|| editions.iter().find(|e| !e.hidden))
        .or_else(|| editions.first())
    else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(ed.chapters.len());
    for atom in &ed.chapters {
        if atom.hidden || !atom.enabled {
            continue;
        }
        let title = atom.displays.first().map(|d| d.string.clone());
        let start = Duration::from_nanos(atom.time_start_ns);
        let end = atom.time_end_ns.map(Duration::from_nanos);
        out.push(Chapter {
            title,
            performer: None,
            start,
            end,
        });
    }
    finalize_chapter_ends(&mut out, duration);
    out
}

struct MkvRichDemuxer {
    inner: MkvDemuxer,
    streams: Vec<StreamInfo>,
    /// Per-stream BlockAddID carrying ITU-T T.35 (HDR10+) dynamic metadata, if any.
    t35_add_ids: Vec<Option<u64>>,
}

impl Demuxer for MkvRichDemuxer {
    fn format_name(&self) -> &str {
        self.inner.format_name()
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let mut pkt = self.inner.next_packet()?;
        let si = pkt.stream_index as usize;
        // When an AV1 track carries ITU-T T.35 HDR10+ dynamic metadata in a
        // Matroska BlockAddition (`BlockAddIDType == 4`), prepend it as an AV1
        // Metadata OBU (`obu_type = 5`, `metadata_type = 4`) so downstream
        // sinks and hardware decoders see the dynamic curve inline.
        if let Some(Some(want_id)) = self.t35_add_ids.get(si).copied() {
            if self
                .streams
                .get(si)
                .is_some_and(|s| s.params.codec_id.as_str() == "av1")
            {
                if let Some(add) = self
                    .inner
                    .block_additions()
                    .iter()
                    .find(|a| a.block_add_id() == want_id)
                {
                    let payload = add.data();
                    if !payload.is_empty() && payload.len() <= 8192 {
                        let obu = build_av1_t35_obu(payload);
                        let mut combined = Vec::with_capacity(obu.len() + pkt.data.len());
                        combined.extend_from_slice(&obu);
                        combined.extend_from_slice(&pkt.data);
                        pkt.data = combined;
                    }
                }
            }
        }
        Ok(pkt)
    }

    fn seek_to(&mut self, stream_index: u32, timestamp: i64) -> Result<i64> {
        self.inner.seek_to(stream_index, timestamp)
    }

    fn duration_micros(&self) -> Option<i64> {
        self.inner.duration_micros()
    }

    fn metadata(&self) -> &[(String, String)] {
        self.inner.metadata()
    }

    fn packet_metadata(&self) -> PacketMetadata {
        self.inner.packet_metadata()
    }
}

fn build_av1_t35_obu(t35_bytes: &[u8]) -> Vec<u8> {
    // OBU header: obu_type = 5 (OBU_METADATA) => (5 << 3) | 0x02 (obu_has_size_field = 1) = 0x2A.
    // Payload: metadata_type LEB128 = 4 (METADATA_TYPE_ITUT_T35) + t35_bytes.
    let body_len = 1 + t35_bytes.len();
    let mut out = Vec::with_capacity(2 + 4 + body_len);
    out.push(0x2A);
    let mut rem = body_len;
    loop {
        let mut byte = (rem & 0x7F) as u8;
        rem >>= 7;
        if rem != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if rem == 0 {
            break;
        }
    }
    out.push(0x04); // METADATA_TYPE_ITUT_T35
    out.extend_from_slice(t35_bytes);
    out
}

/// Opens a Matroska/WebM stream with enriched HDR10, Dolby Vision, HDR10+, and
/// chapter metadata, returning the boxed [`Demuxer`] plus extracted chapters.
pub fn open_mkv_enriched(
    input: Box<dyn ReadSeek>,
    codecs: &dyn CodecResolver,
) -> Result<(Box<dyn Demuxer>, Vec<Chapter>)> {
    let inner = oxideav_mkv::demux::open_typed(input, codecs)?;
    let mut streams = inner.streams().to_vec();
    let mut t35_add_ids = vec![None; streams.len()];

    for s in &mut streams {
        let idx = s.index;
        if let Some(vc) = inner.video_colour(idx) {
            apply_mkv_colour(&mut s.params, vc);
        }
        // A container that omits `Colour` (or leaves its fields unspecified)
        // must not make a PQ film claim SDR: fall back to the bitstream VUI.
        crate::headers::codec::fill_color_from_bitstream(&mut s.params);
        for mapping in inner.block_addition_mappings(idx) {
            match mapping.addid_type {
                ADD_ID_TYPE_DVCC | ADD_ID_TYPE_DVVC => {
                    if let Some(extra) = &mapping.extra_data {
                        if extra.len() >= 24 {
                            s.params
                                .options
                                .insert(OPT_CONTAINER_DOVI, hex_encode(&extra[..24]));
                        }
                    }
                }
                ADD_ID_TYPE_ITU_T_T35 => {
                    if let Some(slot) = t35_add_ids.get_mut(idx as usize) {
                        *slot = Some(mapping.value.unwrap_or(4));
                    }
                }
                _ => {}
            }
        }
    }

    let duration = inner
        .duration_micros()
        .filter(|&us| us > 0)
        .map(|us| Duration::from_micros(us as u64));
    let mut chapters = extract_mkv_chapters(&inner, duration);
    if chapters.is_empty() {
        for (k, v) in inner.metadata() {
            if k.eq_ignore_ascii_case("CUESHEET") {
                let mut sheet = crate::cue::parse_cue_sheet(v);
                finalize_chapter_ends(&mut sheet.chapters, duration);
                if !sheet.chapters.is_empty() {
                    chapters = sheet.chapters;
                    break;
                }
            }
        }
    }

    Ok((
        Box::new(MkvRichDemuxer {
            inner,
            streams,
            t35_add_ids,
        }),
        chapters,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{MediaType, RuntimeContext};
    use std::io::Cursor;

    fn ebml_id(id: u32) -> Vec<u8> {
        if id <= 0xFF {
            vec![id as u8]
        } else if id <= 0xFFFF {
            (id as u16).to_be_bytes().to_vec()
        } else if id <= 0xFF_FFFF {
            let b = id.to_be_bytes();
            vec![b[1], b[2], b[3]]
        } else {
            id.to_be_bytes().to_vec()
        }
    }

    fn ebml_vint_len(len: usize) -> Vec<u8> {
        if len < 0x7F {
            vec![0x80 | (len as u8)]
        } else if len < 0x3FFF {
            let v = 0x4000 | (len as u16);
            v.to_be_bytes().to_vec()
        } else {
            let v = 0x1000_0000 | (len as u32);
            v.to_be_bytes().to_vec()
        }
    }

    fn elem(id: u32, body: &[u8]) -> Vec<u8> {
        let mut out = ebml_id(id);
        out.extend_from_slice(&ebml_vint_len(body.len()));
        out.extend_from_slice(body);
        out
    }

    fn elem_uint(id: u32, val: u64) -> Vec<u8> {
        let raw = val.to_be_bytes();
        let first = raw.iter().position(|&b| b != 0).unwrap_or(7);
        elem(id, &raw[first..])
    }

    fn elem_f64(id: u32, val: f64) -> Vec<u8> {
        elem(id, &val.to_be_bytes())
    }

    #[test]
    fn mkv_dovi_block_addition_and_hdr10_colour_roundtrip_into_stream_params() {
        let ebml_hdr = elem(
            0x1A45_DFA3,
            &[
                elem_uint(0x4286, 1),
                elem_uint(0x42F7, 1),
                elem_uint(0x42F2, 4),
                elem_uint(0x42F3, 8),
                elem(0x4282, b"matroska"),
                elem_uint(0x4287, 4),
                elem_uint(0x4285, 2),
            ]
            .concat(),
        );
        let info = elem(
            0x1549_A966,
            &[elem_uint(0x2AD7B1, 1_000_000), elem_f64(0x4489, 60_000.0)].concat(),
        );
        let mastering = elem(
            0x55D0,
            &[
                elem_f64(0x55D1, 0.680),
                elem_f64(0x55D2, 0.320),
                elem_f64(0x55D3, 0.265),
                elem_f64(0x55D4, 0.690),
                elem_f64(0x55D5, 0.150),
                elem_f64(0x55D6, 0.060),
                elem_f64(0x55D7, 0.3127),
                elem_f64(0x55D8, 0.3290),
                elem_f64(0x55D9, 1000.0),
                elem_f64(0x55DA, 0.005),
            ]
            .concat(),
        );
        let colour = elem(
            0x55B0,
            &[
                elem_uint(0x55B1, 9),  // BT.2020 NCL
                elem_uint(0x55B9, 1),  // Broadcast/Limited
                elem_uint(0x55BA, 16), // SMPTE ST 2084 PQ
                elem_uint(0x55BB, 9),  // BT.2020 primaries
                elem_uint(0x55BC, 1000), // MaxCLL
                elem_uint(0x55BD, 400),  // MaxFALL
                mastering,
            ]
            .concat(),
        );
        let video = elem(
            0xE0,
            &[elem_uint(0xB0, 3840), elem_uint(0xBA, 2160), colour].concat(),
        );
        let dovi_rec = crate::headers::codec::build_dovi_record(7, 6, true, true, true, 0);
        let bam = elem(
            0x41E4,
            &[
                elem_uint(0x41F0, 2),
                elem(0x41A4, b"Dolby Vision configuration"),
                elem_uint(0x41E7, ADD_ID_TYPE_DVCC),
                elem(0x41ED, &dovi_rec),
            ]
            .concat(),
        );
        let track_entry = elem(
            0xAE,
            &[
                elem_uint(0xD7, 1),
                elem_uint(0x73C5, 1),
                elem_uint(0x83, 1),
                elem(0x86, b"V_MPEGH/ISO/HEVC"),
                video,
                bam,
            ]
            .concat(),
        );
        let tracks = elem(0x1654_AE6B, &track_entry);
        let chap_disp = elem(0x80, &elem(0x85, b"Prologue"));
        let chap_atom = elem(
            0xB6,
            &[
                elem_uint(0x73C4, 1),
                elem_uint(0x91, 0),
                elem_uint(0x92, 60_000_000_000),
                chap_disp,
            ]
            .concat(),
        );
        let chapters_el = elem(0x1043_A770, &elem(0x45B9, &chap_atom));
        let cluster = elem(
            0x1F43_B675,
            &[
                elem_uint(0xE7, 0),
                elem(0xA3, &[0x81, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x01, 0x40, 0x01]),
            ]
            .concat(),
        );
        let segment = elem(0x1853_8067, &[info, tracks, chapters_el, cluster].concat());
        let mkv_bytes = [ebml_hdr, segment].concat();

        let mut ctx = RuntimeContext::new();
        oxideav_h265::__oxideav_entry(&mut ctx);
        let (dmx, chapters) =
            open_mkv_enriched(Box::new(Cursor::new(mkv_bytes)), &ctx.codecs).expect("open enriched");
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].title.as_deref(), Some("Prologue"));
        assert_eq!(chapters[0].end, Some(Duration::from_secs(60)));

        let vstream = dmx.streams().iter().find(|s| s.params.media_type == MediaType::Video).unwrap();
        assert_eq!(vstream.params.color_signal.primaries, ColorPrimaries::BT2020);
        assert_eq!(vstream.params.color_signal.transfer, TransferCharacteristics::SMPTE_ST2084);
        let parsed_dovi = hex_decode(vstream.params.options.get(OPT_CONTAINER_DOVI).unwrap()).unwrap();
        assert_eq!(parsed_dovi.as_slice(), &dovi_rec);
        let parsed_clli = hex_decode(vstream.params.options.get(OPT_CONTAINER_CLLI).unwrap()).unwrap();
        assert_eq!(parsed_clli, vec![0x03, 0xE8, 0x01, 0x90]);
        let parsed_mdcv = hex_decode(vstream.params.options.get(OPT_CONTAINER_MDCV).unwrap()).unwrap();
        assert_eq!(parsed_mdcv.len(), 24);
    }
}
