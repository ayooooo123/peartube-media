// Ported from FFmpeg libavformat/spdifenc.c and libavformat/spdif.h (commit 2da55bf).
// Copyright (c) 2009 Bartlomiej Wolowiec
// Copyright (c) 2010, 2020 Anssi Hannula
// Copyright (c) 2010 Carl Eugen Hoyos
// Licensed under LGPL-2.1-or-later.

//! Pure-Rust IEC 61937 / IEC 60958 S/PDIF and HDMI HBR bitstream packetizer.
//!
//! Wraps raw AC-3, E-AC-3 (Dolby Digital Plus / Atmos JOC), DTS Core, DTS-HD
//! Master Audio / DTS:X (Type IV 8ch 192 kHz HBR), and Dolby TrueHD / MLP
//! (Dolby MAT 2.0 8ch 192 kHz HBR) access units into timing-exact IEC 61937
//! data bursts matching FFmpeg's `spdif` muxer byte-for-byte.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

/// First 16-bit IEC 61937 syncword (`Pa`).
pub const SYNCWORD1: u16 = 0xF872;
/// Second 16-bit IEC 61937 syncword (`Pb`).
pub const SYNCWORD2: u16 = 0x4E1F;
/// 8-byte burst preamble (`Pa`, `Pb`, `Pc`, `Pd`).
pub const BURST_HEADER_SIZE: usize = 8;

pub const IEC61937_AC3: u16 = 0x01;
pub const IEC61937_DTS1: u16 = 0x0B;
pub const IEC61937_DTS2: u16 = 0x0C;
pub const IEC61937_DTS3: u16 = 0x0D;
pub const IEC61937_DTSHD: u16 = 0x11;
pub const IEC61937_EAC3: u16 = 0x15;
pub const IEC61937_TRUEHD: u16 = 0x16;

/// Dolby MAT IEC 61937 burst repetition period in bytes (15,360 IEC frames @ 8ch 192 kHz).
pub const MAT_PKT_OFFSET: usize = 61_440;
/// Dolby MAT payload size inside one 61,440-byte burst.
pub const MAT_FRAME_SIZE: usize = 61_424;
const TRUEHD_NOMINAL_AU_SPACING: i32 = 2_560;
const HD_BUF_COUNT: usize = 6;
/// Cap untrusted E-AC-3 / DTS-HD accumulation before refusing malformed packets.
const MAX_BURST_BYTES: usize = 256 * 1024;

const MAT_START_CODE: [u8; 20] = [
    0x07, 0x9E, 0x00, 0x03, 0x84, 0x01, 0x01, 0x01, 0x80, 0x00, 0x56, 0xA5, 0x3B, 0xF4, 0x81, 0x83,
    0x49, 0x80, 0x77, 0xE0,
];
const MAT_MIDDLE_CODE: [u8; 12] = [
    0xC3, 0xC1, 0x42, 0x49, 0x3B, 0xFA, 0x82, 0x83, 0x49, 0x80, 0x77, 0xE0,
];
const MAT_END_CODE: [u8; 16] = [
    0xC3, 0xC2, 0xC0, 0xC4, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x97, 0x11,
];

const MAT_CODES: [(usize, &[u8]); 3] = [
    (0, &MAT_START_CODE),
    (30_708, &MAT_MIDDLE_CODE),
    (MAT_FRAME_SIZE - MAT_END_CODE.len(), &MAT_END_CODE),
];

const DTSHD_START_CODE: [u8; 10] = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFE, 0xFE];

const DCA_SAMPLE_RATES: [u32; 16] = [
    0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 96000, 192000,
];

const DCA_SYNCWORD_CORE_BE: u32 = 0x7FFE_8001;
const DCA_SYNCWORD_CORE_LE: u32 = 0xFE7F_0180;
const DCA_SYNCWORD_CORE_14B_BE: u32 = 0x1FFF_E800;
const DCA_SYNCWORD_CORE_14B_LE: u32 = 0xFF1F_00E8;
const DCA_SYNCWORD_SUBSTREAM: u32 = 0x6458_2025;

