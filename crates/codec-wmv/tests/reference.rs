mod common;

use common::encoded_sample;
use oxideav_core::{CodecTag, RuntimeContext};
use refcheck::fate;
use std::fs::File;

#[test]
fn test_registration() {
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);

    // Verify all registered codec IDs
    for id in ["wmv1", "wmv2", "wmv3", "vc1", "msmpeg4v1", "msmpeg4v2", "msmpeg4v3"] {
        assert!(ctx.codecs.has_decoder(&oxideav_core::CodecId::new(id)), "{id}");
    }

    // The container tags FFmpeg maps to WMV3 / VC-1 (riff.c, isom.c).
    let tags: Vec<(String, String)> =
        ctx.codecs.all_tag_registrations().map(|(t, id)| (format!("{t:?}"), id.as_str().to_string())).collect();
    for (tag, id) in [
        (CodecTag::fourcc(b"WMV3"), "wmv3"),
        (CodecTag::fourcc(b"WVC1"), "vc1"),
        (CodecTag::fourcc(b"WMVA"), "vc1"),
        (CodecTag::fourcc(b"vc-1"), "vc1"),
        (CodecTag::mp4_object_type(0xA3), "vc1"),
    ] {
        assert!(tags.contains(&(format!("{tag:?}"), id.to_string())), "{tag:?} -> {id}");
    }

    // Verify container registrations
    assert_eq!(ctx.containers.container_for_extension("rcv"), Some("vc1test"));
    assert_eq!(ctx.containers.container_for_extension("vc1"), Some("vc1"));
}

#[test]
fn test_demux_smm0005_rcv() {
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);

    let path = fate("vc1/SMM0005.rcv");
    let file = File::open(&path).expect("open SMM0005.rcv");
    let mut demuxer = ctx.containers.open_demuxer("vc1test", Box::new(file), &ctx.codecs).expect("open demuxer");

    assert_eq!(demuxer.format_name(), "vc1test");
    assert_eq!(demuxer.streams().len(), 1);
    let st = &demuxer.streams()[0];
    assert_eq!(st.params.codec_id.as_str(), "wmv3");
    assert_eq!(st.params.width, Some(720));
    assert_eq!(st.params.height, Some(480));
    assert_eq!(st.duration, Some(24));

    let mut pkts = 0;
    while let Ok(pkt) = demuxer.next_packet() {
        if pkts == 0 {
            assert!(pkt.flags.keyframe);
            assert_eq!(pkt.data.len(), 147829);
        }
        pkts += 1;
    }
    assert_eq!(pkts, 24, "SMM0005.rcv packet count must be 24");
}

#[test]
fn test_demux_smm0015_rcv() {
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);

    let path = fate("vc1/SMM0015.rcv");
    let file = File::open(&path).expect("open SMM0015.rcv");
    let mut demuxer = ctx.containers.open_demuxer("vc1test", Box::new(file), &ctx.codecs).expect("open demuxer");

    assert_eq!(demuxer.format_name(), "vc1test");
    assert_eq!(demuxer.streams().len(), 1);
    let st = &demuxer.streams()[0];
    assert_eq!(st.params.codec_id.as_str(), "wmv3");
    assert_eq!(st.params.width, Some(720));
    assert_eq!(st.params.height, Some(576));
    assert_eq!(st.duration, Some(25));

    let mut pkts = 0;
    while let Ok(pkt) = demuxer.next_packet() {
        if pkts == 0 {
            assert!(pkt.flags.keyframe);
        }
        pkts += 1;
    }
    assert_eq!(pkts, 25, "SMM0015.rcv packet count must be 25");
}

