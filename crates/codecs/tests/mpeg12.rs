//! Full-output production-registry regressions. Missing fixtures fail explicitly.
#[path = "../examples/mpeg12.rs"]
#[allow(dead_code)]
mod runner;
use std::path::PathBuf;

fn output(name: &str) -> PathBuf {
    let dir = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from)
        .expect("set CARGO_TARGET_DIR to a dedicated artifact directory").join("evidence");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("test-{name}.framemd5.tsv"))
}
fn corpus(name: &str) -> PathBuf {
    std::env::var_os("PEARTUBE_CORPUS_DIR").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join("projects/peartube-media-corpus"))
        .join(name)
}
#[test]
fn complete_manifest_mpeg_frames() {
    for path in [refcheck::fate("mpeg2/matrixbench_mpeg2.lq1.mpg"),corpus("mpeg1_mp2.mpg"),corpus("mpeg2_mp2.mpg")] {
        let name = path.file_name().unwrap().to_str().unwrap();
        runner::compare(&path,&output(name)).unwrap_or_else(|e|panic!("{}: {e}",path.display()));
    }
}
#[test]
fn complete_generated_hd_interlaced_and_mpeg1() {
    for name in ["mpeg2_720p30.ts","mpeg2_576i25.m2v","mpeg1_480p30.m1v"] {
        runner::compare(&corpus(&format!("perf/{name}")),&output(name)).unwrap_or_else(|e|panic!("{name}: {e}"));
    }
}
#[test]
fn unknown_at_open_geometry_is_published_before_eof() {
    let path = late_header_program_stream();
    let report = runner::compare(&path,&output("late-header")).unwrap();
    assert_eq!(report.opened_dimensions,(None,None));
    assert_eq!(report.initial_dimensions,Some((720,480)));
    assert!(report.geometry_packet.unwrap() < report.packets);
    assert!(report.before_eof > 0);
    assert_eq!(report.frames,10);
}
/// Ten 720x480 MPEG-2 intra pictures in a program stream whose first
/// sequence header sits behind 300 KB of zero-prefixed video PES: past PS's
/// 256 KiB video-parameter scan, so the size is unknown at open, but inside
/// its 1 MiB retained head. FFmpeg and production MPEG decode the unchanged
/// sequence that follows.
fn late_header_program_stream() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mpeg12-late-header.mpg");
    let status = std::process::Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "color=c=0x204060:s=720x480:r=5:d=2",
            "-c:v", "mpeg2video", "-bf", "0", "-g", "1", "-f", "vob"])
        .arg(&path)
        .status()
        .expect("the fixture FFmpeg runs");
    assert!(status.success());
    let original = std::fs::read(&path).unwrap();
    assert_eq!(&original[..4], &[0, 0, 1, 0xba]);
    let pack_end = 14 + usize::from(original[13] & 7);
    let mut bytes = original[..pack_end].to_vec();
    for _ in 0..5 {
        bytes.extend_from_slice(&[0, 0, 1, 0xe0]);
        bytes.extend_from_slice(&60_003u16.to_be_bytes());
        bytes.extend_from_slice(&[0x80, 0, 0]);
        bytes.resize(bytes.len() + 60_000, 0);
    }
    bytes.extend_from_slice(&original[pack_end..]);
    std::fs::write(&path, bytes).unwrap();
    path
}
#[test]
fn sparse_pes_timestamps_match_ffmpeg_through_production_ps() {
    // FFmpeg's PS muxer packs several small pictures into each 2 KiB PES and
    // stamps only the first picture commencing in it; anchors and B-pictures
    // then lack their own PTS.
    let path = output("sparse-pts").with_file_name("sparse-pts.mpg");
    let status = std::process::Command::new(refcheck::system_ffmpeg())
        .args(["-v","error","-nostdin","-y","-threads","1","-f","lavfi","-i","testsrc2=size=176x144:rate=25",
            "-frames:v","60","-c:v","mpeg2video","-threads","1","-bf","2","-g","12","-b:v","150k","-f","mpeg"])
        .arg(&path).status().expect("FFmpeg generates the sparse-PTS input");
    assert!(status.success());
    let report = runner::compare(&path,&output("sparse-pts")).unwrap();
    assert_eq!(report.frames,60);
    assert!(report.stamped_packets < report.frames, "{} stamped PES for {} pictures",report.stamped_packets,report.frames);
}
