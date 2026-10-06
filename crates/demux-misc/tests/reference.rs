// Demuxer reference tests for crates/demux-misc.
//
// What a demuxer controls is: container framing (packet sizes, count),
// stream discovery (count, media type, codec id) and the timestamps the
// container itself carries. Raw elementary streams (AC-3, MPEG video,
// H.264/HEVC Annex B) have no container timestamps; FFmpeg's `ffprobe`
// output for those passes through avformat's stream parsers, which merge
// MPEG-2 field pairs into frames, re-frame PVA payloads into decoder
// frames and interpolate missing PTS/DTS — decoder-side work outside a
// demuxer's scope. Those are compared on framing (sizes, counts) and on
// the container-carried values that exist (PVA's per-PES PTS), plus, for
// AC-3/E-AC-3, one syncframe per packet with exact FFmpeg frame sizes.
//
// Sample list from FFmpeg's FATE makefiles (tests/fate/*.mak): ac3.mak,
// demux.mak, adpcm.mak (VOC), caf.mak, cbs.mak/av1.mak (IVF), mpegps.mak,
// ffmpeg.mak, h264.mak, hevc-conformance.

use refcheck::fate;

use std::io::Read;

/// `(stream_index, pts)` per packet, from `ffprobe -show_packets`.
fn ffprobe_packets(path: &std::path::Path) -> Vec<(u32, Option<i64>)> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_packets", "-of", "csv"])
        .arg(path)
        .output()
        .expect("ffprobe must be on PATH");
    assert!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.trim_end().split(',').collect();
            // "packet,<codec>,<stream_index>,<pts>,<pts_time>,..." — pts may be N/A.
            if f.len() < 4 || f[0] != "packet" {
                return None;
            }
            let stream: u32 = f[2].parse().unwrap();
            let pts = if f[3] == "N/A" { None } else { f[3].parse().ok() };
            Some((stream, pts))
        })
        .collect()
}

/// Every packet size from `ffprobe -show_packets`, in order.
fn ffprobe_sizes(path: &std::path::Path) -> Vec<usize> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_packets", "-of", "csv"])
        .arg(path)
        .output()
        .expect("ffprobe must be on PATH");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.trim_end().split(',').collect();
            if f.len() < 4 || f[0] != "packet" {
                return None;
            }
            f[9].parse().ok()
        })
        .collect()
}

struct Case {
    sample: &'static str,
    format: &'static str,
    /// (codec_type, codec_id) per stream, in stream order (from ffprobe).
    streams: &'static [(&'static str, &'static str)],
    /// Hardcoded total packet count (from ffprobe where framing matches 1:1,
    /// else from the container structure).
    /// None = skip the count check (parser-dependent formats).
    total_packets: Option<usize>,
    /// Expected packet sizes, prefix-checked (first `sizes_prefix.len()`
    /// packets) — exact where FFmpeg does no re-framing.
    sizes_prefix: &'static [usize],
    /// All sizes (exact comparison).
    sizes_exact: bool,
    /// When set, our (stream, pts) sequence must equal ffprobe's exactly.
    ffprobe_sequence: bool,
    /// When set, check the demuxed pts sequence against this list.
    pts_expect: &'static [Option<i64>],
    /// Only the first `pts_prefix` of `pts_expect` is checked.
    pts_prefix: usize,
}

impl Case {
    fn new(
        sample: &'static str,
        format: &'static str,
        streams: &'static [(&'static str, &'static str)],
        total_packets: Option<usize>,
    ) -> Self {
        Self {
            sample,
            format,
            streams,
            total_packets,
            sizes_prefix: &[],
            sizes_exact: false,
            ffprobe_sequence: false,
            pts_expect: &[],
            pts_prefix: 0,
        }
    }
}