#[test]
fn test_demux_sa00040_vc1() {
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);

    let path = fate("vc1/SA00040.vc1");
    let file = File::open(&path).expect("open SA00040.vc1");
    let mut demuxer = ctx.containers.open_demuxer("vc1", Box::new(file), &ctx.codecs).expect("open demuxer");

    assert_eq!(demuxer.format_name(), "vc1");
    assert_eq!(demuxer.streams().len(), 1);
    let st = &demuxer.streams()[0];
    assert_eq!(st.params.codec_id.as_str(), "vc1");
    assert_eq!(st.params.width, Some(176));
    assert_eq!(st.params.height, Some(144));

    let mut pkts = 0;
    while let Ok(pkt) = demuxer.next_packet() {
        if pkts == 0 {
            assert!(pkt.flags.keyframe);
            assert_eq!(pkt.data.len(), 5615);
        }
        pkts += 1;
    }
    assert_eq!(pkts, 15, "SA00040.vc1 packet count must be 15");
}

#[test]
fn test_demux_sa00050_vc1() {
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);

    let path = fate("vc1/SA00050.vc1");
    let file = File::open(&path).expect("open SA00050.vc1");
    let mut demuxer = ctx.containers.open_demuxer("vc1", Box::new(file), &ctx.codecs).expect("open demuxer");
    assert_eq!(demuxer.streams()[0].params.width, Some(320));
    assert_eq!(demuxer.streams()[0].params.height, Some(240));

    let mut pkts = 0;
    while let Ok(_) = demuxer.next_packet() {
        pkts += 1;
    }
    assert_eq!(pkts, 30, "SA00050.vc1 packet count must be 30");
}

#[test]
fn test_demux_sa10091_vc1() {
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);

    let path = fate("vc1/SA10091.vc1");
    let file = File::open(&path).expect("open SA10091.vc1");
    let mut demuxer = ctx.containers.open_demuxer("vc1", Box::new(file), &ctx.codecs).expect("open demuxer");
    assert_eq!(demuxer.streams()[0].params.width, Some(720));
    assert_eq!(demuxer.streams()[0].params.height, Some(480));

    let mut pkts = 0;
    while let Ok(_) = demuxer.next_packet() {
        pkts += 1;
    }
    assert_eq!(pkts, 30, "SA10091.vc1 packet count must be 30");
}

#[test]
fn test_demux_ilaced_twomv_vc1() {
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);

    let path = fate("vc1/ilaced_twomv.vc1");
    let file = File::open(&path).expect("open ilaced_twomv.vc1");
    let mut demuxer = ctx.containers.open_demuxer("vc1", Box::new(file), &ctx.codecs).expect("open demuxer");
    assert_eq!(demuxer.streams()[0].params.width, Some(1920));
    assert_eq!(demuxer.streams()[0].params.height, Some(1080));

    let mut pkts = 0;
    while let Ok(_) = demuxer.next_packet() {
        pkts += 1;
    }
    assert_eq!(pkts, 13, "ilaced_twomv.vc1 packet count must be 13");
}

#[test]
fn test_demux_wmv8_x8intra() {
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);
    demux_asf::register(&mut ctx);

    let path = fate("wmv8/wmv8_x8intra.wmv");
    let file = File::open(&path).expect("open wmv8_x8intra.wmv");
    let mut demuxer = ctx.containers.open_demuxer("asf", Box::new(file), &ctx.codecs).expect("open demuxer");
    let video_st = demuxer.streams().iter().find(|s| s.params.codec_id.as_str() == "wmv2").expect("video stream").clone();
    assert_eq!(video_st.params.width, Some(320));
    assert_eq!(video_st.params.height, Some(240));
    assert_eq!(video_st.params.extradata.len(), 4);

    let mut pkts = 0;
    while let Ok(pkt) = demuxer.next_packet() {
        if pkt.stream_index == video_st.index {
            pkts += 1;
        }
    }
    assert!(pkts > 0, "must find wmv2 video packets");
}

/// Decodes `sample` with our crates and compares every frame's MD5 with
/// FFmpeg's (`input_args` go before `-i`, e.g. `-idct simple`).
fn check_video(sample: &str, registrars: &[refcheck::Registrar], input_args: &[&str]) {
    check_video_path(&fate(sample), registrars, input_args);
}

