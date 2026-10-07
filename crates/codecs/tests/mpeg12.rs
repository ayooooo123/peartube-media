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
    let path = std::env::var_os("MPEG12_LATE_HEADER").map(PathBuf::from).unwrap_or_else(||
        PathBuf::from(std::env::var_os("HOME").unwrap()).join("projects/peartube-media-wt/.targets/t5/tmp/player-subtitle-canvas-16166/late-video-size.mpg"));
    let report = runner::compare(&path,&output("late-header")).unwrap();
    assert_eq!(report.opened_dimensions,(None,None));
    assert_eq!(report.initial_dimensions,Some((720,480)));
    assert!(report.geometry_packet.unwrap() < report.packets);
    assert!(report.before_eof > 0);
    assert_eq!(report.frames,10);
}
#[test]
fn sparse_pes_timestamps_match_ffmpeg_through_production_ps() {
    // FFmpeg's PS muxer packs several small pictures into each 2 KiB PES and
    // stamps only the first picture commencing in it; anchors and B-pictures
    // then lack their own PTS.
    let path = output("sparse-pts").with_file_name("sparse-pts.mpg");
    let status = std::process::Command::new("ffmpeg")
        .args(["-v","error","-nostdin","-y","-threads","1","-f","lavfi","-i","testsrc2=size=176x144:rate=25",
            "-frames:v","60","-c:v","mpeg2video","-threads","1","-bf","2","-g","12","-b:v","150k","-f","mpeg"])
        .arg(&path).status().expect("FFmpeg generates the sparse-PTS input");
    assert!(status.success());
    let report = runner::compare(&path,&output("sparse-pts")).unwrap();
    assert_eq!(report.frames,60);
    assert!(report.stamped_packets < report.frames, "{} stamped PES for {} pictures",report.stamped_packets,report.frames);
}