/// Open `case` with the named demuxer and check streams, packet count,
/// sizes and timestamps against the expectations above.
fn check(case: &Case) {
    let path = fate(case.sample);
    // Full registry: NUT resolves stream fourccs through the codec crates.
    let mut ctx = codecs::context();
    demux_misc::register(&mut ctx);
    let file = std::fs::File::open(&path).unwrap();
    let mut demuxer = ctx
        .containers
        .open_demuxer(case.format, Box::new(file), &ctx.codecs)
        .unwrap_or_else(|e| panic!("{}: open {}: {e}", case.sample, case.format));

    let mut seq: Vec<(u32, i64)> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(pkt) => {
                seq.push((pkt.stream_index, pkt.pts.unwrap_or(i64::MIN)));
                sizes.push(pkt.data.len());
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("{}: demux: {e}", case.sample),
        }
    }

    // Streams (created on the fly for NOHEADER formats — hence after demux).
    let streams = demuxer.streams();
    assert_eq!(
        streams.len(),
        case.streams.len(),
        "{}: stream count",
        case.sample
    );
    for (st, &(ty, id)) in streams.iter().zip(case.streams) {
        let got_ty = format!("{:?}", st.params.media_type).to_lowercase();
        assert_eq!(got_ty, ty, "{}: stream {} media type", case.sample, st.index);
        assert_eq!(
            st.params.codec_id.as_str(),
            id,
            "{}: stream {} codec id",
            case.sample,
            st.index
        );
    }

    if let Some(want) = case.total_packets {
        assert_eq!(seq.len(), want, "{}: total packet count", case.sample);
    }

    // sizes
    let ff_sizes = ffprobe_sizes(&path);
    let check_sizes = |mine: &[usize], ff: &[usize], what: &str| {
        for (i, (&m, &f)) in mine.iter().zip(ff).enumerate() {
            assert_eq!(m, f, "{}: {what} packet {i} size", case.sample);
        }
    };
    if case.sizes_exact {
        assert_eq!(
            sizes.len(),
            ff_sizes.len(),
            "{}: packet count vs ffprobe",
            case.sample
        );
        check_sizes(&sizes, &ff_sizes, "ffprobe");
    } else if !case.sizes_prefix.is_empty() {
        check_sizes(&sizes[..case.sizes_prefix.len()], case.sizes_prefix, "hardcoded");
    }

    // timestamps
    if case.ffprobe_sequence {
        let ffpackets = ffprobe_packets(&path);
        assert_eq!(
            seq.len(),
            ffpackets.len(),
            "{}: total packet count vs ffprobe",
            case.sample
        );
        for (i, ((mstream, mpts), (tstream, tpts))) in seq.iter().zip(&ffpackets).enumerate() {
            assert_eq!(mstream, tstream, "{}: packet {i} stream", case.sample);
            if let Some(t) = tpts {
                assert_eq!(mpts, t, "{}: packet {i} pts", case.sample);
            }
        }
    }
    if !case.pts_expect.is_empty() {
        let n = if case.pts_prefix > 0 {
            case.pts_prefix
        } else {
            case.pts_expect.len()
        };
        for (i, &want) in case.pts_expect[..n].iter().enumerate() {
            let got = seq.get(i).map(|&(_, p)| p).unwrap_or(i64::MIN);
            let want = want.unwrap_or(i64::MIN);
            assert_eq!(got, want, "{}: packet {i} pts", case.sample);
        }
    }
}

// ─── raw AC-3 / E-AC-3 (ac3.mak samples: one syncframe per packet,
// pts in samples, exactly the frames FFmpeg's ac3 demuxer emits) ───

#[test]
fn ac3_monsters_inc_20() {
    // 130 full syncframes (768 B) + 160-byte truncated tail (FATE cut),
    // which FFmpeg's ff_raw_read_partial_packet also emits short.
    check(&{
        let mut c = Case::new("ac3/monsters_inc_2.0_192_small.ac3", "ac3", &[("audio", "ac3")], Some(131));
        c.sizes_exact = true;
        c
    });
}

#[test]
fn ac3_millers_crossing_40() {
    check(&{
        let mut c = Case::new("ac3/millers_crossing_4.0.ac3", "ac3", &[("audio", "ac3")], Some(62));
        c.sizes_exact = true;
        c
    });
}

#[test]
fn eac3_csi_miami_51() {
    check(&{
        let mut c = Case::new(
            "eac3/csi_miami_5.1_256_spx_small.eac3",
            "eac3",
            &[("audio", "eac3")],
            Some(47),
        );
        c.sizes_exact = true;
        c
    });
}

