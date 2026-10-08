// Copyright (C) 2016 Vabishchevich Nikolay <vabnick@gmail.com>
//
// This file is part of libass.
//
// Permission to use, copy, modify, and distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
// ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
// OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//
// Derived from libass 0.17.5 (commit 4a05d81): libass/ass_outline.c and
// libass/ass_outline.h. Changed for PearTube on 2026-10-08: ported to safe
// Rust; glyph outlines come from ttf-parser instead of FreeType.

//! Outlines: contours of lines and quadratic/cubic splines in integer
//! coordinates, their transforms, and the stroker that builds borders.

use crate::utils::lrint;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vector {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DVector {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x_min: i32,
    pub y_min: i32,
    pub x_max: i32,
    pub y_max: i32,
}

impl Rect {
    /// `rectangle_reset`: empty, ready for updates.
    pub const fn reset() -> Rect {
        Rect { x_min: i32::MAX, y_min: i32::MAX, x_max: i32::MIN, y_max: i32::MIN }
    }

    /// `rectangle_update`.
    pub fn update(&mut self, x_min: i32, y_min: i32, x_max: i32, y_max: i32) {
        self.x_min = self.x_min.min(x_min);
        self.y_min = self.y_min.min(y_min);
        self.x_max = self.x_max.max(x_max);
        self.y_max = self.y_max.max(y_max);
    }
}

impl Default for Rect {
    fn default() -> Rect {
        Rect { x_min: 0, y_min: 0, x_max: 0, y_max: 0 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DRect {
    pub x_min: f64,
    pub y_min: f64,
    pub x_max: f64,
    pub y_max: f64,
}

pub const OUTLINE_LINE_SEGMENT: u8 = 1;
pub const OUTLINE_QUADRATIC_SPLINE: u8 = 2;
pub const OUTLINE_CUBIC_SPLINE: u8 = 3;
pub const OUTLINE_COUNT_MASK: u8 = 3;
pub const OUTLINE_CONTOUR_END: u8 = 4;

/// Outline point coordinates stay within ±`OUTLINE_MAX`.
pub const OUTLINE_MAX: i32 = (1 << 28) - 1;

/// An outline: points and segments. Each segment owns as many points as
/// its order and ends on the next segment's first point (a contour's last
/// segment on its first).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outline {
    pub points: Vec<Vector>,
    pub segments: Vec<u8>,
}

impl Outline {
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// `ass_outline_add_rect`.
    pub fn add_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.points.extend([Vector { x: x0, y: y0 }, Vector { x: x1, y: y0 }, Vector { x: x1, y: y1 }, Vector { x: x0, y: y1 }]);
        self.segments.extend([OUTLINE_LINE_SEGMENT, OUTLINE_LINE_SEGMENT, OUTLINE_LINE_SEGMENT, OUTLINE_LINE_SEGMENT | OUTLINE_CONTOUR_END]);
    }

    /// `ass_outline_add_point`: false when the point is out of range.
    pub fn add_point(&mut self, pt: Vector, segment: u8) -> bool {
        if pt.x.unsigned_abs() > OUTLINE_MAX as u32 || pt.y.unsigned_abs() > OUTLINE_MAX as u32 {
            return false;
        }
        self.points.push(pt);
        if segment != 0 {
            self.segments.push(segment);
        }
        true
    }

    pub fn add_segment(&mut self, segment: u8) {
        self.segments.push(segment);
    }

    /// `ass_outline_close_contour`.
    pub fn close_contour(&mut self) {
        if let Some(last) = self.segments.last_mut() {
            *last |= OUTLINE_CONTOUR_END;
        }
    }

    /// `ass_outline_rotate_90`: rotated a quarter and moved by `offs`.
    pub fn rotate_90(&mut self, offs: Vector) -> bool {
        for p in &mut self.points {
            let pt = Vector { x: offs.x.wrapping_add(p.y), y: offs.y.wrapping_sub(p.x) };
            if pt.x.unsigned_abs() > OUTLINE_MAX as u32 || pt.y.unsigned_abs() > OUTLINE_MAX as u32 {
                return false;
            }
            *p = pt;
        }
        true
    }

    /// `ass_outline_scale_pow2`: scaled by `2^ord_x`, `2^ord_y`.
    pub fn scale_pow2(source: &Outline, mut ord_x: i32, mut ord_y: i32) -> Option<Outline> {
        if source.points.is_empty() {
            return Some(Outline::default());
        }
        let mut lim_x = OUTLINE_MAX;
        if ord_x > 0 {
            lim_x = if ord_x < 32 { lim_x >> ord_x } else { 0 };
        } else {
            ord_x = ord_x.max(-32);
        }
        let mut lim_y = OUTLINE_MAX;
        if ord_y > 0 {
            lim_y = if ord_y < 32 { lim_y >> ord_y } else { 0 };
        } else {
            ord_y = ord_y.max(-32);
        }
        if lim_x == 0 || lim_y == 0 {
            return None;
        }
        let (sx, sy) = (ord_x + 32, ord_y + 32);
        let mut points = Vec::with_capacity(source.points.len());
        for p in &source.points {
            if p.x.unsigned_abs() > lim_x as u32 || p.y.unsigned_abs() > lim_y as u32 {
                return None;
            }
            points.push(Vector { x: ((i64::from(p.x) * (1i64 << sx)) >> 32) as i32, y: ((i64::from(p.y) * (1i64 << sy)) >> 32) as i32 });
        }
        Some(Outline { points, segments: source.segments.clone() })
    }

    /// `ass_outline_transform_2d`: by a 2x3 matrix.
    pub fn transform_2d(source: &Outline, m: &[[f64; 3]; 2]) -> Option<Outline> {
        let mut points = Vec::with_capacity(source.points.len());
        for p in &source.points {
            let (x, y) = (f64::from(p.x), f64::from(p.y));
            let v = [m[0][0] * x + m[0][1] * y + m[0][2], m[1][0] * x + m[1][1] * y + m[1][2]];
            if !(v[0].abs() < f64::from(OUTLINE_MAX) && v[1].abs() < f64::from(OUTLINE_MAX)) {
                return None;
            }
            points.push(Vector { x: lrint(v[0]) as i32, y: lrint(v[1]) as i32 });
        }
        Some(Outline { points, segments: source.segments.clone() })
    }

    /// `ass_outline_transform_3d`: by a 3x3 perspective matrix.
    pub fn transform_3d(source: &Outline, m: &[[f64; 3]; 3]) -> Option<Outline> {
        let mut points = Vec::with_capacity(source.points.len());
        for p in &source.points {
            let (x, y) = (f64::from(p.x), f64::from(p.y));
            let mut v = [0.0; 3];
            for k in 0..3 {
                v[k] = m[k][0] * x + m[k][1] * y + m[k][2];
            }
            let w = 1.0 / v[2].max(0.1);
            v[0] *= w;
            v[1] *= w;
            if !(v[0].abs() < f64::from(OUTLINE_MAX) && v[1].abs() < f64::from(OUTLINE_MAX)) {
                return None;
            }
            points.push(Vector { x: lrint(v[0]) as i32, y: lrint(v[1]) as i32 });
        }
        Some(Outline { points, segments: source.segments.clone() })
    }

    /// `ass_outline_update_min_transformed_x`.
    pub fn update_min_transformed_x(&self, m: &[[f64; 3]; 3], min_x: &mut i32) {
        for p in &self.points {
            let (x, y) = (f64::from(p.x), f64::from(p.y));
            let z = m[2][0] * x + m[2][1] * y + m[2][2];
            let tx = (m[0][0] * x + m[0][1] * y + m[0][2]) / z.max(0.1);
            if tx.is_nan() {
                continue;
            }
            let ix = lrint(tx.clamp(-f64::from(OUTLINE_MAX), f64::from(OUTLINE_MAX))) as i32;
            *min_x = (*min_x).min(ix);
        }
    }

    /// `ass_outline_update_cbox`: the box of the control points.
    pub fn update_cbox(&self, cbox: &mut Rect) {
        for p in &self.points {
            cbox.update(p.x, p.y, p.x, p.y);
        }
    }

    /// The segments with their points: `(order, points)` where `points`
    /// holds the segment's start, its control points and its end.
    pub fn for_each_segment(&self, mut f: impl FnMut(&[Vector])) {
        let mut start = 0;
        let mut cur = 0;
        let mut buf = [Vector::default(); 4];
        for &seg in &self.segments {
            let n = usize::from(seg & OUTLINE_COUNT_MASK);
            if n == 0 || cur + n > self.points.len() {
                return;
            }
            buf[..n].copy_from_slice(&self.points[cur..cur + n]);
            cur += n;
            let end = if seg & OUTLINE_CONTOUR_END != 0 {
                let e = start;
                start = cur;
                e
            } else {
                cur
            };
            let Some(&last) = self.points.get(end) else { return };
            buf[n] = last;
            f(&buf[..=n]);
        }
    }
}

/// An outline builder for ttf-parser glyphs: font units scaled by
/// `scale`, y down, contours of fewer than three points left out (as
/// libass skips degenerate two-point contours of broken fonts).
///
/// Builder calls give a contour as `p0, (controls, end)*`. In libass's form
/// each segment owns its start point and controls and ends on the next
/// segment's first point, the last segment on the contour's first: a
/// contour that returns to `p0` drops that repeated end point, one that
/// does not gets a closing line.
pub struct GlyphOutliner {
    pub outline: Outline,
    scale_x: f64,
    scale_y: f64,
    contour_start_point: usize,
    contour_start_segment: usize,
    valid: bool,
}

impl GlyphOutliner {
    pub fn new(scale_x: f64, scale_y: f64) -> GlyphOutliner {
        GlyphOutliner { outline: Outline::default(), scale_x, scale_y, contour_start_point: 0, contour_start_segment: 0, valid: true }
    }

    fn point(&mut self, x: f32, y: f32) -> Vector {
        let (x, y) = (f64::from(x) * self.scale_x, -f64::from(y) * self.scale_y);
        if !(x.abs() <= f64::from(OUTLINE_MAX) && y.abs() <= f64::from(OUTLINE_MAX)) {
            self.valid = false;
            return Vector::default();
        }
        Vector { x: lrint(x) as i32, y: lrint(y) as i32 }
    }

    /// The finished outline, or `None` when a point fell out of range.
    pub fn finish(mut self) -> Option<Outline> {
        self.close_open_contour();
        self.valid.then_some(self.outline)
    }

    fn close_open_contour(&mut self) {
        let start = self.contour_start_point;
        let n = self.outline.points.len();
        if n > start + 1 {
            if self.outline.points[n - 1] == self.outline.points[start] {
                self.outline.points.pop();
            } else {
                self.outline.segments.push(OUTLINE_LINE_SEGMENT);
            }
        }
        let points = self.outline.points.len() - start;
        if points < 3 || self.outline.segments.len() == self.contour_start_segment {
            self.outline.points.truncate(start);
            self.outline.segments.truncate(self.contour_start_segment);
        } else {
            self.outline.close_contour();
        }
        self.contour_start_point = self.outline.points.len();
        self.contour_start_segment = self.outline.segments.len();
    }
}

impl ttf_parser::OutlineBuilder for GlyphOutliner {
    fn move_to(&mut self, x: f32, y: f32) {
        if self.outline.points.len() > self.contour_start_point {
            self.close_open_contour();
        }
        let p = self.point(x, y);
        self.outline.points.push(p);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.point(x, y);
        self.outline.segments.push(OUTLINE_LINE_SEGMENT);
        self.outline.points.push(p);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (c, p) = (self.point(x1, y1), self.point(x, y));
        self.outline.segments.push(OUTLINE_QUADRATIC_SPLINE);
        self.outline.points.extend([c, p]);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (c1, c2, p) = (self.point(x1, y1), self.point(x2, y2), self.point(x, y));
        self.outline.segments.push(OUTLINE_CUBIC_SPLINE);
        self.outline.points.extend([c1, c2, p]);
    }

    fn close(&mut self) {
        self.close_open_contour();
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Normal {
    v: DVector,
    len: f64,
}

fn vec_dot(a: DVector, b: DVector) -> f64 {
    a.x * b.x + a.y * b.y
}

fn vec_crs(a: DVector, b: DVector) -> f64 {
    a.x * b.y - a.y * b.x
}

fn vec_len(v: DVector) -> f64 {
    (v.x * v.x + v.y * v.y).sqrt()
}

const ZERO: DVector = DVector { x: 0.0, y: 0.0 };

struct Stroker<'a> {
    result: [&'a mut Outline; 2],
    contour_first: [usize; 2],
    xbord: f64,
    ybord: f64,
    xscale: f64,
    yscale: f64,
    eps: i32,
    contour_start: bool,
    first_skip: i32,
    last_skip: i32,
    first_normal: DVector,
    last_normal: DVector,
    first_point: Vector,
    last_point: Vector,
    merge_cos: f64,
    split_cos: f64,
    min_len: f64,
    err_q: f64,
    err_c: f64,
    err_a: f64,
}

const FLAG_INTERSECTION: i32 = 1;
const FLAG_ZERO_0: i32 = 2;
const FLAG_ZERO_1: i32 = 4;
const FLAG_CLIP_0: i32 = 8;
const FLAG_CLIP_1: i32 = 16;
const FLAG_DIR_2: i32 = 32;
const FLAG_COUNT: i32 = 6;
const MASK_INTERSECTION: i32 = FLAG_INTERSECTION << FLAG_COUNT;
const MASK_ZERO_0: i32 = FLAG_ZERO_0 << FLAG_COUNT;
const MASK_ZERO_1: i32 = FLAG_ZERO_1 << FLAG_COUNT;
const MASK_CLIP_0: i32 = FLAG_CLIP_0 << FLAG_COUNT;
const MASK_CLIP_1: i32 = FLAG_CLIP_1 << FLAG_COUNT;

impl Stroker<'_> {
    fn emit_point(&mut self, pt: Vector, offs: DVector, segment: u8, dir: i32) -> bool {
        let dx = (self.xbord * offs.x) as i32;
        let dy = (self.ybord * offs.y) as i32;
        if dir & 1 != 0 && !self.result[0].add_point(Vector { x: pt.x.wrapping_add(dx), y: pt.y.wrapping_add(dy) }, segment) {
            return false;
        }
        if dir & 2 != 0 && !self.result[1].add_point(Vector { x: pt.x.wrapping_sub(dx), y: pt.y.wrapping_sub(dy) }, segment) {
            return false;
        }
        true
    }

    fn fix_first_point(&mut self, pt: Vector, offs: DVector, dir: i32) {
        let dx = (self.xbord * offs.x) as i32;
        let dy = (self.ybord * offs.y) as i32;
        if dir & 1 != 0 {
            if let Some(p) = self.result[0].points.get_mut(self.contour_first[0]) {
                *p = Vector { x: pt.x.wrapping_add(dx), y: pt.y.wrapping_add(dy) };
            }
        }
        if dir & 2 != 0 {
            if let Some(p) = self.result[1].points.get_mut(self.contour_first[1]) {
                *p = Vector { x: pt.x.wrapping_sub(dx), y: pt.y.wrapping_sub(dy) };
            }
        }
    }

    fn process_arc(&mut self, pt: Vector, normal0: DVector, normal1: DVector, mul: &[f64], level: usize, dir: i32) -> bool {
        let center = DVector { x: (normal0.x + normal1.x) * mul[level], y: (normal0.y + normal1.y) * mul[level] };
        if level > 0 {
            return self.process_arc(pt, normal0, center, mul, level - 1, dir) && self.process_arc(pt, center, normal1, mul, level - 1, dir);
        }
        self.emit_point(pt, normal0, OUTLINE_QUADRATIC_SPLINE, dir) && self.emit_point(pt, center, 0, dir)
    }

    fn arc_mul(&self, mut c: f64) -> ([f64; 16], usize) {
        const MAX_SUBDIV: usize = 15;
        let mut mul = [0.0; MAX_SUBDIV + 1];
        let mut pos = MAX_SUBDIV;
        while c < self.split_cos && pos > 0 {
            mul[pos] = 0.5f64.sqrt() / (1.0 + c).sqrt();
            c = (1.0 + c) * mul[pos];
            pos -= 1;
        }
        mul[pos] = 1.0 / (1.0 + c);
        (mul, pos)
    }

    fn draw_arc(&mut self, pt: Vector, normal0: DVector, normal1: DVector, mut c: f64, dir: i32) -> bool {
        const MAX_SUBDIV: usize = 15;
        let mut center = ZERO;
        let mut small_angle = true;
        if c < 0.0 {
            let mut mul = if dir & 2 != 0 { -(0.5f64.sqrt()) } else { 0.5f64.sqrt() };
            mul /= (1.0 - c).sqrt();
            center.x = (normal1.y - normal0.y) * mul;
            center.y = (normal0.x - normal1.x) * mul;
            c = (0.5 + 0.5 * c).max(0.0).sqrt();
            small_angle = false;
        }
        let (mul, pos) = self.arc_mul(c);
        let m = &mul[pos..];
        if small_angle {
            self.process_arc(pt, normal0, normal1, m, MAX_SUBDIV - pos, dir)
        } else {
            self.process_arc(pt, normal0, center, m, MAX_SUBDIV - pos, dir) && self.process_arc(pt, center, normal1, m, MAX_SUBDIV - pos, dir)
        }
    }

    fn draw_circle(&mut self, pt: Vector, dir: i32) -> bool {
        const MAX_SUBDIV: usize = 15;
        let (mul, pos) = self.arc_mul(0.0);
        let m = &mul[pos..];
        let n = [DVector { x: 1.0, y: 0.0 }, DVector { x: 0.0, y: 1.0 }, DVector { x: -1.0, y: 0.0 }, DVector { x: 0.0, y: -1.0 }];
        self.process_arc(pt, n[0], n[1], m, MAX_SUBDIV - pos, dir)
            && self.process_arc(pt, n[1], n[2], m, MAX_SUBDIV - pos, dir)
            && self.process_arc(pt, n[2], n[3], m, MAX_SUBDIV - pos, dir)
            && self.process_arc(pt, n[3], n[0], m, MAX_SUBDIV - pos, dir)
    }

    fn start_segment(&mut self, pt: Vector, normal: DVector, mut dir: i32) -> bool {
        if self.contour_start {
            self.contour_start = false;
            self.first_skip = 0;
            self.last_skip = 0;
            self.first_normal = normal;
            self.last_normal = normal;
            self.first_point = pt;
            return true;
        }
        let prev = self.last_normal;
        let c = vec_dot(prev, normal);
        if c > self.merge_cos {
            let mul = 1.0 / (1.0 + c);
            self.last_normal.x = (self.last_normal.x + normal.x) * mul;
            self.last_normal.y = (self.last_normal.y + normal.y) * mul;
            return true;
        }
        self.last_normal = normal;
        // Negative curvature.
        let s = vec_crs(prev, normal);
        let skip_dir = if s < 0.0 { 1 } else { 2 };
        if dir & skip_dir != 0 {
            if !self.emit_point(pt, prev, OUTLINE_LINE_SEGMENT, !self.last_skip & skip_dir) {
                return false;
            }
            if !self.emit_point(pt, ZERO, OUTLINE_LINE_SEGMENT, skip_dir) {
                return false;
            }
        }
        self.last_skip = skip_dir;
        dir &= !skip_dir;
        dir == 0 || self.draw_arc(pt, prev, normal, c, dir)
    }

    fn emit_first_point(&mut self, pt: Vector, segment: u8, dir: i32) -> bool {
        self.last_skip &= !dir;
        let n = self.last_normal;
        self.emit_point(pt, n, segment, dir)
    }

    fn prepare_skip(&mut self, pt: Vector, dir: i32, first: bool) -> bool {
        if first {
            self.first_skip |= dir;
        } else {
            let n = self.last_normal;
            if !self.emit_point(pt, n, OUTLINE_LINE_SEGMENT, !self.last_skip & dir) {
                return false;
            }
        }
        self.last_skip |= dir;
        true
    }

    fn small(&self, dx: i32, dy: i32) -> bool {
        dx > -self.eps && dx < self.eps && dy > -self.eps && dy < self.eps
    }

    fn add_line(&mut self, pt1: Vector, dir: i32) -> bool {
        let dx = pt1.x.wrapping_sub(self.last_point.x);
        let dy = pt1.y.wrapping_sub(self.last_point.y);
        if self.small(dx, dy) {
            return true;
        }
        let deriv = DVector { x: f64::from(dy) * self.yscale, y: -f64::from(dx) * self.xscale };
        let scale = 1.0 / vec_len(deriv);
        let normal = DVector { x: deriv.x * scale, y: deriv.y * scale };
        let last = self.last_point;
        if !self.start_segment(last, normal, dir) {
            return false;
        }
        if !self.emit_first_point(last, OUTLINE_LINE_SEGMENT, dir) {
            return false;
        }
        self.last_normal = normal;
        self.last_point = pt1;
        true
    }

    fn estimate_quadratic_error(&self, c: f64, s: f64, normal: &[Normal], result: &mut DVector) -> bool {
        // Radial error.
        if !((3.0 + c) * (3.0 + c) < self.err_q * (1.0 + c)) {
            return false;
        }
        let mul = 1.0 / (1.0 + c);
        let (l0, l1) = (2.0 * normal[0].len, 2.0 * normal[1].len);
        let (dot0, crs0) = (l0 + normal[1].len * c, (l0 * mul - normal[1].len) * s);
        let (dot1, crs1) = (l1 + normal[0].len * c, (l1 * mul - normal[0].len) * s);
        // Angular error.
        if !(crs0.abs() < self.err_a * dot0 && crs1.abs() < self.err_a * dot1) {
            return false;
        }
        result.x = (normal[0].v.x + normal[1].v.x) * mul;
        result.y = (normal[0].v.y + normal[1].v.y) * mul;
        true
    }

    fn process_quadratic(&mut self, pt: &[Vector], deriv: &[DVector], normal: &[Normal], mut dir: i32, first: bool) -> bool {
        let c = vec_dot(normal[0].v, normal[1].v);
        let s = vec_crs(normal[0].v, normal[1].v);
        let mut check_dir = dir;
        let skip_dir = if s < 0.0 { 1 } else { 2 };
        if dir & skip_dir != 0 {
            let abs_s = s.abs();
            let f0 = normal[0].len * c + normal[1].len;
            let f1 = normal[1].len * c + normal[0].len;
            let g0 = normal[0].len * abs_s;
            let g1 = normal[1].len * abs_s;
            // Self-intersection.
            if f0 < abs_s && f1 < abs_s {
                let d2 = (f0 * normal[1].len + f1 * normal[0].len) / 2.0;
                if d2 < g0 && d2 < g1 {
                    if !self.prepare_skip(pt[0], skip_dir, first) {
                        return false;
                    }
                    if f0 < 0.0 || f1 < 0.0 {
                        if !self.emit_point(pt[0], ZERO, OUTLINE_LINE_SEGMENT, skip_dir) || !self.emit_point(pt[2], ZERO, OUTLINE_LINE_SEGMENT, skip_dir) {
                            return false;
                        }
                    } else {
                        let mul = f0 / abs_s;
                        let offs = DVector { x: normal[0].v.x * mul, y: normal[0].v.y * mul };
                        if !self.emit_point(pt[0], offs, OUTLINE_LINE_SEGMENT, skip_dir) {
                            return false;
                        }
                    }
                    dir &= !skip_dir;
                    if dir == 0 {
                        self.last_normal = normal[1].v;
                        return true;
                    }
                }
                check_dir ^= skip_dir;
            } else if c + g0 < 1.0 && c + g1 < 1.0 {
                check_dir ^= skip_dir;
            }
        }

        let mut result = ZERO;
        if check_dir != 0 && self.estimate_quadratic_error(c, s, normal, &mut result) {
            if !self.emit_first_point(pt[0], OUTLINE_QUADRATIC_SPLINE, check_dir) {
                return false;
            }
            if !self.emit_point(pt[1], result, 0, check_dir) {
                return false;
            }
            dir &= !check_dir;
            if dir == 0 {
                self.last_normal = normal[1].v;
                return true;
            }
        }

        let mut next = [Vector::default(); 5];
        next[1] = Vector { x: pt[0].x.wrapping_add(pt[1].x), y: pt[0].y.wrapping_add(pt[1].y) };
        next[3] = Vector { x: pt[1].x.wrapping_add(pt[2].x), y: pt[1].y.wrapping_add(pt[2].y) };
        next[2] = Vector { x: (next[1].x.wrapping_add(next[3].x).wrapping_add(2)) >> 2, y: (next[1].y.wrapping_add(next[3].y).wrapping_add(2)) >> 2 };
        next[1].x >>= 1;
        next[1].y >>= 1;
        next[3].x >>= 1;
        next[3].y >>= 1;
        next[0] = pt[0];
        next[4] = pt[2];

        let mut next_deriv = [ZERO; 3];
        next_deriv[0] = DVector { x: deriv[0].x / 2.0, y: deriv[0].y / 2.0 };
        next_deriv[2] = DVector { x: deriv[1].x / 2.0, y: deriv[1].y / 2.0 };
        next_deriv[1] = DVector { x: (next_deriv[0].x + next_deriv[2].x) / 2.0, y: (next_deriv[0].y + next_deriv[2].y) / 2.0 };

        let len = vec_len(next_deriv[1]);
        if len < self.min_len {
            // Degenerate case.
            if !self.emit_first_point(next[0], OUTLINE_LINE_SEGMENT, dir) {
                return false;
            }
            if !self.start_segment(next[2], normal[1].v, dir) {
                return false;
            }
            self.last_skip &= !dir;
            return self.emit_point(next[2], normal[1].v, OUTLINE_LINE_SEGMENT, dir);
        }
        let scale = 1.0 / len;
        let next_normal = [
            Normal { v: normal[0].v, len: normal[0].len / 2.0 },
            Normal { v: DVector { x: next_deriv[1].x * scale, y: next_deriv[1].y * scale }, len },
            Normal { v: normal[1].v, len: normal[1].len / 2.0 },
        ];
        self.process_quadratic(&next[0..], &next_deriv[0..], &next_normal[0..], dir, first)
            && self.process_quadratic(&next[2..], &next_deriv[1..], &next_normal[1..], dir, false)
    }

    fn add_quadratic(&mut self, pt1: Vector, pt2: Vector, dir: i32) -> bool {
        let dx0 = pt1.x.wrapping_sub(self.last_point.x);
        let dy0 = pt1.y.wrapping_sub(self.last_point.y);
        if self.small(dx0, dy0) {
            return self.add_line(pt2, dir);
        }
        let dx1 = pt2.x.wrapping_sub(pt1.x);
        let dy1 = pt2.y.wrapping_sub(pt1.y);
        if self.small(dx1, dy1) {
            return self.add_line(pt2, dir);
        }
        let pt = [self.last_point, pt1, pt2];
        self.last_point = pt2;
        let deriv = [
            DVector { x: f64::from(dy0) * self.yscale, y: -f64::from(dx0) * self.xscale },
            DVector { x: f64::from(dy1) * self.yscale, y: -f64::from(dx1) * self.xscale },
        ];
        let (len0, len1) = (vec_len(deriv[0]), vec_len(deriv[1]));
        let (scale0, scale1) = (1.0 / len0, 1.0 / len1);
        let normal = [
            Normal { v: DVector { x: deriv[0].x * scale0, y: deriv[0].y * scale0 }, len: len0 },
            Normal { v: DVector { x: deriv[1].x * scale1, y: deriv[1].y * scale1 }, len: len1 },
        ];
        let first = self.contour_start;
        self.start_segment(pt[0], normal[0].v, dir) && self.process_quadratic(&pt, &deriv, &normal, dir, first)
    }

    #[allow(clippy::too_many_arguments)]
    fn estimate_cubic_error(&self, c: f64, s: f64, dc: &[f64; 2], ds: &[f64; 2], normal: &[Normal], result: &mut [DVector; 2], check_flags: i32, mut dir: i32) -> i32 {
        let t = (ds[0] + ds[1]) / (dc[0] + dc[1]);
        let c1 = 1.0 + c;
        let ss = s * s;
        let ts = t * s;
        let tt = t * t;
        let ttc = tt * c1;
        let ttcc = ttc * c1;

        let w = 0.4;
        let f0 = [10.0 * w * (c - 1.0) + 9.0 * w * tt * c, 2.0 * (c - 1.0) + 3.0 * tt + 2.0 * ts, 2.0 * (c - 1.0) + 3.0 * tt - 2.0 * ts];
        let f1 = [18.0 * w * (ss - ttc * c), 2.0 * ss - 6.0 * ttc - 2.0 * ts * (c + 4.0), 2.0 * ss - 6.0 * ttc + 2.0 * ts * (c + 4.0)];
        let f2 = [9.0 * w * (ttcc - ss) * c, 3.0 * ss + 3.0 * ttcc + 6.0 * ts * c1, 3.0 * ss + 3.0 * ttcc - 6.0 * ts * c1];

        let (mut aa, mut ab) = (0.0, 0.0);
        let ch = (c1 / 2.0).sqrt();
        let inv_ro0 = 1.5 * ch * (ch + 1.0);
        for i in 0..3 {
            let a = 2.0 * f2[i] + f1[i] * inv_ro0;
            let b = f2[i] - f0[i] * inv_ro0 * inv_ro0;
            aa += a * a;
            ab += a * b;
        }
        let ro = ab / (aa * inv_ro0 + 1e-9);

        let mut err2 = 0.0;
        for i in 0..3 {
            let err = f0[i] + ro * (f1[i] + ro * f2[i]);
            err2 += err * err;
        }
        if !(err2 < self.err_c) {
            return 0;
        }

        let r = ro * c1 - 1.0;
        let ro0 = t * r - ro * s;
        let ro1 = t * r + ro * s;

        let check_dir = if check_flags & FLAG_DIR_2 != 0 { 2 } else { 1 };
        if dir & check_dir != 0 {
            let (mut test_s, mut test0, mut test1) = (s, ro0, ro1);
            if check_flags & FLAG_DIR_2 != 0 {
                test_s = -test_s;
                test0 = -test0;
                test1 = -test1;
            }
            let mut flags = 0;
            if 2.0 * test_s * r < dc[0] + dc[1] {
                flags |= FLAG_INTERSECTION;
            }
            if normal[0].len - test0 < 0.0 {
                flags |= FLAG_ZERO_0;
            }
            if normal[1].len + test1 < 0.0 {
                flags |= FLAG_ZERO_1;
            }
            if normal[0].len + dc[0] + test_s - test1 * c < 0.0 {
                flags |= FLAG_CLIP_0;
            }
            if normal[1].len + dc[1] + test_s + test0 * c < 0.0 {
                flags |= FLAG_CLIP_1;
            }
            if (flags ^ check_flags) & (check_flags >> FLAG_COUNT) != 0 {
                dir &= !check_dir;
                if dir == 0 {
                    return 0;
                }
            }
        }

        let (d0c, d0s) = (2.0 * dc[0], 2.0 * ds[0]);
        let (d1c, d1s) = (2.0 * dc[1], 2.0 * ds[1]);
        let mut dot0 = d0c + 3.0 * normal[0].len;
        let mut crs0 = d0s + 3.0 * ro0 * normal[0].len;
        let mut dot1 = d1c + 3.0 * normal[1].len;
        let mut crs1 = d1s + 3.0 * ro1 * normal[1].len;
        // Angular error, stage 1.
        if !(crs0.abs() < self.err_a * dot0 && crs1.abs() < self.err_a * dot1) {
            return 0;
        }
        let (cl0, sl0) = (c * normal[0].len, s * normal[0].len);
        let (cl1, sl1) = (c * normal[1].len, -s * normal[1].len);
        dot0 = d0c - ro0 * d0s + cl0 + ro1 * sl0 + cl1 / 3.0;
        dot1 = d1c - ro1 * d1s + cl1 + ro0 * sl1 + cl0 / 3.0;
        crs0 = d0s + ro0 * d0c - sl0 + ro1 * cl0 - sl1 / 3.0;
        crs1 = d1s + ro1 * d1c - sl1 + ro0 * cl1 - sl0 / 3.0;
        // Angular error, stage 2.
        if !(crs0.abs() < self.err_a * dot0 && crs1.abs() < self.err_a * dot1) {
            return 0;
        }
        result[0] = DVector { x: normal[0].v.x + normal[0].v.y * ro0, y: normal[0].v.y - normal[0].v.x * ro0 };
        result[1] = DVector { x: normal[1].v.x + normal[1].v.y * ro1, y: normal[1].v.y - normal[1].v.x * ro1 };
        dir
    }

    fn process_cubic(&mut self, pt: &[Vector], deriv: &[DVector], normal: &[Normal], mut dir: i32, first: bool) -> bool {
        let c = vec_dot(normal[0].v, normal[1].v);
        let s = vec_crs(normal[0].v, normal[1].v);
        let dc = [vec_dot(normal[0].v, deriv[1]), vec_dot(normal[1].v, deriv[1])];
        let ds = [vec_crs(normal[0].v, deriv[1]), vec_crs(normal[1].v, deriv[1])];
        let f0 = normal[0].len * c + normal[1].len + dc[1];
        let f1 = normal[1].len * c + normal[0].len + dc[0];
        let mut g0 = normal[0].len * s - ds[1];
        let mut g1 = normal[1].len * s + ds[0];

        let mut abs_s = s;
        let mut check_dir = dir;
        let mut skip_dir = 2;
        let mut flags = FLAG_INTERSECTION | FLAG_DIR_2;
        if s < 0.0 {
            abs_s = -s;
            skip_dir = 1;
            flags = 0;
            g0 = -g0;
            g1 = -g1;
        }

        if !(dc[0] + dc[1] > 0.0) {
            check_dir = 0;
        } else if dir & skip_dir != 0 {
            if f0 < abs_s && f1 < abs_s {
                // Self-intersection.
                let mut d2 = (f0 + dc[1]) * normal[1].len + (f1 + dc[0]) * normal[0].len;
                d2 = (d2 + vec_dot(deriv[1], deriv[1])) / 2.0;
                if d2 < g0 && d2 < g1 {
                    let mut q = (d2 / (2.0 - d2)).sqrt();
                    let h0 = (f0 * q + g0) * normal[1].len;
                    let h1 = (f1 * q + g1) * normal[0].len;
                    q *= (4.0 / 3.0) * d2;
                    if h0 > q && h1 > q {
                        if !self.prepare_skip(pt[0], skip_dir, first) {
                            return false;
                        }
                        if f0 < 0.0 || f1 < 0.0 {
                            if !self.emit_point(pt[0], ZERO, OUTLINE_LINE_SEGMENT, skip_dir) || !self.emit_point(pt[3], ZERO, OUTLINE_LINE_SEGMENT, skip_dir) {
                                return false;
                            }
                        } else {
                            let mul = f0 / abs_s;
                            let offs = DVector { x: normal[0].v.x * mul, y: normal[0].v.y * mul };
                            if !self.emit_point(pt[0], offs, OUTLINE_LINE_SEGMENT, skip_dir) {
                                return false;
                            }
                        }
                        dir &= !skip_dir;
                        if dir == 0 {
                            self.last_normal = normal[1].v;
                            return true;
                        }
                    }
                }
                check_dir ^= skip_dir;
            } else {
                if ds[0] < 0.0 {
                    flags ^= MASK_INTERSECTION;
                }
                if ds[1] < 0.0 {
                    flags ^= MASK_INTERSECTION | FLAG_INTERSECTION;
                }
                let parallel = flags & MASK_INTERSECTION != 0;
                let mut badness = if parallel { 0 } else { 1 };
                if c + g0 < 1.0 {
                    if parallel {
                        flags ^= MASK_ZERO_0 | FLAG_ZERO_0;
                        if c < 0.0 {
                            flags ^= MASK_CLIP_0;
                        }
                        if f0 > abs_s {
                            flags ^= FLAG_ZERO_0 | FLAG_CLIP_0;
                        }
                    }
                    badness += 1;
                } else {
                    flags ^= MASK_INTERSECTION | FLAG_INTERSECTION;
                    if !parallel {
                        flags ^= MASK_ZERO_0;
                        if c > 0.0 {
                            flags ^= MASK_CLIP_0;
                        }
                    }
                }
                if c + g1 < 1.0 {
                    if parallel {
                        flags ^= MASK_ZERO_1 | FLAG_ZERO_1;
                        if c < 0.0 {
                            flags ^= MASK_CLIP_1;
                        }
                        if f1 > abs_s {
                            flags ^= FLAG_ZERO_1 | FLAG_CLIP_1;
                        }
                    }
                    badness += 1;
                } else {
                    flags ^= MASK_INTERSECTION;
                    if !parallel {
                        flags ^= MASK_ZERO_1;
                        if c > 0.0 {
                            flags ^= MASK_CLIP_1;
                        }
                    }
                }
                if badness > 2 {
                    check_dir ^= skip_dir;
                }
            }
        }

        let mut result = [ZERO; 2];
        if check_dir != 0 {
            check_dir = self.estimate_cubic_error(c, s, &dc, &ds, normal, &mut result, flags, check_dir);
        }
        if check_dir != 0 {
            if !self.emit_first_point(pt[0], OUTLINE_CUBIC_SPLINE, check_dir) {
                return false;
            }
            if !self.emit_point(pt[1], result[0], 0, check_dir) || !self.emit_point(pt[2], result[1], 0, check_dir) {
                return false;
            }
            dir &= !check_dir;
            if dir == 0 {
                self.last_normal = normal[1].v;
                return true;
            }
        }

        let mut next = [Vector::default(); 7];
        next[1] = Vector { x: pt[0].x.wrapping_add(pt[1].x), y: pt[0].y.wrapping_add(pt[1].y) };
        let center = Vector { x: pt[1].x.wrapping_add(pt[2].x).wrapping_add(2), y: pt[1].y.wrapping_add(pt[2].y).wrapping_add(2) };
        next[5] = Vector { x: pt[2].x.wrapping_add(pt[3].x), y: pt[2].y.wrapping_add(pt[3].y) };
        next[2] = Vector { x: next[1].x.wrapping_add(center.x), y: next[1].y.wrapping_add(center.y) };
        next[4] = Vector { x: center.x.wrapping_add(next[5].x), y: center.y.wrapping_add(next[5].y) };
        next[3] = Vector { x: (next[2].x.wrapping_add(next[4].x).wrapping_sub(1)) >> 3, y: (next[2].y.wrapping_add(next[4].y).wrapping_sub(1)) >> 3 };
        next[2].x >>= 2;
        next[2].y >>= 2;
        next[4].x >>= 2;
        next[4].y >>= 2;
        next[1].x >>= 1;
        next[1].y >>= 1;
        next[5].x >>= 1;
        next[5].y >>= 1;
        next[0] = pt[0];
        next[6] = pt[3];

        let mut next_deriv = [ZERO; 5];
        next_deriv[0] = DVector { x: deriv[0].x / 2.0, y: deriv[0].y / 2.0 };
        let center_deriv = DVector { x: deriv[1].x / 2.0, y: deriv[1].y / 2.0 };
        next_deriv[4] = DVector { x: deriv[2].x / 2.0, y: deriv[2].y / 2.0 };
        next_deriv[1] = DVector { x: (next_deriv[0].x + center_deriv.x) / 2.0, y: (next_deriv[0].y + center_deriv.y) / 2.0 };
        next_deriv[3] = DVector { x: (center_deriv.x + next_deriv[4].x) / 2.0, y: (center_deriv.y + next_deriv[4].y) / 2.0 };
        next_deriv[2] = DVector { x: (next_deriv[1].x + next_deriv[3].x) / 2.0, y: (next_deriv[1].y + next_deriv[3].y) / 2.0 };

        let len = vec_len(next_deriv[2]);
        if len < self.min_len {
            // Degenerate case.
            let mut next_normal = [Normal::default(); 4];
            next_normal[0] = Normal { v: normal[0].v, len: normal[0].len / 2.0 };
            next_normal[3] = Normal { v: normal[1].v, len: normal[1].len / 2.0 };
            next_deriv[1].x += next_deriv[2].x;
            next_deriv[1].y += next_deriv[2].y;
            next_deriv[3].x += next_deriv[2].x;
            next_deriv[3].y += next_deriv[2].y;
            next_deriv[2] = ZERO;

            let len1 = vec_len(next_deriv[1]);
            if len1 < self.min_len {
                next_normal[1] = normal[0];
            } else {
                let scale = 1.0 / len1;
                next_normal[1] = Normal { v: DVector { x: next_deriv[1].x * scale, y: next_deriv[1].y * scale }, len: len1 };
            }
            let len2 = vec_len(next_deriv[3]);
            if len2 < self.min_len {
                next_normal[2] = normal[1];
            } else {
                let scale = 1.0 / len2;
                next_normal[2] = Normal { v: DVector { x: next_deriv[3].x * scale, y: next_deriv[3].y * scale }, len: len2 };
            }

            if len1 < self.min_len {
                if !self.emit_first_point(next[0], OUTLINE_LINE_SEGMENT, dir) {
                    return false;
                }
            } else if !self.process_cubic(&next[0..], &next_deriv[0..], &next_normal[0..], dir, first) {
                return false;
            }
            if !self.start_segment(next[2], next_normal[2].v, dir) {
                return false;
            }
            if len2 < self.min_len {
                if !self.emit_first_point(next[3], OUTLINE_LINE_SEGMENT, dir) {
                    return false;
                }
            } else if !self.process_cubic(&next[3..], &next_deriv[2..], &next_normal[2..], dir, false) {
                return false;
            }
            return true;
        }

        let scale = 1.0 / len;
        let next_normal = [
            Normal { v: normal[0].v, len: normal[0].len / 2.0 },
            Normal { v: DVector { x: next_deriv[2].x * scale, y: next_deriv[2].y * scale }, len },
            Normal { v: normal[1].v, len: normal[1].len / 2.0 },
        ];
        self.process_cubic(&next[0..], &next_deriv[0..], &next_normal[0..], dir, first)
            && self.process_cubic(&next[3..], &next_deriv[2..], &next_normal[1..], dir, false)
    }

    fn add_cubic(&mut self, pt1: Vector, pt2: Vector, pt3: Vector, dir: i32) -> bool {
        let mut flags = 9usize;
        let mut dx0 = pt1.x.wrapping_sub(self.last_point.x);
        let mut dy0 = pt1.y.wrapping_sub(self.last_point.y);
        if self.small(dx0, dy0) {
            dx0 = pt2.x.wrapping_sub(self.last_point.x);
            dy0 = pt2.y.wrapping_sub(self.last_point.y);
            if self.small(dx0, dy0) {
                return self.add_line(pt3, dir);
            }
            flags ^= 1;
        }
        let mut dx2 = pt3.x.wrapping_sub(pt2.x);
        let mut dy2 = pt3.y.wrapping_sub(pt2.y);
        if self.small(dx2, dy2) {
            dx2 = pt3.x.wrapping_sub(pt1.x);
            dy2 = pt3.y.wrapping_sub(pt1.y);
            if self.small(dx2, dy2) {
                return self.add_line(pt3, dir);
            }
            flags ^= 4;
        }
        if flags == 12 {
            return self.add_line(pt3, dir);
        }
        let pt = [self.last_point, pt1, pt2, pt3];
        self.last_point = pt3;
        let dx1 = pt[flags >> 2].x.wrapping_sub(pt[flags & 3].x);
        let dy1 = pt[flags >> 2].y.wrapping_sub(pt[flags & 3].y);
        let deriv = [
            DVector { x: f64::from(dy0) * self.yscale, y: -f64::from(dx0) * self.xscale },
            DVector { x: f64::from(dy1) * self.yscale, y: -f64::from(dx1) * self.xscale },
            DVector { x: f64::from(dy2) * self.yscale, y: -f64::from(dx2) * self.xscale },
        ];
        let (len0, len2) = (vec_len(deriv[0]), vec_len(deriv[2]));
        let (scale0, scale2) = (1.0 / len0, 1.0 / len2);
        let normal = [
            Normal { v: DVector { x: deriv[0].x * scale0, y: deriv[0].y * scale0 }, len: len0 },
            Normal { v: DVector { x: deriv[2].x * scale2, y: deriv[2].y * scale2 }, len: len2 },
        ];
        let first = self.contour_start;
        self.start_segment(pt[0], normal[0].v, dir) && self.process_cubic(&pt, &deriv, &normal, dir, first)
    }

    fn close_contour(&mut self, mut dir: i32) -> bool {
        if self.contour_start {
            if dir & 3 == 3 {
                dir = 1;
            }
            let last = self.last_point;
            if !self.draw_circle(last, dir) {
                return false;
            }
        } else {
            let first = self.first_point;
            if !self.add_line(first, dir) {
                return false;
            }
            let first_normal = self.first_normal;
            if !self.start_segment(first, first_normal, dir) {
                return false;
            }
            if !self.emit_point(first, first_normal, OUTLINE_LINE_SEGMENT, !self.last_skip & dir & self.first_skip) {
                return false;
            }
            if self.last_normal.x != self.first_normal.x || self.last_normal.y != self.first_normal.y {
                let last_normal = self.last_normal;
                self.fix_first_point(first, last_normal, !self.last_skip & dir & !self.first_skip);
            }
            self.contour_start = true;
        }
        if dir & 1 != 0 {
            self.result[0].close_contour();
        }
        if dir & 2 != 0 {
            self.result[1].close_contour();
        }
        self.contour_first = [self.result[0].points.len(), self.result[1].points.len()];
        true
    }
}

/// `ass_outline_stroke`: the two border outlines (±1 offset in the space
/// scaled by `1/xbord`, `1/ybord`) of `path`, within `eps`. `None` on a
/// point out of range or a malformed path.
pub fn stroke(path: &Outline, xbord: i32, ybord: i32, eps: i32) -> Option<(Outline, Outline)> {
    let rad = xbord.max(ybord);
    if rad < eps || rad > OUTLINE_MAX || eps <= 0 {
        return None;
    }
    let mut r0 = Outline { points: Vec::with_capacity(2 * path.points.len()), segments: Vec::with_capacity(2 * path.segments.len()) };
    let mut r1 = Outline { points: Vec::with_capacity(2 * path.points.len()), segments: Vec::with_capacity(2 * path.segments.len()) };
    let rel_err = f64::from(eps) / f64::from(rad);
    let e = (2.0 * rel_err).sqrt();
    let mut str = Stroker {
        result: [&mut r0, &mut r1],
        contour_first: [0, 0],
        xbord: f64::from(xbord),
        ybord: f64::from(ybord),
        xscale: 1.0 / f64::from(eps.max(xbord)),
        yscale: 1.0 / f64::from(eps.max(ybord)),
        eps,
        contour_start: true,
        first_skip: 0,
        last_skip: 0,
        first_normal: ZERO,
        last_normal: ZERO,
        first_point: Vector::default(),
        last_point: Vector::default(),
        merge_cos: 1.0 - rel_err,
        split_cos: 1.0 + 8.0 * rel_err - 4.0 * (1.0 + rel_err) * e,
        min_len: rel_err / 4.0,
        err_q: 8.0 * (1.0 + rel_err) * (1.0 + rel_err),
        err_c: 390.0 * rel_err * rel_err,
        err_a: e,
    };
    const DIR: i32 = 3;
    let (mut start, mut cur) = (0usize, 0usize);
    for &seg in &path.segments {
        if start == cur {
            str.last_point = *path.points.get(start)?;
        }
        let n = usize::from(seg & OUTLINE_COUNT_MASK);
        cur += n;
        if cur > path.points.len() {
            return None;
        }
        let mut end = cur;
        if seg & OUTLINE_CONTOUR_END != 0 {
            end = start;
            start = cur;
        }
        let end_pt = *path.points.get(end)?;
        let ok = match n {
            1 => str.add_line(end_pt, DIR),
            2 => str.add_quadratic(path.points[cur - 1], end_pt, DIR),
            3 => str.add_cubic(path.points[cur - 2], path.points[cur - 1], end_pt, DIR),
            _ => false,
        };
        if !ok {
            return None;
        }
        if start == cur && !str.close_contour(DIR) {
            return None;
        }
    }
    drop(str);
    (start == cur && cur == path.points.len()).then_some((r0, r1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_iterate_with_their_ends() {
        let mut o = Outline::default();
        o.add_rect(0, 0, 10, 10);
        let mut ends = Vec::new();
        o.for_each_segment(|p| ends.push((p[0], p[p.len() - 1])));
        assert_eq!(ends.len(), 4);
        assert_eq!(ends[3], (Vector { x: 0, y: 10 }, Vector { x: 0, y: 0 }));
    }
}
