// DVD-Video LPCM decoder.
//
// Ported from FFmpeg libavcodec/pcm-dvd.c (commit 2da55bf), Copyright (c) 2013
// Christian Schmidt, LGPL-2.1-or-later.

//! `pcm_dvd`: each packet starts with the 3-byte LPCM header of the DVD
//! private stream (the MPEG-PS reader drops the 3 bytes before it, as
//! FFmpeg's does), then big-endian samples: 16-bit ones as they are, 20- and
//! 24-bit ones in groups of four whose low bits follow the four high words.
//! A group can straddle packets; its start waits for the next packet.
//! Output is interleaved S16 (16-bit) or S32 (20/24-bit, MSB-aligned).

use oxideav_core::{AudioFormat, AudioFrame, CodecId, Decoder, Error, Frame, Packet, Result, SampleFormat};

/// No traces of 44100 and 32000 Hz in any commercial software or player
/// (FFmpeg's comment), but the header can say so.
const FREQUENCIES: [u32; 4] = [48_000, 96_000, 44_100, 32_000];

pub struct PcmDvdDecoder {
    codec_id: CodecId,
    /// The header bits that matter (frame number masked off); `None`
    /// forces parsing.
    last_header: Option<u32>,
    bits: u32,
    channels: usize,
    sample_rate: u32,
    block_size: usize,
    last_block_size: usize,
    samples_per_block: usize,
    groups_per_block: usize,
    /// A block's start from the previous packet (8 channels × 3 bytes × 4).
    extra: Vec<u8>,
    pending: Option<Frame>,
    eof: bool,
}

pub fn make_decoder(params: &oxideav_core::CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(PcmDvdDecoder {
        codec_id: params.codec_id.clone(),
        last_header: None,
        bits: 16,
        channels: usize::from(params.channels.unwrap_or(2).max(1)),
        sample_rate: params.sample_rate.unwrap_or(48_000),
        block_size: 0,
        last_block_size: 0,
        samples_per_block: 0,
        groups_per_block: 0,
        extra: Vec::with_capacity(8 * 3 * 4),
        pending: None,
        eof: false,
    }))
}

impl PcmDvdDecoder {
    /// `pcm_dvd_parse_header`.
    fn parse_header(&mut self, header: &[u8]) -> Result<()> {
        let header_int = u32::from(header[0] & 0xe0) | u32::from(header[1]) << 8 | u32::from(header[2]) << 16;
        // Early exit if the header didn't change apart from the frame number.
        if self.last_header == Some(header_int) {
            return Ok(());
        }
        self.last_header = None;
        // Discard potentially existing leftover samples from old channel layout.
        self.extra.clear();
        let bits = 16 + u32::from(header[1] >> 6 & 3) * 4;
        if bits == 28 {
            return Err(Error::invalid(format!("pcm_dvd: unsupported sample depth {bits}")));
        }
        self.bits = bits;
        self.sample_rate = FREQUENCIES[usize::from(header[1] >> 4 & 3)];
        let channels = 1 + usize::from(header[1] & 7);
        self.channels = channels;
        // 4 samples form a group in 20/24-bit PCM on DVD Video. A block is
        // formed by the number of groups that are needed to complete a set
        // of samples for each channel.
        (self.block_size, self.samples_per_block, self.groups_per_block) = if bits == 16 {
            (channels * 2, 1, 0)
        } else {
            match channels {
                // One group has all the samples needed.
                1 | 2 | 4 => (4 * bits as usize / 8, 4 / channels, 1),
                // Two groups have all the samples needed.
                8 => (8 * bits as usize / 8, 1, 2),
                // Need `channels` groups.
                _ => (4 * channels * bits as usize / 8, 4, channels),
            }
        };
        self.last_header = Some(header_int);
        Ok(())
    }

