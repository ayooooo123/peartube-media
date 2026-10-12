// ALAC (Apple Lossless Audio Codec) decoder.
//
// Ported from FFmpeg libavcodec/alac.c, alacdsp.c and alac_data.c
// (commit 2da55bf), LGPL-2.1-or-later.

//! ALAC: Rice-coded prediction errors, an adaptive LPC predictor per
//! channel, stereo decorrelation and up to 16 uncoded low bits per sample,
//! bit-exact with FFmpeg. Output is planar: S16P for 16-bit streams, S32P
//! (samples in the top bits) for 20-, 24- and 32-bit ones.
//!
//! The configuration is the ALAC magic cookie (`ALACSpecificConfig`).
//! FFmpeg's decoder takes it inside its 36-byte `alac` atom, which FFmpeg's
//! demuxers fabricate where a container stores the bare cookie; this one
//! takes every form the containers carry: the atom, an `alac` atom nested
//! in a QuickTime `wave` atom or an old-style CAF `kuki` (`frma` first),
//! and the bare 24-byte cookie (MP4 `alac` box payload, Matroska
//! `A_ALAC` CodecPrivate, new-style CAF `kuki`).

use std::collections::VecDeque;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result,
    SampleFormat,
};

use crate::getbits::GetBits;

/// `ALAC_MAX_CHANNELS`
const MAX_CHANNELS: usize = 8;
/// `alac_set_info`: the largest `max_samples_per_frame` FFmpeg takes.
const MAX_SAMPLES_PER_FRAME: u32 = 4096 * 4096;
/// The largest output frame accepted: one frame of the declared size, all
/// channels, must fit in 256 MiB.
const MAX_FRAME_BYTES: u64 = 256 << 20;

/// `ff_alac_channel_layout_offsets`: the output channel of each coded
/// channel, by channel count.
const CHANNEL_LAYOUT_OFFSETS: [[usize; MAX_CHANNELS]; MAX_CHANNELS] = [
    [0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 0, 0, 0, 0, 0, 0],
    [2, 0, 1, 0, 0, 0, 0, 0],
    [2, 0, 1, 3, 0, 0, 0, 0],
    [2, 0, 1, 3, 4, 0, 0, 0],
    [2, 0, 1, 4, 5, 3, 0, 0],
    [2, 0, 1, 4, 5, 6, 3, 0],
    [2, 6, 7, 0, 1, 4, 5, 3],
];

/// `enum AlacRawDataBlockType`
const TYPE_CPE: u32 = 1;
const TYPE_LFE: u32 = 3;
const TYPE_END: u32 = 7;

/// The fields of `ALACSpecificConfig` FFmpeg reads (`alac_set_info`).
#[derive(Clone, Copy, Debug)]
struct Config {
    max_samples_per_frame: u32,
    sample_size: u8,
    rice_history_mult: u8,
    rice_initial_history: u8,
    rice_limit: u8,
    channels: u8,
    sample_rate: u32,
}

impl Config {
    /// The cookie in any of its container forms (see the module docs).
    fn parse(extradata: &[u8]) -> Result<Self> {
        let be32 = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        let atom_at = |i: usize| {
            extradata.len() >= i + 36
                && &extradata[i + 4..i + 8] == b"alac"
                && (36..=extradata.len() - i).contains(&(be32(&extradata[i..]) as usize))
        };
        let cookie = if let Some(i) = (0..extradata.len().saturating_sub(35)).find(|&i| atom_at(i)) {
            &extradata[i + 12..i + 36]
        } else if extradata.len() >= 24 {
            &extradata[..24]
        } else {
            return Err(Error::invalid("alac: extradata is too small"));
        };
        Ok(Self {
            max_samples_per_frame: be32(&cookie[0..4]),
            // cookie[4]: compatible version
            sample_size: cookie[5],
            rice_history_mult: cookie[6],
            rice_initial_history: cookie[7],
            rice_limit: cookie[8],
            channels: cookie[9],
            // maxRun, max coded frame size, average bitrate
            sample_rate: be32(&cookie[20..24]),
        })
    }
}

/// `av_log2`, with `av_log2(0) == 0`.
fn av_log2(v: u32) -> u32 {
    31 - (v | 1).leading_zeros()
}

/// `sign_extend(val, bits)` for `bits` in 1..=32.
fn sign_extend(val: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((val << shift) as i32) >> shift
}

