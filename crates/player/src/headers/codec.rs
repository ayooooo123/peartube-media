//! Codec-header parsing for direct playback: H.264/HEVC parameter sets and VUI,
//! AV1 sequence headers and av1C records, HDR10 metadata (SEI and AV1 metadata
//! OBUs), Dolby Vision detection, and E-AC-3 dec3 configuration.

use oxideav_av1::{parse_leb128, parse_sequence_header, Av1CodecConfig, SequenceHeader};
use oxideav_core::CodecParameters;

use super::types::*;

/// How the stream's video packets are framed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Framing {
    LengthPrefixed(u8),
    AnnexB,
}

/// From the extradata: an avcC/hvcC record (first byte 1) -> LengthPrefixed(lengthSizeMinusOne+1); empty or Annex B extradata -> AnnexB.
pub(crate) fn video_framing(params: &CodecParameters) -> Framing {
    let extra = &params.extradata;
    match params.codec_id.as_str() {
        "h264" => {
            if extra.len() >= 5 && extra[0] == 1 {
                Framing::LengthPrefixed((extra[4] & 3) + 1)
            } else {
                Framing::AnnexB
            }
        }
        "h265" | "hevc" => {
            if extra.len() >= 22 && extra[0] == 1 {
                Framing::LengthPrefixed((extra[21] & 3) + 1)
            } else {
                Framing::AnnexB
            }
        }
        _ => {
            if extra.len() >= 5 && extra[0] == 1 {
                Framing::LengthPrefixed((extra[4] & 3) + 1)
            } else {
                Framing::AnnexB
            }
        }
    }
}

/// None unless codec_id is "h264", "h265"/"hevc" or "av1" and its parameter sets (an SPS and PPS,
/// or an AV1 sequence header) are found in the extradata (avcC/hvcC/av1C record or Annex B) or,
/// failing that, in `first_keyframe` (framed per `video_framing`; AV1 as low-overhead OBUs).
pub(crate) fn video_config(params: &CodecParameters, first_keyframe: &[u8]) -> Option<VideoConfig> {
    match params.codec_id.as_str() {
        "h264" => h264_config(params, first_keyframe),
        "h265" | "hevc" => hevc_config(params, first_keyframe),
        "av1" => av1_config(params, first_keyframe),
        _ => None,
    }
}

/// None unless codec_id is "eac3" and the headers parse.
pub(crate) fn audio_config(params: &CodecParameters, first_packet: &[u8]) -> Option<AudioConfig> {
    match params.codec_id.as_str() {
        "eac3" => eac3_config(params, first_packet),
        _ => None,
    }
}

/// E-AC-3: sum over the packet's syncframes of independent substream 0 (strmtyp 0 or 2, substreamid 0) of numblks*256;
/// None if the packet does not start with an E-AC-3 syncframe.
pub(crate) fn eac3_frame_samples(data: &[u8]) -> Option<u32> {
    if data.len() < 6 || data[0] != 0x0B || data[1] != 0x77 {
        return None;
    }
    let bsid = data[5] >> 3;
    if bsid <= 10 || bsid > 16 {
        return None;
    }
    let mut pos = 0;
    let mut total_samples = 0u32;
    while pos + 6 <= data.len() {
        if data[pos] != 0x0B || data[pos + 1] != 0x77 {
            break;
        }
        let strmtyp = (data[pos + 2] >> 6) & 3;
        if strmtyp == 3 {
            break;
        }
        let substreamid = (data[pos + 2] >> 3) & 7;
        let frmsiz = ((u16::from(data[pos + 2] & 7) << 8) | u16::from(data[pos + 3])) as usize;
        let frame_size = (frmsiz + 1) * 2;
        if frame_size < 6 || pos + frame_size > data.len() {
            break;
        }
        let fscod = (data[pos + 4] >> 6) & 3;
        let numblks = if fscod == 3 {
            6
        } else {
            let numblkscod = ((data[pos + 4] >> 4) & 3) as usize;
            EAC3_BLOCKS[numblkscod] as u32
        };
        if (strmtyp == 0 || strmtyp == 2) && substreamid == 0 {
            total_samples += numblks * 256;
        }
        pos += frame_size;
    }
    if pos == 0 {
        None
    } else {
        Some(total_samples)
    }
}

// ---------------------------------------------------------------------------
// Helpers: NAL / RBSP / SEI
// ---------------------------------------------------------------------------

fn extract_nals<'a>(data: &'a [u8], framing: Framing) -> Vec<&'a [u8]> {
    let mut nals = Vec::new();
    match framing {
        Framing::LengthPrefixed(size) => {
            let s = size as usize;
            if s > 0 {
                let mut pos = 0;
                while pos + s <= data.len() {
                    let mut len = 0usize;
                    for &b in &data[pos..pos + s] {
                        len = (len << 8) | (b as usize);
                    }
                    pos += s;
                    if pos + len > data.len() {
                        break;
                    }
                    if len > 0 {
                        nals.push(&data[pos..pos + len]);
                    }
                    pos += len;
                }
            }
        }
        Framing::AnnexB => {
            let mut i = 0;
            let mut starts = Vec::new();
            while i + 2 < data.len() {
                if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                    starts.push(i);
                    i += 3;
                } else {
                    i += 1;
                }
            }
            for (idx, &start) in starts.iter().enumerate() {
                let nalu_start = start + 3;
                let next_start = starts.get(idx + 1).copied().unwrap_or(data.len());
                let mut nalu_end = next_start;
                while nalu_end > nalu_start && data[nalu_end - 1] == 0 {
                    nalu_end -= 1;
                }
                if nalu_end > nalu_start {
                    nals.push(&data[nalu_start..nalu_end]);
                }
            }
        }
    }
    nals
}

fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn parse_sei_messages(rbsp: &[u8], mut callback: impl FnMut(u32, &[u8])) {
    let mut pos = 0;
    while pos < rbsp.len() && rbsp[pos] != 0x80 {
        let mut payload_type = 0u32;
        while pos < rbsp.len() && rbsp[pos] == 0xFF {
            payload_type = payload_type.saturating_add(255);
            pos += 1;
        }
        if pos >= rbsp.len() {
            break;
        }
        payload_type = payload_type.saturating_add(rbsp[pos] as u32);
        pos += 1;

        let mut payload_size = 0usize;
        while pos < rbsp.len() && rbsp[pos] == 0xFF {
            payload_size = payload_size.saturating_add(255);
            pos += 1;
        }
        if pos >= rbsp.len() {
            break;
        }
        payload_size = payload_size.saturating_add(rbsp[pos] as usize);
        pos += 1;

        if pos + payload_size > rbsp.len() {
            break;
        }
        callback(payload_type, &rbsp[pos..pos + payload_size]);
        pos += payload_size;
    }
}

