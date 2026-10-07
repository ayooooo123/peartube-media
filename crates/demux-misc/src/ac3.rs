// Ported from FFmpeg libavformat/ac3dec.c, rawdec.c
// (ff_raw_audio_read_header, ff_raw_read_partial_packet) and
// libavcodec/ac3_parser.c (and the tables in libavcodec/ac3tab.c),
// commit 2da55bf.
// License: LGPL-2.1-or-later
//
// Raw AC-3 / E-AC-3 demuxers. As in FFmpeg the input is read in 1024-byte
// pieces and FFmpeg's ac3 parser cuts the packets: one per frame, an
// E-AC-3 frame with its dependent substreams, bytes between frames kept
// with the frame before them. Timestamps are FFmpeg's for a raw stream:
// 1/90000, counted from zero in frame durations.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, ProbeData, ProbeScore, ReadSeek, Result, SampleFormat,
    StreamInfo, TimeBase, PROBE_SCORE_EXTENSION,
};

use crate::parser::{returned, Ac3, AudioClock, Parser};
use demux_seek_core::{read_on, Allowance, Index};

const SYNCWORD_AC3: u16 = 0x0B77;

/// ff_ac3_sample_rate_tab (ac3tab.c)
const SAMPLE_RATES: [u32; 4] = [48000, 44100, 32000, 0];
/// ff_ac3_frame_size_tab[38][3] (ac3tab.c), half-words for each sr_code
const FRAME_SIZE_TAB: [[u16; 3]; 38] = [
    [64, 69, 96], [64, 70, 96], [80, 87, 120], [80, 88, 120],
    [96, 104, 144], [96, 105, 144], [112, 121, 168], [112, 122, 168],
    [128, 139, 192], [128, 140, 192], [160, 174, 240], [160, 175, 240],
    [192, 208, 288], [192, 209, 288], [224, 243, 336], [224, 244, 336],
    [256, 278, 384], [256, 279, 384], [320, 348, 480], [320, 349, 480],
    [384, 417, 576], [384, 418, 576], [448, 487, 672], [448, 488, 672],
    [512, 557, 768], [512, 558, 768], [640, 696, 960], [640, 697, 960],
    [768, 835, 1152], [768, 836, 1152], [896, 975, 1344], [896, 976, 1344],
    [1024, 1114, 1536], [1024, 1115, 1536], [1152, 1253, 1728],
    [1152, 1254, 1728], [1280, 1393, 1920], [1280, 1394, 1920],
];
/// ff_ac3_channels_tab: full-bandwidth channels per acmod (ac3tab.c)
const ACMOD_CHANNELS: [u16; 8] = [2, 1, 2, 3, 3, 4, 4, 5];
/// eac3_blocks (ac3_parser.c): number of blocks per numblkscod
const EAC3_BLOCKS: [u16; 4] = [1, 2, 3, 6];

