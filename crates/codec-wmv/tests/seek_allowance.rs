//! A seek's allowance (1,048,576 packets, 256 MiB) is charged before the
//! bytes are read: the 16 KiB pieces of a raw VC-1 stream and the 8-byte
//! frame headers of the VC-1 test format (.rcv). A seek reading on past
//! 256 MiB ends with ResourceExhausted having read at most that, and
//! reading goes on as without it.

#[path = "../../demux-seek-core/tests/support/mod.rs"]
mod support;

use oxideav_core::{Demuxer, ReadSeek, RuntimeContext};
use refcheck::fate;
use support::{exhausts_within_allowance, Layout};

fn open(format: &str, input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);
    ctx.containers.open_demuxer(format, input, &ctx.codecs).unwrap()
}

fn vc1(input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    open("vc1", input)
}

fn vc1test(input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    open("vc1test", input)
}

/// FATE's SA10091.vc1 over and over, a seek far past the end reading on.
#[test]
fn raw_vc1_reads_are_charged_before_they_happen() {
    let unit = std::fs::read(fate("vc1/SA10091.vc1")).unwrap();
    let layout = Layout::new(Vec::new(), unit, 640, Vec::new());
    exhausts_within_allowance("raw VC-1", vc1, layout, 0, 0, i64::MAX / 2);
}

/// SMM0015.rcv's header (25 fps), then key frames of 16 MiB with their
/// headers: after 16 the seek has spent its 256 MiB, and the 17th frame's
/// header is not read.
#[test]
fn rcv_frame_headers_are_charged() {
    let rcv = std::fs::read(fate("vc1/SMM0015.rcv")).unwrap();
    let size = u32::from_le_bytes(rcv[4..8].try_into().unwrap()) as usize;
    let head = rcv[..8 + size + 24].to_vec();
    let frame = (1u32 << 24) - 8;
    let mut unit = frame.to_le_bytes().to_vec();
    unit[3] = 0x80;
    unit.extend_from_slice(&[0; 4]);
    unit.resize(1 << 24, 0);
    exhausts_within_allowance("RCV frame headers", vc1test, Layout::new(head, unit, 17, Vec::new()), 0, 0, 1_000_000);
}
