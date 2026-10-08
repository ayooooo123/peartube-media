//! A bounds-checked file cursor (`FileReader`) and sample decoding, ported
//! from libopenmpt 0.8.9 `soundlib/SampleIO.cpp`, `ModSampleCopy.h`,
//! `SampleFormatConverters.h` and `ITCompression.cpp`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

#![allow(dead_code)]

use std::cell::Cell;

use crate::defs::*;
use crate::sample::{ModSample, POST_FRAMES, PRE_FRAMES};

/// Decoded sample memory one module may allocate. Compressed IT samples
/// expand up to 16-fold and many sample headers may point at the same
/// bytes, so the file size alone does not bound it.
pub const SAMPLE_BUDGET_BYTES: usize = 512 << 20;

thread_local! {
    static SAMPLE_BUDGET: Cell<usize> = const { Cell::new(SAMPLE_BUDGET_BYTES) };
    static SAMPLE_BUDGET_EXCEEDED: Cell<bool> = const { Cell::new(false) };
}

/// Starts a module's sample budget over; `load` calls it first.
pub fn reset_sample_budget() {
    SAMPLE_BUDGET.with(|b| b.set(SAMPLE_BUDGET_BYTES));
    SAMPLE_BUDGET_EXCEEDED.with(|b| b.set(false));
}

pub fn sample_budget_exceeded() -> bool {
    SAMPLE_BUDGET_EXCEEDED.with(Cell::get)
}

/// Takes `bytes` from the budget; false (taking nothing) when it would run
/// out.
fn take_sample_budget(bytes: usize) -> bool {
    SAMPLE_BUDGET.with(|b| match b.get().checked_sub(bytes) {
        Some(left) => {
            b.set(left);
            true
        }
        None => {
            SAMPLE_BUDGET_EXCEEDED.with(|b| b.set(true));
            false
        }
    })
}

/// Cursor over the module file. Reads past the end yield zeros and do not
/// advance, as libopenmpt's `FileReader` fails such reads.
#[derive(Clone, Copy)]
pub struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }
    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn bytes_left(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
    pub fn can_read(&self, n: usize) -> bool {
        self.bytes_left() >= n
    }
    /// `Seek`: fails (and stays put) past the end.
    pub fn seek(&mut self, pos: usize) -> bool {
        if pos <= self.data.len() {
            self.pos = pos;
            true
        } else {
            false
        }
    }
    /// `Skip`: clamps at the end.
    pub fn skip(&mut self, n: usize) -> bool {
        if self.can_read(n) {
            self.pos += n;
            true
        } else {
            self.pos = self.data.len();
            false
        }
    }
    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos.min(self.data.len())..]
    }
    pub fn read_bytes<const N: usize>(&mut self) -> Option<[u8; N]> {
        if self.can_read(N) {
            let mut b = [0u8; N];
            b.copy_from_slice(&self.data[self.pos..self.pos + N]);
            self.pos += N;
            Some(b)
        } else {
            None
        }
    }
    pub fn read_array<const N: usize>(&mut self) -> [u8; N] {
        self.read_bytes::<N>().unwrap_or([0; N])
    }
    pub fn read_slice(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.can_read(n) {
            let s = &self.data[self.pos..self.pos + n];
            self.pos += n;
            Some(s)
        } else {
            None
        }
    }
    /// `ReadChunk`: up to `n` bytes as a sub-reader; advances past them.
    pub fn read_chunk(&mut self, n: usize) -> Reader<'a> {
        let n = n.min(self.bytes_left());
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Reader::new(s)
    }
    pub fn read_magic(&mut self, magic: &[u8]) -> bool {
        if self.can_read(magic.len()) && &self.data[self.pos..self.pos + magic.len()] == magic {
            self.pos += magic.len();
            true
        } else {
            false
        }
    }
    pub fn u8(&mut self) -> u8 {
        self.read_bytes::<1>().map_or(0, |b| b[0])
    }
    pub fn i8(&mut self) -> i8 {
        self.u8() as i8
    }
    pub fn u16le(&mut self) -> u16 {
        self.read_bytes::<2>().map_or(0, u16::from_le_bytes)
    }
    pub fn u16be(&mut self) -> u16 {
        self.read_bytes::<2>().map_or(0, u16::from_be_bytes)
    }
    pub fn u32le(&mut self) -> u32 {
        self.read_bytes::<4>().map_or(0, u32::from_le_bytes)
    }
    pub fn u32be(&mut self) -> u32 {
        self.read_bytes::<4>().map_or(0, u32::from_be_bytes)
    }
}

