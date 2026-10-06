// Ported from FFmpeg (commit 2da55bf): libavcodec/wmadec.c, wma.c (ff_wma_init,
// ff_wma_run_level_decode, ff_wma_get_large_val, init_coef_vlc), wma.h
// (WMACodecContext), wmadata.h (coef VLC tables), wma_freqs.c
// (ff_wma_critical_freqs), aactab.c (scalefactor VLC), sinewin.c, and
// libavutil/tx.c float MDCT (AV_TX_FLOAT_MDCT, AV_TX_FULL_IMDCT).
// GNU Lesser General Public License 2.1 or later

//! WMA v1/v2 decoder (`wmav1`, `wmav2`).

use crate::bits::BitReader;
use crate::dsp::sine_window;
use crate::fft::ImdctHalf;
use crate::tables::*;
use crate::wma_common::wma_get_frame_len_bits;
use crate::vlc::VlcTable;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat};

const MAX_CHANNELS: usize = 2;
const BLOCK_NB_SIZES: usize = (BLOCK_MAX_BITS - BLOCK_MIN_BITS + 1) as usize;

/// `pow(10, i / 16.0)` for i in -60..95 (wmadec.c pow_tab).
fn pow_tab(i: i32) -> f32 {
    10f32.powf((i - 60) as f32 / 16.0)
}

/// Whole-decoder state (`WMACodecContext`).
pub struct WmaDecoder {
    codec_id: CodecId,
    version: u32,
    channels: usize,
    sample_rate: u32,
    #[allow(dead_code)]
    bit_rate: u64,
    block_align_len: usize,

    use_bit_reservoir: bool,
    use_variable_block_len: bool,
    use_exp_vlc: bool,
    use_noise_coding: bool,
    byte_offset_bits: u32,

    frame_len: usize,
    frame_len_bits: u32,
    nb_block_sizes: usize,

    exp_vlc: VlcTable,
    hgain_vlc: VlcTable,
    coef_vlc: [VlcTable; 2],
    run_table: [Vec<u16>; 2],
    level_table: [Vec<f32>; 2],
    #[allow(dead_code)]
    int_table: [Vec<u16>; 2],

    #[allow(dead_code)]
    exponent_sizes: [usize; BLOCK_NB_SIZES],
    exponent_bands: [[u16; 25]; BLOCK_NB_SIZES],
    high_band_start: [usize; BLOCK_NB_SIZES],
    coefs_start: usize,
    coefs_end: [usize; BLOCK_NB_SIZES],
    exponent_high_sizes: [usize; BLOCK_NB_SIZES],
    exponent_high_bands: [[usize; HIGH_BAND_MAX_SIZE]; BLOCK_NB_SIZES],

    reset_block_lengths: bool,
    block_len_bits: u32,
    next_block_len_bits: u32,
    prev_block_len_bits: u32,
    block_len: usize,
    block_num: usize,
    block_pos: usize,
    ms_stereo: bool,
    channel_coded: [bool; MAX_CHANNELS],
    exponents_bsize: [u32; MAX_CHANNELS],
    exponents: [[f32; BLOCK_MAX_SIZE]; MAX_CHANNELS],
    exponents_initialized: [bool; MAX_CHANNELS],
    max_exponent: [f32; MAX_CHANNELS],
    coefs1: [[f32; BLOCK_MAX_SIZE]; MAX_CHANNELS],
    coefs: [[f32; BLOCK_MAX_SIZE]; MAX_CHANNELS],
    output: [f32; BLOCK_MAX_SIZE * 2],
    mdcts: Vec<ImdctHalf>,
    windows: Vec<Vec<f32>>,
    frame_out: [[f32; BLOCK_MAX_SIZE * 2]; MAX_CHANNELS],

    last_superframe: Vec<u8>,
    last_bitoffset: usize,
    last_superframe_len: usize,
    eof_done: bool,

    noise_table: [f32; NOISE_TAB_SIZE],
    noise_index: usize,
    noise_mult: f32,
    high_band_coded: [[bool; HIGH_BAND_MAX_SIZE]; MAX_CHANNELS],
    high_band_values: [[i32; HIGH_BAND_MAX_SIZE]; MAX_CHANNELS],

    lsp_cos_table: [f32; BLOCK_MAX_SIZE],
    lsp_pow_e_table: [f32; 256],
    lsp_pow_m_table1: [f32; 1 << LSP_POW_BITS],
    lsp_pow_m_table2: [f32; 1 << LSP_POW_BITS],

    pending: Vec<AudioFrame>,
}

/// `init_coef_vlc` (wma.c): expand tables into run/level tables.
fn init_coef_vlc(vlc_table: &CoefVlcTable) -> Result<(VlcTable, Vec<u16>, Vec<f32>, Vec<u16>)> {
    let n = vlc_table.codes.len();
    let vlc = VlcTable::from_bits_codes(vlc_table.bits, vlc_table.codes, None, 0)?;
    let mut run_table = vec![0u16; n];
    let mut level_table = vec![0f32; n];
    let mut int_table = vec![0u16; n];
    let mut i = 2usize;
    let mut level = 1i32;
    let mut k = 0usize;
    while i < n {
        int_table[k] = i as u16;
        let l = vlc_table.levels[k] as usize;
        for j in 0..l {
            run_table[i] = j as u16;
            level_table[i] = level as f32;
            i += 1;
        }
        level += 1;
        k += 1;
    }
    Ok((vlc, run_table, level_table, int_table))
}

