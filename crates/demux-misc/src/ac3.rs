// Ported from FFmpeg libavformat/ac3dec.c and libavcodec/ac3_parser.c
// (and the tables in libavcodec/ac3tab.c), commit 2da55bf.
// License: LGPL-2.1-or-later
//
// Raw AC-3 / E-AC-3 demuxers. A syncframe is one packet; the parser is the
// FFmpeg ac3 parser (bitstream layout of ATSC A/52 and E-AC-3) so packet
// boundaries and stream parameters match FFmpeg's exactly.

use std::io::{Read, Seek, SeekFrom};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, ProbeData, ProbeScore, ReadSeek, Result, SampleFormat,
    StreamInfo, TimeBase, PROBE_SCORE_EXTENSION,
};

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
fn crc16_ansi(data: &[u8]) -> u16 {
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
        let lfeon = (buf[6] >> 4) & 1;
        let channels = ACMOD_CHANNELS[acmod as usize] + u16::from(lfeon);
        Some(Ac3Header {
            bitstream_id: bsid,
            frame_size,
            sample_rate,
            channels,
            num_blocks: 6,
            is_eac3: false,
        })
    } else {
        // Enhanced AC-3
        let frmsiz = ((u16::from(buf[2] & 0x07) << 8) | u16::from(buf[3])) as usize;
        let frame_size = (frmsiz + 1) * 2;
        if frame_size < 6 {
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
            frame_size,
            sample_rate,
            channels,
            num_blocks,
            is_eac3: true,
        })
    }
}

/// Parse `buf` assuming the byte-swapped (0x770B) byte order.
fn parse_ac3_header_swapped(buf: &[u8]) -> Option<Ac3Header> {
    let len = buf.len().min(64);
    let mut tmp = [0u8; 64];
    for i in (0..len).step_by(2) {
        tmp[i] = buf[i + 1];
        if i + 1 < len {
            tmp[i + 1] = buf[i];
        }
    }
    parse_ac3_header(&tmp[..len])
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

pub struct Ac3Demuxer {
    format_name: &'static str,
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    /// samples per frame in stream time base (1 / sample_rate)
    frame_samples: i64,
    pts: i64,
}

/// ff_raw_audio_read_header + ff_raw_read_partial_packet semantics: one
/// syncframe per packet, timestamps in samples.
fn open_ac3_inner(
    mut input: Box<dyn ReadSeek>,
    format_name: &'static str,
    codec_id: CodecId,
) -> Result<Box<dyn Demuxer>> {
    let mut head = vec![0u8; 64 * 1024];
    let n = input.read(&mut head)?;
    if n < 7 {
        return Err(Error::invalid("ac3: file too short"));
    }

    let mut first_hdr = None;
    let mut sync_offset = 0;
    for i in 0..n.saturating_sub(6) {
        if head[i] == 0x0B && head[i + 1] == 0x77
            && let Some(hdr) = parse_ac3_header(&head[i..n]) {
                // require the whole first frame to be present and CRC-valid
                if i + hdr.frame_size <= n
                    && crc16_ansi(&head[i + 2..i + hdr.frame_size]) == 0
                {
                    first_hdr = Some(hdr);
                    sync_offset = i;
                    break;
                }
            }
    }
    let hdr = first_hdr.ok_or_else(|| Error::invalid("ac3: no valid syncframe found"))?;
    input.seek(SeekFrom::Start(sync_offset as u64))?;

    let mut params = CodecParameters::audio(codec_id);
    params.sample_rate = Some(hdr.sample_rate);
    params.channels = Some(hdr.channels);
    params.sample_format = Some(SampleFormat::F32);

    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::from_rate(hdr.sample_rate),
        duration: None,
        start_time: Some(0),
    };
    Ok(Box::new(Ac3Demuxer {
        format_name,
        input,
        streams: vec![stream],
        frame_samples: i64::from(hdr.num_blocks) * 256,
        pts: 0,
    }))
}

pub fn open_ac3(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    open_ac3_inner(input, "ac3", CodecId::new("ac3"))
}

pub fn open_eac3(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    open_ac3_inner(input, "eac3", CodecId::new("eac3"))
}

impl Ac3Demuxer {
    /// Read and validate the next syncframe; returns its header and bytes.
    /// A truncated final frame (file cut mid-frame, as in cut-down FATE
    /// samples) is returned as-is, like FFmpeg's av_get_packet partial
    /// read; anything else is an error.
    fn next_syncframe(&mut self) -> Result<(Ac3Header, Vec<u8>)> {
        let mut data = Vec::with_capacity(4096);
        // Read the 7 header bytes incrementally, searching for sync.
        let mut head = [0u8; 7];
        let mut got = 0usize;
        let mut window: u16 = 0;
        loop {
            let mut byte = [0u8; 1];
            match self.input.read(&mut byte) {
                Ok(0) => return Err(Error::Eof),
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }
            window = (window << 8) | u16::from(byte[0]);
            if got < 7 {
                head[got] = byte[0];
                got += 1;
            } else {
                head.copy_within(1.., 0);
                head[6] = byte[0];
            }
            if window != SYNCWORD_AC3 {
                continue;
            }
            // We have a syncword; make sure 7 header bytes are buffered.
            if got < 7 {
                self.input.read_exact(&mut head[got..])?;
            }
            let Some(hdr) = parse_ac3_header(&head) else {
                // Not a real frame; keep scanning from after the syncword.
                continue;
            };
            if hdr.frame_size > 4096 || hdr.frame_size < 7 {
                return Err(Error::invalid("ac3: invalid frame size"));
            }
            data.extend_from_slice(&head);
            data.resize(hdr.frame_size, 0);
            match self.input.read(&mut data[7..]) {
                Ok(n) if n == hdr.frame_size - 7 => {}
                // Short read at EOF: the file ends mid-frame. FFmpeg's
                // av_get_packet returns the partial packet; emit it once.
                Ok(0) => return Ok((hdr, data[..7].to_vec())),
                Ok(n) => {
                    // Fill the rest until EOF, tolerating a truncated tail.
                    let have = 7 + n;
                    let mut got2 = have;
                    while got2 < hdr.frame_size {
                        let r = self.input.read(&mut data[got2..])?;
                        if r == 0 {
                            break;
                        }
                        got2 += r;
                    }
                    data.truncate(got2);
                }
                Err(e) => return Err(e.into()),
            }
            return Ok((hdr, data));
        }
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
        let (_hdr, data) = self.next_syncframe()?;
        let mut pkt = Packet::new(0, self.streams[0].time_base, data);
        pkt.pts = Some(self.pts);
        pkt.dts = Some(self.pts);
        pkt.duration = Some(self.frame_samples);
        pkt.flags.keyframe = true;
        self.pts += self.frame_samples;
        Ok(pkt)
    }

    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        // Linear scan from the start; raw streams have no index.
        self.input.seek(SeekFrom::Start(0))?;
        self.pts = 0;
        while self.pts < pts {
            match self.next_packet() {
                Ok(_) => {}
                Err(Error::Eof) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(self.pts)
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
