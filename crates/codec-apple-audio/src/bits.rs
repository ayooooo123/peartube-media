//! Bit reader + canonical-table Huffman (VLC) decoder for the Apple
//! audio codecs.
//!
//! QDM2, QDMC and ALAC all read **little-endian bitstreams** (FFmpeg's
//! `BITSTREAM_READER_LE`): within each byte the LSB is consumed first,
//! multi-bit fields are assembled LSB-first, and VLC codes are matched
//! LSB-first against a canonical-length code assignment. This module
//! provides one reader and one Huffman-table builder used by every
//! decoder in this crate.
//!
//! # Provenance
//!
//! The reader mirrors the observable semantics of FFmpeg's
//! `get_bits.h` `BITSTREAM_READER_LE` reader and the VLC tables mirror
//! `ff_vlc_init_from_lengths(..., VLC_INIT_LE, ...)` + `get_vlc2`:
//! canonical codes are assigned in *table order* (symbol order), not
//! in the shortest-first order `ff_vlc_init` needs, because
//! `-from_lengths` walks the length list accumulating code values
//! directly (vlc.c `ff_vlc_init_from_lengths`, commit 2da55bf).

#![allow(clippy::needless_range_loop)]

#[allow(dead_code)]
/// Little-endian bit reader over a byte slice: the **LSB of byte 0 is
/// the first bit read**, and `read(n)` returns the next `n` bits with
/// the first-read bit becoming the LSB of the result — exactly
/// FFmpeg's `get_bits` under `BITSTREAM_READER_LE`.
///
/// Unlike FFmpeg's reader this one never over-reads: every accessor
/// checks the remaining bit count and returns `None` on underflow, so
/// malformed input cannot panic.
#[derive(Debug, Clone)]
pub struct LeBitReader<'a> {
    data: &'a [u8],
    /// Number of bits consumed.
    consumed: usize,
}

impl<'a> LeBitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, consumed: 0 }
    }

    #[inline]
    pub fn bits_consumed(&self) -> usize {
        self.consumed
    }

    /// Total bits available.
    #[inline]
    pub fn len_bits(&self) -> usize {
        self.data.len() * 8
    }

    #[inline]
    pub fn bits_left(&self) -> i64 {
        self.len_bits() as i64 - self.consumed as i64
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bits_left() <= 0
    }

    /// Read `n` bits (n <= 32), LSB-first within each byte and across
    /// the byte stream. Returns `None` when fewer than `n` bits remain.
    #[inline]
    pub fn read(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if n > 32 || self.bits_left() < n as i64 {
            // On underflow the reader is parked at the end; FFmpeg's
            // unchecked path would return stale cache bits. We fail
            // closed; decoders translate this into InvalidData.
            self.consumed = self.len_bits();
            return None;
        }
        let mut v: u64 = 0;
        let mut bit = self.consumed;
        for i in 0..n {
            let byte = self.data[bit >> 3];
            let b = (byte >> (bit & 7)) & 1;
            v |= (b as u64) << i;
            bit += 1;
        }
        self.consumed += n as usize;
        Some(v as u32)
    }

    /// Read a single bit.
    #[inline]
    pub fn read_bit(&mut self) -> Option<bool> {
        self.read(1).map(|v| v != 0)
    }

    /// Skip `n` bits without reading (clamped to the end).
    #[inline]
    pub fn skip(&mut self, n: usize) {
        self.consumed = (self.consumed + n).min(self.len_bits());
    }

    /// Skip to the end (marks the stream exhausted).
    #[inline]
    pub fn skip_to_end(&mut self) {
        self.consumed = self.len_bits();
    }

    /// Read an `n`-bit two's-complement signed value.
    #[inline]
    pub fn read_signed(&mut self, n: u32) -> Option<i32> {
        let v = self.read(n)?;
        let shift = 32 - n;
        Some(((v << shift) as i32) >> shift)
    }

    /// FFmpeg's `get_unary(gb, 0, len)`: count 1-bits until a 0-bit
    /// (the stop) or `len` reads. Index of the first 0 = value.
    #[inline]
    pub fn read_unary(&mut self, stop: u32, len: u32) -> Option<u32> {
        let mut i = 0;
        while i < len {
            match self.read(1) {
                Some(b) if b == stop => break,
                Some(_) => i += 1,
                None => return None,
            }
        }
        Some(i)
    }

    /// The byte-aligned cursor FFmpeg exposes as
    /// `&gb->buffer[get_bits_count(gb) / 8]` (floor of the bit cursor).
    #[inline]
    pub fn byte_cursor(&self) -> usize {
        self.consumed >> 3
    }

    /// Re-anchor the reader at an absolute bit position (used by the
    /// QDM2 superblock walker, which re-inits over the header buffer
    /// and skips to a subpacket offset).
    #[inline]
    pub fn seek_bit(&mut self, bit: usize) {
        self.consumed = bit.min(self.len_bits());
    }

    /// The underlying byte slice (QDMC's checksum walks the raw
    /// packet bytes, not the bit cursor).
    #[inline]
    pub fn data_bytes(&self) -> &'a [u8] {
        self.data
    }

    /// One byte of the underlying slice, or 0 past the end.
    #[inline]
    pub fn data_byte(&self, idx: usize) -> u8 {
        self.data.get(idx).copied().unwrap_or(0)
    }

    /// Rewind `n` consumed bits (ALAC's show_bits/skip_bits(k-1)
    /// pattern peeks one bit more than it consumes).
    #[inline]
    pub fn rewind_bits(&mut self, n: usize) {
        self.consumed = self.consumed.saturating_sub(n);
    }
}

