//! Fragmented MP4 (CMAF) for the native route: the init segment and one
//! `moof` + `mdat` per media segment.

use super::types::{AudioCodec, AudioConfig, VideoCodec, VideoConfig};

pub(crate) enum TrackKind<'a> {
    Video(&'a VideoConfig),
    Audio(&'a AudioConfig),
}

pub(crate) struct Track<'a> {
    pub id: u32,
    pub timescale: u32,
    pub kind: TrackKind<'a>,
}

pub(crate) struct Sample {
    pub data: Vec<u8>,
    pub duration: u32,
    pub composition_offset: i32,
    pub sync: bool,
}

pub(crate) struct Run<'a> {
    pub track_id: u32,
    pub base_decode_time: u64,
    pub samples: &'a [Sample],
}

/// ftyp + moov for these tracks (ids unique, >= 1), in this order.
pub(crate) fn init_segment(tracks: &[Track]) -> Vec<u8> {
    let ftyp = build_ftyp();
    let next_track_id = tracks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
    let mvhd = build_mvhd(next_track_id);

    let mut traks = Vec::new();
    let mut trex_boxes = Vec::new();

    for track in tracks {
        let (is_audio, width_16_16, height_16_16, language_code, sample_entry) = match &track.kind {
            TrackKind::Video(vc) => {
                let display_width = match vc.pixel_aspect {
                    Some((h, v)) if v > 0 => ((vc.width as u64 * h as u64) / v as u64) as u32,
                    _ => vc.width,
                };
                (
                    false,
                    display_width << 16,
                    vc.height << 16,
                    pack_language(None),
                    build_video_sample_entry(vc),
                )
            }
            TrackKind::Audio(ac) => (
                true,
                0,
                0,
                pack_language(ac.language.as_deref()),
                build_audio_sample_entry(ac),
            ),
        };

        let tkhd = build_tkhd(track.id, is_audio, width_16_16, height_16_16);
        let mdia = build_mdia(track.timescale, language_code, is_audio, &sample_entry);
        let mut trak_body = Vec::with_capacity(tkhd.len() + mdia.len());
        trak_body.extend_from_slice(&tkhd);
        trak_body.extend_from_slice(&mdia);
        traks.extend_from_slice(&wrap_box(b"trak", &trak_body));

        trex_boxes.extend_from_slice(&build_trex(track.id));
    }

    let mvex = wrap_box(b"mvex", &trex_boxes);

    let mut moov_body = Vec::with_capacity(mvhd.len() + traks.len() + mvex.len());
    moov_body.extend_from_slice(&mvhd);
    moov_body.extend_from_slice(&traks);
    moov_body.extend_from_slice(&mvex);
    let moov = wrap_box(b"moov", &moov_body);

    let mut out = Vec::with_capacity(ftyp.len() + moov.len());
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out
}

/// One moof (mfhd with `sequence_number`; one traf per non-empty run, in order)
/// followed by one mdat with every run's sample bytes in run order.
pub(crate) fn media_segment(sequence_number: u32, runs: &[Run]) -> Vec<u8> {
    let non_empty_runs: Vec<&Run> = runs.iter().filter(|r| !r.samples.is_empty()).collect();

    let moof_body_size: usize = 16
        + non_empty_runs
            .iter()
            .map(|r| 64 + r.samples.len() * 16)
            .sum::<usize>();
    let moof_box_size = 8 + moof_body_size;

    let total_sample_bytes: u64 = non_empty_runs
        .iter()
        .flat_map(|r| r.samples.iter())
        .map(|s| s.data.len() as u64)
        .sum();

    let (mdat_hdr, mdat_hdr_len) = mdat_header(total_sample_bytes);
    let mdat_box_size: u64 = if total_sample_bytes.saturating_add(8) > u32::MAX as u64 {
        total_sample_bytes.saturating_add(16)
    } else {
        total_sample_bytes.saturating_add(8)
    };

    let total_segment_len = moof_box_size
        .checked_add(mdat_box_size as usize)
        .expect("segment size overflow");

    let mut out = Vec::with_capacity(total_segment_len);

    // --- moof header ---
    out.extend_from_slice(&(moof_box_size as u32).to_be_bytes());
    out.extend_from_slice(b"moof");

    // --- mfhd (FullBox v0, flags 0) ---
    out.extend_from_slice(&16u32.to_be_bytes());
    out.extend_from_slice(b"mfhd");
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&sequence_number.to_be_bytes());

    // --- trafs ---
    let mut current_sample_offset = (moof_box_size + mdat_hdr_len) as i32;
    for run in &non_empty_runs {
        let traf_box_size = (64 + run.samples.len() * 16) as u32;
        out.extend_from_slice(&traf_box_size.to_be_bytes());
        out.extend_from_slice(b"traf");

        // tfhd (FullBox v0, flags 0x020000)
        out.extend_from_slice(&16u32.to_be_bytes());
        out.extend_from_slice(b"tfhd");
        out.extend_from_slice(&[0, 0x02, 0x00, 0x00]);
        out.extend_from_slice(&run.track_id.to_be_bytes());

        // tfdt (FullBox v1, flags 0)
        out.extend_from_slice(&20u32.to_be_bytes());
        out.extend_from_slice(b"tfdt");
        out.extend_from_slice(&[1, 0, 0, 0]);
        out.extend_from_slice(&run.base_decode_time.to_be_bytes());

        // trun (FullBox v1, flags 0x000F01)
        let trun_box_size = (20 + run.samples.len() * 16) as u32;
        out.extend_from_slice(&trun_box_size.to_be_bytes());
        out.extend_from_slice(b"trun");
        out.extend_from_slice(&[1, 0x00, 0x0F, 0x01]);
        out.extend_from_slice(&(run.samples.len() as u32).to_be_bytes());
        out.extend_from_slice(&current_sample_offset.to_be_bytes());

        for sample in run.samples {
            out.extend_from_slice(&sample.duration.to_be_bytes());
            out.extend_from_slice(&(sample.data.len() as u32).to_be_bytes());
            let flags: u32 = if sample.sync { 0x0200_0000 } else { 0x0101_0000 };
            out.extend_from_slice(&flags.to_be_bytes());
            out.extend_from_slice(&sample.composition_offset.to_be_bytes());
        }

        let run_sample_bytes: usize = run.samples.iter().map(|s| s.data.len()).sum();
        current_sample_offset += run_sample_bytes as i32;
    }

    // --- mdat header ---
    out.extend_from_slice(&mdat_hdr[..mdat_hdr_len]);

    // --- mdat payload ---
    for run in &non_empty_runs {
        for sample in run.samples {
            out.extend_from_slice(&sample.data);
        }
    }

    debug_assert_eq!(out.len(), total_segment_len);
    out
}