impl WmaDecoder {
    /// `wma_decode_init` + `ff_wma_init` (wmadec.c / wma.c). `version` is 1
    /// for wmav1, 2 for wmav2.
    pub fn new(params: &CodecParameters, version: u32) -> Result<Self> {
        let channels = params.channels.unwrap_or(0) as usize;
        let sample_rate = params.sample_rate.unwrap_or(0);
        let bit_rate = params.bit_rate.unwrap_or(0);
        let block_align = params
            .options
            .get("block_align")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0) as usize;
        if block_align == 0 {
            return Err(Error::invalid("wma: block_align is not set"));
        }
        if sample_rate > 50_000 || channels > MAX_CHANNELS || bit_rate == 0 {
            return Err(Error::invalid("wma: unsupported stream parameters"));
        }

        let extradata = &params.extradata;
        let flags2 = if version == 1 && extradata.len() >= 4 {
            u16::from_le_bytes([extradata[2], extradata[3]]) as u32
        } else if version == 2 && extradata.len() >= 6 {
            u16::from_le_bytes([extradata[4], extradata[5]]) as u32
        } else {
            0
        };

        let mut use_variable_block_len = flags2 & 0x0004 != 0;
        if version == 2 && extradata.len() >= 8 {
            let v = u16::from_le_bytes([extradata[4], extradata[5]]) as u32;
            if v == 0xd && use_variable_block_len {
                use_variable_block_len = false; // issue1503 fix
            }
        }
        let use_exp_vlc = flags2 & 0x0001 != 0;
        let use_bit_reservoir = flags2 & 0x0002 != 0;

        let frame_len_bits = wma_get_frame_len_bits(sample_rate, version, 0);
        let frame_len = 1usize << frame_len_bits;
        let nb_block_sizes = if use_variable_block_len {
            let mut nb = (((flags2 >> 3) & 3) + 1) as usize;
            if bit_rate / channels as u64 >= 32_000 {
                nb += 2;
            }
            let nb_max = (frame_len_bits - BLOCK_MIN_BITS) as usize;
            nb.min(nb_max) + 1
        } else {
            1
        };

        // rate-dependent parameters
        let mut use_noise_coding = true;
        let mut high_freq = sample_rate as f32 * 0.5;
        let mut sample_rate1 = sample_rate as i32;
        if version == 2 {
            sample_rate1 = if sample_rate1 >= 44100 {
                44100
            } else if sample_rate1 >= 22050 {
                22050
            } else if sample_rate1 >= 16000 {
                16000
            } else if sample_rate1 >= 11025 {
                11025
            } else if sample_rate1 >= 8000 {
                8000
            } else {
                sample_rate1
            };
        }
        let bps = bit_rate as f32 / (channels as f32 * sample_rate as f32);
        let byte_offset_bits =
            32 - (((bps * frame_len as f32 / 8.0 + 0.5) as i32) as u32).leading_zeros() - 1 + 2;
        if byte_offset_bits + 3 > 25 {
            return Err(Error::unsupported("wma: byte_offset_bits too large"));
        }
        let mut bps1 = bps;
        if channels == 2 {
            bps1 = bps * 1.6;
        }
        if sample_rate1 == 44100 {
            if bps1 >= 0.61 {
                use_noise_coding = false;
            } else {
                high_freq *= 0.4;
            }
        } else if sample_rate1 == 22050 {
            if bps1 >= 1.16 {
                use_noise_coding = false;
            } else if bps1 >= 0.72 {
                high_freq *= 0.7;
            } else {
                high_freq *= 0.6;
            }
        } else if sample_rate1 == 16000 {
            if bps > 0.5 {
                high_freq *= 0.5;
            } else {
                high_freq *= 0.3;
            }
        } else if sample_rate1 == 11025 {
            high_freq *= 0.7;
        } else if sample_rate1 == 8000 {
            if bps <= 0.625 {
                high_freq *= 0.5;
            } else if bps > 0.75 {
                use_noise_coding = false;
            } else {
                high_freq *= 0.65;
            }
        } else if bps >= 0.8 {
            high_freq *= 0.75;
        } else if bps >= 0.6 {
            high_freq *= 0.6;
        } else {
            high_freq *= 0.5;
        }