fn aspect_ratio_from_idc(idc: u8, sar_w: Option<u16>, sar_h: Option<u16>) -> Option<(u32, u32)> {
    match idc {
        1 => None, // 1:1 square
        2 => Some((12, 11)),
        3 => Some((10, 11)),
        4 => Some((16, 11)),
        5 => Some((40, 33)),
        6 => Some((24, 11)),
        7 => Some((20, 11)),
        8 => Some((32, 11)),
        9 => Some((80, 33)),
        10 => Some((18, 11)),
        11 => Some((15, 11)),
        12 => Some((64, 33)),
        13 => Some((160, 99)),
        14 => Some((4, 3)),
        15 => Some((3, 2)),
        16 => Some((2, 1)),
        255 => match (sar_w, sar_h) {
            (Some(w), Some(h)) if w > 0 && h > 0 && w != h => Some((w as u32, h as u32)),
            _ => None,
        },
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// H.264
// ---------------------------------------------------------------------------

fn max_dpb_mbs_for_h264_level(level_idc: u8, constraint_set3_flag: bool) -> u32 {
    match level_idc {
        9 => 396,
        10 => 396,
        11 => {
            if constraint_set3_flag {
                396
            } else {
                900
            }
        }
        12 | 13 | 20 => 2_376,
        21 => 4_752,
        22 | 30 => 8_100,
        31 => 18_000,
        32 => 20_480,
        40 | 41 => 32_768,
        42 => 34_816,
        50 => 110_400,
        51 | 52 => 184_320,
        60 | 61 | 62 => 696_320,
        _ => 396,
    }
}

fn h264_config(params: &CodecParameters, first_keyframe: &[u8]) -> Option<VideoConfig> {
    let mut sps_list: Vec<Vec<u8>> = Vec::new();
    let mut pps_list: Vec<Vec<u8>> = Vec::new();
    let mut sei_list: Vec<Vec<u8>> = Vec::new();
    let mut nal_length_size = 4u8;
    let mut record = Vec::new();

    let extra = &params.extradata;
    let is_avcc = extra.len() >= 7 && extra[0] == 1;

    if is_avcc {
        nal_length_size = (extra[4] & 3) + 1;
        record = extra.clone();
        let num_sps = (extra[5] & 0x1F) as usize;
        let mut pos = 6;
        for _ in 0..num_sps {
            if pos + 2 > extra.len() {
                break;
            }
            let len = u16::from_be_bytes([extra[pos], extra[pos + 1]]) as usize;
            pos += 2;
            if pos + len > extra.len() {
                break;
            }
            sps_list.push(extra[pos..pos + len].to_vec());
            pos += len;
        }
        if pos < extra.len() {
            let num_pps = extra[pos] as usize;
            pos += 1;
            for _ in 0..num_pps {
                if pos + 2 > extra.len() {
                    break;
                }
                let len = u16::from_be_bytes([extra[pos], extra[pos + 1]]) as usize;
                pos += 2;
                if pos + len > extra.len() {
                    break;
                }
                pps_list.push(extra[pos..pos + len].to_vec());
                pos += len;
            }
        }
    } else {
        for nal in extract_nals(extra, Framing::AnnexB) {
            if nal.is_empty() {
                continue;
            }
            match nal[0] & 0x1F {
                7 => sps_list.push(nal.to_vec()),
                8 => pps_list.push(nal.to_vec()),
                6 => sei_list.push(nal.to_vec()),
                _ => {}
            }
        }
    }

    if sps_list.is_empty() || pps_list.is_empty() {
        for nal in extract_nals(first_keyframe, video_framing(params)) {
            if nal.is_empty() {
                continue;
            }
            match nal[0] & 0x1F {
                7 => {
                    if !sps_list.iter().any(|s| s == nal) {
                        sps_list.push(nal.to_vec());
                    }
                }
                8 => {
                    if !pps_list.iter().any(|p| p == nal) {
                        pps_list.push(nal.to_vec());
                    }
                }
                6 => sei_list.push(nal.to_vec()),
                _ => {}
            }
        }
    }

    // Also look for SEI NAL units in first_keyframe even if avcC
    if is_avcc {
        for nal in extract_nals(first_keyframe, video_framing(params)) {
            if !nal.is_empty() && (nal[0] & 0x1F) == 6 {
                sei_list.push(nal.to_vec());
            }
        }
    }

    if sps_list.is_empty() || pps_list.is_empty() {
        return None;
    }

    let active_sps_bytes = &sps_list[0];
    let rbsp = unescape_rbsp(&active_sps_bytes[1..]);
    let sps = oxideav_h264::sps::Sps::parse(&rbsp).ok()?;

    if !is_avcc {
        nal_length_size = 4;
        let mut avcc = Vec::new();
        avcc.push(1); // configurationVersion = 1
        avcc.push(sps.profile_idc);
        avcc.push(sps.constraint_set_flags);
        avcc.push(sps.level_idc);
        avcc.push(0xFC | 3); // 4-byte length prefixes
        avcc.push(0xE0 | (sps_list.len() as u8 & 0x1F));
        for s in &sps_list {
            avcc.extend_from_slice(&(s.len() as u16).to_be_bytes());
            avcc.extend_from_slice(s);
        }
        avcc.push(pps_list.len() as u8);
        for p in &pps_list {
            avcc.extend_from_slice(&(p.len() as u16).to_be_bytes());
            avcc.extend_from_slice(p);
        }
        if matches!(sps.profile_idc, 100 | 110 | 122 | 144 | 244) {
            avcc.push(0xFC | (sps.chroma_format_idc as u8 & 0x03));
            avcc.push(0xF8 | (sps.bit_depth_luma_minus8 as u8 & 0x07));
            avcc.push(0xF8 | (sps.bit_depth_chroma_minus8 as u8 & 0x07));
            avcc.push(0); // numOfSequenceParameterSetExt
        }
        record = avcc;
    }

    // Cropping / dimensions
    let pic_width_in_mbs = sps.pic_width_in_mbs_minus1 + 1;
    let pic_width_in_samples = pic_width_in_mbs * 16;
    let pic_height_in_map_units = sps.pic_height_in_map_units_minus1 + 1;
    let frame_height_in_mbs = (2 - sps.frame_mbs_only_flag as u32) * pic_height_in_map_units;
    let pic_height_in_samples = frame_height_in_mbs * 16;
    let (crop_unit_x, crop_unit_y) = match (sps.chroma_format_idc, sps.separate_colour_plane_flag) {
        (1, false) => (2, 2 * (2 - sps.frame_mbs_only_flag as u32)),
        (2, false) => (2, 1 * (2 - sps.frame_mbs_only_flag as u32)),
        _ => (1, 2 - sps.frame_mbs_only_flag as u32),
    };
    let (crop_l, crop_r, crop_t, crop_b) = sps
        .frame_cropping
        .as_ref()
        .map_or((0, 0, 0, 0), |c| (c.left, c.right, c.top, c.bottom));
    let width = pic_width_in_samples.saturating_sub((crop_l + crop_r) * crop_unit_x);
    let height = pic_height_in_samples.saturating_sub((crop_t + crop_b) * crop_unit_y);
    let width = if width > 0 { width } else { params.width.unwrap_or(0) };
    let height = if height > 0 { height } else { params.height.unwrap_or(0) };

    let codec_string = format!(
        "avc1.{:02x}{:02x}{:02x}",
        sps.profile_idc, sps.constraint_set_flags, sps.level_idc
    );

    let (range, colour) = if let Some(vui) = &sps.vui {
        let (range, colour) = if let Some(vst) = &vui.video_signal_type {
            let col = vst.colour_description.as_ref().map(|cd| Colour {
                primaries: cd.colour_primaries as u16,
                transfer: cd.transfer_characteristics as u16,
                matrix: cd.matrix_coefficients as u16,
                full_range: vst.video_full_range_flag,
            });
            let range = match vst.colour_description.as_ref().map(|cd| cd.transfer_characteristics) {
                Some(16) => VideoRange::Pq,
                Some(18) => VideoRange::Hlg,
                _ => VideoRange::Sdr,
            };
            (range, col)
        } else {
            (VideoRange::Sdr, None)
        };
        (range, colour)
    } else {
        (VideoRange::Sdr, None)
    };

    let pixel_aspect = sps.vui.as_ref().and_then(|v| {
        v.aspect_ratio.as_ref().and_then(|ar| {
            let (w, h) = ar.extended_sar.unzip();
            aspect_ratio_from_idc(ar.aspect_ratio_idc, w, h)
        })
    });

    let frame_rate = sps
        .vui
        .as_ref()
        .and_then(|v| v.timing_info.as_ref())
        .and_then(|ti| {
            if ti.num_units_in_tick > 0 {
                Some(ti.time_scale as f64 / (2.0 * ti.num_units_in_tick as f64))
            } else {
                None
            }
        })
        .or_else(|| params.frame_rate.map(|r| r.num as f64 / r.den as f64));

    let reorder_depth = if let Some(br) = sps.vui.as_ref().and_then(|v| v.bitstream_restriction.as_ref()) {
        br.max_num_reorder_frames
    } else if sps.profile_idc == 66 {
        0
    } else {
        let constraint_set3_flag = (sps.constraint_set_flags & 0b0000_1000) != 0;
        let max_dpb_mbs = max_dpb_mbs_for_h264_level(sps.level_idc, constraint_set3_flag);
        let pic_size_mbs = (pic_width_in_mbs * frame_height_in_mbs).max(1);
        (max_dpb_mbs / pic_size_mbs).min(16)
    };

    let interlaced = !sps.frame_mbs_only_flag;

    // SEI extraction
    let mut mastering_display = None;
    let mut content_light = None;
    for nal in &sei_list {
        if nal.len() > 1 {
            let rbsp = unescape_rbsp(&nal[1..]);
            parse_sei_messages(&rbsp, |ptype, payload| {
                if ptype == 137 && payload.len() == 24 && mastering_display.is_none() {
                    mastering_display = payload.try_into().ok();
                } else if ptype == 144 && payload.len() == 4 && content_light.is_none() {
                    content_light = payload.try_into().ok();
                }
            });
        }
    }

    Some(VideoConfig {
        codec: VideoCodec::H264,
        width,
        height,
        record,
        nal_length_size,
        codec_string,
        range,
        colour,
        mastering_display,
        content_light,
        pixel_aspect,
        frame_rate,
        reorder_depth,
        interlaced,
        dovi: None,
    })
}

// ---------------------------------------------------------------------------
// HEVC
// ---------------------------------------------------------------------------

fn hevc_config(params: &CodecParameters, first_keyframe: &[u8]) -> Option<VideoConfig> {
    let mut vps_list: Vec<Vec<u8>> = Vec::new();
    let mut sps_list: Vec<Vec<u8>> = Vec::new();
    let mut pps_list: Vec<Vec<u8>> = Vec::new();
    let mut sei_list: Vec<Vec<u8>> = Vec::new();
    let mut nal_length_size = 4u8;
    let mut header_22 = Vec::new();

    let extra = &params.extradata;
    let is_hvcc = extra.len() >= 23 && extra[0] == 1;

    if is_hvcc {
        nal_length_size = (extra[21] & 3) + 1;
        header_22 = extra[0..22].to_vec();
        let num_arrays = extra[22] as usize;
        let mut pos = 23;
        for _ in 0..num_arrays {
            if pos + 3 > extra.len() {
                break;
            }
            let nal_type = extra[pos] & 0x3F;
            let num_nalus = u16::from_be_bytes([extra[pos + 1], extra[pos + 2]]) as usize;
            pos += 3;
            for _ in 0..num_nalus {
                if pos + 2 > extra.len() {
                    break;
                }
                let len = u16::from_be_bytes([extra[pos], extra[pos + 1]]) as usize;
                pos += 2;
                if pos + len > extra.len() {
                    break;
                }
                let nalu = extra[pos..pos + len].to_vec();
                match nal_type {
                    32 => vps_list.push(nalu),
                    33 => sps_list.push(nalu),
                    34 => pps_list.push(nalu),
                    39 => sei_list.push(nalu),
                    _ => {}
                }
                pos += len;
            }
        }
    } else {
        for nal in extract_nals(extra, Framing::AnnexB) {
            if nal.len() >= 2 {
                let nal_type = (nal[0] >> 1) & 0x3F;
                match nal_type {
                    32 => vps_list.push(nal.to_vec()),
                    33 => sps_list.push(nal.to_vec()),
                    34 => pps_list.push(nal.to_vec()),
                    39 => sei_list.push(nal.to_vec()),
                    _ => {}
                }
            }
        }
    }

    if sps_list.is_empty() || pps_list.is_empty() {
        for nal in extract_nals(first_keyframe, video_framing(params)) {
            if nal.len() >= 2 {
                let nal_type = (nal[0] >> 1) & 0x3F;
                match nal_type {
                    32 => {
                        if !vps_list.iter().any(|v| v == nal) {
                            vps_list.push(nal.to_vec());
                        }
                    }
                    33 => {
                        if !sps_list.iter().any(|s| s == nal) {
                            sps_list.push(nal.to_vec());
                        }
                    }
                    34 => {
                        if !pps_list.iter().any(|p| p == nal) {
                            pps_list.push(nal.to_vec());
                        }
                    }
                    39 => sei_list.push(nal.to_vec()),
                    _ => {}
                }
            }
        }
    }

    // Also look for SEI in first_keyframe
    for nal in extract_nals(first_keyframe, video_framing(params)) {
        if nal.len() >= 2 && ((nal[0] >> 1) & 0x3F) == 39 {
            sei_list.push(nal.to_vec());
        }
    }

    if sps_list.is_empty() || pps_list.is_empty() {
        return None;
    }

    let active_sps_bytes = &sps_list[0];
    let rbsp = unescape_rbsp(&active_sps_bytes[2..]);
    let sps = oxideav_h265::sps::SeqParameterSet::parse(&rbsp).ok()?;

    if !is_hvcc {
        nal_length_size = 4;
        let mut h = Vec::with_capacity(22);
        h.push(1); // configurationVersion = 1
        h.push(
            (sps.ptl.general_profile_space << 6)
                | ((sps.ptl.general_tier_flag as u8) << 5)
                | (sps.ptl.general_profile_idc & 0x1F),
        );
        h.extend_from_slice(&sps.ptl.general_profile_compatibility_flags.to_be_bytes());
        h.extend_from_slice(&sps.ptl.general_constraint_indicator_flags.to_be_bytes()[2..8]);
        h.push(sps.ptl.general_level_idc);
        h.extend_from_slice(&[0xF0, 0x00]); // min_spatial_segmentation = 0xF000
        h.push(0xFC); // parallelism = 0xFC
        h.push(0xFC | (sps.chroma_format_idc & 3));
        h.push(0xF8 | (sps.bit_depth_luma_minus8 & 7));
        h.push(0xF8 | (sps.bit_depth_chroma_minus8 & 7));
        h.extend_from_slice(&[0x00, 0x00]); // avgFrameRate = 0
        h.push(
            ((sps.max_sub_layers_minus1 + 1) << 3)
                | ((sps.temporal_id_nesting_flag as u8) << 2)
                | 3,
        );
        header_22 = h;
    }

    // Build canonical hvcC: only VPS (32), SPS (33), PPS (34) arrays with array_completeness = 1
    let mut num_arrays = 0u8;
    if !vps_list.is_empty() {
        num_arrays += 1;
    }
    if !sps_list.is_empty() {
        num_arrays += 1;
    }
    if !pps_list.is_empty() {
        num_arrays += 1;
    }

    let mut record = Vec::with_capacity(header_22.len() + 1 + 128);
    record.extend_from_slice(&header_22);
    record.push(num_arrays);

    let write_array = |rec: &mut Vec<u8>, nal_type: u8, nals: &[Vec<u8>]| {
        if !nals.is_empty() {
            rec.push(0x80 | nal_type);
            rec.extend_from_slice(&(nals.len() as u16).to_be_bytes());
            for nal in nals {
                rec.extend_from_slice(&(nal.len() as u16).to_be_bytes());
                rec.extend_from_slice(nal);
            }
        }
    };
    write_array(&mut record, 32, &vps_list);
    write_array(&mut record, 33, &sps_list);
    write_array(&mut record, 34, &pps_list);

    // Cropping / dimensions
    let (sub_w, sub_h) = match (sps.chroma_format_idc, sps.separate_colour_plane_flag) {
        (1, false) => (2, 2),
        (2, false) => (2, 1),
        _ => (1, 1),
    };
    let (crop_l, crop_r, crop_t, crop_b) = if sps.conformance_window_flag {
        (
            sps.conformance_window.left_offset,
            sps.conformance_window.right_offset,
            sps.conformance_window.top_offset,
            sps.conformance_window.bottom_offset,
        )
    } else {
        (0, 0, 0, 0)
    };
    let width = sps
        .pic_width_in_luma_samples
        .saturating_sub((crop_l + crop_r) * sub_w);
    let height = sps
        .pic_height_in_luma_samples
        .saturating_sub((crop_t + crop_b) * sub_h);
    let width = if width > 0 { width } else { params.width.unwrap_or(0) };
    let height = if height > 0 { height } else { params.height.unwrap_or(0) };

    // RFC 6381 codec string
    let space_prefix = match sps.ptl.general_profile_space {
        1 => "A",
        2 => "B",
        3 => "C",
        _ => "",
    };
    let compat_hex = format!("{:x}", sps.ptl.general_profile_compatibility_flags.reverse_bits());
    let tier = if sps.ptl.general_tier_flag { "H" } else { "L" };
    let level = sps.ptl.general_level_idc;
    let constraint_bytes = &sps.ptl.general_constraint_indicator_flags.to_be_bytes()[2..8];
    let mut trimmed_constraints = constraint_bytes;
    while let Some((&0, rest)) = trimmed_constraints.split_last() {
        trimmed_constraints = rest;
    }
    let mut codec_string = format!(
        "hvc1.{}{}.{}.{}{}",
        space_prefix, sps.ptl.general_profile_idc, compat_hex, tier, level
    );
    for &b in trimmed_constraints {
        codec_string.push_str(&format!(".{:X}", b));
    }

    let (range, colour) = if let Some(vui) = &sps.vui_parameters {
        let (range, colour) = if let Some(vst) = &vui.video_signal_type {
            let col = vst.colour_description.as_ref().map(|cd| Colour {
                primaries: cd.colour_primaries as u16,
                transfer: cd.transfer_characteristics as u16,
                matrix: cd.matrix_coeffs as u16,
                full_range: vst.video_full_range_flag,
            });
            let range = match vst.colour_description.as_ref().map(|cd| cd.transfer_characteristics) {
                Some(16) => VideoRange::Pq,
                Some(18) => VideoRange::Hlg,
                _ => VideoRange::Sdr,
            };
            (range, col)
        } else {
            (VideoRange::Sdr, None)
        };
        (range, colour)
    } else {
        (VideoRange::Sdr, None)
    };

    let pixel_aspect = sps.vui_parameters.as_ref().and_then(|v| {
        aspect_ratio_from_idc(v.aspect_ratio_idc, v.sar_width, v.sar_height)
    });

    let frame_rate = sps
        .vui_parameters
        .as_ref()
        .and_then(|v| v.timing_info.as_ref())
        .and_then(|ti| {
            if ti.num_units_in_tick > 0 {
                Some(ti.time_scale as f64 / ti.num_units_in_tick as f64)
            } else {
                None
            }
        })
        .or_else(|| params.frame_rate.map(|r| r.num as f64 / r.den as f64));

    let highest_sub_layer = sps.max_sub_layers_minus1 as usize;
    let reorder_depth = sps
        .sub_layer_ordering_info
        .get(highest_sub_layer)
        .map_or(0, |info| info.max_num_reorder_pics);

    let field_seq_flag = sps.vui_parameters.as_ref().map_or(false, |v| v.field_seq_flag);
    let progressive = (sps.ptl.general_constraint_indicator_flags & (1 << 47)) != 0;
    let interlaced_flag = (sps.ptl.general_constraint_indicator_flags & (1 << 46)) != 0;
    let interlaced = field_seq_flag || (interlaced_flag && !progressive);

    // SEI extraction
    let mut mastering_display = None;
    let mut content_light = None;
    for nal in &sei_list {
        if nal.len() > 2 {
            let rbsp = unescape_rbsp(&nal[2..]);
            parse_sei_messages(&rbsp, |ptype, payload| {
                if ptype == 137 && payload.len() == 24 && mastering_display.is_none() {
                    mastering_display = payload.try_into().ok();
                } else if ptype == 144 && payload.len() == 4 && content_light.is_none() {
                    content_light = payload.try_into().ok();
                }
            });
        }
    }

    let config = VideoConfig {
        codec: VideoCodec::Hevc,
        width,
        height,
        record,
        nal_length_size,
        codec_string,
        range,
        colour,
        mastering_display,
        content_light,
        pixel_aspect,
        frame_rate,
        reorder_depth,
        interlaced,
        dovi: None,
    };
    Some(config)
}

/// None unless a sequence header OBU is found in the av1C record's configOBUs or, failing that,
/// in `first_keyframe`. Without a record in the extradata, one is built from that header.
fn av1_config(params: &CodecParameters, first_keyframe: &[u8]) -> Option<VideoConfig> {
    let record = av1_record(params.extradata.as_slice());
    let record_config = record.and_then(Av1CodecConfig::parse);
    let config_obus = record.map_or(&[][..], |body| &body[4..]);
    // The sequence header may live in configOBUs or only in the samples.
    let (seq, seq_obu) = first_sequence_header(config_obus).or_else(|| first_sequence_header(first_keyframe))?;
    let record = match record_config {
        Some(_) => record?.to_vec(),
        None => {
            let mut config = Av1CodecConfig::from_sequence_header(&seq);
            config.config_obus = seq_obu;
            config.to_bytes()
        }
    };
    let config = Av1CodecConfig::parse(&record)?;

    let width = match seq.max_frame_width_minus_1.saturating_add(1) {
        0 => params.width?,
        width => width,
    };
    let height = match seq.max_frame_height_minus_1.saturating_add(1) {
        0 => params.height?,
        height => height,
    };

    let cc = seq.color_config;
    let colour = cc.color_description_present_flag.then_some(Colour {
        primaries: u16::from(cc.color_primaries),
        transfer: u16::from(cc.transfer_characteristics),
        matrix: u16::from(cc.matrix_coefficients),
        full_range: cc.color_range,
    });
    let range = match colour.map(|c| c.transfer) {
        Some(16) => VideoRange::Pq,
        Some(18) => VideoRange::Hlg,
        _ => VideoRange::Sdr,
    };
    let tier = if config.seq_tier == 0 { 'M' } else { 'H' };
    let codec_string =
        format!("av01.{}.{:02}{tier}.{:02}", config.seq_profile, config.seq_level_idx, config.bit_depth());

    let (config_mdcv, config_clli) = hdr_metadata(config_obus);
    let (key_mdcv, key_clli) = hdr_metadata(first_keyframe);

    let frame_rate = seq
        .timing_info
        .and_then(|timing| {
            if !timing.equal_picture_interval || timing.num_units_in_display_tick == 0 || timing.time_scale == 0 {
                return None;
            }
            let ticks = timing.num_ticks_per_picture_minus_1.map_or(1, |n| n.saturating_add(1));
            Some(f64::from(timing.time_scale) / (f64::from(timing.num_units_in_display_tick) * f64::from(ticks)))
        })
        .filter(|fps| fps.is_finite() && (1.0..=500.0).contains(fps))
        .or_else(|| params.frame_rate.filter(|rate| rate.den > 0).map(|rate| rate.num as f64 / rate.den as f64));

    Some(VideoConfig {
        codec: VideoCodec::Av1,
        width,
        height,
        record,
        nal_length_size: 0,
        codec_string,
        range,
        colour,
        mastering_display: config_mdcv.or(key_mdcv),
        content_light: config_clli.or(key_clli),
        pixel_aspect: None,
        frame_rate,
        // AV1 shows pictures in decode order: no reordering.
        reorder_depth: 0,
        interlaced: false,
        dovi: None,
    })
}

/// The `av1C` record body in the extradata: Matroska `V_AV1` CodecPrivate is the body; some MP4
/// paths hand over the whole `av1C` box.
fn av1_record(extra: &[u8]) -> Option<&[u8]> {
    if extra.len() >= 4 && extra[0] == 0x81 {
        return Some(extra);
    }
    if extra.len() >= 8 && &extra[4..8] == b"av1C" {
        let size = u32::from_be_bytes([extra[0], extra[1], extra[2], extra[3]]) as usize;
        // The body carries at least the record's four fixed bytes.
        if size >= 12 && size <= extra.len() {
            return Some(&extra[8..size]);
        }
    }
    None
}

/// An OBU in a low-overhead AV1 bytestream.
struct ObuUnit<'a> {
    kind: u8,
    payload: &'a [u8],
    /// The whole unit, header and size field included.
    bytes: &'a [u8],
}

/// Walks the OBUs of `data`. The last unit may omit its size field (allowed in a sample, though
/// not in configOBUs).
fn obu_units(data: &[u8]) -> impl Iterator<Item = ObuUnit<'_>> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos >= data.len() {
            return None;
        }
        let start = pos;
        let header = data[pos];
        pos += 1;
        if header & 0x80 != 0 {
            return None;
        }
        let kind = (header >> 3) & 0x0F;
        if (header >> 2) & 1 == 1 {
            if pos >= data.len() {
                return None;
            }
            pos += 1;
        }
        let payload_end = if (header >> 1) & 1 == 1 {
            let (size, len) = parse_leb128(data.get(pos..)?).ok()?;
            pos += len;
            match usize::try_from(size).ok().and_then(|size| pos.checked_add(size)) {
                Some(end) if end <= data.len() => end,
                _ => return None,
            }
        } else {
            data.len()
        };
        let unit = ObuUnit { kind, payload: &data[pos..payload_end], bytes: &data[start..payload_end] };
        pos = payload_end;
        Some(unit)
    })
}