/// Compressed bitstream codec framed into IEC 61937 bursts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Iec61937Codec {
    /// Dolby Digital (AC-3): 2ch IEC 60958 @ stream sample rate (typically 48 kHz).
    Ac3,
    /// Dolby Digital Plus / Atmos (E-AC-3): 2ch IEC 60958 @ 4x sample rate (192 kHz).
    Eac3,
    /// Legacy DTS Coherent Acoustics core: 2ch IEC 60958 @ stream sample rate.
    DtsCore,
    /// DTS-HD Master Audio / DTS:X Type IV HBR (`dtshd_rate = 768_000` => 8ch 192 kHz).
    DtsHd { dtshd_rate: u32 },
    /// Dolby TrueHD / Atmos / MLP in Dolby MAT 2.0: 8ch 192 kHz HBR (`768_000` IEC frames/s).
    TrueHd,
}

impl Iec61937Codec {
    /// Resolves a codec id into its highest-fidelity IEC 61937 transport.
    pub fn from_codec_id(codec_id: &str, hbr_supported: bool) -> Option<Self> {
        match codec_id {
            "ac3" => Some(Self::Ac3),
            "eac3" => Some(Self::Eac3),
            "dts" => Some(if hbr_supported {
                Self::DtsHd { dtshd_rate: 768_000 }
            } else {
                Self::DtsCore
            }),
            "truehd" | "mlp" if hbr_supported => Some(Self::TrueHd),
            _ => None,
        }
    }

    /// Carrier PCM sample rate and channel count expected by an HDMI / S/PDIF sink.
    pub fn carrier_shape(self, stream_sample_rate: u32) -> (u32, u16) {
        let base = stream_sample_rate.max(48_000);
        match self {
            Self::Ac3 | Self::DtsCore => (stream_sample_rate.max(32_000), 2),
            Self::Eac3 => (
                if base == 44_100 || base == 88_200 || base == 176_400 {
                    176_400
                } else {
                    192_000
                },
                2,
            ),
            Self::DtsHd { dtshd_rate } => {
                if dtshd_rate >= 705_600 {
                    (dtshd_rate / 4, 8)
                } else {
                    (dtshd_rate.max(192_000), 2)
                }
            }
            Self::TrueHd => (
                if stream_sample_rate == 44_100 || stream_sample_rate == 88_200 || stream_sample_rate == 176_400 {
                    176_400
                } else {
                    192_000
                },
                8,
            ),
        }
    }
}

/// Inspects a compressed audio packet to count how many source PCM samples per
/// channel it spans on the media timeline.
pub fn packet_frame_samples(codec_id: &str, data: &[u8], fallback_rate: u32) -> u32 {
    match codec_id {
        "ac3" => 1536,
        "eac3" => crate::headers::codec::eac3_frame_samples(data).unwrap_or(1536),
        "dts" => dts_packet_samples(data).unwrap_or(512),
        "truehd" | "mlp" => truehd_packet_samples(data).unwrap_or(match fallback_rate {
            88_200 | 96_000 => 80,
            176_400 | 192_000 => 160,
            _ => 40,
        }),
        _ => 1536,
    }
}

fn dts_packet_samples(data: &[u8]) -> Option<u32> {
    if data.len() < 9 {
        return None;
    }
    let sync = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let blocks = match sync {
        DCA_SYNCWORD_CORE_BE => ((u16::from_be_bytes([data[4], data[5]]) >> 2) & 0x7F) as u32 + 1,
        DCA_SYNCWORD_CORE_LE => ((u16::from_le_bytes([data[4], data[5]]) >> 2) & 0x7F) as u32 + 1,
        DCA_SYNCWORD_CORE_14B_BE => {
            ((((data[5] & 0x07) << 4) | ((data[6] & 0x3F) >> 2)) as u32) + 1
        }
        DCA_SYNCWORD_CORE_14B_LE => {
            ((((data[4] & 0x07) << 4) | ((data[7] & 0x3F) >> 2)) as u32) + 1
        }
        _ => return None,
    };
    Some(blocks * 32)
}

fn truehd_packet_samples(data: &[u8]) -> Option<u32> {
    if data.len() < 10 {
        return None;
    }
    let body = &data[4..];
    if body[0] == 0xF8 && body[1] == 0x72 && body[2] == 0x6F {
        let ratebits = match body[3] {
            0xBA => body[4] >> 4,
            0xBB if body.len() >= 6 => body[5] >> 4,
            _ => return None,
        };
        return Some(40 << (ratebits & 3));
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IecError {
    InvalidPacket(&'static str),
    BitrateTooHigh,
}

impl std::fmt::Display for IecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPacket(msg) => write!(f, "invalid IEC 61937 input: {msg}"),
            Self::BitrateTooHigh => write!(f, "compressed audio bitrate exceeds IEC 61937 carrier"),
        }
    }
}

