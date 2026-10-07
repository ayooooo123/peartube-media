// Ported from FFmpeg libavformat/seek.c (commit 2da55bf): the stream index
// (ff_reduce_index, ff_add_index_entry, ff_index_search_timestamp) and
// the timestamp bisection (ff_seek_frame_binary, ff_gen_search,
// ff_find_last_ts) behind this crate's seeks.
// License: LGPL-2.1-or-later
//
// Every seek here uses AVSEEK_FLAG_BACKWARD, the flag `ffmpeg -ss` and
// `ffprobe -read_intervals` pass: land at or before the target.
// Timestamps are absolute (FFmpeg removes RELATIVE_TS_BASE before
// indexing) and never wrap (ff_wrap_timestamp is not modelled).

use oxideav_core::{Error, Result};

/// avformat's max_index_size (1 MiB) over sizeof(AVIndexEntry) (24 bytes).
pub(crate) const MAX_INDEX_ENTRIES: usize = (1 << 20) / 24;

/// Bisection steps before a seek gives up. ff_gen_search narrows its
/// range on every step, so a real file converges in a few dozen; this
/// only bounds hostile input.
const MAX_SEARCH_STEPS: usize = 4096;

/// One AVIndexEntry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IndexEntry {
    pub pos: i64,
    pub timestamp: i64,
    /// AVIndexEntry.size, which a demuxer may use for its own resume
    /// state (VOC: the bytes left of the block).
    pub size: i64,
    pub min_distance: i64,
    pub keyframe: bool,
}

/// A stream's index (FFStream.index_entries), sorted by timestamp.
#[derive(Default)]
pub(crate) struct Index {
    entries: Vec<IndexEntry>,
}

impl Index {
    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    /// ff_reduce_index: a full index keeps every other entry.
    fn reduce(&mut self) {
        if self.entries.len() >= MAX_INDEX_ENTRIES {
            self.entries = self.entries.iter().step_by(2).copied().collect();
        }
    }

    /// ff_reduce_index then ff_add_index_entry: insert by timestamp,
    /// replacing an entry of the same timestamp. False where FFmpeg
    /// rejects the entry. Demuxers whose FFmpeg counterpart never reduces
    /// (VOC) differ only past MAX_INDEX_ENTRIES, which bounds the memory.
    pub fn add(&mut self, pos: i64, timestamp: i64, size: i64, mut distance: i64, keyframe: bool) -> bool {
        if !(0..=0x3FFF_FFFF).contains(&size) {
            return false;
        }
        self.reduce();
        let at = match search(&self.entries, timestamp, false, true) {
            None => {
                self.entries.push(IndexEntry { pos, timestamp, size, min_distance: distance, keyframe });
                return true;
            }
            Some(at) => at,
        };
        let ie = self.entries[at];
        if ie.timestamp != timestamp {
            if ie.timestamp <= timestamp {
                return false;
            }
            self.entries.insert(at, ie);
        } else if ie.pos == pos && distance < ie.min_distance {
            distance = ie.min_distance;
        }
        self.entries[at] = IndexEntry { pos, timestamp, size, min_distance: distance, keyframe };
        true
    }

    /// av_index_search_timestamp with AVSEEK_FLAG_BACKWARD (`backward`)
    /// and without AVSEEK_FLAG_ANY.
    pub fn search(&self, wanted: i64, backward: bool) -> Option<usize> {
        search(&self.entries, wanted, backward, false)
    }
}

/// ff_index_search_timestamp over entries no demuxer here flags
/// AVINDEX_DISCARD_FRAME.
fn search(entries: &[IndexEntry], wanted: i64, backward: bool, any: bool) -> Option<usize> {
    let n = entries.len() as isize;
    let (mut a, mut b) = (-1isize, n);
    // Optimize appending index entries at the end.
    if n > 0 && entries[(n - 1) as usize].timestamp < wanted {
        a = n - 1;
    }
    while b - a > 1 {
        let m = (a + b) >> 1;
        let timestamp = entries[m as usize].timestamp;
        if timestamp >= wanted {
            b = m;
        }
        if timestamp <= wanted {
            a = m;
        }
    }
    let mut m = if backward { a } else { b };
    if !any {
        while m >= 0 && m < n && !entries[m as usize].keyframe {
            m += if backward { -1 } else { 1 };
        }
    }
    (m >= 0 && m < n).then_some(m as usize)
}

