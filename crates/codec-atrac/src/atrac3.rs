// Port of FFmpeg's ATRAC3 and ATRAC3 AL decoders (libavcodec/atrac3.c,
// atrac3data.h, FFmpeg commit 2da55bf).
// Copyright (c) 2006-2008 Maxim Poliakovski, (c) 2006-2008 Benjamin
// Larsson; LGPL-2.1-or-later (see LICENSE).

use std::sync::LazyLock;

use oxideav_core::{CodecParameters, Decoder, Error, Result};

use crate::atrac3_tables::*;
use crate::bits::BitReader;
use crate::common::{GainContext, GainInfo, QmfDelay, SF_TABLE, iqmf};
use crate::frames::{AudioDecoder, FrameCodec, Planes, block_align, channels};
use crate::tx::Imdct;
use crate::vlc::Vlc;

const MAX_CHANNELS: u16 = 8;
const MAX_JS_PAIRS: usize = 8 / 2;
const JOINT_STEREO: u32 = 0x12;
const SINGLE: u32 = 0x2;
const SAMPLES_PER_FRAME: usize = 1024;
const MDCT_SIZE: usize = 512;
const ATRAC3_VLC_BITS: u32 = 8;

/// `mdct_window` (`init_imdct_window`).
static MDCT_WINDOW: LazyLock<[f32; MDCT_SIZE]> = LazyLock::new(|| {
    let mut w = [0f32; MDCT_SIZE];
    let pi = std::f64::consts::PI;
    for (i, j) in (0..128).zip((128..256).rev()) {
        // `float wi = sin(...) + 1.0;`: the sum is rounded, in double.
        let wi = ((((i as f64 + 0.5) / 256.0 - 0.5) * pi).sin() + 1.0) as f32;
        let wj = ((((j as f64 + 0.5) / 256.0 - 0.5) * pi).sin() + 1.0) as f32;
        // `0.5 * (wi * wi + wj * wj)`: float products and sum, double half.
        let w_ = (0.5 * f64::from(wi * wi + wj * wj)) as f32;
        let (a, b) = (wi / w_, wj / w_);
        w[i] = a;
        w[511 - i] = a;
        w[j] = b;
        w[511 - j] = b;
    }
    w
});

/// `spectral_coeff_tab`: the 7 spectral VLCs.
static SPECTRAL_VLC: LazyLock<Vec<Vlc>> = LazyLock::new(|| {
    let mut tabs = Vec::with_capacity(7);
    let mut at = 0;
    for &size in &HUFF_TAB_SIZES {
        let entries = &HUFFTABS[at..at + size];
        let lens: Vec<u8> = entries.iter().map(|e| e[1]).collect();
        let syms: Vec<i32> = entries.iter().map(|e| i32::from(e[0]) - 31).collect();
        tabs.push(Vlc::from_lengths(ATRAC3_VLC_BITS, &lens, &syms));
        at += size;
    }
    tabs
});

#[derive(Clone, Copy, Default)]
struct TonalComponent {
    pos: usize,
    num_coefs: usize,
    coef: [f32; 8],
}

/// `ChannelUnit`.
struct ChannelUnit {
    num_components: usize,
    prev_frame: [f32; SAMPLES_PER_FRAME],
    gc_blk_switch: usize,
    components: [TonalComponent; 64],
    gain_block: [[GainInfo; 4]; 2],
    spectrum: [f32; SAMPLES_PER_FRAME],
    imdct_buf: [f32; MDCT_SIZE],
    delay_buf1: QmfDelay,
    delay_buf2: QmfDelay,
    delay_buf3: QmfDelay,
}

impl ChannelUnit {
    fn new() -> Box<Self> {
        Box::new(Self {
            num_components: 0,
            prev_frame: [0.0; SAMPLES_PER_FRAME],
            gc_blk_switch: 0,
            components: [TonalComponent::default(); 64],
            gain_block: [[GainInfo::default(); 4]; 2],
            spectrum: [0.0; SAMPLES_PER_FRAME],
            imdct_buf: [0.0; MDCT_SIZE],
            delay_buf1: [0.0; 46],
            delay_buf2: [0.0; 46],
            delay_buf3: [0.0; 46],
        })
    }
}

