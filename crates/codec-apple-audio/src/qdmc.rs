// QDMC (QDesign Music Codec 1) decoder.
//
// Ported from FFmpeg libavcodec/qdmc.c (commit 2da55bf), LGPL-2.1-or-later.

//! QDMC: noise bands and tone lists read from a little-endian bitstream,
//! synthesized in the frequency domain and turned to sound by an inverse
//! FFT per 1/32 of a frame (`tx`, FFmpeg's C FFT), with overlap-add into
//! a running buffer. Output is interleaved S16, `frame_size` (2048, 4096 or
//! 8192) samples per channel per frame; a packet holds one frame of
//! `checksum_size` bytes. The arithmetic follows FFmpeg's C, including the
//! multiply-adds clang fuses on arm64.

use std::collections::VecDeque;
use std::sync::LazyLock;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

use crate::getbits::GetBitsLe;
use crate::tx::{Complex, Fft};
use crate::vlc::Vlc;

/// `code_prefix`
#[rustfmt::skip]
const CODE_PREFIX: [u32; 65] = [
    0x0, 0x1, 0x2, 0x3, 0x4, 0x6, 0x8, 0xa,
    0xc, 0x10, 0x14, 0x18, 0x1c, 0x24, 0x2c, 0x34,
    0x3c, 0x4c, 0x5c, 0x6c, 0x7c, 0x9c, 0xbc, 0xdc,
    0xfc, 0x13c, 0x17c, 0x1bc, 0x1fc, 0x27c, 0x2fc, 0x37c,
    0x3fc, 0x4fc, 0x5fc, 0x6fc, 0x7fc, 0x9fc, 0xbfc, 0xdfc,
    0xffc, 0x13fc, 0x17fc, 0x1bfc, 0x1ffc, 0x27fc, 0x2ffc, 0x37fc,
    0x3ffc, 0x4ffc, 0x5ffc, 0x6ffc, 0x7ffc, 0x9ffc, 0xbffc, 0xdffc,
    0xfffc, 0x13ffc, 0x17ffc, 0x1bffc, 0x1fffc, 0x27ffc, 0x2fffc, 0x37ffc,
    0x3fffc,
];

/// `amplitude_tab`
#[rustfmt::skip]
const AMPLITUDE_TAB: [f32; 64] = [
    1.18750000, 1.68359380, 2.37500000, 3.36718750, 4.75000000,
    6.73437500, 9.50000000, 13.4687500, 19.0000000, 26.9375000,
    38.0000000, 53.8750000, 76.0000000, 107.750000, 152.000000,
    215.500000, 304.000000, 431.000000, 608.000000, 862.000000,
    1216.00000, 1724.00000, 2432.00000, 3448.00000, 4864.00000,
    6896.00000, 9728.00000, 13792.0000, 19456.0000, 27584.0000,
    38912.0000, 55168.0000, 77824.0000, 110336.000, 155648.000,
    220672.000, 311296.000, 441344.000, 622592.000, 882688.000,
    1245184.00, 1765376.00, 2490368.00, 3530752.00, 4980736.00,
    7061504.00, 0.0, 0.0, 0.0, 0.0,
    0.0, 0.0, 0.0, 0.0, 0.0,
    0.0, 0.0, 0.0, 0.0, 0.0,
    0.0, 0.0, 0.0, 0.0,
];

