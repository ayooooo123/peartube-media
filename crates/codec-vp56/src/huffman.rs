// Ported from FFmpeg (commit 2da55bf): libavcodec/vp6.c (vp6_huff_cmp,
// vp6_build_huff_tree) and libavcodec/huffman.c (ff_huff_build_tree,
// build_huff_tree, get_tree_codes), decoding as get_vlc2 on the tables
// vlc.c builds from them.
// License: LGPL-2.1-or-later

//! VP6's Huffman coefficient trees, built from its probability models as
//! FFmpeg builds them. FFmpeg turns the tree into VLC tables whose codes
//! are the tree paths (first child 0, second child 1, assigned depth first
//! by ff_vlc_init_from_lengths); walking the tree a bit at a time decodes
//! the same symbols.

use crate::rac::Bits;

/// HNODE: an internal node.
const HNODE: i16 = -1;

/// huffman.h Node.
#[derive(Clone, Copy, Default)]
struct Node {
    sym: i16,
    n0: i16,
    count: u32,
}

/// A built tree: the nodes and the root.
#[derive(Clone, Default)]
pub(crate) struct Huffman {
    nodes: Vec<Node>,
    root: usize,
}

impl Huffman {
    /// vp6_build_huff_tree + ff_huff_build_tree (FF_HUFFMAN_FLAG_HNODE_FIRST)
    /// for `size` symbols from `model` and `map`; None where FFmpeg fails.
    pub fn build(model: &[u8], map: &[u8], size: usize) -> Option<Self> {
        let mut nodes = vec![Node::default(); 2 * size.max(12)];
        // Probabilities from the model; nodes[size..] hold the inner ones.
        nodes[size].count = 256;
        for i in 0..size - 1 {
            let t = nodes[size + i].count;
            let a = t * u32::from(model[i]) >> 8;
            let b = t * (255 - u32::from(model[i])) >> 8;
            nodes[usize::from(map[2 * i])].count = a + u32::from(a == 0);
            nodes[usize::from(map[2 * i + 1])].count = b + u32::from(b == 0);
        }
        // ff_huff_build_tree
        let n = size;
        let mut sum = 0u64;
        for (i, node) in nodes[..n].iter_mut().enumerate() {
            node.sym = i as i16;
            node.n0 = -2;
            sum += u64::from(node.count);
        }
        if sum >> 31 != 0 {
            return None;
        }
        // vp6_huff_cmp: ascending count, then descending symbol (a total
        // order, so any sort gives AV_QSORT's result).
        nodes[..n].sort_by(|a, b| a.count.cmp(&b.count).then(b.sym.cmp(&a.sym)));
        let mut cur_node = n;
        nodes[n * 2 - 1].count = 0;
        let mut i = 0;
        while i < n * 2 - 1 {
            let cur_count = nodes[i].count.wrapping_add(nodes[i + 1].count);
            let mut j = cur_node;
            while j > i + 2 {
                // HNODE_FIRST: a new node goes before nodes of equal count.
                if cur_count > nodes[j - 1].count {
                    break;
                }
                nodes[j] = nodes[j - 1];
                j -= 1;
            }
            nodes[j] = Node { sym: HNODE, count: cur_count, n0: i as i16 };
            cur_node += 1;
            i += 2;
        }
        Some(Self { nodes, root: n * 2 - 2 })
    }

    /// get_vlc2: the next symbol, reading the code's bits.
    pub fn decode(&self, gb: &mut Bits) -> i32 {
        let mut node = self.root;
        // get_tree_codes: a node is a leaf unless it is an HNODE with a
        // count (FFmpeg's no_zero_count).
        while let Some(n) = self.nodes.get(node) {
            if n.sym != HNODE || n.count == 0 {
                return i32::from(n.sym);
            }
            node = n.n0 as usize + gb.get(1) as usize;
        }
        0
    }
}