/// `ATRAC3Context`.
struct Atrac3 {
    channels: usize,
    block_align: usize,
    coding_mode: u32,
    scrambled_stream: bool,
    /// ATRAC3 AL: a packet is one frame of any size.
    al: bool,
    units: Vec<Box<ChannelUnit>>,
    matrix_coeff_index_prev: [[usize; 4]; MAX_JS_PAIRS],
    matrix_coeff_index_now: [[usize; 4]; MAX_JS_PAIRS],
    matrix_coeff_index_next: [[usize; 4]; MAX_JS_PAIRS],
    weighting_delay: [[i32; 6]; MAX_JS_PAIRS],
    decoded_bytes: Vec<u8>,
    reversed: Vec<u8>,
    gainc: GainContext,
    mdct: Imdct,
}

/// `read_quant_spectral_coeffs`.
fn read_quant_spectral_coeffs(
    gb: &mut BitReader,
    selector: usize,
    coding_flag: u32,
    mantissas: &mut [i32],
    num_codes: usize,
) {
    let num_codes = if selector == 1 {
        num_codes / 2
    } else {
        num_codes
    };
    if coding_flag != 0 {
        // constant length coding (CLC)
        let num_bits = CLC_LENGTH_TAB[selector];
        if selector > 1 {
            for m in mantissas.iter_mut().take(num_codes) {
                *m = if num_bits != 0 { gb.get_s(num_bits) } else { 0 };
            }
        } else {
            for i in 0..num_codes {
                let code = if num_bits != 0 {
                    gb.get(num_bits) as usize
                } else {
                    0
                };
                mantissas[i * 2] = MANTISSA_CLC_TAB[code >> 2];
                mantissas[i * 2 + 1] = MANTISSA_CLC_TAB[code & 3];
            }
        }
    } else {
        // variable length coding (VLC)
        let vlc = &SPECTRAL_VLC[selector - 1];
        if selector != 1 {
            for m in mantissas.iter_mut().take(num_codes) {
                *m = vlc.get(gb);
            }
        } else {
            for i in 0..num_codes {
                let symb = usize::try_from(vlc.get(gb)).unwrap_or(0).min(8);
                mantissas[i * 2] = MANTISSA_VLC_TAB[symb * 2];
                mantissas[i * 2 + 1] = MANTISSA_VLC_TAB[symb * 2 + 1];
            }
        }
    }
}

/// `decode_spectrum`: the number of coded subbands minus one.
fn decode_spectrum(gb: &mut BitReader, output: &mut [f32; SAMPLES_PER_FRAME]) -> usize {
    let num_subbands = gb.get(5) as usize;
    let coding_mode = gb.get1();
    let mut subband_vlc_index = [0usize; 32];
    let mut sf_index = [0usize; 32];
    let mut mantissas = [0i32; 128];
    let sf_table = &*SF_TABLE;

    for v in subband_vlc_index.iter_mut().take(num_subbands + 1) {
        *v = gb.get(3) as usize;
    }
    for i in 0..=num_subbands {
        if subband_vlc_index[i] != 0 {
            sf_index[i] = gb.get(6) as usize;
        }
    }
    for i in 0..=num_subbands {
        let (first, last) = (SUBBAND_TAB[i], SUBBAND_TAB[i + 1]);
        let selector = subband_vlc_index[i];
        if selector != 0 {
            read_quant_spectral_coeffs(gb, selector, coding_mode, &mut mantissas, last - first);
            let scale_factor = sf_table[sf_index[i]] * INV_MAX_QUANT[selector];
            for (o, &m) in output[first..last].iter_mut().zip(&mantissas) {
                *o = m as f32 * scale_factor;
            }
        } else {
            output[first..last].fill(0.0);
        }
    }
    output[SUBBAND_TAB[num_subbands + 1]..].fill(0.0);
    num_subbands
}

