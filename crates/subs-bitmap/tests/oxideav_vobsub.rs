//! OxideAV's VobSub support (`oxideav-sub-image`: codec id `vobsub` and its
//! `.idx`/`.sub` demuxer) against FFmpeg's `vobsub` demuxer and `dvdsub`
//! decoder on the FATE samples.
//!
//! These tests pin OxideAV's current behaviour exactly. Where it differs
//! from FFmpeg the assertion spells out the difference, so a fix in OxideAV
//! (or a fork pinned in the workspace) fails the test and the assertion is
//! then tightened to plain equality with FFmpeg.

mod support;

use std::path::{Path, PathBuf};

use oxideav_core::{Error, Frame, MediaType, RuntimeContext, TimeBase};
use refcheck::fate;
use support::{ffmpeg_reference, ffprobe_packets, to_us};

/// The 16 RGB entries of an `.idx` `palette:` line.
fn idx_palette(idx: &str) -> [[u8; 3]; 16] {
    let line = idx.lines().find_map(|l| l.strip_prefix("palette:")).expect("palette line");
    let mut out = [[0u8; 3]; 16];
    for (i, hex) in line.split(',').map(str::trim).enumerate() {
        let v = u32::from_str_radix(hex, 16).unwrap();
        out[i] = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
    }
    out
}

/// FATE's `sub/vobsub.idx` with a leading `# idx-path:` comment, the only
/// way OxideAV's demuxer finds the `.sub` beside an `.idx`.
fn pointed_idx(path: &Path) -> PathBuf {
    let idx = std::fs::read_to_string(path).unwrap();
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("vobsub_pointed.idx");
    std::fs::write(&out, format!("# idx-path: {}\n{idx}", path.display())).unwrap();
    out
}

/// FFmpeg reads 46 SPU packets from `sub/vobsub.sub` and decodes 43
/// subtitles; OxideAV's `vobsub` demuxer never opens the `.sub` beside the
/// `.idx` (it only follows a `# idx-path:` comment of its own tests) and
/// returns no packets.
#[test]
fn vobsub_idx_yields_no_packets() {
    let path = fate("sub/vobsub.idx");
    assert_eq!(ffprobe_packets(&path, 0).len(), 46);
    assert_eq!(support::ffprobe_subtitles(&path, 0).len(), 43);
    let decoded = refcheck::decode(&path, &[oxideav_sub_image::register], MediaType::Subtitle, 0);
    assert_eq!(decoded.params.codec_id.as_str(), "vobsub");
    assert_eq!(decoded.frames.len(), 0, "OxideAV now reads the .sub: replace this test with a reference comparison");
}