/// One Huffman (VLC) decoding table built from **code lengths**.
///
/// The decode walks the tree MSB-of-code first in *stream bit order*,
/// which for a little-endian stream means each consumed bit extends
/// the code as `code = (code << 1) | bit` and a match fires when
/// `(code, len)` equals an assigned canonical pair. FFmpeg's
/// `VLC_INIT_INPUT_LE` bitswaps each canonical code so the identical
/// match happens against its MSB-first table walk; the observable
/// decode (which bits map to which symbol, how many are consumed) is
/// the same.
#[derive(Debug, Clone, Default)]
pub struct VlcTable {
    /// `map[(code, len)]` lookups: sorted list of `(code, len, symbol)`
    /// with codes as consumed LSB-first (i.e. first stream bit = LSB).
    entries: Vec<(u32, u32, i32)>,
    /// Maximum code length in the table (bits).
    max_len: u32,
}

impl VlcTable {
    /// Build a table from code lengths in **symbol order** (the
    /// `ff_vlc_init_from_lengths` layout: entry *i* carries the length
    /// of symbol `i + offset`; a length of 0 marks an unused symbol).
    ///
    /// Canonical code assignment walks the length list ascending,
    /// `code += 1 << (32 - len)` style — here mirrored in LSB-first
    /// domain: codes are compared as *reversed* bit strings, i.e. we
    /// assign `code` MSB-first canonically and store the bit-reversed
    /// value so stream bits (which arrive LSB-of-symbol first) match.
    pub fn from_lengths(lens: &[i8], offset: i32) -> Option<Self> {
        // Collect (len, symbol) pairs, skipping len==0.
        let mut pairs: Vec<(u32, i32)> = Vec::new();
        for (i, &l) in lens.iter().enumerate() {
            if l > 0 {
                pairs.push((l as u32, i as i32 + offset));
            }
        }
        if pairs.is_empty() {
            return None;
        }
        // Canonical assignment (FFmpeg assigns in list order, which is
        // symbol order; codes increase monotonically as a canonical
        // Huffman walk). Code domain: MSB-first, length `len`.
        let mut entries: Vec<(u32, u32, i32)> = Vec::with_capacity(pairs.len());
        let mut code: u32 = 0;
        let mut max_len = 0u32;
        for &(len, sym) in &pairs {
            if code.checked_add(1u32 << (32 - len)).is_none() {
                // Overdetermined table; reject.
                return None;
            }
            // Store bit-reversed so stream-order matching works: the
            // first bit read from the stream is the code's MSB in
            // canonical form.
            let rev = reverse_bits(code, len);
            entries.push((rev, len, sym));
            code += 1u32 << (32 - len);
            max_len = max_len.max(len);
        }
        entries.sort_unstable();
        Some(VlcTable { entries, max_len })
    }