/// `decode_tonal_components`: the number of components.
fn decode_tonal_components(
    gb: &mut BitReader,
    components: &mut [TonalComponent; 64],
    num_bands: usize,
) -> Result<usize> {
    let mut component_count = 0usize;
    let nb_components = gb.get(5);
    if nb_components == 0 {
        return Ok(0);
    }
    let coding_mode_selector = gb.get(2);
    if coding_mode_selector == 2 {
        return Err(Error::invalid("atrac3: invalid tonal coding mode"));
    }
    let mut coding_mode = coding_mode_selector & 1;
    let sf_table = &*SF_TABLE;

    for _ in 0..nb_components {
        let mut band_flags = [0u32; 4];
        for f in band_flags.iter_mut().take(num_bands + 1) {
            *f = gb.get1();
        }
        let coded_values_per_component = gb.get(3) as usize;
        let quant_step_index = gb.get(3) as usize;
        if quant_step_index <= 1 {
            return Err(Error::invalid("atrac3: invalid tonal quantizer"));
        }
        if coding_mode_selector == 3 {
            coding_mode = gb.get1();
        }
        for b in 0..(num_bands + 1) * 4 {
            if band_flags[b >> 2] == 0 {
                continue;
            }
            let coded_components = gb.get(3);
            for _ in 0..coded_components {
                let sf_index = gb.get(6) as usize;
                if component_count >= 64 {
                    return Err(Error::invalid("atrac3: too many tonal components"));
                }
                let cmp = &mut components[component_count];
                cmp.pos = b * 64 + gb.get(6) as usize;
                let max_coded_values = SAMPLES_PER_FRAME - cmp.pos;
                let coded_values = (coded_values_per_component + 1).min(max_coded_values);
                let scale_factor = sf_table[sf_index] * INV_MAX_QUANT[quant_step_index];
                let mut mantissa = [0i32; 8];
                read_quant_spectral_coeffs(
                    gb,
                    quant_step_index,
                    coding_mode,
                    &mut mantissa,
                    coded_values,
                );
                cmp.num_coefs = coded_values;
                for (c, &m) in cmp.coef.iter_mut().zip(&mantissa).take(coded_values) {
                    *c = m as f32 * scale_factor;
                }
                component_count += 1;
            }
        }
    }
    Ok(component_count)
}

/// `decode_gain_control`.
fn decode_gain_control(
    gb: &mut BitReader,
    block: &mut [GainInfo; 4],
    num_bands: usize,
) -> Result<()> {
    for gain in block.iter_mut().take(num_bands + 1) {
        gain.num_points = gb.geti(3);
        for j in 0..gain.num_points as usize {
            gain.lev_code[j] = gb.geti(4);
            gain.loc_code[j] = gb.geti(5);
            if j > 0 && gain.loc_code[j] <= gain.loc_code[j - 1] {
                return Err(Error::invalid("atrac3: invalid gain location"));
            }
        }
    }
    for gain in block.iter_mut().skip(num_bands + 1) {
        gain.num_points = 0;
    }
    Ok(())
}

/// `add_tonal_components`: the end of the last component, or -1.
fn add_tonal_components(
    spectrum: &mut [f32; SAMPLES_PER_FRAME],
    components: &[TonalComponent],
) -> i32 {
    let mut last_pos = -1i32;
    for c in components {
        last_pos = last_pos.max((c.pos + c.num_coefs) as i32);
        for (o, &v) in spectrum[c.pos..c.pos + c.num_coefs].iter_mut().zip(&c.coef) {
            *o += v;
        }
    }
    last_pos
}

fn interpolate(old: f32, new: f32, nsample: usize) -> f64 {
    f64::from(old) + nsample as f64 * 0.125 * f64::from(new - old)
}