/// av_rescale(a, b, c): a * b / c rounded to nearest, ties away from zero;
/// i64::MIN where FFmpeg's av_rescale_rnd refuses (b < 0, c <= 0).
pub(crate) fn rescale(a: i64, b: i64, c: i64) -> i64 {
    if c <= 0 || b < 0 {
        return i64::MIN;
    }
    let num = i128::from(a) * i128::from(b);
    let half = i128::from(c) / 2;
    let q = if num < 0 { -((-num + half) / i128::from(c)) } else { (num + half) / i128::from(c) };
    i64::try_from(q).unwrap_or(if q < 0 { i64::MIN } else { i64::MAX })
}

/// A format's read_timestamp: the timestamp of the first packet of the
/// seek stream found from `*pos` on, `*pos` moved to that packet; `None`
/// (AV_NOPTS_VALUE) when there is none. The second argument is pos_limit.
pub(crate) type ReadTimestamp<'a> = dyn FnMut(&mut i64, i64) -> Result<Option<i64>> + 'a;

/// ff_seek_frame_binary up to its avio_seek: the index bounds the search,
/// ff_gen_search finds the position. `(pos, ts)` to resume at, `None`
/// where FFmpeg's seek fails.
pub(crate) fn seek_frame_binary(
    index: &Index,
    target: i64,
    data_offset: i64,
    file_size: i64,
    read_timestamp: &mut ReadTimestamp<'_>,
) -> Result<Option<(i64, i64)>> {
    let (mut pos_min, mut pos_max, mut pos_limit) = (0, 0, -1);
    let (mut ts_min, mut ts_max) = (None, None);
    let entries = index.entries();
    if !entries.is_empty() {
        let e = entries[index.search(target, true).unwrap_or(0)];
        if e.timestamp <= target || e.pos == e.min_distance {
            pos_min = e.pos;
            ts_min = Some(e.timestamp);
        }
        if let Some(i) = index.search(target, false) {
            let e = entries[i];
            pos_max = e.pos;
            ts_max = Some(e.timestamp);
            pos_limit = pos_max - e.min_distance;
        }
    }
    gen_search(target, pos_min, pos_max, pos_limit, ts_min, ts_max, data_offset, file_size, read_timestamp)
}

/// ff_gen_search with AVSEEK_FLAG_BACKWARD: `(pos, ts)` of the last
/// packet found at or before `target`, `None` where FFmpeg returns -1.
#[allow(clippy::too_many_arguments)]
pub(crate) fn gen_search(
    target: i64,
    mut pos_min: i64,
    mut pos_max: i64,
    mut pos_limit: i64,
    ts_min: Option<i64>,
    ts_max: Option<i64>,
    data_offset: i64,
    file_size: i64,
    read_timestamp: &mut ReadTimestamp<'_>,
) -> Result<Option<(i64, i64)>> {
    let mut ts_min = match ts_min {
        Some(ts) => ts,
        None => {
            pos_min = data_offset;
            match read_timestamp(&mut pos_min, i64::MAX)? {
                Some(ts) => ts,
                None => return Ok(None),
            }
        }
    };
    if ts_min >= target {
        return Ok(Some((pos_min, ts_min)));
    }
    let mut ts_max = match ts_max {
        Some(ts) => ts,
        None => match find_last_ts(file_size, read_timestamp)? {
            Some((ts, pos)) => {
                pos_max = pos;
                pos_limit = pos_max;
                ts
            }
            None => return Ok(None),
        },
    };
    if ts_max <= target {
        return Ok(Some((pos_max, ts_max)));
    }
    let mut no_change = 0;
    let mut steps = 0;
    while pos_min < pos_limit {
        steps += 1;
        if steps > MAX_SEARCH_STEPS || pos_limit > pos_max {
            return Err(Error::invalid("seek: timestamp search does not converge"));
        }
        let guess = match no_change {
            // interpolate position (better than dichotomy)
            0 => rescale(target - ts_min, pos_max - pos_min, ts_max - ts_min)
                .saturating_add(pos_min)
                .saturating_sub(pos_max - pos_limit),
            // bisection if interpolation did not change min / max pos last time
            1 => (pos_min + pos_limit) >> 1,
            // linear search if bisection failed
            _ => pos_min,
        };
        let start_pos = if guess <= pos_min { pos_min + 1 } else { guess.min(pos_limit) };
        let mut pos = start_pos;
        let ts = read_timestamp(&mut pos, i64::MAX)?;
        no_change = if pos == pos_max { no_change + 1 } else { 0 };
        // "read_timestamp() failed in the middle"
        let Some(ts) = ts else { return Ok(None) };
        if target <= ts {
            pos_limit = start_pos - 1;
            pos_max = pos;
            ts_max = ts;
        }
        if target >= ts {
            pos_min = pos;
            ts_min = ts;
        }
    }
    Ok(Some((pos_min, ts_min)))
}

