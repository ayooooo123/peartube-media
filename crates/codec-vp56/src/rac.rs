// Ported from FFmpeg (commit 2da55bf): libavcodec/vpx_rac.h and vpx_rac.c
// (ff_vpx_init_range_decoder, vpx_rac_renorm, vpx_rac_get_prob,
// vpx_rac_get, vpx_rac_is_end), vp56.h (vp56_rac_gets, vp56_rac_gets_nn,
// vp56_rac_get_tree) and get_bits.h (the checked bit reader).
// License: LGPL-2.1-or-later

//! The VP5/VP6 boolean range decoder and the bit reader of VP6's Huffman
//! coefficients. Both read zeros past their data, as FFmpeg's do from its
//! zeroed input padding.

/// ff_vpx_norm_shift: the shift that brings `high` back to 128..=255.
const NORM_SHIFT: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = (i as u8).leading_zeros() as u8;
        i += 1;
    }
    t
};

/// A VP56Tree node: a branch `(val > 0, prob_idx)` or a leaf `(-symbol, _)`.
pub(crate) type Tree = [(i8, u8)];

/// VPXRangeCoder over a copy of the packet from its start: reads before
/// `end` refill it, and reads at or past `end` see the packet's later bytes
/// (another partition, the alpha frame) and then zeros, as FFmpeg's reads
/// see its padded packet.
#[derive(Clone, Default)]
pub(crate) struct Rac {
    high: u32,
    bits: i32,
    data: Vec<u8>,
    end: usize,
    pos: usize,
    code_word: u32,
    end_reached: i32,
}

impl Rac {
    /// ff_vpx_init_range_decoder: `size` bytes of `buf` (the packet from
    /// the coder's start); None for size < 1.
    pub fn new(buf: &[u8], size: i64) -> Option<Self> {
        if size < 1 {
            return None;
        }
        let mut c = Self { high: 255, bits: -16, data: buf.to_vec(), end: size as usize, pos: 0, code_word: 0, end_reached: 0 };
        c.code_word = (u32::from(c.byte(0)) << 16) | (u32::from(c.byte(1)) << 8) | u32::from(c.byte(2));
        c.pos = 3;
        Some(c)
    }

    fn byte(&self, i: usize) -> u8 {
        self.data.get(i).copied().unwrap_or(0)
    }

    /// vpx_rac_is_end.
    pub fn is_end(&mut self) -> bool {
        if self.pos >= self.end && self.bits >= 0 {
            self.end_reached += 1;
        }
        self.end_reached > 10
    }

    /// vpx_rac_renorm.
    fn renorm(&mut self) -> u32 {
        let shift = u32::from(NORM_SHIFT[self.high as usize & 0xff]);
        self.high <<= shift;
        let mut code_word = self.code_word << shift;
        let mut bits = self.bits + shift as i32;
        if bits >= 0 && self.pos < self.end {
            let v = (u32::from(self.byte(self.pos)) << 8) | u32::from(self.byte(self.pos + 1));
            code_word |= v << bits;
            self.pos += 2;
            bits -= 16;
        }
        self.bits = bits;
        code_word
    }

    /// vpx_rac_get_prob (and its branchy variant, the same bit).
    pub fn get_prob(&mut self, prob: u8) -> bool {
        let code_word = self.renorm();
        let low = 1 + (((self.high - 1) * u32::from(prob)) >> 8);
        let low_shift = low << 16;
        let bit = code_word >= low_shift;
        if bit {
            self.high -= low;
            self.code_word = code_word - low_shift;
        } else {
            self.high = low;
            self.code_word = code_word;
        }
        bit
    }

    /// vpx_rac_get: an equiprobable bit.
    pub fn get(&mut self) -> bool {
        let code_word = self.renorm();
        let low = (self.high + 1) >> 1;
        let low_shift = low << 16;
        let bit = code_word >= low_shift;
        if bit {
            self.high -= low;
            self.code_word = code_word - low_shift;
        } else {
            self.high = low;
            self.code_word = code_word;
        }
        bit
    }

    /// vp56_rac_gets: `bits` equiprobable bits, most significant first.
    pub fn gets(&mut self, bits: u32) -> i32 {
        (0..bits).fold(0, |v, _| (v << 1) | i32::from(self.get()))
    }

    /// vp56_rac_gets_nn: a 7-bit probability, never 0.
    pub fn gets_nn(&mut self) -> u8 {
        let v = self.gets(7) << 1;
        (v + i32::from(v == 0)) as u8
    }

    /// vp56_rac_get_tree.
    pub fn get_tree(&mut self, tree: &Tree, probs: &[u8]) -> i32 {
        let mut i = 0usize;
        while tree[i].0 > 0 {
            if self.get_prob(probs[usize::from(tree[i].1)]) {
                i += tree[i].0 as usize;
            } else {
                i += 1;
            }
        }
        -i32::from(tree[i].0)
    }
}

/// GetBitContext (checked): `size` bits of the packet from `data`'s
/// start, reads past them seeing its later bytes, then zeros.
#[derive(Clone, Default)]
pub(crate) struct Bits {
    data: Vec<u8>,
    size: i64,
    index: i64,
}

impl Bits {
    /// init_get_bits8 on `size` bytes of `buf`.
    pub fn new(buf: &[u8], size: usize) -> Self {
        Self { data: buf.to_vec(), size: size as i64 * 8, index: 0 }
    }

    fn bit_at(&self, i: i64) -> u32 {
        let Ok(i) = usize::try_from(i) else { return 0 };
        self.data.get(i / 8).map_or(0, |b| u32::from((b >> (7 - i % 8)) & 1))
    }

    /// get_bits(n), n <= 25.
    pub fn get(&mut self, n: u32) -> u32 {
        let v = (0..i64::from(n)).fold(0, |v, k| (v << 1) | self.bit_at(self.index + k));
        self.index = (self.index + i64::from(n)).min(self.size + 8);
        v
    }

    pub fn bits_left(&self) -> i64 {
        self.size - self.index
    }
}