/// Returns the mdat box header bytes and the header length (8 or 16 bytes).
/// If `payload_len + 8 > u32::MAX`, uses the 16-byte largesize form with size = 1.
pub(crate) fn mdat_header(payload_len: u64) -> ([u8; 16], usize) {
    if payload_len.saturating_add(8) > u32::MAX as u64 {
        let total_box_size = payload_len.saturating_add(16);
        let mut hdr = [0u8; 16];
        hdr[0..4].copy_from_slice(&1u32.to_be_bytes());
        hdr[4..8].copy_from_slice(b"mdat");
        hdr[8..16].copy_from_slice(&total_box_size.to_be_bytes());
        (hdr, 16)
    } else {
        let total_box_size = (payload_len + 8) as u32;
        let mut hdr = [0u8; 16];
        hdr[0..4].copy_from_slice(&total_box_size.to_be_bytes());
        hdr[4..8].copy_from_slice(b"mdat");
        (hdr, 8)
    }
}

fn wrap_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let size = 8 + payload.len();
    assert!(size <= u32::MAX as usize, "box exceeds u32::MAX");
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(size as u32).to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(payload);
    out
}

fn build_ftyp() -> Vec<u8> {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(b"iso6");
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(b"iso6");
    body.extend_from_slice(b"cmfc");
    body.extend_from_slice(b"iso5");
    body.extend_from_slice(b"mp41");
    wrap_box(b"ftyp", &body)
}

fn build_mvhd(next_track_id: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(96);
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&1000u32.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    body.extend_from_slice(&0x0100u16.to_be_bytes());
    body.extend_from_slice(&[0u8; 2]);
    body.extend_from_slice(&[0u8; 8]);
    let unity_matrix: [u32; 9] = [
        0x0001_0000, 0, 0,
        0, 0x0001_0000, 0,
        0, 0, 0x4000_0000,
    ];
    for m in unity_matrix {
        body.extend_from_slice(&m.to_be_bytes());
    }
    body.extend_from_slice(&[0u8; 24]);
    body.extend_from_slice(&next_track_id.to_be_bytes());
    wrap_box(b"mvhd", &body)
}

fn build_tkhd(track_id: u32, is_audio: bool, width_16_16: u32, height_16_16: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(84);
    body.extend_from_slice(&[0, 0, 0, 3]);
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&track_id.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&[0u8; 8]);
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&0i16.to_be_bytes());
    let volume: u16 = if is_audio { 0x0100 } else { 0 };
    body.extend_from_slice(&volume.to_be_bytes());
    body.extend_from_slice(&[0u8; 2]);
    let unity_matrix: [u32; 9] = [
        0x0001_0000, 0, 0,
        0, 0x0001_0000, 0,
        0, 0, 0x4000_0000,
    ];
    for m in unity_matrix {
        body.extend_from_slice(&m.to_be_bytes());
    }
    body.extend_from_slice(&width_16_16.to_be_bytes());
    body.extend_from_slice(&height_16_16.to_be_bytes());
    wrap_box(b"tkhd", &body)
}

fn pack_language(lang: Option<&str>) -> u16 {
    if let Some(s) = lang {
        let b = s.as_bytes();
        if b.len() >= 3 {
            let b0 = b[0].to_ascii_lowercase();
            let b1 = b[1].to_ascii_lowercase();
            let b2 = b[2].to_ascii_lowercase();
            if b0.is_ascii_lowercase() && b1.is_ascii_lowercase() && b2.is_ascii_lowercase() {
                let c0 = (b0 - 0x60) as u16;
                let c1 = (b1 - 0x60) as u16;
                let c2 = (b2 - 0x60) as u16;
                return ((c0 & 0x1F) << 10) | ((c1 & 0x1F) << 5) | (c2 & 0x1F);
            }
        }
    }
    // "und" default: (21 << 10) | (14 << 5) | 4 = 21956
    ((21u16) << 10) | ((14u16) << 5) | 4
}

fn build_mdhd(timescale: u32, language_code: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&timescale.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&language_code.to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes());
    wrap_box(b"mdhd", &body)
}

fn build_hdlr(is_audio: bool) -> Vec<u8> {
    let (handler_type, name) = if is_audio {
        (b"soun", b"SoundHandler\0" as &[u8])
    } else {
        (b"vide", b"VideoHandler\0" as &[u8])
    };
    let mut body = Vec::with_capacity(24 + name.len());
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(handler_type);
    body.extend_from_slice(&[0u8; 12]);
    body.extend_from_slice(name);
    wrap_box(b"hdlr", &body)
}

