// Port of FFmpeg's ATRAC3+ and ATRAC3+ AL decoders (libavcodec/atrac3plus.h,
// atrac3plusdec.c, FFmpeg commit 2da55bf).
// Copyright (c) 2010-2013 Maxim Poliakovski; LGPL-2.1-or-later (see LICENSE).

mod dsp;
mod parser;
#[rustfmt::skip]
mod tables;

use std::sync::LazyLock;

use oxideav_core::{CodecParameters, Decoder, Error, Result};

use crate::bits::BitReader;
use crate::common::{GainContext, GainInfo};
use crate::frames::{AudioDecoder, FrameCodec, Planes, block_align, channels};
use crate::tx::Imdct;
use crate::vlc::Vlc;
use tables::*;

pub(crate) const SUBBANDS: usize = 16;
pub(crate) const SUBBAND_SAMPLES: usize = 128;
pub(crate) const FRAME_SAMPLES: usize = SUBBAND_SAMPLES * SUBBANDS;
pub(crate) const PQF_FIR_LEN: usize = 12;
pub(crate) const POWER_COMP_OFF: u8 = 15;

pub(crate) const CH_UNIT_MONO: u32 = 0;
pub(crate) const CH_UNIT_STEREO: u32 = 1;
pub(crate) const CH_UNIT_EXTENSION: u32 = 2;
pub(crate) const CH_UNIT_TERMINATOR: u32 = 3;

/// `Atrac3pIPQFChannelCtx`.
#[derive(Clone, Copy)]
pub(crate) struct IpqfChannel {
    buf1: [[f32; 8]; PQF_FIR_LEN * 2],
    buf2: [[f32; 8]; PQF_FIR_LEN * 2],
    pos: usize,
}

/// `Atrac3pWaveEnvelope`.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct WaveEnvelope {
    pub has_start_point: bool,
    pub has_stop_point: bool,
    pub start_pos: i32,
    pub stop_pos: i32,
}

/// `Atrac3pWavesData`.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct WavesData {
    pub pend_env: WaveEnvelope,
    pub curr_env: WaveEnvelope,
    pub num_wavs: i32,
    pub start_index: i32,
}

/// `Atrac3pWaveParam`.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct WaveParam {
    pub freq_index: i32,
    pub amp_sf: i32,
    pub amp_index: i32,
    pub phase_index: i32,
}

/// `Atrac3pChanParams`. FFmpeg's current/previous pointer pairs into the
/// two-frame histories are an index (`cur`; the previous is `1 - cur`).
#[derive(Clone)]
pub(crate) struct ChanParams {
    pub ch_num: usize,
    pub num_coded_vals: i32,
    pub fill_mode: i32,
    pub split_point: i32,
    pub table_type: i32,
    pub qu_wordlen: [i32; 32],
    pub qu_sf_idx: [i32; 32],
    pub qu_tab_idx: [i32; 32],
    pub spectrum: [i16; 2048],
    pub power_levs: [u8; 5],
    pub wnd_shape_hist: [[u8; SUBBANDS]; 2],
    pub gain_data_hist: [[GainInfo; SUBBANDS]; 2],
    pub num_gain_subbands: i32,
    pub tones_info_hist: [[WavesData; SUBBANDS]; 2],
    pub cur: usize,
}

impl ChanParams {
    fn new(ch_num: usize) -> Self {
        Self {
            ch_num,
            num_coded_vals: 0,
            fill_mode: 0,
            split_point: 0,
            table_type: 0,
            qu_wordlen: [0; 32],
            qu_sf_idx: [0; 32],
            qu_tab_idx: [0; 32],
            spectrum: [0; 2048],
            power_levs: [0; 5],
            wnd_shape_hist: [[0; SUBBANDS]; 2],
            gain_data_hist: [[GainInfo::default(); SUBBANDS]; 2],
            num_gain_subbands: 0,
            tones_info_hist: [[WavesData::default(); SUBBANDS]; 2],
            cur: 0,
        }
    }

    pub fn wnd_shape(&self) -> &[u8; SUBBANDS] {
        &self.wnd_shape_hist[self.cur]
    }

    pub fn wnd_shape_prev(&self) -> &[u8; SUBBANDS] {
        &self.wnd_shape_hist[1 - self.cur]
    }

    pub fn gain_data(&self) -> &[GainInfo; SUBBANDS] {
        &self.gain_data_hist[self.cur]
    }

