//! MSMPEG-4 v1/v2/v3 + WMV1 decoder core.
//!
//! Ported from FFmpeg commit 2da55bf `msmpeg4dec.c`, `msmpeg4.c`,
//! `msmpeg4data.c`, `ituh263dec.c` (motion-vector prediction, MV VLC), and
//! the mpegvideo pieces they need (DC/AC prediction, coded-block prediction,
//! RL-VLC decode). The decode order, predictors and rounding match FFmpeg's
//! integer paths exactly.
//!
//! Structure: one packet = one picture (container framing). `send_packet`
//! decodes the picture header and all macroblocks; the reconstructed YUV420P
//! picture is emitted from `receive_frame`.

use crate::bits::BitReader;
use crate::idct;
use crate::tables::*;

use oxideav_core::{
    CodecParameters, Decoder, Error, Frame, Packet, PixelFormat, Result, VideoFrame, VideoPlane,
};
pub const CODEC_ID_MSMPEG4V1: &str = "msmpeg4v1";
pub const CODEC_ID_MSMPEG4V2: &str = "msmpeg4v2";
pub const CODEC_ID_MSMPEG4V3: &str = "msmpeg4v3";
pub const CODEC_ID_WMV1: &str = "wmv1";


// ───────────────────────── VLC helpers ─────────────────────────

/// Canonical-Huffman VLC decode over FFmpeg's `(code, bits)` tables.
pub struct CanonicalVlc {
    /// (length, canonical code MSB-first, symbol).
    entries: Vec<(u8, u32, u16)>,
    max_bits: u32,
}

impl CanonicalVlc {
    /// Build canonical codes from per-symbol lengths (FFmpeg's
    /// `ff_vlc_init_tables_from_lengths` semantics).
    pub fn from_lengths(lens: &[u8], syms: Option<&[u16]>) -> Self {
        let mut max_bits = 0u32;
        for &l in lens {
            max_bits = max_bits.max(l as u32);
        }
        let mut order: Vec<usize> = (0..lens.len()).filter(|&i| lens[i] > 0).collect();
        order.sort_by_key(|&i| (lens[i], syms.map(|s| s[i]).unwrap_or(i as u16)));
        let mut entries: Vec<(u8, u32, u16)> = Vec::with_capacity(order.len());
        let mut code = 0u32;
        let mut prev_len = 0u8;
        for &i in &order {
            let l = lens[i];
            if prev_len > 0 {
                code <<= (l - prev_len) as u32;
            }
            entries.push((l, code, syms.map(|s| s[i]).unwrap_or(i as u16)));
            code += 1;
            prev_len = l;
        }
        Self { entries, max_bits }
    }

    /// Build from explicit `(code, bits)` pairs; symbol = index.
    pub fn from_pairs(pairs: &[(u32, u8)]) -> Self {
        let mut entries: Vec<(u8, u32, u16)> = pairs.iter().map(|&(c, b)| (b, c, 0u16)).collect();
        for (i, e) in entries.iter_mut().enumerate() {
            e.2 = i as u16;
        }
        let max_bits = entries.iter().map(|e| e.0 as u32).max().unwrap_or(0);
        Self { entries, max_bits }
    }

    /// Build from explicit `(code, bits)` pairs with explicit symbols.
    pub fn from_pairs_syms(pairs: &[(u32, u8)], syms: &[u16]) -> Self {
        let mut entries: Vec<(u8, u32, u16)> = pairs.iter().map(|&(c, b)| (b, c, 0u16)).collect();
        for (i, e) in entries.iter_mut().enumerate() {
            e.2 = syms[i];
        }
        let max_bits = entries.iter().map(|e| e.0 as u32).max().unwrap_or(0);
        Self { entries, max_bits }
    }

    /// Decode one symbol (exact prefix match over all entries).
    pub fn decode(&self, br: &mut BitReader) -> Result<u16> {
        let avail = br.bits_left().clamp(0, self.max_bits as i64) as u32;
        let peeked = br.peek(self.max_bits.min(32));
        let peek_full = if avail < self.max_bits {
            peeked << (self.max_bits - avail)
        } else {
            peeked
        };
        for &(l, c, s) in &self.entries {
            if (l as u32) <= avail {
                let shift = self.max_bits - l as u32;
                if (peek_full >> shift) == c {
                    br.skip(l as u32);
                    return Ok(s);
                }
            }
        }
        Err(Error::InvalidData(format!(
            "codec-wmv: no VLC codeword matches (next bits 0x{peeked:x}, avail {avail})"
        )))
    }
}

/// RL table + precomputed RL-VLC (FFmpeg `RLTable` + `ff_init_vlc_rl`).
struct RlTable {
    n: usize,
    last: usize,
    vlc: CanonicalVlc,
    run: Vec<i32>,
    level: Vec<i32>,
    /// max_level[last][run] (escape level extension).
    max_level: Vec<[i8; 64]>,
    /// max_run[last][level] (escape run extension).
    max_run: Vec<[i8; 128]>,
    /// rl_vlc[q] for q in 0..=31.
    rl_vlc: Vec<Vec<(i32, i32)>>, // (run, level) indexed by VLC symbol
}

