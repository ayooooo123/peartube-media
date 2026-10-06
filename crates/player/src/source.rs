use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::{Condvar, Mutex, MutexGuard};

pub const RING_CAPACITY: usize = 32 * 1024 * 1024; // 32 MiB
const CHUNK_SIZE: usize = 128 * 1024; // 128 KiB per read

pub trait ReadSeekSend: Read + Seek + Send + 'static {}
impl<T: Read + Seek + Send + 'static> ReadSeekSend for T {}

struct RingState {
    buffer: Vec<u8>,
    capacity: usize,
    tail_pos: u64,
    head_pos: u64,
    cur_pos: u64,
    ring_start: usize,
    eof: bool,
    fatal_error: Option<String>,
    seek_req: Option<SeekFrom>,
    seek_res: Option<std::io::Result<u64>>,
    suspended: bool,
    stop: bool,
    /// The worker is inside a read or seek of the underlying source, with
    /// the ring unlocked. Over a stalled network that call blocks for as long
    /// as the peer withholds bytes.
    io: bool,
    /// The consumer is blocked in `read` or `seek` waiting for bytes that
    /// have not arrived: the ring is empty and the source is not at its end.
    starved: bool,
}

type StarveHook = Arc<dyn Fn(bool) + Send + Sync>;

struct SharedRing {
    state: Mutex<RingState>,
    consumer_cv: Condvar,
    worker_cv: Condvar,
    on_starved: Mutex<Option<StarveHook>>,
}

impl SharedRing {
    /// Records whether the consumer waits for bytes and reports a change to
    /// the hook, which runs with the ring unlocked.
    fn set_starved(&self, state: &mut MutexGuard<'_, RingState>, starved: bool) {
        if state.starved == starved {
            return;
        }
        state.starved = starved;
        let hook = self.on_starved.lock().clone();
        if let Some(hook) = hook {
            MutexGuard::unlocked(state, || hook(starved));
        }
    }

    /// Ends the read-ahead for good: the worker exits at its next look at
    /// the ring, and reads and seeks fail instead of waiting for bytes.
    fn stop(&self) -> MutexGuard<'_, RingState> {
        let mut state = self.state.lock();
        state.stop = true;
        self.worker_cv.notify_all();
        self.consumer_cv.notify_all();
        state
    }
}

/// What reads and seeks return once the ring is stopped. Not `Interrupted`:
/// `read_exact` and the demuxers' read loops retry that kind, and would spin.
fn stopped_error() -> std::io::Error {
    std::io::Error::other("source stopped")
}

/// The engine's handle on a source it handed to a demuxer: starvation
/// reports, suspend/resume of the read-ahead, and stopping it.
#[derive(Clone)]
pub struct SourceMonitor {
    shared: Arc<SharedRing>,
}

impl SourceMonitor {
    /// Calls `hook(true)` when a read or seek starts waiting for bytes that
    /// have not arrived (the ring is empty and the source is not at its
    /// end), and `hook(false)` when it returns. Install it before reading.
    pub fn on_starved(&self, hook: impl Fn(bool) + Send + Sync + 'static) {
        *self.shared.on_starved.lock() = Some(Arc::new(hook));
    }

    pub fn suspend(&self) {
        let mut state = self.shared.state.lock();
        state.suspended = true;
        self.shared.worker_cv.notify_all();
    }

    pub fn resume(&self) {
        let mut state = self.shared.state.lock();
        state.suspended = false;
        self.shared.worker_cv.notify_all();
    }

    /// Stops the source (the player is going away): a demuxer blocked in a
    /// read or seek gets an error instead of waiting for bytes that may never
    /// come, and every later read or seek fails.
    pub fn stop(&self) {
        drop(self.shared.stop());
    }
}

pub struct ReadAheadSource {
    shared: Arc<SharedRing>,
    worker_thread: Option<JoinHandle<()>>,
}

impl ReadAheadSource {
    pub fn new(reader: Box<dyn ReadSeekSend>) -> Self {
        let shared = Arc::new(SharedRing {
            state: Mutex::new(RingState {
                buffer: vec![0u8; RING_CAPACITY],
                capacity: RING_CAPACITY,
                tail_pos: 0,
                head_pos: 0,
                cur_pos: 0,
                ring_start: 0,
                eof: false,
                fatal_error: None,
                seek_req: None,
                seek_res: None,
                suspended: false,
                stop: false,
                io: false,
                starved: false,
            }),
            consumer_cv: Condvar::new(),
            worker_cv: Condvar::new(),
            on_starved: Mutex::new(None),
        });

        let shared_clone = Arc::clone(&shared);
        let worker_thread = std::thread::Builder::new()
            .name("peartube-source-readahead".into())
            .spawn(move || {
                worker_loop(reader, shared_clone);
            })
            .expect("failed to spawn source read-ahead thread");

        Self {
            shared,
            worker_thread: Some(worker_thread),
        }
    }

