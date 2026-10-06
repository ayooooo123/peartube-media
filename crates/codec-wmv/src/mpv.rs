//! Picture buffers and the pixel operations shared by the MS-MPEG-4 / WMV
//! decoders.
//!
//! Ported from FFmpeg commit 2da55bf (LGPL-2.1-or-later):
//! `libavcodec/hpeldsp.c` + `hpel_template.c` (`put[_no_rnd]_pixels*`),
//! `libavcodec/videodsp_template.c` (`emulated_edge_mc`),
//! `libavcodec/h263dsp.c` (H.263 in-loop filter) and the picture handling of
//! `mpegvideo_dec.c` (dummy gray reference frames).
//!
//! Pictures are stored without padding; every motion-compensated read that
//! could leave the coded area goes through [`emulated_edge_mc`], which
//! replicates the border exactly like FFmpeg's edge emulation.

use oxideav_core::{Frame, VideoFrame, VideoPlane};

/// A decoded 4:2:0 picture covering the macroblock-aligned coded area.
#[derive(Clone)]
pub struct Picture {
    pub data: [Vec<u8>; 3],
    pub linesize: [usize; 3],
    /// Luma width/height of the stored area (multiple of 16).
    pub width: usize,
    pub height: usize,
}

impl Picture {
    pub fn new(width: usize, height: usize) -> Picture {
        let cw = width.div_ceil(2);
        let ch = height.div_ceil(2);
        Picture {
            data: [vec![0u8; width * height], vec![0u8; cw * ch], vec![0u8; cw * ch]],
            linesize: [width, cw, cw],
            width,
            height,
        }
    }

    /// `color_frame`: fill the visible `w`×`h` area (chroma rounded up).
    pub fn fill(&mut self, w: usize, h: usize, luma: u8, chroma: u8) {
        let w = w.min(self.width);
        let h = h.min(self.height);
        for y in 0..h {
            let o = y * self.linesize[0];
            self.data[0][o..o + w].fill(luma);
        }
        let (cw, ch) = (w.div_ceil(2).min(self.linesize[1]), h.div_ceil(2));
        for p in 1..3 {
            for y in 0..ch.min(self.data[p].len() / self.linesize[p].max(1)) {
                let o = y * self.linesize[p];
                self.data[p][o..o + cw].fill(chroma);
            }
        }
    }

    /// Copy the visible `w`×`h` area into an output frame.
    pub fn to_frame(&self, w: usize, h: usize, pts: Option<i64>) -> Frame {
        let mut planes = Vec::with_capacity(3);
        for p in 0..3 {
            let (pw, ph) = if p == 0 { (w, h) } else { (w.div_ceil(2), h.div_ceil(2)) };
            let ls = self.linesize[p];
            let mut data = Vec::with_capacity(pw * ph);
            for y in 0..ph {
                data.extend_from_slice(&self.data[p][y * ls..y * ls + pw]);
            }
            planes.push(VideoPlane { stride: pw, data });
        }
        Frame::Video(VideoFrame { pts, planes })
    }
}

/// `emulated_edge_mc`: copy a `bw`×`bh` block whose top-left corner is at
/// (`x`, `y`) in a `w`×`h` plane into `dst` (stride `dst_stride`), replicating
/// the plane's border pixels for coordinates outside it.
#[allow(clippy::too_many_arguments)]
pub fn emulated_edge_mc(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    src_stride: usize,
    bw: usize,
    bh: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    for r in 0..bh {
        let sy = (y + r as i32).clamp(0, h - 1) as usize;
        let row = &src[sy * src_stride..];
        let d = &mut dst[r * dst_stride..r * dst_stride + bw];
        for (c, px) in d.iter_mut().enumerate() {
            let sx = (x + c as i32).clamp(0, w - 1) as usize;
            *px = row[sx];
        }
    }
}

/// Source block for motion compensation: either the reference plane itself
/// (the block lies inside it) or an edge-emulated copy.
pub struct SrcBlock<'a> {
    pub data: &'a [u8],
    pub off: usize,
    pub stride: usize,
}

/// Returns the `bw`×`bh` area at (`x`, `y`) of `plane` (size `w`×`h`,
/// stride `stride`), edge-emulated into `buf` when it leaves the plane.
#[allow(clippy::too_many_arguments)]
pub fn src_block<'a>(
    plane: &'a [u8],
    stride: usize,
    w: i32,
    h: i32,
    x: i32,
    y: i32,
    bw: usize,
    bh: usize,
    buf: &'a mut [u8],
) -> SrcBlock<'a> {
    if x >= 0 && y >= 0 && x + bw as i32 <= w && y + bh as i32 <= h {
        SrcBlock { data: plane, off: y as usize * stride + x as usize, stride }
    } else {
        emulated_edge_mc(buf, bw, plane, stride, bw, bh, x, y, w, h);
        SrcBlock { data: buf, off: 0, stride: bw }
    }
}

