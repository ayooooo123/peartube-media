//! What the reference and mutation tests share: the production demuxers,
//! the inputs (FATE, FFmpeg's sample archive, files FFmpeg makes) and
//! their packets.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{CodecParameters, Error, MediaType, Packet, RuntimeContext};
use refcheck::Registrar;

/// The containers the player registers for these files, in its order.
pub const REGISTRARS: [Registrar; 6] = [
    codec_apple_audio::register,
    oxideav_iff::__oxideav_entry,
    oxideav_mkv::__oxideav_entry,
    oxideav_mov::registry::register,
    oxideav_mp4::__oxideav_entry,
    demux_misc::register,
];

pub fn run(binary: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new(binary)
        .args(["-v", "error", "-nostdin", "-y"])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{}: {e}", binary.display()));
    assert!(out.status.success(), "{} {args:?}: {}", binary.display(), String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// `refcheck::system_ffmpeg`: encodes and remuxes the generated inputs.
pub fn ffmpeg(args: &[&str]) -> Vec<u8> {
    run(&refcheck::system_ffmpeg(), args)
}

/// `name` in the persistent test scratch directory, made by `make` (given
/// a path to write) on first use, published by rename.
pub fn generated(name: &str, make: impl FnOnce(&Path)) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("codec-apple-audio");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    if !path.is_file() {
        let partial = dir.join(format!("{}.{name}", std::process::id()));
        make(&partial);
        std::fs::rename(&partial, &path).unwrap();
    }
    path
}

/// A file from FFmpeg's sample archive (samples.ffmpeg.org), hash-pinned
/// in `tests/data/ffmpeg-samples`: `$FFMPEG_SAMPLES/<archive path>`,
/// default ~/projects/oracles/ffmpeg-samples.
pub fn archive(relative: &str) -> PathBuf {
    let root = std::env::var_os("FFMPEG_SAMPLES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("projects/oracles/ffmpeg-samples"));
    let path = root.join(relative);
    assert!(path.is_file(), "missing {}: fetch it as tests/data/ffmpeg-samples/README.md says", path.display());
    path
}

/// `source`'s audio packets remuxed to CAF by FFmpeg, to a file: it writes
/// the packet table, after the audio data, only where it can seek back.
pub fn caf_remux(name: &str, source: &Path, map: &[&str]) -> PathBuf {
    generated(name, |out| {
        let mut args = vec!["-i", source.to_str().unwrap()];
        args.extend_from_slice(map);
        args.extend_from_slice(&["-c", "copy", "-f", "caf", out.to_str().unwrap()]);
        ffmpeg(&args);
    })
}

/// `source`'s audio track remuxed by FFmpeg to CAF (as
/// `fate-caf-qdm2-remux` and `fate-caf-mace6-remux` do), QDesign's
/// QuickTime atoms in the `kuki` chunk. `mov_packets.rs` checks the MOV
/// files' own packets against FFmpeg's.
pub fn track_caf(source: &Path) -> PathBuf {
    let stem = source.file_stem().unwrap().to_str().unwrap().to_string();
    caf_remux(&format!("{stem}.caf"), source, &["-map", "0:a"])
}

/// The first audio stream of `path` as the player opens it: its
/// parameters and every packet.
pub fn audio_packets(path: &Path) -> (CodecParameters, Vec<Packet>) {
    let mut ctx = RuntimeContext::new();
    for register in REGISTRARS {
        register(&mut ctx);
    }
    let format = refcheck::probe_container(&ctx, path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let file = std::fs::File::open(path).unwrap();
    let mut demuxer = ctx.containers.open_demuxer(&format, Box::new(file), &ctx.codecs).unwrap();
    let stream = demuxer
        .streams()
        .iter()
        .find(|s| s.params.media_type == MediaType::Audio)
        .unwrap_or_else(|| panic!("{}: no audio", path.display()))
        .clone();
    let mut packets = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) if p.stream_index == stream.index => packets.push(p),
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("{}: demux: {e}", path.display()),
        }
    }
    (stream.params, packets)
}