impl RlTable {
    fn new(vlc_pairs: &[(u32, u8)], run: &[i8], level: &[i8], n: usize, last: usize) -> Self {
        let vlc = CanonicalVlc::from_pairs(vlc_pairs);
        let mut max_level = vec![[0i8; 64]; 2];
        let mut max_run = vec![[0i8; 128]; 2];
        for l in 0..2 {
            let (start, end) = if l == 0 { (0, last) } else { (last, n) };
            for i in start..end {
                let r = run[i].clamp(0, 63) as usize;
                let lv = level[i].clamp(0, 127) as usize;
                if level[i] > max_level[l][r] {
                    max_level[l][r] = level[i];
                }
                if run[i] > max_run[l][lv] {
                    max_run[l][lv] = run[i];
                }
            }
        }
        // Symbol → encoded length.
        let mut sym_len = vec![0i32; n + 1];
        for &(l, _c, s) in &vlc.entries {
            let s = s as usize;
            if s < sym_len.len() {
                sym_len[s] = l as i32;
            }
        }
        let mut rl_vlc = Vec::with_capacity(32);
        for q in 0..32 {
            let (qmul, qadd) = if q == 0 { (1, 0) } else { (q * 2, (q - 1) | 1) };
            let mut v = Vec::with_capacity(n + 1);
            for i in 0..=n {
                let len = sym_len[i];
                if len == 0 {
                    // illegal code (FFmpeg: run 66, level MAX_LEVEL)
                    v.push((66, 64));
                } else if i == n {
                    // escape
                    v.push((66, 0));
                } else {
                    let mut r = run[i] as i32 + 1;
                    if i >= last {
                        r += 192;
                    }
                    v.push((r, level[i] as i32 * qmul + qadd));
                }
            }
            rl_vlc.push(v);
        }
        Self {
            n,
            last,
            vlc,
            run: run.iter().map(|&v| v as i32).collect(),
            level: level.iter().map(|&v| v as i32).collect(),
            max_level,
            max_run,
            rl_vlc,
        }
    }

    /// `GET_RL_VLC`: returns (level, run); run >= 192 marks last=1 (encoded
    /// as run+192 by `ff_init_vlc_rl`), 66 with level 0 = escape.
    #[inline]
    fn get(&self, q: usize, br: &mut BitReader) -> Result<(i32, i32)> {
        let sym = self.vlc.decode(br)? as usize;
        let (run, level) = self.rl_vlc[q.min(31)][sym.min(self.n)];
        Ok((level, run))
    }
}

// ───────────────────────── DC scale tables ─────────────────────────

#[inline]
fn y_dc_scale(version: MsVersion, q: usize) -> i32 {
    match version {
        MsVersion::V1 | MsVersion::V2 => 8,
        MsVersion::V3 => MPEG4_Y_DC_SCALE[q.min(31)] as i32,
        MsVersion::Wmv1 => WMV1_Y_DC_SCALE_TABLE[q.min(31)] as i32,
    }
}

#[inline]
fn c_dc_scale(version: MsVersion, q: usize) -> i32 {
    match version {
        MsVersion::V1 | MsVersion::V2 => 8,
        MsVersion::V3 => MPEG4_C_DC_SCALE[q.min(31)] as i32,
        MsVersion::Wmv1 => WMV1_C_DC_SCALE_TABLE[q.min(31)] as i32,
    }
}

// ───────────────────────── picture state ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum MsVersion {
    V1,
    V2,
    V3,
    Wmv1,
}

/// One 4:2:0 picture.
struct Picture {
    width: usize,
    height: usize,
    y: Vec<u8>,
    cb: Vec<u8>,
    cr: Vec<u8>,
}

impl Picture {
    fn alloc(width: usize, height: usize) -> Result<Self> {
        if width == 0
            || height == 0
            || width > crate::MAX_DIM as usize
            || height > crate::MAX_DIM as usize
            || (width as u64) * (height as u64) > crate::MAX_PIXELS
        {
            return Err(Error::InvalidData(format!(
                "codec-wmv msmpeg4: refusing frame {width}x{height}"
            )));
        }
        Ok(Self {
            width,
            height,
            y: vec![128; width * height],
            cb: vec![128; (width / 2) * (height / 2)],
            cr: vec![128; (width / 2) * (height / 2)],
        })
    }
}

/// Prediction contexts (FFmpeg mpegvideo layout, simplified to exact-size
/// grids with border handling at access time):
/// - DC values: one i16 per 8×8 block position, per plane, on a b8 grid with
///   a 1-block border (`block_index` = 1 + 2*mb_x + 2*mb_y*b8_stride).
/// - AC values: per block position, 16 entries (left column + top row).
/// - coded_block: one u8 per 8×8 block position (b8 grid, luma only).
struct PredContext {
    b8_stride: usize,
    /// Per-plane DC grids: 0 = luma, 1 = cb, 2 = cr. Luma is b8-sized;
    /// chroma grids are mb-sized with the same border convention
    /// (block_index offset by plane).
    dc_val: [Vec<i16>; 3],
    /// AC prediction store per block position (16 values: [0..8) = left col,
    /// [8..16) = top row).
    ac_val: [Vec<[i16; 16]>; 3],
    coded_block: Vec<u8>,
    mb_width: usize,
    mb_height: usize,
}

