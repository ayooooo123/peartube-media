//! Raw elementary-stream units at the edges FFmpeg's parsers leave to
//! chance, through the player's registry:
//! - the unit an AC-3 / E-AC-3 parser hands over at the end of the input
//!   is timed from its own header, like every unit before it: a file of
//!   one frame has a timestamp and seeks to it, and a last E-AC-3 frame
//!   with fewer blocks lasts those blocks, not the frame before it;
//! - H.264 SEI payload type and size codes sum as FFmpeg sums them, in
//!   32 bits (h264_sei.c: an int type and an unsigned size): a sum past
//!   2^32 wraps, so its access unit is key exactly where FFmpeg's parser
//!   reads a recovery point out of it;
//! - a CAF header that claims more frames than an i64 holds opens without
//!   a frame count, as FFmpeg's cafdec.c leaves `nb_frames` unset.

use oxideav_core::{Demuxer, Error, Packet};
use refcheck::fate;

/// The production `format` demuxer on `data`, and every packet it gives.
fn demux(format: &str, data: Vec<u8>) -> (Box<dyn Demuxer>, Vec<Packet>) {
    let ctx = codecs::context();
    let mut demuxer = ctx.containers.open_demuxer(format, Box::new(std::io::Cursor::new(data)), &ctx.codecs).unwrap();
    let mut packets = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) => packets.push(p),
            Err(Error::Eof) => return (demuxer, packets),
            Err(e) => panic!("{format}: demux: {e}"),
        }
    }
}

/// CRC-16 ANSI (polynomial 0x8005, initial value 0): the frame check of
/// AC-3 and E-AC-3.
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
    }
    crc
}

// ───────────────────────── AC-3 / E-AC-3 ─────────────────────────

/// A file holding only the first frame of a FATE AC-3 / E-AC-3 sample:
/// the parser hands it over at the end of the input. It comes out timed
/// as it is inside the whole file (pts 0, its own duration), and seeking
/// to 0 returns it.
#[test]
fn a_single_frame_is_timed_and_seekable() {
    for (rel, format) in [("ac3/monsters_inc_2.0_192_small.ac3", "ac3"), ("eac3/csi_miami_5.1_256_spx_small.eac3", "eac3")] {
        let (_, whole) = demux(format, std::fs::read(fate(rel)).unwrap());
        let frame = whole[0].data.clone();
        assert!(whole[0].duration.is_some_and(|d| d > 0), "{rel}: the first frame inside the file is timed");

        let (mut demuxer, alone) = demux(format, frame.clone());
        assert_eq!(alone.len(), 1, "{rel}: one frame");
        let timing = |p: &Packet| (p.pts, p.dts, p.duration);
        assert_eq!(timing(&alone[0]), (Some(0), Some(0), whole[0].duration), "{rel}: the lone frame's timing");

        let landed = demuxer.seek_to(0, 0).unwrap_or_else(|e| panic!("{rel}: seek to 0: {e}"));
        assert_eq!(landed, 0, "{rel}: seek lands on the frame");
        let again = demuxer.next_packet().unwrap_or_else(|e| panic!("{rel}: after seeking to 0: {e}"));
        assert!(again.data == frame, "{rel}: the frame after seeking");
        assert_eq!(timing(&again), timing(&alone[0]), "{rel}: its timing after seeking");
    }
}

/// Four six-block E-AC-3 frames, then the fifth recoded to one block
/// (numblkscod 0) with its CRC restored: the last unit lasts 256 samples
/// (480 ticks at 48 kHz), not the 1536 of the frame before it. The
/// frames before it keep their timing.
#[test]
fn the_last_eac3_frame_lasts_its_own_blocks() {
    let rel = "eac3/csi_miami_5.1_256_spx_small.eac3";
    let (_, whole) = demux("eac3", std::fs::read(fate(rel)).unwrap());
    let mut stream: Vec<u8> = whole[..4].iter().flat_map(|p| p.data.clone()).collect();
    let mut last = whole[4].data.clone();
    assert_eq!((last[4] >> 4) & 3, 3, "{rel}: six-block frames");
    last[4] &= !0x30;
    let n = last.len();
    let crc = crc16(&last[2..n - 2]);
    last[n - 2..].copy_from_slice(&crc.to_be_bytes());
    assert_eq!(crc16(&last[2..]), 0, "recoded frame passes its CRC");
    stream.extend_from_slice(&last);

    let (_, packets) = demux("eac3", stream);
    let timing: Vec<(Option<i64>, Option<i64>)> = packets.iter().map(|p| (p.pts, p.duration)).collect();
    assert_eq!(
        timing,
        [(Some(0), Some(2880)), (Some(2880), Some(2880)), (Some(5760), Some(2880)), (Some(8640), Some(2880)), (Some(11520), Some(480))],
        "pts and duration of each frame"
    );
}