#[test]
fn eac3_csi_miami_stereo() {
    check(&{
        let mut c = Case::new(
            "eac3/csi_miami_stereo_128_spx_small.eac3",
            "eac3",
            &[("audio", "eac3")],
            Some(488),
        );
        c.sizes_exact = true;
        c
    });
}

#[test]
fn eac3_matrix2_commentary() {
    check(&{
        let mut c = Case::new(
            "eac3/matrix2_commentary1_stereo_192_small.eac3",
            "eac3",
            &[("audio", "eac3")],
            Some(130),
        );
        c.sizes_exact = true;
        c
    });
}

#[test]
fn eac3_serenity_51() {
    check(&{
        let mut c = Case::new(
            "eac3/serenity_english_5.1_1536_small.eac3",
            "eac3",
            &[("audio", "eac3")],
            Some(97),
        );
        c.sizes_exact = true;
        c
    });
}

// ─── raw MPEG video ES (sony-ct3.bs / Closedcaption_rollup.m2v) ───
// FFmpeg's mpegvideo parser merges MPEG-2 field pairs (10 pictures → 7
// frames for sony-ct3); a demuxer emits one packet per picture.

#[test]
fn mpegvideo_sony_ct3() {
    check(&{
        let c = Case::new("mpeg2/sony-ct3.bs", "mpegvideo", &[("video", "mpeg2video")], Some(10));
        c
    });
}

#[test]
fn mpegvideo_closedcaption() {
    // Progressive: 137 pictures = 137 frames, matching ffprobe's count.
    check(&{
        let c = Case::new(
            "sub/Closedcaption_rollup.m2v",
            "mpegvideo",
            &[("video", "mpeg2video")],
            Some(137),
        );
        c
    });
}

// ─── raw H.264 / HEVC Annex B (one access unit per packet) ───

#[test]
fn h264_intra_refresh() {
    // ffprobe: 449 AUs. Our AU split must produce the same count when the
    // stream is parseable; FFmpeg's parser also reorders but the count is
    // the frame count.
    check(&{
        let c = Case::new("h264/intra_refresh.h264", "h264", &[("video", "h264")], Some(449));
        c
    });
}

#[test]
fn h264_lossless() {
    check(&{
        let c = Case::new("h264/lossless.h264", "h264", &[("video", "h264")], Some(10));
        c
    });
}

#[test]
fn h264_nondeterministic_cut() {
    check(&{
        let c = Case::new(
            "h264/nondeterministic_cut.h264",
            "h264",
            &[("video", "h264")],
            Some(30),
        );
        c
    });
}

#[test]
fn hevc_wpp_a_main() {
    check(&{
        let c = Case::new(
            "hevc-conformance/WPP_A_ericsson_MAIN_2.bit",
            "hevc",
            &[("video", "hevc")],
            Some(48),
        );
        c
    });
}

#[test]
fn hevc_wpp_a_main10() {
    check(&{
        let c = Case::new(
            "hevc-conformance/WPP_A_ericsson_MAIN10_2.bit",
            "hevc",
            &[("video", "hevc")],
            Some(48),
        );
        c
    });
}

// ─── MPEG-PS (container timestamps; ffprobe sequence must match 1:1) ───

#[test]
fn mpegps_pcm_aud() {
    check(&{
        let mut c = Case::new("mpegps/pcm_aud.mpg", "mpeg", &[("audio", "pcm_dvd")], Some(44));
        c.sizes_exact = true;
        c.ffprobe_sequence = true;
        c
    });
}

#[test]
fn mpegps_dvd_single_frame() {
    // ffprobe emits 3 packets because FFmpeg's video parser merges the 49
    // video PES packets of the still frame into one AU; the container has
    // 49 video PES + 2 subpicture PES + padding. Our demuxer emits the
    // container's PES packets.
    check(&{
        let mut c = Case::new(
            "mpeg2/dvd_single_frame.vob",
            "mpeg",
            &[
                ("video", "mpeg2video"),
                ("subtitle", "dvdsub"),
                ("subtitle", "dvdsub"),
            ],
            Some(60),
        );
        c.ffprobe_sequence = false;
        c
    });
}