/// `qdmc_nodes`
#[rustfmt::skip]
const QDMC_NODES: [u16; 112] = [
    0, 1, 2, 4, 6, 8, 12, 16, 24, 32, 48, 56, 64, 80,
    96, 120, 144, 176, 208, 240, 256, 0, 2, 4, 8, 16, 24, 32,
    48, 56, 64, 80, 104, 128, 160, 208, 256, 0, 0, 0, 0, 0,
    0, 2, 4, 8, 16, 32, 48, 64, 80, 112, 160, 208, 256, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 4, 8, 16, 32, 48, 64,
    96, 144, 208, 256, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 4, 16, 32, 64, 256, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// `noise_bands_size`
const NOISE_BANDS_SIZE: [usize; 7] = [19, 14, 11, 9, 4, 2, 0];

/// `noise_bands_selector`
const NOISE_BANDS_SELECTOR: [usize; 7] = [4, 3, 2, 1, 0, 0, 0];

/// `qdmc_hufftab`: `[symbol, length]` in code order, the six tables of
/// `HUFF_SIZES` one after the other.
#[rustfmt::skip]
const QDMC_HUFFTAB: [[u8; 2]; 132] = [
    [1, 2], [10, 7], [26, 9], [22, 9], [24, 9], [14, 9],
    [8, 6], [6, 5], [7, 5], [9, 7], [30, 9], [32, 10],
    [13, 10], [20, 9], [28, 9], [12, 7], [15, 11], [36, 12],
    [0, 12], [34, 10], [18, 9], [11, 9], [16, 9], [5, 3],
    [2, 3], [4, 3], [3, 2], [1, 1], [2, 2], [3, 4],
    [8, 9], [9, 10], [0, 10], [13, 8], [7, 7], [6, 6],
    [17, 5], [4, 4], [5, 4], [18, 3], [16, 3], [22, 7],
    [8, 10], [4, 10], [3, 9], [2, 8], [23, 8], [10, 8],
    [11, 7], [21, 5], [20, 4], [1, 7], [7, 10], [5, 10],
    [9, 9], [6, 10], [25, 11], [26, 12], [27, 13], [0, 13],
    [24, 9], [12, 6], [13, 5], [14, 4], [19, 3], [15, 3],
    [17, 2], [2, 4], [14, 6], [26, 7], [31, 8], [32, 9],
    [35, 9], [7, 5], [10, 5], [22, 7], [27, 7], [19, 7],
    [20, 7], [4, 5], [13, 5], [17, 6], [15, 6], [8, 5],
    [5, 4], [28, 7], [33, 9], [36, 11], [38, 12], [42, 14],
    [45, 16], [44, 18], [0, 18], [46, 17], [43, 15], [40, 13],
    [37, 11], [39, 12], [41, 12], [34, 8], [16, 6], [11, 5],
    [9, 4], [1, 2], [3, 4], [30, 7], [29, 7], [23, 6],
    [24, 6], [18, 6], [6, 4], [12, 5], [21, 6], [25, 6],
    [1, 2], [3, 3], [4, 4], [5, 5], [6, 6], [7, 7],
    [8, 8], [0, 8], [2, 1], [2, 2], [1, 2], [3, 4],
    [7, 4], [6, 5], [5, 6], [0, 6], [4, 4], [8, 2],
];

/// `huff_sizes`
const HUFF_SIZES: [usize; 6] = [27, 12, 28, 47, 9, 9];

/// The six VLCs (`vtable`).
static VTABLES: LazyLock<Vec<Vlc>> = LazyLock::new(|| {
    let mut offset = 0;
    HUFF_SIZES
        .iter()
        .map(|&n| {
            let entries = QDMC_HUFFTAB[offset..offset + n].iter().map(|&[sym, len]| (i32::from(sym), i32::from(len)));
            offset += n;
            Vlc::from_lengths(entries, -1).expect("qdmc_hufftab")
        })
        .collect()
});

/// `sin_table`: sin(2 * i * π * 0.001953125), the product taken as C takes
/// it (float, then double).
static SIN_TABLE: LazyLock<[f32; 512]> = LazyLock::new(|| {
    std::array::from_fn(|i| (f64::from(2.0f32 * i as f32) * std::f64::consts::PI * f64::from(0.001953125f32)).sin() as f32)
});

#[derive(Clone, Copy, Default)]
struct Tone {
    mode: u8,
    phase: u8,
    offset: u8,
    freq: i16,
    amplitude: i16,
}

/// `QDMCContext`
struct Qdmc {
    frame_bits: u32,
    band_index: usize,
    frame_size: usize,
    subframe_size: usize,
    fft_offset: usize,
    buffer_offset: usize,
    nb_channels: usize,
    checksum_size: usize,
    noise: [[[u8; 17]; 19]; 2],
    tones: Vec<Vec<Tone>>,
    cur_tone: [usize; 5],
    alt_sin: [[f32; 31]; 5],
    /// `fft_buffer[4][8192 * 2]`
    fft_buffer: Vec<Vec<f32>>,
    noise2_buffer: Vec<f32>,
    noise_buffer: Vec<f32>,
    /// `buffer[2 * 32768]`
    buffer: Vec<f32>,
    /// `buffer_ptr`, as an index into `buffer`.
    buffer_ptr: usize,
    rndval: u32,
    cmplx_in: [Vec<Complex>; 2],
    cmplx_out: [Vec<Complex>; 2],
    fft: Fft,
}

/// The most tones a group holds (`tones[5][8192]`).
const MAX_TONES: usize = 8192;

/// `av_log2`
fn av_log2(v: u32) -> u32 {
    31 - (v | 1).leading_zeros()
}

/// `qdmc_get_vlc`: `None` for FFmpeg's error.
fn get_vlc(gb: &mut GetBitsLe, table: usize, flag: bool) -> Option<i32> {
    if gb.bits_left() < 1 {
        return None;
    }
    // Symbol 0 (-1 with the offset) and a miss both mean: the value
    // follows in 1 to 8 explicit bits.
    let mut v = VTABLES[table].read(gb).unwrap_or(-1);
    if v < 0 {
        let n = gb.get(3) + 1;
        v = gb.get(n) as i32;
    }
    if flag {
        let prefix = *CODE_PREFIX.get(usize::try_from(v).ok()?)?;
        v = (prefix + gb.get(v as u32 >> 2)) as i32;
    }
    Some(v)
}

impl Qdmc {
    /// `qdmc_decode_init`; also the sample rate.
    fn new(extradata: &[u8]) -> Result<(Self, u32)> {
        if extradata.len() < 48 {
            return Err(Error::invalid("qdmc: extradata missing or truncated"));
        }
        // bytestream2: look for "frmaQDMC" while more than 8 bytes are left.
        let mut pos = 0;
        while extradata.len() - pos > 8 && &extradata[pos..pos + 8] != b"frmaQDMC" {
            pos += 1;
        }
        pos += 8;
        let left = extradata.len().saturating_sub(pos);
        if left < 36 {
            return Err(Error::invalid(format!("qdmc: not enough extradata ({left})")));
        }
        let be32 = |at: usize| u32::from_be_bytes(extradata[at..at + 4].try_into().unwrap());
        let size = be32(pos) as usize;
        if size > left - 4 {
            return Err(Error::invalid("qdmc: extradata size too small"));
        }
        if &extradata[pos + 4..pos + 8] != b"QDCA" {
            return Err(Error::invalid("qdmc: invalid extradata, expecting QDCA"));
        }
        let nb_channels = be32(pos + 12) as usize;
        if !(1..=2).contains(&nb_channels) {
            return Err(Error::invalid("qdmc: invalid number of channels"));
        }
        let sample_rate = be32(pos + 16);
        let bit_rate = be32(pos + 20);
        let fft_size = be32(pos + 28);
        let fft_order = av_log2(fft_size) + 1;
        let checksum_size = be32(pos + 32);
        if checksum_size >= 1 << 28 {
            return Err(Error::invalid("qdmc: data block size too large"));
        }

        // `avctx->sample_rate` is an int.
        let (mut x, frame_bits) = if sample_rate as i32 >= 32000 {
            (28000, 13)
        } else if sample_rate as i32 >= 16000 {
            (20000, 12)
        } else {
            (16000, 11)
        };
        let frame_size = 1usize << frame_bits;
        let subframe_size = frame_size >> 5;
        if nb_channels == 2 {
            x = 3 * x / 2;
        }
        let selector = (f64::from(bit_rate) * 3.0 / f64::from(x) + 0.5).floor().round_ties_even() as i64;
        let band_index = NOISE_BANDS_SELECTOR[selector.min(6) as usize];

        if !(7..=9).contains(&fft_order) {
            return Err(Error::unsupported(format!("qdmc: unknown FFT order {fft_order}")));
        }
        if fft_size != 1 << (fft_order - 1) {
            return Err(Error::invalid(format!("qdmc: FFT size {fft_size} not power of 2")));
        }

        let mut alt_sin = [[0f32; 31]; 5];
        for g in (1..=5).rev() {
            for j in 0..(1usize << g) - 1 {
                alt_sin[5 - g][j] = SIN_TABLE[((j + 1) << (8 - g)) & 0x1FF];
            }
        }
        let mut s = Self {
            frame_bits,
            band_index,
            frame_size,
            subframe_size,
            fft_offset: 0,
            buffer_offset: 0,
            nb_channels,
            checksum_size: checksum_size as usize,
            noise: [[[0; 17]; 19]; 2],
            tones: vec![Vec::new(); 5],
            cur_tone: [0; 5],
            alt_sin,
            fft_buffer: vec![vec![0.0; 8192 * 2]; 4],
            noise2_buffer: vec![0.0; 4096 * 2],
            noise_buffer: vec![0.0; 4096 * 2],
            buffer: vec![0.0; 2 * 32768],
            buffer_ptr: 0,
            rndval: 0,
            cmplx_in: [vec![Complex::default(); 512], vec![Complex::default(); 512]],
            cmplx_out: [vec![Complex::default(); 512], vec![Complex::default(); 512]],
            fft: Fft::new(1 << fft_order, true),
        };
        s.make_noises();
        Ok((s, sample_rate))
    }

    /// `make_noises`
    fn make_noises(&mut self) {
        let base = 21 * self.band_index;
        for j in 0..NOISE_BANDS_SIZE[self.band_index] {
            let n0 = usize::from(QDMC_NODES[j + base]);
            let n1 = usize::from(QDMC_NODES[j + base + 1]);
            let n2 = usize::from(QDMC_NODES[j + base + 2]);
            let at = 256 * j;
            for i in 0..n1.saturating_sub(n0) {
                self.noise_buffer[at + i] = i as f32 / (n1 - n0) as f32;
            }
            let at = (j << 8) + n1 - n0;
            let mut diff = n2 as i32 - n1 as i32;
            for i in 0..n2.saturating_sub(n1) {
                self.noise_buffer[at + i] = diff as f32 / (n2 - n1) as f32;
                diff -= 1;
            }
        }
    }

    /// `qdmc_flush`
    fn flush(&mut self) {
        self.buffer.fill(0.0);
        for b in &mut self.fft_buffer {
            b.fill(0.0);
        }
        self.fft_offset = 0;
        self.buffer_offset = 0;
    }

    /// `skip_label`: the frame label and checksum.
    fn skip_label(&self, gb: &mut GetBitsLe) -> bool {
        let label = gb.get(32);
        let checksum = gb.get(16) as u16;
        if label != u32::from_le_bytes([b'Q', b'M', b'C', 1]) {
            return false;
        }
        let data = gb.data();
        let mut sum: u16 = 226;
        for i in 0..self.checksum_size.saturating_sub(6) {
            sum = sum.wrapping_add(u16::from(data.get(6 + i).copied().unwrap_or(0)));
        }
        sum == checksum
    }

    /// `read_noise_data`
    fn read_noise_data(&mut self, gb: &mut GetBitsLe) -> Option<()> {
        for ch in 0..self.nb_channels {
            for band in 0..NOISE_BANDS_SIZE[self.band_index] {
                let mut v = get_vlc(gb, 0, false)?;
                v = if v & 1 != 0 { v + 1 } else { -v };
                let mut lastval = v / 2;
                self.noise[ch][band][0] = (lastval - 1) as u8;
                let mut j = 0;
                while j < 15 {
                    let len = get_vlc(gb, 1, true)? + 1;
                    let v = get_vlc(gb, 0, false)?;
                    let newval = if v & 1 != 0 { lastval + (v + 1) / 2 } else { lastval - v / 2 };
                    let idx = j + 1;
                    if len + idx > 16 {
                        return None;
                    }
                    for (k, i) in (idx..=j + len).enumerate() {
                        let k = k as i32 + 1;
                        self.noise[ch][band][i as usize] = (lastval + k * (newval - lastval) / len - 1) as u8;
                    }
                    lastval = newval;
                    j += len;
                }
            }
        }
        Some(())
    }

    /// `add_tone`
    fn add_tone(&mut self, group: usize, offset: i32, freq: i32, stereo_mode: i32, amplitude: i32, phase: i32) {
        if self.tones[group].len() >= MAX_TONES {
            return;
        }
        self.tones[group].push(Tone {
            offset: offset as u8,
            freq: freq as i16,
            mode: stereo_mode as u8,
            amplitude: amplitude as i16,
            phase: phase as u8,
        });
    }

    /// `read_wave_data`
    fn read_wave_data(&mut self, gb: &mut GetBitsLe) -> Option<()> {
        let mut stereo_mode = 0;
        for group in 0..5 {
            let group_size = 1i32 << (self.frame_bits - group as u32 - 1);
            let group_bits = 4 - group as i32;
            let mut pos2 = 0i32;
            let mut off = 0i32;
            let mut i = 1i32;
            loop {
                let v = get_vlc(gb, 3, true)?;
                let mut freq = i + v;
                while freq >= group_size - 1 {
                    freq += 2 - group_size;
                    pos2 += group_size;
                    off += 1 << group_bits;
                }
                if pos2 >= self.frame_size as i32 {
                    break;
                }
                if self.nb_channels > 1 {
                    stereo_mode = gb.get(2) as i32;
                }
                let amp = get_vlc(gb, 2, false)?;
                let phase = gb.get(3) as i32;
                let (mut amp2, mut phase2) = (0, 0);
                if stereo_mode > 1 {
                    amp2 = amp - get_vlc(gb, 4, false)?;
                    phase2 = phase - get_vlc(gb, 5, false)?;
                    if phase2 < 0 {
                        phase2 += 8;
                    }
                }
                if (freq >> group_bits) + 1 < self.subframe_size as i32 {
                    self.add_tone(group, off, freq, stereo_mode & 1, amp, phase);
                    if stereo_mode > 1 {
                        self.add_tone(group, off, freq, !stereo_mode & 1, amp2, phase2);
                    }
                }
                i = freq + 1;
            }
        }
        Some(())
    }

    /// `lin_calc`
    fn lin_calc(&mut self, amplitude: f32, node1: usize, node2: usize, index: usize) {
        let scale = (0.5 * f64::from(amplitude)) as f32;
        let subframe_size = self.subframe_size.min(node2);
        // FFmpeg runs this in steps of 4 and then the rest; each element
        // gets the same single `noise2 += scale * noise` (one expression,
        // fused on arm64).
        let noise = &self.noise_buffer[256 * index..];
        for i in 0..subframe_size.saturating_sub(node1) {
            let j = node1 + i;
            self.noise2_buffer[j] = scale.mul_add(noise[i], self.noise2_buffer[j]);
        }
    }

    /// `add_noise`
    fn add_noise(&mut self, ch: usize, current_subframe: usize) {
        let base = self.fft_offset + self.subframe_size * current_subframe;
        self.noise2_buffer[..self.subframe_size].fill(0.0);
        let band_base = 21 * self.band_index;
        for i in 0..NOISE_BANDS_SIZE[self.band_index] {
            if usize::from(QDMC_NODES[i + band_base]) > self.subframe_size - 1 {
                break;
            }
            let aindex = self.noise[ch][i][current_subframe / 2];
            let amplitude = if aindex > 0 { AMPLITUDE_TAB[usize::from(aindex & 0x3F)] } else { 0.0 };
            self.lin_calc(
                amplitude,
                usize::from(QDMC_NODES[band_base + i]),
                usize::from(QDMC_NODES[band_base + i + 2]),
                i,
            );
        }
        for j in 2..self.subframe_size - 1 {
            self.rndval = 214013u32.wrapping_mul(self.rndval).wrapping_add(2531011);
            let rnd_im = ((self.rndval & 0x7FFF) as f32 - 16384.0) * 0.000030517578 * self.noise2_buffer[j];
            self.rndval = 214013u32.wrapping_mul(self.rndval).wrapping_add(2531011);
            let rnd_re = ((self.rndval & 0x7FFF) as f32 - 16384.0) * 0.000030517578 * self.noise2_buffer[j];
            let (im, re) = (base + j, base + j);
            self.fft_buffer[ch][im] += rnd_im;
            self.fft_buffer[2 + ch][re] += rnd_re;
            self.fft_buffer[ch][im + 1] -= rnd_im;
            self.fft_buffer[2 + ch][re + 1] -= rnd_re;
        }
    }

    /// `add_wave`
    fn add_wave(&mut self, offset: usize, freqs: i32, group: usize, mut stereo_mode: usize, amp: i32, phase: i32) {
        if self.nb_channels == 1 {
            stereo_mode = 0;
        }
        let group_bits = 4 - group as i32;
        let pos = (freqs >> (4 - group)) as usize;
        let amplitude = AMPLITUDE_TAB[(amp & 0x3F) as usize];
        let mut at = self.fft_offset + self.subframe_size * offset + pos;
        let end = 2 * self.frame_size;
        let mut pindex = (phase << 6) - ((2 * (freqs >> (4 - group)) + 1) << 7);
        for j in 0..(1usize << (group_bits + 1)) - 1 {
            pindex += (2 * freqs + 1) << (7 - group_bits);
            let level = amplitude * self.alt_sin[group][j];
            let im = level * SIN_TABLE[(pindex & 0x1FF) as usize];
            let re = level * SIN_TABLE[((pindex + 128) & 0x1FF) as usize];
            self.fft_buffer[stereo_mode][at] += im;
            self.fft_buffer[stereo_mode][at + 1] -= im;
            self.fft_buffer[2 + stereo_mode][at] += re;
            self.fft_buffer[2 + stereo_mode][at + 1] -= re;
            at += self.subframe_size;
            if at >= end {
                at = pos;
            }
        }
    }

    /// `add_wave0`
    fn add_wave0(&mut self, offset: usize, freqs: i32, mut stereo_mode: usize, amp: i32, phase: i32) {
        if self.nb_channels == 1 {
            stereo_mode = 0;
        }
        let level = AMPLITUDE_TAB[(amp & 0x3F) as usize];
        let im = level * SIN_TABLE[((phase << 6) & 0x1FF) as usize];
        let re = level * SIN_TABLE[(((phase << 6) + 128) & 0x1FF) as usize];
        let pos = self.fft_offset + freqs as usize + self.subframe_size * offset;
        self.fft_buffer[stereo_mode][pos] += im;
        self.fft_buffer[2 + stereo_mode][pos] += re;
        self.fft_buffer[stereo_mode][pos + 1] -= im;
        self.fft_buffer[2 + stereo_mode][pos + 1] -= re;
    }

    /// `add_waves`
    fn add_waves(&mut self, current_subframe: usize) {
        for g in 0..5 {
            let mut w = self.cur_tone[g];
            while w < self.tones[g].len() {
                let t = self.tones[g][w];
                if current_subframe < usize::from(t.offset) {
                    break;
                }
                let (offset, freq, mode) = (usize::from(t.offset), i32::from(t.freq), usize::from(t.mode));
                if g < 4 {
                    self.add_wave(offset, freq, g, mode, i32::from(t.amplitude), i32::from(t.phase));
                } else {
                    self.add_wave0(offset, freq, mode, i32::from(t.amplitude), i32::from(t.phase));
                }
                w += 1;
            }
            self.cur_tone[g] = w;
        }
    }

    /// `decode_frame`: one frame into `out` (interleaved).
    fn decode_frame(&mut self, gb: &mut GetBitsLe, out: &mut [i16]) -> Option<()> {
        if !self.skip_label(gb) {
            return None;
        }
        self.fft_offset = self.frame_size - self.fft_offset;
        self.buffer_ptr = self.nb_channels * self.buffer_offset;

        self.read_noise_data(gb)?;
        self.read_wave_data(gb)?;

        let (nch, sub) = (self.nb_channels, self.subframe_size);
        for n in 0..32 {
            for ch in 0..nch {
                self.add_noise(ch, n);
            }
            self.add_waves(n);
            for ch in 0..nch {
                let base = self.fft_offset + n * sub;
                for i in 0..sub {
                    self.cmplx_in[ch][i] = Complex { re: self.fft_buffer[ch + 2][base + i], im: self.fft_buffer[ch][base + i] };
                    self.cmplx_in[ch][sub + i] = Complex::default();
                }
            }
            for ch in 0..nch {
                self.fft.run(&mut self.cmplx_out[ch], &self.cmplx_in[ch]);
            }
            let r = self.buffer_ptr + nch * n * sub;
            for i in 0..2 * sub {
                for ch in 0..nch {
                    self.buffer[r + i * nch + ch] += self.cmplx_out[ch][i].re;
                }
            }
            let r = self.buffer_ptr + n * sub * nch;
            for (i, o) in out[n * sub * nch..(n + 1) * sub * nch].iter_mut().enumerate() {
                // av_clipf, then the float-to-int16 conversion (truncation).
                *o = self.buffer[r + i].clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
            }
            for ch in 0..nch {
                let base = self.fft_offset + n * sub;
                self.fft_buffer[ch][base..base + sub].fill(0.0);
                self.fft_buffer[ch + 2][base..base + sub].fill(0.0);
            }
            let clear = nch * (n * sub + self.frame_size + self.buffer_offset);
            self.buffer[clear..clear + sub * nch].fill(0.0);
        }

        self.buffer_offset += self.frame_size;
        if self.buffer_offset >= 32768 - self.frame_size {
            let from = nch * self.buffer_offset;
            self.buffer.copy_within(from..from + self.frame_size * nch, 0);
            self.buffer_offset = 0;
        }
        Some(())
    }
}

/// The `qdmc` decoder.
pub struct QdmcDecoder {
    codec_id: CodecId,
    s: Qdmc,
    /// The rate the `QDCA` atom gives, as FFmpeg takes it.
    sample_rate: u32,
    out: VecDeque<Frame>,
}

impl Decoder for QdmcDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    /// `qdmc_decode_frame`, called as libavcodec calls it: every
    /// `checksum_size` bytes of the packet is one frame; a shorter
    /// remainder is invalid. A frame that fails to decode resets the
    /// synthesis state (`qdmc_flush`).
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut data = &packet.data[..];
        let mut pts = packet.pts;
        let checksum_size = self.s.checksum_size;
        while !data.is_empty() {
            if checksum_size == 0 || data.len() < checksum_size {
                return Err(Error::invalid("qdmc: packet shorter than its frame"));
            }
            let mut out = vec![0i16; self.s.frame_size * self.s.nb_channels];
            let mut gb = GetBitsLe::new(&data[..checksum_size]);
            for tones in &mut self.s.tones {
                tones.clear();
            }
            self.s.cur_tone = [0; 5];
            if self.s.decode_frame(&mut gb, &mut out).is_none() {
                self.s.flush();
                return Err(Error::invalid("qdmc: invalid frame"));
            }
            let mut bytes = Vec::with_capacity(out.len() * 2);
            for s in out {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
            self.out.push_back(Frame::Audio(AudioFrame {
                samples: self.s.frame_size as u32,
                pts: pts.take(),
                data: vec![bytes],
            }));
            data = &data[checksum_size..];
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.out.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.out.clear();
        self.s.flush();
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: SampleFormat::S16,
            sample_rate: self.sample_rate,
            channels: self.s.nb_channels as u16,
        })
    }
}

/// Decoder factory. `params.extradata` carries the `frma`/`QDCA` atoms
/// (MOV sample entry, CAF `kuki`).
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let (s, sample_rate) = Qdmc::new(&params.extradata)?;
    Ok(Box::new(QdmcDecoder { codec_id: params.codec_id.clone(), s, sample_rate, out: VecDeque::new() }))
}