/// ff_find_last_ts: the last timestamp of the stream, searched backwards
/// from the end in doubling steps, then forwards packet by packet.
fn find_last_ts(file_size: i64, read_timestamp: &mut ReadTimestamp<'_>) -> Result<Option<(i64, i64)>> {
    let mut step: i64 = 1024;
    let mut pos_max = file_size - 1;
    let ts_max = loop {
        let limit = pos_max;
        pos_max = (pos_max - step).max(0);
        let ts = read_timestamp(&mut pos_max, limit)?;
        step = step.saturating_add(step);
        if ts.is_some() || limit.saturating_mul(2) <= step {
            break ts;
        }
    };
    let Some(mut ts_max) = ts_max else { return Ok(None) };
    let mut steps = 0;
    loop {
        let mut tmp_pos = pos_max + 1;
        let Some(tmp_ts) = read_timestamp(&mut tmp_pos, i64::MAX)? else { break };
        steps += 1;
        if tmp_pos <= pos_max || steps > MAX_SEARCH_STEPS * 64 {
            return Err(Error::invalid("seek: the last timestamp search does not advance"));
        }
        ts_max = tmp_ts;
        pos_max = tmp_pos;
        if tmp_pos >= file_size {
            break;
        }
    }
    Ok(Some((ts_max, pos_max)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(ts: &[(i64, bool)]) -> Index {
        let mut index = Index::default();
        for (n, &(t, key)) in ts.iter().enumerate() {
            assert!(index.add(n as i64 * 100, t, 0, 0, key));
        }
        index
    }

    #[test]
    fn backward_search_takes_the_last_keyframe_at_or_before() {
        let index = index(&[(0, true), (10, false), (20, true), (30, false)]);
        assert_eq!(index.search(25, true), Some(2));
        assert_eq!(index.search(20, true), Some(2));
        assert_eq!(index.search(19, true), Some(0));
        assert_eq!(index.search(-1, true), None);
        assert_eq!(index.search(25, false), None);
        assert_eq!(index.search(5, false), Some(2));
    }

    #[test]
    fn an_equal_timestamp_replaces_and_an_older_one_inserts() {
        let mut index = index(&[(0, true), (20, true)]);
        assert!(index.add(700, 20, 0, 0, true));
        assert!(index.add(500, 10, 0, 0, true));
        let got: Vec<(i64, i64)> = index.entries().iter().map(|e| (e.pos, e.timestamp)).collect();
        assert_eq!(got, [(0, 0), (500, 10), (700, 20)]);
    }

    #[test]
    fn a_full_index_keeps_every_other_entry() {
        let mut index = Index::default();
        for n in 0..MAX_INDEX_ENTRIES as i64 {
            index.add(n, n, 0, 0, true);
        }
        index.add(-1, MAX_INDEX_ENTRIES as i64, 0, 0, true);
        assert_eq!(index.entries().len(), MAX_INDEX_ENTRIES / 2 + 1);
        assert_eq!(index.entries()[1].timestamp, 2);
    }

    /// Packets every 10 bytes with timestamp pos / 10 * 3: the search
    /// lands on the last one at or before the target.
    #[test]
    fn bisection_lands_on_the_last_packet_at_or_before() {
        let mut read = |pos: &mut i64, _limit: i64| -> Result<Option<i64>> {
            let at = (*pos + 9) / 10 * 10;
            if at >= 1000 {
                return Ok(None);
            }
            *pos = at;
            Ok(Some(at / 10 * 3))
        };
        for target in [0, 1, 3, 100, 149, 296, 297, 500] {
            let got = seek_frame_binary(&Index::default(), target, 0, 1000, &mut read).unwrap();
            let want = (target.min(297) / 3 * 10, target.min(297) / 3 * 3);
            assert_eq!(got, Some(want), "target {target}");
        }
    }
}