fn build_vmhd() -> Vec<u8> {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&[0, 0, 0, 1]);
    body.extend_from_slice(&0u16.to_be_bytes());
    body.extend_from_slice(&[0u8; 6]);
    wrap_box(b"vmhd", &body)
}

fn build_smhd() -> Vec<u8> {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&[0u8; 2]);
    wrap_box(b"smhd", &body)
}

fn build_dinf() -> Vec<u8> {
    let url_box = wrap_box(b"url ", &[0, 0, 0, 1]);
    let mut dref_body = Vec::with_capacity(8 + url_box.len());
    dref_body.extend_from_slice(&[0, 0, 0, 0]);
    dref_body.extend_from_slice(&1u32.to_be_bytes());
    dref_body.extend_from_slice(&url_box);
    let dref_box = wrap_box(b"dref", &dref_body);
    wrap_box(b"dinf", &dref_box)
}

fn build_video_sample_entry(config: &VideoConfig) -> Vec<u8> {
    let fourcc = match config.codec {
        VideoCodec::H264 => b"avc1",
        VideoCodec::Hevc if config.dovi.as_ref().is_some_and(|d| d.use_dvh1) => b"dvh1",
        VideoCodec::Hevc => b"hvc1",
    };
    let mut body = Vec::with_capacity(78 + 8 + config.record.len());
    body.extend_from_slice(&[0u8; 6]);
    body.extend_from_slice(&1u16.to_be_bytes());
    body.extend_from_slice(&[0u8; 16]);
    body.extend_from_slice(&(config.width as u16).to_be_bytes());
    body.extend_from_slice(&(config.height as u16).to_be_bytes());
    body.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    body.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&1u16.to_be_bytes());
    body.extend_from_slice(&[0u8; 32]);
    body.extend_from_slice(&0x0018u16.to_be_bytes());
    body.extend_from_slice(&0xFFFFu16.to_be_bytes());

    let record_fourcc = match config.codec {
        VideoCodec::H264 => b"avcC",
        VideoCodec::Hevc => b"hvcC",
    };
    body.extend_from_slice(&wrap_box(record_fourcc, &config.record));

    if let Some(dovi) = &config.dovi {
        let dovi_fourcc = if dovi.profile <= 7 { b"dvcC" } else { b"dvvC" };
        body.extend_from_slice(&wrap_box(dovi_fourcc, &dovi.record));
    }

    if let Some(colour) = config.colour {
        let mut colr_body = Vec::with_capacity(11);
        colr_body.extend_from_slice(b"nclx");
        colr_body.extend_from_slice(&colour.primaries.to_be_bytes());
        colr_body.extend_from_slice(&colour.transfer.to_be_bytes());
        colr_body.extend_from_slice(&colour.matrix.to_be_bytes());
        colr_body.push(if colour.full_range { 0x80 } else { 0x00 });
        body.extend_from_slice(&wrap_box(b"colr", &colr_body));
    }

    if let Some((h, v)) = config.pixel_aspect {
        let mut pasp_body = Vec::with_capacity(8);
        pasp_body.extend_from_slice(&h.to_be_bytes());
        pasp_body.extend_from_slice(&v.to_be_bytes());
        body.extend_from_slice(&wrap_box(b"pasp", &pasp_body));
    }

    if let Some(mdcv) = &config.mastering_display {
        body.extend_from_slice(&wrap_box(b"mdcv", mdcv));
    }

    if let Some(clli) = &config.content_light {
        body.extend_from_slice(&wrap_box(b"clli", clli));
    }

    wrap_box(fourcc, &body)
}

fn build_audio_sample_entry(config: &AudioConfig) -> Vec<u8> {
    let (fourcc, config_fourcc) = match config.codec {
        AudioCodec::Aac => (b"mp4a", b"esds"),
        AudioCodec::Ac3 => (b"ac-3", b"dac3"),
        AudioCodec::Eac3 => (b"ec-3", b"dec3"),
        AudioCodec::Flac => (b"fLaC", b"dfLa"),
        AudioCodec::Alac => (b"alac", b"alac"),
    };
    let mut body = Vec::with_capacity(28 + 8 + config.config.len());
    body.extend_from_slice(&[0u8; 6]);
    body.extend_from_slice(&1u16.to_be_bytes());
    body.extend_from_slice(&[0u8; 8]);
    body.extend_from_slice(&config.channels.to_be_bytes());
    body.extend_from_slice(&config.bits.to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes());
    let samplerate_field: u32 = if config.sample_rate <= 65535 {
        config.sample_rate << 16
    } else {
        0
    };
    body.extend_from_slice(&samplerate_field.to_be_bytes());
    body.extend_from_slice(&wrap_box(config_fourcc, &config.config));

    wrap_box(fourcc, &body)
}

fn build_stsd(sample_entry: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + sample_entry.len());
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(sample_entry);
    wrap_box(b"stsd", &body)
}