/// The first sequence header OBU: parsed fields and its exact bytes.
fn first_sequence_header(data: &[u8]) -> Option<(SequenceHeader, Vec<u8>)> {
    let unit = obu_units(data).find(|unit| unit.kind == 1)?;
    let seq = parse_sequence_header(unit.payload).ok()?;
    Some((seq, unit.bytes.to_vec()))
}

/// HDR10 static metadata from the metadata OBUs (types 1 and 2). HDR_CLL is the `clli` body as it
/// stands; HDR_MDCV is converted to the `mdcv` layout (`av1_mdcv_to_mp4`).
fn hdr_metadata(data: &[u8]) -> (Option<[u8; 24]>, Option<[u8; 4]>) {
    let mut mastering_display = None;
    let mut content_light = None;
    for unit in obu_units(data).filter(|unit| unit.kind == 5) {
        let Ok((meta_type, len)) = parse_leb128(unit.payload) else { continue };
        let payload = &unit.payload[len..];
        match meta_type {
            // Metadata OBUs end in trailing bits (a 0x80 byte after these byte-aligned
            // payloads), so the payload runs past the fields.
            1 if payload.len() >= 4 && content_light.is_none() => content_light = payload[..4].try_into().ok(),
            2 if payload.len() >= 24 && mastering_display.is_none() => {
                mastering_display = payload[..24].try_into().ok().map(av1_mdcv_to_mp4)
            }
            _ => {}
        }
    }
    (mastering_display, content_light)
}

