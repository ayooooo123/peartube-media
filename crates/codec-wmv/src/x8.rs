//! IntraX8 (J-frame) sub-decoder used by WMV2 (and VC-1 Simple/Main X8
//! intra pictures).
//!
//! Ported from FFmpeg commit 2da55bf `libavcodec/intrax8.c`,
//! `intrax8dsp.c` and `intrax8huf.h` (LGPL-2.1-or-later). The block
//! transform is the WMV2 IDCT (`wmv2dsp.c`).

use std::sync::LazyLock;

use crate::bits::BitReader;
use crate::idct;
use crate::mpv::Picture;
use crate::tables::{
    WMV1_SCANTABLE, X8_AC_QUANT_TABLE, X8_DC_QUANT_TABLE, X8_ORIENT_HIGHQUANT_TABLE, X8_ORIENT_LOWQUANT_TABLE,
};
use crate::vlc::Vlc;

const AC_VLC_BITS: u32 = 9;
const DC_VLC_BITS: u32 = 9;
const OR_VLC_BITS: u32 = 7;

struct X8Tables {
    /// `[quant < 13][intra/inter][select]`
    ac: Vec<Vlc>,
    /// `[quant < 13][select]`
    dc: Vec<Vlc>,
    /// `[0][0..2]` high quant, `[1][0..4]` low quant.
    orient_high: Vec<Vlc>,
    orient_low: Vec<Vlc>,
}

fn x8_vlc(nb_bits: u32, table: &[[u8; 2]]) -> Vlc {
    let lens: Vec<i8> = table.iter().map(|e| e[1] as i8).collect();
    let syms: Vec<i32> = table.iter().map(|e| e[0] as i32).collect();
    Vlc::from_lengths(nb_bits, &lens, Some(&syms), 0)
}

static TABLES: LazyLock<X8Tables> = LazyLock::new(|| {
    let mut ac = Vec::with_capacity(32);
    for i in 0..2 {
        for j in 0..2 {
            for k in 0..8 {
                ac.push(x8_vlc(AC_VLC_BITS, &X8_AC_QUANT_TABLE[i][j][k]));
            }
        }
    }
    let mut dc = Vec::with_capacity(16);
    for i in 0..2 {
        for j in 0..8 {
            dc.push(x8_vlc(DC_VLC_BITS, &X8_DC_QUANT_TABLE[i][j]));
        }
    }
    let orient_high = (0..2).map(|i| x8_vlc(OR_VLC_BITS, &X8_ORIENT_HIGHQUANT_TABLE[i])).collect();
    let orient_low = (0..4).map(|i| x8_vlc(OR_VLC_BITS, &X8_ORIENT_LOWQUANT_TABLE[i])).collect();
    X8Tables { ac, dc, orient_high, orient_low }
});

/// `ac_decode_table` of intrax8.c.
const AC_DECODE_TABLE: [u32; 27] = {
    const fn e(eb: u32, extra_run: bool, run: u32, level: u32) -> u32 {
        eb | if extra_run { 0xFF << 8 } else { 0 } | (run << 16) | (level << 24)
    }
    [
        e(3, true, 16, 0),
        e(3, true, 24, 0),
        e(2, true, 4, 1),
        e(3, true, 8, 1),
        e(5, true, 32, 0),
        e(4, true, 16, 1),
        e(2, false, 0, 4),
        e(2, false, 0, 8),
        e(2, false, 0, 12),
        e(3, false, 0, 16),
        e(3, false, 0, 24),
        e(2, false, 1, 3),
        e(3, false, 1, 7),
        e(2, true, 16, 0),
        e(2, true, 20, 0),
        e(2, true, 24, 0),
        e(2, true, 28, 0),
        e(4, true, 32, 0),
        e(4, true, 48, 0),
        e(2, true, 4, 1),
        e(3, true, 8, 1),
        e(4, true, 16, 1),
        e(2, false, 0, 4),
        e(3, false, 0, 8),
        e(4, false, 0, 16),
        e(2, false, 1, 3),
        e(3, false, 1, 7),
    ]
};

const CRAZY_MIX_RUNLEVEL: [u8; 32] = [
    0x22, 0x32, 0x33, 0x53, 0x23, 0x42, 0x43, 0x63, 0x24, 0x52, 0x34, 0x73, 0x25, 0x62, 0x44, 0x83, 0x26, 0x72,
    0x35, 0x54, 0x27, 0x82, 0x45, 0x64, 0x28, 0x92, 0x36, 0x74, 0x29, 0xa2, 0x46, 0x84,
];

