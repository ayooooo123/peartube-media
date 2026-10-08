// Ported from FFmpeg libavcodec/apedec.c (commit 2da55bf).
//
// Copyright (c) 2007 Benjamin Zores <ben@geexbox.org>
//   based upon libdemac from Dave Chapman.
// Copyright (c) FFmpeg developers
//
// This file is part of FFmpeg.
// Licensed under the GNU Lesser General Public License 2.1 or later.

use std::collections::VecDeque;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result,
    SampleFormat,
};

use crate::entropy::{
    ape_decode_value_3860, ape_decode_value_3900, ape_decode_value_3990, decode_array_0000,
    APERangecoder, APERice, BitReader,
};
use crate::filter::{APEFilter, APE_FILTER_LEVELS};
use crate::predictor::{
    init_predictor_decoder, predictor_decode_mono_3800, predictor_decode_mono_3930,
    predictor_decode_mono_3950, predictor_decode_stereo_3800, predictor_decode_stereo_3930,
    predictor_decode_stereo_3950, APEPredictor, APEPredictor64,
};

pub const APE_FRAMECODE_MONO_SILENCE: i32 = 1;
pub const APE_FRAMECODE_STEREO_SILENCE: i32 = 3;
pub const APE_FRAMECODE_PSEUDO_STEREO: i32 = 4;

/// Monkey's Audio (APE) audio decoder.
pub struct ApeDecoder {
    codec_id: CodecId,
    channels: usize,
    bps: usize,
    sample_rate: u32,
    fileversion: i32,
    compression_level: i32,
    fset: usize,
    #[allow(dead_code)]
    flags: i32,
    interim_mode: i32,
    blocks_per_loop: usize,
    filters: [[APEFilter; 2]; APE_FILTER_LEVELS],
    predictor: APEPredictor,
    predictor64: APEPredictor64,
    interim: [Vec<i32>; 2],
    pending_frames: VecDeque<Frame>,
    eof: bool,
}

impl ApeDecoder {
    fn reset_filters(&mut self) {
        for f in &mut self.filters {
            f[0].reset();
            f[1].reset();
        }
    }

    fn init_predictors(&mut self) {
        init_predictor_decoder(
            self.fileversion,
            self.compression_level,
            &mut self.predictor,
            &mut self.predictor64,
        );
    }

    fn decode_packet_data(&mut self, packet: &Packet) {
        let data = &packet.data;
        if data.len() < 8 {
            return;
        }

        let mut buf_size = data.len() & !3;
        if self.fileversion < 3950 {
            buf_size += 2;
        }

        let mut swapped = vec![0u8; buf_size];
        crate::dsp::bswap_buf(&mut swapped, data);

        let mut ptr = 0usize;
        let nblocks = u32::from_be_bytes(swapped[ptr..ptr + 4].try_into().unwrap());
        ptr += 4;
        let offset = u32::from_be_bytes(swapped[ptr..ptr + 4].try_into().unwrap());
        ptr += 4;

        if !self.setup_bitstream_offset(offset, &mut ptr, buf_size) {
            return;
        }

        const FFMPEG_MAX_BLOCKS: u32 = 268_435_447; // INT_MAX / 2 / 4 - 8
        const FORMAT_MAX_BLOCKS: u32 = 73728 * 16;   // format-level cap
        if nblocks == 0 || nblocks > FFMPEG_MAX_BLOCKS || nblocks > FORMAT_MAX_BLOCKS {
            return;
        }

        let mut gb = BitReader::new(&swapped[ptr..buf_size]);
        if self.fileversion < 3900 {
            if self.fileversion > 3800 {
                gb.skip_bits((offset as usize) * 8);
            } else {
                gb.skip_bits(offset as usize);
            }
        }
        let mut rc = APERangecoder::new();
        let mut rice_x = APERice::default();
        let mut rice_y = APERice::default();
        let mut frameflags = 0i32;

        if !self.init_frame_entropy(
            &swapped,
            &mut ptr,
            buf_size,
            &mut gb,
            &mut rc,
            &mut rice_x,
            &mut rice_y,
            &mut frameflags,
        ) {
            return;
        }

        self.init_predictors();
        self.reset_filters();

        self.decode_blocks_loop(
            packet,
            nblocks as usize,
            frameflags,
            &swapped,
            &mut ptr,
            &mut gb,
            &mut rc,
            &mut rice_x,
            &mut rice_y,
        );
    }