/// AV1's HDR_MDCV and the ISOBMFF `mdcv` box (SMPTE ST 2086, as in HEVC's SEI) carry the same
/// fields in different orders and units (AV1-ISOBMFF §2.3.5): AV1 lists the primaries red, green,
/// blue with chromaticities in 0.16 fixed point and luminance max in 24.8 and min in 18.14 fixed
/// point (cd/m²); `mdcv` lists green, blue, red with chromaticities in units of 0.00002 and both
/// luminances in 0.0001 cd/m².
fn av1_mdcv_to_mp4(av1: [u8; 24]) -> [u8; 24] {
    let u16_at = |i: usize| u64::from(u16::from_be_bytes([av1[i], av1[i + 1]]));
    let u32_at = |i: usize| u64::from(u32::from_be_bytes([av1[i], av1[i + 1], av1[i + 2], av1[i + 3]]));
    let chroma = |v: u64| u16::try_from((v * 50_000 + 32_768) / 65_536).unwrap_or(u16::MAX);
    let luminance = |v: u64, scale: u64| u32::try_from((v * 10_000 + scale / 2) / scale).unwrap_or(u32::MAX);
    let mut out = [0u8; 24];
    // AV1 primary i sits at 4·i (x) and 4·i + 2 (y): 0 red, 1 green, 2 blue.
    for (slot, primary) in [1, 2, 0].into_iter().enumerate() {
        out[4 * slot..4 * slot + 2].copy_from_slice(&chroma(u16_at(4 * primary)).to_be_bytes());
        out[4 * slot + 2..4 * slot + 4].copy_from_slice(&chroma(u16_at(4 * primary + 2)).to_be_bytes());
    }
    out[12..14].copy_from_slice(&chroma(u16_at(12)).to_be_bytes());
    out[14..16].copy_from_slice(&chroma(u16_at(14)).to_be_bytes());
    out[16..20].copy_from_slice(&luminance(u32_at(16), 256).to_be_bytes());
    out[20..24].copy_from_slice(&luminance(u32_at(20), 16_384).to_be_bytes());
    out
}

/// An AV1 sample without an opening temporal delimiter OBU (type 2): ISOBMFF samples carry none,
/// and some muxers keep one in every temporal unit.
pub(crate) fn without_temporal_delimiter(data: &[u8]) -> &[u8] {
    let Some(&header) = data.first() else { return data };
    if header & 0x80 != 0 || (header >> 3) & 0x0F != 2 {
        return data;
    }
    // A temporal delimiter has no payload: its header and size field (when present) are all of
    // it, even without a size field, where any other OBU would run to the end of the sample.
    let header_len = 1 + usize::from((header >> 2) & 1 == 1);
    let end = if (header >> 1) & 1 == 1 {
        data.get(header_len..)
            .and_then(|rest| parse_leb128(rest).ok())
            .and_then(|(size, len)| header_len.checked_add(len)?.checked_add(usize::try_from(size).ok()?))
    } else {
        Some(header_len)
    };
    match end {
        Some(end) if end <= data.len() => &data[end..],
        _ => data,
    }
}

const AC3_SAMPLE_RATES: [u32; 4] = [48000, 44100, 32000, 0];
const ACMOD_CHANNELS: [u16; 8] = [2, 1, 2, 3, 3, 4, 4, 5];
// ---------------------------------------------------------------------------
// E-AC-3
// ---------------------------------------------------------------------------

const EAC3_BLOCKS: [u16; 4] = [1, 2, 3, 6];

struct EAc3SyncframeInfo {
    strmtyp: u8,
    #[allow(dead_code)]
    substreamid: u8,
    frame_size: usize,
    sample_rate: u32,
    numblks: u32,
    fscod: u8,
    bsid: u8,
    bsmod: u8,
    acmod: u8,
    lfeon: u8,
    chanmap: u16,
    channels: u16,
    joc_complexity: Option<u8>,
}

fn parse_eac3_syncframe(data: &[u8]) -> Option<EAc3SyncframeInfo> {
    if data.len() < 6 || data[0] != 0x0B || data[1] != 0x77 {
        return None;
    }
    let strmtyp = (data[2] >> 6) & 3;
    if strmtyp == 3 {
        return None;
    }
    let substreamid = (data[2] >> 3) & 7;
    let frmsiz = ((u16::from(data[2] & 7) << 8) | u16::from(data[3])) as usize;
    let frame_size = (frmsiz + 1) * 2;
    if frame_size < 6 || frame_size > data.len() {
        return None;
    }

    let fscod = (data[4] >> 6) & 3;
    let (sample_rate, numblks) = if fscod == 3 {
        let fscod2 = (data[4] >> 4) & 3;
        if fscod2 == 3 {
            return None;
        }
        (AC3_SAMPLE_RATES[fscod2 as usize] / 2, 6)
    } else {
        let numblkscod = ((data[4] >> 4) & 3) as usize;
        (AC3_SAMPLE_RATES[fscod as usize], EAC3_BLOCKS[numblkscod] as u32)
    };

    let acmod = (data[4] >> 1) & 7;
    let lfeon = data[4] & 1;
    let bsid = data[5] >> 3;
    let base_channels = ACMOD_CHANNELS[acmod as usize] + lfeon as u16;

    // Bit reader after bsid (byte 5 bit 2)
    let mut br = BitReader::new(&data[5..]);
    br.skip(5)?; // bsid
    br.skip(5)?; // dialnorm
    let compre = br.read(1)? != 0;
    if compre {
        br.skip(8)?;
    }
    if acmod == 0 {
        br.skip(5)?; // dialnorm2
        let compr2e = br.read(1)? != 0;
        if compr2e {
            br.skip(8)?;
        }
    }

    let mut chanmap = 0u16;
    if strmtyp == 1 {
        let chanmape = br.read(1)? != 0;
        if chanmape {
            chanmap = br.read(16)? as u16;
        }
    }
    let mixmdate = br.read(1)? != 0;
    if mixmdate {
        if acmod > 2 {
            br.skip(2)?; // dmixmod
        }
        if acmod & 1 != 0 && acmod > 2 {
            br.skip(6)?; // ltrtcmixlev, lorocmixlev
        }
        if acmod & 4 != 0 {
            br.skip(6)?; // ltrtsurmixlev, lorosurmixlev
        }
        if lfeon != 0 {
            let lfemixlevcode = br.read(1)? != 0;
            if lfemixlevcode {
                br.skip(5)?;
            }
        }
        if strmtyp == 0 {
            let pgmscle = br.read(1)? != 0;
            if pgmscle {
                br.skip(6)?;
            }
            if acmod == 0 {
                let pgmscl2e = br.read(1)? != 0;
                if pgmscl2e {
                    br.skip(6)?;
                }
            }
            let extpgmscle = br.read(1)? != 0;
            if extpgmscle {
                br.skip(6)?;
            }
            let mixdef = br.read(2)?;
            if mixdef == 1 {
                br.skip(3)?;
            } else if mixdef == 2 {
                br.skip(12)?;
            } else if mixdef == 3 {
                let mixdatalen = br.read(5)? as usize;
                br.skip((mixdatalen + 2) * 8)?;
            }
            if acmod < 2 {
                let paninfoe = br.read(1)? != 0;
                if paninfoe {
                    br.skip(14)?;
                }
                if acmod == 0 {
                    let paninfo2e = br.read(1)? != 0;
                    if paninfo2e {
                        br.skip(14)?;
                    }
                }
            }
            if mixdef != 0 {
                let frmmixcfginfoe = br.read(1)? != 0;
                if frmmixcfginfoe {
                    if numblks == 1 {
                        br.skip(5)?;
                    } else {
                        for _ in 0..numblks {
                            let blkmixcfginfoe = br.read(1)? != 0;
                            if blkmixcfginfoe {
                                br.skip(5)?;
                            }
                        }
                    }
                }
            }
        }
    }

    let infomdate = br.read(1).unwrap_or(0) != 0;
    let bsmod = if infomdate {
        (br.read(3).unwrap_or(0)) as u8
    } else {
        0
    };
    let joc_complexity = parse_eac3_joc(&mut br, strmtyp, numblks, fscod, acmod, infomdate);

    Some(EAc3SyncframeInfo {
        strmtyp,
        substreamid,
        frame_size,
        sample_rate,
        numblks,
        fscod,
        bsid,
        bsmod,
        acmod,
        lfeon,
        chanmap,
        channels: base_channels,
        joc_complexity,
    })
}

fn parse_eac3_joc(
    br: &mut BitReader<'_>,
    strmtyp: u8,
    numblks: u32,
    fscod: u8,
    acmod: u8,
    infomdate: bool,
) -> Option<u8> {
    if infomdate {
        br.skip(2)?; // copyrightb, origbs
        if acmod == 2 {
            br.skip(4)?; // dsurmod, dheadphonmod
        }
        if acmod >= 6 {
            br.skip(2)?; // dsurexmod
        }
        if br.read(1)? != 0 {
            br.skip(8)?; // audprodie
        }
        if acmod == 0 && br.read(1)? != 0 {
            br.skip(8)?; // audprodi2e
        }
        if fscod < 3 {
            br.skip(1)?; // sourcefscod
        }
    }
    if strmtyp == 0 && numblks != 6 {
        br.skip(1)?; // convsync
    }
    if strmtyp == 2 {
        let blkid = if numblks == 6 { 1 } else { br.read(1)? };
        if blkid != 0 {
            br.skip(6)?;
        }
    }
    let addbsie = br.read(1)? != 0;
    if !addbsie {
        return None;
    }
    let addbsil = br.read(6)? as usize + 1;
    let mut b0 = 0u8;
    let mut b1 = 0u8;
    for i in 0..addbsil {
        let b = br.read(8)? as u8;
        if i == 0 {
            b0 = b;
        } else if i == 1 {
            b1 = b;
        }
    }
    if addbsil >= 2 && (b0 & 0x01) != 0 {
        Some(b1)
    } else {
        None
    }
}

fn chanmap_to_chan_loc(chanmap: u16) -> (u16, u16) {
    let mut loc = 0u16;
    let mut extra_chans = 0u16;

    if chanmap & (1 << 12) != 0 {
        loc |= 1 << 8; // Lc/Rc
        extra_chans += 2;
    }
    if chanmap & (1 << 4) != 0 {
        loc |= 1 << 7; // Lrs/Rrs
        extra_chans += 2;
    }
    if chanmap & (1 << 5) != 0 {
        loc |= 1 << 6; // Cs
        extra_chans += 1;
    }
    if chanmap & (1 << 6) != 0 {
        loc |= 1 << 5; // Ts
        extra_chans += 1;
    }
    if chanmap & (1 << 7) != 0 {
        loc |= 1 << 4; // Lsd/Rsd
        extra_chans += 2;
    }
    if chanmap & (1 << 8) != 0 {
        loc |= 1 << 3; // Lw/Rw
        extra_chans += 2;
    }
    if chanmap & (1 << 9) != 0 {
        loc |= 1 << 2; // Vhl/Vhr
        extra_chans += 2;
    }
    if chanmap & (1 << 10) != 0 {
        loc |= 1 << 1; // Vhc
        extra_chans += 1;
    }
    if chanmap & (1 << 13) != 0 {
        loc |= 1 << 0; // LFE2
        extra_chans += 1;
    }

    if loc == 0 {
        loc = 1 << 7; // Default: 2 rear surrounds
        extra_chans = 2;
    }

    (loc, extra_chans)
}

