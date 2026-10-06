//! Canonical Huffman decoding from per-length counts, the
//! `ff_vlc_init_from_lengths` equivalent for Cook's VLC tables.
//!
//! Ported/adapted from FFmpeg libavcodec/bitstream.c (VLC init from
//! lengths), commit 2da55bf. License: LGPL-2.1-or-later.
//!
//! FFmpeg's `build_vlc` (cook.c) expands 16 per-length count buckets into a
//! symbol-length list, then assigns canonical codes in symbol order. The
//! tables list symbols in non-decreasing length order, so the symbol at
//! canonical position `(cumulative count of shorter lengths) +
//! (code - first_code[len])` is exactly `symbols[that index] + offset`.

#![forbid(unsafe_code)]

/// One decoded VLC table.
pub struct LengthVlc {
    /// `counts[l]` = number of codes of length `l` (index 0 unused).
    counts: [u32; 17],
    /// `first[l]` = first canonical code of length `l`.
    first: [u32; 17],
    /// `base[l]` = index into `symbols` of the first length-`l` symbol.
    base: [u32; 17],
    /// Symbol values (codec-level, `symbol + offset` applied) in canonical order.
    symbols: Vec<u32>,
    /// Longest code length.
    max_len: usize,
}

impl LengthVlc {
    /// Build from per-length counts (`counts[i]` = codes of length `i+1`),
    /// the symbol list in canonical order, and a constant offset added to
    /// every symbol (FFmpeg's `offset` argument, e.g. -12 for the envelope
    /// quantiser tables).
    pub fn from_counts(counts: &[u8; 16], symbols: &[u32], offset: i32) -> Result<Self, String> {
        let mut cnt = [0u32; 17];
        let mut num = 0usize;
        for (i, &c) in counts.iter().enumerate() {
            cnt[i + 1] = c as u32;
            num += c as usize;
        }
        if num == 0 {
            return Err("vlc: empty table".into());
        }
        if num > symbols.len() {
            return Err(format!("vlc: need {num} symbols, have {}", symbols.len()));
        }

        // Kraft check: canonical codes must fit in the code space, and the
        // tree must be complete (FFmpeg requires complete tables here).
        // A complete canonical tree uses every code of its longest length:
        // first[max] + counts[max] == 1 << max.
        let mut first = [0u32; 17];
        let mut base = [0u32; 17];
        let mut acc = 0u32;
        let mut sym_acc = 0u32;
        for len in 1..=16usize {
            first[len] = acc;
            base[len] = sym_acc;
            if acc + cnt[len] > (1u32 << len) && cnt[len] > 0 {
                return Err(format!("vlc: overfull at length {len}"));
            }
            acc = (acc + cnt[len]) << 1;
            sym_acc += cnt[len];
        }
        let max_len = (1..=16).rev().find(|&l| cnt[l] > 0).unwrap_or(0);
        if first[max_len] + cnt[max_len] != (1u32 << max_len) {
            return Err("vlc: incomplete code space".into());
        }
        let symbols = symbols[..num]
            .iter()
            .map(|&s| (s as i64 + offset as i64) as u32)
            .collect();

        Ok(Self { counts: cnt, first, base, symbols, max_len })
    }

    /// Longest code length in bits.
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    /// Symbol count.
    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    /// Decode one symbol reading MSB-first bits. Returns the symbol value,
    /// or `None` when the bits do not name a code.
    pub fn decode(&self, read_bit: &mut dyn FnMut() -> Option<u32>) -> Option<u32> {
        let mut code = 0u32;
        for len in 1..=self.max_len {
            let bit = read_bit()?;
            code = (code << 1) | bit;
            let cnt = self.counts[len];
            if cnt > 0 && code >= self.first[len] && code - self.first[len] < cnt {
                return Some(self.symbols[(self.base[len] + (code - self.first[len])) as usize]);
            }
        }
        None
    }
    /// Decode one signed symbol reading MSB-first bits.
    #[inline]
    pub fn decode_signed(&self, read_bit: &mut dyn FnMut() -> Option<u32>) -> Option<i32> {
        self.decode(read_bit).map(|v| v as i32)
    }
}