/// CRC-16 ANSI (poly 0x8005, non-reflected, init 0) — FFmpeg's
/// AV_CRC_16_ANSI, used by the ac3 probe and the frame crc1.
/// Bit-at-a-time form; frames are ≤4096 bytes and probing runs on ≤256 KiB.
pub(crate) fn crc16_ansi(data: &[u8]) -> u16 {
    static TABLE: std::sync::LazyLock<[u16; 256]> = std::sync::LazyLock::new(|| {
        let mut table = [0u16; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut c = (i as u16) << 8;
            for _ in 0..8 {
                c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
            }
            *slot = c;
        }
        table
    });
    let table = &*TABLE;
    let mut crc: u16 = 0;
    for &b in data {
        crc = (crc << 8) ^ table[((crc >> 8) as u8 ^ b) as usize];
    }
    crc
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ac3Header {
    pub bitstream_id: u8,
    /// E-AC-3 frame type: 0 independent, 1 dependent, 2 AC-3 convert
    /// (every AC-3 frame).
    pub frame_type: u8,
    /// Bytes of the whole syncframe.
    pub frame_size: usize,
    pub sample_rate: u32,
    pub channels: u16,
    /// blocks per frame × 256 = samples per frame
    pub num_blocks: u16,
    pub is_eac3: bool,
}

/// Mirror of ff_ac3_parse_header (ac3_parser.c) on the first 7 bytes
/// (AC-3) / 6 bytes (E-AC-3) of a syncframe. `buf` must be the big-endian
/// byte order of the bitstream (0x0B77 sync).
pub fn parse_ac3_header(buf: &[u8]) -> Option<Ac3Header> {
    if buf.len() < 7 {
        return None;
    }
    // sync word
    if u16::from_be_bytes([buf[0], buf[1]]) != SYNCWORD_AC3 {
        return None;
    }
    // bitstream id: bit 27..31 of the stream after the syncword.
    // Byte 5 of the bitstream holds bits 27..31 == the top 5 bits of byte 5.
    let bsid = buf[5] >> 3;
    if bsid > 16 {
        return None;
    }

    if bsid <= 10 {
        // Normal AC-3
        // crc1 (16 bits, bytes 2..3), sr_code 2 bits, frame_size_code 6 bits
        let fscod = (buf[4] >> 6) & 3;
        if fscod == 3 {
            return None;
        }
        let frmsizecod = (buf[4] & 0x3F) as usize;
        if frmsizecod > 37 {
            return None;
        }
        let sr_shift = u32::from(bsid.max(8) - 8);
        let sample_rate = SAMPLE_RATES[fscod as usize] >> sr_shift;
        let frame_size = FRAME_SIZE_TAB[frmsizecod][fscod as usize] as usize * 2;
        let acmod = (buf[6] >> 5) & 7;
        // lfeon follows acmod and the 2-bit fields its layout carries:
        // cmixlev (three front channels), surmixlev (surrounds), dsurmod
        // (2/0), as ac3_parse_header reads them.
        let mut skip = 0;
        if acmod & 1 != 0 && acmod != 1 {
            skip += 2;
        }
        if acmod & 4 != 0 {
            skip += 2;
        }
        if acmod == 2 {
            skip += 2;
        }
        let lfeon = (buf[6] >> (4 - skip)) & 1;
        let channels = ACMOD_CHANNELS[acmod as usize] + u16::from(lfeon);
        Some(Ac3Header {
            bitstream_id: bsid,
            frame_type: 2,
            frame_size,
            sample_rate,
            channels,
            num_blocks: 6,
            is_eac3: false,
        })
    } else {
        // Enhanced AC-3
        let frame_type = buf[2] >> 6;
        if frame_type == 3 {
            return None;
        }
        let frmsiz = ((u16::from(buf[2] & 0x07) << 8) | u16::from(buf[3])) as usize;
        let frame_size = (frmsiz + 1) * 2;
        if frame_size < 7 {
            return None;
        }
        let fscod = (buf[4] >> 6) & 3;
        let (sample_rate, num_blocks) = if fscod == 3 {
            let fscod2 = (buf[4] >> 4) & 3;
            if fscod2 == 3 {
                return None;
            }
            (SAMPLE_RATES[fscod2 as usize] / 2, 6)
        } else {
            let numblkscod = (buf[4] >> 4) & 3;
            (SAMPLE_RATES[fscod as usize], EAC3_BLOCKS[numblkscod as usize])
        };
        let acmod = (buf[4] >> 1) & 7;
        let lfeon = buf[4] & 1;
        let channels = ACMOD_CHANNELS[acmod as usize] + u16::from(lfeon);
        Some(Ac3Header {
            bitstream_id: bsid,
            frame_type,
            frame_size,
            sample_rate,
            channels,
            num_blocks,
            is_eac3: true,
        })
    }
}

/// Parse `buf` assuming the byte-swapped (0x770B) byte order: its first 8
/// bytes swapped in pairs, as ac3_eac3_probe does (ac3dec.c:58-64), bytes
/// past its end read as the zero padding of FFmpeg's probe buffer.
fn parse_ac3_header_swapped(buf: &[u8]) -> Option<Ac3Header> {
    let byte = |i: usize| buf.get(i).copied().unwrap_or(0);
    let mut tmp = [0u8; 8];
    for i in (0..8).step_by(2) {
        tmp[i] = byte(i + 1);
        tmp[i + 1] = byte(i);
    }
    parse_ac3_header(&tmp)
}

/// Mirror of ac3_eac3_probe (ac3dec.c): chase syncframes from every
/// 0x0B77/0x770B in the buffer, validating the CRC-16 of each frame; the
/// stream is only AC-3/E-AC-3 if whole syncframes chain up.
pub fn probe_ac3_or_eac3(probe: &ProbeData, expect_eac3: bool) -> ProbeScore {
    let buf = probe.buf;
    let end = buf.len();
    let mut max_frames = 0;
    let mut first_frames = 0;
    let mut codec_eac3 = false;

    let mut i = 0usize;
    while i < end {
        if i > 0 && !(buf[i] == 0x0B && i + 1 < end && buf[i + 1] == 0x77)
            && !(buf[i] == 0x77 && i + 1 < end && buf[i + 1] == 0x0B)
        {
            i += 1;
            continue;
        }
        if i + 2 > end {
            break;
        }
        let swapped = buf[i] == 0x77;
        let mut pos = i;
        let mut frames = 0;
        while pos < end {
            // FFmpeg's skip of a 16-byte "\x01\x10"-prefixed padding block
            if pos + 2 <= end && buf[pos] == 0x01 && buf[pos + 1] == 0x10 {
                if pos + 16 > end {
                    break;
                }
                pos += 16;
                continue;
            }
            if pos + 7 > end {
                break;
            }
            let hdr = if swapped {
                parse_ac3_header_swapped(&buf[pos..])
            } else {
                parse_ac3_header(&buf[pos..])
            };
            let Some(hdr) = hdr else { break };
            if pos + hdr.frame_size > end {
                break;
            }
            // CRC-16 over the frame (bytes 2..) must be 0; FFmpeg checks it
            // in both byte orders to avoid false positives on MPEG data.
            let ok = if swapped {
                let mut tmp = vec![0u8; hdr.frame_size];
                for k in (0..hdr.frame_size).step_by(2) {
                    tmp[k] = buf[pos + k + 1];
                    if k + 1 < hdr.frame_size {
                        tmp[k + 1] = buf[pos + k];
                    }
                }
                crc16_ansi(&tmp[2..]) == 0
            } else {
                crc16_ansi(&buf[pos + 2..pos + hdr.frame_size]) == 0
            };
            if !ok {
                break;
            }
            if hdr.is_eac3 {
                codec_eac3 = true;
            }
            frames += 1;
            pos += hdr.frame_size;
        }
        max_frames = max_frames.max(frames);
        if i == 0 {
            first_frames = frames;
        }
        i += 1;
    }

    if codec_eac3 != expect_eac3 {
        return 0;
    }
    if first_frames >= 7 {
        PROBE_SCORE_EXTENSION + 2
    } else if max_frames > 200 {
        PROBE_SCORE_EXTENSION
    } else if max_frames >= 4 {
        PROBE_SCORE_EXTENSION / 2
    } else if max_frames >= 1 {
        1
    } else {
        0
    }
}

pub fn probe_ac3(probe: &ProbeData) -> ProbeScore {
    probe_ac3_or_eac3(probe, false)
}

pub fn probe_eac3(probe: &ProbeData) -> ProbeScore {
    probe_ac3_or_eac3(probe, true)
}

/// ff_raw_demuxer_class raw_packet_size
const RAW_PACKET_SIZE: usize = 1024;

/// avformat_new_stream's default: 33-bit timestamps in 1/90000.
const TIME_BASE: TimeBase = TimeBase::new(1, 90_000);

pub struct Ac3Demuxer {
    format_name: &'static str,
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    parser: Parser<Ac3>,
    clock: AudioClock,
    queue: VecDeque<Packet>,
    /// Where each queued packet's frame starts in the input.
    positions: VecDeque<i64>,
    pos: i64,
    eof: bool,
    /// AVFMT_GENERIC_INDEX: every frame returned (all are key frames).
    index: Index,
    /// What the seek under way may still read.
    allowance: Allowance,
}

/// Where reading was, given back when a seek fails.
struct Reading {
    at: u64,
    parser: Parser<Ac3>,
    clock: AudioClock,
    queue: VecDeque<Packet>,
    positions: VecDeque<i64>,
    pos: i64,
    eof: bool,
}

/// ff_raw_audio_read_header: one stream, parameters from the first
/// syncframe whose CRC holds, as find_stream_info reports them.
fn open_ac3_inner(
    mut input: Box<dyn ReadSeek>,
    format_name: &'static str,
    codec: &'static str,
) -> Result<Box<dyn Demuxer>> {
    let mut head = vec![0u8; 64 * 1024];
    let mut n = 0;
    while n < head.len() {
        let got = input.read(&mut head[n..])?;
        if got == 0 {
            break;
        }
        n += got;
    }
    let hdr = (0..n.saturating_sub(6))
        .filter(|&i| head[i] == 0x0B && head[i + 1] == 0x77)
        .find_map(|i| {
            let hdr = parse_ac3_header(&head[i..n])?;
            (i + hdr.frame_size <= n && crc16_ansi(&head[i + 2..i + hdr.frame_size]) == 0).then_some(hdr)
        })
        .ok_or_else(|| Error::invalid("ac3: no valid syncframe found"))?;
    input.seek(SeekFrom::Start(0))?;

    let mut params = CodecParameters::audio(CodecId::new(codec));
    params.sample_rate = Some(hdr.sample_rate);
    params.channels = Some(hdr.channels);
    params.sample_format = Some(SampleFormat::F32);

    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TIME_BASE,
        duration: None,
        start_time: Some(0),
    };
    Ok(Box::new(Ac3Demuxer {
        format_name,
        input,
        streams: vec![stream],
        parser: Parser::new(Ac3::new(codec)),
        clock: AudioClock::new(1, 90_000, 33),
        queue: VecDeque::new(),
        positions: VecDeque::new(),
        pos: 0,
        eof: false,
        index: Index::default(),
        allowance: Allowance::default(),
    }))
}

