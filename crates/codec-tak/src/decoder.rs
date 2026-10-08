// TAK decoder.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/takdec.c, takdsp.c (the C
// decorrelation functions) and audiodsp.c (scalarproduct_int16_c), in
// FFmpeg's wrapping integer arithmetic.
// Copyright (c) 2012 Paul B Mahol (takdec.c), (c) 2015 Paul B Mahol
// (takdsp.c); LGPL-2.1-or-later (see LICENSE).

use std::collections::VecDeque;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

use crate::bits::Bits;
use crate::tak::{
    StreamInfo, TAK_CODEC_MONO_STEREO, TAK_CODEC_MULTICHANNEL, TAK_MAX_CHANNELS, TAK_MIN_FRAME_HEADER_BYTES,
    decode_frame_header, parse_streaminfo_block,
};

const MAX_SUBFRAMES: usize = 8;
const MAX_PREDICTORS: usize = 256;
const RESIDUES: usize = 544;
const MC_DMODES: [i32; 4] = [1, 3, 4, 6];
const PREDICTOR_SIZES: [usize; 16] = [4, 8, 12, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 224, 256, 0];

/// `struct CParam`: init, escape, scale, aescape, bias.
#[derive(Clone, Copy)]
struct CParam {
    init: u32,
    escape: u32,
    scale: u32,
    aescape: u32,
    bias: u32,
}

const fn cp(init: u32, escape: u32, scale: u32, aescape: u32, bias: u32) -> CParam {
    CParam { init, escape, scale, aescape, bias }
}

/// `xcodes`
const XCODES: [CParam; 50] = [
    cp(0x01, 0x0000001, 0x0000001, 0x0000003, 0x0000008),
    cp(0x02, 0x0000003, 0x0000001, 0x0000007, 0x0000006),
    cp(0x03, 0x0000005, 0x0000002, 0x000000E, 0x000000D),
    cp(0x03, 0x0000003, 0x0000003, 0x000000D, 0x0000018),
    cp(0x04, 0x000000B, 0x0000004, 0x000001C, 0x0000019),
    cp(0x04, 0x0000006, 0x0000006, 0x000001A, 0x0000030),
    cp(0x05, 0x0000016, 0x0000008, 0x0000038, 0x0000032),
    cp(0x05, 0x000000C, 0x000000C, 0x0000034, 0x0000060),
    cp(0x06, 0x000002C, 0x0000010, 0x0000070, 0x0000064),
    cp(0x06, 0x0000018, 0x0000018, 0x0000068, 0x00000C0),
    cp(0x07, 0x0000058, 0x0000020, 0x00000E0, 0x00000C8),
    cp(0x07, 0x0000030, 0x0000030, 0x00000D0, 0x0000180),
    cp(0x08, 0x00000B0, 0x0000040, 0x00001C0, 0x0000190),
    cp(0x08, 0x0000060, 0x0000060, 0x00001A0, 0x0000300),
    cp(0x09, 0x0000160, 0x0000080, 0x0000380, 0x0000320),
    cp(0x09, 0x00000C0, 0x00000C0, 0x0000340, 0x0000600),
    cp(0x0A, 0x00002C0, 0x0000100, 0x0000700, 0x0000640),
    cp(0x0A, 0x0000180, 0x0000180, 0x0000680, 0x0000C00),
    cp(0x0B, 0x0000580, 0x0000200, 0x0000E00, 0x0000C80),
    cp(0x0B, 0x0000300, 0x0000300, 0x0000D00, 0x0001800),
    cp(0x0C, 0x0000B00, 0x0000400, 0x0001C00, 0x0001900),
    cp(0x0C, 0x0000600, 0x0000600, 0x0001A00, 0x0003000),
    cp(0x0D, 0x0001600, 0x0000800, 0x0003800, 0x0003200),
    cp(0x0D, 0x0000C00, 0x0000C00, 0x0003400, 0x0006000),
    cp(0x0E, 0x0002C00, 0x0001000, 0x0007000, 0x0006400),
    cp(0x0E, 0x0001800, 0x0001800, 0x0006800, 0x000C000),
    cp(0x0F, 0x0005800, 0x0002000, 0x000E000, 0x000C800),
    cp(0x0F, 0x0003000, 0x0003000, 0x000D000, 0x0018000),
    cp(0x10, 0x000B000, 0x0004000, 0x001C000, 0x0019000),
    cp(0x10, 0x0006000, 0x0006000, 0x001A000, 0x0030000),
    cp(0x11, 0x0016000, 0x0008000, 0x0038000, 0x0032000),
    cp(0x11, 0x000C000, 0x000C000, 0x0034000, 0x0060000),
    cp(0x12, 0x002C000, 0x0010000, 0x0070000, 0x0064000),
    cp(0x12, 0x0018000, 0x0018000, 0x0068000, 0x00C0000),
    cp(0x13, 0x0058000, 0x0020000, 0x00E0000, 0x00C8000),
    cp(0x13, 0x0030000, 0x0030000, 0x00D0000, 0x0180000),
    cp(0x14, 0x00B0000, 0x0040000, 0x01C0000, 0x0190000),
    cp(0x14, 0x0060000, 0x0060000, 0x01A0000, 0x0300000),
    cp(0x15, 0x0160000, 0x0080000, 0x0380000, 0x0320000),
    cp(0x15, 0x00C0000, 0x00C0000, 0x0340000, 0x0600000),
    cp(0x16, 0x02C0000, 0x0100000, 0x0700000, 0x0640000),
    cp(0x16, 0x0180000, 0x0180000, 0x0680000, 0x0C00000),
    cp(0x17, 0x0580000, 0x0200000, 0x0E00000, 0x0C80000),
    cp(0x17, 0x0300000, 0x0300000, 0x0D00000, 0x1800000),
    cp(0x18, 0x0B00000, 0x0400000, 0x1C00000, 0x1900000),
    cp(0x18, 0x0600000, 0x0600000, 0x1A00000, 0x3000000),
    cp(0x19, 0x1600000, 0x0800000, 0x3800000, 0x3200000),
    cp(0x19, 0x0C00000, 0x0C00000, 0x3400000, 0x6000000),
    cp(0x1A, 0x2C00000, 0x1000000, 0x7000000, 0x6400000),
    cp(0x1A, 0x1800000, 0x1800000, 0x6800000, 0xC000000),
];

