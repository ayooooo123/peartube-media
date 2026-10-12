//! A seek's allowance (1,048,576 packets, 256 MiB) covers every byte the
//! seek reads or skips forward, each charged before the demuxer gets it.
//! Each hostile input below drives one read path (the review of 94c13d7,
//! attempt 4): MPEG-PS stuffing, program stream maps and private stream 2;
//! NUT size varints that never end; IVF frame headers; VOC extended
//! blocks; the reads of raw AC-3 and MPEG video. The seek ends with
//! ResourceExhausted having read and skipped at most 256 MiB, and reading
//! goes on as without it. A seek whose reposition to its landing fails
//! (NUT, PVA, MPEG-PS) fails, and reading goes on as without it too.

#[path = "../../demux-seek-core/tests/support/mod.rs"]
mod support;

use oxideav_core::{Demuxer, ReadSeek};
use support::{exhausts_within_allowance, final_reposition_rolls_back, Layout};

fn open(format: &str, input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    let ctx = codecs::context();
    ctx.containers.open_demuxer(format, input, &ctx.codecs).unwrap()
}

fn mpeg(input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    open("mpeg", input)
}

fn nut(input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    open("nut", input)
}

fn ivf(input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    open("ivf", input)
}

fn voc(input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    open("voc", input)
}

fn ac3(input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    open("ac3", input)
}

fn mpegvideo(input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    open("mpegvideo", input)
}

fn pva(input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    open("pva", input)
}

/// `name` made by the `ffmpeg` on PATH from `args`, once per test run.
fn generated(name: &str, args: &[&str]) -> Vec<u8> {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("demux-misc-seek-allowance");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}-{name}", std::process::id()));
    let out = std::process::Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y"])
        .args(args)
        .arg(&path)
        .output()
        .expect("the fixture FFmpeg runs");
    assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    let data = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    data
}

/// The first 96 KiB of FATE's matrixbench program stream (video from
/// 0.54 s), then `unit` repeated to about 275 MB.
fn program_stream(unit: Vec<u8>) -> Layout {
    let head = std::fs::read(refcheck::fate("mpeg2/matrixbench_mpeg2.lq1.mpg")).unwrap()[..96 * 1024].to_vec();
    let count = 275_000_000 / unit.len() as u64;
    Layout::new(head, unit, count, vec![0, 0, 1, 0xB9])
}

/// Video PES of the maximal length whose header is 65,534 stuffing bytes
/// and the MPEG-1 "no timestamp" byte: no payload, 65,541 bytes each.
#[test]
fn mpegps_pes_stuffing_is_charged() {
    let mut unit = vec![0, 0, 1, 0xE0, 0xFF, 0xFF];
    unit.resize(6 + 65_534, 0xFF);
    unit.push(0x0F);
    exhausts_within_allowance("PES stuffing", mpeg, program_stream(unit), 0, 0, 90_000 * 3600);
}

/// Program stream maps of the maximal length, 16,381 four-byte
/// elementary stream entries each.
#[test]
fn mpegps_program_stream_maps_are_charged() {
    let mut unit = vec![0, 0, 1, 0xBC, 0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xF5];
    for _ in 0..16_381 {
        unit.extend_from_slice(&[0x02, 0xE0, 0, 0]);
    }
    unit.extend_from_slice(&[0, 0, 0, 0, 0xFF]);
    assert_eq!(unit.len(), 4 + 2 + 0xFFFF);
    exhausts_within_allowance("program stream maps", mpeg, program_stream(unit), 0, 0, 90_000 * 3600);
}

/// Private stream 2 packets of the maximal length that are neither
/// Sofdec nor DVD navigation: read once, skipped after.
#[test]
fn mpegps_private_stream_2_is_charged() {
    let mut unit = vec![0, 0, 1, 0xBF, 0xFF, 0xFF];
    unit.resize(6 + 0xFFFF, 0);
    exhausts_within_allowance("private stream 2", mpeg, program_stream(unit), 0, 0, 90_000 * 3600);
}

/// A NUT without an index, then 64 KiB units: a syncpoint start code and
/// a packet size varint of 0x80 bytes, which keeps its value at 0 and
/// ends only at the next unit's start code. The search reads timestamps
/// from the end of the file, through those syncpoints.
#[test]
fn nut_endless_size_varints_are_charged() {
    let head = generated(
        "unindexed.nut",
        &[
            "-f", "lavfi", "-i", "testsrc=duration=4:size=64x64:rate=10", "-c:v", "mpeg2video", "-g", "8", "-bf", "0",
            "-write_index", "0", "-f", "nut",
        ],
    );
    let mut unit = 0x4E4B_E4AD_EECA_4569u64.to_be_bytes().to_vec();
    unit.resize(65_536, 0x80);
    let layout = Layout::new(head, unit, 4_600, Vec::new());
    exhausts_within_allowance("NUT size varints", nut, layout, 0, 0, 30);
}