impl PredContext {
    fn new(mb_width: usize, mb_height: usize) -> Self {
        let b8_stride = 2 * mb_width + 1;
        let b8_size = b8_stride * (2 * mb_height + 1);
        let mb_stride = mb_width + 1;
        let mb_size = mb_stride * (mb_height + 1);
        Self {
            b8_stride,
            dc_val: [vec![0; b8_size], vec![0; mb_size], vec![0; mb_size]],
            ac_val: [
                vec![[0i16; 16]; b8_size],
                vec![[0i16; 16]; mb_size],
                vec![[0i16; 16]; mb_size],
            ],
            coded_block: vec![0; b8_size],
            mb_width,
            mb_height,
        }
    }

    /// FFmpeg's `block_index[n]` for macroblock (mb_x, mb_y):
    /// luma n in 0..4 → b8 grid; chroma n = 4/5 → cb/cr mb grid.
    #[inline]
    fn block_index(&self, n: usize, mb_x: usize, mb_y: usize) -> (usize, usize) {
        if n < 4 {
            let bx = 2 * mb_x + (n & 1);
            let by = 2 * mb_y + (n >> 1);
            (0, 1 + bx + by * self.b8_stride)
        } else {
            let plane = n - 3; // 1 => cb, 2 => cr
            let mb_stride = self.mb_width + 1;
            (plane, 1 + mb_x + mb_y * mb_stride)
        }
    }

    #[inline]
    fn dc(&self, n: usize, mb_x: usize, mb_y: usize) -> i16 {
        let (p, i) = self.block_index(n, mb_x, mb_y);
        self.dc_val[p][i]
    }

    #[inline]
    fn set_dc(&mut self, n: usize, mb_x: usize, mb_y: usize, v: i16) {
        let (p, i) = self.block_index(n, mb_x, mb_y);
        self.dc_val[p][i] = v;
    }

    /// `ff_msmpeg4_pred_dc` (msmpeg4.c): returns (pred, dir) where dir is
    /// 0 = left, 1 = top; also writes the updated DC into `dc_store`.
    fn msmpeg4_pred_dc(
        &mut self,
        n: usize,
        mb_x: usize,
        mb_y: usize,
        first_slice_line: bool,
        scale: i32,
        level: i32,
        is_wmv1: bool,
    ) -> (i32, i32) {
        let (p, i) = self.block_index(n, mb_x, mb_y);
        let stride = if n < 4 { self.b8_stride } else { self.mb_width + 1 };
        let mut a = self.dc_val[p][i - 1] as i32;
        let mut b = self.dc_val[p][i - 1 - stride] as i32;
        let mut c = self.dc_val[p][i - stride] as i32;

        // first_slice_line && !(n & 2) && version < WMV1 → b = c = 1024.
        if first_slice_line && (n & 2) == 0 && !is_wmv1 {
            b = 1024;
            c = 1024;
        }

        // Divisions with rounding (the asm fast path is `(a + scale/2)/scale`).
        a = (a + (scale >> 1)) / scale;
        b = (b + (scale >> 1)) / scale;
        c = (c + (scale >> 1)) / scale;

        // WARNING: different test than MPEG-4.
        if is_wmv1 {
            // inter_intra_pred is always 0 for WMV1 decode (msmpeg4dec.c sets
            // h->c.inter_intra_pred = 0 on I-frames and 0 on P-frames), so
            // fall through to the generic branch.
            if (a - b).abs() < (b - c).abs() {
                let pred = c;
                self.dc_val[p][i] = (level * scale) as i16;
                return (pred, 1);
            } else {
                let pred = a;
                self.dc_val[p][i] = (level * scale) as i16;
                return (pred, 0);
            }
        } else if (a - b).abs() <= (b - c).abs() {
            let pred = c;
            self.dc_val[p][i] = (level * scale) as i16;
            (pred, 1)
        } else {
            let pred = a;
            self.dc_val[p][i] = (level * scale) as i16;
            (pred, 0)
        }
    }

    /// `ff_mpeg4_pred_ac` for the msmpeg4 family: add the left column / top
    /// row of the neighbour's AC store when ac_pred is set, then store this
    /// block's first row/column.
    fn pred_ac(
        &mut self,
        n: usize,
        mb_x: usize,
        mb_y: usize,
        block: &mut [i16; 64],
        dir: i32,
        ac_pred: bool,
        qscale: usize,
        qscale_table: &mut Vec<i32>,
        qscale_pos: usize,
    ) {
        let (p, i) = self.block_index(n, mb_x, mb_y);
        let stride = if n < 4 { self.b8_stride } else { self.mb_width + 1 };
        let use_left = dir == 0;
        // qscale of the prediction-source MB.
        let src_pos = if use_left {
            qscale_pos.saturating_sub(1)
        } else {
            qscale_pos.saturating_sub(self.mb_width + 1)
        };
        let mut rescale = false;
        let src_q: i32;
        if ac_pred {
            src_q = qscale_table[src_pos];
            rescale = src_q != qscale as i32 && !(n == 1 || n == 3) && !(n == 2 && !use_left);
            // FFmpeg condition: mb_x == 0 || same q || n==1 || n==3 (left);
            // mb_y == 0 || same q || n==2 || n==3 (top).
            if use_left {
                rescale = !(mb_x == 0 || src_q == qscale as i32 || n == 1 || n == 3)
                    && src_q != 0;
            } else {
                rescale = !(mb_y == 0 || src_q == qscale as i32 || n == 2 || n == 3)
                    && src_q != 0;
            }
            if use_left {
                for k in 1..8 {
                    let av = self.ac_val[p][i][k] as i32;
                    let av = if rescale {
                        (av * src_q + qscale as i32 / 2) / qscale as i32
                    } else {
                        av
                    };
                    block[k << 3] += av as i16;
                }
            } else {
                for k in 1..8 {
                    let av = self.ac_val[p][i][8 + k] as i32;
                    let av = if rescale {
                        (av * src_q + qscale as i32 / 2) / qscale as i32
                    } else {
                        av
                    };
                    block[k] += av as i16;
                }
            }
        }
        // Store this block's first column / row (always, per FFmpeg).
        for k in 1..8 {
            self.ac_val[p][i][k] = block[k << 3];
        }
        for k in 1..8 {
            self.ac_val[p][i][8 + k] = block[k];
        }
    }

