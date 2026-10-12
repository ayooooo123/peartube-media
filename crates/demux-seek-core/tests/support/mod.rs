//! Seek test support for the demuxer crates, included with `#[path]`.
//!
//! A virtual input made of a head, a unit repeated many times and a tail,
//! so hostile files of hundreds of MiB cost no memory. It counts the bytes
//! read from it and those skipped forward by relative seeks (up to its
//! end), and can fail one absolute seek. On top of it:
//! - [`exhausts_within_allowance`]: a seek over the input ends with
//!   ResourceExhausted having read and skipped at most [`SEEK_BYTES`], and
//!   reading then goes on as if no seek had been made;
//! - [`final_reposition_rolls_back`]: the seek's last absolute seek, the
//!   reposition to its landing, fails once; the seek fails and reading
//!   goes on as if no seek had been made.

#![allow(dead_code)]

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use demux_seek_core::SEEK_BYTES;
use oxideav_core::{Demuxer, Error, ReadSeek};

/// `head`, then `unit` `count` times, then `tail`.
#[derive(Clone)]
pub struct Layout {
    pub head: Arc<Vec<u8>>,
    pub unit: Arc<Vec<u8>>,
    pub count: u64,
    pub tail: Arc<Vec<u8>>,
}

impl Layout {
    pub fn new(head: Vec<u8>, unit: Vec<u8>, count: u64, tail: Vec<u8>) -> Self {
        assert!(!unit.is_empty() || count == 0, "a repeated unit has bytes");
        Self { head: Arc::new(head), unit: Arc::new(unit), count, tail: Arc::new(tail) }
    }

    pub fn len(&self) -> u64 {
        self.head.len() as u64 + self.unit.len() as u64 * self.count + self.tail.len() as u64
    }

    /// The bytes from `pos` into `buf`, as many as there are.
    fn fill(&self, mut pos: u64, buf: &mut [u8]) -> usize {
        let (head, unit) = (self.head.len() as u64, self.unit.len() as u64);
        let units_end = head + unit * self.count;
        let mut done = 0;
        while done < buf.len() && pos < self.len() {
            let (src, at): (&[u8], u64) = if pos < head {
                (&self.head, pos)
            } else if pos < units_end {
                (&self.unit, (pos - head) % unit)
            } else {
                (&self.tail, pos - units_end)
            };
            let n = (src.len() - at as usize).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&src[at as usize..at as usize + n]);
            done += n;
            pos += n as u64;
        }
        done
    }

    /// The whole input, for inputs small enough to hold.
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = vec![0; self.len() as usize];
        self.fill(0, &mut out);
        out
    }
}

/// What a [`Virtual`] input reports: bytes read, bytes skipped forward by
/// relative seeks (up to the end), absolute seeks made, and the absolute
/// seek (counting from 1) that fails once, 0 for none.
#[derive(Default)]
pub struct Probe {
    pub read: AtomicU64,
    pub skipped: AtomicU64,
    pub absolute_seeks: AtomicU64,
    pub fail_absolute_seek: AtomicU64,
}

impl Probe {
    /// Bytes read and skipped so far.
    pub fn consumed(&self) -> u64 {
        self.read.load(Relaxed) + self.skipped.load(Relaxed)
    }
}

pub struct Virtual {
    layout: Layout,
    pos: u64,
    probe: Arc<Probe>,
}

impl Virtual {
    pub fn new(layout: Layout, probe: Arc<Probe>) -> Self {
        Self { layout, pos: 0, probe }
    }
}

impl Read for Virtual {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.layout.fill(self.pos, buf);
        self.pos += n as u64;
        self.probe.read.fetch_add(n as u64, Relaxed);
        Ok(n)
    }
}

impl Seek for Virtual {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.layout.len().checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        let target = target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        match to {
            SeekFrom::Start(_) => {
                let nth = self.probe.absolute_seeks.fetch_add(1, Relaxed) + 1;
                if nth == self.probe.fail_absolute_seek.load(Relaxed) {
                    return Err(io::Error::other("reposition rejected"));
                }
            }
            SeekFrom::Current(d) if d > 0 => {
                let skipped = target.min(self.layout.len()).saturating_sub(self.pos);
                self.probe.skipped.fetch_add(skipped, Relaxed);
            }
            _ => {}
        }
        self.pos = target;
        Ok(target)
    }
}