/// `FFDIFFSIGN(v, 0)`
fn sign_only(v: i32) -> i32 {
    (v > 0) as i32 - (v < 0) as i32
}

/// `decode_scalar`
fn decode_scalar(gb: &mut GetBits, k: u32, bps: u32) -> u32 {
    let mut x = gb.unary0(9);
    if x > 8 {
        // RICE THRESHOLD: the value follows as is.
        x = gb.get(bps);
    } else if k != 1 {
        let extrabits = gb.show(k);
        // multiply x by 2^k - 1, as part of their strange algorithm
        x = (x << k).wrapping_sub(x);
        if extrabits > 1 {
            x = x.wrapping_add(extrabits - 1);
            gb.skip(k);
        } else {
            gb.skip(k - 1);
        }
    }
    x
}

/// `lpc_prediction`, in place: `buf` holds the prediction errors and
/// receives the samples (FFmpeg reads one buffer and writes another; each
/// error is read before its sample is written, so the result is the same).
fn lpc_prediction(buf: &mut [i32], bps: u32, lpc_coefs: &mut [i16; 32], lpc_order: usize, lpc_quant: u32) {
    let nb_samples = buf.len();
    // The first sample always copies.
    if nb_samples <= 1 || lpc_order == 0 {
        return;
    }
    if lpc_order == 31 {
        // simple 1st-order prediction
        for i in 1..nb_samples {
            buf[i] = sign_extend((buf[i - 1] as u32).wrapping_add(buf[i] as u32), bps);
        }
        return;
    }
    // warm-up samples
    let mut i = 1;
    while i <= lpc_order && i < nb_samples {
        buf[i] = sign_extend((buf[i - 1] as u32).wrapping_add(buf[i] as u32), bps);
        i += 1;
    }
    while i < nb_samples {
        let mut error_val = buf[i] as u32;
        // `pred` points at the sample after `d`: buf[i - lpc_order..i].
        let base = i - lpc_order;
        let d = buf[base - 1];
        let mut val: i32 = 0;
        for j in 0..lpc_order {
            let diff = (buf[base + j] as u32).wrapping_sub(d as u32);
            val = (val as u32).wrapping_add(diff.wrapping_mul(i32::from(lpc_coefs[j]) as u32)) as i32;
        }
        let val = ((i64::from(val) + (1i64 << (lpc_quant - 1))) >> lpc_quant) as i32;
        let val = (val as u32).wrapping_add((d as u32).wrapping_add(error_val));
        buf[i] = sign_extend(val, bps);

        // adapt LPC coefficients
        let error_sign = sign_only(error_val as i32);
        if error_sign != 0 {
            let mut j = 0;
            while j < lpc_order && (error_val.wrapping_mul(error_sign as u32) as i32) > 0 {
                let val = (d as u32).wrapping_sub(buf[base + j] as u32) as i32;
                let sign = sign_only(val) * error_sign;
                lpc_coefs[j] = lpc_coefs[j].wrapping_sub(sign as i16);
                let val = (val as u32).wrapping_mul(sign as u32) as i32;
                error_val = error_val.wrapping_sub(((val >> lpc_quant) as u32).wrapping_mul(j as u32 + 1));
                j += 1;
            }
        }
        i += 1;
    }
}

/// `decorrelate_stereo` (alacdsp.c)
fn decorrelate_stereo(left: &mut [i32], right: &mut [i32], decorr_shift: u32, decorr_left_weight: u32) {
    for (l, r) in left.iter_mut().zip(right.iter_mut()) {
        let mut a = *l as u32;
        let mut b = *r as u32;
        a = a.wrapping_sub(((b.wrapping_mul(decorr_left_weight) as i32) >> decorr_shift) as u32);
        b = b.wrapping_add(a);
        *l = b as i32;
        *r = a as i32;
    }
}

/// `append_extra_bits` (alacdsp.c)
fn append_extra_bits(buf: &mut [i32], extra: &[i32], extra_bits: u32) {
    for (s, &e) in buf.iter_mut().zip(extra) {
        *s = (((*s as u32) << extra_bits) | e as u32) as i32;
    }
}