fn eac3_config(params: &CodecParameters, first_packet: &[u8]) -> Option<AudioConfig> {
    let source_buf = if first_packet.len() >= 6 && first_packet[0] == 0x0B && first_packet[1] == 0x77 {
        Some(first_packet)
    } else if params.extradata.len() >= 6
        && params.extradata[0] == 0x0B
        && params.extradata[1] == 0x77
    {
        Some(params.extradata.as_slice())
    } else {
        None
    };

    if let Some(buf) = source_buf {
        let mut pos = 0;
        let mut syncframes = Vec::new();
        while pos + 6 <= buf.len() {
            if let Some(info) = parse_eac3_syncframe(&buf[pos..]) {
                let size = info.frame_size;
                syncframes.push(info);
                pos += size;
            } else {
                break;
            }
        }

        if syncframes.is_empty() {
            return None;
        }

        // Calculate data_rate (kbit/s, all substreams)
        let mut total_bitrate_bps = 0.0f64;
        for sf in &syncframes {
            let samples = (sf.numblks * 256) as f64;
            let rate_bps = (sf.frame_size as f64 * 8.0 * sf.sample_rate as f64) / samples;
            total_bitrate_bps += rate_bps;
        }
        let data_rate_kbit = (total_bitrate_bps / 1000.0).round() as u32;

        // Group into independent substreams and their dependent substreams
        let mut ind_substreams: Vec<(&EAc3SyncframeInfo, Vec<&EAc3SyncframeInfo>)> = Vec::new();
        for sf in &syncframes {
            if sf.strmtyp == 0 || sf.strmtyp == 2 {
                ind_substreams.push((sf, Vec::new()));
            } else if sf.strmtyp == 1 {
                if let Some(last) = ind_substreams.last_mut() {
                    last.1.push(sf);
                }
            }
        }

        if ind_substreams.is_empty() {
            return None;
        }

        let num_ind_sub = (ind_substreams.len() as u32).saturating_sub(1);

        let atmos_joc = syncframes.iter().find_map(|sf| sf.joc_complexity);
        let mut bw = BitWriter::new();
        bw.write_bits(data_rate_kbit, 13);
        bw.write_bits(num_ind_sub, 3);

        let mut total_channels = 0u16;

        for (i, (ind, deps)) in ind_substreams.iter().enumerate() {
            bw.write_bits(ind.fscod as u32, 2);
            bw.write_bits(ind.bsid as u32, 5);
            bw.write_bits(0, 1); // reserved
            bw.write_bits(0, 1); // asvc
            bw.write_bits(ind.bsmod as u32, 3);
            bw.write_bits(ind.acmod as u32, 3);
            bw.write_bits(ind.lfeon as u32, 1);
            bw.write_bits(0, 3); // reserved
            let num_dep_sub = deps.len().min(15) as u32;
            bw.write_bits(num_dep_sub, 4);

            let mut dep_channels = 0u16;
            if num_dep_sub > 0 {
                let chanmap = deps[0].chanmap;
                let (chan_loc, extra) = chanmap_to_chan_loc(chanmap);
                bw.write_bits(chan_loc as u32, 9);
                dep_channels = extra;
            } else {
                bw.write_bits(0, 1); // reserved
            }

            if i == 0 {
                total_channels = ind.channels + dep_channels;
            }
        }

        if let Some(joc) = atmos_joc {
            bw.write_bits(0x01, 8); // reserved(7) = 0, flag_ec3_extension_type_a(1) = 1
            bw.write_bits(u32::from(joc), 8); // complexity_index_type_a
        }

        let dec3 = bw.finish();
        let sample_rate = ind_substreams[0].0.sample_rate;

        return Some(AudioConfig {
            codec: AudioCodec::Eac3,
            sample_rate,
            channels: total_channels,
            bits: 16,
            config: dec3,
            codec_string: "ec-3".into(),
            frame_samples: 0,
            adts: false,
            language: None,
            atmos_joc,
        });
    }

    None
}

// ---------------------------------------------------------------------------
// BitWriter / BitReader
// ---------------------------------------------------------------------------

struct BitWriter {
    bytes: Vec<u8>,
    bit_pos: usize,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            bit_pos: 0,
        }
    }

    fn write_bits(&mut self, val: u32, count: usize) {
        for i in (0..count).rev() {
            let bit = ((val >> i) & 1) as u8;
            if self.bit_pos == 0 {
                self.bytes.push(bit << 7);
                self.bit_pos = 1;
            } else {
                let last = self.bytes.last_mut().unwrap();
                *last |= bit << (7 - self.bit_pos);
                self.bit_pos = (self.bit_pos + 1) % 8;
            }
        }
    }

    #[cfg(test)]
    fn write_ue(&mut self, val: u32) {
        let v = val + 1;
        let total_bits = 32 - v.leading_zeros() as usize;
        self.write_bits(0, total_bits - 1);
        self.write_bits(v, total_bits);
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn read(&mut self, count: usize) -> Option<u32> {
        if self.bit_pos + count > self.data.len() * 8 {
            return None;
        }
        let mut val = 0u32;
        for _ in 0..count {
            let byte_idx = self.bit_pos / 8;
            let bit_idx = 7 - (self.bit_pos % 8);
            let bit = (self.data[byte_idx] >> bit_idx) & 1;
            val = (val << 1) | (bit as u32);
            self.bit_pos += 1;
        }
        Some(val)
    }

    fn skip(&mut self, count: usize) -> Option<()> {
        if self.bit_pos + count > self.data.len() * 8 {
            return None;
        }
        self.bit_pos += count;
        Some(())
    }
    fn read_ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0;
        while self.read(1)? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return None;
            }
        }
        if leading_zeros == 0 {
            Some(0)
        } else {
            let val = self.read(leading_zeros)?;
            Some((1 << leading_zeros) - 1 + val)
        }
    }
}

/// Parses a 24-byte `DOVIDecoderConfigurationRecord`.
pub(crate) fn parse_dovi_record(bytes: &[u8]) -> Option<(u8, u8, u8, [u8; 24])> {
    if bytes.len() < 24 || bytes[0] != 1 {
        return None;
    }
    let w = u16::from_be_bytes([bytes[2], bytes[3]]);
    let profile = ((w >> 9) & 0x7F) as u8;
    let level = ((w >> 3) & 0x3F) as u8;
    let compat_id = (bytes[4] >> 4) & 0x0F;
    let mut record = [0u8; 24];
    record.copy_from_slice(&bytes[..24]);
    Some((profile, level, compat_id, record))
}

/// Builds a 24-byte `DOVIDecoderConfigurationRecord`.
pub(crate) fn build_dovi_record(
    profile: u8,
    level: u8,
    rpu: bool,
    el: bool,
    bl: bool,
    compat_id: u8,
) -> [u8; 24] {
    let mut rec = [0u8; 24];
    rec[0] = 1;
    rec[1] = 0;
    let w: u16 = ((profile as u16 & 0x7F) << 9)
        | ((level as u16 & 0x3F) << 3)
        | ((rpu as u16) << 2)
        | ((el as u16) << 1)
        | (bl as u16);
    rec[2..4].copy_from_slice(&w.to_be_bytes());
    rec[4] = (compat_id & 0x0F) << 4;
    rec
}

/// Finds a `dvcC` or `dvvC` box body in a byte slice.
pub(crate) fn find_dovi_box(bytes: &[u8]) -> Option<&[u8]> {
    let mut i = 0;
    while i + 8 <= bytes.len() {
        if &bytes[i + 4..i + 8] == b"dvcC" || &bytes[i + 4..i + 8] == b"dvvC" {
            let size = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
            if size >= 32 && i + size <= bytes.len() {
                return Some(&bytes[i + 8..i + size]);
            } else if bytes.len() >= i + 8 + 24 {
                return Some(&bytes[i + 8..i + 8 + 24]);
            }
        }
        i += 1;
    }
    None
}

/// Inspects HEVC `NAL_UNSPEC62` in keyframe NAL units and guesses the Dolby Vision profile.
pub(crate) fn parse_hevc_rpu_profile(nals: &[&[u8]]) -> Option<u8> {
    let nal = nals
        .iter()
        .find(|nal| nal.len() >= 8 && ((nal[0] >> 1) & 0x3F) == 62)?;
    let mut rbsp = unescape_rbsp(&nal[2..]);
    while rbsp.last() == Some(&0) {
        rbsp.pop();
    }
    if rbsp.len() < 6 || rbsp[0] != 0x19 || *rbsp.last()? != 0x80 {
        return None;
    }
    let mut br = BitReader::new(&rbsp[1..]);
    let rpu_type = br.read(6)?;
    if rpu_type != 2 {
        return None;
    }
    let rpu_format = br.read(11)?;
    let vdr_rpu_profile = br.read(4)?;
    let _vdr_rpu_level = br.read(4)?;
    let vdr_seq_info_present = br.read(1)? != 0;
    if !vdr_seq_info_present {
        return None;
    }
    let _chroma_resampling = br.read(1)?;
    let coef_data_type = br.read(2)?;
    if coef_data_type == 0 {
        br.read_ue()?;
    }
    let _norm = br.read(2)?;
    let bl_video_full_range = br.read(1)? != 0;
    if vdr_rpu_profile == 0 && bl_video_full_range {
        Some(5)
    } else if vdr_rpu_profile == 1 {
        if (rpu_format & 0x700) == 0 {
            let _bl = br.read_ue()?;
            let _el = br.read_ue()?;
            let _vdr = br.read_ue()?;
            let _srf = br.read(1)?;
            let _rz = br.read(3)?;
            let _esrf = br.read(1)?;
            let disable_residual_flag = br.read(1)? != 0;
            if !disable_residual_flag {
                Some(7)
            } else {
                Some(8)
            }
        } else {
            Some(8)
        }
    } else {
        None
    }
}

