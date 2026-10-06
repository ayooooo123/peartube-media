use std::fs::File;
use refcheck::fate;
use oxideav_core::RuntimeContext;

#[test]
fn test_registration() {
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);

    // Verify all registered codec IDs
    assert!(ctx.codecs.has_decoder(&oxideav_core::CodecId::new("wmv1")));
    assert!(ctx.codecs.has_decoder(&oxideav_core::CodecId::new("wmv2")));
    assert!(ctx.codecs.has_decoder(&oxideav_core::CodecId::new("wmv3")));
    assert!(ctx.codecs.has_decoder(&oxideav_core::CodecId::new("vc1")));
    assert!(ctx.codecs.has_decoder(&oxideav_core::CodecId::new("msmpeg4v1")));
    assert!(ctx.codecs.has_decoder(&oxideav_core::CodecId::new("msmpeg4v2")));
    assert!(ctx.codecs.has_decoder(&oxideav_core::CodecId::new("msmpeg4v3")));

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

/// Encodes a moving test pattern with FFmpeg's own encoder into AVI, the way
/// FATE's `vsynth` tests produce their WMV1 / MS-MPEG-4 v2 samples (the FATE
/// suite has no such files). Returns the path of the encoded sample.
fn encoded_sample(name: &str, size: &str, codec_args: &[&str]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("codec-wmv-reference");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let out = dir.join(format!("{name}.avi"));
    let src = format!("testsrc2=size={size}:rate=25");
    let mut args = vec!["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", &src, "-frames:v", "40"];
    args.extend_from_slice(codec_args);
    args.extend_from_slice(&["-flags", "+bitexact", "-fflags", "+bitexact", out.to_str().unwrap()]);
    let st = std::process::Command::new("ffmpeg").args(&args).status().expect("ffmpeg must be on PATH");
    assert!(st.success(), "ffmpeg encode of {name} failed");
    out
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
