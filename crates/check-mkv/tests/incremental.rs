//! Incremental demuxing of a 50 MB file (60 s of 640x360 H.264 + AAC in
//! FFmpeg's default Matroska muxing): opening it and reading its first 10
//! packets reads less than 1 MB from the start of the file, plus the
//! Top-Level elements its SeekHead points at past that (the Cues), which
//! are reported. The same streams written to a pipe (no Cues, unknown
//! Segment size) read less than 1 MB in total.

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::Path;

use check_mkv::{Counting, Generated, Pkt, ReadLog, ffprobe_packets, generated};
use oxideav_core::ReadSeek;
use oxideav_mkv::ebml::read_element_header;

const FIRST_PACKETS: usize = 10;
const BUDGET: u64 = 1_000_000;

fn tmp() -> &'static Path {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
}

/// Opens `path` through a read-logging reader and returns its first
/// packets with the reads they took.
fn first_packets(path: &Path) -> (ReadLog, Vec<Pkt>) {
    let log = ReadLog::default();
    let file = File::open(path).expect("open file");
    let mut dmx = check_mkv::open(Box::new(Counting::new(file, log.clone()))).expect("open demuxer");
    let packets = (0..FIRST_PACKETS).map(|_| Pkt::of(&dmx.next_packet().expect("packet"))).collect();
    (log, packets)
}

/// The packets are ffprobe's first ones: same stream, size and data.
fn assert_first_packets_match(path: &Path, ours: &[Pkt]) {
    let theirs = ffprobe_packets(path, &["-read_intervals", "%+#10"]);
    assert!(theirs.len() >= FIRST_PACKETS, "ffprobe returned {} packets", theirs.len());
    for (i, (a, b)) in ours.iter().zip(&theirs).enumerate() {
        assert_eq!((a.stream, a.size, &a.md5), (b.stream, b.size, &b.md5), "packet {i}");
    }
}

/// `(element id, start, end)` of every Top-Level element the SeekHead
/// points at.
fn seek_head_targets(path: &Path) -> Vec<(u32, u64, u64)> {
    let rs: Box<dyn ReadSeek> = Box::new(File::open(path).expect("open file"));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open demuxer");
    let mut f = File::open(path).expect("open file");
    let ebml = read_element_header(&mut f).expect("EBML header");
    f.seek(SeekFrom::Current(ebml.size as i64)).expect("seek");
    read_element_header(&mut f).expect("Segment header");
    let segment_start = f.stream_position().expect("position");
    dmx.seek_entries()
        .iter()
        .filter_map(|e| {
            let start = segment_start + e.seek_position();
            f.seek(SeekFrom::Start(start)).ok()?;
            let h = read_element_header(&mut f).ok()?;
            Some((e.seek_id()?, start, start + h.header_len as u64 + h.size))
        })
        .collect()
}

#[test]
fn cues_at_end_file_reads_its_head_and_seek_head_targets() {
    let path = generated(tmp(), Generated::CuesAtEnd);
    let size = std::fs::metadata(&path).expect("metadata").len();
    assert!(size >= 50_000_000, "{size} bytes");
    let (log, packets) = first_packets(&path);
    assert_first_packets_match(&path, &packets);

    let targets = seek_head_targets(&path);
    let mut head_end = 0;
    let mut tail = Vec::new();
    for (start, end) in log.ranges() {
        if end <= BUDGET {
            head_end = head_end.max(end);
            continue;
        }
        let target = targets.iter().find(|t| start >= t.1 && end <= t.2).unwrap_or_else(|| {
            panic!("read {start}..{end} is past {BUDGET} bytes and outside the SeekHead targets {targets:x?}")
        });
        tail.push((target.0, target.1, target.2, start, end));
    }
    println!(
        "{}: {size} bytes; opening it and reading {FIRST_PACKETS} packets read {} bytes, \
         reaching byte {head_end} from the start",
        path.display(),
        log.total(),
    );
    for (id, el_start, el_end, start, end) in &tail {
        println!(
            "  SeekHead target 0x{id:X} at {el_start}..{el_end} ({} bytes): read {start}..{end}",
            el_end - el_start
        );
    }
    assert!(head_end < BUDGET);
}

#[test]
fn cues_less_file_reads_less_than_a_megabyte() {
    let path = generated(tmp(), Generated::NoCues);
    let (log, packets) = first_packets(&path);
    assert_first_packets_match(&path, &packets);

    let furthest = log.ranges().last().map_or(0, |r| r.1);
    println!(
        "{}: opening it and reading {FIRST_PACKETS} packets read {} bytes, reaching byte {furthest}",
        path.display(),
        log.total(),
    );
    assert!(furthest < BUDGET, "reads reached byte {furthest}");
    assert!(log.total() < BUDGET, "{} bytes read", log.total());
}