fn build_stbl(sample_entry: &[u8]) -> Vec<u8> {
    let stsd = build_stsd(sample_entry);
    let stts = wrap_box(b"stts", &[0, 0, 0, 0, 0, 0, 0, 0]);
    let stsc = wrap_box(b"stsc", &[0, 0, 0, 0, 0, 0, 0, 0]);
    let stsz = wrap_box(b"stsz", &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let stco = wrap_box(b"stco", &[0, 0, 0, 0, 0, 0, 0, 0]);

    let mut body = Vec::with_capacity(stsd.len() + stts.len() + stsc.len() + stsz.len() + stco.len());
    body.extend_from_slice(&stsd);
    body.extend_from_slice(&stts);
    body.extend_from_slice(&stsc);
    body.extend_from_slice(&stsz);
    body.extend_from_slice(&stco);
    wrap_box(b"stbl", &body)
}

fn build_minf(is_audio: bool, sample_entry: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    if is_audio {
        body.extend_from_slice(&build_smhd());
    } else {
        body.extend_from_slice(&build_vmhd());
    }
    body.extend_from_slice(&build_dinf());
    body.extend_from_slice(&build_stbl(sample_entry));
    wrap_box(b"minf", &body)
}

fn build_mdia(timescale: u32, language_code: u16, is_audio: bool, sample_entry: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&build_mdhd(timescale, language_code));
    body.extend_from_slice(&build_hdlr(is_audio));
    body.extend_from_slice(&build_minf(is_audio, sample_entry));
    wrap_box(b"mdia", &body)
}

