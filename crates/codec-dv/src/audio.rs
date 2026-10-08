// Ported from FFmpeg (commit 2da55bf): libavcodec/dvaudiodec.c (decode_init,
// dv_audio_12to16, decode_frame) and libavcodec/dvaudio.h
// (dv_get_audio_sample_count); libavformat/dv.c has the same
// dv_audio_12to16.
// License: LGPL-2.1-or-later

//! The `dvaudio` decoder ("Ulead DV audio", WAVE format tags 0x0215 and
//! 0x0216 in WAV and AVI): each block holds the audio DIF blocks of one DV
//! frame end to end (90 of 80 bytes for 525/60, 108 for 625/50), decoded
//! to interleaved 16-bit stereo.
//!
//! FFmpeg takes 12-bit samples from the container's bits per coded sample,
//! which OxideAV's WAV and AVI demuxers do not pass on; here each block's
//! AAUX source pack says (its quantisation field), which agrees with the
//! container for every file whose header and data agree.

use std::collections::VecDeque;

use oxideav_core::{AudioFormat, AudioFrame, CodecId, CodecParameters, CodecTag, Decoder, Error, Frame, Packet, Result, SampleFormat};

/// dv_audio_12to16: a 12-bit nonlinear sample (IEC 61834) as 16-bit linear.
pub(crate) fn audio_12to16(sample: u16) -> u16 {
    let sample = if sample < 0x800 { sample } else { sample | 0xf000 };
    let shift = (sample & 0xf00) >> 8;
    if !(0x2..=0xd).contains(&shift) {
        sample
    } else if shift < 0x8 {
        let shift = shift - 1;
        sample.wrapping_sub(256 * shift) << shift
    } else {
        let shift = 0xe - shift;
        (sample.wrapping_add(256 * shift + 1) << shift).wrapping_sub(1)
    }
}

/// The DV audio sample rates by the AAUX frequency field.
const FREQUENCY: [u32; 3] = [48_000, 44_100, 32_000];
/// The AAUX source pack's data: audio DIF block 3, byte 4.
const AS_PACK: usize = 244;

/// dv_get_audio_sample_count: the samples of a frame by its AAUX source
/// pack (at `pack`), PAL or not.
fn sample_count(pack: &[u8], pal: bool) -> usize {
    let samples = usize::from(pack[0] & 0x3f);
    samples
        + match (pack[3] >> 3) & 0x07 {
            0 if pal => 1896,
            0 => 1580,
            1 if pal => 1742,
            1 => 1452,
            _ if pal => 1264,
            _ => 1053,
        }
}

/// The dvaudio decoder.
pub struct DvAudioDecoder {
    codec_id: CodecId,
    block_size: usize,
    is_pal: bool,
    /// The container's sample rate, else the first block's.
    sample_rate: Option<u32>,
    ready: VecDeque<AudioFrame>,
    flushed: bool,
}

impl DvAudioDecoder {
    /// decode_init: the block size by the WAVE tag, else the container's
    /// block_align.
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let block_align = params.options.get("block_align").and_then(|v| v.parse::<usize>().ok());
        let block_size = match params.tag {
            Some(CodecTag::WaveFormat(0x0215)) => 7200,
            Some(CodecTag::WaveFormat(0x0216)) => 8640,
            _ => match block_align {
                Some(b @ (7200 | 8640)) => b,
                _ => return Err(Error::invalid("dvaudio: block size is neither 7200 nor 8640")),
            },
        };
        Ok(Self {
            codec_id: CodecId::new("dvaudio"),
            block_size,
            is_pal: block_size == 8640,
            sample_rate: params.sample_rate.filter(|&r| r > 0),
            ready: VecDeque::new(),
            flushed: false,
        })
    }

    /// decode_frame on the block at the start of `src` (the rest of the
    /// packet after it, which FFmpeg's reads past the block can reach).
    fn decode_block(&mut self, src: &[u8], pts: Option<i64>) -> AudioFrame {
        let byte = |k: usize| src.get(k).copied().unwrap_or(0);
        let pack = [byte(AS_PACK), byte(AS_PACK + 1), byte(AS_PACK + 2), byte(AS_PACK + 3)];
        let is_12bit = pack[3] & 0x07 == 1;
        if self.sample_rate.is_none() {
            self.sample_rate = FREQUENCY.get(usize::from((pack[3] >> 3) & 0x07)).copied();
        }
        let samples = sample_count(&pack, self.is_pal);
        let a = if self.is_pal { 18 } else { 15 };
        let b = 3 * a;
        let second = if self.is_pal { 4320 } else { 3600 };
        let mut out = Vec::with_capacity(samples * 4);
        for i in 0..samples {
            // the shuffle table of decode_init
            let v = 80 * ((21 * (i % 3) + 9 * (i / 3) + ((i / a) % 3)) % b) + (2 + usize::from(is_12bit)) * (i / b) + 8;
            let (l, r) = if is_12bit {
                let (v0, v1, v2) = (u16::from(byte(v)), u16::from(byte(v + 1)), u16::from(byte(v + 2)));
                (audio_12to16((v0 << 4) | (v2 >> 4)), audio_12to16((v1 << 4) | (v2 & 0x0f)))
            } else {
                (u16::from_be_bytes([byte(v), byte(v + 1)]), u16::from_be_bytes([byte(v + second), byte(v + second + 1)]))
            };
            out.extend_from_slice(&l.to_le_bytes());
            out.extend_from_slice(&r.to_le_bytes());
        }
        AudioFrame { samples: samples as u32, pts, data: vec![out] }
    }
}

impl Decoder for DvAudioDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    /// Each whole block of the packet is a frame, as FFmpeg decodes a
    /// packet block by block; a remainder shorter than a block is an error.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let data = &packet.data;
        if data.len() < self.block_size {
            return Err(Error::invalid("dvaudio: packet shorter than a block"));
        }
        let mut pts = packet.pts;
        let mut at = 0;
        while data.len() - at >= self.block_size {
            let frame = self.decode_block(&data[at..], pts);
            pts = None;
            self.ready.push_back(frame);
            at += self.block_size;
        }
        if at < data.len() {
            return Err(Error::invalid("dvaudio: packet ends inside a block"));
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.ready.pop_front() {
            Some(frame) => Ok(Frame::Audio(frame)),
            None if self.flushed => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.flushed = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.ready.clear();
        self.flushed = false;
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        self.sample_rate.map(|sample_rate| AudioFormat { sample_format: SampleFormat::S16, sample_rate, channels: 2 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 12-bit samples at each segment boundary expand as FFmpeg's C
    /// dv_audio_12to16 expands them (values from that function).
    #[test]
    fn twelve_bit_samples_expand_as_ffmpeg() {
        let want = [
            (0x000, 0x0000),
            (0x1ff, 0x01ff),
            (0x200, 0x0200),
            (0x2ff, 0x03fe),
            (0x300, 0x0400),
            (0x7ff, 0x7fc0),
            (0x800, 0x803f),
            (0x801, 0x807f),
            (0xcff, 0xfbff),
            (0xd00, 0xfc01),
            (0xdff, 0xfdff),
            (0xe00, 0xfe00),
            (0xfff, 0xffff),
        ];
        for (sample, expanded) in want {
            assert_eq!(audio_12to16(sample), expanded, "0x{sample:03x}");
        }
    }
}
