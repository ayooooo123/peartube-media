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
    let path = fate(sample);
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