    /// A handle that stays with the engine after the source itself moves
    /// into a demuxer.
    pub fn monitor(&self) -> SourceMonitor {
        SourceMonitor {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Drop for ReadAheadSource {
    fn drop(&mut self) {
        let in_io = {
            let mut state = self.shared.stop();
            if state.io {
                // The worker sits in a read or seek of the underlying source,
                // which over a stalled network returns only when the peer
                // sends or gives up: leave it (it exits as soon as the call
                // returns, without touching the ring) and free the ring now.
                state.buffer = Vec::new();
            }
            state.io
        };
        if let Some(thread) = self.worker_thread.take() {
            if !in_io {
                // Parked on the ring: it sees the stop right away.
                let _ = thread.join();
            }
        }
    }
}

impl Read for ReadAheadSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let mut state = self.shared.state.lock();
        let result = loop {
            // Stopped: a dropped player must not leave a demuxer blocked here.
            if state.stop {
                break Err(stopped_error());
            }

            if state.cur_pos < state.head_pos {
                let available = (state.head_pos - state.cur_pos) as usize;
                let to_read = buf.len().min(available);
                let offset = ((state.ring_start as u64 + (state.cur_pos - state.tail_pos))
                    % state.capacity as u64) as usize;

                let first_chunk = to_read.min(state.capacity - offset);
                buf[..first_chunk].copy_from_slice(&state.buffer[offset..offset + first_chunk]);
                if to_read > first_chunk {
                    let second_chunk = to_read - first_chunk;
                    buf[first_chunk..to_read].copy_from_slice(&state.buffer[..second_chunk]);
                }
                state.cur_pos += to_read as u64;
                self.shared.worker_cv.notify_one();
                break Ok(to_read);
            }

            if let Some(err) = &state.fatal_error {
                break Err(std::io::Error::new(std::io::ErrorKind::Other, err.clone()));
            }

            if state.eof {
                break Ok(0);
            }

            // Nothing buffered and more to come: the reader is starved. The
            // hook runs unlocked, so look again before waiting.
            if !state.starved {
                self.shared.set_starved(&mut state, true);
                continue;
            }
            self.shared.consumer_cv.wait_for(&mut state, Duration::from_millis(100));
        };
        self.shared.set_starved(&mut state, false);
        result
    }
}

impl Seek for ReadAheadSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let mut state = self.shared.state.lock();
        if state.stop {
            return Err(stopped_error());
        }

        let target = match pos {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(d) => {
                if d >= 0 {
                    Some(state.cur_pos.saturating_add(d as u64))
                } else {
                    state.cur_pos.checked_sub((-d) as u64)
                }
            }
            SeekFrom::End(_) => None,
        };

        if let Some(target_pos) = target {
            if target_pos >= state.tail_pos && target_pos <= state.head_pos {
                // Free in-buffer seek!
                state.cur_pos = target_pos;
                self.shared.worker_cv.notify_one();
                return Ok(target_pos);
            }
        }

        // Out-of-window or SeekFrom::End seek: request underlying seek. The
        // worker clears the request once the result is in.
        state.seek_req = Some(pos);
        state.seek_res = None;
        self.shared.worker_cv.notify_one();

        // The worker may sit in a network read that has not returned: the
        // seek waits for bytes like a read does.
        while state.seek_req.is_some() && !state.stop {
            if !state.starved {
                self.shared.set_starved(&mut state, true);
                continue;
            }
            self.shared.consumer_cv.wait(&mut state);
        }
        self.shared.set_starved(&mut state, false);

        match state.seek_res.take() {
            Some(Ok(new_pos)) => Ok(new_pos),
            Some(Err(e)) => Err(e),
            // Stopped before the worker got to it.
            None => Err(stopped_error()),
        }
    }
}