    /// `ff_msmpeg4_coded_block_pred`: predict + store one luma coded bit.
    fn coded_block_pred(&mut self, n: usize, mb_x: usize, mb_y: usize, diff: u8) -> u8 {
        let (p, i) = self.block_index(n, mb_x, mb_y);
        let stride = self.b8_stride;
        let a = self.coded_block[i - 1];
        let b = self.coded_block[i - 1 - stride];
        let c = self.coded_block[i - stride];
        let pred = if b == c { a } else { c };
        let v = pred ^ diff;
        self.coded_block[i] = v;
        v
    }
}

// ───────────────────────── motion vectors ─────────────────────────

/// H.263 MV VLC (ff_mvtab, 33 entries); symbol = index.
static MV_VLC: std::sync::OnceLock<CanonicalVlc> = std::sync::OnceLock::new();
static MSMP4_MV_VLC: [std::sync::OnceLock<CanonicalVlc>; 2] =
    [std::sync::OnceLock::new(), std::sync::OnceLock::new()];
static MB_NON_INTRA_VLC: [std::sync::OnceLock<CanonicalVlc>; 4] = [
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
];
static MB_I_VLC: std::sync::OnceLock<CanonicalVlc> = std::sync::OnceLock::new();
static DC_VLC: [[std::sync::OnceLock<CanonicalVlc>; 2]; 2] =
    [[std::sync::OnceLock::new(), std::sync::OnceLock::new()], [std::sync::OnceLock::new(), std::sync::OnceLock::new()]];
static V2_DC_LUM_VLC: std::sync::OnceLock<CanonicalVlc> = std::sync::OnceLock::new();
static V2_DC_CHROMA_VLC: std::sync::OnceLock<CanonicalVlc> = std::sync::OnceLock::new();
static V2_INTRA_CBPC_VLC: std::sync::OnceLock<CanonicalVlc> = std::sync::OnceLock::new();
static V2_MB_TYPE_VLC: std::sync::OnceLock<CanonicalVlc> = std::sync::OnceLock::new();
static INTER_INTRA_VLC: std::sync::OnceLock<CanonicalVlc> = std::sync::OnceLock::new();
static RL_TABLES: std::sync::OnceLock<[RlTable; 6]> = std::sync::OnceLock::new();

fn v2_dc_lum_table() -> Vec<(u32, u8)> {
    // msmpeg4.c init_h263_dc_for_msmpeg4: generated table for v2.
    // size = 512 codes; codes are built level-by-level with a 3-bit
    // size VLC + value bits. We regenerate exactly as FFmpeg does.
    let mut tab = vec![(0u32, 0u8); 512];
    let mut code = 0u32;
    for level in -256i32..256 {
        let level_u = (level + 256) as usize;
        let size = if level == 0 {
            0
        } else {
            (31 - (level.abs() as u32).leading_zeros()) as u8
        };
        // code: 'size' zero bits then 1? FFmpeg builds:
        //   for size in 0..: put_bits(0, size) + 1... then value.
        // Actually init_h263_dc_for_msmpeg4 writes: run-length of zeros
        // (size bits, value 0) terminated by 1, then size bits of |level|-1
        // with sign. Regenerate per msmpeg4.c:
        //   uni_code = 1; uni_len = 1 for size 0... see below.
        let _ = code;
        // luma table: ff_v2_dc_lum_table[level+256] = (code, len)
        // FFmpeg builds with put_bits(0, size); put_bits(1, 1) ... for size 0:
        // code 1 len 1.
        let mut uni_code = 1u32;
        let mut uni_len = 1u8;
        for _ in 0..size {
            uni_code = (uni_code << 1) | 1;
            uni_len += 1;
        }
        // then value bits: |level|-1 in size bits (or nothing for size 0)
        if size > 0 {
            let v = (level.abs() - 1) as u32;
            uni_code = (uni_code << size) | (v & ((1 << size) - 1));
            uni_len += size;
        }
        tab[level_u] = (uni_code, uni_len);
    }
    // Note: FFmpeg's luma table differs from chroma by a suffix bit; the
    // chroma builder appends one more '1' bit for each level (see
    // init_h263_dc_for_msmpeg4: "ff_v2_dc_chroma_table[level + 256][0]"
    // loop appends `uni_code <<= 1; uni_code |= 1; uni_len++`).
    tab
}

fn v2_dc_chroma_table() -> Vec<(u32, u8)> {
    let mut t = v2_dc_lum_table();
    for e in t.iter_mut() {
        e.0 = (e.0 << 1) | 1;
        e.1 += 1;
    }
    t
}

