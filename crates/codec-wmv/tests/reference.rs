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

/// One `ffprobe -show_packets` row of stream 0.
#[derive(Debug, PartialEq)]
struct ProbedPacket {
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<i64>,
    size: usize,
    key: bool,
}

fn ffprobe(args: &[&str], path: &std::path::Path) -> String {
    let out = std::process::Command::new(refcheck::pinned_ffprobe())
        .args(["-v", "error"])
        .args(args)
        .arg(path)
        .output()
        .expect("the pinned ffprobe runs");
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn ffprobe_packets(path: &std::path::Path) -> Vec<ProbedPacket> {
    let ts = |v: &str| if v == "N/A" { None } else { Some(v.parse::<i64>().unwrap()) };
    ffprobe(&["-select_streams", "0", "-show_entries", "packet=pts,dts,duration,size,flags", "-of", "csv=p=0"], path)
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            ProbedPacket { pts: ts(f[0]), dts: ts(f[1]), duration: ts(f[2]), size: f[3].parse().unwrap(), key: f[4].starts_with('K') }
        })
        .collect()
}

/// Demuxes `sample` with our `format` demuxer and compares it with
/// `ffprobe`: stream codec id, size and time base, then packet count,
/// sizes, key flags and every timestamp FFmpeg reports. FFmpeg leaves some
/// timestamps of raw streams unset (N/A); ours carries the frame-rate
/// timestamps there, which the player needs.
fn check_demux(sample: &str, format: &str) {
    let path = fate(sample);
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);
    let mut demuxer = ctx
        .containers
        .open_demuxer(format, Box::new(File::open(&path).unwrap()), &ctx.codecs)
        .unwrap_or_else(|e| panic!("{sample}: {e}"));
    assert_eq!(demuxer.format_name(), format);
    assert_eq!(demuxer.streams().len(), 1, "{sample}: stream count");
    let st = demuxer.streams()[0].clone();

    let stream = ffprobe(&["-select_streams", "0", "-show_entries", "stream=codec_name,width,height,time_base", "-of", "csv=p=0"], &path);
    let f: Vec<&str> = stream.trim().split(',').collect();
    assert_eq!(st.params.codec_id.as_str(), f[0], "{sample}: codec id");
    assert_eq!(st.params.width, Some(f[1].parse().unwrap()), "{sample}: width");
    assert_eq!(st.params.height, Some(f[2].parse().unwrap()), "{sample}: height");
    assert_eq!(format!("{}/{}", st.time_base.0.num, st.time_base.0.den), f[3], "{sample}: time base");

    let expected = ffprobe_packets(&path);
    let mut n = 0;
    loop {
        let pkt = match demuxer.next_packet() {
            Ok(pkt) => pkt,
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("{sample}: packet {n}: {e}"),
        };
        let exp = expected.get(n).unwrap_or_else(|| panic!("{sample}: extra packet {n}"));
        assert_eq!(pkt.data.len(), exp.size, "{sample}: packet {n} size");
        assert_eq!(pkt.flags.keyframe, exp.key, "{sample}: packet {n} key flag");
        for (ours, theirs, what) in [(pkt.pts, exp.pts, "pts"), (pkt.dts, exp.dts, "dts"), (pkt.duration, exp.duration, "duration")] {
            if theirs.is_some() {
                assert_eq!(ours, theirs, "{sample}: packet {n} {what}");
            }
        }
        n += 1;
    }
    assert_eq!(n, expected.len(), "{sample}: packet count");
}

#[test]
fn vc1test_demux_matches_ffprobe() {
    check_demux("vc1/SMM0005.rcv", "vc1test");
    check_demux("vc1/SMM0015.rcv", "vc1test");
}

