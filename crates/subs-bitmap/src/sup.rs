//! Raw HDMV PGS (`.sup`) demuxer.
//!
//! Port of FFmpeg `libavformat/supdec.c` (commit 2da55bf; header: GNU Lesser
//! General Public License 2.1 or later). Every packet is one PGS segment
//! (type, length, body) with the pts and dts of its 10-byte `PG` header in a
//! 1/90000 time base; a zero dts means unset. A segment cut off by the end of
//! the file is returned with what is there.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, MediaType, Packet, ProbeData, ProbeScore,
    ReadSeek, Result, StreamInfo, TimeBase,
};

use crate::{PGS_CODEC_ID, RESOLUTION_PRIORITY};

const SUP_PGS_MAGIC: u16 = 0x5047; // "PG"

pub(crate) fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("sup", open);
    reg.register_extension_with_priority("sup", "sup", RESOLUTION_PRIORITY);
    reg.register_probe_with_priority("sup", probe, RESOLUTION_PRIORITY);
}

/// `sup_probe`: walks up to ten segments. FFmpeg scores one segment
/// `AVPROBE_SCORE_RETRY / 2` and two or more at least `AVPROBE_SCORE_RETRY`;
/// on OxideAV's scale one segment is a signature match without
/// corroboration (50) and a chain of two or more is unambiguous (100).
fn probe(p: &ProbeData) -> ProbeScore {
    let mut buf = p.buf;
    let mut packets = 0;
    while packets < 10 {
        if buf.len() < 10 + 3 {
            break;
        }
        if u16::from_be_bytes([buf[0], buf[1]]) != SUP_PGS_MAGIC {
            return 0;
        }
        let full = usize::from(u16::from_be_bytes([buf[11], buf[12]])) + 10 + 3;
        if buf.len() < full {
            break;
        }
        buf = &buf[full..];
        packets += 1;
    }
    match packets {
        0 => 0,
        1 => 50,
        _ => 100,
    }
}

fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    input.seek(SeekFrom::Start(0))?;
    let mut params = CodecParameters::subtitle(CodecId::new(PGS_CODEC_ID));
    params.media_type = MediaType::Subtitle;
    let stream = StreamInfo { index: 0, time_base: TimeBase::new(1, 90_000), duration: None, start_time: None, params };
    Ok(Box::new(SupDemuxer { input, streams: [stream] }))
}

struct SupDemuxer {
    input: Box<dyn ReadSeek>,
    streams: [StreamInfo; 1],
}

impl SupDemuxer {
    /// Reads up to `n` bytes; fewer only at the end of the input.
    fn read_up_to(&mut self, n: usize, out: &mut Vec<u8>) -> Result<usize> {
        let start = out.len();
        out.resize(start + n, 0);
        let mut got = 0;
        while got < n {
            match self.input.read(&mut out[start + got..]) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        out.truncate(start + got);
        Ok(got)
    }

    /// `avio_rb16` / `avio_rb32`: missing bytes read as 0.
    fn read_be(&mut self, n: usize) -> Result<(u32, usize)> {
        let mut b = Vec::with_capacity(n);
        let got = self.read_up_to(n, &mut b)?;
        Ok((b.iter().fold(0u32, |v, &x| v << 8 | u32::from(x)) << (8 * (n - got)), got))
    }
}

impl Demuxer for SupDemuxer {
    fn format_name(&self) -> &str {
        "sup"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let (magic, got) = self.read_be(2)?;
        if magic as u16 != SUP_PGS_MAGIC {
            return Err(if got < 2 { Error::Eof } else { Error::invalid("sup: missing PG magic") });
        }
        let (pts, _) = self.read_be(4)?;
        let (dts, _) = self.read_be(4)?;
        let mut data = Vec::with_capacity(3);
        if self.read_up_to(3, &mut data)? == 0 {
            return Err(Error::Eof);
        }
        if data.len() >= 3 {
            // The segment length follows its type byte.
            let len = usize::from(u16::from_be_bytes([data[1], data[2]]));
            self.read_up_to(len, &mut data)?;
        }
        let mut packet = Packet::new(0, self.streams[0].time_base, data).with_pts(i64::from(pts)).with_keyframe(true);
        // Many files have DTS set to 0 for all packets: 0 means unset.
        if dts != 0 {
            packet = packet.with_dts(i64::from(dts));
        }
        Ok(packet)
    }
}