fn invalid(what: &str) -> Error {
    Error::invalid(format!("tak: {what}"))
}

/// `av_clip_intp2(a, 13)`
#[inline]
fn clip13(a: i32) -> i32 {
    a.clamp(-(1 << 13), (1 << 13) - 1)
}

/// `scalarproduct_int16_c`
#[inline]
fn scalarproduct(v1: &[i16], v2: &[i16]) -> i32 {
    v1.iter().zip(v2).fold(0u32, |res, (&a, &b)| res.wrapping_add((i32::from(a) * i32::from(b)) as u32)) as i32
}

/// `MCDParam`
#[derive(Clone, Copy, Default)]
struct McdParam {
    present: bool,
    index: usize,
    chan1: usize,
    chan2: usize,
}

/// `decode_lpc`: in place over `c`.
fn decode_lpc(c: &mut [i32], mode: i32, length: usize) {
    if length < 2 {
        return;
    }
    let add = |a: i32, b: u32| (a as u32).wrapping_add(b) as i32;
    match mode {
        1 => {
            let mut a1 = c[0] as u32;
            let mut k = 1;
            for _ in 0..(length - 1) >> 1 {
                c[k] = add(c[k], a1);
                c[k + 1] = add(c[k + 1], c[k] as u32);
                a1 = c[k + 1] as u32;
                k += 2;
            }
            if (length - 1) & 1 != 0 {
                c[k] = add(c[k], a1);
            }
        }
        2 => {
            let mut a1 = c[1] as u32;
            let mut a2 = a1.wrapping_add(c[0] as u32);
            c[1] = a2 as i32;
            if length > 2 {
                let mut k = 2;
                for _ in 0..(length - 2) >> 1 {
                    let a3 = (c[k] as u32).wrapping_add(a1);
                    let a4 = a3.wrapping_add(a2);
                    c[k] = a4 as i32;
                    a1 = (c[k + 1] as u32).wrapping_add(a3);
                    a2 = a1.wrapping_add(a4);
                    c[k + 1] = a2 as i32;
                    k += 2;
                }
                if length & 1 != 0 {
                    c[k] = add(c[k], a1.wrapping_add(a2));
                }
            }
        }
        3 => {
            let a1 = c[1] as u32;
            let a2 = a1.wrapping_add(c[0] as u32);
            c[1] = a2 as i32;
            if length > 2 {
                let mut a3 = c[2] as u32;
                let mut a4 = a3.wrapping_add(a1);
                let mut a5 = a4.wrapping_add(a2);
                c[2] = a5 as i32;
                for v in &mut c[3..length] {
                    a3 = a3.wrapping_add(*v as u32);
                    a4 = a4.wrapping_add(a3);
                    a5 = a5.wrapping_add(a4);
                    *v = a5 as i32;
                }
            }
        }
        _ => {}
    }
}