fn check_video_path(path: &std::path::Path, registrars: &[refcheck::Registrar], input_args: &[&str]) {
    let path = path.to_path_buf();
    let sample = path.display().to_string();
    let decoded = refcheck::decode(&path, registrars, oxideav_core::MediaType::Video, 0);
    let w = decoded.params.width.expect("width") as usize;
    let h = decoded.params.height.expect("height") as usize;
    let dims = [(w, h), (w.div_ceil(2), h.div_ceil(2)), (w.div_ceil(2), h.div_ceil(2))];
    let expected = refcheck::ffmpeg_video_md5s_with(&path, 0, "yuv420p", input_args);
    let mut mismatched = Vec::new();
    for (i, frame) in decoded.frames.iter().enumerate() {
        let oxideav_core::Frame::Video(vf) = frame else { panic!("{sample}: frame {i} is not video") };
        let md5 = refcheck::md5_hex(&refcheck::pack(vf, &dims));
        if expected.get(i) != Some(&md5) {
            mismatched.push(i);
        }
    }
    assert!(
        mismatched.is_empty(),
        "{sample}: {} of {} frames differ from FFmpeg (first: {:?})",
        mismatched.len(),
        decoded.frames.len(),
        &mismatched[..mismatched.len().min(16)]
    );
    assert_eq!(decoded.frames.len(), expected.len(), "{sample}: frame count");
}

/// WMV2 I/P pictures, IntraX8 (J-type) pictures, ABT, mspel MC and the
/// in-loop filter: `fate-wmv8-x8intra`.
#[test]
fn wmv8_x8intra_matches_ffmpeg() {
    check_video("wmv8/wmv8_x8intra.wmv", &[codec_wmv::register, demux_asf::register], &["-idct", "simple", "-flags", "+bitexact"]);
}

/// MS-MPEG-4 v1 in AVI: `fate-msmpeg4v1`.
#[test]
fn msmpeg4v1_mpg4_avi_matches_ffmpeg() {
    check_video("msmpeg4v1/mpg4.avi", &[codec_wmv::register, oxideav_avi::__oxideav_entry], &["-idct", "simple", "-flags", "+bitexact"]);
}

/// MS-MPEG-4 v3 ('MP43') in ASF (the `fate-asf-repldata` sample).
#[test]
fn msmpeg4v3_asf_matches_ffmpeg() {
    check_video("asf/bug821-2.asf", &[codec_wmv::register, demux_asf::register], &["-idct", "simple"]);
}

/// WMV3 Main profile I/P/B pictures in RCV: `fate-vc1test_smm0005`.
#[test]
fn vc1test_smm0005_matches_ffmpeg() {
    check_video("vc1/SMM0005.rcv", &[codec_wmv::register], &["-idct", "simple"]);
}

/// WMV3 Main profile I/P pictures, PAL size: `fate-vc1test_smm0015`.
#[test]
fn vc1test_smm0015_matches_ffmpeg() {
    check_video("vc1/SMM0015.rcv", &[codec_wmv::register], &["-idct", "simple"]);
}

/// VC-1 Advanced profile, progressive I/P, QCIF: `fate-vc1_sa00040`.
#[test]
fn vc1_sa00040_matches_ffmpeg() {
    check_video("vc1/SA00040.vc1", &[codec_wmv::register], &["-idct", "simple"]);
}

/// VC-1 Advanced profile, progressive I/P: `fate-vc1_sa00050`.
#[test]
fn vc1_sa00050_matches_ffmpeg() {
    check_video("vc1/SA00050.vc1", &[codec_wmv::register], &["-idct", "simple"]);
}

/// VC-1 Advanced profile level 1, progressive I/P in slices: `fate-vc1_sa10091`.
#[test]
fn vc1_sa10091_matches_ffmpeg() {
    check_video("vc1/SA10091.vc1", &[codec_wmv::register], &["-idct", "simple"]);
}

