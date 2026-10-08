// Port of FFmpeg's ATRAC1 decoder (libavcodec/atrac1.c, atrac1data.h,
// FFmpeg commit 2da55bf).
// Copyright (c) 2009 Maxim Poliakovski, (c) 2009 Benjamin Larsson;
// LGPL-2.1-or-later (see LICENSE).

use oxideav_core::{CodecParameters, Decoder, Error, Result};

use crate::bits::BitReader;
use crate::common::{QmfDelay, SF_TABLE, iqmf};
use crate::frames::{AudioDecoder, FrameCodec, Planes, block_align, channels};
use crate::tx::Imdct;

const AT1_MAX_BFU: usize = 52;
const AT1_SU_SIZE: usize = 212;
const AT1_SU_SAMPLES: usize = 512;
const AT1_SU_MAX_BITS: usize = AT1_SU_SIZE * 8;
const AT1_MAX_CHANNELS: u16 = 8;
const AT1_QMF_BANDS: usize = 3;

const SAMPLES_PER_BAND: [usize; 3] = [128, 128, 256];
const MDCT_LONG_NBITS: [u32; 3] = [7, 7, 8];

const BFU_AMOUNT_TAB1: [usize; 8] = [20, 28, 32, 36, 40, 44, 48, 52];
const BFU_AMOUNT_TAB2: [usize; 4] = [0, 112, 176, 208];
const BFU_AMOUNT_TAB3: [usize; 8] = [0, 24, 36, 48, 72, 108, 132, 156];
const BFU_BANDS_T: [usize; 4] = [0, 20, 36, 52];
const SPECS_PER_BFU: [usize; 52] = [
    8, 8, 8, 8, 4, 4, 4, 4, 8, 8, 8, 8, 6, 6, 6, 6, 6, 6, 6, 6, // low band
    6, 6, 6, 6, 7, 7, 7, 7, 9, 9, 9, 9, 10, 10, 10, 10, // middle band
    12, 12, 12, 12, 12, 12, 12, 12, 20, 20, 20, 20, 20, 20, 20, 20, // high band
];
const BFU_START_LONG: [usize; 52] = [
    0, 8, 16, 24, 32, 36, 40, 44, 48, 56, 64, 72, 80, 86, 92, 98, 104, 110, 116, 122, 128, 134,
    140, 146, 152, 159, 166, 173, 180, 189, 198, 207, 216, 226, 236, 246, 256, 268, 280, 292, 304,
    316, 328, 340, 352, 372, 392, 412, 432, 452, 472, 492,
];
const BFU_START_SHORT: [usize; 52] = [
    0, 32, 64, 96, 8, 40, 72, 104, 12, 44, 76, 108, 20, 52, 84, 116, 26, 58, 90, 122, 128, 160,
    192, 224, 134, 166, 198, 230, 141, 173, 205, 237, 150, 182, 214, 246, 256, 288, 320, 352, 384,
    416, 448, 480, 268, 300, 332, 364, 396, 428, 460, 492,
];

/// `vector_fmul_window_c` with `len` = 16: overlap-adds 16 previous and 16
/// new samples under a 32-tap window into 32 output samples.
pub(crate) fn fmul_window(dst: &mut [f32], src0: &[f32], src1: &[f32], win: &[f32], len: usize) {
    for k in 0..len {
        let s0 = src0[k];
        let s1 = src1[len - 1 - k];
        let wi = win[k];
        let wj = win[2 * len - 1 - k];
        dst[k] = s0 * wj - s1 * wi;
        dst[2 * len - 1 - k] = s0 * wi + s1 * wj;
    }
}

/// `ff_sine_window_init(window, n)`.
pub(crate) fn sine_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (((i as f64 + 0.5) * (std::f64::consts::PI / (2.0 * n as f64))) as f32).sin())
        .collect()
}

/// `AT1SUCtx`: one channel's state.
struct SoundUnit {
    log2_block_count: [u32; AT1_QMF_BANDS],
    /// FFmpeg's `spectrum[0]` / `spectrum[1]` swap each frame: `spectrum[cur]`
    /// is `spectrum[0]`.
    spectrum: [[f32; AT1_SU_SAMPLES]; 2],
    cur: usize,
    fst_qmf_delay: QmfDelay,
    snd_qmf_delay: QmfDelay,
    last_qmf_delay: [f32; 256 + 39],
}

