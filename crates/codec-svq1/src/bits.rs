// Ported from FFmpeg (commit 2da55bf): libavcodec/get_bits.h (the checked
// reader, get_vlc2, skip_1stop_8data_bits) and libavcodec/mathops.h
// (sign_extend, mid_pred).
// License: LGPL-2.1-or-later

//! FFmpeg's checked bit reader: MSB first; past the end it reads the zero
//! padding FFmpeg's packets carry, and its position stops 8 bits past the
//! end, so `left` goes as low as -8.

pub struct Bits<'a> {
    buf: &'a [u8],
    index: usize,
    size: usize,
}

impl<'a> Bits<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, index: 0, size: buf.len() * 8 }
    }

    fn byte(&self, at: usize) -> u64 {
        u64::from(self.buf.get(at).copied().unwrap_or(0))
    }

    /// The next `n` (at most 32) bits, not consumed.
    fn peek(&self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let at = self.index >> 3;
        let window = (0..5).fold(0u64, |w, k| w << 8 | self.byte(at + k));
        let shift = 40 - (self.index & 7) as u32 - n;
        ((window >> shift) & ((1u64 << n) - 1)) as u32
    }

    pub fn skip(&mut self, n: usize) {
        self.index = (self.index + n).min(self.size + 8);
    }

    pub fn get(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.skip(n as usize);
        v
    }

    pub fn bit(&mut self) -> u32 {
        self.get(1)
    }

    /// get_bits_left.
    pub fn left(&self) -> i64 {
        self.size as i64 - self.index as i64
    }

    /// skip_1stop_8data_bits: false where FFmpeg returns an error.
    pub fn skip_1stop_8data_bits(&mut self) -> bool {
        if self.left() <= 0 {
            return false;
        }
        while self.bit() == 1 {
            self.skip(8);
            if self.left() <= 0 {
                return false;
            }
        }
        true
    }
}

const LEAF: u32 = 1 << 31;

/// A prefix code read bit by bit; symbols are the table indexes, as
/// FFmpeg's VLCs built from `(code, length)` tables (length 0: unused).
pub struct Vlc {
    nodes: Vec<[u32; 2]>,
}

impl Vlc {
    pub fn new(codes: &[(u32, u8)]) -> Self {
        let mut nodes = vec![[0u32; 2]];
        for (symbol, &(code, len)) in codes.iter().enumerate() {
            let mut node = 0;
            for k in (0..len).rev() {
                let b = ((code >> k) & 1) as usize;
                let next = nodes[node][b];
                assert!(next & LEAF == 0, "not a prefix code");
                if k == 0 {
                    assert!(next == 0, "not a prefix code");
                    nodes[node][b] = LEAF | symbol as u32;
                } else if next == 0 {
                    nodes.push([0; 2]);
                    nodes[node][b] = nodes.len() as u32 - 1;
                    node = nodes.len() - 1;
                } else {
                    node = next as usize;
                }
            }
        }
        Self { nodes }
    }

    /// get_vlc2: the symbol, or -1 for a code the table lacks.
    pub fn read(&self, gb: &mut Bits) -> i32 {
        let mut node = 0;
        loop {
            let e = self.nodes[node][gb.bit() as usize];
            if e == 0 {
                return -1;
            }
            if e & LEAF != 0 {
                return (e & !LEAF) as i32;
            }
            node = e as usize;
        }
    }
}

/// sign_extend.
pub fn sign_extend(v: i32, bits: u32) -> i32 {
    (v << (32 - bits)) >> (32 - bits)
}

/// mid_pred: the median.
pub fn mid_pred(a: i32, b: i32, c: i32) -> i32 {
    a.min(b).max(a.max(b).min(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_zeros_past_the_end_and_stops_8_bits_after_it() {
        let mut gb = Bits::new(&[0b1010_0000]);
        assert_eq!(gb.get(3), 0b101);
        assert_eq!(gb.get(12), 0);
        assert_eq!(gb.left(), -7);
        gb.skip(100);
        assert_eq!(gb.left(), -8);
        assert_eq!(gb.get(32), 0);
    }

    #[test]
    fn vlc_reads_codes_and_flags_missing_ones() {
        // 1 -> 0, 01 -> 1, 001 -> 2; 000 is not a code.
        let vlc = Vlc::new(&[(1, 1), (1, 2), (1, 3)]);
        let mut gb = Bits::new(&[0b1010_0100, 0]);
        assert_eq!([vlc.read(&mut gb), vlc.read(&mut gb), vlc.read(&mut gb)], [0, 1, 2]);
        assert_eq!(vlc.read(&mut gb), -1);
    }

    #[test]
    fn sign_extend_and_median() {
        assert_eq!(sign_extend(33, 6), -31);
        assert_eq!(sign_extend(-33, 6), 31);
        assert_eq!(mid_pred(5, -3, 2), 2);
        assert_eq!(mid_pred(-1, -1, 9), -1);
    }
}