    pub fn gain_data_mut(&mut self) -> &mut [GainInfo; SUBBANDS] {
        &mut self.gain_data_hist[self.cur]
    }

    pub fn gain_data_prev(&self) -> &[GainInfo; SUBBANDS] {
        &self.gain_data_hist[1 - self.cur]
    }

    pub fn tones_info(&self) -> &[WavesData; SUBBANDS] {
        &self.tones_info_hist[self.cur]
    }

    pub fn tones_info_mut(&mut self) -> &mut [WavesData; SUBBANDS] {
        &mut self.tones_info_hist[self.cur]
    }

    pub fn tones_info_prev(&self) -> &[WavesData; SUBBANDS] {
        &self.tones_info_hist[1 - self.cur]
    }
}

/// `Atrac3pWaveSynthParams`.
#[derive(Clone, Copy)]
pub(crate) struct WaveSynthParams {
    pub tones_present: bool,
    pub amplitude_mode: i32,
    pub num_tone_bands: i32,
    pub tone_sharing: [u8; SUBBANDS],
    pub tone_master: [u8; SUBBANDS],
    pub invert_phase: [u8; SUBBANDS],
    pub tones_index: i32,
    pub waves: [WaveParam; 48],
}

impl Default for WaveSynthParams {
    fn default() -> Self {
        Self {
            tones_present: false,
            amplitude_mode: 0,
            num_tone_bands: 0,
            tone_sharing: [0; SUBBANDS],
            tone_master: [0; SUBBANDS],
            invert_phase: [0; SUBBANDS],
            tones_index: 0,
            waves: [WaveParam::default(); 48],
        }
    }
}

/// `Atrac3pChanUnitCtx`.
pub(crate) struct ChanUnit {
    pub unit_type: u32,
    pub num_quant_units: i32,
    pub num_subbands: i32,
    pub used_quant_units: i32,
    pub num_coded_subbands: i32,
    pub mute_flag: bool,
    pub use_full_table: bool,
    pub noise_present: bool,
    pub noise_level_index: i32,
    pub noise_table_index: i32,
    pub swap_channels: [u8; SUBBANDS],
    pub negate_coeffs: [u8; SUBBANDS],
    pub channels: [ChanParams; 2],
    pub wave_synth_hist: [WaveSynthParams; 2],
    /// Index of `waves_info` in `wave_synth_hist`; `waves_info_prev` is
    /// the other.
    pub waves_cur: usize,
    pub ipqf_ctx: [IpqfChannel; 2],
    pub prev_buf: [[f32; FRAME_SAMPLES]; 2],
}

impl ChanUnit {
    fn new() -> Box<Self> {
        Box::new(Self {
            unit_type: 0,
            num_quant_units: 0,
            num_subbands: 0,
            used_quant_units: 0,
            num_coded_subbands: 0,
            mute_flag: false,
            use_full_table: false,
            noise_present: false,
            noise_level_index: 0,
            noise_table_index: 0,
            swap_channels: [0; SUBBANDS],
            negate_coeffs: [0; SUBBANDS],
            channels: [ChanParams::new(0), ChanParams::new(1)],
            wave_synth_hist: [WaveSynthParams::default(); 2],
            waves_cur: 0,
            ipqf_ctx: [IpqfChannel {
                buf1: [[0.0; 8]; PQF_FIR_LEN * 2],
                buf2: [[0.0; 8]; PQF_FIR_LEN * 2],
                pos: 0,
            }; 2],
            prev_buf: [[0.0; FRAME_SAMPLES]; 2],
        })
    }

    pub fn waves_info(&self) -> &WaveSynthParams {
        &self.wave_synth_hist[self.waves_cur]
    }

    pub fn waves_info_mut(&mut self) -> &mut WaveSynthParams {
        &mut self.wave_synth_hist[self.waves_cur]
    }

    pub fn waves_info_prev(&self) -> &WaveSynthParams {
        &self.wave_synth_hist[1 - self.waves_cur]
    }
}

/// The VLC tables `ff_atrac3p_init_vlcs` builds.
pub(crate) struct Vlcs {
    pub wl: Vec<Vlc>,
    pub ct: Vec<Vlc>,
    pub sf: Vec<Vlc>,
    spec_store: Vec<Vlc>,
    spec_index: Vec<usize>,
    pub gain: Vec<Vlc>,
    pub tone: Vec<Vlc>,
}

