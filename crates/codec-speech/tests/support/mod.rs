//! What the reference and mutation tests share.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run(binary: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new(binary)
        .args(["-v", "error"])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{}: {e}", binary.display()));
    assert!(out.status.success(), "{} {args:?}: {}", binary.display(), String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// `name` in the persistent scratch directory, made by
/// `refcheck::system_ffmpeg` from `input` with `output_args` on first use,
/// published by rename.
pub fn remux(name: &str, input: &Path, output_args: &[&str]) -> PathBuf {
    // Tests in one binary run in parallel and share the per-process partial
    // name: make one remux at a time so none renames another's file away.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("codec-speech");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    if !path.is_file() {
        let partial = dir.join(format!("{}.{name}", std::process::id()));
        let mut args = vec!["-nostdin", "-y", "-i", input.to_str().unwrap()];
        args.extend_from_slice(output_args);
        args.push(partial.to_str().unwrap());
        run(&refcheck::system_ffmpeg(), &args);
        std::fs::rename(&partial, &path).unwrap();
    }
    path
}

/// FATE's `amrwb/<name>.awb` (3GP) remuxed by FFmpeg to the raw
/// `#!AMR-WB` storage format: the `amr` demuxer's AMR-WB input, as FATE
/// has no raw AMR-WB file.
pub fn raw_amr_wb(name: &str) -> PathBuf {
    let source = refcheck::fate(&format!("amrwb/{name}.awb"));
    remux(&format!("{name}.amr"), &source, &["-c", "copy", "-f", "amr"])
}
