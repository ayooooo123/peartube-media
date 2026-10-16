//! Codec configuration for the native route: sample-entry records, RFC 6381
//! codec strings and per-packet framing for H.264, HEVC, AAC, AC-3 and
//! E-AC-3.

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

/// One video packet as a sample in the record's framing. LengthPrefixed input is returned unchanged.
/// AnnexB input becomes 4-byte length prefixes (the record built from Annex B parameter sets declares 4),
/// dropping empty NAL units and trailing zero bytes.
pub(crate) fn video_sample(data: Vec<u8>, framing: Framing) -> Vec<u8> {
    match framing {
        Framing::LengthPrefixed(_) => data,
        Framing::AnnexB => {
            let mut out = Vec::with_capacity(data.len() + 16);
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
                    let len = (nalu_end - nalu_start) as u32;
                    out.extend_from_slice(&len.to_be_bytes());
                    out.extend_from_slice(&data[nalu_start..nalu_end]);
                }
            }
            out
        }
    }
}

/// None unless codec_id is "h264" or "h265"/"hevc" and an SPS (and PPS) is found in the extradata
/// (avcC/hvcC record or Annex B) or, failing that, in `first_keyframe` (framed per `video_framing`).
pub(crate) fn video_config(params: &CodecParameters, first_keyframe: &[u8]) -> Option<VideoConfig> {
    match params.codec_id.as_str() {
        "h264" => h264_config(params, first_keyframe),
        "h265" | "hevc" => hevc_config(params, first_keyframe),
        _ => None,
    }
}

/// None unless codec_id is "aac", "ac3" or "eac3" and the headers parse.
pub(crate) fn audio_config(params: &CodecParameters, first_packet: &[u8]) -> Option<AudioConfig> {
    match params.codec_id.as_str() {
        "aac" => aac_config(params, first_packet),
        "ac3" => ac3_config(params, first_packet),
        "eac3" => eac3_config(params, first_packet),
        _ => None,
    }
}

/// The packet as a sample: ADTS header (7 bytes, 9 with CRC) stripped when `config.adts`, else unchanged.
pub(crate) fn audio_sample(config: &AudioConfig, mut data: Vec<u8>) -> Vec<u8> {
    if config.adts && data.len() >= 7 && data[0] == 0xFF && (data[1] & 0xF0) == 0xF0 {
        let protection_absent = data[1] & 1;
        let header_len = if protection_absent == 1 { 7 } else { 9 };
        if data.len() >= header_len {
            data.drain(..header_len);
            data
        } else {
            Vec::new()
        }
    } else {
        data
    }
}