/// `put_pixels{8,16}_{,x2,y2,xy2}` (`no_rnd = false`) and
/// `put_no_rnd_pixels*` (`no_rnd = true`) of hpeldsp: `bw` wide, `h` rows.
#[allow(clippy::too_many_arguments)]
pub fn put_hpel(
    dst: &mut [u8],
    doff: usize,
    dstride: usize,
    src: &SrcBlock,
    bw: usize,
    h: usize,
    dxy: usize,
    no_rnd: bool,
) {
    let s = src.data;
    let ss = src.stride;
    let so = src.off;
    let r1 = if no_rnd { 0 } else { 1 };
    let r2 = if no_rnd { 1 } else { 2 };
    for y in 0..h {
        let d = &mut dst[doff + y * dstride..doff + y * dstride + bw];
        let a = &s[so + y * ss..];
        match dxy {
            0 => d.copy_from_slice(&a[..bw]),
            1 => {
                for x in 0..bw {
                    d[x] = ((a[x] as u32 + a[x + 1] as u32 + r1) >> 1) as u8;
                }
            }
            2 => {
                let b = &s[so + (y + 1) * ss..];
                for x in 0..bw {
                    d[x] = ((a[x] as u32 + b[x] as u32 + r1) >> 1) as u8;
                }
            }
            _ => {
                let b = &s[so + (y + 1) * ss..];
                for x in 0..bw {
                    d[x] = ((a[x] as u32 + a[x + 1] as u32 + b[x] as u32 + b[x + 1] as u32 + r2) >> 2) as u8;
                }
            }
        }
    }
}

pub const H263_LOOP_FILTER_STRENGTH: [u8; 32] = [
    0, 1, 1, 2, 2, 3, 3, 4, 4, 4, 5, 5, 6, 6, 7, 7, 7, 8, 8, 8, 9, 9, 9, 10, 10, 10, 11, 11, 11, 12, 12, 12,
];

#[inline]
fn h263_filter_pixels(p0: i32, p1: i32, p2: i32, p3: i32, strength: i32) -> (u8, u8, u8, u8) {
    let d = (p0 - p3 + 4 * (p2 - p1)) / 8;
    let d1 = if d < -2 * strength {
        0
    } else if d < -strength {
        -2 * strength - d
    } else if d < strength {
        d
    } else if d < 2 * strength {
        2 * strength - d
    } else {
        0
    };
    let mut q1 = p1 + d1;
    let mut q2 = p2 - d1;
    if q1 & 256 != 0 {
        q1 = !(q1 >> 31);
    }
    if q2 & 256 != 0 {
        q2 = !(q2 >> 31);
    }
    let ad1 = d1.abs() >> 1;
    let d2 = ((p0 - p3) / 4).clamp(-ad1, ad1);
    ((p0 - d2) as u8, q1 as u8, q2 as u8, (p3 + d2) as u8)
}

/// `h263_h_loop_filter_c`: filters the vertical edge left of `off`.
pub fn h263_h_loop_filter(src: &mut [u8], off: usize, stride: usize, qscale: usize) {
    let strength = H263_LOOP_FILTER_STRENGTH[qscale & 31] as i32;
    for y in 0..8 {
        let o = off + y * stride;
        let (a, b, c, d) =
            h263_filter_pixels(src[o - 2] as i32, src[o - 1] as i32, src[o] as i32, src[o + 1] as i32, strength);
        src[o - 2] = a;
        src[o - 1] = b;
        src[o] = c;
        src[o + 1] = d;
    }
}

/// `h263_v_loop_filter_c`: filters the horizontal edge above `off`.
pub fn h263_v_loop_filter(src: &mut [u8], off: usize, stride: usize, qscale: usize) {
    let strength = H263_LOOP_FILTER_STRENGTH[qscale & 31] as i32;
    for x in 0..8 {
        let o = off + x;
        let (a, b, c, d) = h263_filter_pixels(
            src[o - 2 * stride] as i32,
            src[o - stride] as i32,
            src[o] as i32,
            src[o + stride] as i32,
            strength,
        );
        src[o - 2 * stride] = a;
        src[o - stride] = b;
        src[o] = c;
        src[o + stride] = d;
    }
}

#[inline]
pub fn mid_pred(a: i32, b: i32, c: i32) -> i32 {
    if a > b {
        if c > b {
            if c > a {
                a
            } else {
                c
            }
        } else {
            b
        }
    } else if b > c {
        if c > a {
            c
        } else {
            a
        }
    } else {
        b
    }
}
