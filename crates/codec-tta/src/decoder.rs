// TTA (True Audio) decoder.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/tta.c, ttadsp.c,
// ttadata.c and ttadata.h, with get_bits.h's little-endian reader
// (`BITSTREAM_READER_LE`) and unary.h's get_unary.
// Copyright (c) 2006 Alex Beregszaszi (tta.c); LGPL-2.1-or-later (see
// LICENSE).

use std::collections::VecDeque;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

/// `ff_tta_shift_1`; `ff_tta_shift_16` is this table from index 4.
const SHIFT_1: [u32; 41] = [
    0x0000_0001, 0x0000_0002, 0x0000_0004, 0x0000_0008, 0x0000_0010, 0x0000_0020, 0x0000_0040, 0x0000_0080,
    0x0000_0100, 0x0000_0200, 0x0000_0400, 0x0000_0800, 0x0000_1000, 0x0000_2000, 0x0000_4000, 0x0000_8000,
    0x0001_0000, 0x0002_0000, 0x0004_0000, 0x0008_0000, 0x0010_0000, 0x0020_0000, 0x0040_0000, 0x0080_0000,
    0x0100_0000, 0x0200_0000, 0x0400_0000, 0x0800_0000, 0x1000_0000, 0x2000_0000, 0x4000_0000, 0x8000_0000,
    0x8000_0000, 0x8000_0000, 0x8000_0000, 0x8000_0000, 0x8000_0000, 0x8000_0000, 0x8000_0000, 0x8000_0000,
    0xFFFF_FFFF,
];

#[inline]
fn shift_16(k: u32) -> u32 {
    SHIFT_1[k as usize + 4]
}

/// `ff_tta_filter_configs`: the hybrid filter's shift per bytes per sample.
const FILTER_CONFIGS: [i32; 4] = [10, 9, 10, 12];

/// `MIN_CACHE_BITS`: the most bits FFmpeg's get_bits reads at once.
const MIN_CACHE_BITS: u32 = 25;

/// FFmpeg's channel limit.
const MAX_CHANNELS: u16 = 16;

const FORMAT_ENCRYPTED: u16 = 2;

/// get_bits.h's little-endian reader. Reads past the end give zeros, as
/// FFmpeg's packet padding does; the decoder never reads past it.
struct Bits<'a> {
    buf: &'a [u8],
    index: usize,
    size_in_bits: usize,
}

impl<'a> Bits<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, index: 0, size_in_bits: buf.len() * 8 }
    }

    /// At least 25 valid bits from `index` on, least significant first.
    #[inline]
    fn peek(&self) -> u32 {
        let p = self.index >> 3;
        let b = |i: usize| u32::from(self.buf.get(p + i).copied().unwrap_or(0));
        (b(0) | b(1) << 8 | b(2) << 16 | b(3) << 24) >> (self.index & 7)
    }

    /// `get_bits`, `n` at most 25.
    #[inline]
    fn get_bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek() & ((1u32 << n) - 1);
        self.index += n as usize;
        v
    }

    #[inline]
    fn bits_left(&self) -> isize {
        self.size_in_bits as isize - self.index as isize
    }

    /// `get_unary(gb, 0, len)`: ones until a zero (read too) or `len` bits.
    #[inline]
    fn get_unary(&mut self, len: isize) -> u32 {
        let mut i: isize = 0;
        while i < len {
            let avail = (len - i).min(MIN_CACHE_BITS as isize) as u32;
            let ones = self.peek().trailing_ones().min(avail);
            if ones < avail {
                self.index += ones as usize + 1;
                return (i + ones as isize) as u32;
            }
            self.index += avail as usize;
            i += avail as isize;
        }
        i as u32
    }

    /// `align_get_bits`
    fn align(&mut self) {
        self.index = (self.index + 7) & !7;
    }
}

/// `TTAFilter`
#[derive(Clone, Copy, Default)]
struct Filter {
    qm: [i32; 8],
    dx: [i32; 8],
    dl: [i32; 8],
    shift: i32,
    round: i32,
    error: i32,
}

impl Filter {
    /// `ff_tta_filter_init`
    fn new(shift: i32) -> Self {
        Self { shift, round: SHIFT_1[(shift - 1) as usize] as i32, ..Self::default() }
    }

