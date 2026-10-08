//! MPEG-4 Part 2 reference tests: the player's decode path for each file
//! (the product registry, `codecs::register_all`: FFmpeg's m4v demuxer port
//! for raw streams, AVI for packed B-frames, the forked oxideav-mpeg4video
//! decoder with its default options) against the FFmpeg the ports follow
//! (`refcheck::pinned_ffmpeg`, 2da55bf), frame for frame: every frame's
//! size, pixel format and MD5, in output order.
//!
//! FFmpeg decodes with `-idct simple`, the C `ff_simple_idct_put_int16_8bit`
//! the fork's IDCT reproduces (this host's default picks NEON assembly whose
//! scan-table permutation rounds some blocks differently), and
//! `-noautoscale`, which keeps each frame at the size it decodes at where a
//! stream changes size (FATE's resolution-change tests scale to the first
//! size instead).
//!
//! Samples come from FFmpeg's FATE suite (`refcheck::fate`) plus generated
//! streams (`tests/fixtures/`, see its README).

use oxideav_core::{Frame, MediaType};
use std::path::{Path, PathBuf};
use std::process::Command;

/// One output frame: width, height, FFmpeg's name for its pixel format,
/// and the MD5 of its planes packed without stride (framemd5's layout).
type FrameSig = (u32, u32, String, String);