impl SoundUnit {
    fn new() -> Self {
        Self {
            log2_block_count: [0; AT1_QMF_BANDS],
            spectrum: [[0.0; AT1_SU_SAMPLES]; 2],
            cur: 0,
            fst_qmf_delay: [0.0; 46],
            snd_qmf_delay: [0.0; 46],
            last_qmf_delay: [0.0; 256 + 39],
        }
    }
}

/// `AT1Ctx`.
struct Atrac1 {
    channels: usize,
    block_align: usize,
    units: Vec<SoundUnit>,
    spec: [f32; AT1_SU_SAMPLES],
    bands: [[f32; 256]; 3],
    /// Transforms of 32, 128 and 256 coefficients.
    mdct: [Imdct; 3],
    sine32: Vec<f32>,
}

/// `at1_parse_bsm`: the block size mode byte.
fn parse_bsm(gb: &mut BitReader) -> Result<[u32; AT1_QMF_BANDS]> {
    let mut log2 = [0u32; AT1_QMF_BANDS];
    for v in log2.iter_mut().take(2) {
        // low and mid band
        let tmp = gb.get(2);
        if tmp & 1 != 0 {
            return Err(Error::invalid("atrac1: invalid block size mode"));
        }
        *v = 2 - tmp;
    }
    // high band
    let tmp = gb.get(2);
    if tmp != 0 && tmp != 3 {
        return Err(Error::invalid("atrac1: invalid block size mode"));
    }
    log2[2] = 3 - tmp;
    gb.skip(2);
    Ok(log2)
}

/// `at1_unpack_dequant`: the dequantized MDCT spectrum of one sound unit.
fn unpack_dequant(
    gb: &mut BitReader,
    log2_block_count: &[u32; AT1_QMF_BANDS],
    spec: &mut [f32; AT1_SU_SAMPLES],
) -> Result<()> {
    let num_bfus = BFU_AMOUNT_TAB1[gb.get(3) as usize];
    let mut bits_used = num_bfus * 10
        + 32
        + BFU_AMOUNT_TAB2[gb.get(2) as usize]
        + (BFU_AMOUNT_TAB3[gb.get(3) as usize] << 1);

    let mut idwls = [0u32; AT1_MAX_BFU];
    let mut idsfs = [0usize; AT1_MAX_BFU];
    for w in idwls.iter_mut().take(num_bfus) {
        *w = gb.get(4);
    }
    for s in idsfs.iter_mut().take(num_bfus) {
        *s = gb.get(6) as usize;
    }

    let sf_table = &*SF_TABLE;
    for band in 0..AT1_QMF_BANDS {
        for bfu in BFU_BANDS_T[band]..BFU_BANDS_T[band + 1] {
            let num_specs = SPECS_PER_BFU[bfu];
            let word_len = u32::from(idwls[bfu] != 0) + idwls[bfu];
            let scale_factor = sf_table[idsfs[bfu]];
            bits_used += word_len as usize * num_specs;
            if bits_used > AT1_SU_MAX_BITS {
                return Err(Error::invalid("atrac1: bitstream overflow"));
            }
            let pos = if log2_block_count[band] != 0 {
                BFU_START_SHORT[bfu]
            } else {
                BFU_START_LONG[bfu]
            };
            let out = &mut spec[pos..pos + num_specs];
            if word_len != 0 {
                let max_quant = (1.0f64 / f64::from(((1i32 << (word_len - 1)) - 1) as f32)) as f32;
                for v in out.iter_mut() {
                    *v = gb.get_s(word_len) as f32 * scale_factor * max_quant;
                }
            } else {
                out.fill(0.0);
            }
        }
    }
    Ok(())
}

