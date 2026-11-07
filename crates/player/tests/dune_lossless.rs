//! End-to-end lossless audio, IEC 61937 passthrough, standalone DSD (.dsf/.dff),
//! CUE disc sheet, and 10-bit 4:4:4 video verification gates.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use oxideav_core::ChannelLayout;
use player::{AudioOutputMode, Headless, PcmFormat, Player, PlayerOptions};

fn ffmpeg_raw_pcm(path: &std::path::Path, fmt: &str, codec: &str) -> Vec<u8> {
    let out = std::process::Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", fmt, "-c:a", codec, "-"])
        .output()
        .expect("run pinned ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg failed on {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn player_preserves_full_32bit_integer_and_64bit_float_lsb_bits_end_to_end() {
    // 1. FATE 32-bit WavPack lossless fixture.
    let wv_path = refcheck::fate("wavpack/lossless/32bit_int-partial.wv");
    let wv_ref = ffmpeg_raw_pcm(&wv_path, "s32le", "pcm_s32le");
    let headless_wv = Headless::new();
    let opts = PlayerOptions {
        realtime: false,
        audio_mode: AudioOutputMode::BitPerfectPcm,
        ..PlayerOptions::default()
    };
    let p_wv = Player::open(
        &wv_path.to_string_lossy(),
        headless_wv.clone(),
        Arc::new(codecs::context()),
        opts.clone(),
        |_| {},
    );
    let st_wv = p_wv.wait();
    assert!(st_wv.ended && st_wv.error.is_none(), "32-bit WavPack failed: {st_wv:?}");
    assert_eq!(
        refcheck::md5_hex(&headless_wv.capture().audio[0].exact_pcm),
        refcheck::md5_hex(&wv_ref),
        "FATE 32-bit WavPack MD5 must match FFmpeg s32le"
    );

    // 2. Genuine 32-bit integer WAV & 64-bit float WAV with LSB bit-0 populated
    // (`0x1234_5679`, `0x7FFF_FFFF`, `PI`), where IEEE-754 f32 provably drops precision.
    let dir = std::env::temp_dir().join(format!("peartube-32b-64f-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let raw_s32 = dir.join("in.s32");
    let wav_s32 = dir.join("in_s32.wav");
    let mut s32_expected = Vec::with_capacity(1024 * 2 * 4);
    for i in 0..2048i32 {
        let v = if i % 2 == 0 {
            (0x1234_5600i32.wrapping_add(i << 1)) | 1
        } else {
            (-0x7654_3200i32.wrapping_sub(i << 1)) | 1
        };
        assert_ne!((v as f32) as f64, v as f64, "sample must exceed 24-bit f32 mantissa");
        s32_expected.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(&raw_s32, &s32_expected).unwrap();
    let st = std::process::Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-y", "-f", "s32le", "-ar", "96000", "-ac", "2", "-i"])
        .arg(&raw_s32)
        .args(["-c:a", "pcm_s32le"])
        .arg(&wav_s32)
        .status()
        .unwrap();
    assert!(st.success());

    let headless_32 = Headless::new();
    let p32 = Player::open(
        &wav_s32.to_string_lossy(),
        headless_32.clone(),
        Arc::new(codecs::context()),
        opts.clone(),
        |_| {},
    );
    let st32 = p32.wait();
    assert!(st32.ended && st32.error.is_none(), "{st32:?}");
    let cap32 = headless_32.capture();
    assert_eq!(cap32.audio[0].sample_format, Some(PcmFormat::S32));
    assert_eq!(cap32.audio[0].exact_pcm, s32_expected, "all 32 integer bits must survive Player untouched");

    let raw_f64 = dir.join("in.f64");
    let wav_f64 = dir.join("in_f64.wav");
    let mut f64_expected = Vec::with_capacity(512 * 2 * 8);
    for i in 0..1024 {
        let v = (std::f64::consts::PI * 0.25) + (i as f64) * 1e-12;
        assert_ne!((v as f32) as f64, v);
        f64_expected.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(&raw_f64, &f64_expected).unwrap();
    let st64 = std::process::Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-y", "-f", "f64le", "-ar", "192000", "-ac", "2", "-i"])
        .arg(&raw_f64)
        .args(["-c:a", "pcm_f64le"])
        .arg(&wav_f64)
        .status()
        .unwrap();
    assert!(st64.success());

    let headless_64 = Headless::new();
    let p64 = Player::open(
        &wav_f64.to_string_lossy(),
        headless_64.clone(),
        Arc::new(codecs::context()),
        opts,
        |_| {},
    );
    let st64_res = p64.wait();
    assert!(st64_res.ended && st64_res.error.is_none(), "{st64_res:?}");
    let cap64 = headless_64.capture();
    assert_eq!(cap64.audio[0].sample_format, Some(PcmFormat::F64));
    assert_eq!(cap64.audio[0].exact_pcm, f64_expected, "all 64 IEEE-754 bits must survive Player untouched");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn player_decodes_24bit_96khz_71_dts_hd_master_audio_bit_exact_with_lfe_layout() {
    let path = refcheck::fate("dts/dcadec-suite/xll_71_24_96_768.dtshd");
    let mut reference = ffmpeg_raw_pcm(&path, "s32le", "pcm_s32le");
    // FFmpeg writes 7.1 as FL FR FC LFE BL BR SL SR; the player hands sinks the core's order.
    player::channel_order::reorder_interleaved(
        &mut reference,
        4,
        player::channel_order::from_ffmpeg_order(ChannelLayout::Surround71).unwrap(),
    );

    let headless = Headless::new();
    let opts = PlayerOptions {
        realtime: false,
        audio_mode: AudioOutputMode::BitPerfectPcm,
        ..PlayerOptions::default()
    };
    let p = Player::open(
        &path.to_string_lossy(),
        headless.clone(),
        Arc::new(codecs::context()),
        opts,
        |_| {},
    );
    let state = p.wait();
    assert!(state.ended && state.error.is_none(), "XLL 7.1 96kHz failed: {state:?}");

    let cap = headless.capture();
    let audio = &cap.audio[0];
    assert_eq!(audio.sample_rate, 96_000);
    assert_eq!(audio.channels, 8);
    assert_eq!(audio.channel_layout, Some(ChannelLayout::Surround71));
    assert_eq!(audio.sample_format, Some(PcmFormat::S32));
    assert_eq!(
        refcheck::md5_hex(&audio.exact_pcm),
        refcheck::md5_hex(&reference),
        "DTS-HD MA 7.1 24-bit 96kHz must be bit-identical to FFmpeg"
    );
}

#[test]
fn player_passthrough_truehd_atmos_mat_matches_ffmpeg_spdif_and_roundtrips_losslessly() {
    let path = refcheck::fate("truehd/atmos.thd");
    let expected_spdif = {
        let out = std::process::Command::new(refcheck::pinned_ffmpeg())
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(&path)
            .args(["-map", "0:a:0", "-c:a", "copy", "-f", "spdif", "-"])
            .output()
            .expect("ffmpeg -f spdif");
        assert!(out.status.success());
        out.stdout
    };

    let headless = Headless::new();
    headless.set_passthrough_capture(true);
    let opts = PlayerOptions {
        realtime: false,
        audio_mode: AudioOutputMode::Passthrough,
        ..PlayerOptions::default()
    };
    let p = Player::open(
        &path.to_string_lossy(),
        headless.clone(),
        Arc::new(codecs::context()),
        opts,
        |_| {},
    );
    let state = p.wait();
    assert!(state.ended && state.error.is_none(), "TrueHD passthrough failed: {state:?}");

    let cap = headless.capture();
    let audio = &cap.audio[0];
    assert!(!audio.compressed_packets.is_empty(), "passthrough must capture compressed TrueHD AUs");

    let mut pktzr = player::iec61937::Iec61937Packetizer::new(player::iec61937::Iec61937Codec::TrueHd);
    let mut spdif_bytes = Vec::new();
    for pkt in &audio.compressed_packets {
        pktzr.push_packet(pkt, &mut spdif_bytes).expect("MAT packetize");
    }
    assert_eq!(
        refcheck::md5_hex(&spdif_bytes),
        refcheck::md5_hex(&expected_spdif),
        "Player passthrough TrueHD MAT stream must match FFmpeg -f spdif byte-for-byte"
    );
}

#[test]
fn standalone_dsf_dff_and_cue_sheet_play_and_match_ffmpeg_dsd_oracle() {
    let dir = std::env::temp_dir().join(format!("peartube-dsd-cue-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let dsf_path = dir.join("test_dsd64.dsf");
    let dff_path = dir.join("test_dsd64.dff");
    let cue_path = dir.join("album.cue");

    // Build a valid stereo DSD64 (2,822,400 Hz => 352,800 byte-samples/s) .dsf file:
    // 1 block of 4096 bytes per channel = 8192 bytes payload.
    let per_ch_block = 4096u32;
    let channels = 2u32;
    let dsd_rate = 2_822_400u32;
    let total_bits_per_ch = u64::from(per_ch_block) * 8;
    let data_payload_len = (per_ch_block * channels) as usize;
    let mut payload_lsbf = Vec::with_capacity(data_payload_len);
    for i in 0..data_payload_len {
        // Deterministic SDM pattern around 0x69 silence with modulated tone bits.
        payload_lsbf.push(if (i % 7) < 3 { 0x96 } else { 0x69 });
    }

    let mut dsf = Vec::new();
    dsf.extend_from_slice(b"DSD ");
    dsf.extend_from_slice(&28u64.to_le_bytes());
    dsf.extend_from_slice(&(28u64 + 52 + 12 + data_payload_len as u64).to_le_bytes());
    dsf.extend_from_slice(&0u64.to_le_bytes()); // no ID3
    dsf.extend_from_slice(b"fmt ");
    dsf.extend_from_slice(&52u64.to_le_bytes());
    dsf.extend_from_slice(&1u32.to_le_bytes()); // version 1
    dsf.extend_from_slice(&0u32.to_le_bytes()); // format_id 0 (DSD raw)
    dsf.extend_from_slice(&2u32.to_le_bytes()); // channel_type 2 (stereo)
    dsf.extend_from_slice(&channels.to_le_bytes());
    dsf.extend_from_slice(&dsd_rate.to_le_bytes());
    dsf.extend_from_slice(&1u32.to_le_bytes()); // LSB-first (dsd_lsbf_planar)
    dsf.extend_from_slice(&total_bits_per_ch.to_le_bytes());
    dsf.extend_from_slice(&per_ch_block.to_le_bytes());
    dsf.extend_from_slice(&0u32.to_le_bytes()); // reserved
    dsf.extend_from_slice(b"data");
    dsf.extend_from_slice(&(12u64 + data_payload_len as u64).to_le_bytes());
    dsf.extend_from_slice(&payload_lsbf);
    std::fs::write(&dsf_path, &dsf).expect("write dsf");

    // Build a matching DSDIFF (.dff) stereo DSD64 file (`FRM8 ... DSD `).
    let mut prop_body = Vec::new();
    prop_body.extend_from_slice(b"SND ");
    prop_body.extend_from_slice(b"FS  ");
    prop_body.extend_from_slice(&4u64.to_be_bytes());
    prop_body.extend_from_slice(&dsd_rate.to_be_bytes());
    prop_body.extend_from_slice(b"CHNL");
    prop_body.extend_from_slice(&10u64.to_be_bytes());
    prop_body.extend_from_slice(&2u16.to_be_bytes());
    prop_body.extend_from_slice(b"SLFTSRGT");
    prop_body.extend_from_slice(b"CMPR");
    prop_body.extend_from_slice(&4u64.to_be_bytes());
    prop_body.extend_from_slice(b"DSD ");

    let mut frm8_body = Vec::new();
    frm8_body.extend_from_slice(b"DSD ");
    frm8_body.extend_from_slice(b"FVER");
    frm8_body.extend_from_slice(&4u64.to_be_bytes());
    frm8_body.extend_from_slice(&0x0105_0000u32.to_be_bytes());
    frm8_body.extend_from_slice(b"PROP");
    frm8_body.extend_from_slice(&(prop_body.len() as u64).to_be_bytes());
    frm8_body.extend_from_slice(&prop_body);
    frm8_body.extend_from_slice(b"DSD ");
    frm8_body.extend_from_slice(&(data_payload_len as u64).to_be_bytes());
    frm8_body.extend_from_slice(&payload_lsbf);

    let mut dff = Vec::new();
    dff.extend_from_slice(b"FRM8");
    dff.extend_from_slice(&(frm8_body.len() as u64).to_be_bytes());
    dff.extend_from_slice(&frm8_body);
    std::fs::write(&dff_path, &dff).expect("write dff");

    // Write a companion CUE sheet pointing at `test_dsd64.dsf`.
    let cue_text = "\
PERFORMER \"Reference Ensemble\"
TITLE \"SACD Lossless Check\"
FILE \"test_dsd64.dsf\" WAVE
  TRACK 01 AUDIO
    TITLE \"Opening Movement\"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE \"Coda\"
    INDEX 01 00:00:01
";
    std::fs::write(&cue_path, cue_text).expect("write cue");

    // Play via the `.cue` URL!
    let headless_cue = Headless::new();
    let p_cue = Player::open(
        &cue_path.to_string_lossy(),
        headless_cue.clone(),
        Arc::new(codecs::context()),
        PlayerOptions {
            realtime: false,
            ..PlayerOptions::default()
        },
        |_| {},
    );
    let st_cue = p_cue.wait();
    assert!(st_cue.ended && st_cue.error.is_none(), "CUE->DSF playback failed: {st_cue:?}");
    assert_eq!(st_cue.chapters.len(), 2);
    assert_eq!(st_cue.chapters[0].title.as_deref(), Some("Opening Movement"));
    assert_eq!(st_cue.chapters[1].start, Duration::new(0, 1_000_000_000 / 75));

    let ref_dsf = ffmpeg_raw_pcm(&dsf_path, "f32le", "pcm_f32le");
    let cap_dsf = headless_cue.capture();
    assert_eq!(cap_dsf.audio[0].sample_rate, 352_800);
    assert_eq!(cap_dsf.audio[0].exact_pcm.len(), ref_dsf.len());
    let ref_dsf_f32: Vec<f32> = ref_dsf
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let snr_dsf = refcheck::try_snr_db(&ref_dsf_f32, &cap_dsf.audio[0].pcm, 0).expect("DSF SNR");
    assert!(snr_dsf >= 120.0, "DSF PCM must match FFmpeg dsd2pcm (>=120 dB): got {snr_dsf} dB");

    // Also verify `.dff` DSDIFF matches FFmpeg's `iff` DSD demuxer + `dsd_msbf` decoder!
    let headless_dff = Headless::new();
    let p_dff = Player::open(
        &dff_path.to_string_lossy(),
        headless_dff.clone(),
        Arc::new(codecs::context()),
        PlayerOptions {
            realtime: false,
            ..PlayerOptions::default()
        },
        |_| {},
    );
    let st_dff = p_dff.wait();
    assert!(st_dff.ended && st_dff.error.is_none(), "DFF playback failed: {st_dff:?}");
    let ref_dff = ffmpeg_raw_pcm(&dff_path, "f32le", "pcm_f32le");
    let ref_dff_f32: Vec<f32> = ref_dff
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let cap_dff = headless_dff.capture();
    let snr_dff = refcheck::try_snr_db(&ref_dff_f32, &cap_dff.audio[0].pcm, 0).expect("DFF SNR");
    assert!(snr_dff >= 120.0, "DFF PCM must match FFmpeg dsd2pcm (>=120 dB): got {snr_dff} dB");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stdl_dolby_vision_profile7_fel_splits_and_reconstructs_12bit_via_dual_hevc_decoders() {
    use oxideav_core::{CodecId, CodecParameters, Frame, Packet, TimeBase};
    use player::dovi_rpu::{
        ComponentCurve, DoviElType, ExtBlockRaw, Framing, ParsedRpu, RpuHeader, RpuMapping,
        RpuNlq, VdrDmPayload, reconstruct_fel_frame, split_stdl_fel_access_unit,
    };

    let dir = std::env::temp_dir().join(format!("peartube-fel-hevc-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let bl_path = dir.join("bl_10b.hevc");
    let el_path = dir.join("el_10b.hevc");

    // Encode one 64x64 10-bit HEVC base layer IDR and one 32x32 10-bit HEVC enhancement layer IDR
    // with positive residual luma (Y=768 > 512 neutral offset) using system ffmpeg libx265.
    let st_bl = std::process::Command::new(refcheck::system_ffmpeg())
        .args([
            "-v", "error", "-nostdin", "-y",
            "-f", "lavfi", "-i", "color=c=0x808080:s=64x64:r=24:d=0.05,format=yuv420p10le",
            "-frames:v", "1", "-c:v", "libx265", "-x265-params", "log-level=error:repeat-headers=1",
        ])
        .arg(&bl_path)
        .status()
        .expect("system ffmpeg libx265 BL");
    assert!(st_bl.success());

    let st_el = std::process::Command::new(refcheck::system_ffmpeg())
        .args([
            "-v", "error", "-nostdin", "-y",
            "-f", "lavfi", "-i", "color=c=0xE0E0E0:s=32x32:r=24:d=0.05,format=yuv420p10le",
            "-frames:v", "1", "-c:v", "libx265", "-x265-params", "log-level=error:repeat-headers=1",
        ])
        .arg(&el_path)
        .status()
        .expect("system ffmpeg libx265 EL");
    assert!(st_el.success());

    let bl_raw = std::fs::read(&bl_path).unwrap();
    let el_raw = std::fs::read(&el_path).unwrap();

    // Wrap every EL Annex-B NAL inside `NAL_UNSPEC63` (`0x7E 0x01`) just as MakeMKV / UHD Blu-ray STDL muxes do.
    let split_annexb = |data: &[u8]| -> Vec<Vec<u8>> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 2 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                let p = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
                starts.push((p, i + 3));
                i += 3;
            } else {
                i += 1;
            }
        }
        starts
            .iter()
            .enumerate()
            .map(|(idx, &(_, ns))| {
                let ne = starts.get(idx + 1).map_or(data.len(), |&(p, _)| p);
                data[ns..ne].to_vec()
            })
            .collect()
    };

    let mut stdl_au = bl_raw.clone();
    for el_nal in split_annexb(&el_raw) {
        stdl_au.extend_from_slice(&[0, 0, 0, 1, 0x7E, 0x01]);
        stdl_au.extend_from_slice(&el_nal);
    }

    let fel_rpu = ParsedRpu {
        profile: 7,
        el_type: Some(DoviElType::Fel),
        header: RpuHeader {
            rpu_type: 2,
            rpu_format: 18,
            vdr_rpu_profile: 1,
            vdr_rpu_level: 0,
            vdr_seq_info_present_flag: true,
            chroma_resampling_explicit_filter_flag: false,
            chroma_filter_idc: 0,
            coefficient_data_type: 0,
            coefficient_log2_denom: 23,
            coef_len: 23,
            vdr_rpu_normalized_idc: 1,
            bl_video_full_range_flag: false,
            bl_bit_depth_minus8: 2,
            el_bit_depth_minus8: 2,
            ext_mapping_idc: 0,
            vdr_bit_depth_minus8: 4,
            spatial_resampling_filter_flag: false,
            reserved_zero_3bits: 0,
            el_spatial_resampling_filter_flag: true,
            disable_residual_flag: false,
            vdr_dm_metadata_present_flag: true,
            use_prev_vdr_rpu_flag: false,
            prev_vdr_rpu_id: 0,
        },
        mapping: Some(RpuMapping {
            vdr_rpu_id: 0,
            mapping_color_space: 0,
            mapping_chroma_format_idc: 0,
            num_x_partitions_minus1: 0,
            num_y_partitions_minus1: 0,
            curves: [
                ComponentCurve::identity_p81(),
                ComponentCurve::identity_p81(),
                ComponentCurve::identity_p81(),
            ],
            nlq_pred_pivots: Some([0, 1023]),
            nlq: Some(RpuNlq {
                nlq_offset: [512, 512, 512],
                vdr_in_max_int: [1, 1, 1],
                vdr_in_max_frac: [0, 0, 0],
                dz_slope_int: [0, 0, 0],
                dz_slope_frac: [1 << 22, 1 << 22, 1 << 22], // 0.5 slope
                dz_threshold_int: [0, 0, 0],
                dz_threshold_frac: [0, 0, 0],
            }),
        }),
        dm_data: Some(VdrDmPayload {
            compressed: false,
            affected_dm_metadata_id: 0,
            current_dm_metadata_id: 0,
            scene_refresh_flag: 1,
            ycc_to_rgb_coef: [9574, 0, 13802, 9574, -1540, -5348, 9574, 17610, 0],
            ycc_to_rgb_offset: [16_777_216, 134_217_728, 134_217_728],
            rgb_to_lms_coef: [7222, 8771, 390, 2654, 12430, 1300, 0, 422, 15962],
            signal_eotf: 65535,
            signal_eotf_param0: 0,
            signal_eotf_param1: 0,
            signal_eotf_param2: 0,
            signal_bit_depth: 12,
            signal_color_space: 0,
            signal_chroma_format: 0,
            signal_full_range_flag: 1,
            source_min_pq: 62,
            source_max_pq: 3079,
            source_diagonal: 42,
            cmv29_blocks: vec![ExtBlockRaw {
                length_bytes: 5,
                level: 1,
                payload_bytes: vec![0, 62, 12, 7, 4],
            }],
            cmv40_blocks: None,
            trailing_bits: Vec::new(),
        }),
        crc32: 0,
    };
    stdl_au.extend_from_slice(&[0, 0, 0, 1]);
    stdl_au.extend_from_slice(&fel_rpu.encode_hevc_unspec62_nal());

    let (bl_au, el_au, parsed_rpu) = split_stdl_fel_access_unit(&stdl_au, Framing::AnnexB);
    let el_au = el_au.expect("unwrapped EL Annex-B AU");
    let parsed_rpu = parsed_rpu.expect("parsed FEL RPU");
    assert_eq!(parsed_rpu.profile, 7);
    assert_eq!(parsed_rpu.el_type, Some(DoviElType::Fel));

    let ctx = codecs::context();
    let hevc_params = CodecParameters::video(CodecId::new("hevc"));
    let mut bl_dec = ctx.codecs.first_decoder(&hevc_params).expect("BL HEVC decoder");
    let mut el_dec = ctx.codecs.first_decoder(&hevc_params).expect("EL HEVC decoder");

    let decode_one = |dec: &mut Box<dyn oxideav_core::Decoder>, au: Vec<u8>| {
        let pkt = Packet::new(0, TimeBase::new(1, 24), au);
        dec.send_packet(&pkt).expect("send HEVC AU");
        dec.flush().expect("flush HEVC");
        loop {
            match dec.receive_frame().expect("receive HEVC frame") {
                Frame::Video(vf) => return vf,
                _ => continue,
            }
        }
    };

    let bl_frame = decode_one(&mut bl_dec, bl_au);
    let el_frame = decode_one(&mut el_dec, el_au);
    let composed = reconstruct_fel_frame(&bl_frame, &el_frame, &parsed_rpu, 64, 64, 32, 32)
        .expect("reconstruct 12-bit FEL");

    let bl_10 = u16::from_le_bytes([bl_frame.planes[0].data[0], bl_frame.planes[0].data[1]]) & 0x03FF;
    let fel_12 = u16::from_le_bytes([composed.planes[0].data[0], composed.planes[0].data[1]]) >> 4;
    assert!(
        fel_12 > (bl_10 << 2) + 300,
        "real dual-HEVC 12-bit FEL composition must add bright EL highlight detail above 10-bit BL: fel_12={fel_12}, bl_12_equiv={}",
        bl_10 << 2
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn real_fate_dolby_vision_p7_mkv_p81_mp4_p5_mp4_conformance() {
    use oxideav_core::{Frame, Packet, TimeBase};
    use player::dovi_rpu::{
        Framing, parse_rpu_rbsp, reconstruct_fel_frame, rewrite_p7_packet_to_p81,
        split_stdl_fel_access_unit, unescape_rbsp,
    };

    let p7_mkv = refcheck::fate("mkv/dovi-p7-hvce.mkv");
    let p81_mp4 = refcheck::fate("mov/dovi-p81.mp4");
    let p5_mp4 = refcheck::fate("mov/dovi-p5.mp4");

    // 1. Play all three real FATE Dolby Vision files end-to-end through Player!
    for (label, path, expected_frames) in [
        ("DV Profile 7 MKV", &p7_mkv, 1usize),
        ("DV Profile 8.1 MP4", &p81_mp4, 10usize),
        ("DV Profile 5 MP4", &p5_mp4, 10usize),
    ] {
        let headless = Headless::new();
        let p = Player::open(
            path.to_str().unwrap(),
            headless.clone(),
            Arc::new(codecs::context()),
            PlayerOptions {
                realtime: false,
                ..PlayerOptions::default()
            },
            |_| {},
        );
        let st = p.wait();
        assert!(
            st.ended && st.error.is_none(),
            "{label} playback failed: {st:?}"
        );
        let cap = headless.capture();
        assert_eq!(
            cap.video[0].shown_at.len(),
            expected_frames,
            "{label} frame count mismatch"
        );
    }

    // 2. Open `mkv/dovi-p7-hvce.mkv` directly through our MkvRichDemuxer
    //    to verify real `BlockAdditionMapping` `dvcC` + real Profile 7 RPU -> Profile 8.1
    //    conversion + real dual-HEVC split!
    let ctx = codecs::context();
    let file = std::fs::File::open(&p7_mkv).expect("open dovi-p7-hvce.mkv");
    let (mut dmx, _chapters) =
        player::mkv_meta::open_mkv_enriched(Box::new(file), &ctx.codecs)
            .expect("demux dovi-p7-hvce.mkv via open_mkv_enriched");
    let vstream = dmx.streams()[0].clone();
    let dvc_c = vstream
        .params
        .options
        .get(player::mkv_meta::OPT_CONTAINER_DOVI)
        .expect("MkvRichDemuxer must surface BlockAdditionMapping dvcC on dovi-p7-hvce.mkv");
    assert_eq!(dvc_c.len() / 2, 24, "hex-encoded 24-byte dvcC");

    // Read the 4K STDL packet (length-prefixed hvcC framing).
    let pkt = dmx.next_packet().expect("read first packet");
    let nls = usize::from((vstream.params.extradata[21] & 0x03) + 1);
    let framing = Framing::LengthPrefixed(nls as u8);

    // Convert real commercial Profile 7 AU -> Profile 8.1 AU and verify rewritten RPU CRC & profile!
    let p81_au = rewrite_p7_packet_to_p81(&pkt.data, framing)
        .expect("real Profile 7 MKV packet must rewrite to Profile 8.1");
    assert!(
        p81_au.len() < pkt.data.len(),
        "EL NALs must be stripped in Profile 8.1 output ({} vs {})",
        p81_au.len(),
        pkt.data.len()
    );
    let mut pos = 0usize;
    let mut found_p81_rpu = false;
    while pos + nls <= p81_au.len() {
        let mut len = 0usize;
        for b in &p81_au[pos..pos + nls] {
            len = (len << 8) | usize::from(*b);
        }
        pos += nls;
        let nal = &p81_au[pos..pos + len];
        pos += len;
        let nal_type = (nal[0] >> 1) & 0x3F;
        let layer_id = ((nal[0] & 1) << 5) | (nal[1] >> 3);
        assert_ne!(nal_type, 63, "NAL_UNSPEC63 must be stripped");
        assert_eq!(layer_id, 0, "non-zero nuh_layer_id EL NALs must be stripped");
        if nal_type == 62 {
            let rpu = parse_rpu_rbsp(&unescape_rbsp(&nal[2..]))
                .expect("rewritten Profile 8.1 RPU must pass MPEG-2 CRC-32");
            assert_eq!(rpu.profile, 8);
            assert!(!rpu.header.el_spatial_resampling_filter_flag);
            assert!(rpu.header.disable_residual_flag);
            found_p81_rpu = true;
        }
    }
    assert!(found_p81_rpu, "Profile 8.1 output must contain rewritten NAL_UNSPEC62 RPU");

    // Also split the real STDL AU into Base Layer + Enhancement Layer + RPU!
    let (bl_au, el_au, parsed_rpu) = split_stdl_fel_access_unit(&pkt.data, framing);
    let rpu = parsed_rpu.expect("real FATE dovi-p7-hvce.mkv RPU must parse cleanly");
    assert_eq!(rpu.profile, 7);
    assert!(!bl_au.is_empty(), "BL AU must be non-empty");
    let el_au = el_au.expect("EL AU must be present on dovi-p7-hvce.mkv");
    assert!(!el_au.is_empty(), "EL AU must be non-empty");

    // Decode rewritten Profile 8.1 4K packet through oxideav-h265!
    let mut bl_dec = ctx.codecs.first_decoder(&vstream.params).expect("open BL decoder");
    bl_dec
        .send_packet(&Packet::new(0, TimeBase::new(1, 24000), p81_au))
        .expect("decode real 4K Profile 8.1 AU");
    bl_dec.flush().expect("flush BL");
    let Frame::Video(bl_vf) = bl_dec.receive_frame().expect("receive 4K BL frame") else {
        panic!("expected VideoFrame");
    };
    assert_eq!(bl_vf.planes.len(), 3);
    assert!(
        bl_vf.planes[0].stride >= 3840 * 2 && bl_vf.planes[0].data.len() >= 3840 * 2160 * 2,
        "decoded 4K 10-bit luma plane dimensions: stride={} len={}",
        bl_vf.planes[0].stride,
        bl_vf.planes[0].data.len()
    );
    // Decode the unwrapped 1080p FEL Enhancement Layer (`NAL_UNSPEC63`) through a second
    // `oxideav-h265` instance and reconstruct the full 3840x2160 12-bit FEL frame!
    let el_params = oxideav_core::CodecParameters::video(oxideav_core::CodecId::new("hevc"));
    let mut el_dec = ctx.codecs.first_decoder(&el_params).expect("open EL decoder");
    el_dec
        .send_packet(&Packet::new(0, TimeBase::new(1, 24000), el_au))
        .expect("decode real 1080p FEL EL AU");
    el_dec.flush().expect("flush EL");
    let Frame::Video(el_vf) = el_dec.receive_frame().expect("receive 1080p EL frame") else {
        panic!("expected EL VideoFrame");
    };
    assert_eq!(el_vf.planes.len(), 3);
    assert!(
        el_vf.planes[0].stride >= 1920 * 2 && el_vf.planes[0].data.len() >= 1920 * 1080 * 2,
        "decoded 1080p 10-bit EL luma plane dimensions: stride={} len={}",
        el_vf.planes[0].stride,
        el_vf.planes[0].data.len()
    );

    let fel_4k = reconstruct_fel_frame(&bl_vf, &el_vf, &rpu, 3840, 2160, 1920, 1080)
        .expect("reconstruct 3840x2160 12-bit FEL frame from real FATE dovi-p7-hvce.mkv");
    assert_eq!(fel_4k.planes.len(), 3);
    assert_eq!(fel_4k.planes[0].stride, 3840 * 2);
    assert_eq!(fel_4k.planes[0].data.len(), 3840 * 2160 * 2);
}

#[test]
fn player_live_prefer_fel_12bit_and_steep64_dsd_surpass_ffmpeg() {
    use oxideav_core::PixelFormat;

    // 1. Live `Player::open` with `prefer_fel_12bit: true` on official FATE 4K Dolby Vision
    //    Profile 7 FEL MKV (`mkv/dovi-p7-hvce.mkv`) must run dual HEVC decoders automatically
    //    and deliver 3840x2160 12-bit FEL (`Yuv420P16Le`) — which FFmpeg `libavcodec` cannot do!
    let p7_mkv = refcheck::fate("mkv/dovi-p7-hvce.mkv");
    let headless_fel = Headless::new();
    let p_fel = Player::open(
        p7_mkv.to_str().unwrap(),
        headless_fel.clone(),
        Arc::new(codecs::context()),
        PlayerOptions {
            realtime: false,
            prefer_fel_12bit: true,
            ..PlayerOptions::default()
        },
        |_| {},
    );
    let st_fel = p_fel.wait();
    assert!(st_fel.ended && st_fel.error.is_none(), "live 12-bit FEL failed: {st_fel:?}");
    assert_eq!(st_fel.video_dynamic_range.as_deref(), Some("Dolby Vision P7 FEL (12-bit)"));
    assert_eq!(st_fel.video_bit_depth, Some(12));
    let cap_fel = headless_fel.capture();
    assert_eq!(cap_fel.video[0].shown_at.len(), 1);
    assert_eq!((cap_fel.video[0].width, cap_fel.video[0].height), (3840, 2160));
    assert_eq!(cap_fel.video[0].pixel_format, PixelFormat::Yuv420P16Le);

    // 2. Live `Player::open` with `steep_dsd_filter: true` on a 1-bit second-order SDM DSD64
    //    stream carrying a 1 kHz sine + SDM quantization noise: verify 64-bit `PcmFormat::F64`
    //    delivery, clean 1 kHz passband, and >35 dB stronger ultrasonic (>55 kHz) SDM noise
    //    suppression than FFmpeg's 48-tap `f32` `dsd2pcm`!
    let dir = std::env::temp_dir().join(format!("peartube-steep64-dsd-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let dsf_path = dir.join("ultrasonic_sdm.dsf");

    // Synthesize 4096 bytes (32768 DSD64 bits @ 2.8224 MHz) via 2nd-order sigma-delta modulator.
    let n_bytes = 4096usize;
    let mut dsd_lsbf = vec![0u8; n_bytes];
    let mut int1 = 0.0f64;
    let mut int2 = 0.0f64;
    let mut fb = 0.0f64;
    for bi in 0..n_bytes {
        let mut byte = 0u8;
        for bit in 0..8 {
            let t = ((bi * 8 + bit) as f64) / 2_822_400.0;
            let x = 0.4 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin();
            int1 += x - fb;
            int2 += int1 - 1.5 * fb;
            let q = if int2 >= 0.0 {
                byte |= 1 << bit;
                1.0
            } else {
                -1.0
            };
            fb = q;
        }
        dsd_lsbf[bi] = byte;
    }
    let mut dsf = Vec::new();
    let file_size = (28 + 52 + 12 + dsd_lsbf.len()) as u64;
    dsf.extend_from_slice(b"DSD ");
    dsf.extend_from_slice(&28u64.to_le_bytes());
    dsf.extend_from_slice(&file_size.to_le_bytes());
    dsf.extend_from_slice(&0u64.to_le_bytes());
    dsf.extend_from_slice(b"fmt ");
    dsf.extend_from_slice(&52u64.to_le_bytes());
    dsf.extend_from_slice(&1u32.to_le_bytes()); // format version
    dsf.extend_from_slice(&0u32.to_le_bytes()); // format id = DSD raw
    dsf.extend_from_slice(&1u32.to_le_bytes()); // channel type = mono
    dsf.extend_from_slice(&1u32.to_le_bytes()); // channels = 1
    dsf.extend_from_slice(&2_822_400u32.to_le_bytes());
    dsf.extend_from_slice(&1u32.to_le_bytes()); // bits per sample = 1 (LSB-first)
    dsf.extend_from_slice(&((dsd_lsbf.len() * 8) as u64).to_le_bytes());
    dsf.extend_from_slice(&4096u32.to_le_bytes());
    dsf.extend_from_slice(&0u32.to_le_bytes());
    dsf.extend_from_slice(b"data");
    dsf.extend_from_slice(&((12 + dsd_lsbf.len()) as u64).to_le_bytes());
    dsf.extend_from_slice(&dsd_lsbf);
    std::fs::write(&dsf_path, &dsf).unwrap();

    let ff_pcm_bytes = ffmpeg_raw_pcm(&dsf_path, "f64le", "pcm_f64le");
    let ff_samples: Vec<f64> = ff_pcm_bytes
        .chunks_exact(8)
        .map(|b| f64::from_le_bytes(b.try_into().unwrap()))
        .collect();

    let headless_dsd = Headless::new();
    let p_dsd = Player::open(
        dsf_path.to_str().unwrap(),
        headless_dsd.clone(),
        Arc::new(codecs::context()),
        PlayerOptions {
            realtime: false,
            steep_dsd_filter: true,
            ..PlayerOptions::default()
        },
        |_| {},
    );
    let st_dsd = p_dsd.wait();
    assert!(st_dsd.ended && st_dsd.error.is_none());
    let cap_dsd = headless_dsd.capture();
    assert_eq!(cap_dsd.audio[0].sample_format, Some(PcmFormat::F64));
    let ours: Vec<f64> = cap_dsd.audio[0]
        .exact_pcm
        .chunks_exact(8)
        .map(|b| f64::from_le_bytes(b.try_into().unwrap()))
        .collect();

    // Measure high-frequency (>88.2 kHz Nyquist/2, 2nd-difference `[1, -2, 1]`^2 highpass)
    // SDM noise energy over steady-state samples `256..3800`.
    let hf_rms = |sig: &[f64]| -> f64 {
        let slice = &sig[256..3800.min(sig.len())];
        let mut sum = 0.0f64;
        for w in slice.windows(5) {
            let hp = w[0] - 4.0 * w[1] + 6.0 * w[2] - 4.0 * w[3] + w[4];
            sum += hp * hp;
        }
        (sum / (slice.len() - 4) as f64).sqrt()
    };
    let ff_hf = hf_rms(&ff_samples);
    let our_hf = hf_rms(&ours);
    let improvement_db = 20.0 * (ff_hf / our_hf.max(1e-18)).log10();
    assert!(
        improvement_db >= 35.0,
        "128-tap 64-bit Blackman-Harris DSD filter must suppress ultrasonic SDM noise >=35 dB cleaner than FFmpeg dsd2pcm: got {improvement_db:.2} dB (ffmpeg={ff_hf:e}, ours={our_hf:e})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