/// Little-endian field readers over a fixed header slice.
pub fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
pub fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

/// A name from a fixed-size field (`mpt::String::ReadBuf`): stops at NUL,
/// trailing spaces trimmed when `space_padded`.
pub fn read_name(b: &[u8], space_padded: bool) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let mut s: String = b[..end].iter().map(|&c| if c < 0x20 { ' ' } else { c as char }).collect();
    if space_padded {
        while s.ends_with(' ') {
            s.pop();
        }
    }
    s
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channels {
    Mono,
    StereoSplit,
    StereoInterleaved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Signed,
    Unsigned,
    Delta,
    Adpcm,
    It214,
    It215,
}

/// `SampleIO`.
#[derive(Clone, Copy, Debug)]
pub struct SampleIo {
    pub bits: u8,
    pub channels: Channels,
    pub big_endian: bool,
    pub encoding: Encoding,
}

impl SampleIo {
    pub const fn new(bits: u8, channels: Channels, big_endian: bool, encoding: Encoding) -> Self {
        SampleIo { bits, channels, big_endian, encoding }
    }
    fn num_channels(&self) -> usize {
        if self.channels == Channels::Mono { 1 } else { 2 }
    }
    fn variable_length(&self) -> bool {
        matches!(self.encoding, Encoding::It214 | Encoding::It215)
    }
    /// `CalculateEncodedSize`.
    pub fn encoded_size(&self, length: SmpLength) -> usize {
        match self.encoding {
            Encoding::Adpcm => 16 + (length as usize).div_ceil(2),
            _ => length as usize * self.num_channels() * (self.bits as usize / 8),
        }
    }

