//! Song end detection, ported from libopenmpt 0.8.9 `soundlib/RowVisitor.cpp`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

#![allow(dead_code)]

use std::collections::BTreeMap;

use crate::channel::ModChannel;
use crate::command::*;
use crate::defs::*;
use crate::sndfile::Module;

const FNV_BASIS: u64 = 14695981039346656037;
const FNV_PRIME: u64 = 1099511628211;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LoopState(u64);

impl LoopState {
    fn new(chns: &[ModChannel], ignore_row: bool) -> Self {
        let mut h = FNV_BASIS;
        if ignore_row {
            h = (h ^ 0xFF).wrapping_mul(FNV_PRIME);
        }
        for (i, c) in chns.iter().enumerate() {
            if c.n_pattern_loop_count != 0 {
                h = (h ^ i as u64).wrapping_mul(FNV_PRIME);
                h = (h ^ c.n_pattern_loop_count as u64).wrapping_mul(FNV_PRIME);
            }
        }
        LoopState(h)
    }
    fn has_loops(&self) -> bool {
        self.0 != FNV_BASIS
    }
}

#[derive(Clone, Debug, Default)]
pub struct RowVisitor {
    visited_rows: Vec<Vec<bool>>,
    loop_states: BTreeMap<(OrderIndex, RowIndex), Vec<LoopState>>,
    rows_spent_in_loops: RowIndex,
}

fn rows_vector_size(m: &Module, pat: PatternIndex) -> RowIndex {
    if m.is_valid_pat(pat) { m.patterns[pat as usize].rows } else { 1 }
}

impl RowVisitor {
    pub fn new(m: &Module) -> Self {
        let mut v = RowVisitor::default();
        v.initialize(m, true);
        v
    }

    /// `Initialize`.
    pub fn initialize(&mut self, m: &Module, reset: bool) {
        let end_order = m.order_length_tail_trimmed() as usize;
        let mut reserve_loop_states = true;
        self.visited_rows.resize(end_order, Vec::new());
        if reset {
            reserve_loop_states = self.loop_states.is_empty();
            for v in self.loop_states.values_mut() {
                v.clear();
            }
            self.rows_spent_in_loops = 0;
        }
        let nc = m.num_channels();
        let mut visited_patterns: Vec<OrderIndex> = vec![ORDERINDEX_INVALID; m.patterns.len()];
        let mut loop_count: Vec<u8> = Vec::new();
        for ord in 0..end_order {
            let pat = m.order[ord];
            let num_rows = rows_vector_size(m, pat) as usize;
            let vr = &mut self.visited_rows[ord];
            let old_len = vr.len();
            if reset {
                vr.clear();
                vr.resize(num_rows, false);
            } else {
                vr.resize(num_rows, false);
            }
            if !reserve_loop_states || !m.is_valid_order(ord as OrderIndex) {
                continue;
            }
            let start_row = (if reset { 0 } else { old_len.min(num_rows) }).min(num_rows);
            if visited_patterns[pat as usize] != ORDERINDEX_INVALID {
                let src = visited_patterns[pat as usize];
                let keys: Vec<RowIndex> = self
                    .loop_states
                    .range((src, start_row as RowIndex)..(src, num_rows as RowIndex))
                    .map(|(k, _)| k.1)
                    .collect();
                for r in keys {
                    self.loop_states.insert((ord as OrderIndex, r), Vec::new());
                }
                continue;
            }
            let p = &m.patterns[pat as usize];
            loop_count.clear();
            loop_count.resize(nc, 0);
            let mut i = num_rows;
            while i != start_row {
                let row = i - 1;
                let mut max_loop_states: u32 = 1;
                let mut chn = 0;
                while chn < nc && max_loop_states < 16 {
                    let c = p.cell(row as RowIndex, chn, nc);
                    let mut count = loop_count[chn];
                    if (c.command == CMD_S3MCMDEX && (c.param & 0xF0) == 0xB0) || (c.command == CMD_MODCMDEX && (c.param & 0xF0) == 0x60) {
                        loop_count[chn] = c.param & 0x0F;
                        if loop_count[chn] != 0 {
                            count = loop_count[chn];
                        }
                    }
                    if count != 0 {
                        max_loop_states *= count as u32 + 1;
                    }
                    chn += 1;
                }
                if max_loop_states > 1 {
                    self.loop_states.insert((ord as OrderIndex, row as RowIndex), Vec::new());
                }
                i -= 1;
            }
            if start_row == 0 {
                visited_patterns[pat as usize] = ord as OrderIndex;
            }
        }
    }

    /// Same repeated-row limit used by libopenmpt's duration scanner.
    pub fn too_complex(&self) -> bool {
        self.rows_spent_in_loops >= 32_768
    }

    /// `Visit`: marks the row as visited; true if it was visited before.
    pub fn visit(&mut self, m: &Module, ord: OrderIndex, row: RowIndex, chns: &[ModChannel], ignore_row: bool) -> bool {
        if ord as usize >= m.order.len() || row >= rows_vector_size(m, m.order[ord as usize]) {
            return false;
        }
        if ord as usize >= self.visited_rows.len() || row as usize >= self.visited_rows[ord as usize].len() {
            self.initialize(m, false);
            if ord as usize >= self.visited_rows.len() {
                return false;
            }
        }
        let new_state = LoopState::new(&chns[..m.num_channels()], ignore_row);
        let key = (ord, row);
        let old_had_loops = self.loop_states.get(&key).is_some_and(|v| !v.is_empty());
        let new_has_loops = new_state.has_loops();
        let was_visited = self.visited_rows[ord as usize][row as usize];
        if !old_had_loops && !new_has_loops && was_visited {
            return true;
        }
        if old_had_loops && self.loop_states[&key].contains(&new_state) {
            return true;
        }
        if new_has_loops {
            self.rows_spent_in_loops += 1;
        }
        if old_had_loops || new_has_loops {
            let entry = self.loop_states.entry(key).or_default();
            if !old_had_loops && was_visited {
                entry.push(LoopState(FNV_BASIS));
            }
            entry.push(new_state);
        }
        self.visited_rows[ord as usize][row as usize] = true;
        false
    }

    /// `GetFirstUnvisitedRow`.
    pub fn first_unvisited_row(&self, m: &Module, only_unplayed_patterns: bool) -> Option<(OrderIndex, RowIndex)> {
        let end_order = m.order_length_tail_trimmed() as usize;
        for o in 0..end_order {
            if !m.is_valid_order(o as OrderIndex) {
                continue;
            }
            if o >= self.visited_rows.len() {
                return Some((o as OrderIndex, 0));
            }
            let vr = &self.visited_rows[o];
            let mut first = 0;
            while first < vr.len() {
                if vr[first] == only_unplayed_patterns {
                    break;
                }
                first += 1;
            }
            if only_unplayed_patterns && first == vr.len() {
                return Some((o as OrderIndex, 0));
            } else if !only_unplayed_patterns {
                if first < vr.len() {
                    return Some((o as OrderIndex, first as RowIndex));
                }
                if vr.len() < m.patterns[m.order[o] as usize].rows as usize {
                    return Some((o as OrderIndex, vr.len() as RowIndex));
                }
            }
        }
        None
    }
}