#[test]
fn vc1_demux_matches_ffprobe() {
    for sample in ["SA00040", "SA00050", "SA10091", "SA10143", "SA20021", "ilaced_twomv"] {
        check_demux(&format!("vc1/{sample}.vc1"), "vc1");
    }
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

/// The first three packets are reference, reference, B in decode order and
/// reference, B, reference in display order. Synthetic demux PTS equals DTS:
/// the B is timed at slot 2, so the delayed reference must continue at slot 3,
/// not return to its decode slot 1. Consumers infer that last slot from an
/// untimed frame plus the packet duration (the player's `FrameClock` rule).
#[test]
fn vc1_flush_tail_preserves_presentation_pts_but_not_synthetic_decode_pts() {
    use oxideav_core::{Error, ExecutionContext, Frame};

    for (sample, format) in [("vc1/SMM0005.rcv", "vc1test"), ("vc1/SA10143.vc1", "vc1")] {
        let path = fate(sample);
        let mut ctx = RuntimeContext::new();
        codec_wmv::register(&mut ctx);
        let mut demux = ctx.containers.open_demuxer(format, Box::new(File::open(&path).unwrap()), &ctx.codecs).unwrap();
        let stream = demux.streams()[0].clone();
        let packets: Vec<_> = (0..3).map(|_| demux.next_packet().unwrap()).collect();
        let step = packets[0].duration.expect("fixed-rate fixture duration");
        assert!(step > 0);
        for (i, packet) in packets.iter().enumerate() {
            assert_eq!(packet.pts, Some(i as i64 * step), "{sample}: synthetic packet PTS");
            assert_eq!(packet.dts, packet.pts, "{sample}: synthetic packet DTS");
            assert_eq!(packet.duration, Some(step), "{sample}: fixed cadence");
        }
        // FFmpeg independently establishes both frame identity and display order.
        let reference = refcheck::ffmpeg_video_md5s_with(&path, 0, "yuv420p", &["-idct", "simple"]);
        assert_eq!(
            ffprobe(&["-read_intervals", "%+#3", "-show_entries", "frame=pict_type", "-of", "csv=p=0"], &path)
                .lines()
                .collect::<Vec<_>>(),
            if format == "vc1test" { vec!["I", "B", "P"] } else { vec!["P", "B", "P"] },
            "{sample}: fixture display order"
        );

        for threads in [1, 3] {
            for presentation_pts in [false, true] {
                let mut packets = packets.clone();
                let offset = 100 * step;
                if presentation_pts {
                    // A container can supply actual reordered PTS, distinct from DTS.
                    for (packet, slot) in packets.iter_mut().zip([0, 2, 1]) {
                        packet.pts = Some(offset + slot * step);
                    }
                }
                let mut decoder = ctx.codecs.first_decoder(&stream.params).unwrap();
                decoder.set_execution_context(&ExecutionContext::with_threads(threads));
                let mut pts = Vec::new();
                let mut md5 = Vec::new();
                for packet in packets.iter().map(Some).chain(std::iter::once(None)) {
                    if let Some(packet) = packet {
                        decoder.send_packet(packet).unwrap();
                    } else {
                        decoder.flush().unwrap();
                    }
                    loop {
                        match decoder.receive_frame() {
                            Ok(Frame::Video(frame)) => {
                                let (w, h) = decoder.output_video_dimensions().expect("frame dimensions");
                                let (w, h) = (w as usize, h as usize);
                                let dims = [(w, h), (w.div_ceil(2), h.div_ceil(2)), (w.div_ceil(2), h.div_ceil(2))];
                                md5.push(refcheck::md5_hex(&refcheck::pack(&frame, &dims)));
                                pts.push(frame.pts);
                            }
                            Ok(_) => panic!("{sample}: non-video frame"),
                            Err(Error::NeedMore | Error::Eof) => break,
                            Err(e) => panic!("{sample} (threads={threads}, presentation_pts={presentation_pts}): {e}"),
                        }
                    }
                }
                assert_eq!(md5, reference[..3], "{sample}: truncated GOP display frames, threads={threads}");
                let expected_pts = if presentation_pts {
                    vec![Some(offset), Some(offset + step), Some(offset + 2 * step)]
                } else {
                    vec![Some(0), Some(2 * step), None]
                };
                assert_eq!(pts, expected_pts, "{sample}: decoder PTS, threads={threads}, presentation_pts={presentation_pts}");
                let mut next = 0;
                let display_pts: Vec<_> = pts
                    .into_iter()
                    .map(|pts| {
                        let display = pts.unwrap_or(next);
                        next = display + step;
                        display
                    })
                    .collect();
                let expected_display = if presentation_pts {
                    vec![offset, offset + step, offset + 2 * step]
                } else {
                    vec![0, 2 * step, 3 * step]
                };
                assert_eq!(display_pts, expected_display, "{sample}: inferred display timeline, threads={threads}");
            }
        }
    }
}

/// Exercises `Vc1Decoder` with `ExecutionContext::with_threads(3)` across every VC-1 / WMV3
/// FATE sample (progressive, B-frames, sliced, interlaced, fragmented MP4), proving bit-exact
/// frame MD5s, display order, output dimensions, PTS, flush tail, mid-stream drop/reopen, and
/// corrupt-packet error propagation against serial decode and FFmpeg.
#[test]
fn vc1_and_wmv3_threaded_pipeline_matches_serial_and_ffmpeg() {
    use oxideav_core::{Error, ExecutionContext, Frame, MediaType, Packet, TimeBase};

    let cases: &[(&str, &[refcheck::Registrar], &[&str])] = &[
        ("vc1/SMM0005.rcv", &[codec_wmv::register], &["-idct", "simple"]),
        ("vc1/SMM0015.rcv", &[codec_wmv::register], &["-idct", "simple"]),
        ("vc1/SA00040.vc1", &[codec_wmv::register], &["-idct", "simple"]),
        ("vc1/SA00050.vc1", &[codec_wmv::register], &["-idct", "simple"]),
        ("vc1/SA10091.vc1", &[codec_wmv::register], &["-idct", "simple"]),
        ("vc1/SA10143.vc1", &[codec_wmv::register], &["-idct", "simple"]),
        ("vc1/SA20021.vc1", &[codec_wmv::register], &["-idct", "simple"]),
        ("vc1/ilaced_twomv.vc1", &[codec_wmv::register], &["-idct", "simple", "-flags", "+bitexact"]),
        (
            "isom/vc1-wmapro.ism",
            &[codec_wmv::register, oxideav_mp4::__oxideav_entry, oxideav_mov::registry::register],
            &["-idct", "simple"],
        ),
    ];

    let decode_with = |sample: &str, registrars: &[refcheck::Registrar], threads: usize, abort_at: Option<usize>| {
        let path = fate(sample);
        let mut ctx = RuntimeContext::new();
        for r in registrars {
            r(&mut ctx);
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let container = ctx
            .containers
            .container_for_extension(ext)
            .unwrap_or_else(|| if ext == "ism" { "mp4" } else { panic!("no container for {ext}") });
        let mut demux = ctx
            .containers
            .open_demuxer(container, Box::new(File::open(&path).unwrap()), &ctx.codecs)
            .unwrap();
        let vstream = demux
            .streams()
            .iter()
            .find(|s| s.params.media_type == MediaType::Video)
            .unwrap()
            .clone();
        let mut dec = ctx.codecs.first_decoder(&vstream.params).unwrap();
        dec.set_execution_context(&ExecutionContext::with_threads(threads));

        let mut out_md5 = Vec::new();
        let mut out_pts = Vec::new();
        let mut out_dims = Vec::new();
        let mut pkt_count = 0usize;
        while let Ok(pkt) = demux.next_packet() {
            if pkt.stream_index != vstream.index {
                continue;
            }
            pkt_count += 1;
            if abort_at.is_some_and(|lim| pkt_count > lim) {
                // Drop `dec` mid-flight with workers queued to prove clean teardown without hang.
                drop(dec);
                return (out_md5, out_pts, out_dims);
            }
            dec.send_packet(&pkt).unwrap();
            loop {
                match dec.receive_frame() {
                    Ok(Frame::Video(vf)) => {
                        let (dw, dh) = dec.output_video_dimensions().expect("dims");
                        let (w, h) = (dw as usize, dh as usize);
                        let dims = [(w, h), (w.div_ceil(2), h.div_ceil(2)), (w.div_ceil(2), h.div_ceil(2))];
                        out_md5.push(refcheck::md5_hex(&refcheck::pack(&vf, &dims)));
                        out_pts.push(vf.pts);
                        out_dims.push((dw, dh));
                    }
                    Ok(_) => {}
                    Err(Error::NeedMore) | Err(Error::Eof) => break,
                    Err(e) => panic!("{sample} (threads={threads}): {e}"),
                }
            }
        }
        dec.flush().unwrap();
        loop {
            match dec.receive_frame() {
                Ok(Frame::Video(vf)) => {
                    let (dw, dh) = dec.output_video_dimensions().expect("dims");
                    let (w, h) = (dw as usize, dh as usize);
                    let dims = [(w, h), (w.div_ceil(2), h.div_ceil(2)), (w.div_ceil(2), h.div_ceil(2))];
                    out_md5.push(refcheck::md5_hex(&refcheck::pack(&vf, &dims)));
                    out_pts.push(vf.pts);
                    out_dims.push((dw, dh));
                }
                Ok(_) => {}
                Err(Error::NeedMore) | Err(Error::Eof) => break,
                Err(e) => panic!("{sample} flush (threads={threads}): {e}"),
            }
        }
        (out_md5, out_pts, out_dims)
    };

    for &(sample, registrars, ff_args) in cases {
        let path = fate(sample);
        let expected_ff = refcheck::ffmpeg_video_md5s_with(&path, 0, "yuv420p", ff_args);
        let (ser_md5, ser_pts, ser_dims) = decode_with(sample, registrars, 1, None);
        // Mid-stream abort/reopen check (simulating seek replacement while B-workers are active)
        let _ = decode_with(sample, registrars, 3, Some(5));
        let (mt_md5, mt_pts, mt_dims) = decode_with(sample, registrars, 3, None);
        assert_eq!(mt_md5, expected_ff, "{sample}: threaded MD5s differ from FFmpeg");
        assert_eq!(mt_md5, ser_md5, "{sample}: threaded MD5s differ from serial");
        assert_eq!(mt_dims, ser_dims, "{sample}: threaded dimensions differ from serial");
        assert_eq!(mt_pts, ser_pts, "{sample}: threaded PTS differ from serial");
    }

    // Corrupt packet error propagation under threaded mode (`uninitialized seq/ep` error)
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);
    let mut params = oxideav_core::CodecParameters::video(oxideav_core::CodecId::new("vc1"));
    params.width = Some(320);
    params.height = Some(240);
    let mut dec = ctx.codecs.first_decoder(&params).unwrap();
    dec.set_execution_context(&ExecutionContext::with_threads(3));
    let bad = Packet {
        stream_index: 0,
        time_base: TimeBase::new(1, 1000),
        pts: Some(0),
        dts: Some(0),
        duration: Some(41),
        flags: Default::default(),
        data: vec![0x00, 0x00, 0x01, 0x0D, 0xFF, 0xFF],
    };
    let mut saw_err = false;
    for _ in 0..8 {
        if dec.send_packet(&bad).is_err() || matches!(dec.receive_frame(), Err(e) if !matches!(e, Error::NeedMore | Error::Eof)) {
            saw_err = true;
            break;
        }
    }
    if !saw_err && dec.flush().is_err() {
        saw_err = true;
    }
    assert!(saw_err, "threaded Vc1Decoder must surface decode errors");
}
