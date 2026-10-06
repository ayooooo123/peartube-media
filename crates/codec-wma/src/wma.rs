// Ported from FFmpeg (commit 2da55bf): libavcodec/wmadec.c, wma.c (ff_wma_init,
// ff_wma_run_level_decode, ff_wma_get_large_val, init_coef_vlc), wma.h
// (WMACodecContext), wmadata.h (coef VLC tables), wma_freqs.c
// (ff_wma_critical_freqs), aactab.c (scalefactor VLC), sinewin.c, and
// libavutil/tx.c float MDCT (AV_TX_FLOAT_MDCT, AV_TX_FULL_IMDCT).
// GNU Lesser General Public License 2.1 or later

//! WMA v1/v2 decoder (`wmav1`, `wmav2`).

use crate::bits::BitReader;
use crate::fft::{sine_window, Imdct};
use crate::tables::*;
use crate::wma_common::{av_log2, wma_get_frame_len_bits};
use crate::vlc::VlcTable;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat};

const MAX_CHANNELS: usize = 2;
const BLOCK_NB_SIZES: usize = (BLOCK_MAX_BITS - BLOCK_MIN_BITS + 1) as usize;

/// `pow_tab + 60` (wmadec.c): `pow(10, e / 16.0)` for e in -60..=95,
/// FFmpeg's double literals rounded to float.
fn pow_tab(e: i32) -> f32 {
    10f64.powf(e as f64 / 16.0) as f32
}