fn build_trex(track_id: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(&track_id.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    wrap_box(b"trex", &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::{Colour, VideoRange};
    use std::io::Cursor;
    use oxideav_core::{CodecId, CodecRegistry};

    #[test]
    fn test_round_trip_init_and_two_media_segments() {
        let video_config = VideoConfig {
            codec: VideoCodec::H264,
            width: 1280,
            height: 720,
            record: b"test-avcC-payload-header".to_vec(),
            nal_length_size: 4,
            codec_string: "avc1.64001f".to_string(),
            range: VideoRange::Sdr,
            colour: Some(Colour {
                primaries: 1,
                transfer: 1,
                matrix: 1,
                full_range: false,
            }),
            mastering_display: None,
            content_light: None,
            pixel_aspect: None,
            frame_rate: Some(23.976),
            reorder_depth: 2,
            interlaced: false,
            dovi: None,
        };

        let audio_config = AudioConfig {
            codec: AudioCodec::Ac3,
            sample_rate: 48000,
            channels: 6,
            bits: 16,
            config: b"test-dac3-payload".to_vec(),
            codec_string: "ac-3".to_string(),
            frame_samples: 1536,
            adts: false,
            language: Some("eng".to_string()),
            atmos_joc: None,
        };

        let tracks = [
            Track {
                id: 1,
                timescale: 90000,
                kind: TrackKind::Video(&video_config),
            },
            Track {
                id: 2,
                timescale: 48000,
                kind: TrackKind::Audio(&audio_config),
            },
        ];

        let init = init_segment(&tracks);
        assert!(!init.is_empty());

        // Segment 0:
        // Video: 3 samples with B-frame composition offsets (positive & negative), sync mix
        let v_samples_seg0 = [
            Sample {
                data: b"video_seg0_s0_keyframe".to_vec(),
                duration: 3754,
                composition_offset: 7508,
                sync: true,
            },
            Sample {
                data: b"video_seg0_s1_pframe".to_vec(),
                duration: 3754,
                composition_offset: 7508,
                sync: false,
            },
            Sample {
                data: b"video_seg0_s2_bframe_neg".to_vec(),
                duration: 3754,
                composition_offset: -3754,
                sync: false,
            },
        ];

        // Audio: 2 samples
        let a_samples_seg0 = [
            Sample {
                data: b"audio_seg0_s0".to_vec(),
                duration: 1536,
                composition_offset: 0,
                sync: true,
            },
            Sample {
                data: b"audio_seg0_s1".to_vec(),
                duration: 1536,
                composition_offset: 0,
                sync: true,
            },
        ];

        // Empty run (should be ignored and not emit traf)
        let empty_samples: [Sample; 0] = [];

        let runs_seg0 = [
            Run {
                track_id: 1,
                base_decode_time: 0,
                samples: &v_samples_seg0,
            },
            Run {
                track_id: 2,
                base_decode_time: 0,
                samples: &a_samples_seg0,
            },
            Run {
                track_id: 1,
                base_decode_time: 999999,
                samples: &empty_samples,
            },
        ];

        let seg0 = media_segment(1, &runs_seg0);

        // Segment 1:
        let v_samples_seg1 = [
            Sample {
                data: b"video_seg1_s0_pframe".to_vec(),
                duration: 3754,
                composition_offset: 3754,
                sync: false,
            },
            Sample {
                data: b"video_seg1_s1_bframe".to_vec(),
                duration: 3754,
                composition_offset: -200,
                sync: false,
            },
        ];

        let a_samples_seg1 = [
            Sample {
                data: b"audio_seg1_s0".to_vec(),
                duration: 1536,
                composition_offset: 0,
                sync: true,
            },
        ];

        let runs_seg1 = [
            Run {
                track_id: 1,
                base_decode_time: 3754 * 3, // 11262
                samples: &v_samples_seg1,
            },
            Run {
                track_id: 2,
                base_decode_time: 1536 * 2, // 3072
                samples: &a_samples_seg1,
            },
        ];

        let seg1 = media_segment(2, &runs_seg1);

        // Concatenate init + seg0 + seg1
        let mut full = Vec::with_capacity(init.len() + seg0.len() + seg1.len());
        full.extend_from_slice(&init);
        full.extend_from_slice(&seg0);
        full.extend_from_slice(&seg1);

        // Demux with oxideav_mp4
        let registry = CodecRegistry::new();
        let mut demuxer = oxideav_mp4::demux::open(Box::new(Cursor::new(full)), &registry)
            .expect("oxideav_mp4 demuxer open");

        let streams = demuxer.streams();
        assert_eq!(streams.len(), 2, "must have 2 streams");

        // Verify video stream
        let v_stream = &streams[0];
        assert_eq!(v_stream.params.codec_id, CodecId::new("h264"));
        assert_eq!(v_stream.params.width, Some(1280));
        assert_eq!(v_stream.params.height, Some(720));
        assert_eq!(v_stream.params.extradata, video_config.record);

        // Verify audio stream
        let a_stream = &streams[1];
        assert_eq!(a_stream.params.codec_id, CodecId::new("ac3"));
        assert_eq!(a_stream.params.sample_rate, Some(48000));
        assert_eq!(a_stream.params.channels, Some(6));
        assert_eq!(a_stream.params.extradata, audio_config.config);

        // Collect all packets from demuxer
        let mut packets = Vec::new();
        loop {
            match demuxer.next_packet() {
                Ok(pkt) => packets.push(pkt),
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => panic!("demuxer next_packet error: {e}"),
            }
        }

        // Expected packets:
        // Seg 0: 3 video packets, then 2 audio packets (written in run order into mdat)
        // Seg 1: 2 video packets, then 1 audio packet
        // Total = 8 packets
        assert_eq!(packets.len(), 8, "expected exactly 8 packets");

        struct ExpectedPkt {
            stream_index: u32,
            data: &'static [u8],
            dts: i64,
            pts: i64,
            keyframe: bool,
        }

        let expected = [
            // Seg 0 Video
            ExpectedPkt {
                stream_index: 0,
                data: b"video_seg0_s0_keyframe",
                dts: 0,
                pts: 7508,
                keyframe: true,
            },
            ExpectedPkt {
                stream_index: 0,
                data: b"video_seg0_s1_pframe",
                dts: 3754,
                pts: 3754 + 7508, // 11262
                keyframe: false,
            },
            ExpectedPkt {
                stream_index: 0,
                data: b"video_seg0_s2_bframe_neg",
                dts: 3754 * 2, // 7508
                pts: 7508 - 3754, // 3754
                keyframe: false,
            },
            // Seg 0 Audio
            ExpectedPkt {
                stream_index: 1,
                data: b"audio_seg0_s0",
                dts: 0,
                pts: 0,
                keyframe: true,
            },
            ExpectedPkt {
                stream_index: 1,
                data: b"audio_seg0_s1",
                dts: 1536,
                pts: 1536,
                keyframe: true,
            },
            // Seg 1 Video
            ExpectedPkt {
                stream_index: 0,
                data: b"video_seg1_s0_pframe",
                dts: 11262,
                pts: 11262 + 3754, // 15016
                keyframe: false,
            },
            ExpectedPkt {
                stream_index: 0,
                data: b"video_seg1_s1_bframe",
                dts: 11262 + 3754, // 15016
                pts: 15016 - 200,  // 14816
                keyframe: false,
            },
            // Seg 1 Audio
            ExpectedPkt {
                stream_index: 1,
                data: b"audio_seg1_s0",
                dts: 3072,
                pts: 3072,
                keyframe: true,
            },
        ];

        for (i, (pkt, exp)) in packets.iter().zip(expected.iter()).enumerate() {
            assert_eq!(pkt.stream_index, exp.stream_index, "pkt {i} stream_index");
            assert_eq!(&pkt.data[..], exp.data, "pkt {i} data");
            assert_eq!(pkt.dts, Some(exp.dts), "pkt {i} dts");
            assert_eq!(pkt.pts, Some(exp.pts), "pkt {i} pts");
            assert_eq!(pkt.flags.keyframe, exp.keyframe, "pkt {i} keyframe");
        }
    }

    #[test]
    fn test_byte_level_trun_data_offsets_with_two_runs() {
        let s0 = Sample {
            data: vec![0x11; 14],
            duration: 1000,
            composition_offset: 0,
            sync: true,
        };
        let s1 = Sample {
            data: vec![0x22; 26],
            duration: 1000,
            composition_offset: 0,
            sync: false,
        };
        let s2 = Sample {
            data: vec![0x33; 18],
            duration: 500,
            composition_offset: 0,
            sync: true,
        };
        let s3 = Sample {
            data: vec![0x44; 32],
            duration: 500,
            composition_offset: 0,
            sync: true,
        };

        let run0_samples = [s0, s1];
        let run1_samples = [s2, s3];

        let runs = [
            Run {
                track_id: 1,
                base_decode_time: 0,
                samples: &run0_samples,
            },
            Run {
                track_id: 2,
                base_decode_time: 0,
                samples: &run1_samples,
            },
        ];

        let seg = media_segment(42, &runs);

        // Moof size is the first 4 bytes
        let moof_size = u32::from_be_bytes([seg[0], seg[1], seg[2], seg[3]]) as usize;
        assert_eq!(&seg[4..8], b"moof");

        // mdat starts at moof_size
        let mdat_start = moof_size;
        let mdat_size = u32::from_be_bytes([
            seg[mdat_start],
            seg[mdat_start + 1],
            seg[mdat_start + 2],
            seg[mdat_start + 3],
        ]) as usize;
        assert_eq!(&seg[mdat_start + 4..mdat_start + 8], b"mdat");
        let mdat_header_len = 8;
        assert_eq!(mdat_size, mdat_header_len + 14 + 26 + 18 + 32);

        // Search for both trun boxes and verify their data_offset field
        let mut cur = 8; // skip moof header
        let mut trun_offsets = Vec::new();
        while cur < moof_size {
            let box_size = u32::from_be_bytes([seg[cur], seg[cur + 1], seg[cur + 2], seg[cur + 3]]) as usize;
            let fourcc = &seg[cur + 4..cur + 8];
            if fourcc == b"traf" {
                // search inside traf
                let mut traf_cur = cur + 8;
                let traf_end = cur + box_size;
                while traf_cur < traf_end {
                    let sub_size = u32::from_be_bytes([
                        seg[traf_cur],
                        seg[traf_cur + 1],
                        seg[traf_cur + 2],
                        seg[traf_cur + 3],
                    ]) as usize;
                    let sub_fourcc = &seg[traf_cur + 4..traf_cur + 8];
                    if sub_fourcc == b"trun" {
                        // In trun v1 with flags 0x000F01:
                        // 0..4: size
                        // 4..8: 'trun'
                        // 8: version (1)
                        // 9..12: flags
                        // 12..16: sample_count
                        // 16..20: data_offset (i32)
                        let data_offset = i32::from_be_bytes([
                            seg[traf_cur + 16],
                            seg[traf_cur + 17],
                            seg[traf_cur + 18],
                            seg[traf_cur + 19],
                        ]);
                        trun_offsets.push(data_offset);
                    }
                    traf_cur += sub_size;
                }
            }
            cur += box_size;
        }

        assert_eq!(trun_offsets.len(), 2, "found exactly 2 trun boxes");

        // Run 0's data offset must point to the first byte in mdat
        let expected_offset_0 = (moof_size + mdat_header_len) as i32;
        assert_eq!(trun_offsets[0], expected_offset_0, "run 0 data offset");
        assert_eq!(
            &seg[trun_offsets[0] as usize..trun_offsets[0] as usize + 14],
            &vec![0x11; 14][..],
            "run 0 sample 0 data verified at data_offset"
        );

        // Run 1's data offset must point after Run 0's samples
        let expected_offset_1 = expected_offset_0 + 14 + 26;
        assert_eq!(trun_offsets[1], expected_offset_1, "run 1 data offset");
        assert_eq!(
            &seg[trun_offsets[1] as usize..trun_offsets[1] as usize + 18],
            &vec![0x33; 18][..],
            "run 1 sample 0 data verified at data_offset"
        );
    }

    #[test]
    fn test_mdat_largesize_decision() {
        // Test standard 8-byte header when payload + 8 <= u32::MAX
        let (hdr_zero, len_zero) = mdat_header(0);
        assert_eq!(len_zero, 8);
        assert_eq!(u32::from_be_bytes([hdr_zero[0], hdr_zero[1], hdr_zero[2], hdr_zero[3]]), 8);
        assert_eq!(&hdr_zero[4..8], b"mdat");

        let (hdr_1k, len_1k) = mdat_header(1000);
        assert_eq!(len_1k, 8);
        assert_eq!(u32::from_be_bytes([hdr_1k[0], hdr_1k[1], hdr_1k[2], hdr_1k[3]]), 1008);
        assert_eq!(&hdr_1k[4..8], b"mdat");

        // Exactly at u32::MAX boundary
        let max_small_payload = (u32::MAX - 8) as u64;
        let (hdr_max_small, len_max_small) = mdat_header(max_small_payload);
        assert_eq!(len_max_small, 8);
        assert_eq!(
            u32::from_be_bytes([hdr_max_small[0], hdr_max_small[1], hdr_max_small[2], hdr_max_small[3]]),
            u32::MAX
        );
        assert_eq!(&hdr_max_small[4..8], b"mdat");

        // One byte over u32::MAX boundary: must switch to 16-byte largesize
        let min_large_payload = (u32::MAX - 7) as u64;
        let (hdr_large, len_large) = mdat_header(min_large_payload);
        assert_eq!(len_large, 16);
        assert_eq!(
            u32::from_be_bytes([hdr_large[0], hdr_large[1], hdr_large[2], hdr_large[3]]),
            1,
            "size must be 1 for largesize"
        );
        assert_eq!(&hdr_large[4..8], b"mdat");
        let largesize = u64::from_be_bytes([
            hdr_large[8], hdr_large[9], hdr_large[10], hdr_large[11],
            hdr_large[12], hdr_large[13], hdr_large[14], hdr_large[15],
        ]);
        assert_eq!(largesize, min_large_payload + 16);

        // Huge payload (10 GB)
        let huge_payload = 10_000_000_000u64;
        let (hdr_huge, len_huge) = mdat_header(huge_payload);
        assert_eq!(len_huge, 16);
        assert_eq!(
            u32::from_be_bytes([hdr_huge[0], hdr_huge[1], hdr_huge[2], hdr_huge[3]]),
            1
        );
        assert_eq!(&hdr_huge[4..8], b"mdat");
        let largesize_huge = u64::from_be_bytes([
            hdr_huge[8], hdr_huge[9], hdr_huge[10], hdr_huge[11],
            hdr_huge[12], hdr_huge[13], hdr_huge[14], hdr_huge[15],
        ]);
        assert_eq!(largesize_huge, huge_payload + 16);
    }

    #[test]
    fn test_video_hevc_and_child_boxes() {
        let video_config = VideoConfig {
            codec: VideoCodec::Hevc,
            width: 1920,
            height: 1080,
            record: b"test-hvcC-payload".to_vec(),
            nal_length_size: 4,
            codec_string: "hvc1.1.6.L93.B0".to_string(),
            range: VideoRange::Pq,
            colour: Some(Colour {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: true,
            }),
            mastering_display: Some([0xAA; 24]),
            content_light: Some([0xBB; 4]),
            pixel_aspect: Some((4, 3)),
            frame_rate: Some(60.0),
            reorder_depth: 3,
            interlaced: false,
            dovi: None,
        };

        let tracks = [Track {
            id: 1,
            timescale: 90000,
            kind: TrackKind::Video(&video_config),
        }];

        let init = init_segment(&tracks);

        // Verify tkhd has display width = 1920 * 4 / 3 = 2560 (in 16.16: 2560 << 16)
        let expected_disp_w = 2560u32 << 16;
        let expected_disp_h = 1080u32 << 16;

        // Find tkhd
        let tkhd_pos = init
            .windows(4)
            .position(|w| w == b"tkhd")
            .expect("tkhd must exist");
        // tkhd width is at body offset 76 (relative to start of box: 4 size + 4 fourcc + 76 = 84)
        let w_bytes = &init[tkhd_pos - 4 + 84..tkhd_pos - 4 + 88];
        let h_bytes = &init[tkhd_pos - 4 + 88..tkhd_pos - 4 + 92];
        assert_eq!(u32::from_be_bytes([w_bytes[0], w_bytes[1], w_bytes[2], w_bytes[3]]), expected_disp_w);
        assert_eq!(u32::from_be_bytes([h_bytes[0], h_bytes[1], h_bytes[2], h_bytes[3]]), expected_disp_h);

        // Verify hvc1 and hvcC
        assert!(init.windows(4).any(|w| w == b"hvc1"), "hvc1 sample entry must exist");
        assert!(init.windows(4).any(|w| w == b"hvcC"), "hvcC record box must exist");

        // Verify colr
        let colr_pos = init.windows(4).position(|w| w == b"colr").expect("colr must exist");
        // nclx starts right after colr header: 4 size + 4 fourcc = 8 bytes
        let colr_body = &init[colr_pos + 4..colr_pos + 4 + 11];
        assert_eq!(&colr_body[0..4], b"nclx");
        assert_eq!(u16::from_be_bytes([colr_body[4], colr_body[5]]), 9);
        assert_eq!(u16::from_be_bytes([colr_body[6], colr_body[7]]), 16);
        assert_eq!(u16::from_be_bytes([colr_body[8], colr_body[9]]), 9);
        assert_eq!(colr_body[10], 0x80);

        // Verify pasp
        let pasp_pos = init.windows(4).position(|w| w == b"pasp").expect("pasp must exist");
        let pasp_body = &init[pasp_pos + 4..pasp_pos + 4 + 8];
        assert_eq!(u32::from_be_bytes([pasp_body[0], pasp_body[1], pasp_body[2], pasp_body[3]]), 4);
        assert_eq!(u32::from_be_bytes([pasp_body[4], pasp_body[5], pasp_body[6], pasp_body[7]]), 3);

        // Verify mdcv and clli
        let mdcv_pos = init.windows(4).position(|w| w == b"mdcv").expect("mdcv must exist");
        assert_eq!(&init[mdcv_pos + 4..mdcv_pos + 4 + 24], &[0xAA; 24]);

        let clli_pos = init.windows(4).position(|w| w == b"clli").expect("clli must exist");
        assert_eq!(&init[clli_pos + 4..clli_pos + 4 + 4], &[0xBB; 4]);
    }

    #[test]
    fn test_audio_aac_and_rate_limits() {
        let aac_config_high_rate = AudioConfig {
            codec: AudioCodec::Aac,
            sample_rate: 96000, // > 65535 -> samplerate field must be 0
            channels: 2,
            bits: 16,
            config: b"test-esds-config".to_vec(),
            codec_string: "mp4a.40.2".to_string(),
            frame_samples: 1024,
            adts: false,
            language: Some("fra".to_string()),
            atmos_joc: None,
        };

        let tracks = [Track {
            id: 1,
            timescale: 96000,
            kind: TrackKind::Audio(&aac_config_high_rate),
        }];

        let init = init_segment(&tracks);
        let mp4a_pos = init.windows(4).position(|w| w == b"mp4a").expect("mp4a must exist");
        // samplerate field is at offset 24 in audio preamble: 4 size + 4 fourcc + 24 = 32
        let sr_field = u32::from_be_bytes([
            init[mp4a_pos - 4 + 32],
            init[mp4a_pos - 4 + 33],
            init[mp4a_pos - 4 + 34],
            init[mp4a_pos - 4 + 35],
        ]);
        assert_eq!(sr_field, 0, "samplerate must be 0 when rate > 65535");

        // Verify esds child
        assert!(init.windows(4).any(|w| w == b"esds"), "esds must exist");

        // Verify language code packed for "fra"
        let mdhd_pos = init.windows(4).position(|w| w == b"mdhd").expect("mdhd must exist");
        // language is at body offset 20 (relative to start of box: 8 header + 20 body = 28)
        let lang = u16::from_be_bytes([init[mdhd_pos - 4 + 28], init[mdhd_pos - 4 + 29]]);
        assert_eq!(lang, pack_language(Some("fra")));
    }

    #[test]
    fn test_audio_flac_and_alac_sample_entries() {
        // FLAC
        let flac_cfg = AudioConfig {
            codec: AudioCodec::Flac,
            sample_rate: 48000,
            channels: 8,
            bits: 32,
            config: b"test-dfla-payload".to_vec(),
            codec_string: "fLaC".to_string(),
            frame_samples: 0,
            adts: false,
            language: None,
            atmos_joc: None,
        };
        let tracks_flac = [Track {
            id: 1,
            timescale: 48000,
            kind: TrackKind::Audio(&flac_cfg),
        }];
        let init_flac = init_segment(&tracks_flac);
        let flac_pos = init_flac.windows(4).position(|w| w == b"fLaC").expect("fLaC box");
        let flac_box_start = flac_pos - 4;
        let channels = u16::from_be_bytes([init_flac[flac_box_start + 24], init_flac[flac_box_start + 25]]);
        let samplesize = u16::from_be_bytes([init_flac[flac_box_start + 26], init_flac[flac_box_start + 27]]);
        let samplerate = u32::from_be_bytes([
            init_flac[flac_box_start + 32],
            init_flac[flac_box_start + 33],
            init_flac[flac_box_start + 34],
            init_flac[flac_box_start + 35],
        ]);
        assert_eq!(channels, 8);
        assert_eq!(samplesize, 32);
        assert_eq!(samplerate, 48000 << 16);

        let dfla_pos = init_flac.windows(4).position(|w| w == b"dfLa").expect("dfLa child box");
        let dfla_size = u32::from_be_bytes([
            init_flac[dfla_pos - 4],
            init_flac[dfla_pos - 3],
            init_flac[dfla_pos - 2],
            init_flac[dfla_pos - 1],
        ]) as usize;
        assert_eq!(&init_flac[dfla_pos + 4..dfla_pos - 4 + dfla_size], b"test-dfla-payload");

        // ALAC
        let alac_cfg = AudioConfig {
            codec: AudioCodec::Alac,
            sample_rate: 48000,
            channels: 6,
            bits: 24,
            config: b"test-alac-payload".to_vec(),
            codec_string: "alac".to_string(),
            frame_samples: 4096,
            adts: false,
            language: None,
            atmos_joc: None,
        };
        let tracks_alac = [Track {
            id: 1,
            timescale: 48000,
            kind: TrackKind::Audio(&alac_cfg),
        }];
        let init_alac = init_segment(&tracks_alac);
        let alac_pos = init_alac.windows(4).position(|w| w == b"alac").expect("alac box");
        let alac_box_start = alac_pos - 4;
        let channels_a = u16::from_be_bytes([init_alac[alac_box_start + 24], init_alac[alac_box_start + 25]]);
        let samplesize_a = u16::from_be_bytes([init_alac[alac_box_start + 26], init_alac[alac_box_start + 27]]);
        let samplerate_a = u32::from_be_bytes([
            init_alac[alac_box_start + 32],
            init_alac[alac_box_start + 33],
            init_alac[alac_box_start + 34],
            init_alac[alac_box_start + 35],
        ]);
        assert_eq!(channels_a, 6);
        assert_eq!(samplesize_a, 24);
        assert_eq!(samplerate_a, 48000 << 16);

        // Child alac box starts after the 36-byte preamble (8 box header + 28 audio preamble)
        let child_alac_pos = init_alac[alac_box_start + 36..]
            .windows(4)
            .position(|w| w == b"alac")
            .expect("child alac box")
            + alac_box_start
            + 36;
        let child_size = u32::from_be_bytes([
            init_alac[child_alac_pos - 4],
            init_alac[child_alac_pos - 3],
            init_alac[child_alac_pos - 2],
            init_alac[child_alac_pos - 1],
        ]) as usize;
        assert_eq!(&init_alac[child_alac_pos + 4..child_alac_pos - 4 + child_size], b"test-alac-payload");
    }

    #[test]
    fn test_video_dovi_profile5_and_profile8_boxes() {
        use crate::native::types::DoviConfig;

        // Profile 5: dvh1 entry + dvcC child box
        let mut p5_rec = [0u8; 24];
        p5_rec[0] = 1;
        let cfg_p5 = VideoConfig {
            codec: VideoCodec::Hevc,
            width: 3840,
            height: 2160,
            record: b"test-hvcC-payload".to_vec(),
            nal_length_size: 4,
            codec_string: "dvh1.05.06".to_string(),
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
            dovi: Some(DoviConfig {
                profile: 5,
                level: 6,
                compat_id: 0,
                record: p5_rec,
                use_dvh1: true,
                supplemental_codecs: None,
            }),
        };

        let init_p5 = init_segment(&[Track {
            id: 1,
            timescale: 90000,
            kind: TrackKind::Video(&cfg_p5),
        }]);

        assert!(init_p5.windows(4).any(|w| w == b"dvh1"), "must use dvh1 sample entry FourCC for Profile 5");
        assert!(init_p5.windows(4).any(|w| w == b"hvcC"), "must contain hvcC record");
        assert!(init_p5.windows(4).any(|w| w == b"dvcC"), "must contain dvcC box for Profile 5");
        assert!(!init_p5.windows(4).any(|w| w == b"dvvC"), "must NOT contain dvvC box for Profile 5");

        // Profile 8.1: hvc1 entry + dvvC child box
        let mut p8_rec = [0u8; 24];
        p8_rec[0] = 1;
        let cfg_p8 = VideoConfig {
            codec: VideoCodec::Hevc,
            width: 3840,
            height: 2160,
            record: b"test-hvcC-payload".to_vec(),
            nal_length_size: 4,
            codec_string: "hvc1.2.4.L153.B0".to_string(),
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
            dovi: Some(DoviConfig {
                profile: 8,
                level: 6,
                compat_id: 1,
                record: p8_rec,
                use_dvh1: false,
                supplemental_codecs: Some("dvh1.08.06/db1p".to_string()),
            }),
        };

        let init_p8 = init_segment(&[Track {
            id: 1,
            timescale: 90000,
            kind: TrackKind::Video(&cfg_p8),
        }]);

        assert!(init_p8.windows(4).any(|w| w == b"hvc1"), "must use hvc1 sample entry FourCC for Profile 8");
        assert!(init_p8.windows(4).any(|w| w == b"hvcC"), "must contain hvcC record");
        assert!(init_p8.windows(4).any(|w| w == b"dvvC"), "must contain dvvC box for Profile 8");
        assert!(!init_p8.windows(4).any(|w| w == b"dvcC"), "must NOT contain dvcC box for Profile 8");
    }
}