    /// `pcm_dvd_decode_samples`: `blocks` whole blocks of `src` onto `out`.
    fn decode_samples(&self, src: &[u8], blocks: usize, out: &mut Vec<u8>) {
        if self.bits == 16 {
            for pair in src[..blocks * self.block_size].chunks_exact(2) {
                out.extend_from_slice(&[pair[1], pair[0]]);
            }
            return;
        }
        // 20 or 24 bits: per group, the high words of its samples (two for
        // mono, four otherwise), then their low bits (a nibble or a byte
        // each).
        let (groups, per_group) =
            if self.channels == 1 { (2 * blocks, 2) } else { (self.groups_per_block * blocks, 4) };
        let low = if self.bits == 20 { per_group / 2 } else { per_group };
        let mut at = 0;
        for _ in 0..groups {
            let high = &src[at..at + 2 * per_group];
            let tail = &src[at + 2 * per_group..at + 2 * per_group + low];
            at += 2 * per_group + low;
            for k in 0..per_group {
                let word = u32::from(u16::from_be_bytes([high[2 * k], high[2 * k + 1]])) << 16;
                let low_bits = if self.bits == 20 {
                    let t = u32::from(tail[k / 2]);
                    if k % 2 == 0 {
                        (t & 0xf0) << 8
                    } else {
                        (t & 0x0f) << 12
                    }
                } else {
                    u32::from(tail[k]) << 8
                };
                out.extend_from_slice(&(word.wrapping_add(low_bits) as i32).to_le_bytes());
            }
        }
    }

    /// `pcm_dvd_decode_frame`: the samples per channel, or `None` when the
    /// packet only adds to a block that is still short.
    fn decode(&mut self, packet: &[u8]) -> Result<Option<(usize, Vec<u8>)>> {
        if packet.len() < 3 {
            return Err(Error::invalid("pcm_dvd: packet too small"));
        }
        self.parse_header(&packet[..3])?;
        if self.last_block_size != 0 && self.last_block_size != self.block_size {
            self.extra.clear();
        }
        self.last_block_size = self.block_size;
        let mut src = &packet[3..];
        let mut blocks = (src.len() + self.extra.len()) / self.block_size;
        let samples = blocks * self.samples_per_block;
        let mut out = Vec::with_capacity(samples * self.channels * if self.bits == 16 { 2 } else { 4 });
        // Consume leftover samples from the last packet.
        if !self.extra.is_empty() {
            let missing = self.block_size - self.extra.len();
            if src.len() >= missing {
                self.extra.extend_from_slice(&src[..missing]);
                self.decode_samples(&self.extra, 1, &mut out);
                self.extra.clear();
                src = &src[missing..];
                blocks -= 1;
            } else {
                // The new packet still doesn't complete a block.
                self.extra.extend_from_slice(src);
                return Ok(None);
            }
        }
        // Decode the remaining complete blocks.
        let whole = blocks * self.block_size;
        self.decode_samples(&src[..whole], blocks, &mut out);
        // Store leftover samples.
        self.extra.extend_from_slice(&src[whole..]);
        Ok((samples > 0).then_some((samples, out)))
    }
}

impl Decoder for PcmDvdDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.pending.is_some() {
            return Err(Error::other("pcm_dvd: call receive_frame before sending another packet"));
        }
        if let Some((samples, data)) = self.decode(&packet.data)? {
            self.pending =
                Some(Frame::Audio(AudioFrame { samples: samples as u32, pts: packet.pts, data: vec![data] }));
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.pending.take() {
            Some(frame) => Ok(frame),
            None if self.eof => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        // FFmpeg has no flush for pcm_dvd: a seek keeps the header and any
        // leftover bytes. Dropping the leftovers starts the next packet
        // on its own block boundary.
        self.extra.clear();
        self.pending = None;
        self.eof = false;
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        self.last_header?;
        Some(AudioFormat {
            sample_format: if self.bits == 16 { SampleFormat::S16 } else { SampleFormat::S32 },
            sample_rate: self.sample_rate,
            channels: self.channels as u16,
        })
    }
}
