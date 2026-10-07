//! A test container: `PTSCRIPT`, one mode byte, then a Matroska file. Its
//! demuxer hands out the Matroska file's packets in an order the mode picks
//! and answers `seek_to` as the mode says, so a test can stage packet
//! orders, seek failures and seeks held in the demuxer that real muxers and
//! demuxers do not produce.

use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use oxideav_core::{
    CodecResolver, Demuxer, Error, MediaType, Packet, PacketMetadata, ProbeData, ProbeScore, ReadSeek, Result,
    RuntimeContext, StreamInfo,
};

const MAGIC: &[u8; 8] = b"PTSCRIPT";
const NAME: &str = "ptscript";

/// How the demuxer orders packets and answers `seek_to`.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
pub enum Mode {
    /// Matroska order; `seek_to` returns `Error::Unsupported`.
    Unseekable = 1,
    /// Matroska order; `seek_to` fails with an invalid-data error.
    SeekFails = 2,
    /// Every audio and video packet starting before 0.5 s, then every
    /// subtitle packet, then the rest in Matroska order; `seek_to`
    /// unsupported.
    SubtitlesEarly = 3,
    /// Matroska order; `seek_to` goes on from the seek stream's last
    /// keyframe at or before the target.
    Seekable = 4,
}

/// Marks a file whose demuxer obeys the gate (`arm`).
const GATED: u8 = 0x80;

/// Writes `matroska` wrapped for `mode` to `path`.
pub fn write(path: &Path, matroska: &Path, mode: Mode) {
    write_as(path, matroska, mode as u8);
}

/// `write`, for a file whose demuxer the gate holds (`arm`).
pub fn write_gated(path: &Path, matroska: &Path, mode: Mode) {
    write_as(path, matroska, mode as u8 | GATED);
}

fn write_as(path: &Path, matroska: &Path, mode: u8) {
    let mut bytes = MAGIC.to_vec();
    bytes.push(mode);
    bytes.extend_from_slice(&std::fs::read(matroska).unwrap());
    std::fs::write(path, bytes).unwrap();
}

/// The production registry plus this container, which wins its probe.
pub fn context() -> RuntimeContext {
    let mut ctx = codecs::context();
    ctx.containers.register_demuxer(NAME, open);
    ctx.containers.register_probe_with_priority(NAME, probe, i32::MIN);
    ctx
}

fn probe(data: &ProbeData) -> ProbeScore {
    if data.buf.starts_with(MAGIC) { 100 } else { 0 }
}

/// Where the gate holds the demuxer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hold {
    /// Inside the demuxer's open, before the Player reads its selection.
    Open,
    /// Before the first packet: the Player has made its selection and its
    /// demux loop has looked for switches once.
    FirstPacket,
}

struct GateState {
    armed: Option<Hold>,
    entered: bool,
    released: bool,
}

/// Holds the next demuxer at `Hold` until the test lets it go. One
/// process-wide gate: tests that arm it run one at a time.
static GATE: Mutex<GateState> = Mutex::new(GateState { armed: None, entered: false, released: false });
static GATE_CHANGED: Condvar = Condvar::new();

/// The next demuxer opened waits at `at` for `release`.
pub fn arm(at: Hold) {
    *GATE.lock() = GateState { armed: Some(at), entered: false, released: false };
}

/// Waits until the armed demuxer is held.
pub fn entered(within: Duration) -> bool {
    let mut state = GATE.lock();
    GATE_CHANGED.wait_while_for(&mut state, |state| !state.entered, within);
    state.entered
}

/// Lets the held demuxer go on.
pub fn release() {
    GATE.lock().released = true;
    GATE_CHANGED.notify_all();
}

fn hold(at: Hold) {
    let mut state = GATE.lock();
    if state.armed != Some(at) {
        return;
    }
    state.armed = None;
    state.entered = true;
    GATE_CHANGED.notify_all();
    GATE_CHANGED.wait_while_for(&mut state, |state| !state.released, Duration::from_secs(60));
}

static INNER: LazyLock<RuntimeContext> = LazyLock::new(codecs::context);

struct Scripted {
    streams: Vec<StreamInfo>,
    packets: Vec<(Packet, PacketMetadata)>,
    next: usize,
    /// Packets handed out, for the first-packet hold.
    read: usize,
    last: PacketMetadata,
    mode: Mode,
    /// The gate may hold this demuxer.
    gated: bool,
}

fn open(mut input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes)?;
    if !bytes.starts_with(MAGIC) || bytes.len() < MAGIC.len() + 1 {
        return Err(Error::invalid("not a PTSCRIPT file"));
    }
    let gated = bytes[MAGIC.len()] & GATED != 0;
    let mode = match bytes[MAGIC.len()] & !GATED {
        1 => Mode::Unseekable,
        2 => Mode::SeekFails,
        3 => Mode::SubtitlesEarly,
        4 => Mode::Seekable,
        _ => return Err(Error::invalid("PTSCRIPT: unknown mode")),
    };
    if gated {
        hold(Hold::Open);
    }
    let inner = Cursor::new(bytes.split_off(MAGIC.len() + 1));
    let mut demuxer = INNER.containers.open_demuxer("matroska", Box::new(inner), codecs)?;
    let streams = demuxer.streams().to_vec();
    let mut all = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(packet) => {
                let metadata = demuxer.packet_metadata();
                all.push((packet, metadata));
            }
            Err(Error::Eof) => break,
            Err(error) => return Err(error),
        }
    }
    let kind = |packet: &Packet| {
        streams.iter().find(|s| s.index == packet.stream_index).map(|s| s.params.media_type)
    };
    let packets = match mode {
        Mode::Unseekable | Mode::SeekFails | Mode::Seekable => all,
        Mode::SubtitlesEarly => {
            let early = |packet: &Packet| {
                kind(packet) != Some(MediaType::Subtitle)
                    && packet.pts.is_some_and(|pts| packet.time_base.seconds_of(pts) < 0.5)
            };
            let (subtitles, media): (Vec<_>, Vec<_>) =
                all.into_iter().partition(|(packet, _)| kind(packet) == Some(MediaType::Subtitle));
            let (before, after): (Vec<_>, Vec<_>) = media.into_iter().partition(|(packet, _)| early(packet));
            before.into_iter().chain(subtitles).chain(after).collect()
        }
    };
    Ok(Box::new(Scripted { streams, packets, next: 0, read: 0, last: PacketMetadata::default(), mode, gated }))
}

impl Demuxer for Scripted {
    fn format_name(&self) -> &str {
        NAME
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if self.gated && self.read == 0 {
            hold(Hold::FirstPacket);
        }
        self.last = PacketMetadata::default();
        let (packet, metadata) = self.packets.get(self.next).cloned().ok_or(Error::Eof)?;
        self.next += 1;
        self.read += 1;
        self.last = metadata;
        Ok(packet)
    }

    fn packet_metadata(&self) -> PacketMetadata {
        self.last.clone()
    }

    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        match self.mode {
            Mode::SeekFails => Err(Error::invalid("PTSCRIPT: the seek failed")),
            Mode::Unseekable | Mode::SubtitlesEarly => Err(Error::unsupported("PTSCRIPT: no seeking")),
            Mode::Seekable => {
                let at = self.packets.iter().rposition(|(packet, _)| {
                    packet.stream_index == stream_index && packet.flags.keyframe && packet.pts.is_some_and(|t| t <= pts)
                }).unwrap_or(0);
                self.next = at;
                Ok(self.packets[at].0.pts.unwrap_or(0))
            }
        }
    }
}