pub fn open_ac3(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    open_ac3_inner(input, "ac3", "ac3")
}

pub fn open_eac3(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    open_ac3_inner(input, "eac3", "eac3")
}

impl Ac3Demuxer {
    /// ff_raw_read_partial_packet into the parser: the frames it
    /// completes, timed, join the queue; at the end of the input the
    /// parser hands over the last one.
    fn read_piece(&mut self) -> Result<()> {
        let mut piece = [0u8; RAW_PACKET_SIZE];
        let mut n = 0;
        while n < piece.len() {
            let got = self.input.read(&mut piece[n..])?;
            if got == 0 {
                break;
            }
            n += got;
        }
        self.allowance.spend(1, n as u64)?;
        let mut units = Vec::new();
        if n == 0 {
            self.eof = true;
            self.parser.flush(&mut units);
        } else {
            if self.parser.split.buffered_bytes() + n > 8 * 1024 * 1024 {
                return Err(Error::invalid("ac3: access unit exceeds 8 MiB"));
            }
            self.parser.push(&piece[..n], None, None, self.pos, &mut units);
            self.pos += n as i64;
        }
        for unit in units {
            let pos = unit.pos;
            let packet = self.clock.stamp(unit, 0, TIME_BASE, &mut self.queue);
            self.queue.push_back(packet);
            self.positions.push_back(pos);
        }
        Ok(())
    }