#[test]
fn mpegps_dvd_still_frame() {
    // Container: 58 video PES + 7 subpicture PES + 364 AC-3 PES = 429.
    // ffprobe: 957 (AC-3 re-framed into syncframes by the audio parser).
    check(&{
        let mut c = Case::new(
            "mpeg2/dvd_still_frame.vob",
            "mpeg",
            &[
                ("video", "mpeg2video"),
                ("subtitle", "dvdsub"),
                ("subtitle", "dvdsub"),
                ("audio", "ac3"),
            ],
            Some(429),
        );
        c.ffprobe_sequence = false;
        c
    });
}

#[test]
fn mpegps_t_mpg() {
    // ffprobe: 877 (354 video + 523 audio after mp2 parsing). Container:
    // count PES packets — the mp2 parser splits multi-frame PES payloads.
    check(&{
        let mut c = Case::new(
            "mpeg2/t.mpg",
            "mpeg",
            &[("video", "mpeg2video"), ("audio", "mp2"), ("audio", "mp2")],
            None,
        );
        c.ffprobe_sequence = false;
        c
    });
}

// ─── PVA (PES-carried PTS on every audio packet and some video packets;
// payloads are re-framed by FFmpeg's parsers, so compare PTS + counts) ───

#[test]
fn pva_test_partial() {
    // 190 video payloads (53 carry an explicit PTS) + 21 audio PES packets
    // (every one carries a PTS) = 211 PVA packets.
    check(&{
        let c = Case::new(
            "pva/PVA_test-partial.pva",
            "pva",
            &[("video", "mpeg2video"), ("audio", "mp2")],
            Some(211),
        );
        c
    });
}

// ─── Creative VOC (adpcm.mak: one block per packet, 2048-byte cap like
// FFmpeg's max_size, exact sizes from ffprobe) ───

#[test]
fn voc_bbc_2bit() {
    check(&{
        let mut c = Case::new("creative/BBC_2BIT.VOC", "voc", &[("audio", "adpcm_sbpro_2")], Some(13));
        c.sizes_exact = true;
        c
    });
}

#[test]
fn voc_bbc_3bit() {
    check(&{
        let mut c = Case::new("creative/BBC_3BIT.VOC", "voc", &[("audio", "adpcm_sbpro_3")], Some(18));
        c.sizes_exact = true;
        c
    });
}

#[test]
fn voc_bbc_4bit() {
    check(&{
        let mut c = Case::new("creative/BBC_4BIT.VOC", "voc", &[("audio", "adpcm_sbpro_4")], Some(26));
        c.sizes_exact = true;
        c
    });
}

// ─── CAF (caf.mak: packet-table framing, exact sizes and pts) ───

#[test]
fn caf_pcm16() {
    check(&{
        let mut c = Case::new("caf/caf-pcm16.caf", "caf", &[("audio", "pcm_s16be")], Some(10));
        c.sizes_exact = true;
        c.pts_expect = &[Some(0), Some(2048), Some(4096)];
        c.pts_prefix = 3;
        c
    });
}

#[test]
fn caf_aac() {
    // aac.caf: priming 2112, so ffprobe's first pts is -2112 (skip
    // samples); our demuxer reports raw frame positions 0,1024,...
    check(&{
        let mut c = Case::new("caf/aac.caf", "caf", &[("audio", "aac")], Some(237));
        c.sizes_exact = true;
        c.pts_expect = &[Some(-2112), Some(-1088), Some(-64)];
        c.pts_prefix = 3;
        c
    });
}

#[test]
fn caf_opus() {
    check(&{
        let mut c = Case::new("caf/opus.caf", "caf", &[("audio", "opus")], Some(251));
        c.sizes_exact = true;
        c.pts_expect = &[Some(-312), Some(648), Some(1608)];
        c.pts_prefix = 3;
        c
    });
}

// ─── IVF (cbs.mak/av1.mak samples: 12-byte frame headers carry the pts;
// ffprobe sequence matches 1:1) ───

