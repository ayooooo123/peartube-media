// Ported from FFmpeg libavformat/mxfdec.c (commit 2da55bf): mxf_get_sorted_table_segments,
// mxf_absolute_bodysid_offset, mxf_essence_container_end,
// mxf_edit_unit_absolute_offset, mxf_compute_ptses_fake_index,
// mxf_compute_index_tables; av_rescale_q from libavutil/mathematics.c.
// License: LGPL-2.1-or-later

//! Index tables: segments sorted per IndexSID, edit unit to file offset,
//! and the PTS of each edit unit in stored order.

use oxideav_core::{Error, Result};

use crate::sets::{IndexSegment, Partition};

/// A rational as FFmpeg's AVRational: (num, den).
pub type Q = (i32, i32);

/// av_rescale_rnd(a, b, c, AV_ROUND_NEAR_INF) in exact 128-bit arithmetic;
/// i64::MIN where FFmpeg returns INT64_MIN (b < 0, c <= 0, overflow).
pub fn rescale(a: i64, b: i64, c: i64) -> i64 {
    if b < 0 || c <= 0 {
        return i64::MIN;
    }
    let neg = a < 0;
    let a = i128::from(a.max(-i64::MAX)).abs();
    let r = (a * i128::from(b) + i128::from(c) / 2) / i128::from(c);
    if r > i128::from(i64::MAX) {
        return i64::MIN;
    }
    if neg { -(r as i64) } else { r as i64 }
}

/// av_rescale_q(a, bq, cq).
pub fn rescale_q(a: i64, bq: Q, cq: Q) -> i64 {
    rescale(a, i64::from(bq.0) * i64::from(cq.1), i64::from(cq.0) * i64::from(bq.1))
}

/// av_inv_q.
pub fn inv(q: Q) -> Q {
    (q.1, q.0)
}

/// An MXFIndexTable.
#[derive(Clone, Debug, Default)]
pub struct IndexTable {
    pub index_sid: i32,
    pub body_sid: i32,
    /// Number of PTSes, 0 where the table has no usable TemporalOffsets.
    pub nb_ptses: usize,
    /// DTS = EditUnit + first_dts.
    pub first_dts: i64,
    /// Edit unit (stored order) to PTS; None for AV_NOPTS_VALUE.
    pub ptses: Vec<Option<i64>>,
    /// Sorted by IndexStartPosition.
    pub segments: Vec<IndexSegment>,
    /// The fake index's key flags, in display order.
    pub fake_index_key: Vec<bool>,
    /// Display order to stored order offsets.
    pub offsets: Vec<i8>,
}

/// mxf_get_sorted_table_segments: segments with an EditUnitByteCount or
/// entries, sorted by (BodySID, IndexSID, IndexStartPosition), duplicates
/// dropped (of equal starts, the longest IndexDuration kept).
pub fn sorted_table_segments(all: &[IndexSegment]) -> Vec<IndexSegment> {
    let unsorted: Vec<&IndexSegment> = all.iter().filter(|s| s.edit_unit_byte_count != 0 || s.nb_index_entries() != 0).collect();
    let mut sorted = Vec::new();
    // FFmpeg keeps these keys in int (IndexStartPosition truncated).
    let (mut last_body_sid, mut last_index_sid, mut last_index_start) = (-1i32, -1i32, -1i32);
    for i in 0..unsorted.len() {
        let mut best: Option<usize> = None;
        let (mut best_body_sid, mut best_index_sid, mut best_index_start) = (-1i32, -1i32, -1i32);
        let mut best_index_duration = 0u64;
        for (j, s) in unsorted.iter().enumerate() {
            let start = s.index_start_position as i32;
            let after_last = i == 0
                || s.body_sid > last_body_sid
                || (s.body_sid == last_body_sid && s.index_sid > last_index_sid)
                || (s.body_sid == last_body_sid && s.index_sid == last_index_sid && start > last_index_start);
            let better = best.is_none()
                || s.body_sid < best_body_sid
                || (s.body_sid == best_body_sid && s.index_sid < best_index_sid)
                || (s.body_sid == best_body_sid && s.index_sid == best_index_sid && start < best_index_start)
                || (s.body_sid == best_body_sid
                    && s.index_sid == best_index_sid
                    && start == best_index_start
                    && s.index_duration > best_index_duration);
            if after_last && better {
                best = Some(j);
                best_body_sid = s.body_sid;
                best_index_sid = s.index_sid;
                best_index_start = start;
                best_index_duration = s.index_duration;
            }
        }
        // No suitable entry found: done.
        let Some(best) = best else { break };
        sorted.push(unsorted[best].clone());
        last_body_sid = best_body_sid;
        last_index_sid = best_index_sid;
        last_index_start = best_index_start;
    }
    sorted
}