/// Samples (at config.sample_rate) this packet carries: AAC frame_samples; AC-3 1536 x syncframes in the packet; E-AC-3 via eac3_frame_samples. 0 if unparseable.
pub(crate) fn audio_packet_samples(config: &AudioConfig, data: &[u8]) -> u32 {
    match config.codec {
        AudioCodec::Aac => {
            if data.is_empty() {
                0
            } else {
                config.frame_samples
            }
        }
        AudioCodec::Ac3 => {
            let mut count = 0u32;
            let mut pos = 0;
            while pos + 7 <= data.len() {
                if data[pos] == 0x0B && data[pos + 1] == 0x77 {
                    let fscod = (data[pos + 4] >> 6) & 3;
                    let frmsizecod = (data[pos + 4] & 0x3F) as usize;
                    if fscod < 3 && frmsizecod <= 37 {
                        let frame_size = FRAME_SIZE_TAB[frmsizecod][fscod as usize] as usize * 2;
                        if frame_size > 0 && pos + frame_size <= data.len() {
                            count += 1;
                            pos += frame_size;
                            continue;
                        }
                    }
                }
                break;
            }
            1536 * count
        }
        AudioCodec::Eac3 => eac3_frame_samples(data).unwrap_or(0),
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

    Some(VideoConfig {
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
    })
}

// ---------------------------------------------------------------------------
// AAC
// ---------------------------------------------------------------------------

const AAC_SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

fn encode_expandable_len(mut len: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.push((len & 0x7F) as u8);
    len >>= 7;
    while len > 0 {
        bytes.push(((len & 0x7F) | 0x80) as u8);
        len >>= 7;
    }
    bytes.reverse();
    let n = bytes.len();
    for i in 0..n - 1 {
        bytes[i] |= 0x80;
    }
    bytes[n - 1] &= 0x7F;
    bytes
}

fn encode_descriptor(tag: u8, payload: &[u8]) -> Vec<u8> {
    let len_bytes = encode_expandable_len(payload.len());
    let mut out = Vec::with_capacity(1 + len_bytes.len() + payload.len());
    out.push(tag);
    out.extend_from_slice(&len_bytes);
    out.extend_from_slice(payload);
    out
}

fn aac_config(params: &CodecParameters, first_packet: &[u8]) -> Option<AudioConfig> {
    let (asc, adts) = if !params.extradata.is_empty() {
        (params.extradata.clone(), false)
    } else if first_packet.len() >= 7 && first_packet[0] == 0xFF && (first_packet[1] & 0xF0) == 0xF0 {
        let profile = (first_packet[2] >> 6) & 3;
        let aot = profile + 1;
        let sfi = (first_packet[2] >> 2) & 0x0F;
        let channel_config = ((first_packet[2] & 1) << 2) | ((first_packet[3] >> 6) & 3);
        let byte0 = (aot << 3) | (sfi >> 1);
        let byte1 = ((sfi & 1) << 7) | (channel_config << 3);
        (vec![byte0, byte1], true)
    } else {
        return None;
    };

    let mut br = BitReader::new(&asc);
    let mut aot = br.read(5)?;
    if aot == 31 {
        aot = 32 + br.read(6)?;
    }
    let signalled_aot = aot;

    let sfi = br.read(4)?;
    let sampling_freq = if sfi == 15 {
        br.read(24)?
    } else {
        *AAC_SAMPLE_RATES.get(sfi as usize)?
    };

    let channel_config = br.read(4)?;

    let core_sample_rate = sampling_freq;
    if aot == 5 || aot == 29 {
        let ext_sfi = br.read(4)?;
        let _ext_freq = if ext_sfi == 15 {
            br.read(24)?
        } else {
            *AAC_SAMPLE_RATES.get(ext_sfi as usize)?
        };
        let mut core_aot = br.read(5)?;
        if core_aot == 31 {
            core_aot = 32 + br.read(6)?;
        }
        if core_aot == 22 {
            let _ = br.read(4);
        }
    }

    let frame_length_flag = br.read(1)? != 0;
    let frame_samples = if frame_length_flag { 960 } else { 1024 };

    let channels = match channel_config {
        1..=6 => channel_config as u16,
        7 => 8,
        _ => params.channels.unwrap_or(2),
    };

    let codec_string = format!("mp4a.40.{}", signalled_aot);

    // Build esds FullBox
    let sl_desc = encode_descriptor(6, &[0x02]);
    let dec_spec_info = encode_descriptor(5, &asc);

    let mut tag4_payload = Vec::with_capacity(13 + dec_spec_info.len());
    tag4_payload.push(0x40); // objectTypeIndication
    tag4_payload.push(0x15); // streamType = AudioStream
    tag4_payload.extend_from_slice(&[0, 0, 0]); // bufferSizeDB = 0
    tag4_payload.extend_from_slice(&[0, 0, 0, 0]); // maxBitrate = 0
    tag4_payload.extend_from_slice(&[0, 0, 0, 0]); // avgBitrate = 0
    tag4_payload.extend_from_slice(&dec_spec_info);
    let dec_config_desc = encode_descriptor(4, &tag4_payload);

    let mut tag3_payload = Vec::with_capacity(3 + dec_config_desc.len() + sl_desc.len());
    tag3_payload.extend_from_slice(&[0x00, 0x00]); // ES_ID = 0
    tag3_payload.push(0x00); // flags = 0
    tag3_payload.extend_from_slice(&dec_config_desc);
    tag3_payload.extend_from_slice(&sl_desc);
    let es_desc = encode_descriptor(3, &tag3_payload);

    let mut esds = Vec::with_capacity(4 + es_desc.len());
    esds.extend_from_slice(&[0, 0, 0, 0]); // version = 0, flags = 0
    esds.extend_from_slice(&es_desc);

    Some(AudioConfig {
        codec: AudioCodec::Aac,
        sample_rate: core_sample_rate,
        channels,
        config: esds,
        codec_string,
        frame_samples,
        adts,
        language: None,
    })
}

// ---------------------------------------------------------------------------
// AC-3
// ---------------------------------------------------------------------------

const AC3_SAMPLE_RATES: [u32; 4] = [48000, 44100, 32000, 0];
const ACMOD_CHANNELS: [u16; 8] = [2, 1, 2, 3, 3, 4, 4, 5];
const FRAME_SIZE_TAB: [[u16; 3]; 38] = [
    [64, 69, 96],
    [64, 70, 96],
    [80, 87, 120],
    [80, 88, 120],
    [96, 104, 144],
    [96, 105, 144],
    [112, 121, 168],
    [112, 122, 168],
    [128, 139, 192],
    [128, 140, 192],
    [160, 174, 240],
    [160, 175, 240],
    [192, 208, 288],
    [192, 209, 288],
    [224, 243, 336],
    [224, 244, 336],
    [256, 278, 384],
    [256, 279, 384],
    [320, 348, 480],
    [320, 349, 480],
    [384, 417, 576],
    [384, 418, 576],
    [448, 487, 672],
    [448, 488, 672],
    [512, 557, 768],
    [512, 558, 768],
    [640, 696, 960],
    [640, 697, 960],
    [768, 835, 1152],
    [768, 836, 1152],
    [896, 975, 1344],
    [896, 976, 1344],
    [1024, 1114, 1536],
    [1024, 1115, 1536],
    [1152, 1253, 1728],
    [1152, 1254, 1728],
    [1280, 1393, 1920],
    [1280, 1394, 1920],
];

fn ac3_config(params: &CodecParameters, first_packet: &[u8]) -> Option<AudioConfig> {
    let syncframe = if first_packet.len() >= 7 && first_packet[0] == 0x0B && first_packet[1] == 0x77 {
        Some(first_packet)
    } else if params.extradata.len() >= 7
        && params.extradata[0] == 0x0B
        && params.extradata[1] == 0x77
    {
        Some(params.extradata.as_slice())
    } else {
        None
    };

    if let Some(buf) = syncframe {
        let fscod = (buf[4] >> 6) & 3;
        if fscod == 3 {
            return None;
        }
        let sample_rate = AC3_SAMPLE_RATES[fscod as usize];
        let frmsizecod = (buf[4] & 0x3F) as usize;
        let bit_rate_code = (frmsizecod >> 1) as u32;

        let bsid = (buf[5] >> 3) & 0x1F;
        let bsmod = (buf[5] & 0x07) as u32;

        let acmod = (buf[6] >> 5) & 7;
        let mut skip = 0;
        if acmod & 1 != 0 && acmod != 1 {
            skip += 2;
        }
        if acmod & 4 != 0 {
            skip += 2;
        }
        if acmod == 2 {
            skip += 2;
        }
        let lfeon = ((buf[6] >> (4 - skip)) & 1) as u32;
        let channels = ACMOD_CHANNELS[acmod as usize] + lfeon as u16;

        let val: u32 = ((fscod as u32 & 0x03) << 22)
            | ((bsid as u32 & 0x1F) << 17)
            | ((bsmod & 0x07) << 14)
            | ((acmod as u32 & 0x07) << 11)
            | ((lfeon & 0x01) << 10)
            | ((bit_rate_code & 0x1F) << 5);
        let dac3 = vec![(val >> 16) as u8, (val >> 8) as u8, val as u8];

        return Some(AudioConfig {
            codec: AudioCodec::Ac3,
            sample_rate,
            channels,
            config: dac3,
            codec_string: "ac-3".into(),
            frame_samples: 1536,
            adts: false,
            language: None,
        });
    }

    if params.extradata.len() == 3 {
        let b0 = params.extradata[0];
        let b1 = params.extradata[1];
        let fscod = (b0 >> 6) & 3;
        if fscod == 3 {
            return None;
        }
        let sample_rate = AC3_SAMPLE_RATES[fscod as usize];
        let acmod = (b1 >> 3) & 7;
        let lfeon = (b1 >> 2) & 1;
        let channels = ACMOD_CHANNELS[acmod as usize] + lfeon as u16;
        return Some(AudioConfig {
            codec: AudioCodec::Ac3,
            sample_rate,
            channels,
            config: params.extradata.clone(),
            codec_string: "ac-3".into(),
            frame_samples: 1536,
            adts: false,
            language: None,
        });
    }

    None
}

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
    } else {
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
    }

    let infomdate = br.read(1).unwrap_or(0) != 0;
    let bsmod = if infomdate {
        (br.read(3).unwrap_or(0)) as u8
    } else {
        0
    };

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
    })
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

        let dec3 = bw.finish();
        let sample_rate = ind_substreams[0].0.sample_rate;

        return Some(AudioConfig {
            codec: AudioCodec::Eac3,
            sample_rate,
            channels: total_channels,
            config: dec3,
            codec_string: "ec-3".into(),
            frame_samples: 0,
            adts: false,
            language: None,
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

    const AAC_EXTRA_MP4: &[u8] = &[0x11, 0x90, 0x56, 0xe5, 0x00];

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
    fn test_video_sample_annexb_to_length_prefixed() {
        // Test with 3-byte and 4-byte start codes, empty NAL unit, and trailing zeros
        let input = vec![
            0, 0, 0, 1, 0x65, 0x88, 0x84, // 4-byte start code, NAL len 3
            0, 0, 1, // empty NAL unit before next start code
            0, 0, 1, 0x68, 0x01, // 3-byte start code, NAL len 2
            0, 0, // trailing zeros
        ];
        let sample = video_sample(input, Framing::AnnexB);
        let expected = vec![
            0, 0, 0, 3, 0x65, 0x88, 0x84, // length prefix 3
            0, 0, 0, 2, 0x68, 0x01,       // length prefix 2
        ];
        assert_eq!(sample, expected);

        // LengthPrefixed is returned unchanged
        let lp_input = vec![0, 0, 0, 2, 0x65, 0x88];
        assert_eq!(video_sample(lp_input.clone(), Framing::LengthPrefixed(4)), lp_input);
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
    fn test_ac3_audio_config_and_samples() {
        let p = CodecParameters::audio(CodecId::new("ac3"));
        let mut pkt = AC3_SYNCFRAME_MKV.to_vec();
        pkt.resize(1536, 0); // 1536 bytes AC-3 syncframe

        let cfg = audio_config(&p, &pkt).expect("ac3 config");
        assert_eq!(cfg.codec, AudioCodec::Ac3);
        assert_eq!(cfg.sample_rate, 48000);
        assert_eq!(cfg.channels, 6);
        assert_eq!(cfg.frame_samples, 1536);
        assert_eq!(cfg.codec_string, "ac-3");
        assert_eq!(cfg.config, vec![0x10, 0x3d, 0xc0]); // matches FFmpeg dac3 exactly!
        assert!(!cfg.adts);

        assert_eq!(audio_packet_samples(&cfg, &pkt), 1536);

        // Multi-syncframe test: 2 syncframes
        let mut multi = pkt.clone();
        multi.extend_from_slice(&pkt);
        assert_eq!(audio_packet_samples(&cfg, &multi), 3072);
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
        assert_eq!(audio_packet_samples(&cfg, &pkt), 1536);

        // None if not starting with E-AC-3 syncframe
        let legacy_ac3 = AC3_SYNCFRAME_MKV.to_vec();
        assert_eq!(eac3_frame_samples(&legacy_ac3), None);
    }

    #[test]
    fn test_aac_audio_config_and_adts_stripping() {
        let mut p = CodecParameters::audio(CodecId::new("aac"));
        p.extradata = AAC_EXTRA_MP4.to_vec();

        let cfg = audio_config(&p, &[]).expect("aac config");
        assert_eq!(cfg.codec, AudioCodec::Aac);
        assert_eq!(cfg.sample_rate, 48000);
        assert_eq!(cfg.channels, 2);
        assert_eq!(cfg.frame_samples, 1024);
        assert_eq!(cfg.codec_string, "mp4a.40.2");
        assert!(!cfg.adts);

        // ADTS test without extradata
        let p_adts = CodecParameters::audio(CodecId::new("aac"));
        // 7-byte ADTS header without CRC: 48 kHz, stereo, LC
        let adts_header_7 = vec![0xFF, 0xF1, 0x4C, 0x80, 0x03, 0x1F, 0xFC];
        let mut adts_pkt = adts_header_7.clone();
        adts_pkt.extend_from_slice(&[0x12, 0x34, 0x56]); // 3-byte payload

        let cfg_adts = audio_config(&p_adts, &adts_pkt).expect("aac adts config");
        assert!(cfg_adts.adts);
        assert_eq!(cfg_adts.sample_rate, 48000);
        assert_eq!(cfg_adts.channels, 2);
        assert_eq!(cfg_adts.frame_samples, 1024);
        assert_eq!(cfg_adts.codec_string, "mp4a.40.2");

        let stripped = audio_sample(&cfg_adts, adts_pkt);
        assert_eq!(stripped, vec![0x12, 0x34, 0x56]);

        // 9-byte ADTS header with CRC (protection_absent == 0)
        let adts_header_9 = vec![0xFF, 0xF0, 0x4C, 0x80, 0x03, 0x1F, 0xFC, 0xAB, 0xCD];
        let mut adts_pkt_9 = adts_header_9.clone();
        adts_pkt_9.extend_from_slice(&[0x78, 0x9A]);
        let stripped_9 = audio_sample(&cfg_adts, adts_pkt_9);
        assert_eq!(stripped_9, vec![0x78, 0x9A]);
    }

    #[test]
    fn test_explicit_he_aac() {
        // Explicit HE-AAC (AOT 5, extension rate 48000, core AOT 2, core rate 24000)
        // Bitstream:
        // AOT 5 (5 bits: 00101)
        // ext_sfi 3 (4 bits: 0011 -> 48000)
        // core_aot 2 (5 bits: 00010)
        // core_sfi 6 (4 bits: 0110 -> 24000)
        // channel_config 2 (4 bits: 0010)
        // frame_length_flag 0 (1 bit: 0 -> 1024)
        // depends 0 (1 bit: 0)
        // ext 0 (1 bit: 0)
        // Total 26 bits.
        // 00101 0011 00010 0110 0010 0 0 0 000000
        // Group into bytes:
        // Byte 0: 00101001 = 0x29
        // Byte 1: 10001001 = 0x89
        // Byte 2: 10001000 = 0x88
        // Byte 3: 00000000 = 0x00
        let mut p = CodecParameters::audio(CodecId::new("aac"));
        p.extradata = vec![0x2B, 0x11, 0x88, 0x00];

        let cfg = audio_config(&p, &[]).expect("explicit he-aac config");
        assert_eq!(cfg.codec_string, "mp4a.40.5");
        assert_eq!(cfg.sample_rate, 24000); // Core rate, not extension rate
        assert_eq!(cfg.channels, 2);
        assert_eq!(cfg.frame_samples, 1024);
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

        let p = CodecParameters::audio(CodecId::new("eac3"));
        let cfg = audio_config(&p, &pkt1).expect("eac3 config");
        assert_eq!(audio_packet_samples(&cfg, &multi), 3072);
        assert_eq!(audio_packet_samples(&cfg, &[]), 0);
        assert_eq!(audio_packet_samples(&cfg, &[0xFF, 0xFF]), 0);
    }

    #[test]
    fn test_unsupported_codecs_return_none() {
        let p_mp3 = CodecParameters::audio(CodecId::new("mp3"));
        assert_eq!(audio_config(&p_mp3, &[0xFF, 0xFB]), None);

        let p_vp9 = CodecParameters::video(CodecId::new("vp9"));
        assert_eq!(video_config(&p_vp9, &[]), None);
    }
}