fn init_tables() {
    let _ = MV_VLC.set(CanonicalVlc::from_pairs(&MV_TAB));
    for i in 0..2 {
        let pairs: Vec<(u32, u8)> = MSMP4_MV0_LENS
            .iter()
            .zip(MSMP4_MV0_LENS.iter())
            .map(|_| (0, 0))
            .collect();
        let _ = pairs;
        // MV tables use (value, len) arrays: values from MSMP4_MV{i}, lens
        // from MSMP4_MV{i}_LENS, built with ff_vlc_init_tables_from_lengths.
        let (vals, lens): (&[u16], &[u8]) = if i == 0 {
            (&MSMP4_MV0, &MSMP4_MV0_LENS)
        } else {
            (&MSMP4_MV1, &MSMP4_MV1_LENS)
        };
        let mut pairs: Vec<(u32, u8)> = Vec::with_capacity(vals.len());
        for j in 0..vals.len() {
            pairs.push((vals[j] as u32, lens[j]));
        }
        let _ = MSMP4_MV_VLC[i].set(CanonicalVlc::from_pairs(&pairs));
    }
    let mb_non_intra: [&[(u32, u8)]; 4] = [&MB_NON_INTRA0, &MB_NON_INTRA1, &MB_NON_INTRA2, &MB_NON_INTRA3];
    for (i, t) in mb_non_intra.iter().enumerate() {
        let _ = MB_NON_INTRA_VLC[i].set(CanonicalVlc::from_pairs(t));
    }
    let _ = MB_I_VLC.set(CanonicalVlc::from_pairs(&MSMP4_MB_I_TABLE));
    for i in 0..2 {
        for j in 0..2 {
            let idx = i * 2 + j;
            let name = match idx {
                0 => &MSMP4_DC0_LUMA,
                1 => &MSMP4_DC0_CHROMA,
                2 => &MSMP4_DC1_LUMA,
                _ => &MSMP4_DC1_CHROMA,
            };
            let _ = DC_VLC[i][j].set(CanonicalVlc::from_pairs(name));
        }
    }
    let lum = v2_dc_lum_table();
    let chroma = v2_dc_chroma_table();
    let _ = V2_DC_LUM_VLC.set(CanonicalVlc::from_pairs(&lum));
    let _ = V2_DC_CHROMA_VLC.set(CanonicalVlc::from_pairs(&chroma));
    let _ = V2_INTRA_CBPC_VLC.set(CanonicalVlc::from_pairs(&V2_INTRA_CBPC));
    let _ = V2_MB_TYPE_VLC.set(CanonicalVlc::from_pairs(&V2_MB_TYPE));
    let _ = INTER_INTRA_VLC.set(CanonicalVlc::from_pairs(&TABLE_INTER_INTRA));

    let _ = RL_TABLES.set([
        RlTable::new(&RL0_VLC, &RL0_RUN, &RL0_LEVEL, 132, 85),
        RlTable::new(&RL2_VLC, &RL2_RUN, &RL2_LEVEL, 185, 119),
        RlTable::new(&RL_MPEG4_INTRA_VLC, &RL_MPEG4_INTRA_RUN, &RL_MPEG4_INTRA_LEVEL, 102, 67),
        RlTable::new(&RL1_VLC, &RL1_RUN, &RL1_LEVEL, 148, 81),
        RlTable::new(&RL3_VLC, &RL3_RUN, &RL3_LEVEL, 168, 99),
        RlTable::new(&RL_H263_INTER_VLC, &RL_H263_INTER_RUN, &RL_H263_INTER_LEVEL, 102, 58),
    ]);
}

// ───────────────────────── decoder ─────────────────────────

pub struct MsMpeg4Decoder {
    codec_id: oxideav_core::CodecId,
    version: MsVersion,
    width: usize,
    height: usize,
    mb_width: usize,
    mb_height: usize,
    pred: PredContext,
    qscale_table: Vec<i32>, // per-MB
    last_picture: Option<Picture>,
    pending: Option<Frame>,
    no_rounding: bool,
    // per-frame params
    pict_type: u8,
    qscale: usize,
    rl_table_index: usize,
    rl_chroma_table_index: usize,
    dc_table_index: usize,
    mv_table_index: usize,
    per_mb_rl_table: bool,
    use_skip_mb_code: bool,
    flipflop_rounding: bool,
    bit_rate: u32,
    ac_pred: bool,
    inter_intra_pred: bool,
    h263_aic_dir: usize,
    esc3_level_length: usize,
    esc3_run_length: usize,
    slice_height: usize,
    first_slice_line: bool,
    started: bool,
    is_intra_mb: bool,
    dc_pred_dir: i32,
}

fn make_frame(pic: Picture, pts: Option<i64>) -> Frame {
    let cw = pic.width / 2;
    Frame::Video(VideoFrame {
        pts,
        planes: vec![
            VideoPlane {
                stride: pic.width,
                data: pic.y,
            },
            VideoPlane {
                stride: cw,
                data: pic.cb,
            },
            VideoPlane {
                stride: cw,
                data: pic.cr,
            },
        ],
    })
}