/// Stateful IEC 61937 packetizer keeping E-AC-3 block coalescing, DTS-HD
/// overflow fallback, and Dolby MAT 2.0 6-slot timing ring across packets.
pub struct Iec61937Packetizer {
    codec: Iec61937Codec,
    big_endian_wire: bool,
    eac3_buf: Vec<u8>,
    eac3_count: u8,
    dtshd_skip: u32,
    dtshd_buf: Vec<u8>,
    mat_ring: Box<[[u8; MAT_FRAME_SIZE]; HD_BUF_COUNT]>,
    mat_filled: usize,
    mat_idx: usize,
    mat_ready: VecDeque<usize>,
    truehd_prev_time: u16,
    truehd_prev_size: i32,
    truehd_samples_per_frame: i32,
    truehd_output_timing: u16,
    truehd_output_timing_valid: bool,
    truehd_oi_delta: i32,
}

impl Iec61937Packetizer {
    /// Creates a little-endian (`s16le` / Android `ENCODING_IEC61937`) packetizer.
    pub fn new(codec: Iec61937Codec) -> Self {
        Self {
            codec,
            big_endian_wire: false,
            eac3_buf: Vec::new(),
            eac3_count: 0,
            dtshd_skip: 0,
            dtshd_buf: Vec::new(),
            mat_ring: Box::new([[0u8; MAT_FRAME_SIZE]; HD_BUF_COUNT]),
            mat_filled: 0,
            mat_idx: 0,
            mat_ready: VecDeque::with_capacity(HD_BUF_COUNT),
            truehd_prev_time: 0,
            truehd_prev_size: 0,
            truehd_samples_per_frame: 0,
            truehd_output_timing: 0,
            truehd_output_timing_valid: false,
            truehd_oi_delta: 0,
        }
    }

    pub fn codec(&self) -> Iec61937Codec {
        self.codec
    }

    /// Resets buffered substreams after a seek or device flush.
    pub fn reset(&mut self) {
        self.eac3_buf.clear();
        self.eac3_count = 0;
        self.dtshd_skip = 0;
        self.dtshd_buf.clear();
        self.mat_filled = 0;
        self.mat_idx = 0;
        self.mat_ready.clear();
        self.truehd_prev_time = 0;
        self.truehd_prev_size = 0;
        self.truehd_output_timing = 0;
        self.truehd_output_timing_valid = false;
        self.truehd_oi_delta = 0;
    }

    /// Pushes one compressed packet and appends zero or more complete IEC 61937
    /// bursts into `out`, matching FFmpeg `spdif_write_packet` (at most one
    /// completed MAT burst emitted per TrueHD access unit).
    pub fn push_packet(&mut self, pkt: &[u8], out: &mut Vec<u8>) -> Result<(), IecError> {
        match self.codec {
            Iec61937Codec::Ac3 => {
                if pkt.len() < 6 {
                    return Err(IecError::InvalidPacket("AC-3 packet shorter than header"));
                }
                let bitstream_mode = u16::from(pkt[5] & 0x07);
                let data_type = IEC61937_AC3 | (bitstream_mode << 8);
                let pkt_offset = 1536 * 4;
                let length_code = ((pkt.len() + 1) & !1) << 3;
                if length_code > u16::MAX as usize {
                    return Err(IecError::BitrateTooHigh);
                }
                self.emit_burst(pkt, data_type, length_code as u16, pkt_offset, true, false, out)
            }
            Iec61937Codec::Eac3 => {
                if pkt.len() < 6 {
                    return Err(IecError::InvalidPacket("E-AC-3 packet shorter than header"));
                }
                if self.eac3_buf.len().saturating_add(pkt.len()) > MAX_BURST_BYTES {
                    self.eac3_buf.clear();
                    self.eac3_count = 0;
                    return Err(IecError::BitrateTooHigh);
                }
                const EAC3_REPEAT: [u8; 4] = [6, 3, 2, 1];
                let bsid = pkt[5] >> 3;
                let repeat = if bsid > 10 && (pkt[4] & 0xC0) != 0xC0 {
                    EAC3_REPEAT[((pkt[4] & 0x30) >> 4) as usize]
                } else {
                    1
                };
                self.eac3_buf.extend_from_slice(pkt);
                self.eac3_count = self.eac3_count.saturating_add(1);
                if self.eac3_count < repeat {
                    return Ok(());
                }
                let payload_len = self.eac3_buf.len();
                if payload_len > u16::MAX as usize {
                    self.eac3_buf.clear();
                    self.eac3_count = 0;
                    return Err(IecError::BitrateTooHigh);
                }
                let buf = std::mem::take(&mut self.eac3_buf);
                self.eac3_count = 0;
                let res = self.emit_burst(&buf, IEC61937_EAC3, payload_len as u16, 24_576, true, false, out);
                self.eac3_buf = buf;
                self.eac3_buf.clear();
                res
            }
            Iec61937Codec::DtsCore => self.push_dts(pkt, 0, out),
            Iec61937Codec::DtsHd { dtshd_rate } => self.push_dts(pkt, dtshd_rate, out),
            Iec61937Codec::TrueHd => {
                self.feed_truehd(pkt)?;
                if let Some(ready_idx) = self.mat_ready.pop_front() {
                    let mat_slice = self.mat_ring[ready_idx];
                    self.emit_burst(
                        &mat_slice,
                        IEC61937_TRUEHD,
                        MAT_FRAME_SIZE as u16,
                        MAT_PKT_OFFSET,
                        true,
                        false,
                        out,
                    )?;
                }
                Ok(())
            }
        }
    }

