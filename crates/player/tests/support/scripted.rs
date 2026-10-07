//! A test container: `PTSCRIPT`, one mode byte, then a Matroska file. Its
//! demuxer hands out the Matroska file's packets in an order the mode picks
//! and answers `seek_to` as the mode says, so a test can stage packet
//! orders and seek failures that real muxers and demuxers do not produce.

use std::collections::VecDeque;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::LazyLock;

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
}

/// Writes `matroska` wrapped for `mode` to `path`.
pub fn write(path: &Path, matroska: &Path, mode: Mode) {
    let mut bytes = MAGIC.to_vec();
    bytes.push(mode as u8);
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

static INNER: LazyLock<RuntimeContext> = LazyLock::new(codecs::context);

struct Scripted {
    streams: Vec<StreamInfo>,
    packets: VecDeque<(Packet, PacketMetadata)>,
    last: PacketMetadata,
    mode: Mode,
}

fn open(mut input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes)?;
    if !bytes.starts_with(MAGIC) || bytes.len() < MAGIC.len() + 1 {
        return Err(Error::invalid("not a PTSCRIPT file"));
    }
    let mode = match bytes[MAGIC.len()] {
        1 => Mode::Unseekable,
        2 => Mode::SeekFails,
        3 => Mode::SubtitlesEarly,
        _ => return Err(Error::invalid("PTSCRIPT: unknown mode")),
    };
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
        Mode::Unseekable | Mode::SeekFails => all.into_iter().collect(),
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
    Ok(Box::new(Scripted { streams, packets, last: PacketMetadata::default(), mode }))
}

impl Demuxer for Scripted {
    fn format_name(&self) -> &str {
        NAME
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        self.last = PacketMetadata::default();
        let (packet, metadata) = self.packets.pop_front().ok_or(Error::Eof)?;
        self.last = metadata;
        Ok(packet)
    }

    fn packet_metadata(&self) -> PacketMetadata {
        self.last.clone()
    }

    fn seek_to(&mut self, _stream_index: u32, _pts: i64) -> Result<i64> {
        match self.mode {
            Mode::SeekFails => Err(Error::invalid("PTSCRIPT: the seek failed")),
            Mode::Unseekable | Mode::SubtitlesEarly => Err(Error::unsupported("PTSCRIPT: no seeking")),
        }
    }
}