/// The `tak` decoder (`TAKDecContext`). One packet is one frame.
pub struct TakDecoder {
    codec_id: CodecId,
    ti: StreamInfo,
    bps: u32,
    sample_rate: i32,
    uval: i32,
    subframe_scale: i32,
    nb_samples: usize,
    /// Per channel; channels a frame does not code keep what they held,
    /// as FFmpeg's reused buffer does.
    decoded: Vec<Vec<i32>>,
    lpc_mode: [i32; TAK_MAX_CHANNELS],
    sample_shift: [u32; TAK_MAX_CHANNELS],
    predictors: [i16; MAX_PREDICTORS],
    subframe_len: [i16; MAX_SUBFRAMES],
    dmode: i32,
    mcdparams: [McdParam; TAK_MAX_CHANNELS],
    coding_mode: [i8; 128],
    filter: [i16; MAX_PREDICTORS],
    residues: [i16; RESIDUES],
    ready: VecDeque<Frame>,
}

impl TakDecoder {
    /// `tak_decode_init`: the bits per sample and rate of the stream's
    /// STREAMINFO block (the extradata), as FFmpeg takes them from the
    /// container.
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let info = parse_streaminfo_block(&params.extradata);
        let bps = info.map_or(0, |i| i.bps);
        let sample_rate = info.map_or(params.sample_rate.unwrap_or(0) as i32, |i| i.sample_rate);
        if !matches!(bps, 8 | 16 | 24) {
            return Err(invalid(&format!("invalid/unsupported bits per sample: {bps}")));
        }
        let mut d = Self {
            codec_id: params.codec_id.clone(),
            ti: StreamInfo::default(),
            bps,
            sample_rate,
            uval: 0,
            subframe_scale: 0,
            nb_samples: 0,
            decoded: vec![Vec::new(); TAK_MAX_CHANNELS],
            lpc_mode: [0; TAK_MAX_CHANNELS],
            sample_shift: [0; TAK_MAX_CHANNELS],
            predictors: [0; MAX_PREDICTORS],
            subframe_len: [0; MAX_SUBFRAMES],
            dmode: 0,
            mcdparams: [McdParam::default(); TAK_MAX_CHANNELS],
            coding_mode: [0; 128],
            filter: [0; MAX_PREDICTORS],
            residues: [0; RESIDUES],
            ready: VecDeque::new(),
        };
        d.set_sample_rate_params();
        // The output layout the container declares, until a frame says.
        if let Some(i) = info {
            d.ti.channels = i.channels;
            d.ti.ch_mask = i.ch_mask;
        }
        Ok(d)
    }

    /// `set_sample_rate_params`
    fn set_sample_rate_params(&mut self) {
        let rate = i64::from(self.sample_rate);
        let shift = if rate < 11025 {
            3
        } else if rate < 22050 {
            2
        } else if rate < 44100 {
            1
        } else {
            0
        };
        let aligned = (((rate + 511) >> 9) + 3) & !3;
        self.uval = (aligned << shift) as i32;
        self.subframe_scale = (aligned << 1) as i32;
    }

    /// The channels FFmpeg outputs: the mask's count when the stream gives
    /// a layout, else its channel count.
    fn channels(&self) -> usize {
        if self.ti.ch_mask != 0 { self.ti.ch_mask.count_ones() as usize } else { self.ti.channels }
    }

    fn sample_format(&self) -> SampleFormat {
        match self.bps {
            8 => SampleFormat::U8P,
            16 => SampleFormat::S16P,
            _ => SampleFormat::S32P,
        }
    }

    /// `get_bits_esc4`
    fn esc4(gb: &mut Bits) -> u32 {
        if gb.bit() != 0 { gb.get(4) + 1 } else { 0 }
    }

    /// `decode_segment`
    fn decode_segment(gb: &mut Bits, mode: i8, out: &mut [i32]) -> Result<()> {
        if mode == 0 {
            out.fill(0);
            return Ok(());
        }
        if mode < 0 || mode as usize > XCODES.len() {
            return Err(invalid("invalid coding mode"));
        }
        let code = XCODES[mode as usize - 1];
        for d in out.iter_mut() {
            let mut x = gb.get_long(code.init);
            if x >= code.escape && gb.bit() != 0 {
                x |= 1 << code.init;
                if x >= code.aescape {
                    let mut scale = gb.unary1(9);
                    if scale == 9 {
                        let mut scale_bits = gb.get(3);
                        if scale_bits > 0 {
                            if scale_bits == 7 {
                                scale_bits += gb.get(5);
                                if scale_bits > 29 {
                                    return Err(invalid("invalid scale"));
                                }
                            }
                            scale = gb.get_long(scale_bits).wrapping_add(1);
                            x = x.wrapping_add(code.scale.wrapping_mul(scale));
                        }
                        x = x.wrapping_add(code.bias);
                    } else {
                        x = x.wrapping_add(code.scale.wrapping_mul(scale)).wrapping_sub(code.escape);
                    }
                } else {
                    x = x.wrapping_sub(code.escape);
                }
            }
            *d = ((x >> 1) ^ (x & 1).wrapping_neg()) as i32;
        }
        Ok(())
    }

    /// `decode_residues`: `length` residues into `out[..length]`.
    fn decode_residues(&mut self, gb: &mut Bits, out: &mut [i32], length: usize) -> Result<()> {
        // The rate sets `uval`; a frame's header always gives one first.
        if length > self.nb_samples || length > out.len() || self.uval <= 0 {
            return Err(invalid("residues longer than the frame"));
        }
        if gb.bit() != 0 {
            let uval = self.uval as usize;
            let mut wlength = length / uval;
            let mut rval = length - wlength * uval;
            if rval < uval / 2 {
                rval += uval;
            } else {
                wlength += 1;
            }
            if wlength <= 1 || wlength > 128 {
                return Err(invalid("invalid residue windows"));
            }
            let mut mode = gb.get(6) as i32;
            self.coding_mode[0] = mode as i8;
            for i in 1..wlength {
                let c = gb.unary1(6);
                match c {
                    6 => mode = gb.get(6) as i32,
                    3..=5 => {
                        let sign = gb.bit() as i32;
                        mode = mode.wrapping_add((-sign ^ (c as i32 - 1)) + sign);
                    }
                    2 => mode = mode.wrapping_add(1),
                    1 => mode = mode.wrapping_sub(1),
                    _ => {}
                }
                self.coding_mode[i] = mode as i8;
            }
            let mut i = 0;
            let mut at = 0;
            while i < wlength {
                let mut len = 0;
                let mode = self.coding_mode[i];
                loop {
                    len += if i >= wlength - 1 { rval } else { uval };
                    i += 1;
                    if i == wlength || self.coding_mode[i] != mode {
                        break;
                    }
                }
                let Some(seg) = out.get_mut(at..at + len) else {
                    return Err(invalid("residue window past the frame"));
                };
                Self::decode_segment(gb, mode, seg)?;
                at += len;
            }
        } else {
            let mode = gb.get(6) as i8;
            Self::decode_segment(gb, mode, &mut out[..length])?;
        }
        Ok(())
    }

    /// `decode_subframe`: `buf[start..]` is the subframe in its channel.
    fn decode_subframe(&mut self, gb: &mut Bits, buf: &mut [i32], start: usize, subframe_size: usize, prev_subframe_size: usize) -> Result<()> {
        if gb.bit() == 0 {
            return self.decode_residues(gb, &mut buf[start..], subframe_size);
        }
        let filter_order = PREDICTOR_SIZES[gb.get(4) as usize];
        let mut start = start;
        let mut subframe_size = subframe_size;
        if prev_subframe_size > 0 && gb.bit() != 0 {
            if filter_order > prev_subframe_size {
                return Err(invalid("filter order past the previous subframe"));
            }
            start -= filter_order;
            subframe_size += filter_order;
            if filter_order > subframe_size {
                return Err(invalid("filter order past the subframe"));
            }
        } else {
            if filter_order > subframe_size {
                return Err(invalid("filter order past the subframe"));
            }
            let lpc_mode = gb.get(2) as i32;
            if lpc_mode > 2 {
                return Err(invalid("invalid lpc mode"));
            }
            self.decode_residues(gb, &mut buf[start..], filter_order)?;
            if lpc_mode != 0 {
                decode_lpc(&mut buf[start..start + filter_order], lpc_mode, filter_order);
            }
        }

        let dshift = Self::esc4(gb);
        let size = gb.bit() + 6;
        let mut filter_quant: i32 = 10;
        if gb.bit() != 0 {
            filter_quant -= gb.get(3) as i32 + 1;
            if filter_quant < 3 {
                return Err(invalid("invalid filter quantization"));
            }
        }
        if gb.left() < (2 * 10 + 2 * size) as isize {
            return Err(invalid("subframe header past the frame"));
        }
        let scale = 1i32 << (10 - size);
        self.predictors[0] = gb.sget(10) as i16;
        self.predictors[1] = gb.sget(10) as i16;
        self.predictors[2] = (gb.sget(size) * scale) as i16;
        self.predictors[3] = (gb.sget(size) * scale) as i16;
        if filter_order > 4 {
            let tmp = size - gb.bit();
            let mut x = 0;
            for i in 4..filter_order {
                if i & 3 == 0 {
                    x = tmp - gb.get(2);
                }
                self.predictors[i] = (gb.sget(x) * scale) as i16;
            }
        }

        let mut tfilter = [0u32; MAX_PREDICTORS];
        tfilter[0] = (i32::from(self.predictors[0]) * 64) as u32;
        for i in 1..filter_order {
            let p = i32::from(self.predictors[i]) as u32;
            for j in 0..(i + 1) / 2 {
                let (a, b) = (j, i - 1 - j);
                let (ta, tb) = (tfilter[a], tfilter[b]);
                let x = ta.wrapping_add((p.wrapping_mul(tb).wrapping_add(256) as i32 >> 9) as u32);
                tfilter[b] = tb.wrapping_add((p.wrapping_mul(ta).wrapping_add(256) as i32 >> 9) as u32);
                tfilter[a] = x;
            }
            tfilter[i] = (i32::from(self.predictors[i]) * 64) as u32;
        }
        let qshift = (15 - filter_quant) as u32;
        let x = 1i32 << (32 - qshift);
        let y = 1i32 << (qshift - 1);
        for i in 0..filter_order / 2 {
            let j = filter_order - 1 - i;
            self.filter[j] = x.wrapping_sub((tfilter[i] as i32).wrapping_add(y) >> qshift) as i16;
            self.filter[i] = x.wrapping_sub((tfilter[j] as i32).wrapping_add(y) >> qshift) as i16;
        }

        self.decode_residues(gb, &mut buf[start + filter_order..], subframe_size - filter_order)?;

        for i in 0..filter_order {
            self.residues[i] = (buf[start + i] >> dshift) as i16;
        }
        let mut at = start + filter_order;
        let y = RESIDUES - filter_order;
        let mut left = subframe_size - filter_order;
        let fq = filter_quant as u32;
        let wide = filter_order & !15;
        while left > 0 {
            let tmp = y.min(left);
            for i in 0..tmp {
                let mut v = (1u32 << (fq - 1)) as i32;
                if wide > 0 {
                    v = (v as u32).wrapping_add(scalarproduct(&self.residues[i..i + wide], &self.filter[..wide]) as u32) as i32;
                }
                let mut j = wide;
                while j < filter_order {
                    let r = &self.residues[i + j..i + j + 4];
                    let f = &self.filter[j..j + 4];
                    let sum = (i32::from(r[3]) as u32)
                        .wrapping_mul(i32::from(f[3]) as u32)
                        .wrapping_add((i32::from(r[2]) as u32).wrapping_mul(i32::from(f[2]) as u32))
                        .wrapping_add((i32::from(r[1]) as u32).wrapping_mul(i32::from(f[1]) as u32))
                        .wrapping_add((i32::from(r[0]) as u32).wrapping_mul(i32::from(f[0]) as u32));
                    v = (v as u32).wrapping_add(sum) as i32;
                    j += 4;
                }
                let out = (clip13(v >> fq) << dshift).wrapping_sub(buf[at]);
                buf[at] = out;
                at += 1;
                self.residues[filter_order + i] = (out >> dshift) as i16;
            }
            left -= tmp;
            if left > 0 {
                self.residues.copy_within(y..y + filter_order, 0);
            }
        }
        Ok(())
    }

    /// `decode_channel`
    fn decode_channel(&mut self, gb: &mut Bits, chan: usize) -> Result<()> {
        let bps = self.bps;
        self.sample_shift[chan] = Self::esc4(gb);
        if self.sample_shift[chan] >= bps {
            return Err(invalid("invalid sample shift"));
        }
        let mut buf = std::mem::take(&mut self.decoded[chan]);
        let result = self.decode_channel_into(gb, chan, &mut buf);
        self.decoded[chan] = buf;
        result
    }

    fn decode_channel_into(&mut self, gb: &mut Bits, chan: usize, buf: &mut [i32]) -> Result<()> {
        let bps = self.bps;
        buf[0] = gb.sget(bps - self.sample_shift[chan]);
        self.lpc_mode[chan] = gb.get(2) as i32;
        let nb_subframes = gb.get(3) as usize + 1;
        let mut left = self.nb_samples as i32 - 1;
        let mut i = 0;
        if nb_subframes > 1 {
            if gb.left() < ((nb_subframes - 1) * 6) as isize {
                return Err(invalid("subframe lengths past the frame"));
            }
            let mut prev = 0i32;
            while i < nb_subframes - 1 {
                let v = gb.get(6) as i32;
                self.subframe_len[i] = ((v - prev) * self.subframe_scale) as i16;
                if self.subframe_len[i] <= 0 {
                    return Err(invalid("invalid subframe length"));
                }
                left -= i32::from(self.subframe_len[i]);
                prev = v;
                i += 1;
            }
            if left <= 0 {
                return Err(invalid("invalid subframe lengths"));
            }
        }
        self.subframe_len[i] = left as i16;

        let mut at = 1;
        let mut prev = 0usize;
        for i in 0..nb_subframes {
            let len = self.subframe_len[i] as usize;
            self.decode_subframe(gb, buf, at, len, prev)?;
            at += len;
            prev = len;
        }
        Ok(())
    }

    /// `decorrelate`: channels `c1` and `c2`, `length` samples after the
    /// first.
    fn decorrelate(&mut self, gb: &mut Bits, c1: usize, c2: usize, length: usize) -> Result<()> {
        let mut b1 = std::mem::take(&mut self.decoded[c1]);
        let mut b2 = std::mem::take(&mut self.decoded[c2]);
        let result = self.decorrelate_into(gb, &mut b1, &mut b2, length);
        self.decoded[c1] = b1;
        self.decoded[c2] = b2;
        result
    }

    fn decorrelate_into(&mut self, gb: &mut Bits, b1: &mut [i32], b2: &mut [i32], length: usize) -> Result<()> {
        let off = usize::from(self.dmode > 5);
        let length = length + usize::from(self.dmode < 6);
        let (bp1, bp2) = (b1[off], b2[off]);
        // p1 and p2 as FFmpeg swaps them for modes 4 and 6.
        let swapped = self.dmode == 4 || self.dmode == 6;
        let (p1, p2): (&mut [i32], &mut [i32]) = if swapped { (b2, b1) } else { (b1, b2) };
        let (p1, p2) = (&mut p1[off..], &mut p2[off..]);
        match self.dmode {
            1 => {
                // decorrelate_ls
                for i in 0..length {
                    p2[i] = p1[i].wrapping_add(p2[i]);
                }
            }
            2 => {
                // decorrelate_sr
                for i in 0..length {
                    p1[i] = p2[i].wrapping_sub(p1[i]);
                }
            }
            3 => {
                // decorrelate_sm
                for i in 0..length {
                    let b = p2[i];
                    let a = p1[i].wrapping_sub(b >> 1);
                    p1[i] = a;
                    p2[i] = a.wrapping_add(b);
                }
            }
            4 | 5 => {
                // decorrelate_sf
                let dshift = Self::esc4(gb);
                let dfactor = gb.sget(10);
                for i in 0..length {
                    let a = p1[i];
                    let b = ((dfactor as u32).wrapping_mul((p2[i] >> dshift) as u32).wrapping_add(128) as i32 >> 8) as u32;
                    p1[i] = (b << dshift).wrapping_sub(a as u32) as i32;
                }
            }
            6 | 7 => {
                if length < 256 {
                    return Err(invalid("filter decorrelation of a short frame"));
                }
                let dshift = Self::esc4(gb);
                let filter_order = 8usize << gb.bit();
                let dval1 = gb.bit() != 0;
                let dval2 = gb.bit() != 0;
                let mut code_size = 0;
                for i in 0..filter_order {
                    if i & 3 == 0 {
                        code_size = 14 - gb.get(3);
                    }
                    self.filter[i] = gb.sget(code_size) as i16;
                }
                let order_half = filter_order / 2;
                let mut length2 = length as isize - (filter_order as isize - 1);
                if dval1 {
                    for i in 0..order_half {
                        p1[i] = p1[i].wrapping_add(p2[i]);
                    }
                }
                if dval2 {
                    let from = (length2 + order_half as isize).max(0) as usize;
                    for i in from..length {
                        p1[i] = p1[i].wrapping_add(p2[i]);
                    }
                }
                for i in 0..filter_order {
                    self.residues[i] = (p2[i] >> dshift) as i16;
                }
                let mut q2 = filter_order;
                let mut q1 = order_half;
                let x = RESIDUES - filter_order;
                while length2 > 0 {
                    let tmp = (length2 as usize).min(x);
                    let last = usize::from(tmp as isize == length2);
                    for i in 0..tmp - last {
                        self.residues[filter_order + i] = (p2[q2] >> dshift) as i16;
                        q2 += 1;
                    }
                    for i in 0..tmp {
                        let mut v: i32 = 1 << 9;
                        if filter_order == 16 {
                            v = v.wrapping_add(scalarproduct(&self.residues[i..i + 16], &self.filter[..16]));
                        } else {
                            let r = &self.residues[i..i + 8];
                            let f = &self.filter[..8];
                            let mut s = 0i32;
                            for k in (0..8).rev() {
                                s = s.wrapping_add(i32::from(r[k]) * i32::from(f[k]));
                            }
                            v = v.wrapping_add(s);
                        }
                        let out = ((clip13(v >> 10) as u32).wrapping_mul(1u32 << dshift)).wrapping_sub(p1[q1] as u32);
                        p1[q1] = out as i32;
                        q1 += 1;
                    }
                    self.residues.copy_within(tmp..tmp + filter_order, 0);
                    length2 -= tmp as isize;
                }
            }
            _ => {}
        }
        if self.dmode > 0 && self.dmode < 6 {
            // p1 and p2 get back their first samples (after a swap, each
            // the other's), as FFmpeg restores them.
            let (r1, r2) = if swapped { (bp2, bp1) } else { (bp1, bp2) };
            p1[0] = r1;
            p2[0] = r2;
        }
        Ok(())
    }

    /// `tak_decode_frame`: the frame's planes in the output format.
    fn decode_frame(&mut self, data: &[u8]) -> Result<(usize, Vec<Vec<u8>>)> {
        if data.len() < TAK_MIN_FRAME_HEADER_BYTES {
            return Err(invalid("packet shorter than a frame header"));
        }
        let mut gb = Bits::new(data);
        if !decode_frame_header(&mut gb, &mut self.ti) {
            return Err(invalid("invalid frame header"));
        }
        let ti = self.ti;
        if ti.codec != TAK_CODEC_MONO_STEREO && ti.codec != TAK_CODEC_MULTICHANNEL {
            return Err(Error::unsupported(format!("tak: codec type {}", ti.codec)));
        }
        if ti.data_type != 0 {
            return Err(invalid("unsupported data type"));
        }
        if ti.codec == TAK_CODEC_MONO_STEREO && ti.channels > 2 {
            return Err(invalid("invalid number of channels"));
        }
        if ti.channels > 6 {
            return Err(invalid("unsupported number of channels"));
        }
        if ti.frame_samples <= 0 {
            return Err(invalid("unsupported/invalid number of samples"));
        }
        if !matches!(ti.bps, 8 | 16 | 24) {
            return Err(invalid(&format!("invalid/unsupported bits per sample: {}", ti.bps)));
        }
        self.bps = ti.bps;
        if ti.sample_rate != self.sample_rate {
            self.sample_rate = ti.sample_rate;
            self.set_sample_rate_params();
        }
        let channels = self.channels();
        self.nb_samples = if ti.last_frame_samples != 0 { ti.last_frame_samples } else { ti.frame_samples } as usize;
        let nb = self.nb_samples;
        for d in &mut self.decoded[..channels] {
            d.resize(nb, 0);
        }

        if nb < 16 {
            for chan in 0..channels {
                for i in 0..nb {
                    self.decoded[chan][i] = gb.sget(self.bps);
                }
            }
        } else {
            if ti.codec == TAK_CODEC_MONO_STEREO {
                for chan in 0..channels {
                    self.decode_channel(&mut gb, chan)?;
                }
                if channels == 2 {
                    let nb_subframes = gb.get(1) + 1;
                    if nb_subframes > 1 {
                        self.subframe_len[1] = gb.get(6) as i16;
                    }
                    self.dmode = gb.get(3) as i32;
                    self.decorrelate(&mut gb, 0, 1, nb - 1)?;
                }
            } else {
                let chan;
                if gb.bit() != 0 {
                    let mut ch_mask = 0u32;
                    chan = gb.get(4) as usize + 1;
                    if chan > channels {
                        return Err(invalid("too many coded channels"));
                    }
                    for i in 0..chan {
                        let nbit = gb.get(4) as usize;
                        if nbit >= channels || ch_mask & 1 << nbit != 0 {
                            return Err(invalid("invalid channel"));
                        }
                        let p = &mut self.mcdparams[i];
                        p.present = gb.bit() != 0;
                        if p.present {
                            p.index = gb.get(2) as usize;
                            p.chan2 = gb.get(4) as usize;
                            if p.chan2 >= channels {
                                return Err(invalid("invalid channel 2"));
                            }
                            if p.index == 1 {
                                if nbit == p.chan2 || ch_mask & 1 << p.chan2 != 0 {
                                    return Err(invalid("invalid channel 2"));
                                }
                                ch_mask |= 1 << p.chan2;
                            } else if ch_mask & 1 << p.chan2 == 0 {
                                return Err(invalid("invalid channel 2"));
                            }
                        }
                        p.chan1 = nbit;
                        ch_mask |= 1 << nbit;
                    }
                } else {
                    chan = channels;
                    for i in 0..chan {
                        self.mcdparams[i].present = false;
                        self.mcdparams[i].chan1 = i;
                    }
                }
                for i in 0..chan {
                    let p = self.mcdparams[i];
                    if p.present && p.index == 1 {
                        self.decode_channel(&mut gb, p.chan2)?;
                    }
                    self.decode_channel(&mut gb, p.chan1)?;
                    if p.present {
                        self.dmode = MC_DMODES[p.index];
                        self.decorrelate(&mut gb, p.chan2, p.chan1, nb - 1)?;
                    }
                }
            }
            for chan in 0..channels {
                if self.lpc_mode[chan] != 0 {
                    decode_lpc(&mut self.decoded[chan], self.lpc_mode[chan], nb);
                }
                let shift = self.sample_shift[chan];
                if shift > 0 {
                    for v in &mut self.decoded[chan][..nb] {
                        *v = (*v as u32).wrapping_mul(1u32 << shift) as i32;
                    }
                }
            }
        }
        // The frame CRC: FFmpeg only logs an over- or under-read.
        gb.align();
        gb.skip(24);

        let planes = self.decoded[..channels]
            .iter()
            .map(|d| match self.bps {
                8 => d[..nb].iter().map(|&v| (v as u32).wrapping_add(0x80) as u8).collect(),
                16 => d[..nb].iter().flat_map(|&v| (v as i16).to_le_bytes()).collect(),
                _ => d[..nb].iter().flat_map(|&v| ((v as u32).wrapping_mul(256) as i32).to_le_bytes()).collect(),
            })
            .collect();
        Ok((nb, planes))
    }
}

impl Decoder for TakDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: self.sample_format(),
            sample_rate: self.sample_rate.max(0) as u32,
            channels: self.channels() as u16,
        })
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        let (samples, planes) = self.decode_frame(&packet.data)?;
        self.ready.push_back(Frame::Audio(AudioFrame { samples: samples as u32, pts: packet.pts, data: planes }));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.ready.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// FFmpeg's decoder has no flush callback: the stream info a frame
    /// carried stays for the frames after a seek.
    fn reset(&mut self) -> Result<()> {
        self.ready.clear();
        Ok(())
    }
}
