//! A seek's allowance (1,048,576 packets, 256 MiB) covers the reads that
//! build the Simple Index on the first seek (asf_build_simple_index): the
//! top-level objects after the data object and the index entries. On a
//! file with an index of 300 MB, or 12 M objects before it, the seek ends
//! with ResourceExhausted having read and skipped at most 256 MiB, and
//! reading goes on as without it. A seek whose reposition to its landing
//! fails fails, and reading goes on as without it, the packets queued and
//! assembled before it included.

#[path = "../../demux-seek-core/tests/support/mod.rs"]
mod support;

use oxideav_core::{Demuxer, ReadSeek, RuntimeContext};
use refcheck::fate;
use support::{exhausts_within_allowance, final_reposition_rolls_back, Layout};

fn asf(input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    let mut ctx = RuntimeContext::new();
    demux_asf::register(&mut ctx);
    ctx.containers.open_demuxer("asf", input, &ctx.codecs).unwrap()
}

/// wmv8_x8intra.wmv up to its Simple Index Object, which starts here.
const SIMPLE_INDEX: usize = 449_206;

fn sample() -> Vec<u8> {
    std::fs::read(fate("wmv8/wmv8_x8intra.wmv")).unwrap()
}

/// The sample's Simple Index Object replaced by one of 50 M entries, all
/// at the first packet.
#[test]
fn building_a_huge_simple_index_is_charged() {
    let data = sample();
    let at = SIMPLE_INDEX;
    let entries: u32 = 50_000_000;
    let mut head = data[..at + 16].to_vec();
    head.extend_from_slice(&(56 + 6 * u64::from(entries)).to_le_bytes());
    head.extend_from_slice(&data[at + 24..at + 40]);
    head.extend_from_slice(&10_000_000u64.to_le_bytes());
    head.extend_from_slice(&1u32.to_le_bytes());
    head.extend_from_slice(&entries.to_le_bytes());
    let unit = vec![0, 0, 0, 0, 1, 0];
    exhausts_within_allowance("Simple Index entries", asf, Layout::new(head, unit, u64::from(entries), Vec::new()), 0, 1, 10_500);
}

/// 12 M empty top-level objects of another kind between the data object
/// and the end of the file: the index search reads every header.
#[test]
fn objects_before_the_simple_index_are_charged() {
    let data = sample();
    let mut unit = vec![0x11; 16];
    unit.extend_from_slice(&24u64.to_le_bytes());
    let layout = Layout::new(data[..SIMPLE_INDEX].to_vec(), unit, 12_000_000, Vec::new());
    exhausts_within_allowance("objects after the data", asf, layout, 0, 1, 10_500);
}

/// The sample with its index: the seek lands on an index entry.
#[test]
fn a_failed_reposition_rolls_back() {
    let layout = Layout::new(sample(), Vec::new(), 0, Vec::new());
    final_reposition_rolls_back("ASF", asf, layout, 5, 1, 10_500, 6);
}