    fn setup_bitstream_offset(&self, offset: u32, ptr: &mut usize, buf_size: usize) -> bool {
        if self.fileversion >= 3900 {
            if offset > 3 || buf_size - *ptr < offset as usize {
                return false;
            }
            *ptr += offset as usize;
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn init_frame_entropy(
        &self,
        swapped: &[u8],
        ptr: &mut usize,
        buf_size: usize,
        gb: &mut BitReader,
        rc: &mut APERangecoder,
        rice_x: &mut APERice,
        rice_y: &mut APERice,
        frameflags: &mut i32,
    ) -> bool {
        let crc: u32 = if self.fileversion >= 3900 {
            if buf_size.saturating_sub(*ptr) < 6 {
                return false;
            }
            let val = u32::from_be_bytes(swapped[*ptr..*ptr + 4].try_into().unwrap());
            *ptr += 4;
            val
        } else {
            gb.get_bits(32)
        };

        *frameflags = 0;
        if self.fileversion > 3820 && (crc & 0x8000_0000) != 0 {
            if buf_size.saturating_sub(*ptr) < 6 {
                return false;
            }
            *frameflags = i32::from_be_bytes(swapped[*ptr..*ptr + 4].try_into().unwrap());
            *ptr += 4;
        }

        rice_x.k = 10;
        rice_x.ksum = (1 << rice_x.k) * 16;
        rice_y.k = 10;
        rice_y.ksum = (1 << rice_y.k) * 16;

        if self.fileversion >= 3900 {
            *ptr += 1;
            rc.start_decoding(ptr, swapped);
        }

        true
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_blocks_loop(
        &mut self,
        packet: &Packet,
        nblocks: usize,
        frameflags: i32,
        swapped: &[u8],
        ptr: &mut usize,
        gb: &mut BitReader,
        rc: &mut APERangecoder,
        rice_x: &mut APERice,
        rice_y: &mut APERice,
    ) {
        let mut samples_left = nblocks;
        let mut first_chunk = true;

        while samples_left > 0 {
            let blockstodecode = if self.fileversion < 3930 {
                samples_left
            } else {
                samples_left.min(self.blocks_per_loop)
            };

            let mut decoded0 = vec![0i32; blockstodecode];
            let mut decoded1 = vec![0i32; blockstodecode];
            let mut error = false;

            if self.channels == 1 || (frameflags & APE_FRAMECODE_PSEUDO_STEREO) != 0 {
                if (frameflags & APE_FRAMECODE_STEREO_SILENCE) != 0 {
                    // silence
                } else {
                    self.entropy_decode_mono(
                        &mut decoded0,
                        blockstodecode,
                        swapped,
                        ptr,
                        gb,
                        rc,
                        rice_y,
                        &mut error,
                    );
                    if error {
                        break;
                    }
                    self.predictor_decode_mono(&mut decoded0, blockstodecode);
                    if self.channels == 2 {
                        decoded1.copy_from_slice(&decoded0);
                    }
                }
            } else if (frameflags & APE_FRAMECODE_STEREO_SILENCE) == APE_FRAMECODE_STEREO_SILENCE {
                // silence
            } else {
                self.entropy_decode_stereo(
                    &mut decoded0,
                    &mut decoded1,
                    blockstodecode,
                    swapped,
                    ptr,
                    gb,
                    rc,
                    rice_x,
                    rice_y,
                    &mut error,
                );
                if error {
                    break;
                }
                self.predictor_decode_stereo(&mut decoded0, &mut decoded1, blockstodecode);

                // Decorrelate
                for i in 0..blockstodecode {
                    let y = decoded0[i];
                    let x = decoded1[i];
                    let left = (x as u32).wrapping_sub((y / 2) as u32) as i32;
                    let right = (left as u32).wrapping_add(y as u32) as i32;
                    decoded0[i] = left;
                    decoded1[i] = right;
                }
            }

            let mut planes = Vec::with_capacity(self.channels);
            match self.bps {
                8 => {
                    for ch in 0..self.channels {
                        let src = if ch == 0 { &decoded0 } else { &decoded1 };
                        let mut plane = Vec::with_capacity(blockstodecode);
                        for &val in src {
                            plane.push((val.wrapping_add(0x80) & 0xff) as u8);
                        }
                        planes.push(plane);
                    }
                }
                16 => {
                    for ch in 0..self.channels {
                        let src = if ch == 0 { &decoded0 } else { &decoded1 };
                        let mut plane = Vec::with_capacity(blockstodecode * 2);
                        for &val in src {
                            plane.extend_from_slice(&(val as i16).to_le_bytes());
                        }
                        planes.push(plane);
                    }
                }
                24 => {
                    for ch in 0..self.channels {
                        let src = if ch == 0 { &decoded0 } else { &decoded1 };
                        let mut plane = Vec::with_capacity(blockstodecode * 4);
                        for &val in src {
                            let scaled = val.wrapping_mul(256);
                            plane.extend_from_slice(&scaled.to_le_bytes());
                        }
                        planes.push(plane);
                    }
                }
                _ => break,
            }

            let frame_pts = if first_chunk { packet.pts } else { None };
            first_chunk = false;

            self.pending_frames.push_back(Frame::Audio(AudioFrame {
                samples: blockstodecode as u32,
                pts: frame_pts,
                data: planes,
            }));

            samples_left -= blockstodecode;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn entropy_decode_mono(
        &self,
        decoded0: &mut [i32],
        count: usize,
        swapped: &[u8],
        ptr: &mut usize,
        gb: &mut BitReader,
        rc: &mut APERangecoder,
        rice_y: &mut APERice,
        error: &mut bool,
    ) {
        if self.fileversion < 3860 {
            decode_array_0000(gb, decoded0, rice_y, count, error);
        } else if self.fileversion < 3900 {
            for sample in decoded0.iter_mut() {
                *sample = ape_decode_value_3860(self.fileversion, gb, rice_y, error);
            }
        } else if self.fileversion < 3990 {
            for sample in decoded0.iter_mut() {
                *sample = ape_decode_value_3900(self.fileversion, rc, ptr, swapped, rice_y, error);
            }
        } else {
            for sample in decoded0.iter_mut() {
                *sample = ape_decode_value_3990(rc, ptr, swapped, rice_y, error);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn entropy_decode_stereo(
        &self,
        decoded0: &mut [i32],
        decoded1: &mut [i32],
        count: usize,
        swapped: &[u8],
        ptr: &mut usize,
        gb: &mut BitReader,
        rc: &mut APERangecoder,
        rice_x: &mut APERice,
        rice_y: &mut APERice,
        error: &mut bool,
    ) {
        if self.fileversion < 3860 {
            decode_array_0000(gb, decoded0, rice_y, count, error);
            decode_array_0000(gb, decoded1, rice_x, count, error);
        } else if self.fileversion < 3900 {
            for sample in decoded0.iter_mut() {
                *sample = ape_decode_value_3860(self.fileversion, gb, rice_y, error);
            }
            for sample in decoded1.iter_mut() {
                *sample = ape_decode_value_3860(self.fileversion, gb, rice_x, error);
            }
        } else if self.fileversion < 3930 {
            for sample in decoded0.iter_mut() {
                *sample = ape_decode_value_3900(self.fileversion, rc, ptr, swapped, rice_y, error);
            }
            rc.dec_normalize(ptr, swapped, error);
            if *ptr > 0 {
                *ptr -= 1;
            }
            rc.start_decoding(ptr, swapped);
            for sample in decoded1.iter_mut() {
                *sample = ape_decode_value_3900(self.fileversion, rc, ptr, swapped, rice_x, error);
            }
        } else if self.fileversion < 3990 {
            for i in 0..count {
                decoded0[i] = ape_decode_value_3900(self.fileversion, rc, ptr, swapped, rice_y, error);
                decoded1[i] = ape_decode_value_3900(self.fileversion, rc, ptr, swapped, rice_x, error);
            }
        } else {
            for i in 0..count {
                decoded0[i] = ape_decode_value_3990(rc, ptr, swapped, rice_y, error);
                decoded1[i] = ape_decode_value_3990(rc, ptr, swapped, rice_x, error);
            }
        }
    }

    fn predictor_decode_mono(&mut self, decoded0: &mut [i32], count: usize) {
        if self.fileversion < 3930 {
            predictor_decode_mono_3800(
                self.fileversion,
                self.compression_level,
                &mut self.predictor,
                decoded0,
                count,
            );
        } else if self.fileversion < 3950 {
            predictor_decode_mono_3930(
                self.fileversion,
                self.fset,
                &mut self.filters,
                &mut self.predictor,
                decoded0,
                count,
            );
        } else {
            predictor_decode_mono_3950(
                self.fileversion,
                self.fset,
                &mut self.filters,
                &mut self.predictor64,
                decoded0,
                count,
            );
        }
    }

    fn predictor_decode_stereo(
        &mut self,
        decoded0: &mut [i32],
        decoded1: &mut [i32],
        count: usize,
    ) {
        if self.fileversion < 3930 {
            predictor_decode_stereo_3800(
                self.fileversion,
                self.compression_level,
                &mut self.predictor,
                decoded0,
                decoded1,
                count,
            );
        } else if self.fileversion < 3950 {
            predictor_decode_stereo_3930(
                self.fileversion,
                self.fset,
                &mut self.filters,
                &mut self.predictor,
                decoded0,
                decoded1,
                count,
            );
        } else {
            predictor_decode_stereo_3950(
                self.fileversion,
                self.fset,
                &mut self.filters,
                &mut self.predictor64,
                &mut self.interim_mode,
                &mut self.interim,
                decoded0,
                decoded1,
                count,
            );
        }
    }
}

impl Decoder for ApeDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if !packet.data.is_empty() {
            self.decode_packet_data(packet);
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.pending_frames.pop_front() {
            return Ok(frame);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::NeedMore)
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.pending_frames.clear();
        self.eof = false;
        self.init_predictors();
        self.reset_filters();
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: match self.bps {
                8 => SampleFormat::U8P,
                16 => SampleFormat::S16P,
                24 => SampleFormat::S32P,
                _ => return None,
            },
            sample_rate: self.sample_rate,
            channels: self.channels as u16,
        })
    }
}

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let extradata = &params.extradata;
    if extradata.len() < 6 {
        return Err(Error::invalid("ape: invalid extradata size"));
    }
    let fileversion = u16::from_le_bytes([extradata[0], extradata[1]]) as i32;
    let compression_level = u16::from_le_bytes([extradata[2], extradata[3]]) as i32;
    let flags = u16::from_le_bytes([extradata[4], extradata[5]]) as i32;

    if fileversion < 3800 || fileversion > 3990 {
        return Err(Error::invalid(format!("ape: unsupported version {fileversion}")));
    }
    if compression_level % 1000 != 0
        || compression_level > 5000
        || compression_level == 0
        || (fileversion < 3930 && compression_level == 5000)
    {
        return Err(Error::invalid(format!("ape: invalid compression level {compression_level}")));
    }

    let channels = params.channels.unwrap_or(2) as usize;
    if channels == 0 || channels > 2 {
        return Err(Error::invalid(format!("ape: unsupported channel count {channels}")));
    }

    let bps = match params.sample_format {
        Some(SampleFormat::U8 | SampleFormat::U8P) => 8,
        Some(SampleFormat::S16 | SampleFormat::S16P) => 16,
        Some(SampleFormat::S24 | SampleFormat::S32 | SampleFormat::S32P) => 24,
        _ => {
            if (flags & 1) != 0 {
                8
            } else if (flags & 8) != 0 {
                24
            } else {
                16
            }
        }
    };

    let sample_rate = params.sample_rate.unwrap_or(44100);
    let fset = (compression_level / 1000 - 1) as usize;

    let filters = std::array::from_fn(|i| {
        let order = crate::filter::APE_FILTER_ORDERS[fset][i];
        [APEFilter::new(order), APEFilter::new(order)]
    });

    let interim_mode = if bps == 24 { -1 } else { 0 };

    Ok(Box::new(ApeDecoder {
        codec_id: CodecId::new("ape"),
        channels,
        bps,
        sample_rate,
        fileversion,
        compression_level,
        fset,
        flags,
        interim_mode,
        blocks_per_loop: 4608,
        filters,
        predictor: APEPredictor::default(),
        predictor64: APEPredictor64::default(),
        interim: [Vec::new(), Vec::new()],
        pending_frames: VecDeque::new(),
        eof: false,
    }))
}