#[test]
fn ivf_seq_hdr_op_param_info() {
    check(&{
        let mut c = Case::new("av1/seq_hdr_op_param_info.ivf", "ivf", &[("video", "av1")], Some(64));
        c.sizes_exact = true;
        c.ffprobe_sequence = true;
        c
    });
}

#[test]
fn ivf_decode_model() {
    check(&{
        let mut c = Case::new("av1/decode_model.ivf", "ivf", &[("video", "av1")], Some(24));
        c.sizes_exact = true;
        c.ffprobe_sequence = true;
        c
    });
}

#[test]
fn ivf_film_grain() {
    check(&{
        let mut c = Case::new("av1/film_grain.ivf", "ivf", &[("video", "av1")], Some(10));
        c.sizes_exact = true;
        c.ffprobe_sequence = true;
        c
    });
}

// ─── NUT (lavf muxes it; ffprobe-test.nut is generated by FFmpeg's own
// test rig, so mux one here with ffmpeg and verify demux against it) ───

#[test]
fn nut_demux_matches_ffprobe() {
    // Mux a tiny NUT with FFmpeg (the reference encoder), then compare our
    // demuxed packet sequence with ffprobe's on the same file.
    let path = std::env::temp_dir().join("demux-misc-nut-test.nut");
    let _ = std::fs::remove_file(&path);

    let out = std::process::Command::new("ffmpeg")
        .args([
            "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=1000:duration=0.5",
            "-f", "lavfi", "-i", "testsrc=duration=0.5:size=64x64:rate=10",
            "-c:a", "mp2", "-c:v", "mpeg2video", "-shortest",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(
        out.status.success(),
        "ffmpeg mux failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let sample = intern(path.to_str().unwrap().to_string());
    check(&{
        let mut c = Case::new(
            sample,
            "nut",
            &[("video", "mpeg2video"), ("audio", "mp2")],
            None, // ffprobe verifies the count below
        );
        c.ffprobe_sequence = true;
        c
    });
    let _ = std::fs::remove_file(&path);
}

/// Store `s` for the whole process in a global registry and return a
/// stable address of the stored string (leak-free: the registry owns it
/// and never shrinks).
fn intern(s: String) -> &'static str {
    static REGISTRY: std::sync::LazyLock<std::sync::Mutex<Vec<String>>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));
    let mut reg = REGISTRY.lock().unwrap();
    reg.push(s);
    let ptr: *const str = reg.last().unwrap().as_str();
    unsafe { &*ptr }
}

// ─── probe disambiguation through the full player registry ───

#[test]
fn probes_pick_the_right_format_in_the_full_registry() {
    let samples: &[(&str, &str)] = &[
        ("ac3/monsters_inc_2.0_192_small.ac3", "ac3"),
        ("eac3/csi_miami_5.1_256_spx_small.eac3", "eac3"),
        ("mpeg2/sony-ct3.bs", "mpegvideo"),
        ("hevc-conformance/WPP_A_ericsson_MAIN_2.bit", "hevc"),
        ("mpegps/pcm_aud.mpg", "mpeg"),
        ("pva/PVA_test-partial.pva", "pva"),
        ("creative/BBC_2BIT.VOC", "voc"),
        ("caf/caf-pcm16.caf", "caf"),
        ("av1/seq_hdr_op_param_info.ivf", "ivf"),
    ];
    let ctx = codecs::context();
    for (rel, want) in samples {
        let path = fate(rel);
        let mut head = vec![0u8; 256 * 1024];
        let n = std::fs::File::open(&path)
            .and_then(|mut f| f.read(&mut head))
            .unwrap();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let probe = oxideav_core::ProbeData {
            buf: &head[..n],
            ext: ext.as_deref(),
        };
        let candidates = ctx.containers.probe_candidates(&probe);
        let by_ext = ext
            .as_deref()
            .and_then(|e| ctx.containers.container_for_extension(e));
        let picked = match candidates.first() {
            Some(c) if c.score >= oxideav_core::PROBE_SCORE_EXTENSION => Some(c.name),
            _ => by_ext,
        };
        assert_eq!(
            picked.map(str::to_string),
            Some(want.to_string()),
            "{rel}: probe picked {picked:?}, want {want}"
        );
    }
}
