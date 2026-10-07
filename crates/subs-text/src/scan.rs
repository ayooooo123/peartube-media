//! `sscanf` / `strtol` conversions with C semantics, so ports of FFmpeg's
//! text parsers accept and reject exactly the inputs FFmpeg does.
//!
//! Numeric conversions follow the C library FFmpeg's reference build uses:
//! leading white space is skipped, a sign is accepted (also by `%u`/`%X`),
//! the digits are converted with `strtoimax`/`strtoumax` saturation and the
//! result is truncated to the destination width by the caller.

/// C `isspace` in the "C" locale.
pub(crate) fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// A cursor over NUL-terminated C-string-like input: reading past the end
/// (or at an embedded NUL) yields 0, exactly as a C parser sees it.
#[derive(Clone, Copy)]
pub(crate) struct Scan<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> Scan<'a> {
    pub fn new(s: &'a [u8]) -> Self {
        Self { s, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn peek(&self) -> u8 {
        self.at(0)
    }

    pub fn at(&self, offset: usize) -> u8 {
        self.s.get(self.pos + offset).copied().unwrap_or(0)
    }

    /// A literal byte of the format (not white space).
    pub fn lit(&mut self, b: u8) -> Option<()> {
        (self.peek() == b && b != 0).then(|| self.pos += 1)
    }

    pub fn lit_str(&mut self, lit: &[u8]) -> Option<()> {
        for &b in lit {
            self.lit(b)?;
        }
        Some(())
    }

    /// White space in a format: any amount, including none.
    pub fn ws(&mut self) {
        while is_space(self.peek()) {
            self.pos += 1;
        }
    }

    /// `%c` (`width` = 1): any byte, white space included.
    pub fn any(&mut self) -> Option<u8> {
        let b = self.peek();
        (b != 0).then(|| {
            self.pos += 1;
            b
        })
    }

    /// `%N[set]` / `%N[^set]`: at least one byte for which `accept` holds,
    /// at most `width` (0 = unbounded).
    pub fn set(&mut self, width: usize, accept: impl Fn(u8) -> bool) -> Option<&'a [u8]> {
        let start = self.pos;
        while (width == 0 || self.pos - start < width) && self.peek() != 0 && accept(self.peek()) {
            self.pos += 1;
        }
        (self.pos > start).then(|| &self.s[start..self.pos])
    }

    fn digits(&mut self, width: usize, base: u32) -> Option<(bool, u64)> {
        self.ws();
        let mut taken = 0usize;
        let room = |taken: usize| width == 0 || taken < width;
        let mut negative = false;
        if room(taken) && matches!(self.peek(), b'+' | b'-') {
            negative = self.peek() == b'-';
            self.pos += 1;
            taken += 1;
        }
        if base == 16 && room(taken + 1) && self.peek() == b'0' && matches!(self.at(1), b'x' | b'X')
            && (self.at(2) as char).is_ascii_hexdigit()
        {
            self.pos += 2;
            taken += 2;
        }
        let mut value: u64 = 0;
        let mut overflow = false;
        let mut any = false;
        while room(taken) {
            let Some(d) = (self.peek() as char).to_digit(base) else { break };
            any = true;
            match value.checked_mul(u64::from(base)).and_then(|v| v.checked_add(u64::from(d))) {
                Some(v) => value = v,
                None => overflow = true,
            }
            self.pos += 1;
            taken += 1;
        }
        any.then_some((negative, if overflow { u64::MAX } else { value }))
    }

    /// `%d` (`%Nd`): `strtoimax` saturation; truncate with `as i32`.
    pub fn int(&mut self, width: usize) -> Option<i64> {
        let save = self.pos;
        let Some((negative, magnitude)) = self.digits(width, 10) else {
            self.pos = save;
            return None;
        };
        Some(if negative {
            if magnitude > i64::MAX as u64 { i64::MIN } else { -(magnitude as i64) }
        } else {
            i64::try_from(magnitude).unwrap_or(i64::MAX)
        })
    }

    /// `%u` (`%Nu`): `strtoumax` semantics, a sign negates modulo 2^64;
    /// truncate with `as u32`.
    pub fn uint(&mut self, width: usize) -> Option<u64> {
        self.unsigned(width, 10)
    }

    /// `%X` (`%NX`), optional `0x` prefix.
    pub fn hex(&mut self, width: usize) -> Option<u64> {
        self.unsigned(width, 16)
    }

    fn unsigned(&mut self, width: usize, base: u32) -> Option<u64> {
        let save = self.pos;
        let Some((negative, magnitude)) = self.digits(width, base) else {
            self.pos = save;
            return None;
        };
        Some(if negative && magnitude != u64::MAX { magnitude.wrapping_neg() } else { magnitude })
    }

    /// `%Nlf` for plain decimal notation (sign, digits, one point, digits,
    /// optional exponent), at most `width` bytes.
    pub fn float(&mut self, width: usize) -> Option<f64> {
        self.ws();
        let start = self.pos;
        let room = |p: usize| width == 0 || p - start < width;
        let mut end = self.pos;
        if room(end) && matches!(self.at(end - self.pos), b'+' | b'-') {
            end += 1;
        }
        let mut mantissa = false;
        while room(end) && self.at(end - self.pos).is_ascii_digit() {
            end += 1;
            mantissa = true;
        }
        if room(end) && self.at(end - self.pos) == b'.' {
            end += 1;
            while room(end) && self.at(end - self.pos).is_ascii_digit() {
                end += 1;
                mantissa = true;
            }
        }
        if !mantissa {
            return None;
        }
        if room(end) && matches!(self.at(end - self.pos), b'e' | b'E') {
            let mut exp = end + 1;
            if room(exp) && matches!(self.at(exp - self.pos), b'+' | b'-') {
                exp += 1;
            }
            let digits_start = exp;
            while room(exp) && self.at(exp - self.pos).is_ascii_digit() {
                exp += 1;
            }
            if exp > digits_start {
                end = exp;
            }
        }
        let text = std::str::from_utf8(&self.s[start..end]).ok()?;
        let value = text.parse::<f64>().ok()?;
        self.pos = end;
        Some(value)
    }
}