pub struct AlacDecoder {
    codec_id: CodecId,
    config: Config,
    channels: usize,
    sample_rate: u32,
    /// The coded channels of the element being decoded (at most two):
    /// prediction errors, then samples.
    buf: [Vec<i32>; 2],
    extra_bits_buffer: [Vec<i32>; 2],
    out: VecDeque<Frame>,
}

/// The planes of the frame a packet builds, and its sample count.
struct FrameOut {
    nb_samples: usize,
    planes: Vec<Vec<u8>>,
}

impl AlacDecoder {
    fn sample_format(&self) -> SampleFormat {
        if self.config.sample_size == 16 { SampleFormat::S16P } else { SampleFormat::S32P }
    }

    /// `decode_element`: one SCE/CPE/LFE of `channels` coded channels,
    /// written to the output planes from `ch_index` on.
    fn decode_element(
        &mut self,
        gb: &mut GetBits,
        frame: &mut Option<FrameOut>,
        ch_index: usize,
        channels: usize,
    ) -> Result<()> {
        gb.skip(4); // element instance tag
        gb.skip(12); // unused header bits

        // the number of output samples is stored in the frame
        let has_size = gb.get1() != 0;

        let mut extra_bits = gb.get(2) << 3;
        let bps = i32::from(self.config.sample_size) - extra_bits as i32 + channels as i32 - 1;
        if bps > 32 {
            return Err(Error::unsupported(format!("alac: bps {bps}")));
        }
        if bps < 1 {
            return Err(Error::invalid("alac: bps"));
        }
        let bps = bps as u32;

        // whether the frame is compressed
        let is_compressed = gb.get1() == 0;

        let output_samples =
            if has_size { gb.get(32) } else { self.config.max_samples_per_frame };
        if output_samples == 0 || output_samples > self.config.max_samples_per_frame {
            return Err(Error::invalid(format!("alac: invalid samples per frame: {output_samples}")));
        }
        let nb_samples = output_samples as usize;
        let bytes = nb_samples * self.sample_format().bytes_per_sample();
        let frame = match frame {
            None => frame.insert(FrameOut { nb_samples, planes: vec![vec![0; bytes]; self.channels] }),
            Some(f) if f.nb_samples != nb_samples => {
                return Err(Error::invalid(format!(
                    "alac: sample count mismatch: {output_samples} != {}",
                    f.nb_samples
                )));
            }
            Some(f) => f,
        };
        for ch in 0..channels {
            self.buf[ch].resize(nb_samples, 0);
        }

        let (decorr_shift, decorr_left_weight);
        if is_compressed {
            if self.config.rice_limit == 0 {
                return Err(Error::unsupported("alac: compression with rice limit 0"));
            }
            decorr_shift = gb.get(8);
            decorr_left_weight = gb.get(8);
            if channels == 2 && decorr_left_weight != 0 && decorr_shift > 31 {
                return Err(Error::invalid("alac: decorrelation shift"));
            }

            let mut lpc_coefs = [[0i16; 32]; 2];
            let mut lpc_order = [0usize; 2];
            let mut prediction_type = [0u32; 2];
            let mut lpc_quant = [0u32; 2];
            let mut rice_history_mult = [0u32; 2];
            for ch in 0..channels {
                prediction_type[ch] = gb.get(4);
                lpc_quant[ch] = gb.get(4);
                rice_history_mult[ch] = gb.get(3);
                lpc_order[ch] = gb.get(5) as usize;
                if lpc_order[ch] as u32 >= self.config.max_samples_per_frame || lpc_quant[ch] == 0 {
                    return Err(Error::invalid("alac: predictor"));
                }
                // read the predictor table
                for i in (0..lpc_order[ch]).rev() {
                    lpc_coefs[ch][i] = gb.get_signed(16) as i16;
                }
            }

            if extra_bits != 0 {
                if gb.bits_left() < (nb_samples * channels) as i64 * i64::from(extra_bits) {
                    return Err(Error::invalid("alac: extra bits"));
                }
                for ch in 0..channels {
                    self.extra_bits_buffer[ch].resize(nb_samples, 0);
                }
                for i in 0..nb_samples {
                    for ch in 0..channels {
                        self.extra_bits_buffer[ch][i] = gb.get(extra_bits) as i32;
                    }
                }
            }
            for ch in 0..channels {
                let history_mult = rice_history_mult[ch] * u32::from(self.config.rice_history_mult) / 4;
                rice_decompress(&self.config, gb, &mut self.buf[ch], bps, history_mult)?;

                // adaptive FIR filter
                if prediction_type[ch] == 15 {
                    // Prediction type 15 runs the adaptive FIR twice: first
                    // the special-case order 31, then the coefficients from
                    // the bitstream.
                    lpc_prediction(&mut self.buf[ch], bps, &mut [0; 32], 31, 0);
                }
                lpc_prediction(&mut self.buf[ch], bps, &mut lpc_coefs[ch], lpc_order[ch], lpc_quant[ch]);
            }
        } else {
            // not compressed, easy case
            let sample_size = u32::from(self.config.sample_size);
            if gb.bits_left() < (nb_samples * channels) as i64 * i64::from(sample_size) {
                return Err(Error::invalid("alac: uncompressed frame is short"));
            }
            for i in 0..nb_samples {
                for ch in 0..channels {
                    self.buf[ch][i] = gb.get_signed(sample_size);
                }
            }
            extra_bits = 0;
            decorr_shift = 0;
            decorr_left_weight = 0;
        }

        if channels == 2 {
            let [left, right] = &mut self.buf;
            if decorr_left_weight != 0 {
                decorrelate_stereo(left, right, decorr_shift, decorr_left_weight);
            }
        }
        if extra_bits != 0 {
            for ch in 0..channels {
                append_extra_bits(&mut self.buf[ch], &self.extra_bits_buffer[ch], extra_bits);
            }
        }

        for ch in 0..channels {
            let plane = &mut frame.planes[ch_index + ch];
            let samples = &self.buf[ch][..nb_samples];
            match self.config.sample_size {
                16 => {
                    for (out, &s) in plane.chunks_exact_mut(2).zip(samples) {
                        out.copy_from_slice(&(s as i16).to_le_bytes());
                    }
                }
                size => {
                    let shift = match size {
                        20 => 12,
                        24 => 8,
                        _ => 0,
                    };
                    for (out, &s) in plane.chunks_exact_mut(4).zip(samples) {
                        out.copy_from_slice(&((s as u32).wrapping_mul(1 << shift)).to_le_bytes());
                    }
                }
            }
        }
        Ok(())
    }

