// Shorten decoder.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/shorten.c, with
// libavcodec/bytestream.h's reader for the embedded WAVE/AIFF header,
// libavutil/mem.c's av_fast_realloc growth rule, and decode.c's calling
// pattern (one block per call, the rest of a packet dropped after an
// error, draining until a call returns no block).
// Copyright (c) 2005 Jeff Muizelaar (shorten.c); LGPL-2.1-or-later (see
// LICENSE).

use std::collections::VecDeque;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

use crate::bits::{BitReader, av_log2};

const MAX_CHANNELS: u32 = 8;
const MAX_BLOCKSIZE: u32 = 65535;
const OUT_BUFFER_SIZE: usize = 16384;
const ULONGSIZE: u32 = 2;
const DEFAULT_BLOCK_SIZE: u32 = 256;
const TYPESIZE: u32 = 4;
const CHANSIZE: u32 = 0;
const LPCQSIZE: u32 = 2;
const ENERGYSIZE: u32 = 3;
const BITSHIFTSIZE: u32 = 2;
const TYPE_U8: i32 = 2;
const TYPE_S16HL: i32 = 3;
const TYPE_S16LH: i32 = 5;
const NWRAP: usize = 3;
const NSKIPSIZE: u32 = 1;
const LPCQUANT: u32 = 5;
const V2LPCQOFFSET: i32 = 1 << LPCQUANT;
const FNSIZE: u32 = 2;
const FN_QUIT: u32 = 4;
const FN_BLOCKSIZE: u32 = 5;
const FN_BITSHIFT: u32 = 6;
const FN_QLPC: u32 = 7;
const FN_ZERO: u32 = 8;
const FN_VERBATIM: u32 = 9;
/// Whether each FN_* command carries audio.
const IS_AUDIO_COMMAND: [bool; 10] = [true, true, true, true, false, false, false, true, true, false];
const VERBATIM_CKSIZE_SIZE: u32 = 5;
const VERBATIM_BYTE_SIZE: u32 = 8;
const CANONICAL_HEADER_SIZE: u32 = 44;
/// `AV_INPUT_BUFFER_PADDING_SIZE`
const PADDING: usize = 64;
/// `AVERROR_INVALIDDATA`, which `get_uint` returns as an unsigned value
/// that the range checks after it then refuse.
const AVERROR_INVALIDDATA: u32 = -1_094_995_529i32 as u32;
/// decode.c stops draining after this many errors without a block.
const MAX_DRAIN_ERRORS: u32 = 21;

const FIXED_COEFFS: [[i32; 3]; 4] = [[0, 0, 0], [1, 0, 0], [2, -1, 0], [3, -3, 1]];

/// The stream header: what the decoder (and the demuxer, which reads it
/// the way FFmpeg's stream probing decodes it) learns before any block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamHeader {
    pub channels: u16,
    pub sample_rate: u32,
    pub sample_format: SampleFormat,
}

/// `av_fast_realloc`: grows `buf` to at least `min_size`, with FFmpeg's
/// headroom; new bytes are zero (as shorten.c clears them).
fn fast_realloc(buf: &mut Vec<u8>, min_size: usize) {
    if min_size <= buf.len() {
        return;
    }
    let size = (min_size + min_size / 16 + 32).max(min_size);
    buf.resize(size, 0);
}

/// bytestream.h's reader: reads past the end give zeros, skips stop at it.
struct Bytes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bytes<'_> {
    fn left(&self) -> usize {
        self.data.len() - self.pos
    }
    fn skip(&mut self, n: usize) {
        self.pos += n.min(self.left());
    }
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        if self.left() >= N {
            out.copy_from_slice(&self.data[self.pos..self.pos + N]);
            self.pos += N;
        } else {
            self.pos = self.data.len();
        }
        out
    }
    fn le32(&mut self) -> u32 {
        u32::from_le_bytes(self.take())
    }
    fn be32(&mut self) -> u32 {
        u32::from_be_bytes(self.take())
    }
    fn le16(&mut self) -> u16 {
        u16::from_le_bytes(self.take())
    }
    fn be16(&mut self) -> u16 {
        u16::from_be_bytes(self.take())
    }
    fn be64(&mut self) -> u64 {
        u64::from_be_bytes(self.take())
    }
}