/// C `atoi` / `strtol(s, &end, 10)`: optional white space and sign, then
/// digits; `(value, bytes consumed)`, consumed = 0 when no digit.
pub(crate) fn strtol(s: &[u8]) -> (i64, usize) {
    let mut scan = Scan::new(s);
    match scan.int(0) {
        Some(v) => (v, scan.pos()),
        None => (0, 0),
    }
}

/// C `strtol(s, &end, 16)` (used by MicroDVD colours).
pub(crate) fn strtol_hex(s: &[u8]) -> (i64, usize) {
    let mut scan = Scan::new(s);
    match scan.digits(0, 16) {
        Some((negative, magnitude)) => {
            let v = i64::try_from(magnitude).unwrap_or(i64::MAX);
            (if negative { v.saturating_neg() } else { v }, scan.pos())
        }
        None => (0, 0),
    }
}

/// C `strcspn`.
pub(crate) fn strcspn(s: &[u8], reject: &[u8]) -> usize {
    s.iter().position(|b| *b == 0 || reject.contains(b)).unwrap_or(s.len())
}

/// The bytes of `s` up to its first NUL.
pub(crate) fn c_str(s: &[u8]) -> &[u8] {
    &s[..strcspn(s, &[])]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_follow_c_conversions() {
        let mut s = Scan::new(b"  -12:9x");
        assert_eq!(s.int(0), Some(-12));
        assert_eq!(s.lit(b':'), Some(()));
        assert_eq!(s.int(0), Some(9));
        assert_eq!(s.int(0), None);
        assert_eq!(s.peek(), b'x');
        assert_eq!(Scan::new(b"99999999999999999999").int(0), Some(i64::MAX));
        assert_eq!(Scan::new(b"-5").uint(0).map(|v| v as u32), Some(-5i32 as u32));
        assert_eq!(Scan::new(b"123").uint(2), Some(12));
        assert_eq!(Scan::new(b"0x1F&").hex(0), Some(0x1f));
        assert_eq!(Scan::new(b"FFFFFFFFFF").hex(0).map(|v| v as u32), Some(0xffff_ffff));
        assert_eq!(Scan::new(b"23.976}").float(6), Some(23.976));
        assert_eq!(Scan::new(b"25}").float(6), Some(25.0));
        assert_eq!(strtol(b" 12ab"), (12, 3));
        assert_eq!(strtol(b"ab"), (0, 0));
        assert_eq!(strtol_hex(b"#ff"), (0, 0));
        assert_eq!(strtol_hex(b"ff}"), (255, 2));
    }
}