    /// Flushes any completed MAT frames still queued in the ring at EOS.
    pub fn drain(&mut self, out: &mut Vec<u8>) -> Result<(), IecError> {
        while let Some(ready_idx) = self.mat_ready.pop_front() {
            let mat_slice = self.mat_ring[ready_idx];
            self.emit_burst(
                &mat_slice,
                IEC61937_TRUEHD,
                MAT_FRAME_SIZE as u16,
                MAT_PKT_OFFSET,
                true,
                false,
                out,
            )?;
        }
        Ok(())
    }

    fn push_dts(&mut self, pkt: &[u8], dtshd_rate: u32, out: &mut Vec<u8>) -> Result<(), IecError> {
        if pkt.len() < 9 {
            return Err(IecError::InvalidPacket("DTS packet shorter than 9 bytes"));
        }
        let syncword = u32::from_be_bytes([pkt[0], pkt[1], pkt[2], pkt[3]]);
        let mut extra_bswap = false;
        let mut sample_rate = 0u32;
        let mut core_size = 0usize;
        let raw_blocks = match syncword {
            DCA_SYNCWORD_CORE_BE => {
                let b = (u16::from_be_bytes([pkt[4], pkt[5]]) >> 2) & 0x7F;
                let be24 = (u32::from(pkt[5]) << 16) | (u32::from(pkt[6]) << 8) | u32::from(pkt[7]);
                core_size = (((be24 >> 4) & 0x3FFF) as usize) + 1;
                sample_rate = DCA_SAMPLE_RATES[((pkt[8] >> 2) & 0x0F) as usize];
                usize::from(b)
            }
            DCA_SYNCWORD_CORE_LE => {
                extra_bswap = true;
                usize::from((u16::from_le_bytes([pkt[4], pkt[5]]) >> 2) & 0x7F)
            }
            DCA_SYNCWORD_CORE_14B_BE => {
                usize::from(((pkt[5] & 0x07) << 4) | ((pkt[6] & 0x3F) >> 2))
            }
            DCA_SYNCWORD_CORE_14B_LE => {
                extra_bswap = true;
                usize::from(((pkt[4] & 0x07) << 4) | ((pkt[7] & 0x3F) >> 2))
            }
            DCA_SYNCWORD_SUBSTREAM => {
                return Err(IecError::InvalidPacket("stray coreless DTS-HD substream"));
            }
            _ => return Err(IecError::InvalidPacket("bad DTS syncword")),
        };
        let blocks = raw_blocks + 1;

        if dtshd_rate > 0 {
            if core_size == 0 || sample_rate == 0 {
                return Err(IecError::InvalidPacket("DTS-HD requires BE core size and sample rate"));
            }
            let period = (u64::from(dtshd_rate) * ((blocks as u64) << 5)) / u64::from(sample_rate);
            let subtype: u16 = match period {
                512 => 0x0,
                1024 => 0x1,
                2048 => 0x2,
                4096 => 0x3,
                8192 => 0x4,
                16384 => 0x5,
                _ => return Err(IecError::InvalidPacket("unsupported DTS-HD repetition period")),
            };
            let pkt_offset = (period as usize) * 4;
            let data_type = IEC61937_DTSHD | (subtype << 8);
            let mut pkt_size = pkt.len();
            if DTSHD_START_CODE.len() + 2 + pkt_size > pkt_offset - BURST_HEADER_SIZE && core_size > 0 {
                self.dtshd_skip = (sample_rate * 60) / ((blocks as u32) << 5);
            }
            if self.dtshd_skip > 0 && core_size > 0 && core_size <= pkt.len() {
                pkt_size = core_size;
                self.dtshd_skip -= 1;
            }
            if pkt_size > u16::MAX as usize {
                return Err(IecError::BitrateTooHigh);
            }
            let out_bytes = DTSHD_START_CODE.len() + 2 + pkt_size;
            let length_code = ((out_bytes + 0x8 + 0xF) & !0xF) - 0x8;
            if length_code > u16::MAX as usize {
                return Err(IecError::BitrateTooHigh);
            }
            self.dtshd_buf.clear();
            self.dtshd_buf.reserve(out_bytes);
            self.dtshd_buf.extend_from_slice(&DTSHD_START_CODE);
            self.dtshd_buf.extend_from_slice(&(pkt_size as u16).to_be_bytes());
            self.dtshd_buf.extend_from_slice(&pkt[..pkt_size]);
            let buf = std::mem::take(&mut self.dtshd_buf);
            let res = self.emit_burst(
                &buf,
                data_type,
                length_code as u16,
                pkt_offset,
                true,
                extra_bswap,
                out,
            );
            self.dtshd_buf = buf;
            self.dtshd_buf.clear();
            return res;
        }

        let data_type = match blocks {
            16 => IEC61937_DTS1,
            32 => IEC61937_DTS2,
            64 => IEC61937_DTS3,
            _ => return Err(IecError::InvalidPacket("unsupported DTS core block count")),
        };
        let payload = if core_size > 0 && core_size < pkt.len() {
            &pkt[..core_size]
        } else {
            pkt
        };
        let length_code = if core_size > 0 && core_size < pkt.len() {
            core_size << 3
        } else {
            ((payload.len() + 1) & !1) << 3
        };
        if length_code > u16::MAX as usize {
            return Err(IecError::BitrateTooHigh);
        }
        let pkt_offset = blocks << 7;
        let use_preamble = payload.len() != pkt_offset;
        self.emit_burst(
            payload,
            data_type,
            length_code as u16,
            pkt_offset,
            use_preamble,
            extra_bswap,
            out,
        )
    }

