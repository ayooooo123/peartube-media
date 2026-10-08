//! Frame-search regressions capped at a few 64 KiB reads on the old path.
use super::*;
use std::io::Cursor;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

struct Counted {
    input: Cursor<Vec<u8>>,
    bytes: Arc<AtomicUsize>,
}
impl Read for Counted {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.input.read(out)?;
        self.bytes.fetch_add(n, Ordering::Relaxed);
        Ok(n)
    }
}
impl Seek for Counted {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> { self.input.seek(from) }
}

fn header() -> Vec<u8> {
    let mut b = Vec::new();
    let mut bit = 0;
    // Mono, 16-bit, 8 kHz, 512 samples/frame. Include stream info so
    // the frame itself establishes the allocation bound.
    for (n, value) in [(16, 0xA0FFu64), (3, 2), (21, 0), (6, 2), (4, 0),
        (4, 7), (35, 0), (3, 0), (18, 2000), (5, 8), (4, 0), (1, 0), (6, 0)] {
        for i in 0..n {
            if bit % 8 == 0 { b.push(0); }
            b[bit / 8] |= ((value >> i & 1) as u8) << (bit % 8);
            bit += 1;
        }
    }
    let crc = crate::tak::crc24(0xCE_04B7, &b);
    b.extend_from_slice(&[(crc >> 16) as u8, (crc >> 8) as u8, crc as u8]);
    b
}

fn demux(data: Vec<u8>) -> (TakDemuxer, Arc<AtomicUsize>) {
    let bytes = Arc::new(AtomicUsize::new(0));
    let stream = CoreStreamInfo {
        index: 0, time_base: TimeBase::new(1, 8000), duration: None,
        start_time: Some(0), params: CodecParameters::audio(CodecId::new("tak")),
    };
    (TakDemuxer {
        input: Box::new(Counted { input: Cursor::new(data), bytes: bytes.clone() }),
        stream, data_start: 0, data_end: None, splitter: Splitter::new(0),
        pts: 0, index: Vec::new(),
    }, bytes)
}

#[test]
fn bounds_tak_missing_next_header_stops_reading() {
    let mut data = header();
    data.resize(3 * READ_CHUNK as usize, 0);
    let (mut d, bytes) = demux(data);
    let result = d.next_frame();
    let read = bytes.load(Ordering::Relaxed);
    assert!(read <= 32 * 1024, "missing next sync consumed {read} bytes for a 512-sample mono frame");
    assert!(result.is_err(), "oversized frame was emitted");
    assert!(d.splitter.buf.capacity() <= 32 * 1024, "oversized retained allocation");
}

#[test]
fn bounds_tak_discards_junk_but_retains_split_sync() {
    // FF is the very last byte of a read; its A0 and CRC arrive next.
    let junk = 3 * READ_CHUNK as usize - 1;
    let mut frame = header();
    frame.extend_from_slice(&[0; 20]);
    let mut data = vec![0; junk];
    data.extend_from_slice(&frame);
    data.extend_from_slice(&frame);
    let (mut d, _) = demux(data);
    let first = d.next_frame().unwrap().unwrap();
    assert_eq!(first.pos, junk as u64, "scanned junk must not become frame data");
    assert_eq!(first.data, frame);
    assert_eq!(first.duration, 512);
    assert!(d.splitter.buf.capacity() <= 2 * READ_CHUNK as usize);
    let last = d.next_frame().unwrap().unwrap();
    assert_eq!(last.pos, (junk + frame.len()) as u64);
    assert_eq!(last.data, frame);
    assert!(d.next_frame().unwrap().is_none());
}

#[test]
fn bounds_tak_sync_free_tail_is_not_a_packet() {
    let (mut d, _) = demux(vec![0; 3 * READ_CHUNK as usize]);
    assert!(d.next_frame().unwrap().is_none(), "sync-free bytes were emitted as audio");
    assert!(d.splitter.buf.capacity() <= 2 * READ_CHUNK as usize);
}