/// `reverse_matrixing`.
fn reverse_matrixing(
    su1: &mut [f32],
    su2: &mut [f32],
    prev_code: &[usize; 4],
    curr_code: &[usize; 4],
) {
    for (i, band) in (0..4 * 256).step_by(256).enumerate() {
        let (s1, s2) = (prev_code[i], curr_code[i]);
        let mut nsample = band;
        if s1 != s2 {
            // selector changed: interpolate over the first 8 samples
            let (mc1_l, mc1_r) = (MATRIX_COEFFS[s1 * 2], MATRIX_COEFFS[s1 * 2 + 1]);
            let (mc2_l, mc2_r) = (MATRIX_COEFFS[s2 * 2], MATRIX_COEFFS[s2 * 2 + 1]);
            while nsample < band + 8 {
                let c1 = su1[nsample];
                let c2 = su2[nsample];
                let t = nsample - band;
                let c2 = (f64::from(c1) * interpolate(mc1_l, mc2_l, t)
                    + f64::from(c2) * interpolate(mc1_r, mc2_r, t)) as f32;
                su1[nsample] = c2;
                su2[nsample] = (f64::from(c1) * 2.0 - f64::from(c2)) as f32;
                nsample += 1;
            }
        }
        match s2 {
            0 => {
                // M/S decoding
                for n in nsample..band + 256 {
                    let (c1, c2) = (su1[n], su2[n]);
                    su1[n] = c2 * 2.0;
                    su2[n] = (c1 - c2) * 2.0;
                }
            }
            1 => {
                for n in nsample..band + 256 {
                    let (c1, c2) = (su1[n], su2[n]);
                    su1[n] = (c1 + c2) * 2.0;
                    su2[n] = c2 * -2.0;
                }
            }
            _ => {
                for n in nsample..band + 256 {
                    let (c1, c2) = (su1[n], su2[n]);
                    su1[n] = c1 + c2;
                    su2[n] = c1 - c2;
                }
            }
        }
    }
}

/// `get_channel_weights`.
fn get_channel_weights(index: i32, flag: i32) -> [f32; 2] {
    if index == 7 {
        return [1.0, 1.0];
    }
    let ch0 = (f64::from(index & 7) / 7.0) as f32;
    let ch1 = f64::from(2.0f32 - ch0 * ch0).sqrt() as f32;
    if flag != 0 { [ch1, ch0] } else { [ch0, ch1] }
}

/// `channel_weighting`.
fn channel_weighting(su1: &mut [f32], su2: &mut [f32], p3: &[i32; 6]) {
    if p3[1] == 7 && p3[3] == 7 {
        return;
    }
    let w0 = get_channel_weights(p3[1], p3[0]);
    let w1 = get_channel_weights(p3[3], p3[2]);
    for band in (256..4 * 256).step_by(256) {
        for n in band..band + 8 {
            su1[n] = (f64::from(su1[n]) * interpolate(w0[0], w0[1], n - band)) as f32;
            su2[n] = (f64::from(su2[n]) * interpolate(w1[0], w1[1], n - band)) as f32;
        }
        for n in band + 8..band + 256 {
            su1[n] *= w1[0];
            su2[n] *= w1[1];
        }
    }
}

/// Copies the inputs aside so the filter can write over them.
fn iqmf_in_place(
    buf: &mut [f32],
    lo: std::ops::Range<usize>,
    hi: std::ops::Range<usize>,
    out_at: usize,
    delay: &mut QmfDelay,
) {
    let n = lo.len();
    let mut temp = [0f32; 2 * SAMPLES_PER_FRAME];
    temp[..n].copy_from_slice(&buf[lo]);
    temp[n..2 * n].copy_from_slice(&buf[hi]);
    let (inlo, inhi) = temp[..2 * n].split_at(n);
    iqmf(inlo, inhi, n, &mut buf[out_at..out_at + 2 * n], delay);
}

impl Atrac3 {
    /// `imlt`: inverse transform and windowing of one 256-coefficient band.
    fn imlt(mdct: &Imdct, input: &mut [f32], output: &mut [f32; MDCT_SIZE], odd_band: bool) {
        if odd_band {
            // the odd bands are reversed before the transform
            input[..256].reverse();
        }
        mdct.full(output, &input[..256]);
        for (o, &w) in output.iter_mut().zip(MDCT_WINDOW.iter()) {
            *o *= w;
        }
    }

