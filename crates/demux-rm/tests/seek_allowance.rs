//! A seek's allowance (1,048,576 packets, 256 MiB) covers the fixed
//! packet headers rm_read_dts reads (rmdec.c:rm_sync) as well as the
//! payloads it skips. On a file whose one video key frame is followed by
//! 70,000 packets of a stream it does not have, 4 KiB each, the seek ends
//! with ResourceExhausted having read and skipped at most 256 MiB, and
//! reading goes on as without it. A seek whose reposition to its landing
//! fails fails, and reading goes on as without it.

#[path = "../../demux-seek-core/tests/support/mod.rs"]
mod support;

use oxideav_core::{Demuxer, ReadSeek, RuntimeContext};
use refcheck::fate;
use support::{exhausts_within_allowance, final_reposition_rolls_back, Layout};

fn rm(input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    let mut ctx = RuntimeContext::new();
    demux_rm::register(&mut ctx);
    ctx.containers.open_demuxer("rm", input, &ctx.codecs).unwrap()
}

fn sample() -> Vec<u8> {
    std::fs::read(fate("sipr/sipr_5k0.rm")).unwrap()
}

/// Where the header chunk `tag` starts.
fn chunk(data: &[u8], tag: &[u8; 4]) -> usize {
    let mut at = 0;
    while &data[at..at + 4] != tag {
        at += u32::from_be_bytes(data[at + 4..at + 8].try_into().unwrap()) as usize;
    }
    at
}

/// sipr_5k0.rm's headers and first video key packet, without its index,
/// then data packets of stream 7, which it does not have: version 0,
/// length 4104 (12 header bytes and 4092 of payload).
#[test]
fn packet_headers_of_unknown_streams_are_charged() {
    let sample = sample();
    let data_start = chunk(&sample, b"DATA") + 18;
    let mut head = sample[..data_start].to_vec();
    let mut at = data_start;
    loop {
        let len = usize::from(u16::from_be_bytes([sample[at + 2], sample[at + 3]]));
        if u16::from_be_bytes([sample[at + 4], sample[at + 5]]) == 1 && sample[at + 11] & 2 != 0 {
            head.extend_from_slice(&sample[at..at + len]);
            break;
        }
        at += len;
    }
    let mut unit = vec![0, 0, 0x10, 0x08, 0, 7, 0, 0, 0, 0, 0, 0];
    unit.resize(4104, 0);
    let layout = Layout::new(head, unit, 70_000, Vec::new());
    exhausts_within_allowance("unknown streams' packets", rm, layout, 0, 1, 10_000);
}

/// The whole sample: the seek lands where FFmpeg's bisection does. The
/// first packets after the seek point come from the frames the SIPR
/// deinterleaver queued; 100 reach past them into the input.
#[test]
fn a_failed_reposition_rolls_back() {
    let layout = Layout::new(sample(), Vec::new(), 0, Vec::new());
    final_reposition_rolls_back("RM", rm, layout, 5, 1, 10_000, 100);
}