/// VP8 IVF frames of 16 MiB with their headers, all at pts 0: a seek to
/// pts 1 reads on past every one. After 16 frames the seek has spent its
/// 256 MiB; the 17th frame's header is not read.
#[test]
fn ivf_frame_headers_are_charged() {
    let mut head = b"DKIF\0\0\x20\0VP80".to_vec();
    head.extend_from_slice(&[16, 0, 16, 0, 25, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let size = (1u32 << 24) - 12;
    let mut unit = size.to_le_bytes().to_vec();
    unit.extend_from_slice(&0u64.to_le_bytes());
    unit.resize(1 << 24, 0);
    exhausts_within_allowance("IVF frame headers", ivf, Layout::new(head, unit, 17, Vec::new()), 0, 0, 1);
}

/// A VOC whose one sample is followed by 40 M extended blocks (type 8):
/// each sets the next block's rate and channels, and the block reader
/// reads on to the next without a packet.
#[test]
fn voc_extended_blocks_are_charged() {
    let mut head = b"Creative Voice File\x1a".to_vec();
    head.extend_from_slice(&[26, 0, 0x0A, 0x01]);
    head.extend_from_slice(&(!0x010Au16).wrapping_add(0x1234).to_le_bytes());
    // 8000 Hz unsigned 8-bit, one sample
    head.extend_from_slice(&[1, 3, 0, 0, 131, 0, 0x80]);
    let unit = vec![8, 4, 0, 0, 0x00, 0xD3, 0, 0];
    exhausts_within_allowance("VOC extended blocks", voc, Layout::new(head, unit, 40_000_000, Vec::new()), 0, 0, 8000 * 3600);
}

/// Raw AC-3 read in 1 KiB pieces: FATE's monsters_inc 2.0 sample (192
/// kb/s) over and over, about 3.2 hours, a seek to 10 hours reading on to
/// the end.
#[test]
fn ac3_reads_are_charged_before_they_happen() {
    let unit = std::fs::read(refcheck::fate("ac3/monsters_inc_2.0_192_small.ac3")).unwrap();
    exhausts_within_allowance("raw AC-3", ac3, Layout::new(Vec::new(), unit, 2_800, Vec::new()), 0, 0, 90_000 * 36_000);
}

/// Raw MPEG-2 video read in 1 KiB pieces: FATE's sony-ct3 sample over and
/// over, a seek to an hour reading on to the end.
#[test]
fn mpegvideo_reads_are_charged_before_they_happen() {
    let unit = std::fs::read(refcheck::fate("mpeg2/sony-ct3.bs")).unwrap();
    exhausts_within_allowance("raw MPEG video", mpegvideo, Layout::new(Vec::new(), unit, 2_000, Vec::new()), 0, 0, 1_200_000 * 3600);
}

fn whole(data: Vec<u8>) -> Layout {
    Layout::new(data, Vec::new(), 0, Vec::new())
}

/// NUT with its index: read_seek lands on an index entry.
#[test]
fn nut_failed_reposition_rolls_back() {
    let data = generated(
        "indexed.nut",
        &[
            "-f", "lavfi", "-i", "sine=frequency=1000:duration=4", "-f", "lavfi", "-i",
            "testsrc=duration=4:size=64x64:rate=10", "-c:a", "mp2", "-c:v", "mpeg2video", "-g", "8", "-bf", "0",
            "-shortest", "-f", "nut",
        ],
    );
    final_reposition_rolls_back("NUT", nut, whole(data), 5, 1, 23, 8);
}

/// PVA, the reading mid audio PES: the search bisects the video
/// timestamps.
#[test]
fn pva_failed_reposition_rolls_back() {
    let data = std::fs::read(refcheck::fate("pva/PVA_test-partial.pva")).unwrap();
    final_reposition_rolls_back("PVA", pva, whole(data), 7, 0, 1_708_177_500, 8);
}

/// MPEG-PS: the search bisects the video PES dts; the landing is
/// repositioned to with the parsers new. A minute of video read half
/// through (30 s), so that the packets after come from the input, not
/// from those queued while discovering the streams (the first 5 s).
#[test]
fn mpegps_failed_reposition_rolls_back() {
    let data = generated(
        "minute.mpg",
        &["-f", "lavfi", "-i", "testsrc=duration=60:size=160x120:rate=25", "-c:v", "mpeg2video", "-g", "12", "-bf", "0", "-f", "vob"],
    );
    let mut demuxer = mpeg(Box::new(std::io::Cursor::new(data.clone())));
    let packets = std::iter::from_fn(|| demuxer.next_packet().ok()).count();
    final_reposition_rolls_back("MPEG-PS", mpeg, whole(data), packets / 2, 0, 90_000 * 20, 8);
}
