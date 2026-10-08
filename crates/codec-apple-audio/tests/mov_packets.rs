//! QuickTime sound tables count one entry per sample (`stts` durations of
//! 1, `stsz` sizes of 1) for compressed and uncompressed sound alike.
//! FFmpeg's MOV demuxer groups them into packets per chunk (`mov_build_index`:
//! whole frames of `samplesPerPacket` samples and `bytesPerFrame` bytes, up
//! to 1024 samples, or fixed sizes for MACE, IMA4 and GSM). The production
//! demuxer, as the player opens the file, must read FFmpeg's packets: every
//! packet's pts, dts, duration, size and bytes equal ffprobe's (FFmpeg
//! 2da55bf, whose `mov.c` the grouping ports).
//!
//! The oracle runs with `-ignore_editlist 1`: the player opens MOV files
//! without the demuxer's edit-list mode (`apply_edit_lists`), so packets
//! stay on the media timeline, and FFmpeg's grouping is compared there.
//! With edits applied FFmpeg also drops packets past an edit's end and
//! shifts and discards others (`mov_fix_index`).

mod support;

use std::path::Path;
use std::process::Command;

use support::{archive, audio_packets};

/// `(pts, dts, duration, size, md5)` of one packet.
type Row = (Option<i64>, Option<i64>, Option<i64>, usize, String);

/// ffprobe's packets of the first audio stream of `path`.
fn ffprobe_packets(path: &Path) -> Vec<Row> {
    let out = Command::new(refcheck::pinned_ffprobe())
        .args(["-v", "error", "-ignore_editlist", "1", "-select_streams", "a:0"])
        .args(["-show_entries", "packet=pts,dts,duration,size,data_hash"])
        .args(["-show_data_hash", "md5", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    let number = |s: &str| s.parse::<i64>().ok();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            (number(f[0]), number(f[1]), number(f[2]), f[3].parse().unwrap(), f[4].trim_start_matches("MD5:").to_string())
        })
        .collect()
}

fn assert_packets_are_ffmpegs(path: &Path) {
    let expected = ffprobe_packets(path);
    let (_, packets) = audio_packets(path);
    let actual: Vec<Row> =
        packets.iter().map(|p| (p.pts, p.dts, p.duration, p.data.len(), refcheck::md5_hex(&p.data))).collect();
    let name = path.file_name().unwrap().to_string_lossy();
    let first_wrong = actual.iter().zip(&expected).position(|(a, b)| a != b);
    assert!(
        first_wrong.is_none() && actual.len() == expected.len(),
        "{name}: {} packets, FFmpeg {}; first difference at {first_wrong:?}: ours {:?}, FFmpeg {:?}",
        actual.len(),
        expected.len(),
        first_wrong.and_then(|i| actual.get(i)),
        first_wrong.and_then(|i| expected.get(i)),
    );
    println!("{name}: {} packets equal FFmpeg's", actual.len());
}

/// FATE's QuickTime sound suite: MACE 3:1 and 6:1, IMA4, QDM2, and the
/// uncompressed and Microsoft ADPCM tables FFmpeg groups the same way.
#[test]
fn qt_surge_sound_packets_are_ffmpegs() {
    for name in [
        "surge-1-8-MAC3.mov", "surge-2-8-MAC3.mov", "surge-1-8-MAC6.mov", "surge-2-8-MAC6.mov",
        "surge-1-16-B-ima4.mov", "surge-2-16-B-ima4.mov", "surge-2-16-B-QDM2.mov",
        "surge-1-8-raw.mov", "surge-2-8-raw.mov", "surge-2-16-B-twos.mov", "surge-2-16-L-sowt.mov",
        "surge-1-16-B-alaw.mov", "surge-2-16-B-alaw.mov", "surge-1-16-B-ulaw.mov", "surge-2-16-B-ulaw.mov",
        "surge-2-16-L-ms02.mov", "surge-2-16-L-ms11.mov",
    ] {
        assert_packets_are_ffmpegs(&refcheck::fate(&format!("qt-surge-suite/{name}")));
    }
}

/// QDM2 and QDMC from FFmpeg's sample archive (`tests/data/ffmpeg-samples`).
#[test]
fn archive_qdm2_and_qdmc_packets_are_ffmpegs() {
    let mut files: Vec<String> =
        [8, 10, 12, 16, 20, 24, 32, 40, 48, 64].iter().map(|k| format!("A-codecs/QDM2/sweep/0-22050HzSweep{k}kb.mov")).collect();
    files.extend(
        [
            "A-codecs/QDM2/sweep/0-2222050HzSweep24kbQT.mov",
            "A-codecs/QDM2/fft8/resurrection.mov",
            "A-codecs/QDMC/rumcoke.mov",
            "A-codecs/QDMC/slick.mov",
            "A-codecs/QDMC/tidemo1-24bit-rle.mov",
        ]
        .map(String::from),
    );
    for file in files {
        assert_packets_are_ffmpegs(&archive(&file));
    }
}
