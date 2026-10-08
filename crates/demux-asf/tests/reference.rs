// ASF demuxer reference tests: for every FATE sample carried in an ASF/WMV/WMA
// container, packet count, per-stream codec id and per-packet PTS must equal
// `ffprobe -show_packets`.
//
// Sample list from FFmpeg's FATE makefiles (tests/fate/*.mak): every test whose
// sample is an ASF/WMV/WMA container (asf/, wmv8/, wmapro/, wmavoice/,
// cover_art/, lossless-audio/, mss1/, mss2/, mts2/, g2m/, tdsc/, argo-asf/).

use refcheck::fate;

/// One `(sample, expectations)` row of the matrix below.
struct Case {
    sample: &'static str,
    /// (codec_type, codec_id) per stream, in stream order.
    streams: &'static [(&'static str, &'static str)],
    /// Packets per stream index, in demux order.
    packet_counts: &'static [(usize, usize)],
    /// First packet pts (ms) per stream index.
    first_pts: &'static [(usize, i64)],
}

/// A demuxer test: open the sample through our registry, compare streams and
/// packets with the hardcoded ffprobe expectations, then compare the full
/// packet sequence (per-stream index/pts) with a live `ffprobe -show_packets`
/// run.
fn check(case: &Case) {
    let path = fate(case.sample);
    let mut ctx = oxideav_core::RuntimeContext::new();
    demux_asf::register(&mut ctx);
    let file = std::fs::File::open(&path).unwrap();
    let mut demuxer = ctx
        .containers
        .open_demuxer("asf", Box::new(file), &ctx.codecs)
        .unwrap_or_else(|e| panic!("{}: open: {e}", case.sample));

    // Streams: codec type and id per stream.
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

    // Read all packets; count them and note the first pts per stream.
    let mut counts = vec![0usize; streams.len()];
    let mut first_pts = vec![i64::MIN; streams.len()];
    let mut seq: Vec<(u32, i64)> = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(pkt) => {
                let i = pkt.stream_index as usize;
                assert!(i < counts.len(), "{}: packet on stream {i}", case.sample);
                counts[i] += 1;
                if first_pts[i] == i64::MIN {
                    first_pts[i] = pkt.pts.unwrap_or(i64::MIN);
                }
                seq.push((pkt.stream_index, pkt.pts.unwrap_or(i64::MIN)));
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("{}: demux: {e}", case.sample),
        }
    }

    for &(stream, n) in case.packet_counts {
        assert_eq!(
            counts[stream], n,
            "{}: stream {stream} packet count",
            case.sample
        );
    }
    for &(stream, pts) in case.first_pts {
        assert_eq!(
            first_pts[stream], pts,
            "{}: stream {stream} first pts",
            case.sample
        );
    }

    // Live ffprobe comparison: same packet count overall and identical
    // (stream, pts) sequence where ffprobe knows the pts.
    let ffpackets = ffprobe_packets(&path);
    assert_eq!(
        seq.len(),
        ffpackets.len(),
        "{}: total packet count vs ffprobe",
        case.sample
    );
    for (i, pair) in seq.iter().zip(&ffpackets).enumerate() {
        let (mstream, mpts) = *pair.0;
        let (tstream, tpts) = *pair.1;
        assert_eq!(mstream, tstream, "{}: packet {i} stream", case.sample);
        if let Some(t) = tpts {
            assert_eq!(mpts, t, "{}: packet {i} pts", case.sample);
        }
    }
}