    /// `decode_channel_sound_unit`.
    fn decode_channel_sound_unit(
        &mut self,
        gb: &mut BitReader,
        ch: usize,
        output: &mut [f32],
        channel_num: usize,
        coding_mode: u32,
    ) -> Result<()> {
        let unit = &mut *self.units[ch];
        let (g1, g2) = (unit.gc_blk_switch, 1 - unit.gc_blk_switch);

        if coding_mode == JOINT_STEREO && channel_num % 2 == 1 {
            if gb.get(2) != 3 {
                return Err(Error::invalid("atrac3: JS mono sound unit id != 3"));
            }
        } else if gb.get(6) != 0x28 {
            return Err(Error::invalid("atrac3: sound unit id != 0x28"));
        }

        // number of coded QMF bands
        let bands_coded = gb.get(2) as usize;
        decode_gain_control(gb, &mut unit.gain_block[g2], bands_coded)?;
        unit.num_components = decode_tonal_components(gb, &mut unit.components, bands_coded)?;
        let num_subbands = decode_spectrum(gb, &mut unit.spectrum);

        // merge the decoded spectrum and the tonal components
        let last_tonal =
            add_tonal_components(&mut unit.spectrum, &unit.components[..unit.num_components]);

        // number of used MLT/QMF bands from the coded spectral lines
        let mut num_bands = ((SUBBAND_TAB[num_subbands + 1] - 1) >> 8) as i32;
        if last_tonal >= 0 {
            num_bands = num_bands.max((last_tonal + 256) >> 8);
        }

        for band in 0..4 {
            if band as i32 <= num_bands {
                Self::imlt(
                    &self.mdct,
                    &mut unit.spectrum[band * 256..band * 256 + 256],
                    &mut unit.imdct_buf,
                    band & 1 != 0,
                );
            } else {
                unit.imdct_buf.fill(0.0);
            }
            // gain compensation and overlapping
            let (now, next) = (unit.gain_block[g1][band], unit.gain_block[g2][band]);
            self.gainc.compensate(
                &unit.imdct_buf,
                &mut unit.prev_frame[band * 256..band * 256 + 256],
                &now,
                &next,
                256,
                &mut output[band * 256..band * 256 + 256],
            );
        }
        unit.gc_blk_switch ^= 1;
        Ok(())
    }

    /// `decode_frame`.
    fn decode_frame(&mut self, databuf: &[u8], out: &mut Planes) -> Result<()> {
        let channels = self.channels;
        if self.coding_mode == JOINT_STEREO {
            // channel pairs, each jointly coded
            let js_block_align = (self.block_align / channels) * 2;
            for ch in (0..channels).step_by(2) {
                let js_pair = ch / 2;
                let js_databuf = &databuf[js_pair * js_block_align..(js_pair + 1) * js_block_align];

                let mut gb = BitReader::new(js_databuf, js_block_align * 8);
                let (left, right) = out.split_at_mut(ch + 1);
                self.decode_channel_sound_unit(&mut gb, ch, &mut left[ch], ch, JOINT_STEREO)?;

                // the second sound unit is coded in reverse byte order
                let mut reversed = std::mem::take(&mut self.reversed);
                reversed.clear();
                reversed.extend(js_databuf.iter().rev());
                // skip the sync codes (0xF8)
                let mut p = 0usize;
                let mut i = 4usize;
                while reversed.get(p) == Some(&0xF8) {
                    if i >= js_block_align {
                        self.reversed = reversed;
                        return Err(Error::invalid("atrac3: no second sound unit"));
                    }
                    i += 1;
                    p += 1;
                }
                let mut gb = BitReader::from_bytes(&reversed[p..]);

                // weighting coefficients delay buffer
                let wd = &mut self.weighting_delay[js_pair];
                wd.copy_within(2..6, 0);
                wd[4] = gb.geti(1);
                wd[5] = gb.geti(3);
                for i in 0..4 {
                    self.matrix_coeff_index_prev[js_pair][i] =
                        self.matrix_coeff_index_now[js_pair][i];
                    self.matrix_coeff_index_now[js_pair][i] =
                        self.matrix_coeff_index_next[js_pair][i];
                    self.matrix_coeff_index_next[js_pair][i] = gb.get(2) as usize;
                }

                let result = self.decode_channel_sound_unit(
                    &mut gb,
                    ch + 1,
                    &mut right[0],
                    ch + 1,
                    JOINT_STEREO,
                );
                self.reversed = reversed;
                result?;

                // reconstruct the channel coefficients
                reverse_matrixing(
                    &mut left[ch],
                    &mut right[0],
                    &self.matrix_coeff_index_prev[js_pair],
                    &self.matrix_coeff_index_now[js_pair],
                );
                channel_weighting(&mut left[ch], &mut right[0], &self.weighting_delay[js_pair]);
            }
        } else {
            // single channels
            for i in 0..channels {
                let start = i * self.block_align / channels;
                let mut gb = BitReader::new(&databuf[start..], self.block_align * 8 / channels);
                let coding_mode = self.coding_mode;
                self.decode_channel_sound_unit(&mut gb, i, &mut out[i], i, coding_mode)?;
            }
        }
        self.synthesize(out);
        Ok(())
    }

