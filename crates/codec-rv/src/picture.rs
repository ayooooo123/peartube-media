//! Picture planes shared by the RealVideo decoders.
//!
//! A [`Plane`] holds a macroblock-aligned picture plus a margin on every
//! side, so neighbour reads around a block at the picture edge (which
//! FFmpeg performs on its frame buffers) stay inside the allocation.
//! Motion compensation reads references through [`Plane::fetch`], FFmpeg's
//! `emulated_edge_mc` clamping (ported from libavcodec/videodsp_template.c,
//! commit 2da55bf; LGPL-2.1-or-later).

use oxideav_core::{Error, Frame, Result, VideoFrame, VideoPlane};

/// Margin, in samples, around each plane.
pub const PAD: usize = 32;

#[derive(Clone)]
pub struct Plane {
    pub data: Vec<u8>,
    pub stride: usize,
    /// Coded (edge) width and height: samples outside are replicated.
    pub width: usize,
    pub height: usize,
}

impl Plane {
    pub fn new(width: usize, height: usize, fill: u8) -> Self {
        let stride = width + 2 * PAD;
        Plane { data: vec![fill; stride * (height + 2 * PAD)], stride, width, height }
    }

    /// Index of sample (x, y).
    #[inline]
    pub fn idx(&self, x: usize, y: usize) -> usize {
        (y + PAD) * self.stride + x + PAD
    }

    /// Copies the `w`x`h` block whose top-left sample is (`x0`, `y0`) into
    /// `out` (row pitch `w`), replicating the plane's edge samples for
    /// positions outside it.
    pub fn fetch(&self, x0: i32, y0: i32, w: usize, h: usize, out: &mut [u8]) {
        let max_x = self.width as i64 - 1;
        let max_y = self.height as i64 - 1;
        for j in 0..h {
            let sy = (y0 as i64 + j as i64).clamp(0, max_y.max(0)) as usize;
            let row = self.idx(0, sy);
            let o = &mut out[j * w..j * w + w];
            let x0 = x0 as i64;
            if x0 >= 0 && x0 + w as i64 - 1 <= max_x {
                let s = row + x0 as usize;
                o.copy_from_slice(&self.data[s..s + w]);
            } else {
                for (i, v) in o.iter_mut().enumerate() {
                    let sx = (x0 + i as i64).clamp(0, max_x.max(0)) as usize;
                    *v = self.data[row + sx];
                }
            }
        }
    }
}

/// The decoders' picture size limits: at most 16384 per side and a 4:2:0
/// picture of at most 256 MiB.
pub fn check_dimensions(width: usize, height: usize) -> Result<()> {
    if width == 0 || height == 0 || width > 16384 || height > 16384 || width * height * 3 / 2 > 256 << 20 {
        return Err(Error::invalid(format!("unsupported picture size {width}x{height}")));
    }
    Ok(())
}

/// Packs the visible `width`x`height` area of three 4:2:0 planes into a
/// `Yuv420P` frame.
pub fn yuv420_frame(planes: &[Plane; 3], width: usize, height: usize, pts: Option<i64>) -> Frame {
    let cw = width.div_ceil(2);
    let ch = height.div_ceil(2);
    let crop = |p: &Plane, w: usize, h: usize| {
        let mut data = Vec::with_capacity(w * h);
        for y in 0..h {
            let s = p.idx(0, y);
            data.extend_from_slice(&p.data[s..s + w]);
        }
        VideoPlane { stride: w, data }
    };
    Frame::Video(VideoFrame {
        pts,
        planes: vec![crop(&planes[0], width, height), crop(&planes[1], cw, ch), crop(&planes[2], cw, ch)],
    })
}
