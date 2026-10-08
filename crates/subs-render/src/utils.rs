// Copyright (C) 2006 Evgeniy Stepanov <eugeni.stepanov@gmail.com>
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
// Derived from libass 0.17.5 (commit 4a05d81): libass/ass_utils.c,
// libass/ass_utils.h and the number parsing of libass/ass.c. Changed for
// PearTube on 2026-10-08: ported to safe Rust. `strtod` is written anew
// with C `strtod`'s decimal syntax, as `ass_strtod` reads it.

//! Byte-string and number helpers shared by the parser and renderer.

/// `skip_spaces`: spaces and tabs.
pub fn skip_spaces(s: &[u8], mut at: usize) -> usize {
    while at < s.len() && (s[at] == b' ' || s[at] == b'\t') {
        at += 1;
    }
    at
}

/// `rskip_spaces`: back over spaces and tabs, not past `limit`.
pub fn rskip_spaces(s: &[u8], mut end: usize, limit: usize) -> usize {
    while end > limit && (s[end - 1] == b' ' || s[end - 1] == b'\t') {
        end -= 1;
    }
    end
}

/// C `isspace` in the "C" locale.
pub fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}


/// `ass_strncasecmp(s, prefix, prefix.len()) == 0`.
pub fn starts_with_ignore_case(s: &[u8], prefix: &[u8]) -> bool {
    s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// C `strtod` over decimal numbers, as `ass_strtod` reads them: leading
/// white space, a sign, digits with at most one point, an exponent. The
/// value and the bytes read (0: no number).
pub fn strtod(s: &[u8]) -> (f64, usize) {
    let mut p = 0;
    while p < s.len() && is_space(s[p]) {
        p += 1;
    }
    let start = p;
    if p < s.len() && (s[p] == b'+' || s[p] == b'-') {
        p += 1;
    }
    let mut digits = 0;
    let mut point = false;
    while p < s.len() {
        match s[p] {
            b'0'..=b'9' => digits += 1,
            b'.' if !point => point = true,
            _ => break,
        }
        p += 1;
    }
    if digits == 0 {
        return (0.0, 0);
    }
    let mut end = p;
    if p < s.len() && (s[p] == b'e' || s[p] == b'E') {
        let mut q = p + 1;
        if q < s.len() && (s[q] == b'+' || s[q] == b'-') {
            q += 1;
        }
        let exp_start = q;
        while q < s.len() && s[q].is_ascii_digit() {
            q += 1;
        }
        if q > exp_start {
            end = q;
        }
    }
    // The text is ASCII digits, signs, a point and an exponent.
    let text = std::str::from_utf8(&s[start..end]).unwrap_or("0");
    // A lone point among digits ("5.") parses in C but not in Rust.
    let value = text.parse::<f64>().or_else(|_| format!("{text}0").parse::<f64>()).unwrap_or(0.0);
    (value, end)
}

/// `mystrtod`: the number at `at`, and where it ends.
pub fn mystrtod(s: &[u8], at: usize) -> Option<(f64, usize)> {
    let (v, n) = strtod(s.get(at..).unwrap_or_default());
    (n > 0).then_some((v, at + n))
}

/// C `strtoll` with `base` 10 or 16, saturating; the value and the bytes
/// read (0: no number).
pub fn strtoll(s: &[u8], base: u32) -> (i64, usize) {
    let mut p = 0;
    while p < s.len() && is_space(s[p]) {
        p += 1;
    }
    let mut negative = false;
    if p < s.len() && (s[p] == b'+' || s[p] == b'-') {
        negative = s[p] == b'-';
        p += 1;
    }
    if base == 16 && p + 1 < s.len() && s[p] == b'0' && (s[p + 1] | 0x20) == b'x' && s.get(p + 2).is_some_and(|c| c.is_ascii_hexdigit()) {
        p += 2;
    }
    let start = p;
    let mut value: i128 = 0;
    while p < s.len() {
        let Some(d) = (s[p] as char).to_digit(base) else { break };
        value = (value * i128::from(base) + i128::from(d)).min(i128::from(i64::MAX) + 1);
        p += 1;
    }
    if p == start {
        return (0, 0);
    }
    let value = if negative { -value } else { value };
    (value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64, p)
}


fn read_digits(s: &[u8], mut p: usize, base: u32) -> Option<(u32, usize)> {
    let start = p;
    let mut val: u32 = 0;
    while p < s.len() {
        let c = s[p];
        let digit = match c {
            b'0'..=b'9' if u32::from(c - b'0') < base.min(10) => u32::from(c - b'0'),
            b'a'..=b'z' if base > 10 && u32::from(c - b'a') < base - 10 => u32::from(c - b'a') + 10,
            b'A'..=b'Z' if base > 10 && u32::from(c - b'A') < base - 10 => u32::from(c - b'A') + 10,
            _ => break,
        };
        val = val.wrapping_mul(base).wrapping_add(digit);
        p += 1;
    }
    (p != start).then_some((val, p))
}

/// `mystrtou32_modulo`: a number reduced modulo 2^32 (VSFilter's `scanf`),
/// and where it ends.
pub fn mystrtou32_modulo(s: &[u8], at: usize, base: u32) -> Option<(u32, usize)> {
    let mut p = skip_spaces(s, at);
    let mut negative = false;
    if p < s.len() && s[p] == b'+' {
        p += 1;
    } else if p < s.len() && s[p] == b'-' {
        negative = true;
        p += 1;
    }
    if base == 16 && starts_with_ignore_case(&s[p..], b"0x") {
        p += 2;
    }
    let (v, end) = read_digits(s, p, base)?;
    Some((if negative { v.wrapping_neg() } else { v }, end))
}

/// `parse_int_header`: decimal, or hexadecimal after `&h` or `0x`.
pub fn parse_int_header(s: &[u8]) -> i32 {
    let (s, base) = if starts_with_ignore_case(s, b"&h") || starts_with_ignore_case(s, b"0x") { (&s[2..], 16) } else { (s, 10) };
    mystrtou32_modulo(s, 0, base).map_or(0, |(v, _)| v) as i32
}

/// `parse_color_header`: an `&HAABBGGRR` colour as libass's RGBA.
pub fn parse_color_header(s: &[u8]) -> u32 {
    (parse_int_header(s) as u32).swap_bytes()
}

/// `parse_bool`: `yes`, or a positive number.
pub fn parse_bool(s: &[u8]) -> bool {
    let s = &s[skip_spaces(s, 0)..];
    starts_with_ignore_case(s, b"yes") || strtoll(s, 10).0 > 0
}

pub const VALIGN_SUB: i32 = 0;
pub const VALIGN_CENTER: i32 = 8;
pub const VALIGN_TOP: i32 = 4;

/// `numpad2align`: a numpad alignment (`\an`, ASS styles) as libass's.
pub fn numpad2align(val: i32) -> i32 {
    let val = if val < -i32::MAX {
        2
    } else if val < 0 {
        -val
    } else {
        val
    };
    let mut res = ((val - 1) % 3) + 1;
    if val <= 3 {
        res |= VALIGN_SUB;
    } else if val <= 6 {
        res |= VALIGN_CENTER;
    } else {
        res |= VALIGN_TOP;
    }
    res
}


/// C `lrint` in the default rounding mode, saturating.
pub fn lrint(x: f64) -> i64 {
    if x.is_nan() {
        return i64::MIN;
    }
    x.round_ties_even().clamp(i64::MIN as f64, i64::MAX as f64) as i64
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_parse_as_c_does() {
        assert_eq!(strtod(b"  -1.5e2x"), (-150.0, 8));
        assert_eq!(strtod(b"5."), (5.0, 2));
        assert_eq!(strtod(b".x"), (0.0, 0));
        assert_eq!(strtod(b"1e"), (1.0, 1));
        assert_eq!(parse_int_header(b"&H00FF00FF"), 0x00FF00FF);
        assert_eq!(parse_int_header(b"-1"), -1);
        assert_eq!(parse_int_header(b"99999999999"), 99999999999u64 as u32 as i32);
        assert_eq!(parse_color_header(b"&H00F9FDFB"), 0xFBFDF900);
    }
}
