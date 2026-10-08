//! Seek-table bounds are checked using a reader that cannot supply payload.
//! Even the old code can allocate only a small read buffer before it errors.
use super::*;
use std::io::Cursor;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

struct Guarded {
    header: Cursor<Vec<u8>>,
    payload_reads: Arc<AtomicUsize>,
}
impl Read for Guarded {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.header.position() >= self.header.get_ref().len() as u64 {
            self.payload_reads.fetch_add(1, Ordering::Relaxed);
            return Err(std::io::Error::other("test forbids reading frame payload"));
        }
        self.header.read(out)
    }
}
impl Seek for Guarded {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> { self.header.seek(from) }
}

fn file(channels: u16, bits: u16, rate: u32, samples: u32, sizes: &[u32]) -> Vec<u8> {
    let mut b = b"TTA1".to_vec();
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&bits.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&samples.to_le_bytes());
    b.extend_from_slice(&[0; 4]);
    for size in sizes { b.extend_from_slice(&size.to_le_bytes()); }
    b.extend_from_slice(&[0; 4]);
    b
}

#[test]
fn bounds_tta_rejects_oversized_table_entries_before_payload_reads() {
    for size in [256 * 1024 * 1024 + 1, u32::MAX] {
        let reads = Arc::new(AtomicUsize::new(0));
        let input = Guarded { header: Cursor::new(file(1, 16, 8000, 16, &[size])), payload_reads: reads.clone() };
        let ctx = oxideav_core::RuntimeContext::new();
        let opened = open(Box::new(input), &ctx.codecs);
        let rejected_at_open = opened.is_err();
        if let Ok(mut d) = opened { let _ = d.next_packet(); }
        assert_eq!(reads.load(Ordering::Relaxed), 0, "untrusted size {size} reached the payload reader");
        assert!(rejected_at_open, "size {size} survived seek-table validation");
    }
}

#[test]
fn bounds_tta_budget_includes_declared_pcm_and_seek_table() {
    // At 1 MHz and 16 channels, decoding retains interleaved i32 samples
    // and S32 output together. Leave room for both, plus the seek table.
    let samples = 1_000_000u32 * 256 / 245;
    let budget = 256 * 1024 * 1024 - u64::from(samples) * 16 * 8 - 4;
    let ctx = oxideav_core::RuntimeContext::new();
    assert!(open(Box::new(Cursor::new(file(16, 24, 1_000_000, samples, &[budget as u32]))), &ctx.codecs).is_ok());
    assert!(open(Box::new(Cursor::new(file(16, 24, 1_000_000, samples, &[budget as u32 + 1]))), &ctx.codecs).is_err());
}

#[test]
fn bounds_tta_keeps_short_last_packets_and_seek_offsets() {
    let mut data = file(1, 16, 8000, 2 * (8000 * 256 / 245), &[8, 9]);
    data.extend_from_slice(&[1; 8]);
    data.extend_from_slice(&[2; 5]);
    let ctx = oxideav_core::RuntimeContext::new();
    let mut d = open(Box::new(Cursor::new(data)), &ctx.codecs).unwrap();
    assert_eq!(d.next_packet().unwrap().data, vec![1; 8]);
    let last = d.next_packet().unwrap();
    assert_eq!(last.data, vec![2; 5]);
    assert_eq!(last.pts, Some(8000 * 256 / 245));
    assert!(matches!(d.next_packet(), Err(Error::Eof)));
    assert_eq!(d.seek_to(0, 0).unwrap(), 0);
    assert_eq!(d.next_packet().unwrap().data, vec![1; 8]);
}
