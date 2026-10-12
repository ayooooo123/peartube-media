// Copyright (C) 2009 Grigori Goronzy <greg@geekmind.org>
//
// Permission to use, copy, modify, and distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
// ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
// OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//
// Ported from libass 0.17.5 (4a05d81), ass_drawing.c. Allocations bounded;
// the token list uses indices, and an OutlineBuilder closes contours.

use crate::outline::{GlyphOutliner, Outline, Rect, Vector, OUTLINE_MAX};
use crate::utils::mystrtod;
use ttf_parser::OutlineBuilder;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind { Move, MoveNc, Line, Cubic, Spline, Extend }

#[derive(Clone, Copy)]
struct Token { kind: Kind, point: Vector }

fn point(text: &[u8], at: &mut usize) -> Option<Vector> {
    let (x, end) = mystrtod(text, *at)?;
    *at = end;
    let (y, end) = mystrtod(text, *at)?;
    *at = end;
    if !x.is_finite() || !y.is_finite() || x.abs() > f64::from(OUTLINE_MAX) / 64.0 || y.abs() > f64::from(OUTLINE_MAX) / 64.0 { return None; }
    Some(Vector { x: (x * 64.0) as i32, y: (y * 64.0) as i32 })
}

fn tokens(text: &[u8]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let (mut at, mut m_seen, mut spline) = (0, false, None);
    while at < text.len() && out.len() < 8192 {
        let command = text[at];
        at += 1;
        let (kind, batch) = match command {
            b'm' => { m_seen = true; (Kind::Move, 1) }
            b'n' => {
                if out.is_empty() && !m_seen { return Vec::new(); }
                (Kind::MoveNc, 1)
            }
            b'l' if !out.is_empty() => (Kind::Line, 1),
            b'b' if !out.is_empty() => (Kind::Cubic, 3),
            b's' if !out.is_empty() => {
                spline = Some(out.len() - 1);
                (Kind::Spline, 3)
            }
            b'p' if out.len() >= 3 => (Kind::Extend, 1),
            b'c' => {
                if let Some(start) = spline.take() {
                    if start + 3 <= out.len() {
                        for i in start..start + 3 { out.push(Token { kind: Kind::Extend, point: out[i].point }); }
                    }
                }
                continue;
            }
            _ => continue,
        };
        let mut first = true;
        loop {
            let n = if kind == Kind::Spline && !first { 1 } else { batch };
            let mut points = [Vector::default(); 3];
            let mut valid = true;
            for dst in &mut points[..n] {
                if let Some(p) = point(text, &mut at) { *dst = p; } else { valid = false; break; }
            }
            if !valid { if kind == Kind::Spline && first { spline = None; } break; }
            if out.len() + n > 8192 { return out; }
            let kind = if kind == Kind::Spline && !first { Kind::Extend } else { kind };
            out.extend(points[..n].iter().map(|&point| Token { kind, point }));
            first = false;
        }
    }
    out
}

/// Drawing coordinates in 26.6 units, before `\p` scale and baseline offset.
/// The box includes the spline's control points, as libass does.
pub fn parse(text: &[u8]) -> Option<(Outline, Rect)> {
    if text.len() > 64 << 10 { return None; }
    let tokens = tokens(text);
    let mut builder = GlyphOutliner::new(1.0, -1.0);
    let mut bbox = Rect::reset();
    let mut pen = Vector::default();
    let (mut started, mut i) = (false, 0);
    while let Some(token) = tokens.get(i) {
        bbox.update(token.point.x, token.point.y, token.point.x, token.point.y);
        match token.kind {
            Kind::Move | Kind::MoveNc => {
                pen = token.point;
                if token.kind == Kind::Move && started { builder.close(); started = false; }
                i += 1;
            }
            Kind::Line => {
                if !started { builder.move_to(pen.x as f32, pen.y as f32); }
                builder.line_to(token.point.x as f32, token.point.y as f32);
                started = true;
                i += 1;
            }
            Kind::Cubic | Kind::Spline | Kind::Extend => {
                let start = if token.kind == Kind::Extend { i.checked_sub(3)? } else { i.checked_sub(1)? };
                let nodes = tokens.get(start..start + 4)?;
                let mut p = nodes.map_points();
                for v in &p { bbox.update(v.x, v.y, v.x, v.y); }
                if token.kind != Kind::Cubic {
                    for axis in 0..2 {
                        let v: Vec<i32> = p.iter().map(|p| if axis == 0 { p.x } else { p.y }).collect();
                        let (d01, d12, d23) = ((v[1] - v[0]) / 3, (v[2] - v[1]) / 3, (v[3] - v[2]) / 3);
                        let values = [v[1] + ((d12 - d01) >> 1), v[1] + d12, v[2] - d12, v[2] + ((d23 - d12) >> 1)];
                        for (p, value) in p.iter_mut().zip(values) { if axis == 0 { p.x = value; } else { p.y = value; } }
                    }
                }
                if !started { builder.move_to(p[0].x as f32, p[0].y as f32); }
                builder.curve_to(p[1].x as f32, p[1].y as f32, p[2].x as f32, p[2].y as f32, p[3].x as f32, p[3].y as f32);
                started = true;
                i += if token.kind == Kind::Extend { 1 } else { 3 };
            }
        }
    }
    Some((builder.finish()?, bbox))
}

trait Points { fn map_points(&self) -> [Vector; 4]; }
impl Points for [Token] {
    fn map_points(&self) -> [Vector; 4] { [self[0].point, self[1].point, self[2].point, self[3].point] }
}
