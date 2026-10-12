// MPEG audio Layer I decoder, fixed point.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/mpegaudiodec_template.c
// (decode_frame, mp_decode_frame, mp_decode_layer1, l1_unscale, the Layer I
// part of decode_init_static, mp_flush) as mpegaudiodec_fixed.c builds it
// (FRAC_BITS 23, S16P output), with the scale factor table of
// mpegaudiodec_common.c, and decode.c's calling pattern (a packet decoded
// until its bytes are taken).
// Copyright (c) 2001, 2002 Fabrice Bellard; LGPL-2.1-or-later (see
// LICENSE).

use std::collections::VecDeque;
use std::sync::LazyLock;

use mpegaudiodsp::MpaSynth;
use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

use crate::bits::Bits;
use crate::header::{self, Header, MPA_JSTEREO};

/// `SBLIMIT`
const SBLIMIT: usize = 32;
/// Samples per channel in a Layer I frame (`avctx->frame_size`).
const FRAME_SAMPLES: usize = 384;
const FRAC_BITS: u32 = 23;
const FRAC_ONE: i64 = 1 << FRAC_BITS;
const HEADER_SIZE: usize = 4;

/// `FIXR(a)`: `(int)(a * FRAC_ONE + 0.5)`.
fn fixr(a: f64) -> i32 {
    (a * FRAC_ONE as f64 + 0.5) as i32
}

/// `scale_factor_mult[15][3]`, decode_init_static's Layer I table.
static SCALE_FACTOR_MULT: LazyLock<[[i32; 3]; 15]> = LazyLock::new(|| {
    let mut t = [[0i32; 3]; 15];
    for (i, row) in t.iter_mut().enumerate() {
        let n = i as u32 + 2;
        let norm = (((1i64 << n) * FRAC_ONE) / ((1i64 << n) - 1)) as i32;
        for (m, f) in [1.0, 0.7937005259, 0.6299605249].into_iter().enumerate() {
            // MULLx(norm, FIXR(f * 2.0), FRAC_BITS)
            row[m] = ((i64::from(norm) * i64::from(fixr(f * 2.0))) >> FRAC_BITS) as i32;
        }
    }
    t
});

/// `l1_unscale`: `n` the mantissa bits minus one (1 to 15).
fn l1_unscale(n: u32, mant: u32, scale_factor: u32) -> i32 {
    // ff_scale_factor_modshift[scale_factor] = mod | shift << 2
    let shift = scale_factor / 3;
    let modulo = (scale_factor % 3) as usize;
    let mant = mant.wrapping_add(u32::MAX << n).wrapping_add(1) as i32;
    let val = i64::from(mant) * i64::from(SCALE_FACTOR_MULT[n as usize - 1][modulo]);
    let shift = shift + n;
    ((val + (1i64 << (shift - 1))) >> shift) as i32
}

fn invalid(what: &str) -> Error {
    Error::invalid(format!("mp1: {what}"))
}

/// The `mp1` decoder (`MPADecodeContext` for Layer I).
pub struct Mp1Decoder {
    codec_id: CodecId,
    sample_rate: u32,
    channels: usize,
    synth: MpaSynth,
    dither_state: i32,
    ready: VecDeque<Frame>,
}