/// Applies Dolby Vision configuration to a `VideoConfig`.
pub(crate) fn apply_dovi(
    config: &mut VideoConfig,
    raw_dovi_record: Option<&[u8]>,
    first_keyframe: &[u8],
    framing: Framing,
) {
    if config.codec != VideoCodec::Hevc {
        config.dovi = None;
        return;
    }
    if raw_dovi_record.is_none() && config.dovi.is_some() {
        return;
    }

    let nals = extract_nals(first_keyframe, framing);
    let rpu_profile = parse_hevc_rpu_profile(&nals);

    let record_payload = raw_dovi_record.and_then(|r| find_dovi_box(r).or(Some(r)));
    let parsed = record_payload.and_then(parse_dovi_record);

    let resolved = match parsed {
        None => {
            // Profile 5 leaves its colour unspecified or names its own matrix, IPT-C2 (15).
            let unspecified_colour = config.colour.is_none()
                || config.colour.is_some_and(|c| (c.primaries == 2 && c.transfer == 2 && c.matrix == 2) || c.matrix == 15);
            match rpu_profile {
                Some(5) if unspecified_colour => Some((5u8, 6u8, 0u8, build_dovi_record(5, 6, true, false, true, 0))),
                // A profile 8 RPU with no container record: the base layer's own transfer says
                // which compatibility it keeps (8.1 over HDR10, 8.4 over HLG).
                Some(8) if config.range == VideoRange::Pq => Some((8, 6, 1, build_dovi_record(8, 6, true, false, true, 1))),
                Some(8) if config.range == VideoRange::Hlg => Some((8, 6, 4, build_dovi_record(8, 6, true, false, true, 4))),
                _ => None,
            }
        }
        Some((mut profile, level, mut compat_id, mut record)) => {
            if profile == 5
                && matches!(config.range, VideoRange::Pq | VideoRange::Hlg)
                && matches!(rpu_profile, Some(7 | 8))
            {
                profile = rpu_profile.unwrap();
                compat_id = if config.range == VideoRange::Hlg { 4 } else { 1 };
                record = build_dovi_record(8, if level > 0 { level } else { 6 }, true, false, true, compat_id);
            }
            Some((profile, level, compat_id, record))
        }
    };
    match resolved {
        Some((5, level, _compat_id, _record)) => {
            let lvl = if level > 0 { level } else { 6 };
            config.codec_string = format!("dvh1.05.{lvl:02}");
            config.range = VideoRange::Pq;
            if config.colour.is_none()
                || config.colour.is_some_and(|c| c.primaries == 2 && c.transfer == 2 && c.matrix == 2)
            {
                config.colour = Some(Colour {
                    primaries: 9,
                    transfer: 16,
                    matrix: 9,
                    full_range: false,
                });
            }
            config.dovi = Some(DoviConfig {
                profile: 5,
                level: lvl,
                compat_id: 0,
                record: build_dovi_record(5, lvl, true, false, true, 0),
                use_dvh1: true,
                supplemental_codecs: None,
                convert_p7_to_81: false,
            });
        }
        Some((7, level, _compat_id, _record)) => {
            let lvl = if level > 0 { level } else { 6 };
            config.range = VideoRange::Pq;
            config.dovi = Some(DoviConfig {
                profile: 8,
                level: lvl,
                compat_id: 1,
                record: build_dovi_record(8, lvl, true, false, true, 1),
                use_dvh1: false,
                supplemental_codecs: Some(format!("dvh1.08.{lvl:02}/db1p")),
                convert_p7_to_81: true,
            });
        }
        Some((8, level, compat_id, record)) => {
            let lvl = if level > 0 { level } else { 6 };
            match compat_id {
                2 => {
                    config.dovi = None;
                }
                4 => {
                    config.range = VideoRange::Hlg;
                    config.dovi = Some(DoviConfig {
                        profile: 8,
                        level: lvl,
                        compat_id: 4,
                        record,
                        use_dvh1: false,
                        supplemental_codecs: Some(format!("dvh1.08.{lvl:02}/db4h")),
                        convert_p7_to_81: false,
                    });
                }
                _ => {
                    let rec = if compat_id == 1 {
                        record
                    } else {
                        build_dovi_record(8, lvl, true, false, true, 1)
                    };
                    config.range = VideoRange::Pq;
                    config.dovi = Some(DoviConfig {
                        profile: 8,
                        level: lvl,
                        compat_id: 1,
                        record: rec,
                        use_dvh1: false,
                        supplemental_codecs: Some(format!("dvh1.08.{lvl:02}/db1p")),
                        convert_p7_to_81: false,
                    });
                }
            }
        }
        _ => {
            config.dovi = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::CodecId;

    // Fixture byte constants extracted from test media:
    const H264_AVCC_MKV: &[u8] = &[
        0x01, 0x64, 0x00, 0x1f, 0xff, 0xe1, 0x00, 0x18, 0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9,
        0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00, 0x3e, 0x90, 0x00, 0x0b, 0xb8, 0x00,
        0xf1, 0x83, 0x19, 0x60, 0x01, 0x00, 0x04, 0x68, 0xef, 0x8f, 0xcb, 0xfd, 0xf8, 0xf8,
        0x00,
    ];

    const AC3_SYNCFRAME_MKV: &[u8] = &[
        0x0b, 0x77, 0xb7, 0x3c, 0x1c, 0x40, 0xeb, 0xf8, 0x40, 0x3e, 0xff, 0x74, 0x84, 0x04,
        0x10, 0x80,
    ];

    const EAC3_SYNCFRAME_MKV: &[u8] = &[
        0x0b, 0x77, 0x02, 0xff, 0x3f, 0x87, 0xc0, 0x00, 0x20, 0x00, 0x00, 0x00, 0x02, 0x00,
        0x00, 0x00,
    ];

    fn hevc_hdr10_fixture_hvcc() -> Vec<u8> {
        // Extradata from hevc10_hdr10_eac3.mkv: header, VPS, SPS, PPS, and 2 prefix SEI NALUs (CLLI 144 and MDCV 137)
        vec![
            0x01, 0x02, 0x20, 0x00, 0x00, 0x00, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0xf0,
            0x00, 0xfc, 0xfd, 0xfa, 0xfa, 0x00, 0x00, 0x0f, 0x04,
            // Array 0: VPS (type 32, 1 NALU, len 24)
            0x20, 0x00, 0x01, 0x00, 0x18, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x02, 0x20, 0x00,
            0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x78, 0x95, 0x94,
            0x09,
            // Array 1: SPS (type 33, 1 NALU, len 46)
            0x21, 0x00, 0x01, 0x00, 0x2e, 0x42, 0x01, 0x01, 0x02, 0x20, 0x00, 0x00, 0x03, 0x00,
            0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x78, 0xa0, 0x03, 0xc0, 0x80, 0x11,
            0x07, 0xca, 0xd9, 0x65, 0x65, 0x4a, 0x4c, 0x2f, 0x01, 0x6a, 0x12, 0x20, 0x12, 0x08,
            0x00, 0x00, 0x1f, 0x48, 0x00, 0x02, 0xee, 0x00, 0x40,
            // Array 2: PPS (type 34, 1 NALU, len 6)
            0x22, 0x00, 0x01, 0x00, 0x06, 0x44, 0x01, 0xc0, 0x73, 0xc1, 0x89,
            // Array 3: Prefix SEI (type 39, 2 NALUs: len 9, len 30)
            0x27, 0x00, 0x02,
            0x00, 0x09, 0x4e, 0x01, 0x90, 0x04, 0x03, 0xe8, 0x01, 0x90, 0x80,
            0x00, 0x1e, 0x4e, 0x01, 0x89, 0x18, 0x33, 0xc2, 0x86, 0xc4, 0x1d, 0x4c, 0x0b, 0xb8,
            0x84, 0xd0, 0x3e, 0x80, 0x3d, 0x13, 0x40, 0x42, 0x00, 0x98, 0x96, 0x80, 0x00, 0x00,
            0x03, 0x00, 0x32, 0x80,
        ]
    }

    #[test]
    fn test_video_framing() {
        let mut p_h264 = CodecParameters::video(CodecId::new("h264"));
        p_h264.extradata = H264_AVCC_MKV.to_vec();
        assert_eq!(video_framing(&p_h264), Framing::LengthPrefixed(4));

        let mut p_hevc = CodecParameters::video(CodecId::new("hevc"));
        p_hevc.extradata = hevc_hdr10_fixture_hvcc();
        assert_eq!(video_framing(&p_hevc), Framing::LengthPrefixed(4));

        let mut p_annexb = CodecParameters::video(CodecId::new("h264"));
        p_annexb.extradata = vec![0, 0, 0, 1, 0x67, 0x42];
        assert_eq!(video_framing(&p_annexb), Framing::AnnexB);

        let mut p_empty = CodecParameters::video(CodecId::new("h264"));
        p_empty.extradata = vec![];
        assert_eq!(video_framing(&p_empty), Framing::AnnexB);
    }

    #[test]
    fn test_video_config_h264_fixture() {
        let mut p = CodecParameters::video(CodecId::new("h264"));
        p.extradata = H264_AVCC_MKV.to_vec();
        let cfg = video_config(&p, &[]).expect("video_config h264");
        assert_eq!(cfg.codec, VideoCodec::H264);
        assert_eq!(cfg.width, 1280);
        assert_eq!(cfg.height, 720);
        assert_eq!(cfg.codec_string, "avc1.64001f");
        assert_eq!(cfg.nal_length_size, 4);
        assert_eq!(cfg.range, VideoRange::Sdr);
        assert_eq!(cfg.reorder_depth, 2);
        assert!(!cfg.interlaced);
        assert_eq!(cfg.record, H264_AVCC_MKV);
    }

    #[test]
    fn test_video_config_hevc_hdr10_fixture() {
        let mut p = CodecParameters::video(CodecId::new("hevc"));
        let full_hvcc = hevc_hdr10_fixture_hvcc();
        p.extradata = full_hvcc.clone();
        let cfg = video_config(&p, &[]).expect("video_config hevc");

        assert_eq!(cfg.codec, VideoCodec::Hevc);
        assert_eq!(cfg.width, 1920);
        assert_eq!(cfg.height, 1080);
        assert_eq!(cfg.codec_string, "hvc1.2.4.L120.90");
        assert_eq!(cfg.nal_length_size, 4);
        assert_eq!(cfg.range, VideoRange::Pq);

        let col = cfg.colour.expect("colour description present");
        assert_eq!(col.primaries, 9);
        assert_eq!(col.transfer, 16);
        assert_eq!(col.matrix, 9);
        assert!(!col.full_range);

        // Check mastering display and content light from SEIs
        let md = cfg.mastering_display.expect("mastering display");
        assert_eq!(
            md,
            [
                0x33, 0xc2, 0x86, 0xc4, 0x1d, 0x4c, 0x0b, 0xb8, 0x84, 0xd0, 0x3e, 0x80, 0x3d,
                0x13, 0x40, 0x42, 0x00, 0x98, 0x96, 0x80, 0x00, 0x00, 0x00, 0x32
            ]
        );
        let cl = cfg.content_light.expect("content light");
        assert_eq!(cl, [0x03, 0xe8, 0x01, 0x90]);

        // Verify canonicalisation: SEI array stripped, keeping only VPS, SPS, PPS
        // Header (22) + numOfArrays (1) = 23. numOfArrays must be 3.
        assert_eq!(cfg.record[22], 3);
        // Each array has array_completeness=1 (bit 7 set)
        let array0_type = cfg.record[23] & 0x3F;
        let array0_comp = cfg.record[23] >> 7;
        assert_eq!(array0_type, 32);
        assert_eq!(array0_comp, 1);
    }

    #[test]
    fn test_hevc_codec_string_variations() {
        let sps_bytes = vec![
            0x42, 0x01, 0x01, 0x02, 0x20, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00,
            0x00, 0x03, 0x00, 0x78, 0xa0, 0x03, 0xc0, 0x80, 0x11, 0x07, 0xca, 0xd9, 0x65, 0x65,
            0x4a, 0x4c, 0x2f, 0x01, 0x6a, 0x12, 0x20, 0x12, 0x08, 0x00, 0x00, 0x1f, 0x48, 0x00,
            0x02, 0xee, 0x00, 0x40,
        ];
        let pps_bytes = vec![0x44, 0x01, 0xc0, 0x73, 0xc1, 0x89];

        // Annex B input without extradata
        let mut keyframe = Vec::new();
        keyframe.extend_from_slice(&[0, 0, 0, 1]);
        keyframe.extend_from_slice(&sps_bytes);
        keyframe.extend_from_slice(&[0, 0, 0, 1]);
        keyframe.extend_from_slice(&pps_bytes);

        let p = CodecParameters::video(CodecId::new("hevc"));
        let cfg = video_config(&p, &keyframe).expect("hevc config from annex b");
        assert_eq!(cfg.codec_string, "hvc1.2.4.L120.90");
    }

    #[test]
    fn test_eac3_audio_config_and_samples() {
        let p = CodecParameters::audio(CodecId::new("eac3"));
        let mut pkt = EAC3_SYNCFRAME_MKV.to_vec();
        pkt.resize(1536, 0); // 1536 bytes E-AC-3 syncframe

        let cfg = audio_config(&p, &pkt).expect("eac3 config");
        assert_eq!(cfg.codec, AudioCodec::Eac3);
        assert_eq!(cfg.sample_rate, 48000);
        assert_eq!(cfg.channels, 6);
        assert_eq!(cfg.frame_samples, 0);
        assert_eq!(cfg.codec_string, "ec-3");
        assert_eq!(cfg.config, vec![0x0c, 0x00, 0x20, 0x0f, 0x00]); // matches FFmpeg dec3 exactly!
        assert!(!cfg.adts);

        assert_eq!(eac3_frame_samples(&pkt), Some(1536));

        // None if not starting with E-AC-3 syncframe
        let legacy_ac3 = AC3_SYNCFRAME_MKV.to_vec();
        assert_eq!(eac3_frame_samples(&legacy_ac3), None);
    }

    #[test]
    fn test_h264_annex_b_keyframe_builds_avcc() {
        let sps = vec![
            0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00,
            0x3e, 0x90, 0x00, 0x0b, 0xb8, 0x00, 0xf1, 0x83, 0x19, 0x60,
        ];
        let pps = vec![0x68, 0xef, 0x8f, 0xcb];

        let mut keyframe = Vec::new();
        keyframe.extend_from_slice(&[0, 0, 1]);
        keyframe.extend_from_slice(&sps);
        keyframe.extend_from_slice(&[0, 0, 0, 1]);
        keyframe.extend_from_slice(&pps);

        let p = CodecParameters::video(CodecId::new("h264"));
        let cfg = video_config(&p, &keyframe).expect("h264 config from annex b");
        assert_eq!(cfg.codec, VideoCodec::H264);
        assert_eq!(cfg.width, 1280);
        assert_eq!(cfg.height, 720);
        assert_eq!(cfg.codec_string, "avc1.64001f");
        assert_eq!(cfg.nal_length_size, 4);

        // Profile 100 has extension in avcC (chroma_format_idc, bit depths, numOfSequenceParameterSetExt)
        let rec = &cfg.record;
        assert_eq!(rec[0], 1);
        assert_eq!(rec[1], 100); // 0x64
        assert_eq!(rec[4] & 3, 3); // lengthSizeMinusOne = 3
        assert_eq!(rec[5] & 0x1F, 1); // 1 SPS
        let pos_pps = 6 + 2 + sps.len();
        assert_eq!(rec[pos_pps], 1); // 1 PPS
        let pos_ext = pos_pps + 1 + 2 + pps.len();
        assert_eq!(rec[pos_ext] & 0x03, 1); // chroma_format_idc 1 (4:2:0)
        assert_eq!(rec[pos_ext + 1] & 0x07, 0); // bit_depth_luma_minus8 = 0
        assert_eq!(rec[pos_ext + 2] & 0x07, 0); // bit_depth_chroma_minus8 = 0
        assert_eq!(rec[pos_ext + 3], 0); // numOfSequenceParameterSetExt = 0
    }

    #[test]
    fn test_eac3_multiple_syncframes_and_unparseable() {
        let mut pkt1 = EAC3_SYNCFRAME_MKV.to_vec();
        pkt1.resize(1536, 0);
        let mut pkt2 = pkt1.clone();
        let mut multi = pkt1.clone();
        multi.append(&mut pkt2);

        // 2 frames with 6 blocks each -> 2 * 6 * 256 = 3072 samples
        assert_eq!(eac3_frame_samples(&multi), Some(3072));

    }

    #[test]
    fn test_unsupported_codecs_return_none() {
        let p_mp3 = CodecParameters::audio(CodecId::new("mp3"));
        assert_eq!(audio_config(&p_mp3, &[0xFF, 0xFB]), None);

        let p_vp9 = CodecParameters::video(CodecId::new("vp9"));
        assert_eq!(video_config(&p_vp9, &[]), None);
    }

    #[test]
    fn test_parse_and_build_dovi_record() {
        let rec = build_dovi_record(5, 6, true, false, true, 0);
        let parsed = parse_dovi_record(&rec).expect("parse dovi profile 5");
        assert_eq!(parsed.0, 5); // profile
        assert_eq!(parsed.1, 6); // level
        assert_eq!(parsed.2, 0); // compat_id
        assert_eq!(parsed.3, rec);

        let rec8 = build_dovi_record(8, 7, true, false, true, 4);
        let parsed8 = parse_dovi_record(&rec8).expect("parse dovi profile 8");
        assert_eq!(parsed8.0, 8);
        assert_eq!(parsed8.1, 7);
        assert_eq!(parsed8.2, 4);
        assert_eq!(parsed8.3, rec8);
    }

    #[test]
    fn test_apply_dovi_profile5() {
        let mut cfg = VideoConfig {
            codec: VideoCodec::Hevc,
            width: 3840,
            height: 2160,
            record: vec![1, 2, 3],
            nal_length_size: 4,
            codec_string: "hvc1.2.4.L153.B0".into(),
            range: VideoRange::Sdr,
            colour: None,
            mastering_display: None,
            content_light: None,
            pixel_aspect: None,
            frame_rate: Some(24.0),
            reorder_depth: 2,
            interlaced: false,
            dovi: None,
        };
        let rec = build_dovi_record(5, 6, true, false, true, 0);
        apply_dovi(&mut cfg, Some(&rec), &[], Framing::LengthPrefixed(4));

        assert_eq!(cfg.codec_string, "dvh1.05.06");
        assert_eq!(cfg.range, VideoRange::Pq);
        assert_eq!(
            cfg.colour,
            Some(Colour {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: false,
            })
        );
        let dovi = cfg.dovi.expect("dovi config");
        assert_eq!(dovi.profile, 5);
        assert_eq!(dovi.level, 6);
        assert_eq!(dovi.compat_id, 0);
        assert!(dovi.use_dvh1);
        assert_eq!(dovi.supplemental_codecs, None);
    }

    #[test]
    fn test_apply_dovi_profile81() {
        let mut cfg = VideoConfig {
            codec: VideoCodec::Hevc,
            width: 3840,
            height: 2160,
            record: vec![1, 2, 3],
            nal_length_size: 4,
            codec_string: "hvc1.2.4.L153.B0".into(),
            range: VideoRange::Pq,
            colour: Some(Colour {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: false,
            }),
            mastering_display: None,
            content_light: None,
            pixel_aspect: None,
            frame_rate: Some(24.0),
            reorder_depth: 2,
            interlaced: false,
            dovi: None,
        };
        let rec = build_dovi_record(8, 6, true, false, true, 1);
        apply_dovi(&mut cfg, Some(&rec), &[], Framing::LengthPrefixed(4));

        assert_eq!(cfg.codec_string, "hvc1.2.4.L153.B0");
        assert_eq!(cfg.range, VideoRange::Pq);
        let dovi = cfg.dovi.expect("dovi config");
        assert_eq!(dovi.profile, 8);
        assert_eq!(dovi.level, 6);
        assert_eq!(dovi.compat_id, 1);
        assert!(!dovi.use_dvh1);
        assert_eq!(dovi.supplemental_codecs.as_deref(), Some("dvh1.08.06/db1p"));
    }

    #[test]
    fn test_apply_dovi_profile84_hlg() {
        let mut cfg = VideoConfig {
            codec: VideoCodec::Hevc,
            width: 3840,
            height: 2160,
            record: vec![1, 2, 3],
            nal_length_size: 4,
            codec_string: "hvc1.2.4.L153.B0".into(),
            range: VideoRange::Sdr,
            colour: None,
            mastering_display: None,
            content_light: None,
            pixel_aspect: None,
            frame_rate: Some(24.0),
            reorder_depth: 2,
            interlaced: false,
            dovi: None,
        };
        let rec = build_dovi_record(8, 6, true, false, true, 4);
        apply_dovi(&mut cfg, Some(&rec), &[], Framing::LengthPrefixed(4));

        assert_eq!(cfg.range, VideoRange::Hlg);
        let dovi = cfg.dovi.expect("dovi config");
        assert_eq!(dovi.profile, 8);
        assert_eq!(dovi.level, 6);
        assert_eq!(dovi.compat_id, 4);
        assert!(!dovi.use_dvh1);
        assert_eq!(dovi.supplemental_codecs.as_deref(), Some("dvh1.08.06/db4h"));
    }

    #[test]
    fn test_apply_dovi_profile7_rewrite_to_81() {
        let mut cfg = VideoConfig {
            codec: VideoCodec::Hevc,
            width: 3840,
            height: 2160,
            record: vec![1, 2, 3],
            nal_length_size: 4,
            codec_string: "hvc1.2.4.L153.B0".into(),
            range: VideoRange::Pq,
            colour: Some(Colour {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: false,
            }),
            mastering_display: None,
            content_light: None,
            pixel_aspect: None,
            frame_rate: Some(24.0),
            reorder_depth: 2,
            interlaced: false,
            dovi: None,
        };
        let rec7 = build_dovi_record(7, 6, true, true, true, 0);
        apply_dovi(&mut cfg, Some(&rec7), &[], Framing::LengthPrefixed(4));

        assert_eq!(cfg.range, VideoRange::Pq);
        let dovi = cfg.dovi.expect("dovi config");
        assert_eq!(dovi.profile, 8); // Profile 7 rewritten to 8.1
        assert_eq!(dovi.level, 6);
        assert_eq!(dovi.compat_id, 1);
        assert!(!dovi.use_dvh1);
        assert_eq!(dovi.supplemental_codecs.as_deref(), Some("dvh1.08.06/db1p"));
        let parsed = parse_dovi_record(&dovi.record).expect("rewritten record parses");
        assert_eq!(parsed.0, 8);
        assert_eq!(parsed.2, 1);
    }

    #[test]
    fn test_eac3_atmos_joc_detection_and_dec3() {
        // Construct E-AC-3 syncframe with Atmos JOC:
        // 0x0B, 0x77
        // strmtyp=0, substreamid=0, frmsiz=383 (frame_size = 768)
        // fscod=0 (48kHz), numblkscod=3 (6 blks), acmod=7 (3/2), lfeon=1
        // bsid=16, dialnorm=31, compre=0, mixmdate=0, infomdate=0
        // addbsie=1, addbsil=1 (2 bytes), b0=0x01 (extension type A), b1=16 (complexity 16)
        let mut frame = vec![0x0B, 0x77];
        let frmsiz: u16 = 383; // 768 bytes total
        frame.push(((frmsiz >> 8) & 0x07) as u8); // strmtyp=0, substreamid=0
        frame.push((frmsiz & 0xFF) as u8);
        frame.push((0 << 6) | (3 << 4) | (7 << 1) | 1); // fscod=0, numblkscod=3, acmod=7, lfeon=1

        let mut bw = BitWriter::new();
        bw.write_bits(16, 5); // bsid = 16
        bw.write_bits(31, 5); // dialnorm = 31
        bw.write_bits(0, 1); // compre = 0
        bw.write_bits(0, 1); // mixmdate = 0
        bw.write_bits(0, 1); // infomdate = 0
        // parse_eac3_joc fields:
        bw.write_bits(1, 1); // addbsie = 1
        bw.write_bits(1, 6); // addbsil = 1 (2 bytes)
        bw.write_bits(0x01, 8); // b0 = 0x01 (flag_ec3_extension_type_a = 1)
        bw.write_bits(16, 8); // b1 = 16 (complexity_index_type_a = 16)
        let bsi_bytes = bw.finish();
        frame.extend_from_slice(&bsi_bytes);
        frame.resize(768, 0);

        let info = parse_eac3_syncframe(&frame).expect("parse eac3 syncframe with joc");
        assert_eq!(info.joc_complexity, Some(16));

        let p = CodecParameters::audio(CodecId::new("eac3"));
        let cfg = audio_config(&p, &frame).expect("eac3 audio config");
        assert_eq!(cfg.atmos_joc, Some(16));
        // Verify dec3 box ends with the 2-byte extension: 0x01, 0x10
        let dec3 = &cfg.config;
        assert!(dec3.len() >= 2);
        assert_eq!(&dec3[dec3.len() - 2..], &[0x01, 0x10]);
    }

    #[test]
    fn test_apply_dovi_recordless_profile5_from_rpu() {
        // Build synthetic RPU NAL for Profile 5
        let mut bw = BitWriter::new();
        bw.write_bits(2, 6); // rpu_type = 2
        bw.write_bits(0, 11); // rpu_format = 0
        bw.write_bits(0, 4); // vdr_rpu_profile = 0
        bw.write_bits(0, 4); // vdr_rpu_level = 0
        bw.write_bits(1, 1); // vdr_seq_info_present = 1
        bw.write_bits(0, 1); // chroma_resampling = 0
        bw.write_bits(0, 2); // coef_data_type = 0
        bw.write_ue(0); // log2_denom = 0
        bw.write_bits(0, 2); // norm = 0
        bw.write_bits(1, 1); // bl_video_full_range = 1
        let payload = bw.finish();

        let mut rbsp = vec![0x19];
        rbsp.extend_from_slice(&payload);
        rbsp.push(0x80);

        let mut nal = vec![62 << 1, 0x01]; // UNSPEC62 NAL header
        nal.extend_from_slice(&rbsp);

        let mut keyframe = Vec::new();
        keyframe.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        keyframe.extend_from_slice(&nal);

        let mut cfg = VideoConfig {
            codec: VideoCodec::Hevc,
            width: 3840,
            height: 2160,
            record: vec![1, 2, 3],
            nal_length_size: 4,
            codec_string: "hvc1.2.4.L153.B0".into(),
            range: VideoRange::Sdr,
            colour: None,
            mastering_display: None,
            content_light: None,
            pixel_aspect: None,
            frame_rate: Some(24.0),
            reorder_depth: 2,
            interlaced: false,
            dovi: None,
        };

        apply_dovi(&mut cfg, None, &keyframe, Framing::LengthPrefixed(4));
        // Idempotent when called a second time with None:
        apply_dovi(&mut cfg, None, &keyframe, Framing::LengthPrefixed(4));
        assert_eq!(cfg.codec_string, "dvh1.05.06");
        assert_eq!(cfg.range, VideoRange::Pq);
        assert_eq!(
            cfg.colour,
            Some(Colour {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: false,
            })
        );
        let dovi = cfg.dovi.expect("synthesized dovi config");
        assert_eq!(dovi.profile, 5);
        assert_eq!(dovi.level, 6);
        assert_eq!(dovi.compat_id, 0);
        assert!(dovi.use_dvh1);
        assert_eq!(dovi.supplemental_codecs, None);
    }

    #[test]
    fn test_apply_dovi_contradicted_profile5_to_81() {
        // Build synthetic RPU NAL for Profile 8: vdr_rpu_profile = 1, rpu_format = 0x700
        let mut bw = BitWriter::new();
        bw.write_bits(2, 6); // rpu_type = 2
        bw.write_bits(0x700, 11); // rpu_format = 0x700 -> profile 8
        bw.write_bits(1, 4); // vdr_rpu_profile = 1
        bw.write_bits(0, 4); // vdr_rpu_level = 0
        bw.write_bits(1, 1); // vdr_seq_info_present = 1
        bw.write_bits(0, 1); // chroma_resampling = 0
        bw.write_bits(0, 2); // coef_data_type = 0
        bw.write_ue(0); // log2_denom = 0
        bw.write_bits(0, 2); // norm = 0
        bw.write_bits(0, 1); // bl_video_full_range = 0
        let payload = bw.finish();

        let mut rbsp = vec![0x19];
        rbsp.extend_from_slice(&payload);
        rbsp.push(0x80);

        let mut nal = vec![62 << 1, 0x01]; // UNSPEC62 NAL header
        nal.extend_from_slice(&rbsp);

        let mut keyframe = Vec::new();
        keyframe.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        keyframe.extend_from_slice(&nal);

        // VUI declares BT.2020 PQ, but container claims Profile 5
        let mut cfg = VideoConfig {
            codec: VideoCodec::Hevc,
            width: 3840,
            height: 2160,
            record: vec![1, 2, 3],
            nal_length_size: 4,
            codec_string: "hvc1.2.4.L153.B0".into(),
            range: VideoRange::Pq,
            colour: Some(Colour {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: false,
            }),
            mastering_display: None,
            content_light: None,
            pixel_aspect: None,
            frame_rate: Some(24.0),
            reorder_depth: 2,
            interlaced: false,
            dovi: None,
        };

        let rec5 = build_dovi_record(5, 6, true, false, true, 0);
        apply_dovi(&mut cfg, Some(&rec5), &keyframe, Framing::LengthPrefixed(4));

        // Overridden from Profile 5 to Profile 8.1
        let dovi = cfg.dovi.expect("overridden dovi config");
        assert_eq!(dovi.profile, 8);
        assert_eq!(dovi.compat_id, 1);
        assert!(!dovi.use_dvh1);
        assert_eq!(dovi.supplemental_codecs.as_deref(), Some("dvh1.08.06/db1p"));
    }
}

#[cfg(test)]
mod av1_tests {
    use super::*;
    use oxideav_core::CodecId;

    /// av1C record body from a 1080p30 AV1 test encode: Main, seq_level_idx 8, 8-bit 4:2:0,
    /// with the 12-byte sequence header OBU in configOBUs.
    const AV1C_MKV: &[u8] = &[
        0x81, 0x08, 0x0c, 0x00, 0x0a, 0x0c, 0x02, 0x00, 0x00, 0x42, 0x95, 0x5d, 0xfe, 0x1b, 0x80,
        0x5f, 0x00, 0x08,
    ];

    fn av1_params(extradata: &[u8]) -> CodecParameters {
        let mut params = CodecParameters::video(CodecId::new("av1"));
        params.extradata = extradata.to_vec();
        params.width = Some(1920);
        params.height = Some(1080);
        params
    }

    #[test]
    fn av1c_box_shorter_than_its_fixed_fields_is_no_configuration() {
        for size in 8u8..12 {
            let mut extradata = vec![0, 0, 0, size];
            extradata.extend_from_slice(b"av1C");
            extradata.resize(usize::from(size), 0x81);
            assert_eq!(video_config(&av1_params(&extradata), &[]), None, "box size {size}");
        }
    }

    #[test]
    fn av1c_record_from_matroska_configures_main_8bit() {
        let config = video_config(&av1_params(AV1C_MKV), &[]).expect("AV1 configuration");
        assert_eq!(config.codec, VideoCodec::Av1);
        assert_eq!((config.width, config.height), (1920, 1080));
        assert_eq!(config.record, AV1C_MKV);
        assert_eq!(config.reorder_depth, 0);
        assert_eq!(config.range, VideoRange::Sdr);
        assert_eq!(config.colour, None);
        assert_eq!(config.codec_string, "av01.0.08M.08");
        let record = Av1CodecConfig::parse(&config.record).expect("record parses");
        assert_eq!((record.seq_profile, record.seq_level_idx, record.seq_tier, record.bit_depth()), (0, 8, 0, 8));
        assert!(record.chroma_subsampling_x && record.chroma_subsampling_y && !record.monochrome);
        assert_eq!(record.config_obus, &AV1C_MKV[4..]);
    }

    #[test]
    fn av1c_is_built_from_the_first_keyframes_sequence_header() {
        let config = video_config(&av1_params(&[]), &AV1C_MKV[4..]).expect("AV1 configuration");
        assert_eq!((config.width, config.height), (1920, 1080));
        assert_eq!(&config.record[..4], &[0x81, 0x08, 0x0c, 0x00]);
        assert_eq!(&config.record[4..], &AV1C_MKV[4..]);
    }

    #[test]
    fn a_full_av1c_box_in_the_extradata_is_unwrapped() {
        let mut boxed = Vec::new();
        boxed.extend_from_slice(&((AV1C_MKV.len() + 8) as u32).to_be_bytes());
        boxed.extend_from_slice(b"av1C");
        boxed.extend_from_slice(AV1C_MKV);
        let config = video_config(&av1_params(&boxed), &[]).expect("AV1 configuration");
        assert_eq!(config.record, AV1C_MKV);
    }

    #[test]
    fn av1_without_any_sequence_header_has_no_configuration() {
        assert!(video_config(&av1_params(&[0x81, 0x08, 0x0c, 0x00]), &[]).is_none());
    }

    #[test]
    fn av1_samples_lose_a_leading_temporal_delimiter_only() {
        let frame = [0x32, 0x02, 0xAA, 0xBB];
        let mut with_td = vec![0x12, 0x00];
        with_td.extend_from_slice(&frame);
        assert_eq!(without_temporal_delimiter(&with_td), frame);
        assert_eq!(without_temporal_delimiter(&frame), frame);
        // Without a size field the delimiter is its header alone.
        let mut no_size = vec![0x10];
        no_size.extend_from_slice(&frame);
        assert_eq!(without_temporal_delimiter(&no_size), frame);
        // A truncated delimiter is left as it is.
        assert_eq!(without_temporal_delimiter(&[0x12]), [0x12]);
    }

    #[test]
    fn av1_metadata_obus_become_mdcv_and_clli() {
        // BT.2020 primaries, D65 white, 1000 / 0.005 cd/m², in AV1's units.
        let mut av1_mdcv = Vec::new();
        for v in [46_400u16, 19_136, 11_141, 52_232, 8_585, 3_015, 20_493, 21_561] {
            av1_mdcv.extend_from_slice(&v.to_be_bytes());
        }
        av1_mdcv.extend_from_slice(&(1000u32 << 8).to_be_bytes());
        av1_mdcv.extend_from_slice(&82u32.to_be_bytes());
        let mut data = vec![0x2A, 0x1A, 0x02];
        data.extend_from_slice(&av1_mdcv);
        data.push(0x80);
        data.extend_from_slice(&[0x2A, 0x06, 0x01, 0x03, 0xE8, 0x01, 0x90, 0x80]);
        let (mastering, light) = hdr_metadata(&data);
        // The same display in `mdcv` units and order: G, B, R, white point, max 1000 cd/m² =
        // 10,000,000, min 0.005 cd/m² = 50.
        let mut mdcv = Vec::new();
        for v in [8_500u16, 39_850, 6_550, 2_300, 35_400, 14_600, 15_635, 16_450] {
            mdcv.extend_from_slice(&v.to_be_bytes());
        }
        mdcv.extend_from_slice(&10_000_000u32.to_be_bytes());
        mdcv.extend_from_slice(&50u32.to_be_bytes());
        assert_eq!(mastering, Some(<[u8; 24]>::try_from(&mdcv[..]).unwrap()));
        assert_eq!(light, Some([0x03, 0xE8, 0x01, 0x90]));
    }
}