const DC_INDEX_OFFSET: [i32; 17] = [0, 1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193];

const QUANT_TABLE: [i32; 64] = [
    256, 256, 256, 256, 256, 256, 259, 262, 265, 269, 272, 275, 278, 282, 285, 288, 292, 295, 299, 303, 306, 310,
    314, 317, 321, 325, 329, 333, 337, 341, 345, 349, 353, 358, 362, 366, 371, 375, 379, 384, 389, 393, 398, 403,
    408, 413, 417, 422, 428, 433, 438, 443, 448, 454, 459, 465, 470, 476, 482, 488, 493, 499, 505, 511,
];

const ZERO_PREDICTION_WEIGHTS: [u32; 128] = [
    640, 640, 669, 480, 708, 354, 748, 257, 792, 198, 760, 143, 808, 101, 772, 72, 480, 669, 537, 537, 598, 416, 661,
    316, 719, 250, 707, 185, 768, 134, 745, 97, 354, 708, 416, 598, 488, 488, 564, 388, 634, 317, 642, 241, 716, 179,
    706, 132, 257, 748, 316, 661, 388, 564, 469, 469, 543, 395, 571, 311, 655, 238, 660, 180, 198, 792, 250, 719, 317,
    634, 395, 543, 469, 469, 507, 380, 597, 299, 616, 231, 161, 855, 206, 788, 266, 710, 340, 623, 411, 548, 455,
    455, 548, 366, 576, 288, 122, 972, 159, 914, 211, 842, 276, 758, 341, 682, 389, 584, 483, 483, 520, 390, 110,
    1172, 144, 1107, 193, 1028, 254, 932, 317, 846, 366, 731, 458, 611, 499, 499,
];

// Scratchpad areas (intrax8dsp.c).
const AREA1: usize = 0;
const AREA2: usize = 8;
const AREA3: usize = 8 + 8;
const AREA4: usize = 8 + 8 + 1;
const AREA5: usize = 8 + 8 + 1 + 8;
const AREA6: usize = 8 + 8 + 1 + 16;

pub struct IntraX8 {
    mb_width: usize,
    mb_height: usize,
    prediction_table: Vec<u8>,
    j_ac: [Option<usize>; 4],
    j_dc: [Option<usize>; 3],
    j_orient: Option<(bool, usize)>,
    use_quant_matrix: bool,
    quant: i32,
    dquant: i32,
    qsum: i32,
    loopfilter: bool,
    quant_dc_chroma: i32,
    divide_quant_dc_luma: i32,
    divide_quant_dc_chroma: i32,
    dest: [usize; 3],
    scratchpad: [u8; 42],
    edges: i32,
    flat_dc: bool,
    predicted_dc: i32,
    raw_orient: i32,
    chroma_orient: i32,
    orient: i32,
    est_run: i32,
    mb_x: usize,
    mb_y: usize,
    block: [i16; 64],
}

impl IntraX8 {
    /// `ff_intrax8_common_init`.
    pub fn new(mb_width: usize, mb_height: usize) -> IntraX8 {
        LazyLock::force(&TABLES);
        IntraX8 {
            mb_width,
            mb_height,
            prediction_table: vec![0; mb_width * 2 * 2],
            j_ac: [None; 4],
            j_dc: [None; 3],
            j_orient: None,
            use_quant_matrix: false,
            quant: 0,
            dquant: 0,
            qsum: 0,
            loopfilter: false,
            quant_dc_chroma: 0,
            divide_quant_dc_luma: 0,
            divide_quant_dc_chroma: 0,
            dest: [0; 3],
            scratchpad: [0; 42],
            edges: 0,
            flat_dc: false,
            predicted_dc: 0,
            raw_orient: 0,
            chroma_orient: 0,
            orient: 0,
            est_run: 0,
            mb_x: 0,
            mb_y: 0,
            block: [0; 64],
        }
    }

    fn select_ac_table(&mut self, br: &mut BitReader, mode: usize) {
        if self.j_ac[mode].is_some() {
            return;
        }
        let table_index = br.read(3) as usize;
        let q = (self.quant < 13) as usize;
        self.j_ac[mode] = Some(q * 16 + (mode >> 1) * 8 + table_index);
    }