impl Vlcs {
    pub fn spec(&self, tab_index: usize) -> &Vlc {
        &self.spec_store[self.spec_index[tab_index]]
    }
}

/// `build_canonical_huff`: a canonical code from the number of codes of
/// each length 1..=12; symbols are the next entries of `xlat`.
fn build_canonical_huff(cb: &[u8], xlat: &mut &[u8]) -> Vlc {
    let mut bits = Vec::new();
    for (b, &count) in (1u8..=12).zip(cb) {
        bits.extend(std::iter::repeat_n(b, usize::from(count)));
    }
    let max_len = u32::from(*bits.last().expect("non-empty codebook"));
    let (syms, rest) = xlat.split_at(bits.len());
    *xlat = rest;
    let syms: Vec<i32> = syms.iter().map(|&s| i32::from(s)).collect();
    Vlc::from_lengths(max_len, &bits, &syms)
}

pub(crate) static VLCS: LazyLock<Vlcs> = LazyLock::new(|| {
    let mut wl = Vec::new();
    let mut ct = Vec::new();
    let mut xlats: &[u8] = &WL_CT_XLATS;
    for i in 0..4 {
        wl.push(build_canonical_huff(&WL_CBS[i], &mut xlats));
        ct.push(build_canonical_huff(&CT_CBS[i], &mut xlats));
    }
    let mut xlats: &[u8] = &SF_XLATS;
    let sf = SF_CBS
        .iter()
        .map(|cb| build_canonical_huff(cb, &mut xlats))
        .collect();

    // spectrum tables; a negative first count reuses an earlier table
    let mut xlats: &[u8] = &SPECTRA_XLATS;
    let mut spec_store = Vec::new();
    let mut spec_index = Vec::with_capacity(SPECTRA_CBS.len());
    for row in &SPECTRA_CBS {
        if row[0] >= 0 {
            let counts: Vec<u8> = row.iter().map(|&c| c as u8).collect();
            spec_index.push(spec_store.len());
            spec_store.push(build_canonical_huff(&counts, &mut xlats));
        } else {
            let reused = spec_index[usize::from(row[0].unsigned_abs())];
            spec_index.push(reused);
        }
    }
    let mut xlats: &[u8] = &GAIN_XLATS;
    let gain = GAIN_CBS
        .iter()
        .map(|cb| build_canonical_huff(cb, &mut xlats))
        .collect();
    let mut xlats: &[u8] = &TONE_XLATS;
    let tone = TONE_CBS
        .iter()
        .map(|cb| build_canonical_huff(cb, &mut xlats))
        .collect();
    Vlcs {
        wl,
        ct,
        sf,
        spec_store,
        spec_index,
        gain,
        tone,
    }
});

/// `channel_map`: output channel of each decoded channel, per count.
const CHANNEL_MAP: [[usize; 8]; 8] = [
    [0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 0, 0, 0, 0, 0, 0],
    [0, 1, 2, 0, 0, 0, 0, 0],
    [0, 1, 2, 3, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 2, 4, 5, 3, 0, 0],
    [0, 1, 2, 4, 5, 6, 3, 0],
    [0, 1, 2, 4, 5, 6, 7, 3],
];

/// `ATRAC3PContext`.
struct Atrac3p {
    channels: usize,
    block_align: usize,
    /// ATRAC3+ AL consumes the whole packet.
    al: bool,
    samples: Box<[[f32; FRAME_SAMPLES]; 2]>,
    time_buf: Box<[[f32; FRAME_SAMPLES]; 2]>,
    outp_buf: Box<[[f32; FRAME_SAMPLES]; 2]>,
    gainc: GainContext,
    mdct: Imdct,
    ipqf_dct: Imdct,
    ch_units: Vec<Box<ChanUnit>>,
    channel_blocks: Vec<u32>,
    channel_map: [usize; 8],
    dsp: dsp::Tables,
}