impl Mp1Decoder {
    /// The stream's rate and channel count come from its parameters (the
    /// demuxer reads them from the first frame header).
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let channels = params.channels.ok_or_else(|| invalid("no channel count"))?;
        let sample_rate = params.sample_rate.ok_or_else(|| invalid("no sample rate"))?;
        if !(1..=2).contains(&channels) {
            return Err(invalid(&format!("{channels} channels (Layer I carries 1 or 2)")));
        }
        Ok(Self {
            codec_id: params.codec_id.clone(),
            sample_rate,
            channels: usize::from(channels),
            synth: MpaSynth::new(),
            dither_state: 0,
            ready: VecDeque::new(),
        })
    }

    /// `mp_decode_layer1`: the 12 subband samples of each subband.
    fn decode_layer1(hdr: &Header, gb: &mut Bits) -> [[[i32; SBLIMIT]; 12]; 2] {
        let nch = hdr.nb_channels;
        let bound = if hdr.mode == MPA_JSTEREO { (hdr.mode_ext as usize + 1) * 4 } else { SBLIMIT };
        let mut allocation = [[0u32; SBLIMIT]; 2];
        let mut scale_factors = [[0u32; SBLIMIT]; 2];
        for i in 0..bound {
            for ch in 0..nch {
                allocation[ch][i] = gb.get(4);
            }
        }
        for a in &mut allocation[0][bound..] {
            *a = gb.get(4);
        }
        for i in 0..bound {
            for ch in 0..nch {
                if allocation[ch][i] != 0 {
                    scale_factors[ch][i] = gb.get(6);
                }
            }
        }
        for i in bound..SBLIMIT {
            if allocation[0][i] != 0 {
                scale_factors[0][i] = gb.get(6);
                scale_factors[1][i] = gb.get(6);
            }
        }
        let mut sb = [[[0i32; SBLIMIT]; 12]; 2];
        for j in 0..12 {
            for i in 0..bound {
                for ch in 0..nch {
                    let n = allocation[ch][i];
                    if n != 0 {
                        let mant = gb.get(n + 1);
                        sb[ch][j][i] = l1_unscale(n, mant, scale_factors[ch][i]);
                    }
                }
            }
            for i in bound..SBLIMIT {
                let n = allocation[0][i];
                if n != 0 {
                    let mant = gb.get(n + 1);
                    sb[0][j][i] = l1_unscale(n, mant, scale_factors[0][i]);
                    sb[1][j][i] = l1_unscale(n, mant, scale_factors[1][i]);
                }
            }
        }
        sb
    }

    /// `mp_decode_frame` for a Layer I frame of `size` bytes at the start
    /// of `buf` (the rest of the packet follows it).
    fn decode_frame(&mut self, hdr: &Header, buf: &[u8], size: usize) -> Frame {
        let mut gb = Bits::new(&buf[HEADER_SIZE.min(buf.len())..], size.saturating_sub(HEADER_SIZE));
        if hdr.error_protection {
            // The CRC: checked only with err_detect crccheck, which is off
            // by default, and even then only logged.
            gb.get(16);
        }
        let sb = Self::decode_layer1(hdr, &mut gb);
        let mut planes = Vec::with_capacity(hdr.nb_channels);
        let mut pcm = [0i16; FRAME_SAMPLES];
        for (ch, sb) in sb.iter().enumerate().take(hdr.nb_channels) {
            for (i, sb) in sb.iter().enumerate() {
                self.synth.filter(ch, &mut self.dither_state, &mut pcm[32 * i..32 * i + 32], sb);
            }
            planes.push(pcm.iter().flat_map(|s| s.to_le_bytes()).collect());
        }
        Frame::Audio(AudioFrame { samples: FRAME_SAMPLES as u32, pts: None, data: planes })
    }
}

impl Decoder for Mp1Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: SampleFormat::S16P,
            sample_rate: self.sample_rate,
            channels: self.channels as u16,
        })
    }

    /// decode.c's loop over `decode_frame`: frames until the packet's bytes
    /// are taken. As FFmpeg, leading zero bytes are skipped, an ID3v1 tag
    /// ends the packet, and an invalid or free-format header drops the rest
    /// of the packet with an error. Unlike FFmpeg's decoder (one for all
    /// three layers), Layer II and III frames are refused, and so is a
    /// frame whose rate or channel count is not the stream's.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut data: &[u8] = &packet.data;
        let mut pts = packet.pts;
        while !data.is_empty() {
            let skipped = data.iter().take_while(|&&b| b == 0).count();
            let buf = &data[skipped..];
            let [a, b, c, d, ..] = *buf else { return Err(invalid("packet too small")) };
            let word = u32::from_be_bytes([a, b, c, d]);
            if word >> 8 == u32::from_be_bytes([0, b'T', b'A', b'G']) {
                break;
            }
            let Some(hdr) = header::decode(word) else { return Err(invalid("header missing")) };
            let Some(frame_size) = hdr.frame_size else { return Err(invalid("free-format frame")) };
            if hdr.layer != 1 {
                return Err(Error::unsupported(format!("mp1: a Layer {} frame", hdr.layer)));
            }
            if hdr.sample_rate != self.sample_rate || hdr.nb_channels != self.channels {
                return Err(invalid(&format!(
                    "a {} Hz {}-channel frame in a {} Hz {}-channel stream",
                    hdr.sample_rate, hdr.nb_channels, self.sample_rate, self.channels
                )));
            }
            let size = buf.len().min(frame_size);
            let mut frame = self.decode_frame(&hdr, buf, size);
            if let Frame::Audio(a) = &mut frame {
                a.pts = pts.take();
            }
            self.ready.push_back(frame);
            data = &buf[size..];
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.ready.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// `mp_flush`: the synthesis history and the dither state start again
    /// (the ring buffer's position stays, as in FFmpeg).
    fn reset(&mut self) -> Result<()> {
        self.synth.synth_buf = [[0; 1024]; 2];
        self.dither_state = 0;
        self.ready.clear();
        Ok(())
    }
}