    /// `ReadSample`: decodes into `sample` (allocating it) and returns the
    /// bytes consumed; the reader ends past them.
    pub fn read_sample(&self, sample: &mut ModSample, file: &mut Reader) -> usize {
        sample.n_length = sample.n_length.min(MAX_SAMPLE_LENGTH);
        let start = file.pos;
        let available = file.bytes_left();
        let file_size = if self.variable_length() { available } else { self.encoded_size(sample.n_length).min(available) };
        if !self.variable_length() && file_size < 1 {
            return 0;
        }
        if !self.variable_length() && sample.n_length > 0x40000 {
            let header = if self.encoding == Encoding::Adpcm { 16 } else { 0 };
            let mut max_len = file_size - header.min(file_size);
            if self.encoding == Encoding::Adpcm {
                max_len = max_len.saturating_mul(2);
            } else {
                let bps = self.num_channels() * self.bits as usize / 8;
                max_len = max_len.saturating_add(bps - 1) / bps;
            }
            sample.n_length = sample.n_length.min(max_len.min(u32::MAX as usize) as u32);
        } else if self.variable_length() {
            let max_len = file_size.saturating_mul(8 / self.num_channels());
            sample.n_length = sample.n_length.min(max_len.min(u32::MAX as usize) as u32);
        }
        if sample.n_length < 1 {
            return 0;
        }
        if self.bits >= 16 {
            sample.u_flags |= CHN_16BIT;
        } else {
            sample.u_flags &= !CHN_16BIT;
        }
        if self.channels != Channels::Mono {
            sample.u_flags |= CHN_STEREO;
        } else {
            sample.u_flags &= !CHN_STEREO;
        }
        let bytes = (PRE_FRAMES + sample.n_length as usize + POST_FRAMES) * self.num_channels() * if self.bits >= 16 { 2 } else { 1 };
        if !take_sample_budget(bytes) || !sample.allocate() {
            sample.n_length = 0;
            return 0;
        }
        let len = sample.n_length as usize;
        let src = &file.data[start..start + file_size];
        let nch = self.num_channels();
        let bytes_read = match (self.encoding, self.bits) {
            (Encoding::Adpcm, _) => {
                let mut table = [0i8; 16];
                if src.len() >= 16 {
                    for (t, &b) in table.iter_mut().zip(&src[..16]) {
                        *t = b as i8;
                    }
                    let read_len = len.div_ceil(2).min(available.saturating_sub(16));
                    let read_len = read_len.min(src.len() - 16);
                    let mut delta: i8 = 0;
                    if let Some(d) = sample.data.i8_mut() {
                        let mut o = PRE_FRAMES;
                        for &b in &src[16..16 + read_len] {
                            delta = delta.wrapping_add(table[(b & 0x0F) as usize]);
                            if o < PRE_FRAMES + len {
                                d[o] = delta;
                            }
                            o += 1;
                            delta = delta.wrapping_add(table[((b >> 4) & 0x0F) as usize]);
                            if o < PRE_FRAMES + len {
                                d[o] = delta;
                            }
                            o += 1;
                        }
                    }
                    16 + read_len
                } else {
                    0
                }
            }
            (Encoding::It214 | Encoding::It215, _) => {
                it_decompress(file, sample, self.encoding == Encoding::It215);
                file.pos - start
            }
            (enc, 8) => {
                let Some(d) = sample.data.i8_mut() else { unreachable!() };
                let conv = |enc: Encoding, state: &mut i8, b: u8| -> i8 {
                    match enc {
                        Encoding::Unsigned => (b as i32 - 128) as i8,
                        Encoding::Delta => {
                            *state = state.wrapping_add(b as i8);
                            *state
                        }
                        _ => b as i8,
                    }
                };
                decode_frames(d, src, len, nch, self.channels, 1, |st: &mut i8, s: &[u8]| conv(enc, st, s[0]))
            }
            (enc, _) => {
                let Some(d) = sample.data.i16_mut() else { unreachable!() };
                let be = self.big_endian;
                decode_frames(d, src, len, nch, self.channels, 2, |st: &mut i16, s: &[u8]| {
                    let raw = if be { u16::from_be_bytes([s[0], s[1]]) } else { u16::from_le_bytes([s[0], s[1]]) };
                    match enc {
                        Encoding::Unsigned => raw.wrapping_sub(0x8000) as i16,
                        Encoding::Delta => {
                            *st = st.wrapping_add(raw as i16);
                            *st
                        }
                        _ => raw as i16,
                    }
                })
            }
        };
        file.pos = start + bytes_read;
        bytes_read
    }
}

/// `CopyMonoSample` / `CopyStereoInterleavedSample` / `CopyStereoSplitSample`.
fn decode_frames<T: Copy + Default>(
    d: &mut [T],
    src: &[u8],
    len: usize,
    nch: usize,
    layout: Channels,
    inc: usize,
    mut conv: impl FnMut(&mut T, &[u8]) -> T,
) -> usize {
    let base = PRE_FRAMES * nch;
    match layout {
        Channels::Mono => {
            let n = (src.len() / inc).min(len);
            let mut st = T::default();
            for i in 0..n {
                d[base + i] = conv(&mut st, &src[i * inc..]);
            }
            n * inc
        }
        Channels::StereoInterleaved => {
            let n = (src.len() / (2 * inc)).min(len);
            let (mut sl, mut sr) = (T::default(), T::default());
            for i in 0..n {
                d[base + 2 * i] = conv(&mut sl, &src[2 * i * inc..]);
                d[base + 2 * i + 1] = conv(&mut sr, &src[(2 * i + 1) * inc..]);
            }
            n * 2 * inc
        }
        Channels::StereoSplit => {
            let size_left = (len * inc).min(src.len());
            let size_right = (len * inc).min(src.len() - size_left);
            let nl = size_left / inc;
            let nr = size_right / inc;
            let mut st = T::default();
            for i in 0..nl {
                d[base + 2 * i] = conv(&mut st, &src[i * inc..]);
            }
            let mut st = T::default();
            let off = len * inc;
            for i in 0..nr {
                d[base + 2 * i + 1] = conv(&mut st, &src[off + i * inc..]);
            }
            (nl + nr) * inc
        }
    }
}