    /// `tta_filter_process_c`: the adaptive filter, in FFmpeg's wrapping
    /// 32-bit arithmetic.
    #[inline]
    fn process(&mut self, input: i32) -> i32 {
        let (qm, dx, dl) = (&mut self.qm, &mut self.dx, &mut self.dl);
        if self.error < 0 {
            for i in 0..8 {
                qm[i] = qm[i].wrapping_sub(dx[i]);
            }
        } else if self.error > 0 {
            for i in 0..8 {
                qm[i] = qm[i].wrapping_add(dx[i]);
            }
        }
        let mut round = self.round as u32;
        for i in 0..8 {
            round = round.wrapping_add((dl[i] as u32).wrapping_mul(qm[i] as u32));
        }
        let round = round as i32;

        dx[0] = dx[1];
        dx[1] = dx[2];
        dx[2] = dx[3];
        dx[3] = dx[4];
        dl[0] = dl[1];
        dl[1] = dl[2];
        dl[2] = dl[3];
        dl[3] = dl[4];

        dx[4] = (dl[4] >> 30) | 1;
        dx[5] = ((dl[5] >> 30) | 2) & !1;
        dx[6] = ((dl[6] >> 30) | 2) & !1;
        dx[7] = ((dl[7] >> 30) | 4) & !3;

        self.error = input;
        let out = input.wrapping_add(round >> self.shift);

        dl[4] = dl[5].wrapping_neg();
        dl[5] = dl[6].wrapping_neg();
        dl[6] = out.wrapping_sub(dl[7]);
        dl[7] = out;
        dl[5] = dl[5].wrapping_add(dl[6]);
        dl[4] = dl[4].wrapping_add(dl[5]);
        out
    }
}

/// `TTARice`
#[derive(Clone, Copy)]
struct Rice {
    k0: u32,
    k1: u32,
    sum0: u32,
    sum1: u32,
}

impl Rice {
    /// `ff_tta_rice_init(c, 10, 10)`
    fn new() -> Self {
        Self { k0: 10, k1: 10, sum0: shift_16(10), sum1: shift_16(10) }
    }
}

/// `TTAChannel`
#[derive(Clone, Copy)]
struct Channel {
    filter: Filter,
    predictor: i32,
    rice: Rice,
}

/// `PRED(x, k)`: `(int32_t)((((uint64_t)x << k) - x) >> k)`.
#[inline]
fn pred(x: i32, k: u32) -> i32 {
    let x = x as i64 as u64;
    ((x << k).wrapping_sub(x) >> k) as i32
}

/// `tta_check_crc64`: the key an encrypted stream's filters start from.
fn crc64(pass: &[u8]) -> u64 {
    const POLY: u64 = 0x42F0_E1EB_A9EA_3693;
    let mut crc = u64::MAX;
    for &b in pass.iter().take_while(|&&b| b != 0) {
        crc ^= u64::from(b) << 56;
        for _ in 0..8 {
            crc = (crc << 1) ^ (POLY & ((crc as i64 >> 63) as u64));
        }
    }
    crc ^ u64::MAX
}

/// The `tta` decoder. One packet is one frame.
pub struct TtaDecoder {
    codec_id: CodecId,
    encrypted: bool,
    crc_pass: [u8; 8],
    channels: usize,
    /// Bytes per sample: 1, 2 or 3.
    bps: usize,
    sample_rate: u32,
    frame_length: usize,
    last_frame_length: usize,
    ready: VecDeque<Frame>,
}