    fn get_orient_vlc(&mut self, br: &mut BitReader) -> i32 {
        if self.j_orient.is_none() {
            let low = self.quant < 13;
            let idx = br.read(1 + low as u32) as usize;
            self.j_orient = Some((low, idx));
        }
        let (low, idx) = self.j_orient.unwrap_or((false, 0));
        let t = &*TABLES;
        if low {
            t.orient_low[idx].get(br)
        } else {
            t.orient_high[idx].get(br)
        }
    }

    /// `x8_get_ac_rlf`: returns (run, level, final).
    fn get_ac_rlf(&mut self, br: &mut BitReader, mode: usize) -> (i32, i32, bool) {
        let Some(tab) = self.j_ac[mode] else { return (64, 64, true) };
        let mut i = TABLES.ac[tab].get(br);
        if i < 46 {
            if i < 0 {
                return (64, 64, true);
            }
            let t = i > 22;
            i -= 23 * t as i32;
            let l = (0xE50000 >> (i & 0x1E)) & 3;
            let mask = 0x01030F >> (l << 3);
            (i & mask, l, t)
        } else if i < 73 {
            i -= 46;
            let mut sm = AC_DECODE_TABLE[i as usize];
            let e = br.read(sm & 0xF);
            sm >>= 8;
            let mask = sm & 0xff;
            sm >>= 8;
            let run = (sm & 0xff) + (e & mask);
            let level = (sm >> 8) + (e & !mask);
            (run as i32, level as i32, i > 58 - 46)
        } else if i < 75 {
            let fin = i & 1 == 0;
            let e = br.read(5) as usize;
            (CRAZY_MIX_RUNLEVEL[e] as i32 >> 4, CRAZY_MIX_RUNLEVEL[e] as i32 & 0x0F, fin)
        } else {
            let level = br.read(7 - 3 * (i & 1) as u32) as i32;
            let run = br.read(6) as i32;
            let fin = br.read_bit() != 0;
            (run, level, fin)
        }
    }

    /// `x8_get_dc_rlf`: returns (zero-run, level, final).
    fn get_dc_rlf(&mut self, br: &mut BitReader, mode: usize) -> (i32, i32, bool) {
        if self.j_dc[mode].is_none() {
            let table_index = br.read(3) as usize;
            self.j_dc[mode] = Some((self.quant < 13) as usize * 8 + table_index);
        }
        let tab = self.j_dc[mode].unwrap_or(0);
        let mut i = TABLES.dc[tab].get(br);
        let c = i > 16;
        i -= 17 * c as i32;
        if i <= 0 {
            return (-i, 0, c);
        }
        let mut cc = (i + 1) >> 1;
        cc -= (cc > 1) as i32;
        let e = br.read(cc as u32) as i32;
        let idx = DC_INDEX_OFFSET.get(i as usize).copied().unwrap_or(0) + (e >> 1);
        let s = -(e & 1);
        (0, (idx ^ s) - s, c)
    }

    /// `x8_setup_spatial_predictor`.
    fn setup_spatial_predictor(&mut self, pic: &Picture, br: &mut BitReader, chroma: usize) -> bool {
        let plane = &pic.data[chroma];
        let stride = pic.linesize[chroma];
        let (range, mut sum) = setup_spatial_compensation(plane, self.dest[chroma], &mut self.scratchpad, stride, self.edges);
        let quant;
        if chroma != 0 {
            self.orient = self.chroma_orient;
            quant = self.quant_dc_chroma;
        } else {
            quant = self.quant;
        }
        self.flat_dc = false;
        if range < quant || range < 3 {
            self.orient = 0;
            if range < 3 {
                self.flat_dc = true;
                sum += 9;
                self.predicted_dc = (sum * 6899) >> 17;
            }
        }
        if chroma != 0 {
            return true;
        }
        if range < 2 * self.quant {
            if self.edges & 3 == 0 {
                if self.orient == 1 {
                    self.orient = 11;
                }
                if self.orient == 2 {
                    self.orient = 10;
                }
            } else {
                self.orient = 0;
            }
            self.raw_orient = 0;
        } else {
            const PREDICTION_TABLE: [[i32; 12]; 3] = [
                [0, 8, 4, 10, 11, 2, 6, 9, 1, 3, 5, 7],
                [4, 0, 8, 11, 10, 3, 5, 2, 6, 9, 1, 7],
                [8, 0, 4, 10, 11, 1, 7, 2, 6, 9, 3, 5],
            ];
            self.raw_orient = self.get_orient_vlc(br);
            if self.raw_orient < 0 || self.raw_orient >= 12 || self.orient < 0 || self.orient >= 3 {
                return false;
            }
            self.orient = PREDICTION_TABLE[self.orient as usize][self.raw_orient as usize];
        }
        true
    }

