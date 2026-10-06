//! OxideAV's PGS decoder (`oxideav-sub-image`, codec id `pgs`) against
//! FFmpeg's `pgssub` on the FATE PGS sample, through every container that
//! carries PGS.
//!
//! These tests pin OxideAV's current behaviour exactly. Where it differs
//! from FFmpeg the assertion spells out the difference, so a fix in OxideAV
//! (or a fork pinned in the workspace) fails the test and the assertion is
//! then tightened to plain equality with FFmpeg.

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{MediaType, RuntimeContext, TimeBase};
use refcheck::fate;
use support::{Match, ffmpeg_reference, shown_states, timeline, timeline_diffs};

/// One PDS entry: Y, Cr, Cb, alpha.
type Ycca = [u8; 4];

/// Palette entries of every PDS in a `.sup` file, in file order.
fn sup_palette_entries(sup: &[u8]) -> Vec<Ycca> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 13 <= sup.len() {
        assert_eq!(&sup[pos..pos + 2], b"PG", "segment header at {pos}");
        let kind = sup[pos + 10];
        let len = u16::from_be_bytes([sup[pos + 11], sup[pos + 12]]) as usize;
        let body = &sup[pos + 13..(pos + 13 + len).min(sup.len())];
        if kind == 0x14 {
            for e in body[2..].chunks_exact(5) {
                out.push([e[1], e[2], e[3], e[4]]);
            }
        }
        pos += 13 + len;
    }
    out
}

/// FFmpeg's palette conversion (`pgssubdec.c: parse_palette_segment`):
/// limited-range BT.709 above 576 lines, limited-range BT.601 otherwise
/// (`YUV_TO_RGB1_CCIR[_BT709]` + `YUV_TO_RGB2_CCIR`).
fn ffmpeg_rgb([y, cr, cb, _]: Ycca, height: usize) -> [u8; 3] {
    const SCALEBITS: i32 = 10;
    let fix = |x: f64| (x * f64::from(1 << SCALEBITS) + 0.5) as i32;
    let one_half = 1 << (SCALEBITS - 1);
    let (cb, cr) = (i32::from(cb) - 128, i32::from(cr) - 128);
    let (r_add, g_add, b_add) = if height > 576 {
        (
            one_half + fix(1.5747 * 255.0 / 224.0) * cr,
            one_half - fix(0.1873 * 255.0 / 224.0) * cb - fix(0.4682 * 255.0 / 224.0) * cr,
            one_half + fix(1.8556 * 255.0 / 224.0) * cb,
        )
    } else {
        (
            fix(1.40200 * 255.0 / 224.0) * cr + one_half,
            -fix(0.34414 * 255.0 / 224.0) * cb - fix(0.71414 * 255.0 / 224.0) * cr + one_half,
            fix(1.77200 * 255.0 / 224.0) * cb + one_half,
        )
    };
    let y = (i32::from(y) - 16) * fix(255.0 / 219.0);
    let cm = |v: i32| (v >> SCALEBITS).clamp(0, 255) as u8;
    [cm(y + r_add), cm(y + g_add), cm(y + b_add)]
}

/// OxideAV's conversion (`oxideav-sub-image` `pgs.rs: ycbcr_to_rgba`):
/// full-range BT.601, whatever the canvas height.
fn oxideav_rgb([y, cr, cb, _]: Ycca) -> [u8; 3] {
    let (y, cb, cr) = (i32::from(y), i32::from(cb) - 128, i32::from(cr) - 128);
    let r = y + ((91881 * cr) >> 16);
    let g = y - ((22554 * cb + 46802 * cr) >> 16);
    let b = y + ((116130 * cb) >> 16);
    [r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, b.clamp(0, 255) as u8]
}