impl TtaDecoder {
    /// `tta_decode_init`: the stream's 22-byte TTA1 header in the extradata;
    /// an encrypted stream (format 2) needs the `password` option.
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let e = &params.extradata;
        if e.len() < 22 {
            return Err(Error::invalid("tta: extradata shorter than a TTA1 header"));
        }
        if &e[0..4] != b"TTA1" {
            return Err(Error::invalid("tta: wrong extradata present"));
        }
        let le16 = |at: usize| u16::from_le_bytes([e[at], e[at + 1]]);
        let le32 = |at: usize| u32::from_le_bytes([e[at], e[at + 1], e[at + 2], e[at + 3]]);
        let format = le16(4);
        if format > 2 {
            return Err(Error::invalid("tta: invalid format"));
        }
        let mut crc_pass = [0u8; 8];
        if format == FORMAT_ENCRYPTED {
            let pass = params
                .options
                .get("password")
                .ok_or_else(|| Error::invalid("tta: missing password for encrypted stream"))?;
            crc_pass = crc64(pass.as_bytes()).to_le_bytes();
        }
        let channels = le16(6);
        let bits = le16(8);
        let bps = usize::from(bits.div_ceil(8));
        let sample_rate = le32(10);
        let data_length = le32(14);
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(Error::invalid("tta: invalid number of channels"));
        }
        if sample_rate == 0 {
            return Err(Error::invalid("tta: invalid samplerate"));
        }
        if !(1..=3).contains(&bps) {
            return Err(Error::invalid("tta: invalid/unsupported sample format"));
        }
        if sample_rate > 0x7F_FFFF {
            return Err(Error::invalid("tta: sample_rate too large"));
        }
        let frame_length = 256 * sample_rate / 245;
        if u64::from(frame_length) >= u64::from(u32::MAX) / (u64::from(channels) * 4) {
            return Err(Error::invalid("tta: frame_length too large"));
        }
        Ok(Self {
            codec_id: params.codec_id.clone(),
            encrypted: format == FORMAT_ENCRYPTED,
            crc_pass,
            channels: usize::from(channels),
            bps,
            sample_rate,
            frame_length: frame_length as usize,
            last_frame_length: (data_length % frame_length) as usize,
            ready: VecDeque::new(),
        })
    }

    fn sample_format(&self) -> SampleFormat {
        match self.bps {
            1 => SampleFormat::U8,
            2 => SampleFormat::S16,
            _ => SampleFormat::S32,
        }
    }

    /// `tta_decode_frame`: the frame's samples, interleaved, and their count
    /// per channel.
    fn decode_frame(&self, data: &[u8]) -> Result<(usize, Vec<i32>)> {
        let channels = self.channels;
        let mut gb = Bits::new(data);
        let mut ch = vec![
            Channel {
                filter: Filter::new(FILTER_CONFIGS[self.bps - 1]),
                predictor: 0,
                rice: Rice::new(),
            };
            channels
        ];
        if self.encrypted {
            for c in &mut ch {
                for j in 0..8 {
                    c.filter.qm[j] = i32::from(self.crc_pass[j] as i8);
                }
            }
        }

        let mut framelen = self.frame_length;
        // Every sample read leaves at least one bit read and the frame's
        // 32-bit CRC to come, or FFmpeg fails the frame at its end: what a
        // frame can hold is bounded by its bytes, not the header's length.
        let mut buf: Vec<i32> = Vec::with_capacity((framelen * channels).min(data.len() * 8));
        let mut cur_chan = 0;
        let mut i = 0;
        while buf.len() < framelen * channels {
            // FFmpeg reads on and fails the frame at its end; nothing it
            // reads past this point changes that.
            if gb.bits_left() < 32 {
                return Err(Error::invalid("tta: frame shorter than its samples"));
            }
            let c = &mut ch[cur_chan];
            let mut unary = gb.get_unary(gb.bits_left());
            let (depth, k) = if unary == 0 {
                (0, c.rice.k0)
            } else {
                unary -= 1;
                (1, c.rice.k1)
            };
            if gb.bits_left() < k as isize {
                return Err(Error::invalid("tta: frame shorter than its samples"));
            }
            let mut value: i32 = if k > 0 {
                if k > MIN_CACHE_BITS || unary > (i32::MAX as u32) >> k {
                    return Err(Error::invalid("tta: invalid rice code"));
                }
                ((unary << k) + gb.get_bits(k)) as i32
            } else {
                unary as i32
            };

            let rice = &mut c.rice;
            if depth == 1 {
                rice.sum1 = rice.sum1.wrapping_add((value as u32).wrapping_sub(rice.sum1 >> 4));
                if rice.k1 > 0 && rice.sum1 < shift_16(rice.k1) {
                    rice.k1 -= 1;
                } else if rice.sum1 > shift_16(rice.k1 + 1) {
                    rice.k1 += 1;
                }
                value = (value as u32).wrapping_add(SHIFT_1[rice.k0 as usize]) as i32;
            }
            rice.sum0 = rice.sum0.wrapping_add((value as u32).wrapping_sub(rice.sum0 >> 4));
            if rice.k0 > 0 && rice.sum0 < shift_16(rice.k0) {
                rice.k0 -= 1;
            } else if rice.sum0 > shift_16(rice.k0 + 1) {
                rice.k0 += 1;
            }

            // The coded value, then the hybrid filter and the fixed predictor.
            value = 1i32.wrapping_add((value >> 1) ^ ((value & 1) - 1));
            value = c.filter.process(value);
            value = value.wrapping_add(match self.bps {
                1 => pred(c.predictor, 4),
                _ => pred(c.predictor, 5),
            });
            c.predictor = value;
            buf.push(value);

            if cur_chan < channels - 1 {
                cur_chan += 1;
            } else {
                // Decorrelate the frame's channels, last to first.
                if channels > 1 {
                    let p = buf.len() - 1;
                    buf[p] = buf[p].wrapping_add(buf[p - 1] / 2);
                    for r in (p + 1 - channels..p).rev() {
                        buf[r] = buf[r + 1].wrapping_sub(buf[r]);
                    }
                }
                cur_chan = 0;
                i += 1;
                if i == self.last_frame_length && gb.bits_left() / 8 == 4 {
                    framelen = self.last_frame_length;
                    break;
                }
            }
        }

        gb.align();
        if gb.bits_left() < 32 {
            return Err(Error::invalid("tta: frame without its CRC"));
        }
        Ok((framelen, buf))
    }
}

impl Decoder for TtaDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: self.sample_format(),
            sample_rate: self.sample_rate,
            channels: self.channels as u16,
        })
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        let (samples, buf) = self.decode_frame(&packet.data)?;
        let bytes: Vec<u8> = match self.bps {
            1 => buf.iter().map(|&v| v.wrapping_add(0x80) as u8).collect(),
            2 => buf.iter().flat_map(|&v| (v as i16).to_le_bytes()).collect(),
            _ => buf.iter().flat_map(|&v| ((v as u32).wrapping_mul(256) as i32).to_le_bytes()).collect(),
        };
        self.ready.push_back(Frame::Audio(AudioFrame { samples: samples as u32, pts: packet.pts, data: vec![bytes] }));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.ready.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// Every frame starts from fresh filters and predictors; nothing else
    /// carries over.
    fn reset(&mut self) -> Result<()> {
        self.ready.clear();
        Ok(())
    }
}