    fn update_predictions(&mut self, orient: i32, est_run: i32) {
        self.prediction_table[self.mb_x * 2 + (self.mb_y & 1)] =
            ((est_run << 2) + (orient == 4) as i32 + 2 * (orient == 8) as i32) as u8;
    }

    fn get_prediction_chroma(&mut self) {
        self.edges = (self.mb_x >> 1 == 0) as i32;
        self.edges |= 2 * (self.mb_y >> 1 == 0) as i32;
        self.edges |= 4 * (self.mb_x >= 2 * self.mb_width - 1) as i32;
        self.raw_orient = 0;
        if self.edges & 3 != 0 {
            self.chroma_orient = 4 << ((0xCC >> self.edges) & 1);
            return;
        }
        self.chroma_orient = ((self.prediction_table[2 * self.mb_x - 2] & 0x03) as i32) << 2;
    }

    fn get_prediction(&mut self) {
        self.edges = (self.mb_x == 0) as i32;
        self.edges |= 2 * (self.mb_y == 0) as i32;
        self.edges |= 4 * (self.mb_x >= 2 * self.mb_width - 1) as i32;
        let odd = self.mb_y & 1;
        match self.edges & 3 {
            0 => {}
            1 => {
                self.est_run = (self.prediction_table[1 - odd] >> 2) as i32;
                self.orient = 1;
                return;
            }
            2 => {
                self.est_run = (self.prediction_table[2 * self.mb_x - 2] >> 2) as i32;
                self.orient = 2;
                return;
            }
            _ => {
                self.est_run = 16;
                self.orient = 0;
                return;
            }
        }
        let b = self.prediction_table[2 * self.mb_x + 1 - odd] as i32;
        let a = self.prediction_table[2 * self.mb_x - 2 + odd] as i32;
        let c = self.prediction_table[2 * self.mb_x - 2 + 1 - odd] as i32;
        self.est_run = b.min(a);
        if (self.mb_x & self.mb_y) != 0 {
            self.est_run = c.min(self.est_run);
        }
        self.est_run >>= 2;
        let (a, b, c) = (a & 3, b & 3, c & 3);
        let i = (0xFFEAF4C4u32 >> (2 * b + 8 * a)) & 3;
        if i != 3 {
            self.orient = i as i32;
        } else {
            self.orient = ((0xFFEAD8u32 >> (2 * c + 8 * (self.quant > 12) as i32)) & 3) as i32;
        }
    }

    fn ac_compensation(&mut self, direction: i32, dc_level: i32) {
        let t = |x: i32| (x.wrapping_mul(dc_level).wrapping_add(0x8000)) >> 16;
        let b = &mut self.block;
        let mut add = |x: usize, y: usize, v: i32| {
            let e = &mut b[x + y * 8];
            *e = (*e as i32).wrapping_add(v) as i16;
        };
        match direction {
            0 => {
                let v = t(3811);
                add(1, 0, -v);
                add(0, 1, -v);
                let v = t(487);
                add(2, 0, -v);
                add(0, 2, -v);
                let v = t(506);
                add(3, 0, -v);
                add(0, 3, -v);
                let v = t(135);
                add(4, 0, -v);
                add(0, 4, -v);
                add(2, 1, v);
                add(1, 2, v);
                add(3, 1, v);
                add(1, 3, v);
                let v = t(173);
                add(5, 0, -v);
                add(0, 5, -v);
                let v = t(61);
                add(6, 0, -v);
                add(0, 6, -v);
                add(5, 1, v);
                add(1, 5, v);
                let v = t(42);
                add(7, 0, -v);
                add(0, 7, -v);
                add(4, 1, v);
                add(1, 4, v);
                add(4, 4, v);
                let v = t(1084);
                add(1, 1, v);
            }
            1 => {
                add(0, 1, -t(6269));
                add(0, 3, -t(708));
                add(0, 5, -t(172));
                add(0, 7, -t(73));
            }
            2 => {
                add(1, 0, -t(6269));
                add(3, 0, -t(708));
                add(5, 0, -t(172));
                add(7, 0, -t(73));
            }
            _ => {}
        }
    }