const fn tag(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

/// What a call of `shorten_decode_frame` did.
enum Call {
    /// Bytes of the input taken, and the block it decoded if any.
    Done(usize, Option<Frame>),
    /// An error; a block decoded in the same call is lost with it.
    Failed(Error),
}

/// The `shorten` decoder state (`ShortenContext`).
struct State {
    // decode.c's view of the internal bitstream buffer.
    bitstream: Vec<u8>,
    bitstream_index: usize,
    bitstream_size: usize,
    max_framesize: usize,
    bitindex: usize,

    got_header: bool,
    got_quit_command: bool,
    version: u32,
    internal_ftype: i32,
    channels: u32,
    blocksize: u32,
    nmean: i32,
    nwrap: usize,
    bitshift: u32,
    lpcqoffset: i32,
    swap: bool,
    sample_rate: i32,
    has_extradata: bool,
    /// Per channel: `nwrap` samples of the previous block, then this one.
    decoded: Vec<Vec<i32>>,
    offset: Vec<Vec<i32>>,
    coeffs: Vec<i32>,
    /// The buffer size FFmpeg grows to right after the header
    /// (`av_fast_realloc` keeps the bytes, so growing after the call is the
    /// same).
    grow_to: Option<usize>,
    /// Samples per channel emitted so far: the next block's pts.
    next_pts: i64,
}

impl State {
    fn new(has_extradata: bool) -> Self {
        Self {
            bitstream: Vec::new(),
            bitstream_index: 0,
            bitstream_size: 0,
            max_framesize: 0,
            bitindex: 0,
            got_header: false,
            got_quit_command: false,
            version: 0,
            internal_ftype: 0,
            channels: 0,
            blocksize: DEFAULT_BLOCK_SIZE,
            nmean: -1,
            nwrap: NWRAP,
            bitshift: 0,
            lpcqoffset: 0,
            swap: false,
            sample_rate: 0,
            has_extradata,
            decoded: Vec::new(),
            offset: Vec::new(),
            coeffs: Vec::new(),
            grow_to: None,
            next_pts: 0,
        }
    }

    fn sample_format(&self) -> SampleFormat {
        if self.internal_ftype == TYPE_U8 { SampleFormat::U8P } else { SampleFormat::S16P }
    }

    /// `get_uint`: a version 0 stream codes the width, later ones send it.
    fn get_uint(&self, gb: &mut BitReader, k: u32) -> u32 {
        let mut k = k;
        if self.version != 0 {
            k = gb.ur(ULONGSIZE);
            if k > 31 {
                return AVERROR_INVALIDDATA;
            }
        }
        gb.ur(k)
    }

    /// `allocate_buffers` and `init_offset`.
    fn allocate(&mut self) -> Result<()> {
        let channels = self.channels as usize;
        let nmean = self.nmean.max(1) as usize;
        let mean = match self.internal_ftype {
            TYPE_U8 => 0x80,
            TYPE_S16HL | TYPE_S16LH => 0,
            _ => return Err(Error::unsupported("shorten: unknown audio type")),
        };
        self.offset = vec![vec![mean; nmean]; channels];
        self.decoded = vec![vec![0; self.blocksize as usize + self.nwrap]; channels];
        self.coeffs = vec![0; self.nwrap];
        Ok(())
    }

    /// `decode_wave_header`
    fn wave_header(&mut self, header: &[u8]) -> Result<()> {
        let mut gb = Bytes { data: header, pos: 0 };
        if gb.le32() != tag(b"RIFF") {
            return Err(Error::invalid("shorten: missing RIFF tag"));
        }
        gb.skip(4);
        if gb.le32() != tag(b"WAVE") {
            return Err(Error::invalid("shorten: missing WAVE tag"));
        }
        while gb.le32() != tag(b"fmt ") {
            let len = gb.le32() as i32;
            gb.skip(len as u32 as usize);
            if len < 0 || gb.left() < 16 {
                return Err(Error::invalid("shorten: no fmt chunk found"));
            }
        }
        let len = gb.le32() as i32;
        if len < 16 {
            return Err(Error::invalid("shorten: fmt chunk was too short"));
        }
        if gb.le16() != 1 {
            return Err(Error::unsupported("shorten: unsupported wave format"));
        }
        gb.skip(2);
        self.sample_rate = gb.le32() as i32;
        gb.skip(4);
        gb.skip(2);
        let bps = gb.le16();
        if bps != 16 && bps != 8 {
            return Err(Error::unsupported(format!("shorten: unsupported number of bits per sample: {bps}")));
        }
        Ok(())
    }

    /// `decode_aiff_header`
    fn aiff_header(&mut self, header: &[u8]) -> Result<()> {
        let mut gb = Bytes { data: header, pos: 0 };
        if gb.le32() != tag(b"FORM") {
            return Err(Error::invalid("shorten: missing FORM tag"));
        }
        gb.skip(4);
        let form = gb.le32();
        if form != tag(b"AIFF") && form != tag(b"AIFC") {
            return Err(Error::invalid("shorten: missing AIFF tag"));
        }
        while gb.le32() != tag(b"COMM") {
            let len = gb.be32() as i32;
            if len < 0 || (gb.left() as i64) < 18 + i64::from(len) + i64::from(len & 1) {
                return Err(Error::invalid("shorten: no COMM chunk found"));
            }
            gb.skip((len + (len & 1)) as usize);
        }
        let len = gb.be32() as i32;
        if len < 18 {
            return Err(Error::invalid("shorten: COMM chunk was too short"));
        }
        gb.skip(6);
        let bps = gb.be16();
        self.swap = form == tag(b"AIFC");
        if bps != 16 && bps != 8 {
            return Err(Error::unsupported(format!("shorten: unsupported number of bits per sample: {bps}")));
        }
        let exp = i32::from(gb.be16()) - 16383 - 63;
        let val = gb.be64();
        if !(-63..=63).contains(&exp) {
            return Err(Error::invalid(format!("shorten: exp {exp} is out of range")));
        }
        let rate = if exp >= 0 { val << exp } else { val.wrapping_add(1u64 << (-exp - 1)) >> -exp };
        self.sample_rate = rate as u32 as i32;
        Ok(())
    }

    /// `read_header`
    fn read_header(&mut self, gb: &mut BitReader) -> Result<()> {
        if gb.get_bits_long(32) != u32::from_be_bytes(*b"ajkg") {
            return Err(Error::invalid("shorten: missing shorten magic 'ajkg'"));
        }
        self.lpcqoffset = 0;
        self.blocksize = DEFAULT_BLOCK_SIZE;
        self.nmean = -1;
        self.version = gb.get_bits(8);
        self.internal_ftype = self.get_uint(gb, TYPESIZE) as i32;
        self.channels = self.get_uint(gb, CHANSIZE);
        if self.channels == 0 {
            return Err(Error::invalid("shorten: no channels reported"));
        }
        if self.channels > MAX_CHANNELS {
            self.channels = 0;
            return Err(Error::invalid("shorten: too many channels"));
        }
        let mut maxnlpc = 0u32;
        if self.version > 0 {
            let blocksize = self.get_uint(gb, av_log2(DEFAULT_BLOCK_SIZE) as u32);
            if blocksize == 0 || blocksize > MAX_BLOCKSIZE {
                return Err(Error::invalid("shorten: invalid or unsupported block size"));
            }
            self.blocksize = blocksize;
            maxnlpc = self.get_uint(gb, LPCQSIZE);
            if maxnlpc > 1024 {
                return Err(Error::invalid("shorten: maxnlpc too large"));
            }
            let nmean = self.get_uint(gb, 0);
            self.nmean = nmean as i32;
            if nmean > 32768 {
                return Err(Error::invalid("shorten: nmean too large"));
            }
            let skip_bytes = self.get_uint(gb, NSKIPSIZE);
            if skip_bytes as usize > gb.left().max(0) as usize / 8 {
                return Err(Error::invalid("shorten: invalid skip_bytes"));
            }
            gb.skip_bits(8 * skip_bytes as usize);
        }
        self.nwrap = NWRAP.max(maxnlpc as usize);
        if self.version > 1 {
            self.lpcqoffset = V2LPCQOFFSET;
        }

        if !self.has_extradata {
            if gb.ur(FNSIZE) != FN_VERBATIM {
                return Err(Error::invalid("shorten: missing verbatim section at beginning of stream"));
            }
            let header_size = gb.ur(VERBATIM_CKSIZE_SIZE) as i32;
            if header_size >= OUT_BUFFER_SIZE as i32 || header_size < CANONICAL_HEADER_SIZE as i32 {
                return Err(Error::invalid(format!("shorten: header is wrong size: {header_size}")));
            }
            let header: Vec<u8> = (0..header_size).map(|_| gb.ur(VERBATIM_BYTE_SIZE) as u8).collect();
            match u32::from_le_bytes([header[0], header[1], header[2], header[3]]) {
                t if t == tag(b"RIFF") => self.wave_header(&header)?,
                t if t == tag(b"FORM") => self.aiff_header(&header)?,
                _ => return Err(Error::unsupported("shorten: unsupported bit packing")),
            }
        }

        self.allocate()?;
        self.bitshift = 0;
        self.got_header = true;
        Ok(())
    }

    /// `decode_subframe_lpc`: `decoded[channel]` from index `nwrap` on.
    fn decode_subframe_lpc(&mut self, gb: &mut BitReader, command: u32, channel: usize, residual_size: u32, coffset: i32) -> Result<()> {
        let nwrap = self.nwrap;
        // `coeffs` has `nwrap` (at least 3) entries.
        let (pred_order, qshift) = if command == FN_QLPC {
            let pred_order = gb.ur(LPCQSIZE) as usize;
            if pred_order > nwrap {
                return Err(Error::invalid(format!("shorten: invalid pred_order {pred_order}")));
            }
            for coeff in &mut self.coeffs[..pred_order] {
                *coeff = gb.sr(LPCQUANT);
            }
            (pred_order, LPCQUANT)
        } else {
            let pred_order = command as usize;
            let Some(fixed) = FIXED_COEFFS.get(pred_order) else {
                return Err(Error::invalid(format!("shorten: invalid pred_order {pred_order}")));
            };
            self.coeffs[..3].copy_from_slice(fixed);
            (pred_order, 0)
        };
        let coeffs = &self.coeffs[..pred_order];
        let d = &mut self.decoded[channel];
        if command == FN_QLPC && coffset != 0 {
            for v in &mut d[nwrap - pred_order..nwrap] {
                *v = v.wrapping_sub(coffset);
            }
        }
        let init_sum = if pred_order > 0 {
            if command == FN_QLPC { self.lpcqoffset } else { 0 }
        } else {
            coffset
        };
        for i in nwrap..nwrap + self.blocksize as usize {
            let mut sum = init_sum;
            for (j, &c) in coeffs.iter().enumerate() {
                sum = sum.wrapping_add(c.wrapping_mul(d[i - j - 1]));
            }
            d[i] = gb.sr(residual_size).wrapping_add(sum >> qshift);
        }
        if command == FN_QLPC && coffset != 0 {
            for v in &mut d[nwrap..nwrap + self.blocksize as usize] {
                *v = v.wrapping_add(coffset);
            }
        }
        Ok(())
    }

    /// One audio command for the current channel; the block's frame when
    /// it was the last channel.
    fn audio_command(&mut self, gb: &mut BitReader, cmd: u32, cur_chan: usize) -> Result<()> {
        let mut residual_size = 0u32;
        if cmd != FN_ZERO {
            residual_size = gb.ur(ENERGYSIZE);
            // Version 0 coded the signed Rice codes one bit shorter.
            if self.version == 0 {
                residual_size = residual_size.wrapping_sub(1);
            }
            if residual_size > 30 {
                return Err(Error::invalid(format!("shorten: residual size unsupported: {}", residual_size as i32)));
            }
        }
        let channel = cur_chan;
        let coffset = if self.nmean == 0 {
            self.offset[channel][0]
        } else {
            let mut sum: i32 = if self.version < 2 { 0 } else { self.nmean / 2 };
            for i in 0..self.nmean.max(0) as usize {
                sum = sum.wrapping_add(self.offset[channel][i]);
            }
            let mut c = sum.wrapping_div(self.nmean);
            if self.version >= 2 && self.bitshift != 0 {
                c = c >> (self.bitshift - 1) >> 1;
            }
            c
        };

        let nwrap = self.nwrap;
        let blocksize = self.blocksize as usize;
        if cmd == FN_ZERO {
            self.decoded[channel][nwrap..nwrap + blocksize].fill(0);
        } else {
            self.decode_subframe_lpc(gb, cmd, channel, residual_size, coffset)?;
        }

        if self.nmean > 0 {
            let nmean = self.nmean as usize;
            let mut sum: i64 = if self.version < 2 { 0 } else { (blocksize / 2) as i64 };
            for &v in &self.decoded[channel][nwrap..nwrap + blocksize] {
                sum += i64::from(v);
            }
            let offset = &mut self.offset[channel];
            offset.copy_within(1..nmean, 0);
            offset[nmean - 1] = if self.version < 2 {
                (sum / blocksize as i64) as i32
            } else if self.bitshift == 32 {
                0
            } else {
                (sum / blocksize as i64).wrapping_mul(1i64 << self.bitshift) as i32
            };
        }

        // The wrap samples for the next block, before the bit shift.
        let d = &mut self.decoded[channel];
        for i in 0..nwrap {
            d[i] = d[i + blocksize];
        }
        // fix_bitshift
        if self.bitshift == 32 {
            d[nwrap..nwrap + blocksize].fill(0);
        } else if self.bitshift != 0 {
            for v in &mut d[nwrap..nwrap + blocksize] {
                *v = (*v as u32).wrapping_mul(1u32 << self.bitshift) as i32;
            }
        }
        Ok(())
    }

    /// The block's planes, clipped to the output format (`av_clip_uint8`,
    /// `av_clip_int16`, AIFC's byte swap).
    fn block_frame(&mut self) -> Frame {
        let nwrap = self.nwrap;
        let blocksize = self.blocksize as usize;
        let planes = self.decoded[..self.channels as usize]
            .iter()
            .map(|d| {
                let samples = &d[nwrap..nwrap + blocksize];
                if self.internal_ftype == TYPE_U8 {
                    samples.iter().map(|&v| v.clamp(0, 255) as u8).collect()
                } else {
                    samples
                        .iter()
                        .flat_map(|&v| {
                            let s = v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
                            if self.swap { s.swap_bytes().to_le_bytes() } else { s.to_le_bytes() }
                        })
                        .collect()
                }
            })
            .collect();
        let pts = self.next_pts;
        self.next_pts += blocksize as i64;
        Frame::Audio(AudioFrame { samples: blocksize as u32, pts: Some(pts), data: planes })
    }

    /// `shorten_decode_frame`: `input` is a packet's remaining bytes, or
    /// `None` when draining.
    fn call(&mut self, input: Option<&[u8]>) -> Call {
        if self.max_framesize == 0 {
            self.max_framesize = 8192;
            fast_realloc(&mut self.bitstream, self.max_framesize + PADDING);
        }
        let pkt_size = input.map_or(0, <[u8]>::len);
        let buf_size = pkt_size.min(self.max_framesize.saturating_sub(self.bitstream_size));
        let input_buf_size = buf_size;
        if self.bitstream_index + self.bitstream_size + buf_size + PADDING > self.bitstream.len() {
            let (index, size) = (self.bitstream_index, self.bitstream_size);
            self.bitstream.copy_within(index..index + size, 0);
            self.bitstream_index = 0;
        }
        if let Some(data) = input {
            let at = self.bitstream_index + self.bitstream_size;
            self.bitstream[at..at + buf_size].copy_from_slice(&data[..buf_size]);
        }
        let buf_size = buf_size + self.bitstream_size;
        self.bitstream_size = buf_size;

        // Wait for a whole frame's worth of bytes, unless at the end.
        if buf_size < self.max_framesize && input.is_some() {
            return Call::Done(input_buf_size, None);
        }
        let bitstream = std::mem::take(&mut self.bitstream);
        let result = self.decode_buffered(&bitstream, buf_size, pkt_size, input_buf_size);
        self.bitstream = bitstream;
        if let Some(size) = self.grow_to.take() {
            fast_realloc(&mut self.bitstream, size);
        }
        result
    }

    /// The part of `shorten_decode_frame` that reads the buffered bytes.
    fn decode_buffered(&mut self, bitstream: &[u8], buf_size: usize, pkt_size: usize, input_buf_size: usize) -> Call {
        let mut gb = BitReader::new(&bitstream[self.bitstream_index..], buf_size);
        gb.skip_bits(self.bitindex);
        let mut frame = None;

        let mut decode_blocks = true;
        if !self.got_header {
            if let Err(e) = self.read_header(&mut gb) {
                return Call::Failed(e);
            }
            if pkt_size > 0 {
                let max_framesize = self.blocksize as usize * self.channels as usize * 8;
                self.grow_to = Some(max_framesize + PADDING);
                self.max_framesize = self.max_framesize.max(max_framesize);
                decode_blocks = false;
            }
        }
        if decode_blocks {
            if self.got_quit_command {
                return Call::Done(pkt_size, None);
            }
            let mut cur_chan = 0usize;
            while cur_chan < self.channels as usize {
                if gb.left() < 3 + FNSIZE as isize {
                    break;
                }
                let cmd = gb.ur(FNSIZE);
                if cmd > FN_VERBATIM {
                    break;
                }
                if !IS_AUDIO_COMMAND[cmd as usize] {
                    match cmd {
                        FN_VERBATIM => {
                            let len = gb.ur(VERBATIM_CKSIZE_SIZE) as i32;
                            if len < 0 || len as isize > gb.left() {
                                return Call::Failed(Error::invalid(format!("shorten: verbatim length {len} invalid")));
                            }
                            for _ in 0..len {
                                gb.ur(VERBATIM_BYTE_SIZE);
                            }
                        }
                        FN_BITSHIFT => {
                            let bitshift = gb.ur(BITSHIFTSIZE);
                            if bitshift > 32 {
                                return Call::Failed(Error::invalid("shorten: bitshift is invalid"));
                            }
                            self.bitshift = bitshift;
                        }
                        FN_BLOCKSIZE => {
                            let blocksize = self.get_uint(&mut gb, av_log2(self.blocksize) as u32);
                            if blocksize > self.blocksize {
                                return Call::Failed(Error::unsupported("shorten: increasing block size"));
                            }
                            if blocksize == 0 || blocksize > MAX_BLOCKSIZE {
                                return Call::Failed(Error::invalid("shorten: invalid or unsupported block size"));
                            }
                            self.blocksize = blocksize;
                        }
                        FN_QUIT => {
                            self.got_quit_command = true;
                            break;
                        }
                        // Every other command carries audio.
                        _ => {}
                    }
                } else {
                    if let Err(e) = self.audio_command(&mut gb, cmd, cur_chan) {
                        return Call::Failed(e);
                    }
                    cur_chan += 1;
                    if cur_chan == self.channels as usize {
                        frame = Some(self.block_frame());
                    }
                }
            }
        }

        // finish_frame
        let count = gb.count();
        self.bitindex = count % 8;
        let i = count / 8;
        if i > buf_size {
            self.bitstream_size = 0;
            self.bitstream_index = 0;
            if frame.is_some() {
                self.next_pts -= i64::from(self.blocksize);
            }
            return Call::Failed(Error::invalid(format!("shorten: overread: {}", i - buf_size)));
        }
        if self.bitstream_size != 0 {
            self.bitstream_index += i;
            self.bitstream_size -= i;
            Call::Done(input_buf_size, frame)
        } else {
            Call::Done(i, frame)
        }
    }
}

/// The stream header at the start of `data` (a Shorten stream's first
/// bytes), as the decoder reads it before its first block.
pub fn parse_stream_header(data: &[u8]) -> Result<StreamHeader> {
    let mut state = State::new(false);
    let mut gb = BitReader::new(data, data.len());
    state.read_header(&mut gb)?;
    Ok(StreamHeader {
        channels: state.channels as u16,
        sample_rate: state.sample_rate.max(0) as u32,
        sample_format: state.sample_format(),
    })
}

/// The `shorten` decoder. FFmpeg's decoder buffers the stream itself and
/// decodes a block once it holds `max_framesize` bytes, so a packet gives
/// any number of blocks and the last ones come out on `flush`. Blocks carry
/// their pts in samples from the start.
pub struct ShortenDecoder {
    codec_id: CodecId,
    has_extradata: bool,
    /// The layout the container reports (the demuxer reads it from the
    /// stream header), until the decoder has read the header itself; the
    /// rate also for a stream whose header the container carries.
    params_format: Option<AudioFormat>,
    state: State,
    drained: bool,
    ready: VecDeque<Frame>,
}

impl ShortenDecoder {
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let has_extradata = !params.extradata.is_empty();
        let params_format = match (params.sample_format, params.sample_rate, params.channels) {
            (Some(sample_format), Some(sample_rate), Some(channels)) => {
                Some(AudioFormat { sample_format, sample_rate, channels })
            }
            _ => None,
        };
        Ok(Self {
            codec_id: params.codec_id.clone(),
            has_extradata,
            params_format,
            state: State::new(has_extradata),
            drained: false,
            ready: VecDeque::new(),
        })
    }
}