fn run(binary: &Path, args: &[&str]) -> String {
    let out = Command::new(binary)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{}: {e}", binary.display()));
    assert!(out.status.success(), "{} {args:?}: {}", binary.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// FFmpeg's frames of the first video stream: each frame's size and
/// format from the pinned ffprobe, its MD5 from the pinned ffmpeg's
/// framemd5 in that format.
fn ffmpeg_frames(path: &Path) -> Vec<FrameSig> {
    let ffmpeg = refcheck::pinned_ffmpeg();
    let input = path.to_str().unwrap();
    let layouts: Vec<(u32, u32, String)> = run(
        &ffmpeg.with_file_name("ffprobe"),
        &["-v", "error", "-select_streams", "v:0", "-show_entries", "frame=width,height,pix_fmt", "-of", "csv=p=0", input],
    )
    .lines()
    .filter(|l| !l.trim().is_empty())
    .map(|l| {
        let f: Vec<&str> = l.trim().split(',').collect();
        (f[0].parse().unwrap(), f[1].parse().unwrap(), f[2].to_string())
    })
    .collect();
    let pix_fmt = layouts.first().map(|l| l.2.clone()).unwrap_or_else(|| panic!("{input}: FFmpeg decodes no frame"));
    let md5s = refcheck::parse_framemd5(&run(
        &ffmpeg,
        &[
            "-v", "error", "-nostdin", "-idct", "simple", "-i", input, "-map", "0:v:0", "-fps_mode", "passthrough",
            "-noautoscale", "-pix_fmt", &pix_fmt, "-f", "framemd5", "-",
        ],
    ));
    assert_eq!(layouts.len(), md5s.len(), "{input}: ffprobe and ffmpeg disagree on the frame count");
    layouts.into_iter().zip(md5s).map(|((w, h, f), md5)| (w, h, f, md5)).collect()
}

/// Our frames of the first video stream through the product registry:
/// each frame at the size and format its decoder reported for it.
fn our_frames(path: &Path) -> Vec<FrameSig> {
    let decoded = refcheck::decode(path, &[codecs::register_all], MediaType::Video, 0);
    decoded
        .frames
        .iter()
        .zip(&decoded.frame_video_layouts)
        .enumerate()
        .map(|(i, (frame, layout))| {
            let Frame::Video(v) = frame else { panic!("{}: frame {i} is not video", path.display()) };
            let (Some((w, h)), Some(format)) = *layout else {
                panic!("{}: frame {i}: the decoder reported {layout:?}", path.display())
            };
            let planes: Vec<(usize, usize)> = (0..format.plane_count())
                .map(|p| (format.plane_row_bytes(p, w).unwrap(), format.plane_dimensions(p, w, h).unwrap().1 as usize))
                .collect();
            (w, h, refcheck::ffmpeg_pix_fmt(format).to_string(), refcheck::md5_hex(&refcheck::pack(v, &planes)))
        })
        .collect()
}

/// Every frame of `path` as FFmpeg decodes it, in FFmpeg's order.
fn check(path: &Path) {
    let expected = ffmpeg_frames(path);
    let ours = our_frames(path);
    let first = ours.iter().zip(&expected).position(|(a, b)| a != b);
    assert!(
        ours.len() == expected.len() && first.is_none(),
        "{}: {} frames, FFmpeg {}; first differing frame {first:?}: ours {:?}, FFmpeg {:?}",
        path.display(),
        ours.len(),
        expected.len(),
        first.map(|k| &ours[k]),
        first.map(|k| &expected[k]),
    );
}

fn fate_mpeg4(name: &str) -> PathBuf {
    refcheck::fate(&format!("mpeg4/{name}"))
}

fn fixture(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    assert!(path.is_file(), "missing generated fixture {} (see tests/fixtures/README)", path.display());
    path
}

// ---- FATE (tests/fate/mpeg4.mak, xvid.mak) ----

/// Simple Profile from libavcodec 54 whose VOL declares a time increment
/// width its VOPs do not use: FFmpeg derives the width from the first VOP
/// and keeps it for the VOPs after it.
#[test]
fn fate_m4v_demo() {
    check(&fate_mpeg4("demo.m4v"));
}

/// Advanced Simple Profile from Xvid with B-frames and a custom MPEG
/// quantisation matrix: FFmpeg applies no mismatch control to intra
/// blocks.
#[test]
fn fate_xvid_vlc_trac7411() {
    check(&fate_mpeg4("xvid_vlc_trac7411.h263"));
}

/// DivX-style packed B-frames in AVI: a B-VOP rides in the packet of the
/// P-VOP before it and the next packet holds a not-coded placeholder,
/// whose place the B-VOP takes; the last packet's B-VOP, with no packet
/// after it, is not shown.
#[test]
fn fate_packed_bframes() {
    check(&fate_mpeg4("packed_bframes.avi"));
}

/// Simple Studio Profile, 4:2:2 at 10 bits, DPCM-coded.
#[test]
fn fate_mpeg4_sstp_dpcm() {
    check(&fate_mpeg4("mpeg4_sstp_dpcm.m4v"));
}

/// Resolution changes every ten frames through a VOL, 640x480 down to
/// 400x300 and on: at 400x300 the last macroblock row reaches past the
/// picture, and FFmpeg places the 8x8 blocks of four-vector macroblocks
/// within the picture.
#[test]
fn fate_mpeg4_resolution_change_down_down() {
    check(&fate_mpeg4("resize_down-down.h263"));
}

#[test]
fn fate_mpeg4_resolution_change_down_up() {
    check(&fate_mpeg4("resize_down-up.h263"));
}

#[test]
fn fate_mpeg4_resolution_change_up_down() {
    check(&fate_mpeg4("resize_up-down.h263"));
}

#[test]
fn fate_mpeg4_resolution_change_up_up() {
    check(&fate_mpeg4("resize_up-up.h263"));
}

// ---- Generated (tests/fixtures/README) ----

/// `-bf 2`: B-frames with FFmpeg's encoder defaults.
#[test]
fn generated_ipb_bf2() {
    check(&fixture("ipb_bf2_64x64.m4v"));
}

/// `-flags +qpel`
#[test]
fn generated_qpel() {
    check(&fixture("qpel_64x64.m4v"));
}

/// `-mbd rd -mpv_flags +qp_rd`
#[test]
fn generated_qp_rd() {
    check(&fixture("qp_rd_64x64.m4v"));
}

/// `-flags +ildct+ilme`
#[test]
fn generated_interlaced() {
    check(&fixture("ilaced_64x64.m4v"));
}

/// `-bf 2 -flags +aic+qpel`
#[test]
fn generated_ipb_aic_qpel() {
    check(&fixture("ipb_aic_qpel_64x64.m4v"));
}

/// 84x76, past the macroblock grid both ways, scrolling fast enough that
/// vectors point past the picture's edges: four-vector P-VOPs and
/// quarter-sample B-VOPs whose direct mode compensates 8x8 blocks, which
/// FFmpeg places within the picture.
#[test]
fn generated_qpel_mv4_edges() {
    check(&fixture("qpel_mv4_bf2_84x76.m4v"));
}