    /// `x8_decode_intra_mb`.
    fn decode_intra_mb(&mut self, pic: &mut Picture, br: &mut BitReader, chroma: usize) -> bool {
        self.block = [0; 64];
        let dc_mode = if chroma != 0 { 2 } else { (self.est_run != 0) as usize };
        let (zr, mut dc_level, mut fin) = self.get_dc_rlf(br, dc_mode);
        if zr != 0 {
            return false;
        }
        let mut n = 0;
        let mut zeros_only = false;
        let stride = pic.linesize[chroma];
        let dest = self.dest[chroma];
        if !fin {
            let mut use_quant_matrix = self.use_quant_matrix;
            let mut ac_mode;
            let est_run;
            if chroma != 0 {
                ac_mode = 1;
                est_run = 64;
            } else {
                if self.raw_orient < 3 {
                    use_quant_matrix = false;
                }
                if self.raw_orient > 4 {
                    ac_mode = 0;
                    est_run = 64;
                } else if self.est_run > 1 {
                    ac_mode = 2;
                    est_run = self.est_run;
                } else {
                    ac_mode = 3;
                    est_run = 64;
                }
            }
            self.select_ac_table(br, ac_mode);
            let scan = &WMV1_SCANTABLE[[0usize, 2, 3][((0x928548u32 >> (2 * self.orient)) & 3) as usize]];
            let mut pos = 0i32;
            loop {
                n += 1;
                if n >= est_run {
                    ac_mode = 3;
                    self.select_ac_table(br, 3);
                }
                let (run, level, f) = self.get_ac_rlf(br, ac_mode);
                fin = f;
                pos += run + 1;
                if pos > 63 {
                    return false;
                }
                let mut level = (level + 1).wrapping_mul(self.dquant);
                level = level.wrapping_add(self.qsum);
                let sign = -(br.read_bit() as i32);
                level = (level ^ sign) - sign;
                if use_quant_matrix {
                    level = level.wrapping_mul(QUANT_TABLE[pos as usize]) >> 8;
                }
                self.block[scan[pos as usize] as usize] = level as i16;
                if fin {
                    break;
                }
            }
        } else {
            if self.flat_dc && ((dc_level + 1) as u32) < 3 {
                let (divide_quant, dc_quant) = if chroma == 0 {
                    (self.divide_quant_dc_luma, self.quant)
                } else {
                    (self.divide_quant_dc_chroma, self.quant_dc_chroma)
                };
                dc_level = dc_level.wrapping_add((self.predicted_dc.wrapping_mul(divide_quant).wrapping_add(1 << 12)) >> 13);
                let v = ((dc_level.wrapping_mul(dc_quant).wrapping_add(4)) >> 3).clamp(0, 255) as u8;
                put_solidcolor(v, &mut pic.data[chroma], dest, stride);
                return self.block_placed(pic, chroma, n, zeros_only);
            }
            zeros_only = dc_level == 0;
        }
        self.block[0] = if chroma == 0 {
            dc_level.wrapping_mul(self.quant)
        } else {
            dc_level.wrapping_mul(self.quant_dc_chroma)
        } as i16;

        if ((dc_level + 1) as u32) >= 3 && (self.edges & 3) != 3 {
            let direction = (0x6A017C >> (self.orient * 2)) & 3;
            if direction != 3 {
                let dc = self.block[0] as i32;
                self.ac_compensation(direction, dc);
            }
        }

        if self.flat_dc {
            put_solidcolor(self.predicted_dc as u8, &mut pic.data[chroma], dest, stride);
        } else {
            spatial_compensation(self.orient, &self.scratchpad, &mut pic.data[chroma], dest, stride);
        }
        if !zeros_only {
            idct::wmv2_idct_add(&mut pic.data[chroma], dest, stride, &mut self.block);
        }
        self.block_placed(pic, chroma, n, zeros_only)
    }

    fn block_placed(&mut self, pic: &mut Picture, chroma: usize, n: i32, zeros_only: bool) -> bool {
        if chroma == 0 {
            self.update_predictions(self.orient, n);
        }
        if self.loopfilter {
            let off = self.dest[chroma];
            let stride = pic.linesize[chroma];
            if !((self.edges & 2) != 0 || (zeros_only && (self.orient | 4) == 4)) {
                x8_loop_filter(&mut pic.data[chroma], off, stride as isize, 1, self.quant);
            }
            if !((self.edges & 1) != 0 || (zeros_only && (self.orient | 8) == 8)) {
                x8_loop_filter(&mut pic.data[chroma], off, 1, stride as isize, self.quant);
            }
        }
        true
    }