    fn feed_truehd(&mut self, pkt: &[u8]) -> Result<(), IecError> {
        if pkt.len() < 10 {
            return Err(IecError::InvalidPacket("TrueHD packet shorter than 10 bytes"));
        }
        let au = parse_truehd_au(pkt)?;
        if au.samples_per_frame > 0 {
            self.truehd_samples_per_frame = au.samples_per_frame;
        }
        if self.truehd_samples_per_frame <= 0 {
            return Err(IecError::InvalidPacket("TrueHD stream missing major sync header"));
        }
        let input_timing = au.input_timing;
        self.truehd_output_timing = self
            .truehd_output_timing
            .wrapping_add(self.truehd_samples_per_frame as u16);

        let max_padding = (MAT_FRAME_SIZE * 2) as i32;
        let mut padding_remaining = 0i32;
        let mut output_discontinuity = false;

        if let Some(out_timing) = au.output_timing {
            if self.truehd_output_timing_valid && out_timing != self.truehd_output_timing {
                let bytes_per_sample = TRUEHD_NOMINAL_AU_SPACING / self.truehd_samples_per_frame;
                let out_minus_spf = out_timing.wrapping_sub(self.truehd_samples_per_frame as u16);
                let prev_oi_delta = self.truehd_oi_delta;
                let cur_oi_delta = i32::from(out_minus_spf.wrapping_sub(input_timing) as i16);
                output_discontinuity = true;
                let disc_padding = (prev_oi_delta - cur_oi_delta) * bytes_per_sample;
                let prev_padding = if self.truehd_prev_size > 0 {
                    TRUEHD_NOMINAL_AU_SPACING - self.truehd_prev_size
                } else {
                    0
                };
                padding_remaining = prev_padding + disc_padding;
                if padding_remaining < 0 || padding_remaining > max_padding {
                    padding_remaining = 0;
                }
            }
            self.truehd_output_timing = out_timing;
            self.truehd_output_timing_valid = true;
        }

        if self.truehd_prev_size > 0 && !output_discontinuity {
            let delta_samples = i32::from(input_timing.wrapping_sub(self.truehd_prev_time));
            let delta_bytes =
                (delta_samples * TRUEHD_NOMINAL_AU_SPACING) / self.truehd_samples_per_frame;
            padding_remaining = delta_bytes - self.truehd_prev_size;
            if padding_remaining < 0 || padding_remaining > max_padding {
                padding_remaining = 0;
            }
        }

        if self.truehd_output_timing_valid {
            let out_minus_spf = self
                .truehd_output_timing
                .wrapping_sub(self.truehd_samples_per_frame as u16);
            self.truehd_oi_delta = i32::from(out_minus_spf.wrapping_sub(input_timing) as i16);
        }

        let mut next_code_idx = MAT_CODES
            .iter()
            .position(|&(pos, _)| self.mat_filled <= pos)
            .ok_or(IecError::InvalidPacket("MAT buffer cursor past end code"))?;

        let mut total_frame_size = pkt.len() as i32;
        let mut data_offset = 0usize;
        let mut data_remaining = pkt.len();
        let mut pad = padding_remaining as usize;

        while pad > 0 || data_remaining > 0 || MAT_CODES[next_code_idx].0 == self.mat_filled {
            if MAT_CODES[next_code_idx].0 == self.mat_filled {
                let (pos, code) = MAT_CODES[next_code_idx];
                let code_len = code.len();
                let mut code_len_rem = code_len;
                self.mat_ring[self.mat_idx][pos..pos + code_len].copy_from_slice(code);
                self.mat_filled += code_len;
                next_code_idx += 1;
                if next_code_idx == MAT_CODES.len() {
                    next_code_idx = 0;
                    if self.mat_ready.len() >= HD_BUF_COUNT - 1 {
                        return Err(IecError::BitrateTooHigh);
                    }
                    self.mat_ready.push_back(self.mat_idx);
                    self.mat_idx = (self.mat_idx + 1) % HD_BUF_COUNT;
                    self.mat_filled = 0;
                    code_len_rem += MAT_PKT_OFFSET - MAT_FRAME_SIZE;
                }
                if pad > 0 {
                    let counted = pad.min(code_len_rem);
                    pad -= counted;
                    code_len_rem -= counted;
                }
                if code_len_rem > 0 {
                    total_frame_size += code_len_rem as i32;
                }
            }

            if pad > 0 {
                let room = MAT_CODES[next_code_idx].0 - self.mat_filled;
                let n = room.min(pad);
                self.mat_ring[self.mat_idx][self.mat_filled..self.mat_filled + n].fill(0);
                self.mat_filled += n;
                pad -= n;
                if pad > 0 {
                    continue;
                }
            }

            if data_remaining > 0 {
                let room = MAT_CODES[next_code_idx].0 - self.mat_filled;
                let n = room.min(data_remaining);
                self.mat_ring[self.mat_idx][self.mat_filled..self.mat_filled + n]
                    .copy_from_slice(&pkt[data_offset..data_offset + n]);
                self.mat_filled += n;
                data_offset += n;
                data_remaining -= n;
            }
        }

        self.truehd_prev_size = total_frame_size;
        self.truehd_prev_time = input_timing;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_burst(
        &self,
        payload: &[u8],
        data_type: u16,
        length_code: u16,
        pkt_offset: usize,
        use_preamble: bool,
        extra_bswap: bool,
        out: &mut Vec<u8>,
    ) -> Result<(), IecError> {
        let header_bytes = if use_preamble { BURST_HEADER_SIZE } else { 0 };
        let aligned_payload = (payload.len() + 1) & !1;
        if header_bytes + aligned_payload > pkt_offset {
            return Err(IecError::BitrateTooHigh);
        }
        let padding = (pkt_offset - header_bytes - payload.len()) & !1;
        out.reserve(pkt_offset);

        if use_preamble {
            put_u16(out, SYNCWORD1, self.big_endian_wire);
            put_u16(out, SYNCWORD2, self.big_endian_wire);
            put_u16(out, data_type, self.big_endian_wire);
            put_u16(out, length_code, self.big_endian_wire);
        }

        let even_len = payload.len() & !1;
        if extra_bswap ^ self.big_endian_wire {
            out.extend_from_slice(&payload[..even_len]);
        } else {
            for pair in payload[..even_len].chunks_exact(2) {
                out.push(pair[1]);
                out.push(pair[0]);
            }
        }

        if payload.len() & 1 != 0 {
            let tail = u16::from(payload[payload.len() - 1]) << 8;
            put_u16(out, tail, self.big_endian_wire);
        }

        out.resize(out.len() + padding, 0);
        Ok(())
    }
}

#[inline]
fn put_u16(out: &mut Vec<u8>, val: u16, be: bool) {
    if be {
        out.extend_from_slice(&val.to_be_bytes());
    } else {
        out.extend_from_slice(&val.to_le_bytes());
    }
}

struct TrueHdAuInfo {
    input_timing: u16,
    output_timing: Option<u16>,
    samples_per_frame: i32,
}

struct BitCursor<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn bits_left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.bit_pos)
    }

    fn get_bit(&mut self) -> Option<u32> {
        if self.bit_pos >= self.data.len() * 8 {
            return None;
        }
        let byte = self.data[self.bit_pos >> 3];
        let shift = 7 - (self.bit_pos & 7);
        self.bit_pos += 1;
        Some(u32::from((byte >> shift) & 1))
    }

    fn skip(&mut self, n: usize) -> bool {
        if self.bits_left() < n {
            return false;
        }
        self.bit_pos += n;
        true
    }

    fn get_bits(&mut self, n: usize) -> Option<u32> {
        if n == 0 || n > 32 || self.bits_left() < n {
            return None;
        }
        let mut acc = 0u32;
        for _ in 0..n {
            acc = (acc << 1) | self.get_bit()?;
        }
        Some(acc)
    }
}