/// `ff_exp10` (libavutil/ffmath.h).
fn ff_exp10(x: f64) -> f64 {
    (std::f64::consts::LOG2_10 * x).exp2()
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
    mdcts: Vec<Imdct>,
    windows: Vec<Vec<f32>>,
    frame_out: [[f32; BLOCK_MAX_SIZE * 2]; MAX_CHANNELS],

    last_superframe: Vec<u8>,
    last_bitoffset: usize,
    last_superframe_len: usize,
    eof_done: bool,
    /// FFmpeg's `internal->skip_samples` (`avctx->delay = frame_len * 2`).
    skip_samples: usize,

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

        // rate-dependent parameters (float/double promotions as in wma.c)
        let mut use_noise_coding = true;
        let mut high_freq = (sample_rate as f64 * 0.5) as f32;
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
        let bps = bit_rate as f32 / (channels as u32 * sample_rate) as f32;
        let byte_offset_bits = av_log2(((bps * frame_len as f32) as f64 / 8.0 + 0.5) as i32 as u32) + 2;
        if byte_offset_bits + 3 > 25 {
            return Err(Error::unsupported("wma: byte_offset_bits too large"));
        }
        let scale_hf = |hf: f32, k: f64| (hf as f64 * k) as f32;
        let bps1 = if channels == 2 { (bps as f64 * 1.6) as f32 } else { bps };
        let (bps_d, bps1_d) = (bps as f64, bps1 as f64);
        if sample_rate1 == 44100 {
            if bps1_d >= 0.61 {
                use_noise_coding = false;
            } else {
                high_freq = scale_hf(high_freq, 0.4);
            }
        } else if sample_rate1 == 22050 {
            if bps1_d >= 1.16 {
                use_noise_coding = false;
            } else if bps1_d >= 0.72 {
                high_freq = scale_hf(high_freq, 0.7);
            } else {
                high_freq = scale_hf(high_freq, 0.6);
            }
        } else if sample_rate1 == 16000 {
            if bps_d > 0.5 {
                high_freq = scale_hf(high_freq, 0.5);
            } else {
                high_freq = scale_hf(high_freq, 0.3);
            }
        } else if sample_rate1 == 11025 {
            high_freq = scale_hf(high_freq, 0.7);
        } else if sample_rate1 == 8000 {
            if bps_d <= 0.625 {
                high_freq = scale_hf(high_freq, 0.5);
            } else if bps_d > 0.75 {
                use_noise_coding = false;
            } else {
                high_freq = scale_hf(high_freq, 0.65);
            }
        } else if bps_d >= 0.8 {
            high_freq = scale_hf(high_freq, 0.75);
        } else if bps_d >= 0.6 {
            high_freq = scale_hf(high_freq, 0.6);
        } else {
            high_freq = scale_hf(high_freq, 0.5);
        }

        // scale factor band sizes for each MDCT block size
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
                // wma.c writes every block size's layout into index 0.
                let mut lpos = 0i64;
                let mut i = 0usize;
                while i < 25 {
                    let a = CRITICAL_FREQS[i] as i64;
                    let b = sample_rate as i64;
                    let mut pos = ((block_len as i64 * 2 * a) + (b >> 1)) / b;
                    if pos > block_len as i64 {
                        pos = block_len as i64;
                    }
                    exponent_bands[0][i] = (pos - lpos) as u16;
                    if pos >= block_len as i64 {
                        i += 1;
                        break;
                    }
                    lpos = pos;
                    i += 1;
                }
                exponent_sizes[0] = i;
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

            // max number of coefs
            coefs_end[k] = (frame_len - (frame_len * 9) / 100) >> k;
            // high freq computation
            let hbs = (((block_len * 2) as f32 * high_freq) / sample_rate as f32) as f64 + 0.5;
            high_band_start[k] = hbs as i32 as usize;
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

        // MDCT windows (simple sine window) and transforms
        let mut windows = Vec::with_capacity(nb_block_sizes);
        let mut mdcts = Vec::with_capacity(nb_block_sizes);
        for i in 0..nb_block_sizes {
            windows.push(sine_window(1 << (frame_len_bits - i as u32)));
            mdcts.push(Imdct::new(1 << (frame_len_bits - i as u32), 1.0 / 32768.0));
        }

        // noise generator
        let mut noise_mult = 0f32;
        let mut noise_table = [0f32; NOISE_TAB_SIZE];
        if use_noise_coding {
            noise_mult = if use_exp_vlc { 0.02 } else { 0.04 };
            let mut seed: u32 = 1;
            let norm = ((1.0 / (1u64 << 31) as f64) * 3f64.sqrt() * noise_mult as f64) as f32;
            for entry in noise_table.iter_mut() {
                seed = seed.wrapping_mul(314159).wrapping_add(1);
                *entry = (seed as i32) as f32 * norm;
            }
        }

        // coef VLC tables
        let coef_vlc_table = if sample_rate >= 32000 {
            if bps1_d < 0.72 {
                0
            } else if bps1_d < 1.16 {
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
            skip_samples: frame_len * 2,
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
        let wdel = (std::f64::consts::PI / frame_len as f64) as f32;
        for (i, v) in self.lsp_cos_table.iter_mut().take(frame_len).enumerate() {
            *v = (2.0 * ((wdel * i as f32) as f64).cos()) as f32;
        }
        // tables for x^-0.25 computation
        for (i, v) in self.lsp_pow_e_table.iter_mut().enumerate() {
            let e = i as i32 - 126;
            *v = ((e as f64 * -0.25) as f32).exp2();
        }
        let mut b = 1.0f32;
        for i in (0..(1 << LSP_POW_BITS)).rev() {
            let m = (1 << LSP_POW_BITS) + i;
            let a = (m as f32 as f64 * (0.5 / (1 << LSP_POW_BITS) as f64)) as f32;
            let a = (1.0 / (a as f64).sqrt().sqrt()) as f32;
            self.lsp_pow_m_table1[i] = 2.0 * a - b;
            self.lsp_pow_m_table2[i] = b - a;
            b = a;
        }
    }

    /// `pow_m1_4` (wmadec.c).
    fn pow_m1_4(&self, x: f32) -> f32 {
        let u = x.to_bits();
        let e = (u >> 23) & 0xFF;
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

    /// `decode_exp_vlc` (wmadec.c). The band sizes are walked as FFmpeg's
    /// pointer does: a zero-sized band still writes four values (its
    /// Duff's-device copy), and the walk may continue into the next block
    /// size's row (wmav1 keeps every layout in row 0).
    fn decode_exp_vlc(&mut self, gb: &mut BitReader<'_>, ch: usize) -> Result<()> {
        let bands = self.exponent_bands.as_flattened();
        let mut band_pos = (self.frame_len_bits - self.block_len_bits) as usize * 25;
        let block_len = self.block_len;
        let exponents = &mut self.exponents[ch];
        let mut q = 0usize;
        let mut fill = |q: &mut usize, band_pos: &mut usize, v: f32| -> Result<()> {
            let n = *bands.get(*band_pos).ok_or_else(|| Error::invalid("wma: exponent band overflow"))? as usize;
            *band_pos += 1;
            let count = if n == 0 { 4 } else { n };
            for _ in 0..count {
                *exponents.get_mut(*q).ok_or_else(|| Error::invalid("wma: exponent overflow"))? = v;
                *q += 1;
            }
            Ok(())
        };
        let mut max_scale = 0f32;
        let mut last_exp;
        if self.version == 1 {
            last_exp = gb.get_bits(5)? as i32 + 10;
            let v = pow_tab(last_exp);
            max_scale = v;
            fill(&mut q, &mut band_pos, v)?;
        } else {
            last_exp = 36;
        }
        while q < block_len {
            let code = self.exp_vlc.get_vlc(gb)?;
            // NOTE: this offset is the same as MPEG-4 AAC!
            last_exp += code - 60;
            if !(-60..96).contains(&last_exp) {
                return Err(Error::invalid(format!("wma: exponent out of range: {last_exp}")));
            }
            let v = pow_tab(last_exp);
            if v > max_scale {
                max_scale = v;
            }
            fill(&mut q, &mut band_pos, v)?;
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
            n_log = crate::wma_common::av_log2((self.nb_block_sizes - 1) as u32) + 1;
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

        // if no channel is coded, there is no need to go further
        if any_coded {
            // read total gain and extract the corresponding number of bits
            // for coef escape coding
            let mut total_gain = 1i32;
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
            let coef_nb_bits = wma_total_gain_to_bits(total_gain);

            // compute number of coefficients
            let mut nb_coefs = [self.coefs_end[bsize] - self.coefs_start; MAX_CHANNELS];

            if self.use_noise_coding {
                for ch in 0..channels {
                    if self.channel_coded[ch] {
                        let n = self.exponent_high_sizes[bsize];
                        for i in 0..n {
                            let a = gb.get_bits1()? != 0;
                            self.high_band_coded[ch][i] = a;
                            if a {
                                // if noise coding, the coefficients are not
                                // transmitted
                                nb_coefs[ch] = nb_coefs[ch]
                                    .checked_sub(self.exponent_high_bands[bsize][i])
                                    .ok_or_else(|| Error::invalid("wma: negative coefficient count"))?;
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

            // parse spectral coefficients: just RLE encoding
            for ch in 0..channels {
                if self.channel_coded[ch] {
                    // special VLC tables are used for ms stereo because
                    // there is potentially less energy there
                    let tindex = (ch == 1 && self.ms_stereo) as usize;
                    self.coefs1[ch][..self.block_len].fill(0.0);
                    self.run_level_decode(gb, ch, tindex, nb_coefs[ch], coef_nb_bits)?;
                }
                if self.version == 1 && channels >= 2 {
                    gb.align_to_byte();
                }
            }

            // normalize
            let n4 = self.block_len / 2;
            let mut mdct_norm = (1.0 / n4 as f32 as f64) as f32;
            if self.version == 1 {
                mdct_norm = (mdct_norm as f64 * (n4 as f64).sqrt()) as f32;
            }

            // finally compute the MDCT coefficients
            for ch in 0..channels {
                if self.channel_coded[ch] {
                    self.compute_coefs(ch, bsize, total_gain, mdct_norm, nb_coefs[ch])?;
                }
            }

            if self.ms_stereo && self.channel_coded[1] {
                // nominal case for ms stereo: we do it before mdct
                if !self.channel_coded[0] {
                    self.coefs[0][..self.block_len].fill(0.0);
                    self.channel_coded[0] = true;
                }
                // butterflies_float
                let (c0, c1) = self.coefs.split_at_mut(1);
                for (a, b) in c0[0][..self.block_len].iter_mut().zip(c1[0][..self.block_len].iter_mut()) {
                    let t = *a - *b;
                    *a += *b;
                    *b = t;
                }
            }
        }

        // next: inverse MDCT, window and overlap-add into the frame
        let n4 = self.block_len / 2;
        for ch in 0..channels {
            if self.channel_coded[ch] {
                self.mdcts[bsize].imdct_full(&mut self.output, &self.coefs[ch]);
            } else if !(self.ms_stereo && ch == 1) {
                self.output.fill(0.0);
            }
            let index = (self.frame_len / 2) + self.block_pos - n4;
            self.wma_window(ch, index);
        }

        // update block number
        self.block_num += 1;
        self.block_pos += self.block_len;
        Ok(self.block_pos >= self.frame_len)
    }

    /// The coefficient reconstruction of `wma_decode_block` for one coded
    /// channel: dequantized coefficients, noise substitution, exponents.
    /// `exponents` is walked with FFmpeg's pointer arithmetic
    /// (`i << bsize >> esize` steps), which floors per step.
    fn compute_coefs(&mut self, ch: usize, bsize: usize, total_gain: i32, mdct_norm: f32, nb_coefs: usize) -> Result<()> {
        let esize = self.exponents_bsize[ch] as usize;
        let mut mult = (ff_exp10(total_gain as f64 * 0.05) / self.max_exponent[ch] as f64) as f32;
        mult *= mdct_norm;
        let block_len = self.block_len;
        let exponents = &self.exponents[ch];
        let exp_at = |base: isize, i: isize| -> Result<f32> {
            usize::try_from(base + ((i << bsize) >> esize))
                .ok()
                .and_then(|idx| exponents.get(idx).copied())
                .ok_or_else(|| Error::invalid("wma: exponent index out of range"))
        };
        let coefs = &mut self.coefs[ch];
        let coefs1 = &self.coefs1[ch];
        let mut out = 0usize;
        let mut put = |out: &mut usize, v: f32| -> Result<()> {
            *coefs.get_mut(*out).ok_or_else(|| Error::invalid("wma: coefficient overflow"))? = v;
            *out += 1;
            Ok(())
        };
        if self.use_noise_coding {
            let noise_table = &self.noise_table;
            let noise_index = &mut self.noise_index;
            let mut next_noise = || {
                let v = noise_table[*noise_index];
                *noise_index = (*noise_index + 1) & (NOISE_TAB_SIZE - 1);
                v
            };

            // very low freqs: noise
            for i in 0..self.coefs_start as isize {
                let v = next_noise() * exp_at(0, i)? * mult;
                put(&mut out, v)?;
            }

            let n1 = self.exponent_high_sizes[bsize];
            let high_bands = &self.exponent_high_bands[bsize];

            // compute power of high bands
            let mut exp_power = [0f32; HIGH_BAND_MAX_SIZE];
            let mut base = ((self.high_band_start[bsize] as isize) << bsize) >> esize;
            let mut last_high_band = 0usize;
            for j in 0..n1 {
                let n = high_bands[j] as isize;
                if self.high_band_coded[ch][j] {
                    let mut e2 = 0f32;
                    for i in 0..n {
                        let v = exp_at(base, i)?;
                        e2 += v * v;
                    }
                    exp_power[j] = e2 / n as f32;
                    last_high_band = j;
                }
                base += (n << bsize) >> esize;
            }

            // main freqs and high freqs
            let mut base = ((self.coefs_start as isize) << bsize) >> esize;
            let mut c1 = 0usize;
            for j in -1isize..n1 as isize {
                let n = if j < 0 {
                    self.high_band_start[bsize] as isize - self.coefs_start as isize
                } else {
                    high_bands[j as usize] as isize
                };
                if j >= 0 && self.high_band_coded[ch][j as usize] {
                    // use noise with specified power
                    let j = j as usize;
                    let mut mult1 = ((exp_power[j] / exp_power[last_high_band]) as f64).sqrt() as f32;
                    mult1 = (mult1 as f64 * ff_exp10(self.high_band_values[ch][j] as f64 * 0.05)) as f32;
                    mult1 /= self.max_exponent[ch] * self.noise_mult;
                    mult1 *= mdct_norm;
                    for i in 0..n {
                        let noise = next_noise();
                        put(&mut out, noise * exp_at(base, i)? * mult1)?;
                    }
                } else {
                    // coded values + small noise
                    for i in 0..n {
                        let noise = next_noise();
                        let c = *coefs1.get(c1).ok_or_else(|| Error::invalid("wma: coefficient overflow"))?;
                        c1 += 1;
                        put(&mut out, (c + noise) * exp_at(base, i)? * mult)?;
                    }
                }
                base += (n << bsize) >> esize;
            }

            // very high freqs: noise
            let n = block_len as isize - self.coefs_end[bsize] as isize;
            let mult1 = mult * exp_at(base, -1)?;
            for _ in 0..n {
                put(&mut out, next_noise() * mult1)?;
            }
        } else {
            for _ in 0..self.coefs_start {
                put(&mut out, 0.0)?;
            }
            for i in 0..nb_coefs {
                put(&mut out, coefs1[i] * exp_at(0, i as isize)? * mult)?;
            }
            for _ in 0..block_len.saturating_sub(self.coefs_end[bsize]) {
                put(&mut out, 0.0)?;
            }
        }
        Ok(())
    }

    /// `ff_wma_run_level_decode` (wma.c), version 0 (wmav1/2).
    fn run_level_decode(
        &mut self,
        gb: &mut BitReader<'_>,
        ch: usize,
        tindex: usize,
        num_coefs: usize,
        coef_nb_bits: usize,
    ) -> Result<()> {
        let coef_mask = self.block_len - 1;
        let frame_len_bits = self.frame_len_bits as usize;
        let vlc = &self.coef_vlc[tindex];
        let run_table = &self.run_table[tindex];
        let level_table = &self.level_table[tindex];
        let ptr = &mut self.coefs1[ch];
        let mut offset = 0usize;
        while offset < num_coefs {
            let code = vlc.get_vlc(gb)?;
            if code > 1 {
                // normal code
                offset += run_table[code as usize] as usize;
                let level = level_table[code as usize];
                ptr[offset & coef_mask] = if gb.get_bits1()? == 0 { -level } else { level };
            } else if code == 1 {
                // EOB
                break;
            } else {
                // escape
                let level = gb.get_bits(coef_nb_bits)? as i32;
                // NOTE: this is rather suboptimal. reading block_len_bits
                // would be better
                offset += gb.get_bits(frame_len_bits)? as usize;
                let negative = gb.get_bits1()? == 0;
                ptr[offset & coef_mask] = if negative { -level } else { level } as f32;
            }
            offset += 1;
        }
        // NOTE: EOB can be omitted
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

    /// `wma_decode_superframe` (wmadec.c) for one `block_align`-sized
    /// superframe, or the end-of-stream frame when `buf` is empty. On error
    /// the frames decoded so far from this superframe are dropped, as FFmpeg
    /// discards the whole output AVFrame.
    fn wma_decode_superframe(&mut self, buf: &[u8]) -> Result<()> {
        let pending_before = self.pending.len();
        let res = self.decode_superframe_inner(buf);
        if res.is_err() {
            self.pending.truncate(pending_before);
        }
        res
    }

    /// The `fail:` exit of `wma_decode_superframe`: reset the bit reservoir.
    fn superframe_fail(&mut self, e: Error) -> Result<()> {
        self.last_superframe_len = 0;
        Err(e)
    }

    fn decode_superframe_inner(&mut self, buf: &[u8]) -> Result<()> {
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
            self.pending.push(frame);
            self.last_superframe_len = 0;
            self.eof_done = true;
            return Ok(());
        }
        let buf_size = self.block_align_len;
        if buf.len() < buf_size {
            return Err(Error::invalid("wma: input packet size too small"));
        }
        let buf = &buf[..buf_size];
        let mut gb = BitReader::new(buf);

        let mut nb_frames: i32 = 1;
        if self.use_bit_reservoir {
            // super frame index
            gb.skip_bits(4)?;
            nb_frames = gb.get_bits(4)? as i32 - (self.last_superframe_len == 0) as i32;
            if nb_frames <= 0 {
                let is_error = nb_frames < 0 || gb.bits_left() <= 8;
                if is_error {
                    return Err(Error::invalid("wma: invalid nb_frames"));
                }
                if self.last_superframe_len + buf_size - 1 > MAX_CODED_SUPERFRAME_SIZE {
                    return self.superframe_fail(Error::invalid("wma: superframe overflow"));
                }
                let q = self.last_superframe_len;
                for i in 0..buf_size - 1 {
                    self.last_superframe[q + i] = gb.get_bits(8)? as u8;
                }
                let end = q + buf_size - 1;
                self.last_superframe[end..end + SUPERFRAME_PADDING].fill(0);
                // FFmpeg adds a bit count here, not a byte count.
                self.last_superframe_len += 8 * buf_size - 8;
                return Ok(());
            }
        }

        if self.use_bit_reservoir {
            let bit_offset = gb.get_bits(self.byte_offset_bits as usize + 3)? as usize;
            if bit_offset > gb.bits_left() {
                return self.superframe_fail(Error::invalid("wma: invalid last frame bit offset"));
            }

            if self.last_superframe_len > 0 {
                // add bit_offset bits to the last frame
                if self.last_superframe_len + ((bit_offset + 7) >> 3) > MAX_CODED_SUPERFRAME_SIZE {
                    return self.superframe_fail(Error::invalid("wma: superframe overflow"));
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
                    q += 1;
                }
                self.last_superframe[q..q + SUPERFRAME_PADDING].fill(0);

                // this frame is stored in the last superframe and in the
                // current one
                let bits = self.last_superframe_len * 8 + bit_offset;
                let data = std::mem::take(&mut self.last_superframe);
                let res = {
                    let mut gb2 = BitReader::with_bit_len(&data, bits);
                    gb2.skip_bits(self.last_bitoffset).and_then(|_| self.wma_decode_frame(&mut gb2))
                };
                self.last_superframe = data;
                if let Err(e) = res {
                    return self.superframe_fail(e);
                }
                nb_frames -= 1;
            }

            // read each frame starting from bit_offset
            let pos = bit_offset + 4 + 4 + self.byte_offset_bits as usize + 3;
            if pos >= MAX_CODED_SUPERFRAME_SIZE * 8 || pos > buf_size * 8 {
                return Err(Error::invalid("wma: invalid bit offset"));
            }
            let mut gb2 = BitReader::new(&buf[pos >> 3..]);
            gb2.skip_bits(pos & 7)?;

            self.reset_block_lengths = true;
            for _ in 0..nb_frames {
                if let Err(e) = self.wma_decode_frame(&mut gb2) {
                    return self.superframe_fail(e);
                }
            }

            // copy the end of the frame into the last frame buffer
            let pos = gb2.bits_count() + (pos & !7);
            self.last_bitoffset = pos & 7;
            let pos = pos >> 3;
            if pos > buf_size || buf_size - pos > MAX_CODED_SUPERFRAME_SIZE {
                return self.superframe_fail(Error::invalid("wma: invalid superframe tail length"));
            }
            let len = buf_size - pos;
            self.last_superframe_len = len;
            self.last_superframe[..len].copy_from_slice(&buf[pos..]);
        } else if let Err(e) = self.wma_decode_frame(&mut gb) {
            // single frame decode
            return self.superframe_fail(e);
        }
        Ok(())
    }
}

/// Zeroed bytes kept after the reservoir data (FFmpeg's
/// `AV_INPUT_BUFFER_PADDING_SIZE`).
const SUPERFRAME_PADDING: usize = 64;

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

    /// FFmpeg's decode loop: each call consumes one `block_align`
    /// superframe and the rest of the packet is fed again. A superframe
    /// FFmpeg rejects (short remainder, corrupt data) yields no output and
    /// drops the rest of the packet; decoding resumes with the next packet,
    /// as in FFmpeg.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut data = &packet.data[..];
        while !data.is_empty() {
            if self.wma_decode_superframe(data).is_err() {
                break;
            }
            data = &data[self.block_align_len..];
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        while !self.pending.is_empty() {
            let mut frame = self.pending.remove(0);
            if crate::wma_common::discard_samples(&mut self.skip_samples, &mut frame, 4) {
                return Ok(Frame::Audio(frame));
            }
        }
        Err(if self.eof_done { Error::Eof } else { Error::NeedMore })
    }

    /// End of stream: emit the last overlap frame (FFmpeg's empty-packet
    /// drain, `AV_CODEC_CAP_DELAY`).
    fn flush(&mut self) -> Result<()> {
        self.wma_decode_superframe(&[])
    }

    /// Seek: FFmpeg's `flush` callback.
    fn reset(&mut self) -> Result<()> {
        self.last_bitoffset = 0;
        self.last_superframe_len = 0;
        self.eof_done = false;
        self.skip_samples = self.frame_len * 2;
        self.pending.clear();
        Ok(())
    }

    fn output_audio_format(&self) -> Option<oxideav_core::AudioFormat> {
        Some(oxideav_core::AudioFormat {
            sample_format: SampleFormat::F32P,
            sample_rate: self.sample_rate,
            channels: self.channels as u16,
        })
    }
}