    fn init_block_index(&mut self, pic: &Picture) {
        self.dest[0] = (self.mb_y * pic.linesize[0]) << 3;
        self.dest[1] = ((self.mb_y & !1) * pic.linesize[1]) << 2;
        self.dest[2] = ((self.mb_y & !1) * pic.linesize[2]) << 2;
    }

    /// `ff_intrax8_decode_picture`.
    pub fn decode_picture(
        &mut self,
        pic: &mut Picture,
        br: &mut BitReader,
        dquant: i32,
        quant_offset: i32,
        loopfilter: bool,
        qscale_table: &mut [i8],
    ) {
        self.dquant = dquant;
        self.quant = dquant >> 1;
        self.qsum = quant_offset;
        self.loopfilter = loopfilter;
        self.use_quant_matrix = br.read_bit() != 0;
        if self.quant <= 0 {
            return;
        }
        self.divide_quant_dc_luma = ((1 << 16) + (self.quant >> 1)) / self.quant;
        if self.quant < 5 {
            self.quant_dc_chroma = self.quant;
            self.divide_quant_dc_chroma = self.divide_quant_dc_luma;
        } else {
            self.quant_dc_chroma = self.quant + ((self.quant + 3) >> 3);
            self.divide_quant_dc_chroma = ((1 << 16) + (self.quant_dc_chroma >> 1)) / self.quant_dc_chroma;
        }
        self.j_ac = [None; 4];
        self.j_dc = [None; 3];
        self.j_orient = None;

        self.mb_y = 0;
        while self.mb_y < self.mb_height * 2 {
            self.init_block_index(pic);
            let mut mb_xy = (self.mb_y >> 1) * (self.mb_width + 1);
            if br.bits_left() < 1 {
                return;
            }
            self.mb_x = 0;
            while self.mb_x < self.mb_width * 2 {
                self.get_prediction();
                if !self.setup_spatial_predictor(pic, br, 0) {
                    return;
                }
                if !self.decode_intra_mb(pic, br, 0) {
                    return;
                }
                if self.mb_x & self.mb_y & 1 != 0 {
                    self.get_prediction_chroma();
                    self.setup_spatial_predictor(pic, br, 1);
                    if !self.decode_intra_mb(pic, br, 1) {
                        return;
                    }
                    self.setup_spatial_predictor(pic, br, 2);
                    if !self.decode_intra_mb(pic, br, 2) {
                        return;
                    }
                    self.dest[1] += 8;
                    self.dest[2] += 8;
                    if let Some(q) = qscale_table.get_mut(mb_xy) {
                        *q = self.quant as i8;
                    }
                    mb_xy += 1;
                }
                self.dest[0] += 8;
                self.mb_x += 1;
            }
            self.mb_y += 1;
        }
    }
}

fn put_solidcolor(pix: u8, dst: &mut [u8], off: usize, stride: usize) {
    for k in 0..8 {
        dst[off + k * stride..off + k * stride + 8].fill(pix);
    }
}

/// `x8_setup_spatial_compensation`: returns (range, sum).
fn setup_spatial_compensation(src: &[u8], off: usize, dst: &mut [u8; 42], stride: usize, edges: i32) -> (i32, i32) {
    if edges & 3 == 3 {
        dst[..16 + 1 + 16 + 8].fill(0x80);
        return (0, 0x80 * (8 + 1 + 8 + 2));
    }
    let mut min_pix = 256i32;
    let mut max_pix = -1i32;
    let mut sum = 0i32;
    if edges & 1 == 0 {
        let mut ptr = off - 1;
        for i in (0..8).rev() {
            dst[AREA1 + i] = src[ptr - 1];
            let c = src[ptr] as i32;
            sum += c;
            min_pix = min_pix.min(c);
            max_pix = max_pix.max(c);
            dst[AREA2 + i] = c as u8;
            ptr += stride;
        }
    }
    if edges & 2 == 0 {
        let ptr = off - stride;
        let mut c = 0u8;
        for i in 0..8 {
            c = src[ptr + i];
            sum += c as i32;
            min_pix = min_pix.min(c as i32);
            max_pix = max_pix.max(c as i32);
        }
        if edges & 4 != 0 {
            dst[AREA5..AREA5 + 8].fill(c);
            dst[AREA4..AREA4 + 8].copy_from_slice(&src[ptr..ptr + 8]);
        } else {
            dst[AREA4..AREA4 + 16].copy_from_slice(&src[ptr..ptr + 16]);
        }
        dst[AREA6..AREA6 + 8].copy_from_slice(&src[ptr - stride..ptr - stride + 8]);
    }
    if edges & 3 != 0 {
        let avg = ((sum + 4) >> 3) as u8;
        if edges & 1 != 0 {
            dst[AREA1..AREA1 + 17].fill(avg);
        } else {
            dst[AREA3..AREA3 + 25].fill(avg);
        }
        sum += avg as i32 * 9;
    } else {
        let c = src[off - 1 - stride];
        dst[AREA3] = c;
        sum += c as i32;
    }
    let range = max_pix - min_pix;
    sum += dst[AREA5] as i32 + dst[AREA5 + 1] as i32;
    (range, sum)
}