        // scale factor band sizes
        let coefs_start = if version == 1 { 3 } else { 0 };
        let mut exponent_sizes = [0usize; BLOCK_NB_SIZES];
        let mut exponent_bands = [[0u16; 25]; BLOCK_NB_SIZES];
        let mut coefs_end = [0usize; BLOCK_NB_SIZES];
        let mut high_band_start = [0usize; BLOCK_NB_SIZES];
        let mut exponent_high_sizes = [0usize; BLOCK_NB_SIZES];
        let mut exponent_high_bands = [[0usize; HIGH_BAND_MAX_SIZE]; BLOCK_NB_SIZES];
        for k in 0..nb_block_sizes {
            let block_len = frame_len >> k;
            if version == 1 {
                let mut lpos = 0i64;
                let mut i = 0usize;
                while i < 25 {
                    let a = CRITICAL_FREQS[i] as i64;
                    let b = sample_rate as i64;
                    let mut pos = ((block_len as i64 * 2 * a) + (b >> 1)) / b;
                    if pos > block_len as i64 {
                        pos = block_len as i64;
                    }
                    exponent_bands[k][i] = (pos - lpos) as u16;
                    if pos >= block_len as i64 {
                        i += 1;
                        break;
                    }
                    lpos = pos;
                    i += 1;
                }
                exponent_sizes[k] = i;
            } else {
                let a = (frame_len_bits - BLOCK_MIN_BITS) as usize - k;
                let table: Option<&[u8]> = if a < 3 {
                    if sample_rate >= 44100 {
                        Some(&EXPONENT_BAND_44100[a])
                    } else if sample_rate >= 32000 {
                        Some(&EXPONENT_BAND_32000[a])
                    } else if sample_rate >= 22050 {
                        Some(&EXPONENT_BAND_22050[a])
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(table) = table {
                    let n = table[0] as usize;
                    for i in 0..n {
                        exponent_bands[k][i] = table[1 + i] as u16;
                    }
                    exponent_sizes[k] = n;
                } else {
                    let mut j = 0usize;
                    let mut lpos = 0i64;
                    for i in 0..25 {
                        let a = CRITICAL_FREQS[i] as i64;
                        let b = sample_rate as i64;
                        let mut pos = ((block_len as i64 * 2 * a) + (b << 1)) / (4 * b);
                        pos <<= 2;
                        if pos > block_len as i64 {
                            pos = block_len as i64;
                        }
                        if pos > lpos {
                            exponent_bands[k][j] = (pos - lpos) as u16;
                            j += 1;
                        }
                        if pos >= block_len as i64 {
                            break;
                        }
                        lpos = pos;
                    }
                    exponent_sizes[k] = j;
                }
            }

            coefs_end[k] = (frame_len - (frame_len * 9) / 100) >> k;
            high_band_start[k] =
                ((block_len as f64 * 2.0 * high_freq as f64) / sample_rate as f64 + 0.5) as usize;
            let n = exponent_sizes[k];
            let mut j = 0usize;
            let mut pos = 0usize;
            for i in 0..n {
                let start = pos.max(high_band_start[k]);
                pos += exponent_bands[k][i] as usize;
                let end = pos.min(coefs_end[k]);
                if end > start {
                    exponent_high_bands[k][j] = end - start;
                    j += 1;
                }
            }
            exponent_high_sizes[k] = j;
        }

        // windows + MDCTs
        let mut windows = Vec::with_capacity(nb_block_sizes);
        let mut mdcts = Vec::with_capacity(nb_block_sizes);
        for i in 0..nb_block_sizes {
            windows.push(sine_window(1 << (frame_len_bits - i as u32)));
            mdcts.push(ImdctHalf::new(1 << (frame_len_bits - i as u32), 1.0 / 32768.0));
        }

        // noise generator
        let mut noise_mult = 0f32;
        let mut noise_table = [0f32; NOISE_TAB_SIZE];
        if use_noise_coding {
            noise_mult = if use_exp_vlc { 0.02 } else { 0.04 };
            let mut seed: u32 = 1;
            let norm = (1.0f32 / (1u32 << 31) as f32) * 3f32.sqrt() * noise_mult;
            for entry in noise_table.iter_mut() {
                seed = seed.wrapping_mul(314159).wrapping_add(1);
                *entry = (seed as i32) as f32 * norm;
            }
        }

        // coef VLC tables
        let coef_vlc_table = if sample_rate >= 32000 {
            if bps1 < 0.72 {
                0
            } else if bps1 < 1.16 {
                1
            } else {
                2
            }
        } else {
            2
        };
        let (cv0, r0, l0, i0) = init_coef_vlc(&COEF_VLCS[coef_vlc_table * 2])?;
        let (cv1, r1, l1, i1) = init_coef_vlc(&COEF_VLCS[coef_vlc_table * 2 + 1])?;

        // hgain VLC (from lengths, symbols = codebook index - 18)
        let hgain_lengths: Vec<i8> = WGAIN_HUFFTAB.iter().map(|&(_, l)| l as i8).collect();
        let hgain_symbols: Vec<i32> = WGAIN_HUFFTAB.iter().map(|&(s, _)| s as i32 - 18).collect();
        let hgain_vlc = VlcTable::from_lengths(&hgain_lengths, Some(&hgain_symbols), 0)?;

        // exp VLC (AAC scalefactor)
        let exp_vlc = VlcTable::from_bits_codes(&AAC_SCALEFACTOR_BITS, &AAC_SCALEFACTOR_CODE, None, 0)?;

        let mut d = Self {
            codec_id: params.codec_id.clone(),
            version,
            channels,
            sample_rate,
            bit_rate,
            block_align_len: block_align,
            use_bit_reservoir,
            use_variable_block_len,
            use_exp_vlc,
            use_noise_coding,
            byte_offset_bits,
            frame_len,
            frame_len_bits,
            nb_block_sizes,
            exp_vlc,
            hgain_vlc,
            coef_vlc: [cv0, cv1],
            run_table: [r0, r1],
            level_table: [l0, l1],
            int_table: [i0, i1],
            exponent_sizes,
            exponent_bands,
            high_band_start,
            coefs_start,
            coefs_end,
            exponent_high_sizes,
            exponent_high_bands,
            reset_block_lengths: true,
            block_len_bits: frame_len_bits,
            next_block_len_bits: frame_len_bits,
            prev_block_len_bits: frame_len_bits,
            block_len: frame_len,
            block_num: 0,
            block_pos: 0,
            ms_stereo: false,
            channel_coded: [false; MAX_CHANNELS],
            exponents_bsize: [0; MAX_CHANNELS],
            exponents: [[0.0; BLOCK_MAX_SIZE]; MAX_CHANNELS],
            exponents_initialized: [false; MAX_CHANNELS],
            max_exponent: [1.0; MAX_CHANNELS],
            coefs1: [[0.0; BLOCK_MAX_SIZE]; MAX_CHANNELS],
            coefs: [[0.0; BLOCK_MAX_SIZE]; MAX_CHANNELS],
            output: [0.0; BLOCK_MAX_SIZE * 2],
            mdcts,
            windows,
            frame_out: [[0.0; BLOCK_MAX_SIZE * 2]; MAX_CHANNELS],
            last_superframe: vec![0u8; MAX_CODED_SUPERFRAME_SIZE + 64],
            last_bitoffset: 0,
            last_superframe_len: 0,
            eof_done: false,
            noise_table,
            noise_index: 0,
            noise_mult,
            high_band_coded: [[false; HIGH_BAND_MAX_SIZE]; MAX_CHANNELS],
            high_band_values: [[0; HIGH_BAND_MAX_SIZE]; MAX_CHANNELS],
            lsp_cos_table: [0.0; BLOCK_MAX_SIZE],
            lsp_pow_e_table: [0.0; 256],
            lsp_pow_m_table1: [0.0; 1 << LSP_POW_BITS],
            lsp_pow_m_table2: [0.0; 1 << LSP_POW_BITS],
            pending: Vec::new(),
        };
        if !use_exp_vlc {
            d.wma_lsp_to_curve_init(frame_len);
        }
        Ok(d)
    }

    /// `wma_lsp_to_curve_init` (wmadec.c).
    fn wma_lsp_to_curve_init(&mut self, frame_len: usize) {
        let wdel = std::f64::consts::PI / frame_len as f64;
        for (i, v) in self.lsp_cos_table.iter_mut().take(frame_len).enumerate() {
            *v = (2.0 * (wdel * i as f64).cos()) as f32;
        }
        for (i, v) in self.lsp_pow_e_table.iter_mut().enumerate() {
            let e = i as i32 - 126;
            *v = 2f32.powf(e as f32 * -0.25);
        }
        let mut b = 1.0f32;
        for i in (0..(1 << LSP_POW_BITS)).rev() {
            let m = (1 << LSP_POW_BITS) + i;
            let a = (m as f32 * (0.5 / (1 << LSP_POW_BITS) as f32)).recip().sqrt().sqrt();
            self.lsp_pow_m_table1[i] = 2.0 * a - b;
            self.lsp_pow_m_table2[i] = b - a;
            b = a;
        }
    }

    /// `pow_m1_4` (wmadec.c).
    fn pow_m1_4(&self, x: f32) -> f32 {
        let u = x.to_bits();
        let e = u >> 23;
        let m = ((u >> (23 - LSP_POW_BITS)) & ((1 << LSP_POW_BITS) - 1)) as usize;
        let t_bits = ((u << LSP_POW_BITS) & ((1 << 23) - 1)) | (127 << 23);
        let t = f32::from_bits(t_bits);
        self.lsp_pow_e_table[e as usize] * (self.lsp_pow_m_table1[m] + self.lsp_pow_m_table2[m] * t)
    }

    /// `wma_lsp_to_curve` (wmadec.c).
    fn wma_lsp_to_curve(&self, out: &mut [f32], n: usize, lsp: &[f32; NB_LSP_COEFS]) -> f32 {
        let mut val_max = 0f32;
        for (i, o) in out.iter_mut().take(n).enumerate() {
            let mut p = 0.5f32;
            let mut q = 0.5f32;
            let w = self.lsp_cos_table[i];
            for j in (1..NB_LSP_COEFS).step_by(2) {
                q *= w - lsp[j - 1];
                p *= w - lsp[j];
            }
            p *= p * (2.0 - w);
            q *= q * (2.0 + w);
            let v = p + q;
            let v = self.pow_m1_4(v);
            if v > val_max {
                val_max = v;
            }
            *o = v;
        }
        val_max
    }

    /// `decode_exp_lsp` (wmadec.c).
    fn decode_exp_lsp(&mut self, gb: &mut BitReader<'_>, ch: usize) -> Result<()> {
        let mut lsp_coefs = [0f32; NB_LSP_COEFS];
        for (i, l) in lsp_coefs.iter_mut().enumerate() {
            let val = if i == 0 || i >= 8 {
                gb.get_bits(3)?
            } else {
                gb.get_bits(4)?
            };
            *l = LSP_CODEBOOK[i][val as usize];
        }
        let block_len = self.block_len;
        let mut exp = [0f32; BLOCK_MAX_SIZE];
        let vmax = self.wma_lsp_to_curve(&mut exp, block_len, &lsp_coefs);
        self.exponents[ch][..block_len].copy_from_slice(&exp[..block_len]);
        self.max_exponent[ch] = vmax;
        Ok(())
    }

    /// `decode_exp_vlc` (wmadec.c).
    fn decode_exp_vlc(&mut self, gb: &mut BitReader<'_>, ch: usize) -> Result<()> {
        let mut last_exp;
        let mut max_scale = 0f32;
        let band = (self.frame_len_bits - self.block_len_bits) as usize;
        let mut band_idx = 0usize;
        let mut out_idx = 0usize;
        if self.version == 1 {
            last_exp = gb.get_bits(5)? as i32 + 10;
            let v = pow_tab(last_exp);
            max_scale = v;
            let n = self.exponent_bands[band][band_idx] as usize;
            band_idx += 1;
            for e in self.exponents[ch][out_idx..out_idx + n].iter_mut() {
                *e = v;
            }
            out_idx += n;
        } else {
            last_exp = 36;
        }
        while out_idx < self.block_len {
            let code = self.exp_vlc.get_vlc(gb)?;
            last_exp += code - 60;
            if (last_exp + 60) < 0 || (last_exp + 60) as usize >= 156 {
                return Err(Error::invalid(format!("wma: exponent out of range: {last_exp}")));
            }
            let v = pow_tab(last_exp);
            if v > max_scale {
                max_scale = v;
            }
            if band_idx >= 25 {
                return Err(Error::invalid("wma: exponent band overflow"));
            }
            let n = self.exponent_bands[band][band_idx] as usize;
            band_idx += 1;
            if out_idx + n > self.block_len {
                return Err(Error::invalid("wma: exponent overflow"));
            }
            for e in self.exponents[ch][out_idx..out_idx + n].iter_mut() {
                *e = v;
            }
            out_idx += n;
        }
        self.max_exponent[ch] = max_scale;
        Ok(())
    }

    /// `wma_window` (wmadec.c): apply MDCT window, adding into frame_out.
    fn wma_window(&mut self, ch: usize, out_pos: usize) {
        let block_len = self.block_len;
        let prev_bits = self.prev_block_len_bits;
        let next_bits = self.next_block_len_bits;
        let frame_bits = self.frame_len_bits;
        let input: Vec<f32> = self.output[..2 * block_len].to_vec();

        // left part
        if self.block_len_bits <= prev_bits {
            let bsize = (frame_bits - self.block_len_bits) as usize;
            let win = &self.windows[bsize];
            for i in 0..block_len {
                self.frame_out[ch][out_pos + i] += input[i] * win[i];
            }
        } else {
            let block_len2 = 1usize << prev_bits;
            let n = (block_len - block_len2) / 2;
            let bsize = (frame_bits - prev_bits) as usize;
            let win = &self.windows[bsize];
            for i in 0..block_len2 {
                self.frame_out[ch][out_pos + n + i] += input[n + i] * win[i];
            }
            for i in 0..n {
                self.frame_out[ch][out_pos + n + block_len2 + i] = input[n + block_len2 + i];
            }
        }

        // right part
        let base = out_pos + block_len;
        if self.block_len_bits <= next_bits {
            let bsize = (frame_bits - self.block_len_bits) as usize;
            let win = &self.windows[bsize];
            for i in 0..block_len {
                self.frame_out[ch][base + i] = input[block_len + i] * win[block_len - 1 - i];
            }
        } else {
            let block_len2 = 1usize << next_bits;
            let n = (block_len - block_len2) / 2;
            let bsize = (frame_bits - next_bits) as usize;
            let win = &self.windows[bsize];
            for i in 0..n {
                self.frame_out[ch][base + i] = input[block_len + i];
            }
            for i in 0..block_len2 {
                self.frame_out[ch][base + n + i] =
                    input[block_len + n + i] * win[block_len2 - 1 - i];
            }
            for v in self.frame_out[ch][base + n + block_len2..base + block_len].iter_mut() {
                *v = 0.0;
            }
        }
    }
}

impl WmaDecoder {
    /// `wma_decode_block` (wmadec.c). Returns true when this was the last
    /// block of the frame.
    fn wma_decode_block(&mut self, gb: &mut BitReader<'_>) -> Result<bool> {
        let channels = self.channels;
        let n_log;
        if self.use_variable_block_len {
            n_log = 32 - ((self.nb_block_sizes - 1) as u32).leading_zeros();
            if self.reset_block_lengths {
                self.reset_block_lengths = false;
                let v = gb.get_bits(n_log as usize)? as usize;
                if v >= self.nb_block_sizes {
                    return Err(Error::invalid("wma: prev_block_len_bits out of range"));
                }
                self.prev_block_len_bits = self.frame_len_bits - v as u32;
                let v = gb.get_bits(n_log as usize)? as usize;
                if v >= self.nb_block_sizes {
                    return Err(Error::invalid("wma: block_len_bits out of range"));
                }
                self.block_len_bits = self.frame_len_bits - v as u32;
            } else {
                self.prev_block_len_bits = self.block_len_bits;
                self.block_len_bits = self.next_block_len_bits;
            }
            let v = gb.get_bits(n_log as usize)? as usize;
            if v >= self.nb_block_sizes {
                return Err(Error::invalid("wma: next_block_len_bits out of range"));
            }
            self.next_block_len_bits = self.frame_len_bits - v as u32;
        } else {
            self.next_block_len_bits = self.frame_len_bits;
            self.prev_block_len_bits = self.frame_len_bits;
            self.block_len_bits = self.frame_len_bits;
        }

        if self.frame_len_bits - self.block_len_bits >= self.nb_block_sizes as u32 {
            return Err(Error::invalid("wma: block_len_bits not initialized"));
        }

        self.block_len = 1usize << self.block_len_bits;
        if self.block_pos + self.block_len > self.frame_len {
            return Err(Error::invalid("wma: frame_len overflow"));
        }

        if channels == 2 {
            self.ms_stereo = gb.get_bits1()? != 0;
        }
        let mut any_coded = false;
        for ch in 0..channels {
            self.channel_coded[ch] = gb.get_bits1()? != 0;
            any_coded |= self.channel_coded[ch];
        }

        let bsize = (self.frame_len_bits - self.block_len_bits) as usize;

        let mut nb_coefs = [0usize; MAX_CHANNELS];
        let mut total_gain = 1i32;
        let mut coef_nb_bits = 13usize;
        if any_coded {
            loop {
                if gb.bits_left() < 7 {
                    return Err(Error::invalid("wma: total_gain overread"));
                }
                let a = gb.get_bits(7)? as i32;
                total_gain += a;
                if a != 127 {
                    break;
                }
            }
            coef_nb_bits = wma_total_gain_to_bits(total_gain);

            let n = self.coefs_end[bsize] - self.coefs_start;
            for ch in 0..channels {
                nb_coefs[ch] = n;
            }

            if self.use_noise_coding {
                for ch in 0..channels {
                    if self.channel_coded[ch] {
                        let n = self.exponent_high_sizes[bsize];
                        for i in 0..n {
                            let a = gb.get_bits1()? != 0;
                            self.high_band_coded[ch][i] = a;
                            if a {
                                nb_coefs[ch] -= self.exponent_high_bands[bsize][i];
                            }
                        }
                    }
                }
                for ch in 0..channels {
                    if self.channel_coded[ch] {
                        let n = self.exponent_high_sizes[bsize];
                        let mut val = i32::MIN;
                        for i in 0..n {
                            if self.high_band_coded[ch][i] {
                                if val == i32::MIN {
                                    val = gb.get_bits(7)? as i32 - 19;
                                } else {
                                    val += self.hgain_vlc.get_vlc(gb)?;
                                }
                                self.high_band_values[ch][i] = val;
                            }
                        }
                    }
                }
            }

            // exponents can be reused in short blocks
            if self.block_len_bits == self.frame_len_bits || gb.get_bits1()? != 0 {
                for ch in 0..channels {
                    if self.channel_coded[ch] {
                        if self.use_exp_vlc {
                            self.decode_exp_vlc(gb, ch)?;
                        } else {
                            self.decode_exp_lsp(gb, ch)?;
                        }
                        self.exponents_bsize[ch] = bsize as u32;
                        self.exponents_initialized[ch] = true;
                    }
                }
            }
            for ch in 0..channels {
                if self.channel_coded[ch] && !self.exponents_initialized[ch] {
                    return Err(Error::invalid("wma: exponents not initialized"));
                }
            }
        }

        // parse spectral coefficients (RLE)
        for ch in 0..channels {
            if self.channel_coded[ch] {
                let tindex = if ch == 1 && self.ms_stereo { 1 } else { 0 };
                for v in self.coefs1[ch][..self.block_len].iter_mut() {
                    *v = 0.0;
                }
                let run = self.run_table[tindex].clone();
                let level = self.level_table[tindex].clone();
                self.run_level_decode(gb, ch, tindex, &run, &level, 0, nb_coefs[ch], coef_nb_bits)?;
            }
            if self.version == 1 && channels >= 2 {
                gb.align_to_byte();
            }
        }

        // normalize
        let n4 = self.block_len / 2;
        let mut mdct_norm = 1.0f32 / n4 as f32;
        if self.version == 1 {
            mdct_norm *= (n4 as f32).sqrt();
        }

        // compute the MDCT coefficients
        for ch in 0..channels {
            if !self.channel_coded[ch] {
                continue;
            }
            let esize = self.exponents_bsize[ch] as usize;
            let mut mult = 10f32.powf(total_gain as f32 * 0.05) / self.max_exponent[ch];
            mult *= mdct_norm;
            let mut coefs_idx = 0usize;
            if self.use_noise_coding {
                let mut mult1 = mult;
                // very low freqs: noise
                for i in 0..self.coefs_start {
                    self.coefs[ch][coefs_idx] = self.noise_table[self.noise_index]
                        * self.exponents[ch][(i << bsize) >> esize]
                        * mult1;
                    coefs_idx += 1;
                    self.noise_index = (self.noise_index + 1) & (NOISE_TAB_SIZE - 1);
                }

                let n1 = self.exponent_high_sizes[bsize];
                // compute power of high bands
                let mut exp_power = [0f32; HIGH_BAND_MAX_SIZE];
                let mut exp_pos = self.high_band_start[bsize];
                let mut last_high_band = 0usize;
                for j in 0..n1 {
                    let n = self.exponent_high_bands[(self.frame_len_bits - self.block_len_bits) as usize][j];
                    if self.high_band_coded[ch][j] {
                        let mut e2 = 0f32;
                        for i in 0..n {
                            let v = self.exponents[ch][((exp_pos + i) << bsize) >> esize];
                            e2 += v * v;
                        }
                        exp_power[j] = e2 / n as f32;
                        last_high_band = j;
                    }
                    exp_pos += n;
                }

                // main freqs and high freqs
                let mut exp_pos = self.coefs_start;
                for j in -1i32..n1 as i32 {
                    let n = if j < 0 {
                        self.high_band_start[bsize] - self.coefs_start
                    } else {
                        self.exponent_high_bands[(self.frame_len_bits - self.block_len_bits) as usize][j as usize]
                    };
                    if j >= 0 && self.high_band_coded[ch][j as usize] {
                        // noise with specified power
                        let j = j as usize;
                        mult1 = (exp_power[j] / exp_power[last_high_band]).sqrt();
                        mult1 = mult1 * 10f32.powf(self.high_band_values[ch][j] as f32 * 0.05);
                        mult1 = mult1 / (self.max_exponent[ch] * self.noise_mult);
                        mult1 *= mdct_norm;
                        for i in 0..n {
                            let noise = self.noise_table[self.noise_index];
                            self.noise_index = (self.noise_index + 1) & (NOISE_TAB_SIZE - 1);
                            self.coefs[ch][coefs_idx] =
                                noise * self.exponents[ch][((exp_pos + i) << bsize) >> esize] * mult1;
                            coefs_idx += 1;
                        }
                        exp_pos += n;
                    } else {
                        // coded values + small noise
                        for i in 0..n {
                            let noise = self.noise_table[self.noise_index];
                            self.noise_index = (self.noise_index + 1) & (NOISE_TAB_SIZE - 1);
                            let c1 = self.coefs1[ch][coefs_idx - self.coefs_start];
                            self.coefs[ch][coefs_idx] = (c1 + noise)
                                * self.exponents[ch][((exp_pos + i) << bsize) >> esize]
                                * mult;
                            coefs_idx += 1;
                        }
                        exp_pos += n;
                    }
                }

                // very high freqs: noise
                let n = self.block_len - self.coefs_end[bsize];
                let exp_last = self.exponents[ch][(self.block_len - (1 << bsize)) >> esize];
                let mult1 = mult * exp_last;
                for _i in 0..n {
                    self.coefs[ch][coefs_idx] = self.noise_table[self.noise_index] * mult1;
                    coefs_idx += 1;
                    self.noise_index = (self.noise_index + 1) & (NOISE_TAB_SIZE - 1);
                }
            } else {
                for _i in 0..self.coefs_start {
                    self.coefs[ch][coefs_idx] = 0.0;
                    coefs_idx += 1;
                }
                let n = nb_coefs[ch];
                for i in 0..n {
                    self.coefs[ch][coefs_idx] =
                        self.coefs1[ch][i] * self.exponents[ch][(i << bsize) >> esize] * mult;
                    coefs_idx += 1;
                }
                let n = self.block_len - self.coefs_end[bsize];
                for _i in 0..n {
                    self.coefs[ch][coefs_idx] = 0.0;
                    coefs_idx += 1;
                }
            }
        }

        if self.ms_stereo && self.channel_coded[1] {
            if !self.channel_coded[0] {
                for v in self.coefs[0][..self.block_len].iter_mut() {
                    *v = 0.0;
                }
                self.channel_coded[0] = true;
            }
            // butterflies_float
            for i in 0..self.block_len {
                let a = self.coefs[0][i];
                let b = self.coefs[1][i];
                self.coefs[0][i] = a - b;
                self.coefs[1][i] = a + b;
            }
        }

        // MDCT + windowing
        let bsize = (self.frame_len_bits - self.block_len_bits) as usize;
        let n4 = self.block_len / 2;
        for ch in 0..channels {
            if self.channel_coded[ch] {
                let coefs = self.coefs[ch];
                self.mdcts[bsize].run_full(&coefs[..self.block_len], &mut self.output);
            } else if !(self.ms_stereo && ch == 1) {
                self.output = [0.0; BLOCK_MAX_SIZE * 2];
            }
            let index = (self.frame_len / 2) + self.block_pos - n4;
            self.wma_window(ch, index);
        }

        self.block_num += 1;
        self.block_pos += self.block_len;
        Ok(self.block_pos >= self.frame_len)
    }

