//! Raw elementary-stream units at the edges FFmpeg's parsers leave to
//! chance, through the player's registry: the unit an AC-3 / E-AC-3
//! parser hands over at the end of the input is timed from its own
//! header, like every unit before it. A file of one frame has a timestamp
//! and seeks to it, and a last E-AC-3 frame with fewer blocks lasts those
//! blocks, not the frame before it.

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