fn spatial_compensation(orient: i32, src: &[u8; 42], dst: &mut [u8], off: usize, stride: usize) {
    let mut put = |x: usize, y: usize, v: u8| dst[off + y * stride + x] = v;
    match orient {
        0 => {
            let mut left_sum = [[0u32; 8]; 2];
            let mut top_sum = [[0u32; 8]; 2];
            for i in 0..8i32 {
                let a = (src[AREA2 + 7 - i as usize] as u32) << 4;
                for j in 0..8i32 {
                    let p = (i - j).unsigned_abs();
                    left_sum[(p & 1) as usize][j as usize] += a >> (p >> 1);
                }
            }
            for i in 0..12i32 {
                let a = (src[AREA4 + i as usize] as u32) << 4;
                let jstart = if i < 8 {
                    0
                } else if i < 10 {
                    5
                } else {
                    7
                };
                for j in jstart..8i32 {
                    let p = (i - j).unsigned_abs();
                    top_sum[(p & 1) as usize][j as usize] += a >> (p >> 1);
                }
            }
            for i in 0..8 {
                // uint16_t accumulators in C.
                top_sum[0][i] = (top_sum[0][i] + ((top_sum[1][i] as u16 as u32 * 181 + 128) >> 8)) & 0xFFFF;
                left_sum[0][i] = (left_sum[0][i] + ((left_sum[1][i] as u16 as u32 * 181 + 128) >> 8)) & 0xFFFF;
            }
            for y in 0..8 {
                for x in 0..8 {
                    let v = ((top_sum[0][x] & 0xFFFF) * ZERO_PREDICTION_WEIGHTS[y * 16 + x * 2]
                        + (left_sum[0][y] & 0xFFFF) * ZERO_PREDICTION_WEIGHTS[y * 16 + x * 2 + 1]
                        + 0x8000)
                        >> 16;
                    put(x, y, v as u8);
                }
            }
        }
        1 => {
            for y in 0..8 {
                for x in 0..8 {
                    put(x, y, src[AREA4 + (2 * y + x + 2).min(15)]);
                }
            }
        }
        2 => {
            for y in 0..8 {
                for x in 0..8 {
                    put(x, y, src[AREA4 + 1 + y + x]);
                }
            }
        }
        3 => {
            for y in 0..8 {
                for x in 0..8 {
                    put(x, y, src[AREA4 + ((y + 1) >> 1) + x]);
                }
            }
        }
        4 => {
            for y in 0..8 {
                for x in 0..8 {
                    put(x, y, ((src[AREA4 + x] as u32 + src[AREA6 + x] as u32 + 1) >> 1) as u8);
                }
            }
        }
        5 => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    let v = if 2 * x - y < 0 {
                        src[(AREA2 as i32 + 9 + 2 * x - y) as usize]
                    } else {
                        src[(AREA4 as i32 + x - ((y + 1) >> 1)) as usize]
                    };
                    put(x as usize, y as usize, v);
                }
            }
        }
        6 => {
            for y in 0..8 {
                for x in 0..8 {
                    put(x, y, src[AREA3 - y + x]);
                }
            }
        }
        7 => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    let v = if x - 2 * y > 0 {
                        ((src[(AREA3 as i32 - 1 + x - 2 * y) as usize] as u32
                            + src[(AREA3 as i32 + x - 2 * y) as usize] as u32
                            + 1)
                            >> 1) as u8
                    } else {
                        src[(AREA2 as i32 + 8 - y + (x >> 1)) as usize]
                    };
                    put(x as usize, y as usize, v);
                }
            }
        }
        8 => {
            for y in 0..8 {
                for x in 0..8 {
                    put(x, y, ((src[AREA1 + 7 - y] as u32 + src[AREA2 + 7 - y] as u32 + 1) >> 1) as u8);
                }
            }
        }
        9 => {
            for y in 0..8 {
                for x in 0..8 {
                    put(x, y, src[AREA2 + 6 - (x + y).min(6)]);
                }
            }
        }
        10 => {
            for y in 0..8 {
                for x in 0..8 {
                    put(
                        x,
                        y,
                        ((src[AREA2 + 7 - y] as u32 * (8 - x as u32) + src[AREA4 + x] as u32 * x as u32 + 4) >> 3) as u8,
                    );
                }
            }
        }
        _ => {
            for y in 0..8 {
                for x in 0..8 {
                    put(
                        x,
                        y,
                        ((src[AREA2 + 7 - y] as u32 * y as u32 + src[AREA4 + x] as u32 * (8 - y as u32) + 4) >> 3) as u8,
                    );
                }
            }
        }
    }
}

