//! MPEG / H.263 decoder state.
//! Ported from FFmpeg libavcodec/mpegvideo.h, mpegvideodec.c (commit 2da55bf).
//! License: GNU Lesser General Public License, version 2.1 or later.

#![forbid(unsafe_code)]

use crate::mpegtables::{ALTERNATE_HORIZONTAL_SCAN, ZIGZAG_DIRECT};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MotionType {
    #[default]
    Mv16x16,
    Mv8x8,
    MvField,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PLANE {
    Y,
    U,
    V,
}

#[derive(Default, Clone)]
pub struct Picture {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

pub struct MpegState {
    pub width: usize,
    pub height: usize,
    pub mb_width: usize,
    pub mb_height: usize,
    pub mb_stride: usize,
    pub b8_stride: usize,
    pub linesize: usize,
    pub uvlinesize: usize,

    pub mb_x: usize,
    pub mb_y: usize,
    pub resync_mb_x: usize,
    pub resync_mb_y: usize,
    pub first_slice_line: bool,

    pub qscale: u32,
    pub chroma_qscale: u32,
    pub chroma_qscale_table: [u32; 32],
    pub qscale_table: Vec<u32>,
    pub mb_type: Vec<u32>,

    pub block_index: [isize; 6],
    pub block_last_index: [i32; 6],
    pub block_wrap: [usize; 6],

    pub dc_val: Vec<i16>,
    pub ac_val: Vec<i16>,

    pub mv: [[[i32; 2]; 4]; 2],
    pub motion_val: [Vec<[i16; 2]>; 2],
    pub mv_type: MotionType,
    pub mv_dir: usize,

    pub mb_intra: bool,
    pub mb_skipped: bool,
    pub h263_aic: bool,
    pub h263_aic_dir: bool,
    pub h263_pred: bool,
    pub alt_inter_vlc: bool,
    pub ac_pred: bool,
    pub obmc: bool,
    pub codec_id_rv10: bool,
    pub modified_quant: bool,
    pub no_rounding: bool,
    pub pict_type: i32,

    pub y_dc_scale: u32,
    pub c_dc_scale: u32,

    pub intra_scantable: [usize; 64],
    pub inter_scantable: [usize; 64],
    pub intra_h_scantable: [usize; 64],

    pub cur_pic: Picture,
    pub refs: [Picture; 2],
    pub dest: [usize; 3],
}

impl MpegState {
    pub fn new(width: usize, height: usize) -> Self {
        let mb_width = (width + 15) / 16;
        let mb_height = (height + 15) / 16;
        let mb_stride = mb_width + 1;
        let b8_stride = mb_width * 2 + 1;
        let linesize = mb_width * 16;
        let uvlinesize = mb_width * 8;
        let b8_num = (mb_width * 2 + 2) * (mb_height * 2 + 2);

        let mut intra_scantable = [0usize; 64];
        let mut inter_scantable = [0usize; 64];
        let mut intra_h_scantable = [0usize; 64];
        for i in 0..64 {
            intra_scantable[i] = ZIGZAG_DIRECT[i] as usize;
            inter_scantable[i] = ZIGZAG_DIRECT[i] as usize;
            intra_h_scantable[i] = ALTERNATE_HORIZONTAL_SCAN[i] as usize;
        }

        Self {
            width,
            height,
            mb_width,
            mb_height,
            mb_stride,
            b8_stride,
            linesize,
            uvlinesize,
            mb_x: 0,
            mb_y: 0,
            resync_mb_x: 0,
            resync_mb_y: 0,
            first_slice_line: true,
            qscale: 1,
            chroma_qscale: 1,
            chroma_qscale_table: [
                0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
                16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
            ],
            qscale_table: vec![0; mb_stride * (mb_height + 2)],
            mb_type: vec![0; mb_stride * (mb_height + 2)],
            block_index: [0; 6],
            block_last_index: [-1; 6],
            block_wrap: [b8_stride, b8_stride, b8_stride, b8_stride, mb_stride, mb_stride],
            dc_val: vec![1024; b8_stride + 1 + b8_stride * (2 * mb_height + 1) + 2 * mb_stride * (mb_height + 2) + 256],
            ac_val: vec![0; (b8_stride + 1 + b8_stride * (2 * mb_height + 1) + 2 * mb_stride * (mb_height + 2) + 256) * 16],
            mv: [[[0; 2]; 4]; 2],
            motion_val: [vec![[0; 2]; b8_num], vec![[0; 2]; b8_num]],
            mv_type: MotionType::Mv16x16,
            mv_dir: 0,
            mb_intra: false,
            mb_skipped: false,
            h263_aic: false,
            h263_aic_dir: false,
            h263_pred: false,
            alt_inter_vlc: false,
            ac_pred: false,
            obmc: false,
            codec_id_rv10: false,
            modified_quant: false,
            no_rounding: false,
            pict_type: 1,
            y_dc_scale: 8,
            c_dc_scale: 8,
            intra_scantable,
            inter_scantable,
            intra_h_scantable,
            cur_pic: Picture {
                y: vec![0; linesize * mb_height * 16],
                u: vec![0; uvlinesize * mb_height * 8],
                v: vec![0; uvlinesize * mb_height * 8],
            },
            refs: [
                Picture {
                    y: vec![0; linesize * mb_height * 16],
                    u: vec![0; uvlinesize * mb_height * 8],
                    v: vec![0; uvlinesize * mb_height * 8],
                },
                Picture {
                    y: vec![0; linesize * mb_height * 16],
                    u: vec![0; uvlinesize * mb_height * 8],
                    v: vec![0; uvlinesize * mb_height * 8],
                },
            ],
            dest: [0; 3],
        }
    }

    pub fn clear_blocks(&mut self, block: &mut [[i16; 64]; 6]) {
        for b in block.iter_mut() {
            b.fill(0);
        }
    }

    pub fn set_qscale(&mut self, q: u32) {
        self.qscale = q.clamp(1, 31);
        let idx = self.qscale as usize;
        self.chroma_qscale = self.chroma_qscale_table[idx];
        if self.h263_aic {
            self.y_dc_scale = crate::h263tables::AIC_DC_SCALE_TABLE[idx] as u32;
            self.c_dc_scale = crate::h263tables::AIC_DC_SCALE_TABLE[idx] as u32;
        } else {
            self.y_dc_scale = crate::h263tables::MPEG1_DC_SCALE_TABLE[idx] as u32;
            self.c_dc_scale = crate::h263tables::MPEG1_DC_SCALE_TABLE[idx] as u32;
        }
    }

    pub fn ref_planes(&self, dir: usize) -> (&[u8], &[u8], &[u8]) {
        let p = &self.refs[dir.min(1)];
        (&p.y, &p.u, &p.v)
    }

    pub fn ref_y(&self, dir: usize, _field_select: usize) -> &[u8] {
        &self.refs[dir.min(1)].y
    }

    pub fn intra_scantable_raster_end(&self, idx: usize) -> usize {
        idx
    }

    pub fn inter_scantable_raster_end(&self, idx: usize) -> usize {
        idx
    }

    pub fn init_block_index(&mut self) {
        let b8_stride = self.b8_stride as isize;
        let mb_stride = self.mb_stride as isize;
        let mb_x = self.mb_x as isize;
        let mb_y = self.mb_y as isize;
        let mb_h = self.mb_height as isize;

        self.block_index[0] = b8_stride * (mb_y * 2) - 2 + mb_x * 2;
        self.block_index[1] = b8_stride * (mb_y * 2) - 1 + mb_x * 2;
        self.block_index[2] = b8_stride * (mb_y * 2 + 1) - 2 + mb_x * 2;
        self.block_index[3] = b8_stride * (mb_y * 2 + 1) - 1 + mb_x * 2;
        self.block_index[4] = mb_stride * (mb_y + 1) + b8_stride * mb_h * 2 + mb_x - 1;
        self.block_index[5] = mb_stride * (mb_y + mb_h + 2) + b8_stride * mb_h * 2 + mb_x - 1;

        self.dest[0] = self.mb_y * 16 * self.linesize + self.mb_x * 16;
        self.dest[1] = self.mb_y * 8 * self.uvlinesize + self.mb_x * 8;
        self.dest[2] = self.mb_y * 8 * self.uvlinesize + self.mb_x * 8;
    }

    pub fn update_block_index(&mut self) {
        self.block_index[0] += 2;
        self.block_index[1] += 2;
        self.block_index[2] += 2;
        self.block_index[3] += 2;
        self.block_index[4] += 1;
        self.block_index[5] += 1;

        self.dest[0] = self.mb_y * 16 * self.linesize + self.mb_x * 16;
        self.dest[1] = self.mb_y * 8 * self.uvlinesize + self.mb_x * 8;
        self.dest[2] = self.mb_y * 8 * self.uvlinesize + self.mb_x * 8;
    }

    pub fn clean_intra_table_entries(&mut self) {
        let base = (self.b8_stride + 1) as isize;
        let wrap = self.b8_stride as isize;
        let xy = (base + self.block_index[0]) as usize;
        let uxy = (base + self.block_index[4]) as usize;
        let vxy = (base + self.block_index[5]) as usize;

        if xy + wrap as usize + 1 < self.dc_val.len() {
            self.dc_val[xy] = 1024;
            self.dc_val[xy + 1] = 1024;
            self.dc_val[xy + wrap as usize] = 1024;
            self.dc_val[xy + wrap as usize + 1] = 1024;
        }
        if uxy < self.dc_val.len() {
            self.dc_val[uxy] = 1024;
        }
        if vxy < self.dc_val.len() {
            self.dc_val[vxy] = 1024;
        }

        if (xy + wrap as usize + 2) * 16 <= self.ac_val.len() {
            self.ac_val[(xy + 1) * 16..(xy + 2) * 16].fill(0);
            self.ac_val[(xy + wrap as usize) * 16..(xy + wrap as usize + 2) * 16].fill(0);
        }
        if (uxy + 1) * 16 <= self.ac_val.len() {
            self.ac_val[uxy * 16..(uxy + 1) * 16].fill(0);
        }
        if (vxy + 1) * 16 <= self.ac_val.len() {
            self.ac_val[vxy * 16..(vxy + 1) * 16].fill(0);
        }
    }
}