    /// Decode one symbol: consumes `len` bits and returns
    /// `(symbol, len)`, or `None` when the bits do not form a valid
    /// code (FFmpeg's `get_vlc2` returning -1 — callers then apply
    /// their escape syntax).
    ///
    /// `limit` caps how many bits are speculatively consumed; pass
    /// `table.max_len` (the effective cap also honours a short stream,
    /// so truncated input errors instead of over-reading).
    pub fn decode(&self, br: &mut LeBitReader, limit: u32) -> Option<(i32, u32)> {
        let max = limit.min(self.max_len);
        let available = br.bits_left();
        let effective_max = if available < max as i64 {
            if available <= 0 {
                return None;
            }
            available as u32
        } else {
            max
        };
        let mut code: u32 = 0;
        for len in 1..=effective_max {
            let bit = br.read(1)?;
            code = (code << 1) | bit;
            // Stored codes are left-aligned; the first stream bit is
            // the code's first compared bit, so the top `len` bits of
            // the stored value must equal the bits consumed so far.
            if let Some(&(_, _l, sym)) =
                self.entries.iter().find(|&&(c, l, _)| l == len && (c >> (32 - len)) == code)
            {
                return Some((sym, len));
            }
        }
        None
    }
}

/// Reverse the low `len` bits of `code`.
#[inline]
fn reverse_bits(code: u32, len: u32) -> u32 {
    let mut v = 0u32;
    for i in 0..len {
        v |= ((code >> i) & 1) << (len - 1 - i);
    }
    // Left-align into a 32-bit code so `c >> (32 - len)` extracts it.
    v << (32 - len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_reader_reads_lsb_first() {
        // 0b1010_0101: first bits read = 1,0,1,0,0,1,0,1
        let data = [0b1010_0101u8, 0x00];
        let mut br = LeBitReader::new(&data);
        assert_eq!(br.read(1), Some(1));
        assert_eq!(br.read(1), Some(0));
        assert_eq!(br.read(2), Some(0b10)); // bits: 1,0 -> value 0b01
        assert_eq!(br.read(4), Some(0b0101));
    }

    #[test]
    fn le_reader_multi_byte_assembles_lsb_first() {
        // byte0 = 0b00000001, byte1 = 0b00000010
        // A 9-bit read yields bit0 of byte0 as LSB ... bit0 of byte1 as MSB-ish.
        let data = [0x01u8, 0x02];
        let mut br = LeBitReader::new(&data);
        let v = br.read(9).unwrap();
        // bits in read order: 1 (b0.0), then 0..0, then byte1 bit0 = 0 (LSB of 0x02)
        assert_eq!(v, 1);
        let v2 = br.read(7).unwrap();
        // remaining byte1 bits (LSB-first): 1,0,0,0,0,0,0
        assert_eq!(v2, 0b0000001);
    }

    #[test]
    fn vlc_canonical_from_lengths_matches_ffmpeg_assignment() {
        // Symbol order with lengths [2, 1, 2, 3]:
        // canonical: s1=0 (len1), s0=10, s2=110, s3=111.
        let tbl = VlcTable::from_lengths(&[2, 1, 2, 3], 0).unwrap();
        // Stream bits for s0 (10) in LE order: first bit 1, then 0.
        let mut br = LeBitReader::new(&[0b01]);
        let (sym, len) = tbl.decode(&mut br, 8).unwrap();
        assert_eq!((sym, len), (0, 2));
        // s1 = single 0 bit.
        let mut br = LeBitReader::new(&[0b10]);
        let (sym, len) = tbl.decode(&mut br, 8).unwrap();
        assert_eq!((sym, len), (1, 1));
        // s3 = 111.
        let mut br = LeBitReader::new(&[0b111]);
        let (sym, len) = tbl.decode(&mut br, 8).unwrap();
        assert_eq!((sym, len), (3, 3));
        // s2 = 110.
        let mut br = LeBitReader::new(&[0b011]);
        let (sym, len) = tbl.decode(&mut br, 8).unwrap();
        assert_eq!((sym, len), (2, 3));
    }

    #[test]
    fn unary_stops_at_zero() {
        let data = [0b0011_1101u8]; // LE read: 1,0,1,1,1,1,0,0
        let mut br = LeBitReader::new(&data);
        assert_eq!(br.read_unary(0, 9), Some(1));
        assert_eq!(br.read_unary(0, 9), Some(0));
        assert_eq!(br.read_unary(0, 9), Some(5));
    }
}
