//! Helpers shared by the reference and robustness tests.

use std::path::PathBuf;

/// Encodes a moving test pattern with FFmpeg's own encoder into AVI, the way
/// FATE's `vsynth` tests produce their WMV1 / MS-MPEG-4 v2 samples (the FATE
/// suite has no such files). Returns the path of the encoded sample; `name`
/// must be unique per test, since tests run in parallel.
pub fn encoded_sample(name: &str, size: &str, codec_args: &[&str]) -> PathBuf {
    let dir = std::env::temp_dir().join("codec-wmv-reference");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let out = dir.join(format!("{name}.avi"));
    let src = format!("testsrc2=size={size}:rate=25");
    let mut args = vec!["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", &src, "-frames:v", "40"];
    args.extend_from_slice(codec_args);
    args.extend_from_slice(&["-flags", "+bitexact", "-fflags", "+bitexact", out.to_str().unwrap()]);
    let st = std::process::Command::new("ffmpeg").args(&args).status().expect("ffmpeg must be on PATH");
    assert!(st.success(), "ffmpeg encode of {name} failed");
    out
}
