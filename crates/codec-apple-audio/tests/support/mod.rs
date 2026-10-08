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

/// The `ffmpeg` on PATH: encodes and remuxes the generated inputs.
pub fn ffmpeg(args: &[&str]) -> Vec<u8> {
    run(Path::new("ffmpeg"), args)
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

/// FFmpeg's CAF muxer writes the packet table (`pakt`) after the audio
/// data. Where the packets vary in size the table is their only framing,
/// and demux-misc reads chunks only up to `data`, as FFmpeg's demuxer does
/// on input it cannot seek; so for those the table moves in front of the
/// data. Constant-size packets stay as written: FFmpeg's demuxer counts
/// them from the data size, which it needs to have read first.
pub fn pakt_before_data(caf: &[u8]) -> Vec<u8> {
    let mut chunks = Vec::new();
    let mut pos = 8; // 'caff', version, flags
    while pos + 12 <= caf.len() {
        let size = i64::from_be_bytes(caf[pos + 4..pos + 12].try_into().unwrap());
        let end = if size < 0 { caf.len() } else { (pos + 12 + size as usize).min(caf.len()) };
        chunks.push(&caf[pos..end]);
        pos = end;
    }
    // desc: sample rate (8), format id (4), flags (4), bytes per packet
    // (4), frames per packet (4), ...
    let desc = chunks.iter().find(|c| &c[..4] == b"desc").expect("a desc chunk");
    let bytes_per_packet = u32::from_be_bytes(desc[28..32].try_into().unwrap());
    let frames_per_packet = u32::from_be_bytes(desc[32..36].try_into().unwrap());
    let data = chunks.iter().position(|c| &c[..4] == b"data").expect("a data chunk");
    if bytes_per_packet == 0 || frames_per_packet == 0 {
        if let Some(pakt) = chunks.iter().position(|c| &c[..4] == b"pakt").filter(|&p| p > data) {
            let moved = chunks.remove(pakt);
            chunks.insert(data, moved);
        }
    }
    let mut out = caf[..8].to_vec();
    for chunk in chunks {
        out.extend_from_slice(chunk);
    }
    out
}

/// `source`'s audio packets remuxed to CAF by FFmpeg (to a file: it writes
/// the packet table only where it can seek back), then
/// [`pakt_before_data`].
pub fn caf_remux(name: &str, source: &Path, map: &[&str]) -> PathBuf {
    generated(name, |out| {
        let mut args = vec!["-i", source.to_str().unwrap()];
        args.extend_from_slice(map);
        args.extend_from_slice(&["-c", "copy", "-f", "caf", out.to_str().unwrap()]);
        ffmpeg(&args);
        let caf = std::fs::read(out).unwrap();
        std::fs::write(out, pakt_before_data(&caf)).unwrap();
    })
}

/// `source`'s audio track remuxed by FFmpeg to CAF (as
/// `fate-caf-qdm2-remux` and `fate-caf-mace6-remux` do), QDesign's
/// QuickTime atoms in the `kuki` chunk. Read from the MOV files the
/// packets are not the codec's: oxideav-mov hands over the 1-byte samples
/// of QuickTime's compressed sound tables (and fails at the end of the
/// data), where FFmpeg's MOV demuxer groups them into packets.
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