    /// `ff_wma_run_level_decode` (wma.c), version 0 (wmav1/2).
    fn run_level_decode(
        &mut self,
        gb: &mut BitReader<'_>,
        ch: usize,
        tindex: usize,
        run_table: &[u16],
        level_table: &[f32],
        offset0: usize,
        num_coefs: usize,
        coef_nb_bits: usize,
    ) -> Result<()> {
        let block_len = self.block_len;
        let coef_mask = block_len - 1;
        let frame_len_bits = self.frame_len_bits as usize;
        let mut offset = offset0;
        while offset < num_coefs {
            let code = self.coef_vlc[tindex].get_vlc(gb)?;
            if code > 1 {
                offset += run_table[code as usize] as usize;
                let sign = gb.get_bits1()? as i32 - 1;
                let level = level_table[code as usize];
                self.coefs1[ch][offset & coef_mask] =
                    if sign != 0 { -level } else { level };
            } else if code == 1 {
                break;
            } else {
                let level = gb.get_bits(coef_nb_bits)? as i32;
                offset += gb.get_bits(frame_len_bits)? as usize;
                let sign = gb.get_bits1()? as i32 - 1;
                let signed = if sign != 0 { -level } else { level };
                self.coefs1[ch][offset & coef_mask] = signed as f32;
            }
        }
        if offset > num_coefs {
            return Err(Error::invalid("wma: overflow in spectral RLE"));
        }
        Ok(())
    }

