// Ported from FFmpeg (commit 2da55bf): libavcodec/vlc.c, libavcodec/vlc.h
// GNU Lesser General Public License 2.1 or later

use crate::bits::BitReader;
use oxideav_core::{Error, Result};

#[derive(Clone, Debug)]
enum VlcEntry {
    Empty,
    Leaf(i32),
    Branch(u32, u32), // left child (0), right child (1) as indices in nodes
}

/// A safe variable-length code table.
#[derive(Clone, Debug)]
pub struct VlcTable {
    nodes: Vec<VlcEntry>,
    // Fast lookup for the first `lookup_bits` (if applicable)
    lookup: Vec<VlcLookup>,
    lookup_bits: usize,
}

#[derive(Clone, Copy, Debug)]
struct VlcLookup {
    bits: u8,
    symbol: i32,
    node_idx: u32, // If bits == 0, index in `nodes` to continue bit-by-bit walk
}

impl Default for VlcLookup {
    fn default() -> Self {
        Self {
            bits: 0,
            symbol: 0,
            node_idx: 0,
        }
    }
}

impl VlcTable {
    pub fn empty() -> Self {
        Self {
            nodes: vec![VlcEntry::Empty],
            lookup: Vec::new(),
            lookup_bits: 0,
        }
    }

    /// Build VLC table from explicit (bits, code) pairs, with symbol = index + offset.
    pub fn from_bits_codes(
        bits: &[u8],
        codes: &[u32],
        symbols: Option<&[i32]>,
        offset: i32,
    ) -> Result<Self> {
        let mut table = Self {
            nodes: vec![VlcEntry::Empty],
            lookup: Vec::new(),
            lookup_bits: 0,
        };

        for i in 0..bits.len() {
            let len = bits[i] as usize;
            if len == 0 {
                continue;
            }
            let code = codes[i];
            let sym = if let Some(syms) = symbols {
                syms[i] + offset
            } else {
                (i as i32) + offset
            };
            table.insert(code, len, sym)?;
        }

        table.build_lookup(8);
        Ok(table)
    }

    /// Build canonical VLC table from lengths (FFmpeg's ff_vlc_init_from_lengths).
    pub fn from_lengths(lengths: &[i8], symbols: Option<&[i32]>, offset: i32) -> Result<Self> {
        let mut table = Self {
            nodes: vec![VlcEntry::Empty],
            lookup: Vec::new(),
            lookup_bits: 0,
        };

        let mut code: u64 = 0;
        for i in 0..lengths.len() {
            let len = lengths[i];
            if len > 0 {
                let ulen = len as usize;
                let sym = if let Some(s) = symbols {
                    s[i] + offset
                } else {
                    (i as i32) + offset
                };
                let code_val = (code >> (32 - ulen)) as u32;
                table.insert(code_val, ulen, sym)?;
                code += 1u64 << (32 - ulen);
            }
        }

        table.build_lookup(8);
        Ok(table)
    }

    fn insert(&mut self, code: u32, len: usize, symbol: i32) -> Result<()> {
        let mut curr = 0;
        for bit_idx in (0..len).rev() {
            let bit = (code >> bit_idx) & 1;
            let (left, right) = match self.nodes[curr] {
                VlcEntry::Empty => {
                    let next = self.nodes.len() as u32;
                    self.nodes.push(VlcEntry::Empty);
                    let left = if bit == 0 { next } else { 0 };
                    let right = if bit == 1 { next } else { 0 };
                    self.nodes[curr] = VlcEntry::Branch(left, right);
                    (left, right)
                }
                VlcEntry::Branch(mut left, mut right) => {
                    if bit == 0 && left == 0 {
                        left = self.nodes.len() as u32;
                        self.nodes.push(VlcEntry::Empty);
                        self.nodes[curr] = VlcEntry::Branch(left, right);
                    } else if bit == 1 && right == 0 {
                        right = self.nodes.len() as u32;
                        self.nodes.push(VlcEntry::Empty);
                        self.nodes[curr] = VlcEntry::Branch(left, right);
                    }
                    (left, right)
                }
                VlcEntry::Leaf(_) => {
                    return Err(Error::invalid("Vlc prefix collision"));
                }
            };
            curr = if bit == 0 { left as usize } else { right as usize };
        }

        self.nodes[curr] = VlcEntry::Leaf(symbol);
        Ok(())
    }

    fn build_lookup(&mut self, lookup_bits: usize) {
        let size = 1usize << lookup_bits;
        let mut lookup = vec![VlcLookup::default(); size];

        for (pattern, entry) in lookup.iter_mut().enumerate() {
            let mut curr = 0;
            let mut matched_leaf = None;
            let mut bits_used = 0;

            for bit_pos in (0..lookup_bits).rev() {
                let bit = (pattern >> bit_pos) & 1;
                match self.nodes[curr] {
                    VlcEntry::Branch(left, right) => {
                        let next = if bit == 0 { left } else { right };
                        if next == 0 {
                            break;
                        }
                        curr = next as usize;
                        bits_used += 1;
                        if let VlcEntry::Leaf(sym) = self.nodes[curr] {
                            matched_leaf = Some((sym, bits_used));
                            break;
                        }
                    }
                    VlcEntry::Leaf(sym) => {
                        matched_leaf = Some((sym, bits_used));
                        break;
                    }
                    VlcEntry::Empty => break,
                }
            }

            if let Some((sym, bits)) = matched_leaf {
                entry.bits = bits as u8;
                entry.symbol = sym;
                entry.node_idx = 0;
            } else {
                entry.bits = 0;
                entry.symbol = 0;
                entry.node_idx = curr as u32;
            }
        }

        self.lookup = lookup;
        self.lookup_bits = lookup_bits;
    }

    /// Decode one symbol from the bit reader.
    #[inline]
    pub fn get_vlc(&self, reader: &mut BitReader<'_>) -> Result<i32> {
        if self.lookup_bits > 0 && reader.bits_left() >= self.lookup_bits {
            let peek = reader.show_bits(self.lookup_bits)? as usize;
            let entry = self.lookup[peek];
            if entry.bits > 0 {
                reader.skip_bits(entry.bits as usize)?;
                return Ok(entry.symbol);
            }
            // Fall back to walking from entry.node_idx
            reader.skip_bits(self.lookup_bits)?;
            let mut curr = entry.node_idx as usize;
            loop {
                match self.nodes[curr] {
                    VlcEntry::Leaf(sym) => return Ok(sym),
                    VlcEntry::Branch(left, right) => {
                        let bit = reader.get_bits1()?;
                        let next = if bit == 0 { left } else { right };
                        if next == 0 {
                            return Err(Error::invalid("invalid VLC code"));
                        }
                        curr = next as usize;
                    }
                    VlcEntry::Empty => return Err(Error::invalid("empty VLC node")),
                }
            }
        }

        // Direct tree walk
        let mut curr = 0;
        loop {
            match self.nodes[curr] {
                VlcEntry::Leaf(sym) => return Ok(sym),
                VlcEntry::Branch(left, right) => {
                    let bit = reader.get_bits1()?;
                    let next = if bit == 0 { left } else { right };
                    if next == 0 {
                        return Err(Error::invalid("invalid VLC code"));
                    }
                    curr = next as usize;
                }
                VlcEntry::Empty => return Err(Error::invalid("empty VLC node")),
            }
        }
    }
}