fn worker_loop(mut reader: Box<dyn ReadSeekSend>, shared: Arc<SharedRing>) {
    let mut failure_start: Option<std::time::Instant> = None;
    let mut backoff = Duration::from_millis(50);
    let mut temp_buf = vec![0u8; CHUNK_SIZE];

    let mut state = shared.state.lock();
    loop {
        if state.stop {
            break;
        }

        if let Some(seek_from) = state.seek_req {
            // An HTTP source drains a short forward hop from its open
            // response, which blocks while the peer withholds bytes: seek
            // with the ring unlocked so a stop never waits behind it. The
            // request stays posted until its result is in.
            let res = source_io(&mut state, || reader.seek(seek_from));
            if state.stop {
                break;
            }
            match res {
                Ok(new_pos) => {
                    state.tail_pos = new_pos;
                    state.head_pos = new_pos;
                    state.cur_pos = new_pos;
                    state.ring_start = 0;
                    state.eof = false;
                    state.fatal_error = None;
                    failure_start = None;
                    backoff = Duration::from_millis(50);
                    state.seek_res = Some(Ok(new_pos));
                }
                Err(e) => {
                    state.seek_res = Some(Err(e));
                }
            }
            state.seek_req = None;
            shared.consumer_cv.notify_all();
            continue;
        }

        if state.suspended {
            shared.worker_cv.wait_for(&mut state, Duration::from_millis(100));
            continue;
        }

        let ahead = (state.head_pos - state.cur_pos) as usize;
        let total_buffered = (state.head_pos - state.tail_pos) as usize;
        if state.eof {
            // EOF stands until a seek or a reset; nothing to read.
            shared.worker_cv.wait_for(&mut state, Duration::from_millis(100));
            continue;
        }

        // Window full and the consumer has not advanced: park on the
        // worker condvar (the consumer's read/seek notifies it) instead
        // of spinning through this loop.
        if ahead >= state.capacity
            || (state.capacity == total_buffered && state.cur_pos > state.tail_pos)
        {
            shared.worker_cv.wait_for(&mut state, Duration::from_millis(100));
            continue;
        }

        if state.capacity - total_buffered == 0 && state.cur_pos > state.tail_pos {
            let evict = (state.cur_pos - state.tail_pos) as usize;
            state.tail_pos += evict as u64;
            state.ring_start = (state.ring_start + evict) % state.capacity;
        }

        let write_room = state.capacity - ((state.head_pos - state.tail_pos) as usize);
        let read_amount = temp_buf.len().min(write_room);
        let ring_write_offset = ((state.ring_start as u64 + (state.head_pos - state.tail_pos))
            % state.capacity as u64) as usize;
        if read_amount == 0 {
            continue;
        }

        let read_result = source_io(&mut state, || reader.read(&mut temp_buf[..read_amount]));
        if state.stop {
            break;
        }
        if state.seek_req.is_some() {
            continue;
        }

        match read_result {
            Ok(0) => {
                state.eof = true;
                failure_start = None;
                shared.consumer_cv.notify_all();
            }
            Ok(n) => {
                let first_chunk = n.min(state.capacity - ring_write_offset);
                state.buffer[ring_write_offset..ring_write_offset + first_chunk]
                    .copy_from_slice(&temp_buf[..first_chunk]);
                if n > first_chunk {
                    let second_chunk = n - first_chunk;
                    state.buffer[..second_chunk].copy_from_slice(&temp_buf[first_chunk..n]);
                }
                state.head_pos += n as u64;
                failure_start = None;
                backoff = Duration::from_millis(50);
                shared.consumer_cv.notify_all();
            }
            Err(e) => {
                let is_suspended = state.suspended;
                if failure_start.is_none() {
                    failure_start = Some(std::time::Instant::now());
                }
                let elapsed = failure_start.unwrap().elapsed();
                if !is_suspended && elapsed > Duration::from_secs(30) {
                    state.fatal_error = Some(e.to_string());
                    shared.consumer_cv.notify_all();
                } else {
                    let retry_in = backoff;
                    backoff = (backoff * 2).min(Duration::from_millis(1000));
                    let resume_pos = state.head_pos;
                    // Back off before reconnecting; a stop or a seek ends the
                    // wait early.
                    shared.worker_cv.wait_for(&mut state, retry_in);
                    if state.stop {
                        break;
                    }
                    if state.seek_req.is_none() {
                        let _ = source_io(&mut state, || reader.seek(SeekFrom::Start(resume_pos)));
                    }
                }
            }
        }
    }
}

/// Runs `call` on the underlying source with the ring unlocked, flagged as
/// in I/O so a dropping source knows not to wait for it.
fn source_io<T>(state: &mut MutexGuard<'_, RingState>, call: impl FnOnce() -> T) -> T {
    state.io = true;
    let out = MutexGuard::unlocked(state, call);
    state.io = false;
    out
}

/// Opens a URL (http(s) or file) with read-ahead ring buffering.
pub fn open_source(url: &str) -> std::io::Result<ReadAheadSource> {
    if url.starts_with("http://") || url.starts_with("https://") {
        let cfg = oxideav_http::HttpConfig::builder()
            .range_probe(true)
            .build();
        let http = oxideav_http::HttpSource::open_with_config(url, &cfg)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        Ok(ReadAheadSource::new(Box::new(http)))
    } else if let Some(path) = url.strip_prefix("file://") {
        let file = std::fs::File::open(path)?;
        Ok(ReadAheadSource::new(Box::new(file)))
    } else {
        let file = std::fs::File::open(url)?;
        Ok(ReadAheadSource::new(Box::new(file)))
    }
}