    /// `al_decode_frame`: the sound units one after another, each found by
    /// its sync code.
    fn al_decode_frame(&mut self, data: &[u8], out: &mut Planes) -> Result<()> {
        let mut gb = BitReader::from_bytes(data);
        for i in 0..self.channels {
            let coding_mode = self.coding_mode;
            self.decode_channel_sound_unit(&mut gb, i, &mut out[i], i, coding_mode)?;
            while gb.left() > 6 && gb.show(6) != 0x28 {
                gb.skip(1);
            }
        }
        self.synthesize(out);
        Ok(())
    }

    /// The iQMF synthesis filters of every channel.
    fn synthesize(&mut self, out: &mut Planes) {
        for (unit, p) in self.units.iter_mut().zip(out.iter_mut()) {
            iqmf_in_place(p, 0..256, 256..512, 0, &mut unit.delay_buf1);
            iqmf_in_place(p, 768..1024, 512..768, 512, &mut unit.delay_buf2);
            iqmf_in_place(p, 0..512, 512..1024, 0, &mut unit.delay_buf3);
        }
    }
}

impl FrameCodec for Atrac3 {
    /// `atrac3_decode_frame` / `atrac3al_decode_frame`.
    fn decode(&mut self, data: &[u8]) -> Result<(usize, Option<Planes>)> {
        let mut out = vec![vec![0f32; SAMPLES_PER_FRAME]; self.channels];
        if self.al {
            self.al_decode_frame(data, &mut out)?;
            return Ok((data.len(), Some(out)));
        }
        if data.len() < self.block_align {
            return Err(Error::invalid(format!(
                "atrac3: frame too small ({} bytes)",
                data.len()
            )));
        }
        if self.scrambled_stream {
            // `decode_bytes`: XOR with 0x537F6103 (big-endian)
            let mut buf = std::mem::take(&mut self.decoded_bytes);
            const KEY: [u8; 4] = [0x53, 0x7F, 0x61, 0x03];
            for (i, (o, &b)) in buf.iter_mut().zip(&data[..self.block_align]).enumerate() {
                *o = b ^ KEY[i & 3];
            }
            let result = self.decode_frame(&buf, &mut out);
            self.decoded_bytes = buf;
            result?;
        } else {
            self.decode_frame(&data[..self.block_align], &mut out)?;
        }
        Ok((self.block_align, Some(out)))
    }
}

fn be16(d: &[u8], at: usize) -> u32 {
    u32::from(u16::from_be_bytes([d[at], d[at + 1]]))
}