/// A packet as these checks compare it: stream, pts, dts, key flag,
/// payload size and a hash of the payload.
pub type Pkt = (u32, Option<i64>, Option<i64>, bool, usize, u64);

/// The next `n` packets, stopping at the first error.
pub fn next_packets(demuxer: &mut dyn Demuxer, n: usize) -> Vec<Pkt> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    (0..n)
        .map_while(|_| demuxer.next_packet().ok())
        .map(|p| {
            let mut hash = DefaultHasher::new();
            p.data.hash(&mut hash);
            (p.stream_index, p.pts, p.dts, p.flags.keyframe, p.data.len(), hash.finish())
        })
        .collect()
}

pub type Open = fn(Box<dyn ReadSeek>) -> Box<dyn Demuxer>;

fn opened(open: Open, layout: &Layout, probe: &Arc<Probe>, skip: usize) -> Box<dyn Demuxer> {
    let mut demuxer = open(Box::new(Virtual::new(layout.clone(), probe.clone())));
    assert_eq!(next_packets(&mut *demuxer, skip).len(), skip, "the input has {skip} packets to read before the seek");
    demuxer
}

/// Run `f` on a worker thread: its result, or a panic when it does not end
/// within two minutes (a loop) or panics.
pub fn bounded<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(value) => {
            worker.join().unwrap();
            value
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("{what} does not end"),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let _ = worker.join();
            panic!("{what} panicked")
        }
    }
}

/// After `skip` packets, a seek of `stream` to `ts` over `layout` fails
/// with ResourceExhausted having read and skipped at most SEEK_BYTES; the
/// next packets are those reading gives without the seek. Returns the
/// bytes it read and skipped.
pub fn exhausts_within_allowance(what: &str, open: Open, layout: Layout, skip: usize, stream: u32, ts: i64) -> u64 {
    let control = {
        let probe = Arc::new(Probe::default());
        let mut demuxer = opened(open, &layout, &probe, skip);
        next_packets(&mut *demuxer, 3)
    };
    let (exhausted, outcome, during, after) = bounded(what, move || {
        let probe = Arc::new(Probe::default());
        let mut demuxer = opened(open, &layout, &probe, skip);
        let before = probe.consumed();
        let result = demuxer.seek_to(stream, ts);
        let during = probe.consumed() - before;
        let exhausted = matches!(result, Err(Error::ResourceExhausted(_)));
        (exhausted, format!("{result:?}"), during, next_packets(&mut *demuxer, 3))
    });
    assert!(exhausted, "{what}: the seek ends on its allowance: {outcome} after {during} bytes");
    assert!(during <= SEEK_BYTES, "{what}: the seek read and skipped {during} bytes, over its allowance of {SEEK_BYTES}");
    assert_eq!(after, control, "{what}: reading after the failed seek goes on as without it");
    during
}

/// After `skip` packets, a seek of `stream` to `ts` over `layout` lands;
/// with the seek's last absolute seek (its reposition to the landing)
/// failing once it fails instead, and the next `n` packets are those
/// reading gives without the seek.
pub fn final_reposition_rolls_back(what: &str, open: Open, layout: Layout, skip: usize, stream: u32, ts: i64, n: usize) {
    let control = {
        let probe = Arc::new(Probe::default());
        let mut demuxer = opened(open, &layout, &probe, skip);
        next_packets(&mut *demuxer, n)
    };
    assert_eq!(control.len(), n, "{what}: {n} packets after the first {skip}");
    let last = {
        let probe = Arc::new(Probe::default());
        let mut demuxer = opened(open, &layout, &probe, skip);
        let before = probe.absolute_seeks.load(Relaxed);
        let landed = demuxer.seek_to(stream, ts);
        assert!(landed.is_ok(), "{what}: the seek lands: {landed:?}");
        let last = probe.absolute_seeks.load(Relaxed);
        assert!(last > before, "{what}: the seek repositions");
        last
    };
    let probe = Arc::new(Probe::default());
    let mut demuxer = opened(open, &layout, &probe, skip);
    probe.fail_absolute_seek.store(last, Relaxed);
    let failed = demuxer.seek_to(stream, ts);
    assert!(failed.is_err(), "{what}: the seek whose reposition failed fails: {failed:?}");
    assert_eq!(next_packets(&mut *demuxer, n), control, "{what}: reading after the failed seek goes on as without it");
}