    /// Reads on from `pos` with a fresh parser (ff_read_frame_flush), the
    /// clock at `ts` (avpriv_update_cur_dts) or, without one, as at open.
    fn restart(&mut self, pos: i64, ts: Option<i64>) -> Result<()> {
        self.input.seek(SeekFrom::Start(pos as u64))?;
        // What the parser set on the codec context outlives it.
        self.parser = Parser::new(self.parser.split.reset());
        self.clock = AudioClock::new(1, 90_000, 33);
        if let Some(ts) = ts {
            self.clock.seeked(ts);
        }
        self.queue.clear();
        self.positions.clear();
        self.pos = pos;
        self.eof = false;
        Ok(())
    }

    /// Reading as it stands, moved out for a seek to give back if it fails.
    fn take_reading(&mut self) -> Result<Reading> {
        let fresh = Parser::new(self.parser.split.reset());
        Ok(Reading {
            at: self.input.stream_position()?,
            parser: std::mem::replace(&mut self.parser, fresh),
            clock: std::mem::replace(&mut self.clock, AudioClock::new(1, 90_000, 33)),
            queue: std::mem::take(&mut self.queue),
            positions: std::mem::take(&mut self.positions),
            pos: self.pos,
            eof: self.eof,
        })
    }

    fn give_back(&mut self, reading: Reading) -> Result<()> {
        self.input.seek(SeekFrom::Start(reading.at))?;
        (self.parser, self.clock, self.queue, self.positions) = (reading.parser, reading.clock, reading.queue, reading.positions);
        (self.pos, self.eof) = (reading.pos, reading.eof);
        Ok(())
    }