fn parse_truehd_au(au_data: &[u8]) -> Result<TrueHdAuInfo, IecError> {
    if au_data.len() < 4 {
        return Err(IecError::InvalidPacket("TrueHD AU shorter than 4 bytes"));
    }
    let input_timing = u16::from_be_bytes([au_data[2], au_data[3]]);
    let data = &au_data[4..];
    let mut info = TrueHdAuInfo {
        input_timing,
        output_timing: None,
        samples_per_frame: 0,
    };

    if data.len() < 6 || data[0] != 0xF8 || data[1] != 0x72 || data[2] != 0x6F {
        return Ok(info);
    }

    let ratebits = match data[3] {
        0xBA => data[4] >> 4,
        0xBB => data[5] >> 4,
        _ => return Err(IecError::InvalidPacket("unknown MLP/TrueHD sync signature")),
    };
    info.samples_per_frame = 40 << (ratebits & 3);

    if data.len() < 27 {
        return Ok(info);
    }

    let mut major_sync_size = 28usize;
    if data[3] == 0xBA && (data[25] & 1) != 0 {
        let ext_size = usize::from(data[26] >> 4);
        major_sync_size += 2 + ext_size * 2;
    }
    if major_sync_size > data.len() {
        return Ok(info);
    }

    let num_substreams = usize::from(data[16] >> 4);
    if num_substreams == 0 {
        return Ok(info);
    }

    let mut gb = BitCursor::new(&data[major_sync_size..]);
    for _ in 0..num_substreams {
        if gb.bits_left() < 16 {
            return Ok(info);
        }
        let extra_word = gb.get_bit().unwrap_or(0);
        if !gb.skip(15) {
            return Ok(info);
        }
        if extra_word != 0 && !gb.skip(16) {
            return Ok(info);
        }
    }

    if gb.get_bit() != Some(1) {
        return Ok(info);
    }
    if gb.get_bit() != Some(1) {
        return Ok(info);
    }
    if gb.bits_left() < 13 + 1 + 16 {
        return Ok(info);
    }
    let sync_word = gb.get_bits(13).unwrap_or(0);
    if sync_word != (0x31EA >> 1) {
        return Ok(info);
    }
    let _ = gb.get_bit();
    if let Some(ot) = gb.get_bits(16) {
        info.output_timing = Some(ot as u16);
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::MediaType;
    use std::fs::File;

    fn packetize_stream(path: &std::path::Path, codec: Iec61937Codec) -> Vec<u8> {
        let ctx = codecs::context();
        let fmt = refcheck::probe_container(&ctx, path).expect("probe container");
        let file = File::open(path).expect("open sample");
        let mut demuxer = ctx
            .containers
            .open_demuxer(&fmt, Box::new(file), &ctx.codecs)
            .expect("open demuxer");
        let stream_idx = demuxer
            .streams()
            .iter()
            .find(|s| s.params.media_type == MediaType::Audio)
            .expect("audio stream")
            .index;
        let mut pktzr = Iec61937Packetizer::new(codec);
        let mut out = Vec::new();
        loop {
            match demuxer.next_packet() {
                Ok(pkt) if pkt.stream_index == stream_idx => {
                    pktzr.push_packet(&pkt.data, &mut out).expect("packetize");
                }
                Ok(_) => {}
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => panic!("demux error: {e}"),
            }
        }
        out
    }

    fn ffmpeg_spdif(path: &std::path::Path, extra_args: &[&str]) -> Vec<u8> {
        let mut cmd = std::process::Command::new(refcheck::pinned_ffmpeg());
        cmd.args(["-v", "error", "-nostdin", "-i"]).arg(path);
        cmd.args(["-map", "0:a:0", "-c:a", "copy"]);
        cmd.args(extra_args);
        cmd.args(["-f", "spdif", "-"]);
        let out = cmd.output().expect("run pinned ffmpeg -f spdif");
        assert!(
            out.status.success(),
            "ffmpeg spdif failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    #[test]
    fn iec61937_ac3_eac3_dts_dtshd_and_truehd_mat_match_ffmpeg_spdif_byte_for_byte() {
        // 1. AC-3
        let ac3_path = refcheck::fate("ac3/millers_crossing_4.0.ac3");
        let ours_ac3 = packetize_stream(&ac3_path, Iec61937Codec::Ac3);
        let ref_ac3 = ffmpeg_spdif(&ac3_path, &[]);
        assert_eq!(refcheck::md5_hex(&ours_ac3), refcheck::md5_hex(&ref_ac3), "AC-3 IEC 61937 mismatch");

        // 2. E-AC-3
        let eac3_path = refcheck::fate("eac3/csi_miami_5.1_256_spx_small.eac3");
        let ours_eac3 = packetize_stream(&eac3_path, Iec61937Codec::Eac3);
        let ref_eac3 = ffmpeg_spdif(&eac3_path, &[]);
        assert_eq!(refcheck::md5_hex(&ours_eac3), refcheck::md5_hex(&ref_eac3), "E-AC-3 IEC 61937 mismatch");

        // 3. DTS Core
        let dts_path = refcheck::fate("dts/dts_es.dts");
        let ours_dts = packetize_stream(&dts_path, Iec61937Codec::DtsCore);
        let ref_dts = ffmpeg_spdif(&dts_path, &[]);
        assert_eq!(refcheck::md5_hex(&ours_dts), refcheck::md5_hex(&ref_dts), "DTS Core IEC 61937 mismatch");

        // 4. DTS-HD Master Audio (XLL 8ch 192 kHz HBR: -dtshd_rate 768000)
        let dtshd_path = refcheck::fate("dts/dcadec-suite/xll_51_24_48_768.dtshd");
        let ours_dtshd = packetize_stream(&dtshd_path, Iec61937Codec::DtsHd { dtshd_rate: 768_000 });
        let ref_dtshd = ffmpeg_spdif(&dtshd_path, &["-dtshd_rate", "768000"]);
        assert_eq!(
            refcheck::md5_hex(&ours_dtshd),
            refcheck::md5_hex(&ref_dtshd),
            "DTS-HD MA 8ch 192kHz HBR IEC 61937 mismatch"
        );

        // 5. Dolby TrueHD Atmos in Dolby MAT 2.0 (8ch 192 kHz HBR)
        let thd_path = refcheck::fate("truehd/atmos.thd");
        let ours_thd = packetize_stream(&thd_path, Iec61937Codec::TrueHd);
        let ref_thd = ffmpeg_spdif(&thd_path, &[]);
        assert_eq!(
            refcheck::md5_hex(&ours_thd),
            refcheck::md5_hex(&ref_thd),
            "TrueHD Atmos Dolby MAT 2.0 IEC 61937 mismatch"
        );
    }
}