/// OxideAV's demuxer and decoder on the FATE SPUs, with the `.sub` made
/// reachable through that comment:
///
/// * three SPUs span two PS packs; FFmpeg hands each half over as a packet
///   and its decoder joins them, stamped with the second half's time.
///   OxideAV's demuxer emits the joined SPU stamped with the first half's
///   time (185919 / 302018 / 328369 ms where FFmpeg says 185910 / 302035 /
///   328361), then the second half again, which the decoder rejects;
/// * each frame is the SPU's own 720x478 display area, not a canvas: its
///   (0, 2) offset is lost and the player draws it at (0, 0);
/// * no end: FFmpeg ends every subtitle at its stop command (the first
///   4960 ms after its start), OxideAV emits nothing for it;
/// * SET_COLOR / SET_CONTR nibbles are read from the wrong end, so pixel
///   value v takes the colour FFmpeg (and the DVD spec) give value 3 - v:
///   the yellow text with a dark outline renders dark with a yellow
///   outline. Alpha still matches only because these SPUs' contrast
///   pattern (0, 15, 15, 0) is symmetric.
#[test]
fn vobsub_sub_spus_differ_from_ffmpeg() {
    let path = fate("sub/vobsub.idx");
    let palette = idx_palette(&std::fs::read_to_string(&path).unwrap());
    let reference = ffmpeg_reference(&path, 0);
    let (cw, ch) = (reference.width, reference.height);
    assert_eq!((cw, ch, reference.cues.len()), (720, 480, 43));
    let ff_packets = ffprobe_packets(&path, 0);

    let mut ctx = RuntimeContext::new();
    oxideav_sub_image::register(&mut ctx);
    let input = std::fs::File::open(pointed_idx(&path)).unwrap();
    let mut demux = ctx.containers.open_demuxer("vobsub", Box::new(input), &ctx.codecs).unwrap();
    let stream = demux.streams()[0].clone();
    assert_eq!(stream.time_base, TimeBase::new(1, 1_000_000));
    let mut dec = ctx.codecs.first_decoder(&stream.params).unwrap();

    let mut packets = Vec::new();
    while let Ok(p) = demux.next_packet() {
        packets.push(p);
    }
    assert_eq!(packets.len(), ff_packets.len());
    let split = [4usize, 28, 37];
    for (i, (p, f)) in packets.iter().zip(&ff_packets).enumerate() {
        if split.contains(&i) {
            assert_eq!(p.data.len(), f.size + ff_packets[i + 1].size, "packet {i}: the joined SPU");
            assert_eq!(p.pts, f.pts.map(|t| t * 1000), "packet {i}: first half's time");
        } else if !split.contains(&(i.wrapping_sub(1))) {
            assert_eq!((p.data.len(), p.pts), (f.size, f.pts.map(|t| t * 1000)), "packet {i}");
        }
    }

    let mut cue = 0;
    let mut swapped = 0usize;
    for (i, p) in packets.iter().enumerate() {
        let sent = dec.send_packet(p);
        if split.contains(&(i.wrapping_sub(1))) {
            assert!(matches!(sent, Err(Error::InvalidData(_))), "packet {i}: the second half is rejected");
            continue;
        }
        sent.unwrap();
        let Ok(Frame::Video(v)) = dec.receive_frame() else { panic!("packet {i}: no frame") };
        assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)), "packet {i}: one frame, no end state");
        let (spu, pixels, (w, h)) = oxideav_sub_image::vobsub::parse_and_decode_spu(&p.data).unwrap();
        let (w, h) = (w as usize, h as usize);
        assert_eq!((spu.x1, spu.y1, w, h), (0, 2, 720, 478), "packet {i}: SPU area");
        assert_eq!((v.planes[0].stride, v.planes[0].data.len()), (w * 4, w * h * 4), "packet {i}: bitmap, not canvas");

        let want = &reference.cues[cue];
        let start = to_us(v.pts.unwrap(), stream.time_base);
        if split.contains(&i) {
            assert_ne!(start, want.sub.start_us(), "packet {i}: start");
        } else {
            assert_eq!(start, want.sub.start_us(), "packet {i}: start");
        }
        assert!(want.sub.end_us().is_some(), "FFmpeg ends subtitle {cue}");

        // FFmpeg's nibble order: value u uses colormap[u] / alpha[u], with
        // the first SET_COLOR byte holding entries 3 and 2.
        let ox_sel = spu.palette_sel;
        let ox_alpha = spu.alpha;
        let ff = |u: usize| {
            let rgb = palette[ox_sel[3 - u] as usize & 15];
            [rgb[0], rgb[1], rgb[2], (ox_alpha[3 - u] & 15) * 17]
        };
        let visible_eq = |a: &[u8], b: [u8; 4]| a[3] == b[3] && (b[3] == 0 || a == b);
        for y in 0..h {
            for x in 0..w {
                let u = pixels[y * w + x] as usize & 3;
                let c = ((spu.y1 as usize + y) * cw + spu.x1 as usize + x) * 4;
                let ffmpeg_px = &want.canvas[c..c + 4];
                let oxideav_px = &v.planes[0].data[(y * w + x) * 4..][..4];
                assert!(visible_eq(ffmpeg_px, ff(u)), "cue {cue} ({x},{y}): FFmpeg {ffmpeg_px:?} for value {u}");
                assert!(visible_eq(oxideav_px, ff(3 - u)), "cue {cue} ({x},{y}): OxideAV {oxideav_px:?} for value {u}");
                if ff(u)[3] != 0 && ff(u) != ff(3 - u) {
                    swapped += 1;
                }
            }
        }
        cue += 1;
    }
    assert_eq!(cue, 43);
    assert_eq!(swapped, 169_585, "visible pixels in the swapped colour");
}

/// Matroska carries VobSub as `S_VOBSUB`, which OxideAV's mkv demuxer maps
/// to codec id `dvd_subtitle` (with the `.idx` header as CodecPrivate); no
/// OxideAV decoder is registered under that id, so the VobSub tracks of
/// FATE's `filter/242_4.mkv` and `mkv/subtitle_zlib.mks` decode to nothing.
#[test]
fn vobsub_in_matroska_finds_no_oxideav_decoder() {
    for (sample, tracks) in [("filter/242_4.mkv", 3), ("mkv/subtitle_zlib.mks", 1)] {
        let path = fate(sample);
        assert_eq!(support::ffprobe_subtitles(&path, 0).is_empty(), false, "{sample}: FFmpeg decodes it");
        let mut ctx = RuntimeContext::new();
        oxideav_mkv::register(&mut ctx);
        oxideav_sub_image::register(&mut ctx);
        let input = std::fs::File::open(&path).unwrap();
        let demux = ctx.containers.open_demuxer("matroska", Box::new(input), &ctx.codecs).unwrap();
        let subs: Vec<_> = demux.streams().iter().filter(|s| s.params.media_type == MediaType::Subtitle).collect();
        assert_eq!(subs.len(), tracks, "{sample}");
        for s in subs {
            assert_eq!(s.params.codec_id.as_str(), "dvd_subtitle");
            assert!(
                matches!(ctx.codecs.first_decoder(&s.params), Err(Error::CodecNotFound(_))),
                "{sample}: VobSub in Matroska now has a decoder; replace this test with a reference comparison"
            );
        }
    }
}