    /// `alac_decode_frame`: the frame a packet holds, if it decodes every
    /// channel.
    fn decode_packet(&mut self, data: &[u8]) -> Result<Option<FrameOut>> {
        let mut gb = GetBits::new(data);
        let mut frame = None;
        let mut got_end = false;
        let mut ch = 0;
        while gb.bits_left() >= 3 {
            let element = gb.get(3);
            if element == TYPE_END {
                got_end = true;
                break;
            }
            if element > TYPE_CPE && element != TYPE_LFE {
                return Err(Error::unsupported(format!("alac: syntax element {element}")));
            }
            let channels = if element == TYPE_CPE { 2 } else { 1 };
            let offsets = &CHANNEL_LAYOUT_OFFSETS[self.channels - 1];
            if ch + channels > self.channels || offsets[ch] + channels > self.channels {
                return Err(Error::invalid("alac: invalid element channel count"));
            }
            let result = self.decode_element(&mut gb, &mut frame, offsets[ch], channels);
            if let Err(e) = result {
                if gb.bits_left() != 0 {
                    return Err(e);
                }
            }
            ch += channels;
        }
        if !got_end {
            return Err(Error::invalid("alac: no end tag found, incomplete packet"));
        }
        // FFmpeg outputs the frame only when every channel decoded.
        Ok(frame.filter(|_| ch == self.channels))
    }
}

