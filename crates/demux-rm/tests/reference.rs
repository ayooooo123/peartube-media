// RealMedia demuxer reference tests: for every FATE sample carried in a
// RealMedia/RealAudio container, packet count, per-stream codec id and
// per-packet PTS must equal `ffprobe -show_packets`.
//
// Sample list from FFmpeg's FATE makefiles (tests/fate/real.mak,
// lossless-audio.mak): every test whose sample is an .rm/.rmvb/.ra container
// (real/, sipr/, realaudio/, lossless-audio/).

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
    demux_rm::register(&mut ctx);
    let file = std::fs::File::open(&path).unwrap();
    let mut demuxer = ctx
        .containers
        .open_demuxer("rm", Box::new(file), &ctx.codecs)
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
    // (stream, pts) sequence where ffprobe knows the pts. The rv34 pts
    // correction (rv34.rs, ported from libavcodec/rv34_parser.c) rewrites
    // rv30/rv40 B-frame timestamps exactly like FFmpeg's parser, so the
    // full sequence matches.
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

// ─── matrix: every FATE sample in a RealMedia/RealAudio container ───

/// FFmpeg real.mak: `fate-ra3-144` — RealAudio 1.0 (.ra, 14_4) in the old
/// `.ra` format: one big packet.
#[test]
fn ra3_144() {
    check(&Case {
        sample: "realaudio/ra3.ra",
        streams: &[("audio", "ra_144")],
        packet_counts: &[(0, 1)],
        first_pts: &[(0, 0)],    });
}

/// FFmpeg real.mak: `fate-ra4-288` — RealAudio 2.0 (28_8) in the old `.ra`
/// format, 72 interleaved blocks.
#[test]
fn ra4_288() {
    check(&Case {
        sample: "realaudio/ra4_288.ra",
        streams: &[("audio", "ra_288")],
        packet_counts: &[(0, 72)],
        first_pts: &[(0, 0)],    });
}

/// FFmpeg real.mak: `fate-ra-144` — RealAudio 1.0 inside a modern .rm.
#[test]
fn ra_144_in_rm() {
    check(&Case {
        sample: "real/ra3_in_rm_file.rm",
        streams: &[("audio", "ra_144")],
        packet_counts: &[(0, 454)],
        first_pts: &[(0, 0)],    });
}

/// FFmpeg real.mak: `fate-ra-288` — RealAudio 2.0 (int4 interleaver).
#[test]
fn ra_288() {
    check(&Case {
        sample: "real/ra_288.rm",
        streams: &[("audio", "ra_288")],
        packet_counts: &[(0, 2448)],
        first_pts: &[(0, 0)],    });
}

/// FFmpeg real.mak: `fate-ra-cook` — cook (genr interleaver).
#[test]
fn ra_cook() {
    check(&Case {
        sample: "real/ra_cook.rm",
        streams: &[("audio", "cook")],
        packet_counts: &[(0, 240)],
        first_pts: &[(0, 0)],    });
}

/// FFmpeg real.mak: `fate-rv30` — cook audio + rv30 video (sliced frames).
#[test]
fn rv30() {
    check(&Case {
        sample: "real/rv30.rm",
        streams: &[("audio", "cook"), ("video", "rv30")],
        packet_counts: &[(0, 160), (1, 109)],
        first_pts: &[(0, 0), (1, 1)],    });
}

/// FFmpeg real.mak: `fate-rv40` — cook audio + rv40 video
/// (spygames-2MB.rmvb; 2 MB of vbr frames, many multi-slice).
#[test]
fn rv40() {
    check(&Case {
        sample: "real/spygames-2MB.rmvb",
        streams: &[("audio", "cook"), ("video", "rv40")],
        packet_counts: &[(0, 880), (1, 521)],
        first_pts: &[(0, 0), (0, 0)],    });
}

/// FFmpeg real.mak: `fate-rv20-1239` — rv20-only file (G2).
#[test]
fn rv20() {
    check(&Case {
        sample: "real/G2_with_SVT_320_240.rm",
        streams: &[("video", "rv20")],
        packet_counts: &[(0, 145)],
        first_pts: &[(0, 1)],    });
}