/// mxf_absolute_bodysid_offset: the file offset of `offset` in the essence
/// container `body_sid`, and the partition holding it.
pub fn absolute_bodysid_offset(partitions: &[Partition], body_sid: i32, offset: i64) -> Result<(i64, usize)> {
    if offset < 0 {
        return Err(Error::invalid("mxf: negative essence offset"));
    }
    let (mut a, mut b): (isize, isize) = (-1, partitions.len() as isize);
    while b - a > 1 {
        let m0 = (a + b) >> 1;
        let mut m = m0;
        while m < b && partitions[m as usize].body_sid != body_sid {
            m += 1;
        }
        if m < b && partitions[m as usize].body_offset <= offset {
            a = m;
        } else {
            b = m0;
        }
    }
    if a >= 0 {
        let p = &partitions[a as usize];
        if p.essence_length == 0 || p.essence_length > offset - p.body_offset {
            return Ok((p.essence_offset + (offset - p.body_offset), a as usize));
        }
    }
    Err(Error::invalid("mxf: failed to find the absolute offset of an essence offset"))
}

/// mxf_essence_container_end: where the essence container `body_sid`
/// ends, 0 where unknown.
pub fn essence_container_end(partitions: &[Partition], body_sid: i32) -> i64 {
    match partitions.iter().rev().find(|p| p.body_sid == body_sid) {
        Some(p) if p.essence_length != 0 => p.essence_offset + p.essence_length,
        _ => 0,
    }
}

/// mxf_edit_unit_absolute_offset: (edit unit landed on in the track's
/// edit rate, file offset, partition) of `edit_unit`. `nag` only changes
/// FFmpeg's logging.
pub fn edit_unit_absolute_offset(
    partitions: &[Partition],
    table: &IndexTable,
    edit_unit: i64,
    edit_rate: Q,
) -> Result<(i64, i64, usize)> {
    let Some(first_segment) = table.segments.first() else {
        return Err(Error::invalid("mxf: index table without segments"));
    };
    let last_segment = &table.segments[table.segments.len() - 1];
    let mut edit_unit = rescale_q(edit_unit, first_segment.index_edit_rate, edit_rate);
    let index_end = (last_segment.index_start_position as i64).saturating_add(last_segment.index_duration as i64);
    // FFMAX of an int64_t and FFmpeg's uint64_t IndexStartPosition compares
    // unsigned.
    let clamped = edit_unit.min(index_end);
    let first_start = first_segment.index_start_position;
    edit_unit = if clamped as u64 > first_start { clamped } else { first_start as i64 };
    if edit_unit < 0 {
        return Err(Error::unsupported("mxf: negative edit unit"));
    }
    let index_duration = index_end.saturating_sub(first_segment.index_start_position as i64);
    let nb = table.segments.len() as i64;
    let mut i: i64 = 0;
    if index_duration > 0 && edit_unit <= i64::MAX / nb {
        i = (nb * edit_unit / index_duration).clamp(0, nb - 1);
    }
    let mut dir = 0;
    while i >= 0 && i < nb {
        let s = &table.segments[i as usize];
        // IndexStartPosition and IndexDuration are uint64_t in FFmpeg: the
        // range test is unsigned and the end wraps; edit_unit >= 0 here.
        let (start, eu) = (s.index_start_position, edit_unit as u64);
        if start <= eu && eu < start.wrapping_add(s.index_duration) {
            let mut index = (eu - start) as i64;
            let mut offset_temp = s.offset;
            if s.edit_unit_byte_count != 0 {
                let eubc = i64::from(s.edit_unit_byte_count);
                if index > i64::MAX / eubc || eubc * index > i64::MAX - offset_temp {
                    return Err(Error::invalid("mxf: essence offset overflows"));
                }
                offset_temp += eubc * index;
            } else {
                if s.nb_index_entries() as u64 == s.index_duration.wrapping_mul(2).wrapping_add(1) {
                    index = index.wrapping_mul(2); // Avid index
                }
                if index < 0 || index >= s.nb_index_entries() as i64 {
                    return Err(Error::invalid("mxf: index entry out of range"));
                }
                offset_temp = s.stream_offset_entries[index as usize] as i64;
            }
            let edit_unit_out = rescale_q(edit_unit, edit_rate, s.index_edit_rate);
            let (offset, partition) = absolute_bodysid_offset(partitions, table.body_sid, offset_temp)?;
            return Ok((edit_unit_out, offset, partition));
        } else if dir == 0 {
            dir = if eu < start { -1 } else { 1 };
        }
        i += dir;
    }
    Err(Error::invalid("mxf: edit unit not in the index table"))
}

