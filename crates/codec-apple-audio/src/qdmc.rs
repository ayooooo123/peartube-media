//! QDMC (QDesign Music Codec 1) decoder.
//!
//! Ported from FFmpeg's `libavcodec/qdmc.c` (LGPL-2.1-or-later, commit
//! 2da55bf) to safe Rust: the `qdmc_hufftab` length tables, the
//! `code_prefix` amplitude escape ladder, noise-band data, the
//! per-group wave-tone reader, `add_wave`/`add_wave0`/`add_noise`
//! synthesis into the FFT buffer, the inverse-FFT per subframe, and
//! the overlap `buffer` bookkeeping (`buffer_offset` rotation).
//!
//! Output is interleaved S16, `frame_size` samples per channel per
//! packet (`frame_bits` 11/12/13 → 2048/4096/8192 samples).

#![allow(clippy::needless_range_loop)]
#![allow(clippy::too_many_arguments)]

use std::collections::VecDeque;

use crate::bits::{LeBitReader, VlcTable};
use crate::qdm2::rdft::{inverse_fft, Complex};
use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

const FRAME_SIZE_MAX: usize = 8192;

/// qdmc.c `code_prefix`.
#[rustfmt::skip]
static CODE_PREFIX: [u32; 65] = [
    0x0, 0x1, 0x2, 0x3, 0x4, 0x6, 0x8, 0xA,
    0xC, 0x10, 0x14, 0x18, 0x1C, 0x24, 0x2C, 0x34,
    0x3C, 0x4C, 0x5C, 0x6C, 0x7C, 0x9C, 0xBC, 0xDC,
    0xFC, 0x13C, 0x17C, 0x1BC, 0x1FC, 0x27C, 0x2FC, 0x37C,
    0x3FC, 0x4FC, 0x5FC, 0x6FC, 0x7FC, 0x9FC, 0xBFC, 0xDFC,
    0xFFC, 0x13FC, 0x17FC, 0x1BFC, 0x1FFC, 0x27FC, 0x2FFC, 0x37FC,
    0x3FFC, 0x4FFC, 0x5FFC, 0x6FFC, 0x7FFC, 0x9FFC, 0xBFFC, 0xDFFC,
    0xFFFC, 0x13FFC, 0x17FFC, 0x1BFFC, 0x1FFFC, 0x27FFC, 0x2FFFC, 0x37FFC,
    0x3FFFC,
];

/// qdmc.c `amplitude_tab`.
#[rustfmt::skip]
static AMPLITUDE_TAB: [f32; 65] = [
    1.1875, 1.6835938, 2.375, 3.3671875, 4.75,
    6.734375, 9.5, 13.46875, 19.0, 26.9375,
    38.0, 53.875, 76.0, 107.75, 152.0,
    215.5, 304.0, 431.0, 608.0, 862.0,
    1216.0, 1724.0, 2432.0, 3448.0, 4864.0,
    6896.0, 9728.0, 13792.0, 19456.0, 27584.0,
    38912.0, 55168.0, 77824.0, 110336.0, 155648.0,
    220672.0, 311296.0, 441344.0, 622592.0, 882688.0,
    1245184.0, 1765376.0, 2490368.0, 3530752.0, 4980736.0,
    7061504.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
    0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
];