impl Atrac3p {
    /// `decode_residual_spectrum`.
    fn decode_residual_spectrum(&mut self, block: usize, num_channels: usize) {
        let unit = &*self.ch_units[block];
        let out = &mut *self.samples;
        if unit.mute_flag {
            for o in out.iter_mut().take(num_channels) {
                o.fill(0.0);
            }
            return;
        }

        // RNG table index of each subband
        let mut sb_rng_index = [0usize; SUBBANDS];
        let mut rng_index = 0i32;
        for qu in 0..unit.used_quant_units as usize {
            rng_index += unit.channels[0].qu_sf_idx[qu] + unit.channels[1].qu_sf_idx[qu];
        }
        for sb in 0..unit.num_coded_subbands as usize {
            sb_rng_index[sb] = (rng_index & 0x3FC) as usize;
            rng_index += 128;
        }

        // inverse quantization and power compensation
        for ch in 0..num_channels {
            out[ch].fill(0.0);
            let chan = &unit.channels[ch];
            for qu in 0..unit.used_quant_units as usize {
                let (start, end) = (
                    usize::from(QU_TO_SPEC_POS[qu]),
                    usize::from(QU_TO_SPEC_POS[qu + 1]),
                );
                let wl = chan.qu_wordlen[qu];
                if wl > 0 {
                    let q = SF_TAB[chan.qu_sf_idx[qu] as usize & 63] * MANT_TAB[wl as usize & 7];
                    for (d, &s) in out[ch][start..end]
                        .iter_mut()
                        .zip(&chan.spectrum[start..end])
                    {
                        *d = f32::from(s) * q;
                    }
                }
            }
            for sb in 0..unit.num_coded_subbands as usize {
                dsp::power_compensation(unit, ch, &mut out[ch], sb_rng_index[sb], sb);
            }
        }

        if unit.unit_type == CH_UNIT_STEREO {
            for sb in 0..unit.num_coded_subbands as usize {
                let range = sb * SUBBAND_SAMPLES..(sb + 1) * SUBBAND_SAMPLES;
                if unit.swap_channels[sb] != 0 {
                    let (a, b) = out.split_at_mut(1);
                    a[0][range.clone()].swap_with_slice(&mut b[0][range.clone()]);
                }
                // flip the coefficients' sign if requested
                if unit.negate_coeffs[sb] != 0 {
                    for v in &mut out[1][range] {
                        *v = -*v;
                    }
                }
            }
        }
    }

    /// `reconstruct_frame`.
    fn reconstruct_frame(&mut self, block: usize, num_channels: usize) {
        let unit = &mut *self.ch_units[block];
        for ch in 0..num_channels {
            let num_subbands = (unit.num_subbands.max(0) as usize).min(SUBBANDS);
            for sb in 0..num_subbands {
                let range = sb * SUBBAND_SAMPLES..(sb + 1) * SUBBAND_SAMPLES;
                let chan = &unit.channels[ch];
                let wind_id = (chan.wnd_shape_prev()[sb] << 1) + chan.wnd_shape()[sb];
                let mut mdct_out = [0f32; 2 * SUBBAND_SAMPLES];
                dsp::imdct(
                    &self.dsp,
                    &self.mdct,
                    &mut self.samples[ch][range.clone()],
                    &mut mdct_out,
                    wind_id,
                    sb,
                );
                // gain compensation and overlapping
                let (now, next) = (chan.gain_data_prev()[sb], chan.gain_data()[sb]);
                self.gainc.compensate(
                    &mdct_out,
                    &mut unit.prev_buf[ch][range.clone()],
                    &now,
                    &next,
                    SUBBAND_SAMPLES,
                    &mut self.time_buf[ch][range],
                );
            }
            // zero the unused subbands in the output and overlap buffers
            unit.prev_buf[ch][num_subbands * SUBBAND_SAMPLES..].fill(0.0);
            self.time_buf[ch][num_subbands * SUBBAND_SAMPLES..].fill(0.0);

            // resynthesize and add the tonal signal
            if unit.waves_info().tones_present || unit.waves_info_prev().tones_present {
                for sb in 0..num_subbands {
                    let chan = &unit.channels[ch];
                    if chan.tones_info()[sb].num_wavs != 0
                        || chan.tones_info_prev()[sb].num_wavs != 0
                    {
                        dsp::generate_tones(
                            &self.dsp,
                            unit,
                            ch,
                            sb,
                            &mut self.time_buf[ch][sb * 128..sb * 128 + 128],
                        );
                    }
                }
            }

            // subband synthesis and output
            dsp::ipqf(
                &self.ipqf_dct,
                &mut unit.ipqf_ctx[ch],
                &self.time_buf[ch],
                &mut self.outp_buf[ch],
            );
        }

        // swap the window shape, gain control and tone buffers
        for chan in unit.channels.iter_mut().take(num_channels) {
            chan.cur ^= 1;
        }
        unit.waves_cur ^= 1;
    }
}

