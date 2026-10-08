// Ported from FFmpeg (commit 2da55bf): libavcodec/svq1dec.c, with the
// put_pixels functions of libavcodec/hpeldsp.c (pel_template.c) and
// av_clip of libavutil/common.h.
// License: LGPL-2.1-or-later

//! svq1dec.c: the frame header (with its byte swap and embedded
//! message), the breadth-first multistage vector quantiser for intra and
//! inter vectors, median motion prediction with half-pel motion
//! compensation, and the one-picture reference. Pixel arithmetic is per
//! sample: FFmpeg's packed four-sample arithmetic clips each sample to
//! 0..=255 exactly as written here.

use std::borrow::Cow;
use std::sync::LazyLock;

use oxideav_core::{Error, Result};

use crate::bits::{mid_pred, sign_extend, Bits, Vlc};
use crate::tables;

const BLOCK_SKIP: i32 = 0;
const BLOCK_INTER: i32 = 1;
const BLOCK_INTER_4V: i32 = 2;
const BLOCK_INTRA: i32 = 3;

struct Vlcs {
    block_type: Vlc,
    motion: Vlc,
    intra_multistage: [Vlc; 6],
    inter_multistage: [Vlc; 6],
    intra_mean: Vlc,
    inter_mean: Vlc,
}

/// svq1_static_init.
static VLCS: LazyLock<Vlcs> = LazyLock::new(|| Vlcs {
    block_type: Vlc::new(&tables::BLOCK_TYPE_VLC),
    motion: Vlc::new(&tables::MVTAB),
    intra_multistage: std::array::from_fn(|l| Vlc::new(&tables::INTRA_MULTISTAGE_VLC[l])),
    inter_multistage: std::array::from_fn(|l| Vlc::new(&tables::INTER_MULTISTAGE_VLC[l])),
    intra_mean: Vlc::new(&tables::INTRA_MEAN_VLC),
    inter_mean: Vlc::new(&tables::INTER_MEAN_VLC),
});

fn invalid(what: &str) -> Error {
    Error::invalid(format!("svq1: {what}"))
}

/// av_clip.
fn av_clip(a: i32, lo: i32, hi: i32) -> i32 {
    if a < lo {
        lo
    } else if a > hi {
        hi
    } else {
        a
    }
}

fn align16(v: usize) -> usize {
    (v + 15) & !15
}

/// svq1_pmv.
#[derive(Clone, Copy, Default)]
struct Pmv {
    x: i32,
    y: i32,
}

/// A decoded picture: three planes over the macroblock-aligned area the
/// decoder writes (and at least the area the picture shows).
#[derive(Clone)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    /// `(stride, samples)` of Y, U and V.
    pub planes: [(usize, Vec<u8>); 3],
}

impl Picture {
    fn new(width: usize, height: usize) -> Self {
        let plane = |p: usize| {
            let (aw, ah) = plane_area(p, width, height);
            let (sw, sh) = if p == 0 { (width, height) } else { (width.div_ceil(4), height.div_ceil(4)) };
            let stride = aw.max(sw);
            (stride, vec![0u8; stride * ah.max(sh)])
        };
        Self { width, height, planes: [plane(0), plane(1), plane(2)] }
    }
}

/// The area svq1_decode_frame decodes in plane `p`: FFALIGN(width, 16)
/// for luma, FFALIGN(width / 4, 16) for chroma (likewise the height).
fn plane_area(p: usize, width: usize, height: usize) -> (usize, usize) {
    if p == 0 { (align16(width), align16(height)) } else { (align16(width / 4), align16(height / 4)) }
}

/// The SVQ1 decoder state (SVQ1Context).
pub struct Svq1Decoder {
    width: usize,
    height: usize,
    last_tempref: u32,
    no_extradata: bool,
    prev: Option<Picture>,
    nonref_out: Option<Picture>,
    pmv: Vec<Pmv>,
    /// The size FFmpeg reports (avctx dimensions): the container's until
    /// a frame header sets it.
    pub dims: (u32, u32),
}