    /// seek_frame_generic from the index search's result `found`.
    fn land(&mut self, timestamp: i64, mut found: Option<usize>) -> Result<i64> {
        if found.is_none() || found == Some(self.index.entries().len() - 1) {
            match self.index.entries().last().copied() {
                Some(last) => self.restart(last.pos, Some(last.timestamp))?,
                None => self.restart(0, None)?,
            }
            // Every frame is a key frame.
            read_on(timestamp, || self.next_packet().map(|p| (true, p.dts)))?;
            found = self.index.search(timestamp, true);
        }
        let Some(i) = found else {
            return Err(Error::invalid("ac3: no frame to seek to"));
        };
        let e = self.index.entries()[i];
        self.restart(e.pos, Some(e.timestamp))?;
        Ok(e.timestamp)
    }
}

impl Demuxer for Ac3Demuxer {
    fn format_name(&self) -> &str {
        self.format_name
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            if let Some(mut packet) = self.queue.pop_front() {
                let pos = self.positions.pop_front().unwrap_or(-1);
                packet.pts = returned(packet.pts);
                packet.dts = returned(packet.dts);
                // av_read_frame indexes every key packet it returns.
                if let Some(dts) = packet.dts {
                    self.index.add(pos, dts, 0, 0, true);
                }
                return Ok(packet);
            }
            if self.eof {
                return Err(Error::Eof);
            }
            self.read_piece()?;
        }
    }

    /// seek.c seek_frame_generic with AVSEEK_FLAG_BACKWARD over the index
    /// of the frames returned so far (ac3dec.c: AVFMT_GENERIC_INDEX). Past
    /// its last entry the frames are read on, within the seek's
    /// allowance, until one starts after the target; reading resumes at
    /// the last frame at or before it, timed from there as FFmpeg times
    /// it. A seek that fails leaves reading where it was.
    fn seek_to(&mut self, _stream_index: u32, timestamp: i64) -> Result<i64> {
        let found = self.index.search(timestamp, true);
        if found.is_none() && self.index.entries().first().is_some_and(|e| timestamp < e.timestamp) {
            return Err(Error::invalid("ac3: seek before the first frame"));
        }
        let reading = self.take_reading()?;
        self.allowance.start();
        let landed = self.land(timestamp, found);
        self.allowance.stop();
        if landed.is_err() {
            self.give_back(reading)?;
        }
        landed
    }
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("ac3", open_ac3);
    reg.register_probe("ac3", probe_ac3);
    reg.register_extension("ac3", "ac3");

    reg.register_demuxer("eac3", open_eac3);
    reg.register_probe("eac3", probe_eac3);
    reg.register_extension("eac3", "eac3");
    reg.register_extension("ec3", "eac3");
}
