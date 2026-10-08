//! The SCC demuxer against FFmpeg 2da55bf's (`refcheck::pinned_ffmpeg`'s
//! ffprobe) on FATE `sub/witch.scc` (pop-on captions, special characters,
//! split lines): the probe picks it, and every packet's pts, duration,
//! size and bytes equal FFmpeg's.

use std::fs::File;
use std::process::Command;

use oxideav_core::Error;

#[test]
fn witch_scc_packets_match_ffmpeg() {
    let path = refcheck::fate("sub/witch.scc");
    let ctx = codecs::context();
    assert_eq!(refcheck::probe_container(&ctx, &path).unwrap(), "scc");
    let mut demuxer = ctx.containers.open_demuxer("scc", Box::new(File::open(&path).unwrap()), &ctx.codecs).unwrap();
    assert_eq!(demuxer.streams()[0].params.codec_id.as_str(), "eia_608");
    let mut ours = Vec::new();
    loop {
        match demuxer.next_packet() {
            // ffprobe shows a zero duration as N/A.
            Ok(p) => ours.push(format!(
                "{},{},{},MD5:{}",
                p.pts.unwrap(),
                p.duration.filter(|&d| d != 0).map_or("N/A".into(), |d| d.to_string()),
                p.data.len(),
                refcheck::md5_hex(&p.data)
            )),
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    let out = Command::new(refcheck::pinned_ffmpeg().with_file_name("ffprobe"))
        .args(["-v", "error", "-show_data_hash", "md5", "-show_entries", "packet=pts,duration,size,data_hash", "-of", "csv=p=0"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let theirs: Vec<String> = String::from_utf8(out.stdout).unwrap().lines().map(str::to_string).collect();
    assert!(theirs.len() > 100, "{} FFmpeg packets", theirs.len());
    for (i, (a, b)) in ours.iter().zip(&theirs).enumerate() {
        assert_eq!(a, b, "packet {i}");
    }
    assert_eq!(ours.len(), theirs.len());
}
