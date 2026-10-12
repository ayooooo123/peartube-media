//! A seek's allowance (1,048,576 packets, 256 MiB) covers an access
//! unit's 2-byte header and the bytes mlp_parser's parity check reads
//! past the unit, as well as the unit. After one real TrueHD access unit,
//! units of the largest size (8190 bytes) whose header parity holds: a
//! seek far past them ends with ResourceExhausted having read at most
//! 256 MiB, and reading goes on as without it.

#[path = "../../demux-seek-core/tests/support/mod.rs"]
mod support;

use oxideav_core::{Demuxer, ReadSeek, RuntimeContext};
use refcheck::fate;
use support::{exhausts_within_allowance, Layout};

fn truehd(input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    let mut ctx = RuntimeContext::new();
    codec_mlp::register(&mut ctx);
    ctx.containers.open_demuxer("truehd", input, &ctx.codecs).unwrap()
}

#[test]
fn unit_headers_and_parity_lookahead_are_charged() {
    let sample = std::fs::read(fate("truehd/ticket-1726-monocut.thd")).unwrap();
    // The first unit, with its major sync.
    let first = usize::from(u16::from_be_bytes([sample[0], sample[1]]) & 0xFFF) * 2;
    let head = sample[..first].to_vec();
    // Length 4095 words; header bytes whose parity nibbles XOR to 0xF.
    let mut unit = vec![0x0F, 0xFF, 0x00, 0x00];
    unit.resize(8190, 0);
    let layout = Layout::new(head, unit, 34_000, Vec::new());
    exhausts_within_allowance("TrueHD units", truehd, layout, 0, 0, i64::MAX / 2);
}