/// `(stream_index, pts_ms)` per packet, from the pinned `ffprobe -show_packets`.
fn ffprobe_packets(path: &std::path::Path) -> Vec<(u32, Option<i64>)> {
    let out = std::process::Command::new(refcheck::pinned_ffprobe())
        .args(["-v", "error", "-show_packets", "-of", "csv"])
        .arg(path)
        .output()
        .expect("the pinned ffprobe runs");
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

// ─── matrix: every FATE sample in an ASF/WMV/WMA container ───

#[test]
fn asf_bug821_repldata() {
    check(&Case {
        sample: "asf/bug821-2.asf",
        streams: &[("video", "msmpeg4v3")],
        packet_counts: &[(0, 14)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn wmv8_x8intra() {
    check(&Case {
        sample: "wmv8/wmv8_x8intra.wmv",
        streams: &[("audio", "wmav2"), ("video", "wmv2")],
        packet_counts: &[(0, 110), (1, 474)],
        first_pts: &[(0, 0), (1, 0)],
    });
}

#[test]
fn wmv_drm() {
    check(&Case {
        sample: "wmv8/wmv_drm.wmv",
        streams: &[("audio", "wmavoice"), ("video", "wmv3")],
        packet_counts: &[(0, 20), (1, 130)],
        first_pts: &[(0, 0), (1, 0)],
    });
}

#[test]
fn wmapro_2ch() {
    check(&Case {
        sample: "wmapro/Beethovens_9th-1_small.wma",
        streams: &[("audio", "wmapro")],
        packet_counts: &[(0, 6)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn wmapro_51() {
    check(&Case {
        sample: "wmapro/latin_192_mulitchannel_cut.wma",
        streams: &[("audio", "wmapro")],
        packet_counts: &[(0, 4)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn wmavoice_7k() {
    check(&Case {
        sample: "wmavoice/streaming_CBR-7K.wma",
        streams: &[("audio", "wmavoice")],
        packet_counts: &[(0, 32)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn wmavoice_11k() {
    check(&Case {
        sample: "wmavoice/streaming_CBR-11K.wma",
        streams: &[("audio", "wmavoice")],
        packet_counts: &[(0, 61)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn wmavoice_19k() {
    check(&Case {
        sample: "wmavoice/streaming_CBR-19K.wma",
        streams: &[("audio", "wmavoice")],
        packet_counts: &[(0, 61)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn cover_art_wma() {
    check(&Case {
        sample: "cover_art/Californication_cover.wma",
        streams: &[("video", "mjpeg"), ("audio", "wmav2")],
        packet_counts: &[(0, 1), (1, 9)],
        first_pts: &[(1, 0)],
    });
}

#[test]
fn cover_art_wma_id3() {
    check(&Case {
        sample: "cover_art/wma_with_ID3_APIC_trimmed.wma",
        streams: &[("video", "mjpeg"), ("audio", "wmav2")],
        packet_counts: &[(0, 1), (1, 60)],
        first_pts: &[(1, 0)],
    });
}

#[test]
fn cover_art_wma_metadata_library() {
    check(&Case {
        sample: "cover_art/wma_with_metadata_library_object_tag_trimmed.wma",
        streams: &[("video", "mjpeg"), ("video", "mjpeg"), ("audio", "wmav2")],
        packet_counts: &[(0, 1), (1, 1), (2, 20)],
        first_pts: &[(2, 0)],
    });
}

#[test]
fn lossless_wma() {
    check(&Case {
        sample: "lossless-audio/luckynight-partial.wma",
        streams: &[("audio", "wmalossless")],
        packet_counts: &[(0, 79)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn lossless_wma24_1() {
    check(&Case {
        sample: "lossless-audio/master_audio_2.0_24bit.wma",
        streams: &[("audio", "wmalossless")],
        packet_counts: &[(0, 6)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn lossless_wma24_2() {
    check(&Case {
        sample: "lossless-audio/Mega_Weird_Audio_Test_24bit.wma",
        streams: &[("audio", "wmalossless")],
        packet_counts: &[(0, 2)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn lossless_wma24_rawtile() {
    check(&Case {
        sample: "lossless-audio/g2_24bit.wma",
        streams: &[("audio", "wmalossless")],
        packet_counts: &[(0, 5)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn mss1() {
    check(&Case {
        sample: "mss1/screen_codec.wmv",
        streams: &[("audio", "wmav2"), ("video", "mss1")],
        packet_counts: &[(0, 96), (1, 20)],
        first_pts: &[(0, 0), (1, 291)],
    });
}

#[test]
fn mss2_rlepal() {
    check(&Case {
        sample: "mss2/rlepal.wmv",
        streams: &[("video", "mss2")],
        packet_counts: &[(0, 2)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn mss2_rlepals() {
    check(&Case {
        sample: "mss2/rlepals.wmv",
        streams: &[("video", "mss2")],
        packet_counts: &[(0, 2)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn mss2_rle555() {
    check(&Case {
        sample: "mss2/rle555.wmv",
        streams: &[("video", "mss2")],
        packet_counts: &[(0, 2)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn mss2_rle555s() {
    check(&Case {
        sample: "mss2/rle555s.wmv",
        streams: &[("video", "mss2")],
        packet_counts: &[(0, 2)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn mss2_screen_codec() {
    check(&Case {
        sample: "mss2/msscreencodec.wmv",
        streams: &[("audio", "wmapro"), ("video", "mss2")],
        packet_counts: &[(0, 96), (1, 228)],
        first_pts: &[(0, 29), (1, 0)],
    });
}

#[test]
fn mss2_region() {
    check(&Case {
        sample: "mss2/mss2_2.wmv",
        streams: &[("video", "mss2")],
        packet_counts: &[(0, 2)],
        first_pts: &[(0, 0)],
    });
}

/// FFmpeg microsoft.mak: `FATE_MTS2-$(call FRAMECRC, ASF, MTS2)` — the ASF
/// demuxer carries an MTS2 (xesc) video stream.
#[test]
fn mts2_xesc() {
    check(&Case {
        sample: "mts2/sample.xesc",
        streams: &[("video", "mts2")],
        packet_counts: &[(0, 17)],
        first_pts: &[(0, 0)],
    });
}

/// FFmpeg microsoft.mak: `FATE_MICROSOFT-$(call FRAMECRC, ASF, MTS2)` —
/// ScreenCapture.xesc demuxed through the ASF demuxer.
#[test]
fn mts2_screen_capture() {
    check(&Case {
        sample: "mts2/ScreenCapture.xesc",
        streams: &[("video", "mts2")],
        packet_counts: &[(0, 128)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn g2m2() {
    check(&Case {
        sample: "g2m/g2m2.asf",
        streams: &[("video", "g2m"), ("audio", "wmav2")],
        packet_counts: &[(0, 160), (1, 17)],
        first_pts: &[(0, 47), (1, 0)],
    });
}

#[test]
fn g2m3() {
    check(&Case {
        sample: "g2m/g2m3.asf",
        streams: &[
            ("audio", "wmav2"),
            ("data", "unknown"),
            ("video", "g2m"),
        ],
        packet_counts: &[(0, 56), (1, 1), (2, 351)],
        first_pts: &[(0, 0), (1, 0), (2, 0)],
    });
}

#[test]
fn g2m4() {
    check(&Case {
        sample: "g2m/g2m4.asf",
        streams: &[("video", "g2m")],
        packet_counts: &[(0, 28)],
        first_pts: &[(0, 0)],
    });
}

/// FFmpeg screen.mak: `FATE_SCREEN-$(call FRAMECRC, ASF, TDSC) += fate-tdsc`
/// on tdsc/tdsc.asf — TDSC video plus an MP3 track.
#[test]
fn tdsc() {
    check(&Case {
        sample: "tdsc/tdsc.asf",
        streams: &[("video", "tdsc"), ("audio", "mp3")],
        packet_counts: &[(0, 42), (1, 99)],
        first_pts: &[(0, 0), (1, 20)],
    });
}

#[test]
fn adpcm_argo_mono() {
    check(&Case {
        sample: "argo-asf/PWIN22M.ASF",
        streams: &[("audio", "adpcm_argo")],
        packet_counts: &[(0, 431)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn adpcm_argo_stereo() {
    check(&Case {
        sample: "argo-asf/CBK2_cut.asf",
        streams: &[("audio", "adpcm_argo")],
        packet_counts: &[(0, 256)],
        first_pts: &[(0, 0)],
    });
}

#[test]
fn truncated_and_mutated_files_never_panic() {
    let src = fate("asf/bug821-2.asf");
    let data = std::fs::read(&src).unwrap();

    // Truncations.
    for len in [
        0usize,
        1,
        16,
        17,
        31,
        63,
        100,
        1000,
        data.len() / 2,
        data.len() - 1,
    ] {
        let _ = demux_all(&data[..len.min(data.len())]);
    }

    // Bit flips at a fixed seed, >= 2000 mutations.
    let mut state = 0x2545F4914F6CDD1Du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..2000 {
        let mut mutated = data.clone();
        let mutations = 1 + (next() % 8) as usize;
        for _ in 0..mutations {
            let pos = (next() as usize) % mutated.len();
            mutated[pos] ^= (next() & 0xFF) as u8 | 1;
        }
        let _ = demux_all(&mutated);
    }

    // Extra: mutate the wmv8 sample too (video + audio + payload extensions).
    let data2 = std::fs::read(fate("wmv8/wmv8_x8intra.wmv")).unwrap();
    for _ in 0..500 {
        let mut mutated = data2.clone();
        let pos = (next() as usize) % mutated.len();
        mutated[pos] ^= (next() & 0xFF) as u8 | 1;
        let _ = demux_all(&mutated);
    }
}

/// Demux every packet of `data`, discarding results; errors are fine, panics are not.
fn demux_all(data: &[u8]) -> Result<(), oxideav_core::Error> {
    let mut ctx = oxideav_core::RuntimeContext::new();
    demux_asf::register(&mut ctx);
    let mut demuxer = ctx.containers.open_demuxer(
        "asf",
        Box::new(std::io::Cursor::new(data.to_vec())),
        &ctx.codecs,
    )?;
    loop {
        match demuxer.next_packet() {
            Ok(_) => {}
            Err(oxideav_core::Error::Eof) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}