/// mxf_compute_ptses_fake_index: nb_ptses is an int and IndexDuration a
/// uint64_t in FFmpeg, so its bound compares unsigned and its sums wrap.
fn compute_ptses_fake_index(table: &mut IndexTable) {
    let mut nb_ptses: i32 = 0;
    for s in &table.segments {
        if s.nb_index_entries() == 0 {
            return; // no TemporalOffsets
        }
        let d = s.index_duration;
        if d > (i32::MAX - nb_ptses) as u64 {
            return;
        }
        let n = s.nb_index_entries() as u64;
        if n != d && n != d.wrapping_add(1) && n != d.wrapping_mul(2).wrapping_add(1) {
            return;
        }
        nb_ptses += d as i32;
    }
    if nb_ptses <= 0 {
        return;
    }
    let nb = nb_ptses as usize;
    let mut ptses: Vec<Option<i64>> = vec![None; nb];
    let mut offsets = vec![0i8; nb];
    let mut flags = vec![false; nb];
    let mut max_temporal_offset: i8 = -128;
    let mut x: usize = 0;
    // Bucket sort x by x + TemporalOffset[x] into ptses; first_dts =
    // -max(TemporalOffset) makes DTS <= PTS.
    for s in &table.segments {
        let n_entries = s.nb_index_entries() as u64;
        let index_delta: usize = if n_entries == s.index_duration.wrapping_mul(2).wrapping_add(1) { 2 } else { 1 };
        let mut n = s.nb_index_entries();
        if n_entries == (index_delta as u64).wrapping_mul(s.index_duration).wrapping_add(1) {
            // Ignore the last entry: the size of the essence container in Avid.
            n -= 1;
        }
        let mut j = 0;
        while j < n {
            if x >= nb {
                break;
            }
            let offset = (i32::from(s.temporal_offset_entries[j]) / index_delta as i32) as i8;
            let index = x as i64 + i64::from(offset);
            flags[x] = s.flag_entries[j] & 0x30 == 0;
            if index >= 0 && (index as usize) < nb {
                offsets[x] = offset;
                ptses[index as usize] = Some(x as i64);
                max_temporal_offset = max_temporal_offset.max(offset);
            }
            j += index_delta;
            x += 1;
        }
    }
    // The fake index in display order.
    let mut fake_index_key = vec![false; nb];
    for x in 0..nb {
        if let Some(p) = ptses[x] {
            fake_index_key[p as usize] = flags[x];
        }
    }
    table.nb_ptses = nb;
    table.ptses = ptses;
    table.offsets = offsets;
    table.fake_index_key = fake_index_key;
    table.first_dts = -i64::from(max_temporal_offset);
}

/// mxf_compute_index_tables. `track_for` gives, for an IndexSID, the edit
/// rate and duration of the first stream's track indexed by it.
pub fn compute_index_tables(segments: &[IndexSegment], track_for: impl Fn(i32) -> Option<(Q, i64)>) -> Result<Vec<IndexTable>> {
    let sorted = sorted_table_segments(segments);
    if sorted.is_empty() {
        return Ok(Vec::new());
    }
    // Sanity check: one BodySID per IndexSID.
    for i in 1..sorted.len() {
        if sorted[i - 1].index_sid == sorted[i].index_sid && sorted[i - 1].body_sid != sorted[i].body_sid {
            return Err(Error::invalid("mxf: an IndexSID spans two BodySIDs"));
        }
    }
    let mut tables: Vec<IndexTable> = Vec::new();
    for s in sorted {
        match tables.last_mut() {
            Some(t) if t.index_sid == s.index_sid => t.segments.push(s),
            _ => tables.push(IndexTable { index_sid: s.index_sid, body_sid: s.body_sid, segments: vec![s], ..Default::default() }),
        }
    }
    for t in &mut tables {
        compute_ptses_fake_index(t);
        let track = track_for(t.index_sid);
        // Fix zero IndexDurations and compute segment offsets.
        let mut offset_temp: i64 = 0;
        for k in 0..t.segments.len() {
            let s = &mut t.segments[k];
            if s.index_edit_rate.0 == 0 || s.index_edit_rate.1 == 0 {
                if let Some((edit_rate, _)) = track {
                    s.index_edit_rate = edit_rate;
                }
            }
            s.offset = offset_temp;
            // EditUnitByteCount == 0 for VBR indexes, which use explicit
            // StreamOffsets. IndexDuration is a uint64_t in FFmpeg: the guard
            // compares unsigned, so huge durations fail it.
            let eubc = i64::from(s.edit_unit_byte_count);
            let product = (eubc as u64).wrapping_mul(s.index_duration);
            if eubc != 0 && (s.index_duration > (i64::MAX / eubc) as u64 || product > (i64::MAX - offset_temp) as u64) {
                return Err(Error::invalid("mxf: index segment offsets overflow"));
            }
            offset_temp += product as i64;
            if s.index_duration != 0 {
                continue;
            }
            let Some((_, original_duration)) = track else { break };
            // Assume the first stream's duration is reasonable; further
            // segments keep IndexDuration 0.
            s.index_duration = original_duration as u64;
            break;
        }
    }
    Ok(tables)
}
