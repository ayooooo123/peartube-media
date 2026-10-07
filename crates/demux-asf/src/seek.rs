// Ported from FFmpeg libavformat/seek.c (commit 2da55bf): the stream index
// (ff_add_index_entry, ff_index_search_timestamp) and the timestamp
// bisection (ff_seek_frame_binary, ff_gen_search, ff_find_last_ts) behind
// asfdec_f.c asf_read_seek.
// License: LGPL-2.1-or-later
//
// Every seek here uses AVSEEK_FLAG_BACKWARD, the flag `ffmpeg -ss` and
// `ffprobe -read_intervals` pass for a target after 0: land at or before
// the target. Timestamps never wrap (ff_wrap_timestamp is not modelled).

use oxideav_core::{Error, Result};

/// avformat's max_index_size (1 MiB) over sizeof(AVIndexEntry) (24
/// bytes). FFmpeg does not reduce an ASF index; this one keeps every
/// other entry once full, which bounds the memory a hostile Simple Index
/// Object claims.
const MAX_INDEX_ENTRIES: usize = (1 << 20) / 24;

/// Bisection steps before a seek gives up. ff_gen_search narrows its
/// range on every step, so a real file converges in a few dozen; this
/// only bounds hostile input.
const MAX_SEARCH_STEPS: usize = 4096;

/// One AVIndexEntry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IndexEntry {
    pub pos: i64,
    pub timestamp: i64,
    pub min_distance: i64,
}

/// A stream's index (FFStream.index_entries), sorted by timestamp. ASF
/// adds key frames only (AVINDEX_KEYFRAME).
#[derive(Default)]
pub(crate) struct Index {
    entries: Vec<IndexEntry>,
}

impl Index {
    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    /// av_add_index_entry of a key frame: insert by timestamp, replacing
    /// an entry of the same timestamp.
    pub fn add(&mut self, pos: i64, timestamp: i64, size: i64, mut distance: i64) {
        if !(0..=0x3FFF_FFFF).contains(&size) {
            return;
        }
        if self.entries.len() >= MAX_INDEX_ENTRIES {
            self.entries = self.entries.iter().step_by(2).copied().collect();
        }
        let at = match search(&self.entries, timestamp, false) {
            None => {
                self.entries.push(IndexEntry { pos, timestamp, min_distance: distance });
                return;
            }
            Some(at) => at,
        };
        let ie = self.entries[at];
        if ie.timestamp != timestamp {
            if ie.timestamp <= timestamp {
                return;
            }
            self.entries.insert(at, ie);
        } else if ie.pos == pos && distance < ie.min_distance {
            // do not reduce the distance
            distance = ie.min_distance;
        }
        self.entries[at] = IndexEntry { pos, timestamp, min_distance: distance };
    }

    /// av_index_search_timestamp, AVSEEK_FLAG_BACKWARD where `backward`.
    pub fn search(&self, wanted: i64, backward: bool) -> Option<usize> {
        search(&self.entries, wanted, backward)
    }

    /// ff_seek_frame_binary before it bisects: the entries around
    /// `target` bound the search.
    pub fn bounds(&self, target: i64) -> Bounds {
        let mut bounds = Bounds { pos_min: 0, pos_max: 0, pos_limit: -1, ts_min: None, ts_max: None };
        if !self.entries.is_empty() {
            let e = self.entries[self.search(target, true).unwrap_or(0)];
            if e.timestamp <= target || e.pos == e.min_distance {
                bounds.pos_min = e.pos;
                bounds.ts_min = Some(e.timestamp);
            }
            if let Some(i) = self.search(target, false) {
                let e = self.entries[i];
                bounds.pos_max = e.pos;
                bounds.ts_max = Some(e.timestamp);
                bounds.pos_limit = e.pos - e.min_distance;
            }
        }
        bounds
    }
}

/// ff_index_search_timestamp over key-frame entries.
fn search(entries: &[IndexEntry], wanted: i64, backward: bool) -> Option<usize> {
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
    let m = if backward { a } else { b };
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

/// A format's read_timestamp: the timestamp of the first key packet of
/// the seek stream found from `*pos` on, `*pos` moved to that packet;
/// `None` (AV_NOPTS_VALUE) when there is none.
pub(crate) type ReadTimestamp<'a> = dyn FnMut(&mut i64) -> Result<Option<i64>> + 'a;

/// The search range ff_gen_search starts from: positions, and the
/// timestamps at its ends where known.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bounds {
    pub pos_min: i64,
    pub pos_max: i64,
    pub pos_limit: i64,
    pub ts_min: Option<i64>,
    pub ts_max: Option<i64>,
}

/// ff_gen_search with AVSEEK_FLAG_BACKWARD: `(pos, ts)` of the last key
/// packet found at or before `target`, `None` where FFmpeg returns -1.
/// ff_seek_frame_binary is this over [`Index::bounds`].
pub(crate) fn gen_search(
    target: i64,
    bounds: Bounds,
    data_offset: i64,
    file_size: i64,
    read_timestamp: &mut ReadTimestamp<'_>,
) -> Result<Option<(i64, i64)>> {
    let Bounds { mut pos_min, mut pos_max, mut pos_limit, ts_min, ts_max } = bounds;
    let mut ts_min = match ts_min {
        Some(ts) => ts,
        None => {
            pos_min = data_offset;
            match read_timestamp(&mut pos_min)? {
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
            return Err(Error::invalid("asf: timestamp search does not converge"));
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
        let ts = read_timestamp(&mut pos)?;
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
        let ts = read_timestamp(&mut pos_max)?;
        step = step.saturating_add(step);
        if ts.is_some() || limit.saturating_mul(2) <= step {
            break ts;
        }
    };
    let Some(mut ts_max) = ts_max else { return Ok(None) };
    let mut steps = 0;
    loop {
        let mut tmp_pos = pos_max + 1;
        let Some(tmp_ts) = read_timestamp(&mut tmp_pos)? else { break };
        steps += 1;
        if tmp_pos <= pos_max || steps > MAX_SEARCH_STEPS * 64 {
            return Err(Error::invalid("asf: the last timestamp search does not advance"));
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

    #[test]
    fn backward_search_takes_the_last_entry_at_or_before() {
        let mut index = Index::default();
        for (pos, ts) in [(100, 0), (200, 1000), (300, 2000)] {
            index.add(pos, ts, 0, 0);
        }
        assert_eq!(index.search(1500, true), Some(1));
        assert_eq!(index.search(1500, false), Some(2));
        assert_eq!(index.search(-1, true), None);
        assert_eq!(index.search(2001, false), None);
    }

    #[test]
    fn bisection_lands_on_the_last_key_at_or_before() {
        // Key packets every 1000 bytes, 100 ms apart, from offset 50.
        let keys: Vec<(i64, i64)> = (0..50).map(|i| (50 + 1000 * i, 100 * i)).collect();
        let mut read = |pos: &mut i64| -> Result<Option<i64>> {
            Ok(keys.iter().find(|k| k.0 >= *pos).map(|k| {
                *pos = k.0;
                k.1
            }))
        };
        let bounds = Index::default().bounds(2345);
        assert_eq!(gen_search(2345, bounds, 50, 50_000, &mut read).unwrap(), Some((23_050, 2300)));
        assert_eq!(gen_search(-5, bounds, 50, 50_000, &mut read).unwrap(), Some((50, 0)));
        assert_eq!(gen_search(99_999, bounds, 50, 50_000, &mut read).unwrap(), Some((49_050, 4900)));
    }
}