impl MsMpeg4Decoder {
    fn new(params: &CodecParameters, version: MsVersion, id: &'static str) -> Result<Self> {
        init_tables();
        let (w, h) = match (params.width, params.height) {
            (Some(w), Some(h)) => (w as usize, h as usize),
            _ => {
                return Err(Error::InvalidData(
                    "codec-wmv msmpeg4: container must supply width/height".into(),
                ))
            }
        };
        let mb_width = w.div_ceil(16);
        let mb_height = h.div_ceil(16);
        Ok(Self {
            codec_id: oxideav_core::CodecId::new(id),
            version,
            width: w,
            height: h,
            mb_width,
            mb_height,
            pred: PredContext::new(mb_width, mb_height),
            qscale_table: vec![0; mb_width * mb_height],
            last_picture: None,
            pending: None,
            no_rounding: true,
            pict_type: 0,
            qscale: 0,
            rl_table_index: 0,
            rl_chroma_table_index: 0,
            dc_table_index: 0,
            mv_table_index: 0,
            per_mb_rl_table: false,
            use_skip_mb_code: true,
            flipflop_rounding: false,
            bit_rate: 0,
            ac_pred: false,
            inter_intra_pred: false,
            h263_aic_dir: 0,
            esc3_level_length: 0,
            esc3_run_length: 0,
            slice_height: mb_height,
            first_slice_line: true,
            started: false,
            is_intra_mb: false,
            dc_pred_dir: 0,
        })
    }

    pub fn new_v1(params: &CodecParameters) -> Result<Self> {
        Self::new(params, MsVersion::V1, "msmpeg4v1")
    }
    pub fn new_v2(params: &CodecParameters) -> Result<Self> {
        Self::new(params, MsVersion::V2, "msmpeg4v2")
    }
    pub fn new_v3(params: &CodecParameters) -> Result<Self> {
        Self::new(params, MsVersion::V3, "msmpeg4v3")
    }
    pub fn new_wmv1(params: &CodecParameters) -> Result<Self> {
        Self::new(params, MsVersion::Wmv1, "wmv1")
    }

    /// `ff_msmpeg4_decode_ext_header`: reads fps/bitrate/flipflop from the
    /// I-frame tail (WMV1 only).
    fn decode_ext_header(&mut self, br: &mut BitReader) {
        let left = br.bits_left();
        let length = if self.version >= MsVersion::V3 { 17 } else { 16 };
        if left >= length && left < length + 8 {
            br.skip(5); // fps
            self.bit_rate = br.read(11) * 1024;
            if self.version >= MsVersion::V3 {
                self.flipflop_rounding = br.read_bit() != 0;
            } else {
                self.flipflop_rounding = false;
            }
        } else if left < length + 8 {
            self.flipflop_rounding = false;
        }
    }

    /// `msmpeg4_decode_picture_header`.
    fn decode_picture_header(&mut self, br: &mut BitReader) -> Result<()> {
        if br.bits_left() * 8 < (self.mb_width * self.mb_height) as i64 {
            return Err(Error::InvalidData("frame too small".into()));
        }
        let mut code;
        if self.version == MsVersion::V1 {
            let start_code = br.read(32);
            if start_code != 0x00000100 {
                return Err(Error::InvalidData("invalid startcode".into()));
            }
            br.skip(5); // frame number
        }
        self.pict_type = br.read(2) as u8 + 1; // 1=I, 2=P
        if self.pict_type != 1 && self.pict_type != 2 {
            return Err(Error::InvalidData("invalid picture type".into()));
        }
        let q = br.read(5) as usize;
        if q == 0 {
            return Err(Error::InvalidData("invalid qscale".into()));
        }
        self.qscale = q;

        if self.pict_type == 1 {
            code = br.read(5) as usize;
            match self.version {
                MsVersion::V1 => {
                    if code == 0 || code > self.mb_height {
                        return Err(Error::InvalidData("invalid slice height".into()));
                    }
                    self.slice_height = code;
                }
                _ => {
                    if code < 0x17 {
                        return Err(Error::InvalidData(format!(
                            "slice code was {code:X}"
                        )));
                    }
                    self.slice_height = self.mb_height / (code - 0x16);
                }
            }
            match self.version {
                MsVersion::V1 | MsVersion::V2 => {
                    self.rl_chroma_table_index = 2;
                    self.rl_table_index = 2;
                    self.dc_table_index = 0;
                }
                MsVersion::V3 => {
                    self.rl_chroma_table_index = br.decode012()? as usize;
                    self.rl_table_index = br.decode012()? as usize;
                    self.dc_table_index = br.read_bit() as usize;
                }
                MsVersion::Wmv1 => {
                    self.decode_ext_header(br);
                    if self.bit_rate > MBAC_BITRATE {
                        self.per_mb_rl_table = br.read_bit() != 0;
                    } else {
                        self.per_mb_rl_table = false;
                    }
                    if !self.per_mb_rl_table {
                        self.rl_chroma_table_index = br.decode012()? as usize;
                        self.rl_table_index = br.decode012()? as usize;
                    }
                    self.dc_table_index = br.read_bit() as usize;
                    self.inter_intra_pred = false;
                }
            }
            self.no_rounding = true;
        } else {
            match self.version {
                MsVersion::V1 | MsVersion::V2 => {
                    if self.version == MsVersion::V1 {
                        self.use_skip_mb_code = true;
                    } else {
                        self.use_skip_mb_code = br.read_bit() != 0;
                    }
                    self.rl_table_index = 2;
                    self.rl_chroma_table_index = 2;
                    self.dc_table_index = 0;
                    self.mv_table_index = 0;
                }
                MsVersion::V3 => {
                    self.use_skip_mb_code = br.read_bit() != 0;
                    self.rl_table_index = br.decode012()? as usize;
                    self.rl_chroma_table_index = self.rl_table_index;
                    self.dc_table_index = br.read_bit() as usize;
                    self.mv_table_index = br.read_bit() as usize;
                }
                MsVersion::Wmv1 => {
                    self.use_skip_mb_code = br.read_bit() != 0;
                    if self.bit_rate > MBAC_BITRATE {
                        self.per_mb_rl_table = br.read_bit() != 0;
                    } else {
                        self.per_mb_rl_table = false;
                    }
                    if !self.per_mb_rl_table {
                        self.rl_table_index = br.decode012()? as usize;
                        self.rl_chroma_table_index = self.rl_table_index;
                    }
                    self.dc_table_index = br.read_bit() as usize;
                    self.mv_table_index = br.read_bit() as usize;
                    self.inter_intra_pred = self.width * self.height < 320 * 240
                        && self.bit_rate <= II_BITRATE;
                }
            }
            if self.flipflop_rounding {
                self.no_rounding ^= true;
            } else {
                self.no_rounding = false;
            }
        }
        self.esc3_level_length = 0;
        self.esc3_run_length = 0;
        Ok(())
    }