impl Svq1Decoder {
    /// svq1_decode_init.
    pub fn new(extradata: &[u8], width: u32, height: u32) -> Self {
        Self {
            width: (width as usize + 3) & !3,
            height: (height as usize + 3) & !3,
            last_tempref: 0xFF,
            no_extradata: extradata.is_empty(),
            prev: None,
            nonref_out: None,
            pmv: Vec::new(),
            dims: (width, height),
        }
    }

    /// svq1_flush: the reference picture is dropped.
    pub fn flush(&mut self) {
        self.prev = None;
    }

    /// svq1_decode_frame: the picture `data` codes.
    pub fn decode(&mut self, data: &[u8]) -> Result<&Picture> {
        let mut gb = Bits::new(data);
        let frame_code = gb.get(22);
        if frame_code & !0x70 != 0 || frame_code & 0x60 == 0 {
            return Err(invalid("frame code"));
        }
        // "swap some header bytes (why?)": bytes 4..20 as four words, each
        // with its 16-bit halves swapped, XORed with words 7..4.
        let buf: Cow<[u8]> = if frame_code != 0x20 {
            if data.len() < 9 * 4 {
                return Err(invalid("input packet too small"));
            }
            let mut s = data.to_vec();
            for i in 0..4 {
                let (o, c) = (4 + 4 * i, 4 + 4 * (7 - i));
                let w = [s[o], s[o + 1], s[o + 2], s[o + 3]];
                for k in 0..4 {
                    s[o + k] = w[(k + 2) % 4] ^ s[c + k];
                }
            }
            Cow::Owned(s)
        } else {
            Cow::Borrowed(data)
        };
        let mut gb = Bits::new(&buf);
        gb.skip(22);

        let (intra, nonref, buggy) = self.decode_frame_header(&mut gb, frame_code)?;
        self.dims = (self.width as u32, self.height as u32);
        if gb.left() < (align16(self.width) * align16(self.height) / 256) as i64 {
            return Err(invalid("packet too small for the picture"));
        }

        let mut cur = Picture::new(self.width, self.height);
        for p in 0..3 {
            let (width, height) = plane_area(p, self.width, self.height);
            let pitch = cur.planes[p].0;
            let plane = &mut cur.planes[p].1;
            if intra {
                for y in (0..height).step_by(16) {
                    for x in (0..width).step_by(16) {
                        decode_block_intra(&mut gb, plane, y * pitch + x, pitch)?;
                    }
                }
            } else {
                let previous = match &self.prev {
                    Some(prev) if prev.width == self.width && prev.height == self.height => &prev.planes[p].1,
                    _ => return Err(invalid("missing reference frame")),
                };
                self.pmv.clear();
                self.pmv.resize(width / 8 + 3, Pmv::default());
                for y in (0..height).step_by(16) {
                    for x in (0..width).step_by(16) {
                        let at = Block { x, y, width, height, pitch };
                        decode_delta_block(&mut gb, plane, previous, &mut self.pmv, at, buggy)?;
                    }
                    self.pmv[0] = Pmv::default();
                }
            }
        }

        let slot = if nonref { &mut self.nonref_out } else { &mut self.prev };
        Ok(slot.insert(cur))
    }

    /// svq1_decode_frame_header: (intra, non-reference, buggy).
    fn decode_frame_header(&mut self, gb: &mut Bits, frame_code: u32) -> Result<(bool, bool, bool)> {
        let tempref = gb.get(8);
        let buggy = tempref == 0 && self.last_tempref == 0 && self.no_extradata;
        self.last_tempref = tempref;

        let (intra, nonref) = match gb.get(2) {
            0 => (true, false),
            1 => (false, false),
            2 => (false, true),
            _ => return Err(invalid("invalid frame type")),
        };
        let (mut width, mut height) = (self.width, self.height);
        if intra {
            // The packet checksum is only logged.
            if frame_code == 0x50 || frame_code == 0x60 {
                gb.skip(16);
            }
            // svq1_parse_string: a length byte, then that many bytes.
            if (frame_code ^ 0x10) >= 0x50 {
                let len = gb.get(8);
                gb.skip(8 * len as usize);
            }
            gb.skip(2);
            gb.skip(2);
            gb.skip(1);
            let frame_size_code = gb.get(3) as usize;
            if frame_size_code == 7 {
                width = gb.get(12) as usize;
                height = gb.get(12) as usize;
                if width == 0 || height == 0 {
                    return Err(invalid("zero picture size"));
                }
            } else {
                let (w, h) = tables::FRAME_SIZE[frame_size_code];
                (width, height) = (w as usize, h as usize);
            }
        }
        if gb.bit() == 1 {
            gb.skip(1); // use packet checksum if (1)
            gb.skip(1); // component checksums after image data if (1)
            if gb.get(2) != 0 {
                return Err(invalid("frame header"));
            }
        }
        if gb.bit() == 1 {
            gb.skip(1);
            gb.skip(4);
            gb.skip(1);
            gb.skip(2);
            if !gb.skip_1stop_8data_bits() {
                return Err(invalid("frame header"));
            }
        }
        if gb.left() <= 0 {
            return Err(invalid("frame header"));
        }
        self.width = width;
        self.height = height;
        Ok((intra, nonref, buggy))
    }
}