fn le16(d: &[u8], at: usize) -> u32 {
    u32::from(u16::from_le_bytes([d[at], d[at + 1]]))
}

/// `atrac3_decode_init`.
fn init(params: &CodecParameters, al: bool) -> Result<Box<dyn FrameCodec>> {
    let channels = channels(params, MAX_CHANNELS)?;
    let ed = &params.extradata;
    let block_align = block_align(params);
    let (version, samples_per_frame, delay, coding_mode, scrambled);
    if al {
        (version, samples_per_frame, delay, coding_mode, scrambled) =
            (4, SAMPLES_PER_FRAME * channels, 0x88E, SINGLE, false);
    } else if ed.len() == 14 {
        // WAV format
        let frame_factor = le16(ed, 10) as usize;
        coding_mode = if le16(ed, 6) != 0 {
            JOINT_STEREO
        } else {
            SINGLE
        };
        (version, samples_per_frame, delay, scrambled) =
            (4, SAMPLES_PER_FRAME * channels, 0x88E, false);
        let ba = block_align.unwrap_or(0);
        if ba != 96 * channels * frame_factor
            && ba != 152 * channels * frame_factor
            && ba != 192 * channels * frame_factor
        {
            return Err(Error::invalid(format!(
                "atrac3: unknown frame/channel/frame_factor configuration {ba}/{channels}/{frame_factor}"
            )));
        }
    } else if ed.len() == 12 || ed.len() == 10 {
        // RM format
        version = u32::from_be_bytes([ed[0], ed[1], ed[2], ed[3]]);
        samples_per_frame = be16(ed, 4) as usize;
        delay = be16(ed, 6);
        coding_mode = be16(ed, 8);
        scrambled = true;
    } else {
        return Err(Error::invalid(format!(
            "atrac3: unknown extradata size {}",
            ed.len()
        )));
    }

    if version != 4 {
        return Err(Error::invalid(format!("atrac3: version {version} != 4")));
    }
    if samples_per_frame != SAMPLES_PER_FRAME * channels {
        return Err(Error::invalid(format!(
            "atrac3: unknown samples per frame {samples_per_frame}"
        )));
    }
    if delay != 0x88E {
        return Err(Error::invalid(format!("atrac3: unknown delay {delay:x}")));
    }
    match coding_mode {
        SINGLE => {}
        JOINT_STEREO if channels % 2 == 0 => {}
        JOINT_STEREO => {
            return Err(Error::invalid(
                "atrac3: joint stereo needs an even channel count",
            ));
        }
        other => {
            return Err(Error::invalid(format!(
                "atrac3: unknown channel coding mode {other:x}"
            )));
        }
    }
    let block_align = match block_align {
        Some(ba) if ba <= 4096 => ba,
        _ => return Err(Error::invalid("atrac3: block align unknown or above 4096")),
    };

    Ok(Box::new(Atrac3 {
        channels,
        block_align,
        coding_mode,
        scrambled_stream: scrambled,
        al,
        units: (0..channels).map(|_| ChannelUnit::new()).collect(),
        matrix_coeff_index_prev: [[3; 4]; MAX_JS_PAIRS],
        matrix_coeff_index_now: [[3; 4]; MAX_JS_PAIRS],
        matrix_coeff_index_next: [[3; 4]; MAX_JS_PAIRS],
        weighting_delay: [[0, 7, 0, 7, 0, 7]; MAX_JS_PAIRS],
        decoded_bytes: vec![0; block_align],
        reversed: Vec::with_capacity(block_align),
        gainc: GainContext::new(4, 3),
        mdct: Imdct::new(256, 1.0 / 32768.0),
    }))
}

fn make_atrac3(params: &CodecParameters) -> Result<Box<dyn FrameCodec>> {
    init(params, false)
}

fn make_atrac3al(params: &CodecParameters) -> Result<Box<dyn FrameCodec>> {
    init(params, true)
}

pub(crate) fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    AudioDecoder::open(params, make_atrac3)
}

pub(crate) fn make_al_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    AudioDecoder::open(params, make_atrac3al)
}