/// `sub/pgs_sub.sup` holds one complete display set (two objects on a
/// 1920x1080 plane, shown from 67467 µs until the next set) and a second
/// set cut off before its END segment, which neither decoder shows.
///
/// OxideAV shows the right number of states at the right times with the
/// right shapes and transparency (alpha is identical on every pixel), but
/// paints every opaque pixel in the wrong colour: it converts PGS palette
/// entries as full-range BT.601, where FFmpeg (and the Blu-ray spec)
/// treats them as limited-range, BT.709 above 576 lines. Y=16 (black)
/// comes out as (16, 16, 16) instead of (0, 0, 0).
#[test]
fn pgs_sub_sup_differs_from_ffmpeg_only_in_palette_conversion() {
    let path = fate("sub/pgs_sub.sup");
    let reference = ffmpeg_reference(&path, 0);
    let (w, h) = (reference.width, reference.height);
    assert_eq!((w, h, reference.cues.len()), (1920, 1080, 1));
    let decoded = refcheck::decode(&path, &[oxideav_sub_image::register], MediaType::Subtitle, 0);
    assert_eq!(decoded.params.codec_id.as_str(), "pgs");
    let got = shown_states(&decoded.frames, TimeBase::new(1, 90_000), w, h);
    let want = timeline(&reference);

    // Count, times and shapes: alpha equal on every pixel.
    assert_eq!(got.len(), want.len(), "states shown");
    let alpha = |c: &[u8]| c.chunks_exact(4).map(|p| p[3]).collect::<Vec<u8>>();
    for (i, (g, r)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g.at_us, r.at_us, "state {i} time");
        assert!(alpha(&g.canvas) == alpha(&r.canvas), "state {i}: alpha planes differ");
    }

    // Colour: every visible pixel is one palette entry converted FFmpeg's
    // way in the reference and OxideAV's way in the decode.
    let entries = sup_palette_entries(&std::fs::read(&path).unwrap());
    let mut explained: HashMap<([u8; 4], [u8; 4]), Ycca> = HashMap::new();
    for e in &entries {
        let [r, g, b] = ffmpeg_rgb(*e, h);
        let [or, og, ob] = oxideav_rgb(*e);
        explained.insert(([r, g, b, e[3]], [or, og, ob, e[3]]), *e);
    }
    let mut wrong_colour = 0usize;
    let mut unexplained = Vec::new();
    for (i, (g, r)) in got.iter().zip(&want).enumerate() {
        for (n, (gp, rp)) in g.canvas.chunks_exact(4).zip(r.canvas.chunks_exact(4)).enumerate() {
            if rp[3] == 0 || gp == rp {
                continue;
            }
            wrong_colour += 1;
            let key = (<[u8; 4]>::try_from(rp).unwrap(), <[u8; 4]>::try_from(gp).unwrap());
            if !explained.contains_key(&key) {
                unexplained.push(format!("state {i} ({},{}): FFmpeg {rp:?} OxideAV {gp:?}", n % w, n / w));
            }
        }
    }
    assert!(unexplained.is_empty(), "colour differences not explained by the conversion: {:?}", &unexplained[..unexplained.len().min(8)]);
    // The gap is still open: every visible pixel that differs does so by
    // the conversion alone. A different count means OxideAV changed: if it
    // is 0, replace this test with a plain `timeline_diffs` comparison.
    assert_eq!(wrong_colour, 38320, "visible pixels in OxideAV's colour");
    assert_eq!(timeline_diffs(&want, &got, w, Match::Visible).len(), 1);
}

/// `ffmpeg -c:s copy` remux of `sub/pgs_sub.sup` into a scratch file, with
/// the given muxer arguments.
fn remux(path: &Path, name: &str, muxer: &[&str]) -> PathBuf {
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-copyts", "-i"])
        .arg(path)
        .args(["-map", "0:s:0", "-c:s", "copy"])
        .args(muxer)
        .arg(&out)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg remux to {name}");
    out
}

/// Matroska (`S_HDMV/PGS`) and Blu-ray M2TS (stream type 0x90) carry PGS
/// under codec id `hdmv_pgs_subtitle` (OxideAV's own mkv and mpegts
/// demuxers map it so), but `oxideav-sub-image` registers its decoder only
/// as `pgs`: PGS outside a bare `.sup` file finds no decoder at all.
#[test]
fn pgs_in_matroska_and_m2ts_finds_no_oxideav_decoder() {
    let sup = fate("sub/pgs_sub.sup");
    let cases: [(&str, &[&str], &str, fn(&mut RuntimeContext)); 2] = [
        ("pgs_sub.mks", &["-f", "matroska"], "matroska", oxideav_mkv::__oxideav_entry),
        ("pgs_sub.m2ts", &["-f", "mpegts", "-mpegts_m2ts_mode", "1"], "mpegts", oxideav_mpegts::__oxideav_entry),
    ];
    for (name, muxer, demuxer, register) in cases {
        let container = name;
        let file = remux(&sup, name, muxer);
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        oxideav_sub_image::register(&mut ctx);
        let input = std::fs::File::open(&file).unwrap();
        let demux = ctx.containers.open_demuxer(demuxer, Box::new(input), &ctx.codecs).unwrap();
        let stream = demux
            .streams()
            .iter()
            .find(|s| s.params.media_type == MediaType::Subtitle)
            .unwrap_or_else(|| panic!("{container}: no subtitle stream"));
        assert_eq!(stream.params.codec_id.as_str(), "hdmv_pgs_subtitle", "{container} codec id");
        let err = ctx.codecs.first_decoder(&stream.params).err();
        assert!(
            matches!(err, Some(oxideav_core::Error::CodecNotFound(_))),
            "{container}: PGS now has a decoder; replace this test with a reference comparison"
        );
    }
}