/// A 16x16 block of a plane: its position, the plane's decoded area and
/// stride.
#[derive(Clone, Copy)]
struct Block {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    pitch: usize,
}

/// SVQ1_PROCESS_VECTOR for the vector `list[*i]` at `*level`: reads the
/// split flags, queues the halves of each split vector and moves `*i` to
/// the next vector to code.
fn process_vector(gb: &mut Bits, list: &mut [usize; 63], i: &mut usize, m: &mut usize, n: &mut usize, level: &mut u32, pitch: usize) {
    while *level > 0 {
        if *i == *m {
            *m = *n;
            *level -= 1;
            if *level == 0 {
                break;
            }
        }
        if gb.bit() == 0 {
            break;
        }
        let half = (if *level & 1 == 1 { pitch } else { 1 }) << ((*level >> 1) + 1);
        list[*n] = list[*i];
        list[*n + 1] = list[*i] + half;
        *n += 2;
        *i += 1;
    }
}

/// SVQ1_CALC_CODEBOOK_ENTRIES: each stage's vector offset in the level's
/// codebook.
fn codebook_entries(gb: &mut Bits, stages: usize, level: u32) -> [usize; 6] {
    let size = 8 << level;
    let cache = if stages > 0 { gb.get(4 * stages as u32) } else { 0 };
    let mut entries = [0; 6];
    for (j, e) in entries.iter_mut().enumerate().take(stages) {
        *e = ((cache >> (4 * (stages - j - 1))) & 0xF) as usize * size + 16 * j * size;
    }
    entries
}

/// svq1_decode_block_intra.
fn decode_block_intra(gb: &mut Bits, plane: &mut [u8], pixels: usize, pitch: usize) -> Result<()> {
    let v = &*VLCS;
    let mut list = [0usize; 63];
    list[0] = pixels;
    let (mut i, mut m, mut n, mut level) = (0usize, 1usize, 1usize, 5u32);
    while i < n {
        process_vector(gb, &mut list, &mut i, &mut m, &mut n, &mut level, pitch);
        let dst = list[i];
        i += 1;
        let width = 1usize << ((4 + level) / 2);
        let height = 1usize << ((3 + level) / 2);

        // The number of stages: -1 skips the vector (zero here), 0 is the
        // mean alone.
        let stages = v.intra_multistage[level as usize].read(gb) - 1;
        if stages == -1 {
            for y in 0..height {
                plane[dst + y * pitch..][..width].fill(0);
            }
            continue;
        }
        if stages < 0 || (stages > 0 && level >= 4) {
            return Err(invalid("invalid intra vector"));
        }
        let stages = stages as usize;
        let mean = v.intra_mean.read(gb);
        if stages == 0 {
            for y in 0..height {
                plane[dst + y * pitch..][..width].fill(mean as u8);
            }
        } else {
            let entries = codebook_entries(gb, stages, level);
            let book = tables::INTRA_CODEBOOKS[level as usize];
            for y in 0..height {
                for x in 0..width {
                    let k = y * width + x;
                    let sum = entries[..stages].iter().fold(mean, |s, &e| s + i32::from(book[e + k]));
                    plane[dst + y * pitch + x] = sum.clamp(0, 255) as u8;
                }
            }
        }
    }
    Ok(())
}