    /// `wma_decode_frame` (wmadec.c): appends one frame to `pending`.
    fn wma_decode_frame(&mut self, gb: &mut BitReader<'_>) -> Result<()> {
        self.block_num = 0;
        self.block_pos = 0;
        loop {
            if self.wma_decode_block(gb)? {
                break;
            }
        }
        let frame_len = self.frame_len;
        let mut frame = AudioFrame {
            samples: frame_len as u32,
            pts: None,
            data: Vec::with_capacity(self.channels),
        };
        for ch in 0..self.channels {
            let mut plane = Vec::with_capacity(frame_len * 4);
            for &v in &self.frame_out[ch][..frame_len] {
                plane.extend_from_slice(&v.to_le_bytes());
            }
            frame.data.push(plane);
            self.frame_out[ch].copy_within(frame_len..2 * frame_len, 0);
        }
        self.pending.push(frame);
        Ok(())
    }

    /// `wma_decode_superframe` (wmadec.c).
    fn wma_decode_superframe(&mut self, data: &[u8]) -> Result<()> {
        let mut buf = data;
        if buf.is_empty() {
            if self.eof_done {
                return Ok(());
            }
            let frame_len = self.frame_len;
            let mut frame = AudioFrame {
                samples: frame_len as u32,
                pts: None,
                data: Vec::with_capacity(self.channels),
            };
            for ch in 0..self.channels {
                let mut plane = Vec::with_capacity(frame_len * 4);
                for &v in &self.frame_out[ch][..frame_len] {
                    plane.extend_from_slice(&v.to_le_bytes());
                }
                frame.data.push(plane);
            }
            self.last_superframe_len = 0;
            self.eof_done = true;
            self.pending.push(frame);
            return Ok(());
        }
        if buf.len() < self.block_align_bytes() {
            return Ok(());
        }
        buf = &buf[..self.block_align_bytes()];
        let mut gb = BitReader::new(buf);
        let mut nb_frames = 1usize;
        if self.use_bit_reservoir {
            gb.skip_bits(4)?;
            let raw_nb = gb.get_bits(4)? as i32;
            let sub = if self.last_superframe_len <= 0 { 1 } else { 0 };
            let nb = raw_nb - sub;
            if nb <= 0 {
                let bits_left_now = gb.bits_left();
                let is_error = nb < 0 || bits_left_now <= 8;
                if is_error {
                    return Err(Error::invalid("wma: nb_frames is 0"));
                }
                // nb_frames == 0: append to last superframe
                if self.last_superframe_len + buf.len() - 1 > MAX_CODED_SUPERFRAME_SIZE {
                    return Err(Error::invalid("wma: superframe overflow"));
                }
                let mut q = self.last_superframe_len;
                let mut len = buf.len() - 1;
                while len > 0 {
                    self.last_superframe[q] = gb.get_bits(8)? as u8;
                    q += 1;
                    len -= 1;
                }
                self.last_superframe_len += 8 * buf.len() - 8;
                return Ok(());
            }
            nb_frames = nb as usize;
        }

        if self.use_bit_reservoir {
            let bit_offset = gb.get_bits(self.byte_offset_bits as usize + 3)? as usize;
            if bit_offset > gb.bits_left() {
                return Err(Error::invalid("wma: invalid last frame bit offset"));
            }
            if self.last_superframe_len > 0 {
                if self.last_superframe_len + ((bit_offset + 7) >> 3) > MAX_CODED_SUPERFRAME_SIZE {
                    return Err(Error::invalid("wma: superframe overflow"));
                }
                let mut q = self.last_superframe_len;
                let mut len = bit_offset;
                while len > 7 {
                    self.last_superframe[q] = gb.get_bits(8)? as u8;
                    q += 1;
                    len -= 8;
                }
                if len > 0 {
                    self.last_superframe[q] = (gb.get_bits(len)? << (8 - len)) as u8;
                }
                let bits = self.last_superframe_len * 8 + bit_offset;
                let data = self.last_superframe.clone();
                let mut gb2 = BitReader::with_bit_len(&data, bits);
                if self.last_bitoffset > 0 {
                    gb2.skip_bits(self.last_bitoffset)?;
                }
                let res = self.wma_decode_frame(&mut gb2);
                self.last_superframe_len = 0;
                res?;
                nb_frames -= 1;
            }

            let pos = bit_offset + 4 + 4 + self.byte_offset_bits as usize + 3;
            if pos >= MAX_CODED_SUPERFRAME_SIZE * 8 || pos > buf.len() * 8 {
                return Err(Error::invalid("wma: invalid bit offset"));
            }
            let data = &buf[pos >> 3..];
            let mut gb2 = BitReader::new(data);
            let len = pos & 7;
            if len > 0 {
                gb2.skip_bits(len)?;
            }
            self.reset_block_lengths = true;
            for _ in 0..nb_frames {
                self.wma_decode_frame(&mut gb2)?;
            }

            let pos2 = gb2.bits_count() + ((bit_offset + 4 + 4 + self.byte_offset_bits as usize + 3) & !7);
            self.last_bitoffset = pos2 & 7;
            let byte_pos = pos2 >> 3;
            let len = buf.len() - byte_pos;
            if len > MAX_CODED_SUPERFRAME_SIZE {
                return Err(Error::invalid("wma: len invalid"));
            }
            self.last_superframe_len = len;
            self.last_superframe[..len].copy_from_slice(&buf[byte_pos..]);
        } else {
            self.wma_decode_frame(&mut gb)?;
        }
        Ok(())
    }
}

impl WmaDecoder {
    /// The packet framing size: the decoder was created with a fixed
    /// `block_align`; recover it from the first packet (FFmpeg reads it from
    /// AVCodecContext). We store it as the log2-based size from init.
    fn block_align_bytes(&self) -> usize {
        self.block_align_len
    }
}

fn wma_total_gain_to_bits(total_gain: i32) -> usize {
    if total_gain < 15 {
        13
    } else if total_gain < 32 {
        12
    } else if total_gain < 40 {
        11
    } else if total_gain < 45 {
        10
    } else {
        9
    }
}


impl Decoder for WmaDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.wma_decode_superframe(&packet.data)
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(f) = self.pending.first().cloned() {
            self.pending.remove(0);
            return Ok(Frame::Audio(f));
        }
        Err(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        self.last_bitoffset = 0;
        self.last_superframe_len = 0;
        self.eof_done = false;
        self.pending.clear();
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.flush()
    }

    fn output_audio_format(&self) -> Option<oxideav_core::AudioFormat> {
        Some(oxideav_core::AudioFormat {
            sample_format: SampleFormat::F32P,
            sample_rate: self.sample_rate,
            channels: self.channels as u16,
        })
    }
}