impl Decoder for ShortenDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        let s = &self.state;
        if !s.got_header {
            return self.params_format;
        }
        let sample_rate = if self.has_extradata {
            self.params_format.map_or(0, |f| f.sample_rate)
        } else {
            s.sample_rate.max(0) as u32
        };
        Some(AudioFormat { sample_format: s.sample_format(), sample_rate, channels: s.channels as u16 })
    }

    /// decode.c's loop: calls until the packet is taken, a block per call.
    /// After an error the rest of the packet is dropped; the blocks decoded
    /// before it stay queued.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        self.drained = false;
        let mut data: &[u8] = &packet.data;
        loop {
            let before = (self.state.bitstream_index, self.state.bitstream_size, self.state.bitindex);
            match self.state.call(Some(data)) {
                Call::Done(consumed, frame) => {
                    let decoded = frame.is_some();
                    if let Some(f) = frame {
                        self.ready.push_back(f);
                    }
                    if consumed >= data.len() {
                        return Ok(());
                    }
                    // A call that neither takes input nor moves through
                    // the buffer would repeat forever.
                    let after = (self.state.bitstream_index, self.state.bitstream_size, self.state.bitindex);
                    if consumed == 0 && !decoded && before == after {
                        return Ok(());
                    }
                    data = &data[consumed..];
                }
                Call::Failed(e) => return Err(e),
            }
        }
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.ready.pop_front().ok_or(Error::NeedMore)
    }

    /// Draining: calls without input until one decodes no block. A call
    /// that fails loses its block and draining goes on, as decode.c does
    /// (giving up after its error limit).
    fn flush(&mut self) -> Result<()> {
        if self.drained {
            return Ok(());
        }
        self.drained = true;
        let mut errors = 0;
        loop {
            match self.state.call(None) {
                Call::Done(_, Some(f)) => self.ready.push_back(f),
                Call::Done(_, None) => return Ok(()),
                Call::Failed(_) => {
                    errors += 1;
                    if errors >= MAX_DRAIN_ERRORS {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Back to the start of a stream: the demuxer only rewinds to the first
    /// byte, where the stream header is.
    fn reset(&mut self) -> Result<()> {
        self.state = State::new(self.has_extradata);
        self.drained = false;
        self.ready.clear();
        Ok(())
    }
}