/// svq1_decode_block_non_intra: the residual over the prediction.
fn decode_block_non_intra(gb: &mut Bits, plane: &mut [u8], pixels: usize, pitch: usize, buggy: bool) -> Result<()> {
    let v = &*VLCS;
    let mut list = [0usize; 63];
    list[0] = pixels;
    let (mut i, mut m, mut n, mut level) = (0usize, 1usize, 1usize, 5u32);
    while i < n {
        process_vector(gb, &mut list, &mut i, &mut m, &mut n, &mut level, pitch);
        let dst = list[i];
        i += 1;
        let width = 1usize << ((4 + level) / 2);
        let height = 1usize << ((3 + level) / 2);

        let stages = v.inter_multistage[level as usize].read(gb) - 1;
        if stages == -1 {
            continue;
        }
        if stages < 0 || (stages > 0 && level >= 4) {
            return Err(invalid("invalid inter vector"));
        }
        let stages = stages as usize;
        let mut mean = v.inter_mean.read(gb) - 256;
        if buggy {
            if mean == -128 {
                mean = 128;
            } else if mean == 128 {
                mean = -128;
            }
        }
        let entries = codebook_entries(gb, stages, level);
        let book = tables::INTER_CODEBOOKS.get(level as usize).copied().unwrap_or(&[]);
        for y in 0..height {
            for x in 0..width {
                let k = y * width + x;
                let at = dst + y * pitch + x;
                let base = i32::from(plane[at]) + mean;
                let sum = entries[..stages].iter().fold(base, |s, &e| s + i32::from(book[e + k]));
                plane[at] = sum.clamp(0, 255) as u8;
            }
        }
    }
    Ok(())
}

/// svq1_decode_motion_vector: the vector `pmv`'s median predicts.
fn decode_motion_vector(gb: &mut Bits, pmv: [Pmv; 3]) -> Result<Pmv> {
    let mut mv = Pmv::default();
    for i in 0..2 {
        let mut diff = VLCS.motion.read(gb);
        if diff < 0 {
            return Err(invalid("motion vector"));
        }
        if diff != 0 && gb.bit() == 1 {
            diff = -diff;
        }
        if i == 1 {
            mv.y = sign_extend(diff + mid_pred(pmv[0].y, pmv[1].y, pmv[2].y), 6);
        } else {
            mv.x = sign_extend(diff + mid_pred(pmv[0].x, pmv[1].x, pmv[2].x), 6);
        }
    }
    Ok(mv)
}

/// hpeldsp put_pixels_tab[size][dxy]: a `size`×`size` block from `src`,
/// at half-sample positions by rounded averages.
fn put_pixels(dst: &mut [u8], d: usize, src: &[u8], s: usize, pitch: usize, size: usize, dxy: i32) {
    for y in 0..size {
        for x in 0..size {
            let at = s + y * pitch + x;
            let a = u32::from(src[at]);
            let v = match dxy {
                0 => a,
                1 => (a + u32::from(src[at + 1]) + 1) >> 1,
                2 => (a + u32::from(src[at + pitch]) + 1) >> 1,
                _ => (a + u32::from(src[at + 1]) + u32::from(src[at + pitch]) + u32::from(src[at + pitch + 1]) + 2) >> 2,
            };
            dst[d + y * pitch + x] = v as u8;
        }
    }
}

/// svq1_skip_block.
fn skip_block(cur: &mut [u8], prev: &[u8], at: Block) {
    let o = at.y * at.pitch + at.x;
    for y in 0..16 {
        let r = o + y * at.pitch;
        cur[r..r + 16].copy_from_slice(&prev[r..r + 16]);
    }
}

/// svq1_motion_inter_block.
fn motion_inter_block(gb: &mut Bits, cur: &mut [u8], prev: &[u8], motion: &mut [Pmv], at: Block) -> Result<()> {
    let c = at.x / 8;
    let p0 = motion[0];
    let (p1, p2) = if at.y == 0 { (p0, p0) } else { (motion[c + 2], motion[c + 4]) };
    let mut mv = decode_motion_vector(gb, [p0, p1, p2])?;
    motion[0] = mv;
    motion[c + 2] = mv;
    motion[c + 3] = mv;

    let (x, y, w, h) = (at.x as i32, at.y as i32, at.width as i32, at.height as i32);
    mv.x = av_clip(mv.x, -2 * x, 2 * (w - x - 16));
    mv.y = av_clip(mv.y, -2 * y, 2 * (h - y - 16));
    let src = ((y + (mv.y >> 1)) * at.pitch as i32 + x + (mv.x >> 1)) as usize;
    put_pixels(cur, at.y * at.pitch + at.x, prev, src, at.pitch, 16, (mv.y & 1) << 1 | (mv.x & 1));
    Ok(())
}