/// `rice_decompress`: `buf.len()` prediction errors.
fn rice_decompress(config: &Config, gb: &mut GetBits, buf: &mut [i32], bps: u32, rice_history_mult: u32) -> Result<()> {
    let nb_samples = buf.len();
    let rice_limit = u32::from(config.rice_limit);
    let mut history = u32::from(config.rice_initial_history);
    let mut sign_modifier = 0u32;
    let mut i = 0;
    while i < nb_samples {
        if gb.bits_left() <= 0 {
            return Err(Error::invalid("alac: rice data is short"));
        }
        // calculate rice param and decode next value
        let k = av_log2((history >> 9) + 3).min(rice_limit);
        let mut x = decode_scalar(gb, k, bps);
        x = x.wrapping_add(sign_modifier);
        sign_modifier = 0;
        buf[i] = ((x >> 1) ^ 0u32.wrapping_sub(x & 1)) as i32;

        // update the history
        if x > 0xffff {
            history = 0xffff;
        } else {
            history = history
                .wrapping_add(x.wrapping_mul(rice_history_mult))
                .wrapping_sub(history.wrapping_mul(rice_history_mult) >> 9);
        }

        // special case: there may be compressed blocks of 0
        if history < 128 && i + 1 < nb_samples {
            let k = (7 - av_log2(history) as i32 + ((history + 16) >> 6) as i32) as u32;
            let k = k.min(rice_limit);
            let mut block_size = decode_scalar(gb, k, 16) as usize;
            if block_size > 0 {
                if block_size >= nb_samples - i {
                    block_size = nb_samples - i - 1;
                }
                buf[i + 1..i + 1 + block_size].fill(0);
                i += block_size;
            }
            if block_size <= 0xffff {
                sign_modifier = 1;
            }
            history = 0;
        }
        i += 1;
    }
    Ok(())
}

impl Decoder for AlacDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        if let Some(frame) = self.decode_packet(&packet.data)? {
            self.out.push_back(Frame::Audio(AudioFrame {
                samples: frame.nb_samples as u32,
                pts: packet.pts,
                data: frame.planes,
            }));
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.out.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.out.clear();
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: self.sample_format(),
            sample_rate: self.sample_rate,
            channels: self.channels as u16,
        })
    }
}

/// `alac_decode_init`
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let config = Config::parse(&params.extradata)?;
    if config.max_samples_per_frame == 0 || config.max_samples_per_frame > MAX_SAMPLES_PER_FRAME {
        return Err(Error::invalid(format!(
            "alac: max samples per frame invalid: {}",
            config.max_samples_per_frame
        )));
    }
    let bytes_per_sample = match config.sample_size {
        16 => 2u64,
        20 | 24 | 32 => 4,
        size => return Err(Error::unsupported(format!("alac: sample depth {size}"))),
    };
    let channels = match config.channels {
        0 => match params.channels {
            Some(c) if c >= 1 => usize::from(c),
            _ => return Err(Error::invalid("alac: invalid channel count")),
        },
        c => usize::from(c),
    };
    if channels > MAX_CHANNELS {
        return Err(Error::unsupported(format!("alac: channel count {channels}")));
    }
    if u64::from(config.max_samples_per_frame) * channels as u64 * bytes_per_sample > MAX_FRAME_BYTES {
        return Err(Error::invalid("alac: frames would exceed 256 MiB"));
    }
    // FFmpeg takes the rate from the cookie.
    let sample_rate = match config.sample_rate {
        0 => params.sample_rate.unwrap_or(0),
        rate => rate,
    };
    Ok(Box::new(AlacDecoder {
        codec_id: CodecId::new("alac"),
        config,
        channels,
        sample_rate,
        buf: [Vec::new(), Vec::new()],
        extra_bits_buffer: [Vec::new(), Vec::new()],
        out: VecDeque::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_forms() {
        let mut cookie = vec![0, 0, 0x10, 0, 0, 16, 40, 10, 14, 2, 0, 255];
        cookie.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xac, 0x44]);
        let mut atom = vec![0, 0, 0, 36];
        atom.extend_from_slice(b"alac");
        atom.extend_from_slice(&[0; 4]);
        atom.extend_from_slice(&cookie);
        let mut wave = vec![0, 0, 0, 12];
        wave.extend_from_slice(b"frmaalac");
        wave.extend_from_slice(&atom);
        wave.extend_from_slice(&[0, 0, 0, 8, 0, 0, 0, 0]);
        for (what, form) in [("bare", &cookie), ("atom", &atom), ("nested", &wave)] {
            let c = Config::parse(form).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(
                (c.max_samples_per_frame, c.sample_size, c.rice_limit, c.channels, c.sample_rate),
                (4096, 16, 14, 2, 44100),
                "{what}"
            );
        }
        assert!(Config::parse(&cookie[..23]).is_err());
    }

    #[test]
    fn log2_of_zero_is_zero() {
        assert_eq!((av_log2(0), av_log2(1), av_log2(3), av_log2(0x8003)), (0, 0, 1, 15));
    }
}