/// LSB-first bit reader over one compressed block (`BitReader`).
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u32,
    bits: i32,
}

impl BitReader<'_> {
    fn read_bits(&mut self, n: i32) -> Option<u32> {
        while self.bits < n {
            let b = *self.data.get(self.pos)?;
            self.pos += 1;
            self.buf |= (b as u32) << self.bits;
            self.bits += 8;
        }
        let v = self.buf & ((1u32 << n) - 1);
        self.buf >>= n;
        self.bits -= n;
        Some(v)
    }
}

/// `ITDecompression`.
fn it_decompress(file: &mut Reader, sample: &mut ModSample, is215: bool) {
    let nch = sample.num_channels() as usize;
    let len = sample.n_length;
    let sixteen = sample.u_flags & CHN_16BIT != 0;
    for chn in 0..nch {
        let mut written: SmpLength = 0;
        let mut write_pos = PRE_FRAMES * nch + chn;
        while written < len && file.can_read(2) {
            let size = file.u16le() as usize;
            if size == 0 {
                continue;
            }
            let chunk = file.read_chunk(size);
            let mut bits = BitReader { data: chunk.data, pos: 0, buf: 0, bits: 0 };
            let mut mem1: u32 = 0;
            let mut mem2: u32 = 0;
            let (def_width, fetch_a, lower_b, upper_b, block) =
                if sixteen { (17i32, 4i32, -8i32, 7i32, 0x8000 / 2) } else { (9, 3, -4, 3, 0x8000) };
            let mut cur_length = (len - written).min(block as u32);
            let mut width = def_width;
            let mut write = |v: i32, top_bit: i32, sample: &mut ModSample, written: &mut SmpLength, write_pos: &mut usize, cur_length: &mut u32| {
                let mut v = v;
                if v & top_bit != 0 {
                    v -= top_bit << 1;
                }
                mem1 = mem1.wrapping_add(v as u32);
                mem2 = mem2.wrapping_add(mem1);
                let out = if is215 { mem2 } else { mem1 } as i32;
                if let Some(d) = sample.data.i16_mut() {
                    d[*write_pos] = out as i16;
                } else if let Some(d) = sample.data.i8_mut() {
                    d[*write_pos] = out as i8;
                }
                *written += 1;
                *write_pos += nch;
                *cur_length -= 1;
            };
            while cur_length > 0 {
                if width > def_width {
                    break;
                }
                let Some(v) = bits.read_bits(width) else { break };
                let v = v as i32;
                let top_bit = 1i32 << (width - 1);
                if width <= 6 {
                    if v == top_bit {
                        let Some(nw) = bits.read_bits(fetch_a) else { break };
                        let mut nw = nw as i32 + 1;
                        if nw >= width {
                            nw += 1;
                        }
                        width = nw;
                    } else {
                        write(v, top_bit, sample, &mut written, &mut write_pos, &mut cur_length);
                    }
                } else if width < def_width {
                    if v >= top_bit + lower_b && v <= top_bit + upper_b {
                        let mut nw = v - (top_bit + lower_b) + 1;
                        if nw >= width {
                            nw += 1;
                        }
                        width = nw;
                    } else {
                        write(v, top_bit, sample, &mut written, &mut write_pos, &mut cur_length);
                    }
                } else if v & top_bit != 0 {
                    width = (v & !top_bit) + 1;
                } else {
                    write(v & !top_bit, 0, sample, &mut written, &mut write_pos, &mut cur_length);
                }
            }
        }
    }
}