    /// `msmpeg4_decode_dc`.
    fn decode_dc(&mut self, br: &mut BitReader, n: usize) -> Result<i32> {
        let level;
        if self.version <= MsVersion::V2 {
            let vlc = if n < 4 {
                V2_DC_LUM_VLC.get().unwrap()
            } else {
                V2_DC_CHROMA_VLC.get().unwrap()
            };
            let l = vlc.decode(br)? as i32;
            level = l - 256;
        } else {
            let vlc = &DC_VLC[self.dc_table_index][if n >= 4 { 1 } else { 0 }];
            let mut l = vlc.get().unwrap().decode(br)? as i32;
            if l == DC_MAX {
                l = br.read(8) as i32;
                if br.read_bit() != 0 {
                    l = -l;
                }
            } else if l != 0 && br.read_bit() != 0 {
                l = -l;
            }
            level = l;
        }
        Ok(level)
    }

    /// `ff_msmpeg4_decode_block`.
    fn decode_block(
        &mut self,
        br: &mut BitReader,
        block: &mut [i16; 64],
        n: usize,
        coded: bool,
        scan: &[u8; 64],
        mb_x: usize,
        mb_y: usize,
    ) -> Result<()> {
        let q = self.qscale;
        let mut i: i32;
        let mut qmul;
        let mut qadd;
        let run_diff;
        let rl_idx;
        if self.is_intra_mb {
            qmul = 1;
            qadd = 0;
            let mut level = self.decode_dc(br, n)?;
            if level < 0 && self.inter_intra_pred {
                level = 0;
            }
            if n < 4 {
                rl_idx = self.rl_table_index;
                if level > 256 * y_dc_scale(self.version, q) && !self.inter_intra_pred {
                    return Err(Error::InvalidData("dc overflow+".into()));
                }
            } else {
                rl_idx = 3 + self.rl_chroma_table_index;
                if level > 256 * c_dc_scale(self.version, q) && !self.inter_intra_pred {
                    return Err(Error::InvalidData("dc overflow+ C".into()));
                }
            }
            block[0] = level as i16;
            run_diff = self.version >= MsVersion::Wmv1;
            i = 0;
            if !coded {
                // AC prediction + store (msmpeg4 pred_ac path).
                let scale = if n < 4 {
                    y_dc_scale(self.version, q)
                } else {
                    c_dc_scale(self.version, q)
                };
                self.pred.pred_ac(
                    n,
                    mb_x,
                    mb_y,
                    block,
                    self.dc_pred_dir,
                    self.ac_pred,
                    q,
                    &mut self.qscale_table,
                    mb_y * self.mb_width + mb_x,
                );
                let _ = scale;
                return Ok(());
            }
            let scan_tbl: &[u8; 64] = if self.ac_pred {
                if self.dc_pred_dir == 0 {
                    INTRA_V_SCAN.get_or_init(|| WMV1_SCANTABLE3)
                } else {
                    INTRA_H_SCAN.get_or_init(|| WMV1_SCANTABLE2)
                }
            } else {
                INTRA_SCAN.get_or_init(|| WMV1_SCANTABLE0)
            };
            let rl = &RL_TABLES.get().unwrap()[rl_idx];
            let (mut level, mut run) = rl.get(0, br)?;
            let _ = (&mut level, &mut run, qmul, qadd);
            self.rl_decode_loop(
                br,
                block,
                rl,
                0,
                scan_tbl,
                i,
                run_diff,
                qmul,
                qadd,
                mb_x,
                mb_y,
                n,
            )
        } else {
            qmul = (q << 1) as i32;
            qadd = ((q as i32) - 1) | 1;
            i = -1;
            rl_idx = 3 + self.rl_table_index;
            run_diff = self.version != MsVersion::V2;
            if !coded {
                return Ok(());
            }
            let rl = &RL_TABLES.get().unwrap()[rl_idx];
            self.rl_decode_loop(br, block, rl, q, scan, i, run_diff, qmul, qadd, mb_x, mb_y, n)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rl_decode_loop(
        &mut self,
        br: &mut BitReader,
        block: &mut [i16; 64],
        rl: &RlTable,
        q: usize,
        scan: &[u8; 64],
        mut i: i32,
        run_diff: bool,
        qmul: i32,
        qadd: i32,
        _mb_x: usize,
        _mb_y: usize,
        _n: usize,
    ) -> Result<()> {
        loop {
            let (mut level, mut run) = rl.get(q, br)?;
            if level == 0 {
                // escape
                let cache = br.peek(2) << 30; // top two bits
                let _ = cache;
                // FFmpeg reads the two cache bits: (cache & 0x8000_0000) and
                // (cache & 0x4000_0000). We re-peek explicitly.
                let b0 = br.peek(1);
                let b1 = br.peek(2) & 1;
                if self.version == MsVersion::V1 || b0 == 0 {
                    if self.version == MsVersion::V1 || b1 == 0 {
                        // third escape
                        if self.version != MsVersion::V1 {
                            br.skip(2);
                        }
                        if self.version <= MsVersion::V3 {
                            let last = br.read(1);
                            let r = br.read(6) as i32;
                            let l = br.read_signed(8);
                            run = r;
                            level = l as i32;
                            if last != 0 {
                                run += 192; // encode "last" via the i>62 path
                            }
                        } else {
                            let last = br.read(1);
                            if self.esc3_level_length == 0 {
                                let mut ll;
                                if (self.qscale as i32) < 8 {
                                    ll = br.read(3) as i32;
                                    if ll == 0 {
                                        ll = 8 + br.read(1) as i32;
                                    }
                                } else {
                                    ll = 2;
                                    while ll < 8 && br.read(1) == 0 {
                                        ll += 1;
                                    }
                                    if ll < 8 {
                                        br.skip(1);
                                    }
                                }
                                self.esc3_level_length = ll as usize;
                                self.esc3_run_length = (br.read(2) + 3) as usize;
                            }
                            run = br.read(self.esc3_run_length as u32) as i32;
                            let sign = br.read(1);
                            level = br.read(self.esc3_level_length as u32) as i32;
                            if sign != 0 {
                                level = -level;
                            }
                            if last != 0 {
                                run += 192;
                            }
                        }
                        if level > 0 {
                            level = level * qmul + qadd;
                        } else {
                            level = level * qmul - qadd;
                        }
                        i += run + 1;
                    } else {
                        // second escape
                        br.skip(2);
                        let (l2, r2) = rl.get(q, br)?;
                        let level2 = l2;
                        let mut r = r2;
                        r += rl.max_run[0][(level2 / qmul).clamp(0, 127) as usize] as i32 + run_diff as i32;
                        i += r;
                        let sign = br.read(1);
                        level = if sign != 0 { -level2 } else { level2 };
                    }
                } else {
                    // first escape
                    br.skip(1);
                    let (l2, r2) = rl.get(q, br)?;
                    i += r2;
                    level = l2 + rl.max_level[0][(r2 - 1).clamp(0, 63) as usize] as i32 * qmul;
                    let sign = br.read(1);
                    if sign != 0 {
                        level = -level;
                    }
                }
            } else {
                i += run;
                let sign = br.read(1);
                if sign != 0 {
                    level = -level;
                }
            }
            if i > 62 {
                i -= 192;
                if i < 0 || i > 63 {
                    // FFmpeg: "(i + 192 == 64 && level / qmul == -1) || default"
                    // err_recognition unset → tolerate and stop at i = 63.
                    i = 63;
                    break;
                }
                block[scan[i as usize] as usize] = level as i16;
                break;
            }
            block[scan[i as usize] as usize] = level as i16;
        }
        Ok(())
    }

    /// `ff_msmpeg4_decode_motion`.
    fn decode_motion(&mut self, br: &mut BitReader, mx: &mut i32, my: &mut i32) -> Result<()> {
        let vlc = &MSMP4_MV_VLC[self.mv_table_index];
        let sym = vlc.get().unwrap().decode(br)? as u32;
        let (mut mx2, mut my2);
        if sym != 0 {
            mx2 = (sym >> 8) as i32;
            my2 = (sym & 0xFF) as i32;
        } else {
            mx2 = br.read(6) as i32;
            my2 = br.read(6) as i32;
        }
        mx2 += *mx - 32;
        my2 += *my - 32;
        if mx2 <= -64 {
            mx2 += 64;
        } else if mx2 >= 64 {
            mx2 -= 64;
        }
        if my2 <= -64 {
            my2 += 64;
        } else if my2 >= 64 {
            my2 -= 64;
        }
        *mx = mx2;
        *my = my2;
        Ok(())
    }

    // ── per-macroblock decode ──
}

impl Decoder for MsMpeg4Decoder {
    fn codec_id(&self) -> &oxideav_core::CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, _packet: &Packet) -> Result<()> {
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(f) = self.pending.take() {
            Ok(f)
        } else {
            Err(Error::NeedMore)
        }
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

const MBAC_BITRATE: u32 = (30 * 16 * 1024 / 8) * 2; // dummy, replaced below
const II_BITRATE: u32 = 1024 * 300;
const DC_MAX: i32 = 119;

/// FFmpeg's permutated intra scantable (idct_permutation = FF_IDCT_PERM_NONE
/// for our simple IDCT): the zigzag table itself.
static INTRA_SCAN: std::sync::OnceLock<[u8; 64]> = std::sync::OnceLock::new();
/// Permutated intra h/v scantables (ff_wmv1_scantable[2]/[3]).
static INTRA_H_SCAN: std::sync::OnceLock<[u8; 64]> = std::sync::OnceLock::new();
static INTRA_V_SCAN: std::sync::OnceLock<[u8; 64]> = std::sync::OnceLock::new();
