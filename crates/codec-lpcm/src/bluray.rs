// Blu-ray LPCM decoder.
//
// Ported from FFmpeg libavcodec/pcm-bluray.c (commit 2da55bf), Copyright (c)
// 2009, 2013 Christian Schmidt, LGPL-2.1-or-later.

//! `pcm_bluray`: each packet starts with a 4-byte header (channel
//! assignment, sample rate, bit depth), then big-endian samples of an even
//! number of channels: a layout with an odd count carries an empty one, and
//! 5.1, 7.0 and 7.1 come in Blu-ray order (L R C LS RS LFE; L R C LS Rls
//! Rrs RS), which FFmpeg reorders to its own. Output is interleaved S16
//! (16-bit) or S32 (24-bit, MSB-aligned); 20-bit is refused, as FFmpeg does.

use oxideav_core::{AudioFormat, AudioFrame, CodecId, Decoder, Error, Frame, Packet, Result, SampleFormat};

/// Per channel assignment (header byte 2, high nibble): the channels FFmpeg
/// outputs, and the source channel each comes from. Reserved ones are empty.
const LAYOUTS: [&[usize]; 16] = [
    &[],
    &[0],                      // mono
    &[],
    &[0, 1],                   // stereo
    &[0, 1, 2],                // 3/0
    &[0, 1, 2],                // 2/1
    &[0, 1, 2, 3],             // 3/1
    &[0, 1, 2, 3],             // 2/2
    &[0, 1, 2, 3, 4],          // 3/2
    &[0, 1, 2, 5, 3, 4],       // 3/2+lfe: L R C LFE LS RS
    &[0, 1, 2, 4, 5, 3, 6],    // 3/4: L R C BL BR SL SR
    &[0, 1, 2, 7, 4, 5, 3, 6], // 3/4+lfe: L R C LFE BL BR SL SR
    &[],
    &[],
    &[],
    &[],
];

pub struct PcmBlurayDecoder {
    codec_id: CodecId,
    /// Header-derived layout: `None` until the first packet.
    format: Option<(u32, u32, usize)>,
    pending: Option<Frame>,
    eof: bool,
}

pub fn make_decoder(params: &oxideav_core::CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(PcmBlurayDecoder { codec_id: params.codec_id.clone(), format: None, pending: None, eof: false }))
}

impl PcmBlurayDecoder {
    /// `pcm_bluray_parse_header`: bits, rate and the channel assignment.
    fn parse_header(header: &[u8]) -> Result<(u32, u32, usize)> {
        let bits = [0, 16, 20, 24][usize::from(header[3] >> 6)];
        if bits != 16 && bits != 24 {
            return Err(Error::invalid(format!("pcm_bluray: unsupported sample depth ({bits})")));
        }
        let rate = match header[2] & 0x0f {
            1 => 48_000,
            4 => 96_000,
            5 => 192_000,
            other => return Err(Error::invalid(format!("pcm_bluray: reserved sample rate ({other})"))),
        };
        let assignment = usize::from(header[2] >> 4);
        if LAYOUTS[assignment].is_empty() {
            return Err(Error::invalid(format!("pcm_bluray: reserved channel configuration ({assignment})")));
        }
        Ok((bits, rate, assignment))
    }

    /// `pcm_bluray_decode_frame`: samples per channel and the interleaved
    /// output.
    fn decode(&mut self, packet: &[u8]) -> Result<(usize, Vec<u8>)> {
        if packet.len() < 4 {
            return Err(Error::invalid("pcm_bluray: packet too small"));
        }
        let (bits, rate, assignment) = Self::parse_header(&packet[..4])?;
        self.format = Some((bits, rate, assignment));
        let map = LAYOUTS[assignment];
        // There's always an even number of channels in the source.
        let source = map.len().next_multiple_of(2);
        let width = bits as usize / 8;
        let frame = source * width;
        let src = &packet[4..];
        let samples = src.len() / frame;
        let mut out = Vec::with_capacity(samples * map.len() * if bits == 16 { 2 } else { 4 });
        for f in src.chunks_exact(frame).take(samples) {
            for &c in map {
                let s = &f[c * width..(c + 1) * width];
                if bits == 16 {
                    out.extend_from_slice(&[s[1], s[0]]);
                } else {
                    out.extend_from_slice(&i32::from_be_bytes([s[0], s[1], s[2], 0]).to_le_bytes());
                }
            }
        }
        Ok((samples, out))
    }
}

impl Decoder for PcmBlurayDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.pending.is_some() {
            return Err(Error::other("pcm_bluray: call receive_frame before sending another packet"));
        }
        let (samples, data) = self.decode(&packet.data)?;
        if samples > 0 {
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
        self.pending = None;
        self.eof = false;
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        let (bits, sample_rate, assignment) = self.format?;
        Some(AudioFormat {
            sample_format: if bits == 16 { SampleFormat::S16 } else { SampleFormat::S32 },
            sample_rate,
            channels: LAYOUTS[assignment].len() as u16,
        })
    }
}