/// `x8_loop_filter`.
fn x8_loop_filter(buf: &mut [u8], off: usize, a_stride: isize, b_stride: isize, quant: i32) {
    let ql = (quant + 10) >> 3;
    let mut ptr = off as isize;
    for _ in 0..8 {
        let at = |k: isize| (ptr + k * a_stride) as usize;
        let p0 = buf[at(-5)] as i32;
        let p1 = buf[at(-4)] as i32;
        let p2 = buf[at(-3)] as i32;
        let p3 = buf[at(-2)] as i32;
        let p4 = buf[at(-1)] as i32;
        let p5 = buf[at(0)] as i32;
        let p6 = buf[at(1)] as i32;
        let p7 = buf[at(2)] as i32;
        let p8 = buf[at(3)] as i32;
        let p9 = buf[at(4)] as i32;

        let mut t = ((p1 - p2).abs() <= ql) as i32
            + ((p2 - p3).abs() <= ql) as i32
            + ((p3 - p4).abs() <= ql) as i32
            + ((p4 - p5).abs() <= ql) as i32;
        let mut done = false;
        if t > 0 {
            t += ((p5 - p6).abs() <= ql) as i32
                + ((p6 - p7).abs() <= ql) as i32
                + ((p7 - p8).abs() <= ql) as i32
                + ((p8 - p9).abs() <= ql) as i32
                + ((p0 - p1).abs() <= ql) as i32;
            if t >= 6 {
                let mut min = p1.min(p3).min(p5).min(p8);
                let mut max = p1.max(p3).max(p5).max(p8);
                if max - min < 2 * quant {
                    min = min.min(p2).min(p4).min(p6).min(p7);
                    max = max.max(p2).max(p4).max(p6).max(p7);
                    if max - min < 2 * quant {
                        buf[at(-2)] = ((4 * p2 + 3 * p3 + p7 + 4) >> 3) as u8;
                        buf[at(-1)] = ((3 * p2 + 3 * p4 + 2 * p7 + 4) >> 3) as u8;
                        buf[at(0)] = ((2 * p2 + 3 * p5 + 3 * p7 + 4) >> 3) as u8;
                        buf[at(1)] = ((p2 + 3 * p6 + 4 * p7 + 4) >> 3) as u8;
                        done = true;
                    }
                }
            }
        }
        if !done {
            let x0 = (2 * p3 - 5 * p4 + 5 * p5 - 2 * p6 + 4) >> 3;
            if x0.abs() < quant {
                let x1 = (2 * p1 - 5 * p2 + 5 * p3 - 2 * p4 + 4) >> 3;
                let x2 = (2 * p5 - 5 * p6 + 5 * p7 - 2 * p8 + 4) >> 3;
                let mut x = x0.abs() - x1.abs().min(x2.abs());
                let mut m = p4 - p5;
                if x > 0 && (m ^ x0) < 0 {
                    let sign = m >> 31;
                    m = (m ^ sign) - sign;
                    m >>= 1;
                    x = (5 * x) >> 3;
                    if x > m {
                        x = m;
                    }
                    x = (x ^ sign) - sign;
                    buf[at(-1)] = (p4 - x) as u8;
                    buf[at(0)] = (p5 + x) as u8;
                }
            }
        }
        ptr += b_stride;
    }
}