/// VC-1 Advanced profile, interlaced field pictures (P/B): `fate-vc1_sa10143`.
#[test]
fn vc1_sa10143_matches_ffmpeg() {
    check_video("vc1/SA10143.vc1", &[codec_wmv::register], &["-idct", "simple"]);
}

/// VC-1 Advanced profile level 2, 704x480 I/P in slices: `fate-vc1_sa20021`.
#[test]
fn vc1_sa20021_matches_ffmpeg() {
    check_video("vc1/SA20021.vc1", &[codec_wmv::register], &["-idct", "simple"]);
}

/// VC-1 Advanced profile, 1080i: interlaced frame pictures with 2MV/4MV
/// field motion, and field pictures: `fate-vc1_ilaced_twomv`.
#[test]
fn vc1_ilaced_twomv_matches_ffmpeg() {
    check_video("vc1/ilaced_twomv.vc1", &[codec_wmv::register], &["-idct", "simple", "-flags", "+bitexact"]);
}

/// VC-1 in a Smooth Streaming (fragmented MP4) file, 'vc-1' sample entry
/// with a `dvc1` sequence header: `fate-vc1-ism`.
#[test]
fn vc1_ism_matches_ffmpeg() {
    check_video(
        "isom/vc1-wmapro.ism",
        &[codec_wmv::register, oxideav_mp4::__oxideav_entry, oxideav_mov::registry::register],
        &["-idct", "simple"],
    );
}

/// WMV1, CIF, fixed quantiser (`fate-vsynth*-wmv1` settings).
#[test]
fn wmv1_vsynth_matches_ffmpeg() {
    let path = encoded_sample("wmv1_cif", "352x288", &["-c:v", "wmv1", "-qscale:v", "10"]);
    check_video_path(&path, &[codec_wmv::register, oxideav_avi::__oxideav_entry], &["-idct", "simple"]);
}

/// WMV1, QCIF at 100 kbit/s: the low-rate, small-picture mode with
/// inter-intra DC prediction in P-pictures.
#[test]
fn wmv1_inter_intra_matches_ffmpeg() {
    let path = encoded_sample("wmv1_qcif", "176x144", &["-c:v", "wmv1", "-b:v", "100k"]);
    check_video_path(&path, &[codec_wmv::register, oxideav_avi::__oxideav_entry], &["-idct", "simple"]);
}

/// MS-MPEG-4 v2 (`fate-vsynth*-msmpeg4v2` settings).
#[test]
fn msmpeg4v2_vsynth_matches_ffmpeg() {
    let path = encoded_sample("msmpeg4v2_cif", "352x288", &["-c:v", "msmpeg4v2", "-qscale:v", "10"]);
    check_video_path(&path, &[codec_wmv::register, oxideav_avi::__oxideav_entry], &["-idct", "simple"]);
}

/// MS-MPEG-4 v3 (`fate-vsynth*-msmpeg4` settings), odd macroblock grid.
#[test]
fn msmpeg4v3_vsynth_matches_ffmpeg() {
    let path = encoded_sample("msmpeg4v3_odd", "200x152", &["-c:v", "msmpeg4", "-qscale:v", "10"]);
    check_video_path(&path, &[codec_wmv::register, oxideav_avi::__oxideav_entry], &["-idct", "simple"]);
}

/// WMV2 from FFmpeg's encoder (`fate-vsynth*-wmv2` settings) with the
/// in-loop filter enabled.
#[test]
fn wmv2_vsynth_matches_ffmpeg() {
    let path = encoded_sample("wmv2_cif", "352x288", &["-c:v", "wmv2", "-qscale:v", "10"]);
    check_video_path(&path, &[codec_wmv::register, oxideav_avi::__oxideav_entry], &["-idct", "simple"]);
}