/// svq1_motion_inter_4v_block: four 8x8 vectors, each predicted from its
/// decoded neighbours.
fn motion_inter_4v_block(gb: &mut Bits, cur: &mut [u8], prev: &[u8], motion: &mut [Pmv], at: Block) -> Result<()> {
    let c = at.x / 8;
    // (0): left, above, above right.
    let p0 = motion[0];
    let (p1, p2) = if at.y == 0 { (p0, p0) } else { (motion[c + 2], motion[c + 4]) };
    let mv = decode_motion_vector(gb, [p0, p1, p2])?;
    // (1): vector 0, above, above right.
    let (q1, q2) = if at.y == 0 { (mv, mv) } else { (motion[c + 3], motion[c + 4]) };
    motion[0] = decode_motion_vector(gb, [mv, q1, q2])?;
    // (2): vector 0, vector 1, the left block's lower right.
    motion[c + 2] = decode_motion_vector(gb, [mv, motion[0], motion[c + 1]])?;
    // (3): vector 0, vector 1, vector 2.
    motion[c + 3] = decode_motion_vector(gb, [mv, motion[0], motion[c + 2]])?;

    let vectors = [mv, motion[0], motion[c + 2], motion[c + 3]];
    let (x, y, w, h) = (at.x as i32, at.y as i32, at.width as i32, at.height as i32);
    let mut dst = at.y * at.pitch + at.x;
    for (i, v) in vectors.iter().enumerate() {
        let mvx = av_clip(v.x + (i & 1) as i32 * 16, -2 * x, 2 * (w - x - 8));
        let mvy = av_clip(v.y + (i >> 1) as i32 * 16, -2 * y, 2 * (h - y - 8));
        let src = ((y + (mvy >> 1)) * at.pitch as i32 + x + (mvx >> 1)) as usize;
        put_pixels(cur, dst, prev, src, at.pitch, 8, (mvy & 1) << 1 | (mvx & 1));
        dst = if i & 1 == 1 { dst + 8 * at.pitch - 8 } else { dst + 8 };
    }
    Ok(())
}

/// svq1_decode_delta_block.
fn decode_delta_block(gb: &mut Bits, cur: &mut [u8], prev: &[u8], motion: &mut [Pmv], at: Block, buggy: bool) -> Result<()> {
    let block_type = VLCS.block_type.read(gb);
    let c = at.x / 8;
    if block_type == BLOCK_SKIP || block_type == BLOCK_INTRA {
        motion[0] = Pmv::default();
        motion[c + 2] = Pmv::default();
        motion[c + 3] = Pmv::default();
    }
    let pixels = at.y * at.pitch + at.x;
    match block_type {
        BLOCK_SKIP => skip_block(cur, prev, at),
        BLOCK_INTER => {
            motion_inter_block(gb, cur, prev, motion, at)?;
            decode_block_non_intra(gb, cur, pixels, at.pitch, buggy)?;
        }
        BLOCK_INTER_4V => {
            motion_inter_4v_block(gb, cur, prev, motion, at)?;
            decode_block_non_intra(gb, cur, pixels, at.pitch, buggy)?;
        }
        BLOCK_INTRA => decode_block_intra(gb, cur, pixels, at.pitch)?,
        // A code the table lacks: FFmpeg leaves the block as it is.
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_is_a_prefix_code() {
        let v = &*VLCS;
        // Each VLC builds; the block type code is complete.
        let mut gb = Bits::new(&[0b1010_0100, 0b0000_0000]);
        let read: Vec<i32> = (0..4).map(|_| v.block_type.read(&mut gb)).collect();
        assert_eq!(read, [BLOCK_SKIP, BLOCK_INTER, BLOCK_INTER_4V, BLOCK_INTRA]);
    }
}