impl FrameCodec for Atrac3p {
    /// `atrac3p_decode_frame`.
    fn decode(&mut self, data: &[u8]) -> Result<(usize, Option<Planes>)> {
        let mut planes = vec![vec![0f32; FRAME_SAMPLES]; self.channels];
        let mut gb = BitReader::from_bytes(data);
        if gb.get1() != 0 {
            return Err(Error::invalid("atrac3plus: invalid start bit"));
        }
        let (mut ch_block, mut out_ch_index) = (0usize, 0usize);
        while gb.left() >= 2 {
            let ch_unit_id = gb.get(2);
            if ch_unit_id == CH_UNIT_TERMINATOR {
                break;
            }
            if ch_unit_id == CH_UNIT_EXTENSION {
                return Err(Error::unsupported("atrac3plus: channel unit extension"));
            }
            if ch_block >= self.channel_blocks.len() || self.channel_blocks[ch_block] != ch_unit_id
            {
                return Err(Error::invalid(
                    "atrac3plus: frame data doesn't match channel configuration",
                ));
            }
            self.ch_units[ch_block].unit_type = ch_unit_id;
            let channels_to_process = ch_unit_id as usize + 1;
            parser::decode_channel_unit(
                &mut gb,
                &mut self.ch_units[ch_block],
                channels_to_process,
            )?;
            self.decode_residual_spectrum(ch_block, channels_to_process);
            self.reconstruct_frame(ch_block, channels_to_process);
            for i in 0..channels_to_process {
                planes[self.channel_map[out_ch_index + i]].copy_from_slice(&self.outp_buf[i]);
            }
            ch_block += 1;
            out_ch_index += channels_to_process;
        }
        let consumed = if self.al {
            data.len()
        } else {
            self.block_align.min(data.len())
        };
        Ok((consumed, Some(planes)))
    }
}

/// `atrac3p_decode_init` with `set_channel_params`.
fn init(params: &CodecParameters, al: bool) -> Result<Box<dyn FrameCodec>> {
    let block_align =
        block_align(params).ok_or_else(|| Error::invalid("atrac3plus: block_align is not set"))?;
    let channels = channels(params, 8)?;
    let channel_blocks = match channels {
        1 => vec![CH_UNIT_MONO],
        2 => vec![CH_UNIT_STEREO],
        3 => vec![CH_UNIT_STEREO, CH_UNIT_MONO],
        4 => vec![CH_UNIT_STEREO, CH_UNIT_MONO, CH_UNIT_MONO],
        6 => vec![CH_UNIT_STEREO, CH_UNIT_MONO, CH_UNIT_STEREO, CH_UNIT_MONO],
        7 => vec![
            CH_UNIT_STEREO,
            CH_UNIT_MONO,
            CH_UNIT_STEREO,
            CH_UNIT_MONO,
            CH_UNIT_MONO,
        ],
        8 => vec![
            CH_UNIT_STEREO,
            CH_UNIT_MONO,
            CH_UNIT_STEREO,
            CH_UNIT_STEREO,
            CH_UNIT_MONO,
        ],
        other => {
            return Err(Error::unsupported(format!(
                "atrac3plus: unsupported channel count {other}"
            )));
        }
    };
    LazyLock::force(&VLCS);
    Ok(Box::new(Atrac3p {
        channels,
        block_align,
        al,
        samples: Box::new([[0.0; FRAME_SAMPLES]; 2]),
        time_buf: Box::new([[0.0; FRAME_SAMPLES]; 2]),
        outp_buf: Box::new([[0.0; FRAME_SAMPLES]; 2]),
        gainc: GainContext::new(6, 2),
        mdct: Imdct::new(128, -1.0),
        ipqf_dct: Imdct::new(16, 32.0 / 32768.0),
        ch_units: channel_blocks.iter().map(|_| ChanUnit::new()).collect(),
        channel_blocks,
        channel_map: CHANNEL_MAP[channels - 1],
        dsp: dsp::Tables::new(),
    }))
}

fn make_atrac3p(params: &CodecParameters) -> Result<Box<dyn FrameCodec>> {
    init(params, false)
}

fn make_atrac3pal(params: &CodecParameters) -> Result<Box<dyn FrameCodec>> {
    init(params, true)
}

pub(crate) fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    AudioDecoder::open(params, make_atrac3p)
}

pub(crate) fn make_al_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    AudioDecoder::open(params, make_atrac3pal)
}