/// qdmc.c `qdmc_nodes`.
#[rustfmt::skip]
static QDMC_NODES: [u16; 112] = [
    0, 1, 2, 4, 6, 8, 12, 16, 24, 32, 48, 56, 64,
    80, 96, 120, 144, 176, 208, 240, 256,
    0, 2, 4, 8, 16, 24, 32, 48, 56, 64, 80, 104,
    128, 160, 208, 256, 0, 0, 0, 0, 0,
    0, 2, 4, 8, 16, 32, 48, 64, 80, 112, 160, 208,
    256, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 4, 8, 16, 32, 48, 64, 96, 144, 208, 256,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 4, 16, 32, 64, 256, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

static NOISE_BANDS_SIZE: [usize; 7] = [19, 14, 11, 9, 4, 2, 0];
static NOISE_BANDS_SELECTOR: [usize; 7] = [4, 3, 2, 1, 0, 0, 0];

/// qdmc.c `qdmc_hufftab`: six tables of `{symbol, length}` pairs.
#[rustfmt::skip]
static QDMC_HUFFTAB: [[u8; 2]; 132] = [
    /* Noise value - 27 */
    [1,2],[10,7],[26,9],[22,9],[24,9],[14,9],[8,6],[6,5],[7,5],[9,7],
    [30,9],[32,10],[13,10],[20,9],[28,9],[12,7],[15,11],[36,12],[0,12],
    [34,10],[18,9],[11,9],[16,9],[5,3],[2,3],[4,3],[3,2],
    /* Noise segment length - 12 */
    [1,1],[2,2],[3,4],[8,9],[9,10],[0,10],[13,8],[7,7],[6,6],[17,5],
    [4,4],[5,4],
    /* Amplitude - 28 */
    [18,3],[16,3],[22,7],[8,10],[4,10],[3,9],[2,8],[23,8],[10,8],[11,7],
    [21,5],[20,4],[1,7],[7,10],[5,10],[9,9],[6,10],[25,11],[26,12],
    [27,13],[0,13],[24,9],[12,6],[13,5],[14,4],[19,3],[15,3],[17,2],
    /* Frequency differences - 47 */
    [2,4],[14,6],[26,7],[31,8],[32,9],[35,9],[7,5],[10,5],[22,7],[27,7],
    [19,7],[20,7],[4,5],[13,5],[17,6],[15,6],[8,5],[5,4],[28,7],[33,9],
    [36,11],[38,12],[42,14],[45,16],[44,18],[0,18],[46,17],[43,15],
    [40,13],[37,11],[39,12],[41,12],[34,8],[16,6],[11,5],[9,4],[1,2],
    [3,4],[30,7],[29,7],[23,6],[24,6],[18,6],[6,4],[12,5],[21,6],[25,6],
    /* Amplitude differences - 9 */
    [1,2],[3,3],[4,4],[5,5],[6,6],[7,7],[8,8],[0,8],[2,1],
    /* Phase differences - 9 */
    [2,2],[1,2],[3,4],[7,4],[6,5],[5,6],[0,6],[4,4],[8,2],
];

static HUFF_SIZES: [usize; 6] = [27, 12, 28, 47, 9, 9];
/// FFmpeg's per-table `nb_bits`; the Rust table builder derives its
/// own lookup breadth.
#[allow(dead_code)]
static HUFF_BITS: [u32; 6] = [12, 10, 12, 12, 8, 6];

struct Vtables {
    tabs: [VlcTable; 6],
}

static VTABLES: std::sync::LazyLock<Vtables> = std::sync::LazyLock::new(|| {
    let mut tabs: [VlcTable; 6] = Default::default();
    let mut off = 0usize;
    for (i, tab) in tabs.iter_mut().enumerate() {
        let slice = &QDMC_HUFFTAB[off..off + HUFF_SIZES[i]];
        let mut lens = vec![0i8; slice.iter().map(|r| r[0] as usize + 1).max().unwrap_or(0)];
        for row in slice {
            lens[row[0] as usize] = row[1] as i8;
        }
        *tab = VlcTable::from_lengths(&lens, -1).unwrap_or_default();
        off += HUFF_SIZES[i];
    }
    Vtables { tabs }
});

/// qdmc.c `sin_table[512]`: `sin(2 * i * π * 0.001953125)`.
static SIN_TABLE: std::sync::LazyLock<[f32; 512]> = std::sync::LazyLock::new(|| {
    let mut t = [0f32; 512];
    for (i, v) in t.iter_mut().enumerate() {
        *v = (2.0f32 * i as f32 * core::f32::consts::PI * 0.001953125).sin();
    }
    t
});

#[derive(Clone, Copy, Default)]
struct QdmcTone {
    mode: u8,
    phase: u8,
    offset: u8,
    freq: i16,
    amplitude: i16,
}

struct QdmcContext {
    frame_bits: u32,
    band_index: usize,
    frame_size: usize,
    subframe_size: usize,
    fft_offset: usize,
    buffer_offset: usize,
    nb_channels: usize,
    checksum_size: usize,

    noise: [[[u8; 17]; 19]; 2],
    /// `tones[5][8192]` flattened; group g owns row g.
    tones: Vec<[QdmcTone; 8192]>,
    nb_tones: [usize; 5],
    cur_tone: [usize; 5],
    alt_sin: [[f32; 31]; 5],
    /// `fft_buffer[4][8192*2]` flattened.
    fft_buffer: Box<[f32]>,
    noise2_buffer: Box<[f32]>,
    noise_buffer: Box<[f32]>,
    /// `buffer[2*32768]`.
    buffer: Box<[f32]>,
    buffer_ptr: usize,
    rndval: u32,

    cmplx_in: [[Complex; 512]; 2],
    cmplx_out: [[Complex; 512]; 2],
}

impl QdmcContext {
    fn new() -> Self {
        Self {
            frame_bits: 0,
            band_index: 0,
            frame_size: 0,
            subframe_size: 0,
            fft_offset: 0,
            buffer_offset: 0,
            nb_channels: 0,
            checksum_size: 0,
            noise: [[[0; 17]; 19]; 2],
            tones: vec![[QdmcTone::default(); 8192]; 5],
            nb_tones: [0; 5],
            cur_tone: [0; 5],
            alt_sin: [[0f32; 31]; 5],
            fft_buffer: vec![0f32; 4 * 8192 * 2].into_boxed_slice(),
            noise2_buffer: vec![0f32; 4096 * 2].into_boxed_slice(),
            noise_buffer: vec![0f32; 4096 * 2].into_boxed_slice(),
            buffer: vec![0f32; 2 * 32768].into_boxed_slice(),
            buffer_ptr: 0,
            rndval: 0,
            cmplx_in: [[Complex::default(); 512]; 2],
            cmplx_out: [[Complex::default(); 512]; 2],
        }
    }

    /// qdmc.c `make_noises`.
    fn make_noises(&mut self) {
        for j in 0..NOISE_BANDS_SIZE[self.band_index] {
            let base = 21 * self.band_index;
            let n0 = QDMC_NODES[j + base] as usize;
            let n1 = QDMC_NODES[j + base + 1] as usize;
            let n2 = QDMC_NODES[j + base + 2] as usize;

            let mut idx = 256 * j;
            let mut i = 0usize;
            while i + n0 < n1 {
                if idx < self.noise_buffer.len() {
                    self.noise_buffer[idx] = i as f32 / (n1 - n0) as f32;
                }
                i += 1;
                idx += 1;
            }

            let mut diff = (n2 - n1) as i64;
            let mut idx = (j << 8) + n1.saturating_sub(n0);
            for _ in n1..n2 {
                if idx < self.noise_buffer.len() {
                    self.noise_buffer[idx] = diff as f32 / (n2 - n1) as f32;
                }
                idx += 1;
                diff -= 1;
            }
        }
    }

    /// qdmc.c `qdmc_decode_init` parameter portion. Returns
    /// (sample_rate, bit_rate) from the QDCA atom.
    fn decode_init(&mut self, extradata: &[u8]) -> Result<(u32, u32)> {
        if extradata.len() < 48 {
            return Err(Error::invalid("qdmc: extradata missing or truncated"));
        }

        let mut pos = 0usize;
        while pos + 8 <= extradata.len() {
            if &extradata[pos..pos + 4] == b"frma" && &extradata[pos + 4..pos + 8] == b"QDMC" {
                break;
            }
            pos += 1;
        }
        pos += 8;
        if extradata.len().saturating_sub(pos) < 36 {
            return Err(Error::invalid("qdmc: not enough extradata"));
        }

        let mut p = pos;
        let size = be32(extradata, &mut p) as usize;
        if size > extradata.len().saturating_sub(p) {
            return Err(Error::invalid("qdmc: extradata size too small"));
        }
        if be32(extradata, &mut p) != u32::from_be_bytes(*b"QDCA") {
            return Err(Error::invalid("qdmc: invalid extradata, expecting QDCA"));
        }
        p += 4; // unknown

        self.nb_channels = be32(extradata, &mut p) as usize;
        if self.nb_channels == 0 || self.nb_channels > 2 {
            return Err(Error::invalid("qdmc: invalid number of channels"));
        }

        let sample_rate = be32(extradata, &mut p);
        let bit_rate = be32(extradata, &mut p);
        p += 4; // unknown
        let fft_size = be32(extradata, &mut p) as usize;
        let fft_order = log2_usize(fft_size) + 1;
        self.checksum_size = be32(extradata, &mut p) as usize;
        if self.checksum_size >= 1 << 28 {
            return Err(Error::invalid(format!(
                "qdmc: data block size too large ({})",
                self.checksum_size
            )));
        }

        let x: u32;
        if sample_rate >= 32000 {
            x = 28000;
            self.frame_bits = 13;
        } else if sample_rate >= 16000 {
            x = 20000;
            self.frame_bits = 12;
        } else {
            x = 16000;
            self.frame_bits = 11;
        }
        self.frame_size = 1usize << self.frame_bits;
        self.subframe_size = self.frame_size >> 5;

        let x = if self.nb_channels == 2 { 3 * x / 2 } else { x };
        let v = ((bit_rate as f64 * 3.0 / x as f64 + 0.5).floor() as i64).max(0);
        self.band_index = NOISE_BANDS_SELECTOR[(v as usize).min(6)];

        if !(7..=9).contains(&fft_order) {
            return Err(Error::unsupported(format!(
                "qdmc: unknown FFT order {fft_order}"
            )));
        }
        if fft_size != 1 << (fft_order - 1) {
            return Err(Error::invalid("qdmc: FFT size not power of 2"));
        }

        // alt_sin[5-g][j] = sin_table[((j+1) << (8-g)) & 0x1FF]
        for g in (1..=5).rev() {
            let gi = 5 - g;
            for j in 0..(1usize << g) - 1 {
                self.alt_sin[gi][j] = SIN_TABLE[((j + 1) << (8 - g)) & 0x1FF];
            }
        }

        self.make_noises();

        Ok((sample_rate, bit_rate))
    }

    /// qdmc.c `qdmc_flush`.
    fn flush(&mut self) {
        self.buffer.fill(0.0);
        self.fft_buffer.fill(0.0);
        self.fft_offset = 0;
        self.buffer_offset = 0;
    }
}

fn be32(data: &[u8], p: &mut usize) -> u32 {
    let v = u32::from_be_bytes([data[*p], data[*p + 1], data[*p + 2], data[*p + 3]]);
    *p += 4;
    v
}

fn log2_usize(v: usize) -> u32 {
    if v == 0 {
        0
    } else {
        31 - (v as u32).leading_zeros()
    }
}

/// qdmc.c `qdmc_get_vlc`.
fn qdmc_get_vlc(br: &mut LeBitReader, table: &VlcTable, flag: bool) -> Result<i32> {
    if br.bits_left() < 1 {
        return Err(Error::invalid("qdmc: out of bits"));
    }
    let mut v = match table.decode(br, u32::MAX) {
        Some((v, _)) => v,
        None => {
            let n = br.read(3).ok_or_else(|| Error::invalid("qdmc: escape"))? as u32;
            br.read(n + 1)
                .ok_or_else(|| Error::invalid("qdmc: escape"))? as i32
        }
    };

    if flag {
        if v < 0 || v as usize >= CODE_PREFIX.len() {
            return Err(Error::invalid("qdmc: code prefix out of range"));
        }
        let prefix = CODE_PREFIX[v as usize];
        let extra = if prefix >> 2 > 0 {
            br.read(prefix >> 2)
                .ok_or_else(|| Error::invalid("qdmc: prefix bits"))?
        } else {
            0
        };
        v = prefix as i32 + extra as i32;
    }

    Ok(v)
}

/// qdmc.c `skip_label`: label QMC1 + additive checksum over the
/// packet body.
fn skip_label(s: &QdmcContext, br: &mut LeBitReader) -> Result<()> {
    let label = br.read(32).ok_or_else(|| Error::invalid("qdmc: label"))?;
    let mut sum: i32 = 226;
    let checksum = br.read(16).ok_or_else(|| Error::invalid("qdmc: checksum"))? as u16;

    // MKTAG('Q', 'M', 'C', 1), read as the little-endian reader reads it.
    if label != u32::from_le_bytes([b'Q', b'M', b'C', 1]) {
        return Err(Error::invalid("qdmc: bad frame label"));
    }

    // FFmpeg's `ptr = gb->buffer + 6` runs over the raw packet bytes.
    let packet = br.data_bytes();
    for i in 0..s.checksum_size.saturating_sub(6) {
        let idx = 6 + i;
        let byte = if idx < packet.len() { packet[idx] } else { 0 };
        sum += byte as i32;
    }
    if (sum as u16) != checksum {
        return Err(Error::invalid("qdmc: checksum mismatch"));
    }
    Ok(())
}

/// qdmc.c `read_noise_data`.
fn read_noise_data(s: &mut QdmcContext, br: &mut LeBitReader) -> Result<()> {
    for ch in 0..s.nb_channels {
        for band in 0..NOISE_BANDS_SIZE[s.band_index] {
            let mut v = qdmc_get_vlc(br, &VTABLES.tabs[0], false)?;
            if v & 1 != 0 {
                v += 1;
            } else {
                v = -v;
            }

            let mut lastval = v / 2;
            s.noise[ch][band][0] = (lastval - 1) as u8;
            let mut j = 0usize;
            while j < 15 {
                let mut len = qdmc_get_vlc(br, &VTABLES.tabs[1], true)?;
                len += 1;

                let v = qdmc_get_vlc(br, &VTABLES.tabs[0], false)?;
                let newval = if v & 1 != 0 {
                    lastval + (v + 1) / 2
                } else {
                    lastval - v / 2
                };

                let mut idx = j + 1;
                if len as usize + idx > 16 {
                    return Err(Error::invalid("qdmc: noise run overflow"));
                }

                let mut k = 1;
                while idx <= j + len as usize {
                    s.noise[ch][band][idx] =
                        (lastval + k * (newval - lastval) / len - 1) as u8;
                    k += 1;
                    idx += 1;
                }

                lastval = newval;
                j += len as usize;
            }
        }
    }
    Ok(())
}

/// qdmc.c `add_tone`.
fn add_tone(
    s: &mut QdmcContext,
    group: usize,
    offset: i32,
    freq: i32,
    stereo_mode: i32,
    amplitude: i32,
    phase: i32,
) {
    let index = s.nb_tones[group];
    if index >= 8192 {
        // "Too many tones already in buffer, ignoring tone!"
        return;
    }
    let clamped_offset = offset.clamp(0, 255) as u8;
    s.tones[group][index] = QdmcTone {
        offset: clamped_offset,
        freq: freq.clamp(-32768, 32767) as i16,
        mode: stereo_mode.clamp(0, 255) as u8,
        amplitude: amplitude.clamp(-32768, 32767) as i16,
        phase: phase.clamp(0, 255) as u8,
    };
    s.nb_tones[group] += 1;
}

/// qdmc.c `read_wave_data`.
fn read_wave_data(s: &mut QdmcContext, br: &mut LeBitReader) -> Result<()> {
    let mut stereo_mode;

    for group in 0..5 {
        let group_size = 1i32 << (s.frame_bits - group as u32 - 1);
        let group_bits = 4 - group;
        let mut pos2 = 0i32;
        let mut off = 0i32;

        let mut i = 1i32;
        loop {
            let v = qdmc_get_vlc(br, &VTABLES.tabs[3], true)?;
            let mut freq = i + v;
            while freq >= group_size - 1 {
                freq += 2 - group_size;
                pos2 += group_size;
                off += 1 << group_bits;
            }

            if pos2 >= s.frame_size as i32 {
                break;
            }

            stereo_mode = 0;
            if s.nb_channels > 1 {
                stereo_mode = br.read(2).ok_or_else(|| Error::invalid("qdmc: bits"))? as i32;
            }

            let amp = qdmc_get_vlc(br, &VTABLES.tabs[2], false)?;
            let phase = br.read(3).ok_or_else(|| Error::invalid("qdmc: bits"))? as i32;

            let mut amp2 = 0i32;
            let mut phase2 = 0i32;
            if stereo_mode > 1 {
                let a2 = qdmc_get_vlc(br, &VTABLES.tabs[4], false)?;
                amp2 = amp - a2;

                let p2 = qdmc_get_vlc(br, &VTABLES.tabs[5], false)?;
                phase2 = phase - p2;

                if phase2 < 0 {
                    phase2 += 8;
                }
            }

            if ((freq >> group_bits) + 1) < s.subframe_size as i32 {
                add_tone(s, group, off, freq, stereo_mode & 1, amp, phase);
                if stereo_mode > 1 {
                    add_tone(s, group, off, freq, !(stereo_mode & 1) & 1, amp2, phase2);
                }
            }

            i = freq + 1;
        }
    }

    Ok(())
}

/// qdmc.c `lin_calc`.
fn lin_calc(s: &mut QdmcContext, amplitude: f32, node1: usize, node2: usize, index: usize) {
    let scale = 0.5 * amplitude;
    let mut subframe_size = s.subframe_size;
    if subframe_size >= node2 {
        subframe_size = node2;
    }
    let length = (subframe_size - node1) & 0xFFFC;
    let mut j = node1;
    let mut nptr = 256 * index;

    let mut i = 0usize;
    while i < length {
        for k in 0..4 {
            if j + k < s.noise2_buffer.len() && nptr + k < s.noise_buffer.len() {
                s.noise2_buffer[j + k] += scale * s.noise_buffer[nptr + k];
            }
        }
        i += 4;
        j += 4;
        nptr += 4;
    }

    let mut k = length + node1;
    let mut nptr = length + (index << 8);
    let mut i = length;
    while i < subframe_size.saturating_sub(node1) {
        if k < s.noise2_buffer.len() && nptr < s.noise_buffer.len() {
            s.noise2_buffer[k] += scale * s.noise_buffer[nptr];
        }
        i += 1;
        k += 1;
        nptr += 1;
    }
}

/// qdmc.c `add_noise`.
fn add_noise(s: &mut QdmcContext, ch: usize, current_subframe: usize) {
    let fft_off = s.fft_offset + s.subframe_size * current_subframe;
    let im_base = (0 + ch) * 8192 * 2 + fft_off;
    let re_base = (2 + ch) * 8192 * 2 + fft_off;

    for v in s.noise2_buffer.iter_mut().take(4 * s.subframe_size) {
        *v = 0.0;
    }

    for i in 0..NOISE_BANDS_SIZE[s.band_index] {
        if QDMC_NODES[i + 21 * s.band_index] as usize > s.subframe_size - 1 {
            break;
        }

        let aindex = s.noise[ch][i][current_subframe / 2] as i32;
        let amplitude = if aindex > 0 {
            // C indexes amplitude_tab[aindex & 0x3F] over a 64-entry
            // table; the port's table carries FFmpeg's 65 entries (the
            // last being a benign duplicate of the trailing 0). The
            // mask stays & 0x3F.
            AMPLITUDE_TAB[(aindex as usize) & 0x3F]
        } else {
            0.0
        };

        lin_calc(
            s,
            amplitude,
            QDMC_NODES[21 * s.band_index + i] as usize,
            QDMC_NODES[21 * s.band_index + i + 2] as usize,
            i,
        );
    }

    for j in 2..s.subframe_size - 1 {
        s.rndval = 214013u32.wrapping_mul(s.rndval).wrapping_add(2531011);
        let rnd_im =
            ((s.rndval & 0x7FFF) as f32 - 16384.0) * 0.000030517578 * s.noise2_buffer[j];
        s.rndval = 214013u32.wrapping_mul(s.rndval).wrapping_add(2531011);
        let rnd_re =
            ((s.rndval & 0x7FFF) as f32 - 16384.0) * 0.000030517578 * s.noise2_buffer[j];
        if im_base + j + 1 < s.fft_buffer.len() {
            s.fft_buffer[im_base + j] += rnd_im;
            s.fft_buffer[im_base + j + 1] -= rnd_im;
        }
        if re_base + j + 1 < s.fft_buffer.len() {
            s.fft_buffer[re_base + j] += rnd_re;
            s.fft_buffer[re_base + j + 1] -= rnd_re;
        }
    }
}

/// qdmc.c `add_wave`.
fn add_wave(
    s: &mut QdmcContext,
    offset: usize,
    freqs: i32,
    group: usize,
    stereo_mode: i32,
    amp: i32,
    phase: i32,
) {
    let stereo_mode = if s.nb_channels == 1 { 0 } else { stereo_mode };

    let group_bits = 4 - group;
    let pos = (freqs >> (4 - group)) as usize;
    let amplitude = AMPLITUDE_TAB[(amp as usize) & 0x3F];
    let mut imptr =
        (stereo_mode as usize) * 8192 * 2 + s.fft_offset + s.subframe_size * offset + pos;
    let mut reptr = (2 + stereo_mode as usize) * 8192 * 2
        + s.fft_offset
        + s.subframe_size * offset
        + pos;
    let mut pindex: i32 = (phase << 6)
        - (((2 * (freqs >> (4 - group)) + 1) as i32) << 7);
    for j in 0..((1usize << (group_bits + 1)) - 1) {
        pindex += ((2 * freqs + 1) << (7 - group_bits)) as i32;
        let level = amplitude * s.alt_sin[group][j];
        let im = level * SIN_TABLE[(pindex as usize) & 0x1FF];
        let re = level * SIN_TABLE[((pindex + 128) as usize) & 0x1FF];
        if imptr + 1 < s.fft_buffer.len() {
            s.fft_buffer[imptr] += im;
            s.fft_buffer[imptr + 1] -= im;
        }
        if reptr + 1 < s.fft_buffer.len() {
            s.fft_buffer[reptr] += re;
            s.fft_buffer[reptr + 1] -= re;
        }
        imptr += s.subframe_size;
        reptr += s.subframe_size;
        if imptr >= (stereo_mode as usize) * 8192 * 2 + 2 * s.frame_size {
            imptr = (stereo_mode as usize) * 8192 * 2 + pos;
            reptr = (2 + stereo_mode as usize) * 8192 * 2 + pos;
        }
    }
}

/// qdmc.c `add_wave0`.
fn add_wave0(
    s: &mut QdmcContext,
    offset: usize,
    freqs: i32,
    stereo_mode: i32,
    amp: i32,
    phase: i32,
) {
    let stereo_mode = if s.nb_channels == 1 { 0 } else { stereo_mode };

    let level = AMPLITUDE_TAB[(amp as usize) & 0x3F];
    let im = level * SIN_TABLE[((phase << 6) as usize) & 0x1FF];
    let re = level * SIN_TABLE[(((phase << 6) + 128) as usize) & 0x1FF];
    let pos = s.fft_offset + freqs as usize + s.subframe_size * offset;
    let im_base = (stereo_mode as usize) * 8192 * 2 + pos;
    let re_base = (2 + stereo_mode as usize) * 8192 * 2 + pos;
    if im_base + 1 < s.fft_buffer.len() {
        s.fft_buffer[im_base] += im;
        s.fft_buffer[im_base + 1] -= im;
    }
    if re_base + 1 < s.fft_buffer.len() {
        s.fft_buffer[re_base] += re;
        s.fft_buffer[re_base + 1] -= re;
    }
}

/// qdmc.c `add_waves`.
fn add_waves(s: &mut QdmcContext, current_subframe: usize) {
    for g in 0..4 {
        let mut w = s.cur_tone[g];
        while w < s.nb_tones[g] {
            let t = s.tones[g][w];
            if (current_subframe as i32) < t.offset as i32 {
                break;
            }
            add_wave(
                s,
                t.offset as usize,
                t.freq as i32,
                g,
                t.mode as i32,
                t.amplitude as i32,
                t.phase as i32,
            );
            w += 1;
        }
        s.cur_tone[g] = w;
    }
    let mut w = s.cur_tone[4];
    while w < s.nb_tones[4] {
        let t = s.tones[4][w];
        if (current_subframe as i32) < t.offset as i32 {
            break;
        }
        add_wave0(
            s,
            t.offset as usize,
            t.freq as i32,
            t.mode as i32,
            t.amplitude as i32,
            t.phase as i32,
        );
        w += 1;
    }
    s.cur_tone[4] = w;
}

/// qdmc.c `decode_frame`.
fn decode_frame(s: &mut QdmcContext, br: &mut LeBitReader, out: &mut [i16]) -> Result<()> {
    skip_label(s, br)?;

    s.fft_offset = s.frame_size - s.fft_offset;
    s.buffer_ptr = s.nb_channels * s.buffer_offset;

    read_noise_data(s, br)?;
    read_wave_data(s, br)?;

    for n in 0..32 {
        for ch in 0..s.nb_channels {
            add_noise(s, ch, n);
        }

        add_waves(s, n);

        for ch in 0..s.nb_channels {
            for i in 0..s.subframe_size {
                let base_re = (ch + 2) * 8192 * 2 + s.fft_offset + n * s.subframe_size;
                let base_im = (ch + 0) * 8192 * 2 + s.fft_offset + n * s.subframe_size;
                s.cmplx_in[ch][i].re = s.fft_buffer[base_re + i];
                s.cmplx_in[ch][i].im = s.fft_buffer[base_im + i];
                s.cmplx_in[ch][s.subframe_size + i].re = 0.0;
                s.cmplx_in[ch][s.subframe_size + i].im = 0.0;
            }
        }

        // inverse FFT per channel, size = 2 * subframe_size
        let fft_len = 2 * s.subframe_size;
        for ch in 0..s.nb_channels {
            let input: Vec<Complex> = s.cmplx_in[ch][..fft_len].to_vec();
            let mut out_c = vec![Complex::default(); fft_len];
            inverse_fft(&input, &mut out_c, 1.0);
            for (i, c) in out_c.iter().enumerate() {
                s.cmplx_out[ch][i] = *c;
            }
        }

        let r_base = s.buffer_ptr + s.nb_channels * n * s.subframe_size;
        for i in 0..2 * s.subframe_size {
            for ch in 0..s.nb_channels {
                if r_base + (i * s.nb_channels + ch) < s.buffer.len() {
                    s.buffer[r_base + i * s.nb_channels + ch] += s.cmplx_out[ch][i].re;
                }
            }
        }

        let r2_base = s.buffer_ptr + n * s.subframe_size * s.nb_channels;
        let mut o = n * s.subframe_size * s.nb_channels;
        for i in 0..s.nb_channels * s.subframe_size {
            let v = if r2_base + i < s.buffer.len() {
                s.buffer[r2_base + i]
            } else {
                0.0
            };
            if o < out.len() {
                out[o] = av_clipf_i16(v);
            }
            o += 1;
        }

        for ch in 0..s.nb_channels {
            let base0 = ch * 8192 * 2 + s.fft_offset + n * s.subframe_size;
            let base2 = (ch + 2) * 8192 * 2 + s.fft_offset + n * s.subframe_size;
            for i in 0..4 * s.subframe_size {
                if base0 + i < s.fft_buffer.len() {
                    s.fft_buffer[base0 + i] = 0.0;
                }
                if base2 + i < s.fft_buffer.len() {
                    s.fft_buffer[base2 + i] = 0.0;
                }
            }
        }
        let zero_base =
            s.nb_channels * (n * s.subframe_size + s.frame_size + s.buffer_offset);
        for i in 0..4 * s.subframe_size * s.nb_channels {
            if zero_base + i < s.buffer.len() {
                s.buffer[zero_base + i] = 0.0;
            }
        }
    }

    s.buffer_offset += s.frame_size;
    if s.buffer_offset >= 32768 - s.frame_size {
        let src = s.nb_channels * s.buffer_offset;
        for i in 0..4 * s.frame_size * s.nb_channels {
            if src + i < s.buffer.len() {
                s.buffer[i] = s.buffer[src + i];
            }
        }
        s.buffer_offset = 0;
    }

    Ok(())
}

/// `out[i] = av_clipf(r[i], INT16_MIN, INT16_MAX)` — FFmpeg assigns a
/// clipped float to an int16_t, which truncates toward zero.
#[inline]
fn av_clipf_i16(v: f32) -> i16 {
    let c = v.clamp(i16::MIN as f32, i16::MAX as f32);
    c as i16
}

/// The QDMC packet→frame decoder.
pub struct QdmcDecoder {
    codec_id: CodecId,
    s: QdmcContext,
    /// The rate the `QDCA` atom gives, as FFmpeg takes it.
    sample_rate: u32,
    out: VecDeque<Frame>,
}

impl Decoder for QdmcDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    /// FFmpeg `qdmc_decode_frame`, called as libavcodec calls it: every
    /// `checksum_size` bytes of the packet is one frame of `frame_size`
    /// samples; a shorter remainder is invalid. A frame that fails to
    /// decode resets the synthesis state (`qdmc_flush`).
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut data = &packet.data[..];
        let mut pts = packet.pts;
        while !data.is_empty() {
            let checksum_size = self.s.checksum_size;
            if data.len() < checksum_size {
                return Err(Error::invalid("qdmc: packet shorter than its frame"));
            }
            let nb_samples = self.s.frame_size;
            if nb_samples > FRAME_SIZE_MAX {
                return Err(Error::invalid("qdmc: frame too large"));
            }
            let mut out = vec![0i16; nb_samples * self.s.nb_channels];
            let mut br = LeBitReader::new(&data[..checksum_size]);
            self.s.nb_tones = [0; 5];
            self.s.cur_tone = [0; 5];
            if let Err(e) = decode_frame(&mut self.s, &mut br, &mut out) {
                self.s.flush();
                return Err(e);
            }
            let mut bytes = Vec::with_capacity(out.len() * 2);
            for s in out {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
            self.out.push_back(Frame::Audio(AudioFrame { samples: nb_samples as u32, pts: pts.take(), data: vec![bytes] }));
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

/// Construct a QDMC decoder from stream parameters. `params.extradata`
/// carries the `frma`/`QDCA` atoms (MOV sample entry, CAF `kuki`).
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let mut s = QdmcContext::new();
    let (sample_rate, _bit_rate) = s.decode_init(&params.extradata)?;
    Ok(Box::new(QdmcDecoder { codec_id: params.codec_id.clone(), s, sample_rate, out: VecDeque::new() }))
}