impl Atrac1 {
    /// `at1_imdct_block`: the per-band inverse MDCTs with windowed overlap.
    fn imdct_block(&mut self, ch: usize) -> Result<()> {
        let su = &mut self.units[ch];
        let cur = su.cur;
        let (mut ref_pos, mut pos) = (0usize, 0usize);
        for band in 0..AT1_QMF_BANDS {
            let band_samples = SAMPLES_PER_BAND[band];
            let log2 = su.log2_block_count[band];
            let num_blocks = 1usize << log2;
            let (block_size, nbits) = if num_blocks == 1 {
                let nbits = MDCT_LONG_NBITS[band] - log2;
                if nbits != 5 && nbits != 7 && nbits != 8 {
                    return Err(Error::invalid("atrac1: invalid transform size"));
                }
                (band_samples >> log2, nbits)
            } else {
                (32, 5)
            };
            let transf = 1usize << nbits;
            let tx = &self.mdct[(nbits - 5 - u32::from(nbits > 6)) as usize];

            let mut start_pos = 0usize;
            // previous frame's tail first, then this frame's previous block
            let mut prev = (1 - cur, ref_pos + band_samples - 16);
            for _ in 0..num_blocks {
                let coeffs = &mut self.spec[pos..pos + transf];
                if band != 0 {
                    coeffs.reverse();
                }
                let at = ref_pos + start_pos;
                tx.half(&mut su.spectrum[cur][at..at + transf], coeffs);
                let (pb, po) = prev;
                let mut prev_samples = [0f32; 16];
                prev_samples.copy_from_slice(&su.spectrum[pb][po..po + 16]);
                fmul_window(
                    &mut self.bands[band][start_pos..start_pos + 32],
                    &prev_samples,
                    &su.spectrum[cur][at..at + 16],
                    &self.sine32,
                    16,
                );
                prev = (cur, at + 16);
                start_pos += block_size;
                pos += block_size;
            }
            if num_blocks == 1 {
                let n = band_samples - 32;
                self.bands[band][32..32 + n]
                    .copy_from_slice(&su.spectrum[cur][ref_pos + 16..ref_pos + 16 + n]);
            }
            ref_pos += band_samples;
        }
        // swap buffers so the overlap works
        su.cur = 1 - cur;
        Ok(())
    }

    /// `at1_subband_synthesis`.
    fn subband_synthesis(&mut self, ch: usize, out: &mut [f32]) {
        let SoundUnit {
            fst_qmf_delay,
            snd_qmf_delay,
            last_qmf_delay,
            ..
        } = &mut self.units[ch];
        let mut temp = [0f32; 256];
        iqmf(
            &self.bands[0][..128],
            &self.bands[1][..128],
            128,
            &mut temp,
            fst_qmf_delay,
        );
        // delay the high band by 39 samples
        last_qmf_delay.copy_within(256..256 + 39, 0);
        last_qmf_delay[39..39 + 256].copy_from_slice(&self.bands[2][..256]);
        iqmf(&temp, &last_qmf_delay[..256], 256, out, snd_qmf_delay);
    }
}

impl FrameCodec for Atrac1 {
    /// `atrac1_decode_frame`.
    fn decode(&mut self, data: &[u8]) -> Result<(usize, Option<Planes>)> {
        if data.len() < AT1_SU_SIZE * self.channels {
            return Err(Error::invalid("atrac1: not enough data to decode"));
        }
        let mut planes = vec![vec![0f32; AT1_SU_SAMPLES]; self.channels];
        for ch in 0..self.channels {
            let mut gb = BitReader::new(
                &data[AT1_SU_SIZE * ch..AT1_SU_SIZE * (ch + 1)],
                AT1_SU_SIZE * 8,
            );
            let log2 = parse_bsm(&mut gb)?;
            self.units[ch].log2_block_count = log2;
            unpack_dequant(&mut gb, &log2, &mut self.spec)?;
            self.imdct_block(ch)?;
            self.subband_synthesis(ch, &mut planes[ch]);
        }
        Ok((self.block_align, Some(planes)))
    }
}

/// `atrac1_decode_init`.
fn make_codec(params: &CodecParameters) -> Result<Box<dyn FrameCodec>> {
    let channels = channels(params, AT1_MAX_CHANNELS)?;
    let block_align =
        block_align(params).ok_or_else(|| Error::unsupported("atrac1: block align unknown"))?;
    let scale = -1.0 / f64::from(1 << 15);
    Ok(Box::new(Atrac1 {
        channels,
        block_align,
        units: (0..channels).map(|_| SoundUnit::new()).collect(),
        spec: [0.0; AT1_SU_SAMPLES],
        bands: [[0.0; 256]; 3],
        mdct: [
            Imdct::new(32, scale),
            Imdct::new(128, scale),
            Imdct::new(256, scale),
        ],
        sine32: sine_window(32),
    }))
}

pub(crate) fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    AudioDecoder::open(params, make_codec)
}