/// FFmpeg real.mak: `fate-sipr-5k0` — sipr 5 kbit/s (block_align 19, one
/// audio frame per block) + rv10 video.
#[test]
fn sipr_5k0() {
    check(&Case {
        sample: "sipr/sipr_5k0.rm",
        streams: &[("audio", "sipr"), ("video", "rv10")],
        packet_counts: &[(0, 336), (1, 77)],
        first_pts: &[(0, -6), (1, 0)],    });
}

/// FFmpeg real.mak: `fate-sipr-6k5` — sipr 6.5 kbit/s (block_align 29) + rv30
/// video.
#[test]
fn sipr_6k5() {
    check(&Case {
        sample: "sipr/sipr_6k5.rm",
        streams: &[("audio", "sipr"), ("video", "rv30")],
        packet_counts: &[(0, 1680), (1, 876)],
        first_pts: &[(0, 0), (1, 1)],    });
}

/// FFmpeg real.mak: `fate-sipr-8k5` — sipr 8.5 kbit/s (block_align 37, 60 ms
/// frames, -4 first pts) + rv20 video.
#[test]
fn sipr_8k5() {
    check(&Case {
        sample: "sipr/sipr_8k5.rm",
        streams: &[("audio", "sipr"), ("video", "rv20")],
        packet_counts: &[(0, 1152), (1, 175)],
        first_pts: &[(0, -4), (1, 1)],    });
}

/// FFmpeg real.mak: `fate-sipr-16k` — sipr 16 kbit/s (block_align 20, 10 ms
/// frames) + rv20 video.
#[test]
fn sipr_16k() {
    check(&Case {
        sample: "sipr/sipr_16k.rm",
        streams: &[("audio", "sipr"), ("video", "rv20")],
        packet_counts: &[(0, 3360), (1, 42)],
        first_pts: &[(0, 0), (1, 0)],    });
}

/// FFmpeg lossless-audio.mak: `fate-ralf` — ralf (vbrf interleaver) in a
/// .rmvb.
#[test]
fn ralf() {
    check(&Case {
        sample: "lossless-audio/luckynight-partial.rmvb",
        streams: &[("audio", "ralf")],
        packet_counts: &[(0, 190)],
        first_pts: &[(0, 0)],    });
}

// ─── untrusted input: truncated and mutated files never panic ───

#[test]
fn truncated_and_mutated_files_never_panic() {
    // Small audio sample + a video sample, so both the audio-interleaver and
    // video-slice paths get mutated.
    let sources = [
        fate("sipr/sipr_5k0.rm"),
        fate("real/rv30.rm"),
        fate("realaudio/ra4_288.ra"),
    ];

    // Truncations.
    for src in &sources {
        let data = std::fs::read(src).unwrap();
        for len in [0usize, 1, 4, 10, 12, 16, 18, 27, 40, 100, 1000, data.len() / 2, data.len() - 1] {
            let _ = demux_all(&data[..len.min(data.len())]);
        }
    }

    // Bit flips at a fixed seed, >= 2000 mutations per sample.
    let mut state = 0x2545F4914F6CDD1Du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for src in &sources {
        let data = std::fs::read(src).unwrap();
        for _ in 0..2000 {
            let mut mutated = data.clone();
            let mutations = 1 + (next() % 8) as usize;
            for _ in 0..mutations {
                let pos = (next() as usize) % mutated.len();
                mutated[pos] ^= (next() & 0xFF) as u8 | 1;
            }
            let _ = demux_all(&mutated);
        }
    }
}

/// Demux every packet of `data`, discarding results; errors are fine, panics are not.
fn demux_all(data: &[u8]) -> Result<(), oxideav_core::Error> {
    let mut ctx = oxideav_core::RuntimeContext::new();
    demux_rm::register(&mut ctx);
    let mut demuxer = ctx.containers.open_demuxer(
        "rm",
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
