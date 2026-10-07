//! A seek's allowance (1,048,576 packets, 256 MiB) is charged before the
//! bytes are read: the 1 KiB pieces of a raw DTS stream
//! (ff_raw_read_partial_packet). A seek reading on past 256 MiB ends with
//! ResourceExhausted having read at most that, and reading goes on as
//! without it.

#[path = "../../demux-seek-core/tests/support/mod.rs"]
mod support;

use oxideav_core::{Demuxer, ReadSeek, RuntimeContext};
use refcheck::fate;
use support::{exhausts_within_allowance, Layout};

fn dts(input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    let mut ctx = RuntimeContext::new();
    codec_dca::register(&mut ctx);
    ctx.containers.open_demuxer("dts", input, &ctx.codecs).unwrap()
}

/// FATE's dts_es.dts over and over, a seek far past the end reading on.
#[test]
fn raw_dts_reads_are_charged_before_they_happen() {
    let unit = std::fs::read(fate("dts/dts_es.dts")).unwrap();
    let layout = Layout::new(Vec::new(), unit, 560, Vec::new());
    exhausts_within_allowance("raw DTS", dts, layout, 0, 0, i64::MAX / 2);
}
