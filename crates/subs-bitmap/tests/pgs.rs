//! PGS against FFmpeg. FATE's PGS sample `sub/pgs_sub.sup` (the input of
//! FFmpeg's `fate-sub2video_time_limited`, `fate-sub-pgs-remux` and
//! `fate-matroska-pgs-remux`) goes through this crate's `sup` demuxer, and
//! through OxideAV's Matroska and MPEG-TS demuxers after FFmpeg remuxes it as
//! those FATE tests do; every decode must equal FFmpeg's decode of the same
//! file: the same subtitles at the same times with the same ends, painted on
//! byte-identical canvases.

mod support;

use std::path::Path;

use oxideav_core::RuntimeContext;
use refcheck::{Registrar, fate};
use support::{Match, cue_diffs, decode_subtitles, decoded_cues, ffmpeg_reference, ffprobe_packets, reference_cues, remux};

/// Decodes subtitle stream 0 of `path` and compares it with FFmpeg; returns
/// the number of subtitles.
fn assert_matches_ffmpeg(path: &Path, registrars: &[Registrar]) -> usize {
    let reference = ffmpeg_reference(path, 0);
    let decoded = decode_subtitles(path, registrars, 0);
    assert!(decoded.errors.is_empty(), "{}: decode errors {:?}", path.display(), decoded.errors);
    let want = reference_cues(&reference);
    let got = decoded_cues(&decoded.frames, decoded.stream.time_base, reference.width, reference.height);
    let diffs = cue_diffs(&want, &got, reference.width, Match::Exact);
    assert!(diffs.is_empty(), "{}:\n{}", path.display(), diffs.join("\n"));
    want.len()
}

#[test]
fn sup_demuxer_matches_ffprobe_packets() {
    let path = fate("sub/pgs_sub.sup");
    let want = ffprobe_packets(&path, 0);
    let mut ctx = RuntimeContext::new();
    subs_bitmap::register(&mut ctx);
    let file = std::fs::File::open(&path).unwrap();
    let mut demux = ctx.containers.open_demuxer("sup", Box::new(file), &ctx.codecs).unwrap();
    assert_eq!(demux.streams().len(), 1);
    assert_eq!(demux.streams()[0].params.codec_id.as_str(), "hdmv_pgs_subtitle");
    let mut got = Vec::new();
    while let Ok(p) = demux.next_packet() {
        got.push(p);
    }
    assert_eq!(got.len(), want.len(), "packet count");
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g.pts, w.pts, "packet {i} pts");
        // A zero dts in the file is unset; libavformat then reports the pts.
        assert_eq!(g.dts.or(g.pts), w.dts, "packet {i} dts");
        assert_eq!(g.data.len(), w.size, "packet {i} size");
        assert_eq!(refcheck::md5_hex(&g.data), w.md5, "packet {i} data");
    }
}

#[test]
fn pgs_sub_sup_matches_ffmpeg() {
    let path = fate("sub/pgs_sub.sup");
    assert_eq!(assert_matches_ffmpeg(&path, &[subs_bitmap::register]), 1);
}

/// FFmpeg's Matroska muxer stores each PGS segment as a block, or a whole
/// display set per block behind `pgs_frame_merge` (how mkvmerge stores
/// them); both go through OxideAV's Matroska demuxer (`S_HDMV/PGS`).
#[test]
fn pgs_in_matroska_matches_ffmpeg() {
    let sup = fate("sub/pgs_sub.sup");
    let registrars: &[Registrar] = &[oxideav_mkv::__oxideav_entry, subs_bitmap::register];
    let segments = remux(&sup, 0, "pgs_segments.mks", &["-f", "matroska"]);
    assert_eq!(assert_matches_ffmpeg(&segments, registrars), 1);
    let sets = remux(&sup, 0, "pgs_display_sets.mks", &["-bsf:s", "pgs_frame_merge", "-f", "matroska"]);
    assert_eq!(assert_matches_ffmpeg(&sets, registrars), 1);
}

/// Blu-ray M2TS (stream type 0x90) through OxideAV's MPEG-TS demuxer.
#[test]
fn pgs_in_m2ts_matches_ffmpeg() {
    let sup = fate("sub/pgs_sub.sup");
    let m2ts = remux(&sup, 0, "pgs_sub.m2ts", &["-f", "mpegts", "-mpegts_m2ts_mode", "1"]);
    assert_eq!(assert_matches_ffmpeg(&m2ts, &[oxideav_mpegts::__oxideav_entry, subs_bitmap::register]), 1);
}