// ───────────────────────── H.264 SEI ─────────────────────────

/// An Annex B NAL unit.
fn nal(header: u8, body: &[u8]) -> Vec<u8> {
    [&[0, 0, 0, 1, header][..], body].concat()
}

/// The access unit made of an SEI NAL carrying `message` and a P slice
/// (first_mb 0, slice_type 0, pps 0) with no parameter sets: our key
/// flag, and that of FFmpeg's parser (ffprobe) on the same raw stream.
fn key_flags(name: &str, message: &[u8]) -> (bool, bool) {
    let mut stream = nal(0x06, &[message, &[0x80]].concat());
    stream.extend(nal(0x41, &[0xE0, 0x00]));
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("demux-misc-units-{}-{name}.h264", std::process::id()));
    std::fs::write(&path, &stream).unwrap();
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-f", "h264", "-show_entries", "packet=flags", "-of", "csv=p=0"])
        .arg(&path)
        .output()
        .expect("ffprobe must be on PATH");
    let _ = std::fs::remove_file(&path);
    let flags = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(flags.lines().count(), 1, "{name}: FFmpeg's one access unit: {flags:?}");
    let (_, packets) = demux("h264", stream);
    assert_eq!(packets.len(), 1, "{name}: one access unit");
    (packets[0].flags.keyframe, flags.starts_with('K'))
}

/// Payload type and size are sums of bytes, 255 meaning "more follows".
/// 16843009 bytes of 255 then 7 sum to 2^32 + 6, which FFmpeg's 32-bit
/// sum reads as 6, a recovery point; 2^32 + 1 as a size reads as 1. A
/// recovery point is key, another message type is not.
#[test]
fn sei_type_and_size_sums_wrap_as_ffmpegs() {
    let run = vec![0xFF; 16_843_009];
    for (name, message, key) in [
        ("recovery-point", vec![6, 1, 0x80], true),
        ("user-data", vec![5, 1, 0x80], false),
        ("wrapped-type", [&run[..], &[7, 1, 0x80]].concat(), true),
        ("wrapped-size", [&[6][..], &run, &[2, 0x80]].concat(), true),
    ] {
        assert_eq!(key_flags(name, &message), (key, key), "{name}: (ours, FFmpeg's) key flags");
    }
}

// ───────────────────────── CAF ─────────────────────────

/// 68 bytes: a CAF header of MACE 6:1 (constant 1-byte packets of 6
/// frames) and a data chunk declaring 2^62 bytes, with nothing after it.
/// 2^62 packets of 6 frames overflow i64; cafdec.c checks
/// `data_size / bytes_per_packet < INT64_MAX / frames_per_packet` and
/// leaves the count unset. The file opens with no duration and ends.
#[test]
fn a_caf_frame_count_past_i64_is_left_unset() {
    let mut caf = b"caff\0\x01\0\0desc".to_vec();
    caf.extend_from_slice(&32i64.to_be_bytes());
    caf.extend_from_slice(&8000f64.to_be_bytes());
    caf.extend_from_slice(b"MAC6");
    for field in [0u32, 1, 6, 1, 0] {
        // flags, bytes per packet, frames per packet, channels, bits
        caf.extend_from_slice(&field.to_be_bytes());
    }
    caf.extend_from_slice(b"data");
    caf.extend_from_slice(&(1i64 << 62).to_be_bytes());
    caf.extend_from_slice(&[0; 4]); // edit count
    assert_eq!(caf.len(), 68);
    let (demuxer, packets) = demux("caf", caf);
    assert_eq!(demuxer.streams()[0].duration, None);
    assert!(packets.is_empty());
}
