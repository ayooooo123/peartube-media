// Ported from FFmpeg libavformat/seek.c (commit 2da55bf): the stream index
// (ff_reduce_index, ff_add_index_entry, ff_index_search_timestamp), the
// timestamp bisection (ff_seek_frame_binary, ff_gen_search,
// ff_find_last_ts), seek_frame_generic's read-on loop, and av_rescale.
// License: LGPL-2.1-or-later

//! FFmpeg's seek machinery for the workspace's demuxers, bounded for
//! hostile input.
//!
//! Every seek here uses AVSEEK_FLAG_BACKWARD, the flag `ffmpeg -ss` and
//! `ffprobe -read_intervals` pass for a target after 0: land at or
//! before the target. Timestamps never wrap (ff_wrap_timestamp is not
//! modelled).
//!
//! Bounds FFmpeg does not have:
//! - One [`Allowance`] per seek: 1,048,576 packets and 256 MiB read, over
//!   every scan and timestamp read the seek makes. Running out is
//!   [`Error::ResourceExhausted`], never the end of the input.
//! - At most [`MAX_SEARCH_STEPS`] bisection steps.
//! - Search arithmetic in 128 bits, and index positions outside the
//!   input not trusted as bounds.
//! - An index keeps [`MAX_INDEX_ENTRIES`] entries (see [`Reduce`]).

#![forbid(unsafe_code)]

use oxideav_core::{Error, Result};

/// avformat's max_index_size (1 MiB) over sizeof(AVIndexEntry) (24 bytes).
pub const MAX_INDEX_ENTRIES: usize = (1 << 20) / 24;

/// Bisection steps before a seek gives up. ff_gen_search narrows its
/// range on every step, so a real file converges in a few dozen; this
/// only bounds hostile input.
pub const MAX_SEARCH_STEPS: usize = 4096;

/// Packets one seek may read.
pub const SEEK_PACKETS: u64 = 1 << 20;
/// Bytes one seek may read.
pub const SEEK_BYTES: u64 = 256 << 20;

/// What one seek may read, over every scan and timestamp read it makes.
/// Inactive (spending nothing) outside a seek.
#[derive(Debug, Default)]
pub struct Allowance {
    left: Option<(u64, u64)>,
}

impl Allowance {
    /// A seek starts: [`SEEK_PACKETS`] packets and [`SEEK_BYTES`] bytes.
    pub fn start(&mut self) {
        self.left = Some((SEEK_PACKETS, SEEK_BYTES));
    }

    /// The seek is over: reading is no longer counted.
    pub fn stop(&mut self) {
        self.left = None;
    }

    /// Count `packets` read and `bytes` read or skipped; an error once
    /// either runs out (and from then on until [`Allowance::stop`]).
    pub fn spend(&mut self, packets: u64, bytes: u64) -> Result<()> {
        let Some((p, b)) = &mut self.left else { return Ok(()) };
        if packets > *p || bytes > *b {
            (*p, *b) = (0, 0);
            return Err(exhausted());
        }
        *p -= packets;
        *b -= bytes;
        Ok(())
    }
}

/// The error of a seek whose allowance ran out.
pub fn exhausted() -> Error {
    Error::resource_exhausted(format!(
        "seek: read allowance ({SEEK_PACKETS} packets, {} MiB) used up",
        SEEK_BYTES >> 20
    ))
}

/// Whether `error` ends a seek rather than a read: an exhausted
/// allowance.
pub fn is_exhausted(error: &Error) -> bool {
    matches!(error, Error::ResourceExhausted(_))
}

/// A read inside a seek, as FFmpeg's read loops take it: a value, or
/// `None` where av_read_frame or read_timestamp fails (the input's end, a
/// read error, damage). An exhausted allowance stays the seek's error.
pub fn soft<T>(read: Result<T>) -> Result<Option<T>> {
    match read {
        Ok(value) => Ok(Some(value)),
        Err(e) if is_exhausted(&e) => Err(e),
        Err(_) => Ok(None),
    }
}

/// seek_frame_generic's read-on loop (seek.c), after the demuxer resumed
/// at its last index entry: packets are read, `next` indexing the key
/// ones, until a key packet starts after `target`, more than 1000 others
/// did (FFmpeg's cut-off), or reading fails. `next` gives each packet's
/// key flag and dts.
pub fn read_on(target: i64, mut next: impl FnMut() -> Result<(bool, Option<i64>)>) -> Result<()> {
    let mut nonkey = 0;
    while let Some((key, dts)) = soft(next())? {
        if dts.is_some_and(|dts| dts > target) {
            if key {
                break;
            }
            nonkey += 1;
            if nonkey > 1001 {
                break;
            }
        }
    }
    Ok(())
}

/// One AVIndexEntry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub pos: i64,
    pub timestamp: i64,
    /// AVIndexEntry.size, which a demuxer may use for its own resume
    /// state (VOC: the bytes left of the block).
    pub size: i64,
    pub min_distance: i64,
    pub keyframe: bool,
}

/// How a full index ([`MAX_INDEX_ENTRIES`]) sheds entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reduce {
    /// ff_reduce_index, as FFmpeg reduces an AVFMT_GENERIC_INDEX stream's
    /// index and MPEG-PS's: every other entry stays (0, 2, 4, ...).
    #[default]
    Generic,
    /// An index FFmpeg keeps whole. Every other entry goes too, the last
    /// one staying, and [`Index::lossy`] tells a demuxer that lands on
    /// entries to recover the dropped ones from its input.
    Lossy,
}

/// A stream's index (FFStream.index_entries), sorted by timestamp.
#[derive(Clone, Debug, Default)]
pub struct Index {
    entries: Vec<IndexEntry>,
    reduce: Reduce,
    lossy: bool,
}

impl Index {
    pub fn new(reduce: Reduce) -> Self {
        Self { entries: Vec::new(), reduce, lossy: false }
    }

    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    /// Entries FFmpeg would have were dropped ([`Reduce::Lossy`]).
    pub fn lossy(&self) -> bool {
        self.lossy
    }

    fn shed(&mut self) {
        if self.entries.len() < MAX_INDEX_ENTRIES {
            return;
        }
        let last = self.entries.last().copied();
        self.entries = self.entries.iter().step_by(2).copied().collect();
        if self.reduce == Reduce::Lossy {
            self.lossy = true;
            if let Some(last) = last.filter(|l| self.entries.last() != Some(l)) {
                self.entries.push(last);
            }
        }
    }

    /// ff_reduce_index then ff_add_index_entry: insert by timestamp,
    /// replacing an entry of the same timestamp. False where FFmpeg
    /// rejects the entry.
    pub fn add(&mut self, pos: i64, timestamp: i64, size: i64, mut distance: i64, keyframe: bool) -> bool {
        if !(0..=0x3FFF_FFFF).contains(&size) {
            return false;
        }
        self.shed();
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
            // do not reduce the distance
            distance = ie.min_distance;
        }
        self.entries[at] = IndexEntry { pos, timestamp, size, min_distance: distance, keyframe };
        true
    }

    /// av_index_search_timestamp without AVSEEK_FLAG_ANY: the last key
    /// entry at or before `wanted` (`backward`), else the first at or
    /// after it.
    pub fn search(&self, wanted: i64, backward: bool) -> Option<usize> {
        search(&self.entries, wanted, backward, false)
    }

    /// ff_seek_frame_binary before it bisects (AVSEEK_FLAG_BACKWARD): the
    /// entries around `target` bound the search.
    pub fn bounds(&self, target: i64) -> Bounds {
        let mut bounds = Bounds::default();
        if !self.entries.is_empty() {
            let e = self.entries[self.search(target, true).unwrap_or(0)];
            if e.timestamp <= target || e.pos == e.min_distance {
                bounds.pos_min = e.pos;
                bounds.ts_min = Some(e.timestamp);
            }
            if let Some(i) = self.search(target, false) {
                let e = self.entries[i];
                if let Some(limit) = e.pos.checked_sub(e.min_distance) {
                    bounds.pos_max = e.pos;
                    bounds.ts_max = Some(e.timestamp);
                    bounds.pos_limit = limit;
                }
            }
        }
        bounds
    }
}

/// ff_index_search_timestamp over entries none of which is
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
/// i64::MIN where FFmpeg's av_rescale_rnd does: b < 0, c <= 0, or a
/// quotient past int64_t.
pub fn rescale(a: i64, b: i64, c: i64) -> i64 {
    rescale_wide(i128::from(a.max(-i64::MAX)), i128::from(b), i128::from(c)).unwrap_or(i64::MIN)
}

/// av_rescale_rnd(a, b, c, AV_ROUND_NEAR_INF) for a, b and c below 2^64
/// in magnitude, in exact 128-bit arithmetic: None where FFmpeg returns
/// INT64_MIN.
fn rescale_wide(a: i128, b: i128, c: i128) -> Option<i64> {
    if c <= 0 || b < 0 {
        return None;
    }
    let (b, c) = (b as u128, c as u128);
    let q = a.unsigned_abs().checked_mul(b)?.checked_add(c / 2)? / c;
    let q = i64::try_from(q).ok()?;
    Some(if a < 0 { -q } else { q })
}

/// A format's read_timestamp: the timestamp of the first packet of the
/// seek stream found from `*pos` on, `*pos` moved to that packet; `None`
/// (AV_NOPTS_VALUE) when there is none. The second argument is
/// pos_limit.
pub type ReadTimestamp<'a> = dyn FnMut(&mut i64, i64) -> Result<Option<i64>> + 'a;

/// The search range ff_gen_search starts from: positions, and the
/// timestamps at its ends where known.
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub pos_min: i64,
    pub pos_max: i64,
    pub pos_limit: i64,
    pub ts_min: Option<i64>,
    pub ts_max: Option<i64>,
}

impl Default for Bounds {
    fn default() -> Self {
        Self { pos_min: 0, pos_max: 0, pos_limit: -1, ts_min: None, ts_max: None }
    }
}

/// ff_gen_search with AVSEEK_FLAG_BACKWARD: `(pos, ts)` of the last
/// packet found at or before `target`, `None` where FFmpeg returns -1.
/// ff_seek_frame_binary is this over [`Index::bounds`]. Positions and
/// timestamps may be anything an input claims: differences are taken in
/// 128 bits, where FFmpeg's int64_t arithmetic overflows.
pub fn gen_search(
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
        let wide = i128::from;
        let guess: i128 = match no_change {
            // interpolate position (better than dichotomy)
            0 => match rescale_wide(wide(target) - wide(ts_min), wide(pos_max) - wide(pos_min), wide(ts_max) - wide(ts_min)) {
                Some(q) => wide(q) + wide(pos_min) - (wide(pos_max) - wide(pos_limit)),
                // av_rescale refused (INT64_MIN): at or before pos_min
                None => wide(pos_min),
            },
            // bisection if interpolation did not change min / max pos last time
            1 => (wide(pos_min) + wide(pos_limit)) >> 1,
            // linear search if bisection failed
            _ => wide(pos_min),
        };
        let start_pos = if guess <= wide(pos_min) { pos_min + 1 } else { guess.min(wide(pos_limit)) as i64 };
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
    let mut pos_max = file_size.saturating_sub(1);
    let ts_max = loop {
        let limit = pos_max;
        pos_max = pos_max.saturating_sub(step).max(0);
        let ts = read_timestamp(&mut pos_max, limit)?;
        step = step.saturating_add(step);
        if ts.is_some() || limit.saturating_mul(2) <= step {
            break ts;
        }
    };
    let Some(mut ts_max) = ts_max else { return Ok(None) };
    let mut steps = 0;
    loop {
        let Some(mut tmp_pos) = pos_max.checked_add(1) else { break };
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

    /// ff_reduce_index keeps entries 0, 2, 4, ...; a lossy index also
    /// keeps the last one, and says it lost entries.
    #[test]
    fn a_full_index_keeps_every_other_entry() {
        for reduce in [Reduce::Generic, Reduce::Lossy] {
            let mut index = Index::new(reduce);
            for n in 0..MAX_INDEX_ENTRIES as i64 {
                index.add(n, n, 0, 0, true);
            }
            assert!(!index.lossy());
            index.add(-1, MAX_INDEX_ENTRIES as i64, 0, 0, true);
            let kept: Vec<i64> = index.entries().iter().map(|e| e.timestamp).collect();
            assert_eq!(kept[..3], [0, 2, 4]);
            let last = MAX_INDEX_ENTRIES as i64 - 1;
            assert_eq!(kept.contains(&last), reduce == Reduce::Lossy, "{reduce:?}");
            assert_eq!(*kept.last().unwrap(), MAX_INDEX_ENTRIES as i64);
            assert_eq!(index.lossy(), reduce == Reduce::Lossy);
        }
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
            let got = gen_search(target, Index::default().bounds(target), 0, 1000, &mut read).unwrap();
            let want = (target.min(297) / 3 * 10, target.min(297) / 3 * 3);
            assert_eq!(got, Some(want), "target {target}");
        }
    }

    /// Packets every 10 bytes of a 1000-byte input, timestamp pos / 10 * 3;
    /// no timestamp outside the input, as a demuxer's read fails there.
    fn packets(pos: &mut i64, _limit: i64) -> Result<Option<i64>> {
        if !(0..1000).contains(pos) {
            return Ok(None);
        }
        let at = (*pos + 9) / 10 * 10;
        if at >= 1000 {
            return Ok(None);
        }
        *pos = at;
        Ok(Some(at / 10 * 3))
    }

    /// The review's trigger: index entries (0 at i64::MIN) and (1000 at
    /// 100), a seek to 500. FFmpeg's pos_max - pos_min overflows; here
    /// the interpolated guess is exact, falls before the input, and the
    /// search ends as ff_gen_search does when read_timestamp fails.
    #[test]
    fn positions_far_apart_interpolate_without_overflow() {
        let mut index = Index::default();
        index.add(i64::MIN, 0, 0, 0, true);
        index.add(100, 1000, 0, 0, true);
        let got = gen_search(500, index.bounds(500), 0, 1000, &mut packets);
        assert!(matches!(got, Ok(None)), "{got:?}");
    }

    /// Known timestamps at i64's ends: the interpolation stays exact and
    /// the search lands on the packet at the target.
    #[test]
    fn timestamps_at_the_extremes_still_land_on_the_target() {
        let bounds = Bounds { pos_min: 0, pos_max: 1000, pos_limit: 990, ts_min: Some(i64::MIN), ts_max: Some(i64::MAX) };
        assert_eq!(gen_search(150, bounds, 0, 1000, &mut packets).unwrap(), Some((500, 150)));
    }

    /// Bounds anywhere in i64, distances at the extremes: every search
    /// ends, and lands only on a packet it read or on a bound it was
    /// given, never on a position of its own making.
    #[test]
    fn hostile_bounds_land_only_on_read_packets_or_bounds() {
        let extremes = [i64::MIN, -1, 0, 100, 990, 1000, i64::MAX];
        for pos_a in extremes {
            for pos_b in extremes {
                for distance in [0, 10, i64::MIN + 1, i64::MAX] {
                    let mut index = Index::default();
                    index.add(pos_a, 0, 0, 0, true);
                    index.add(pos_b, 1000, 0, distance, true);
                    for target in [i64::MIN, -1, 0, 150, 299, 1000, i64::MAX] {
                        let bounds = index.bounds(target);
                        if let Ok(Some((pos, ts))) = gen_search(target, bounds, 0, 1000, &mut packets) {
                            let read = (0..1000).contains(&pos) && pos % 10 == 0 && ts == pos / 10 * 3;
                            let given = (Some(ts) == bounds.ts_min && pos == bounds.pos_min)
                                || (Some(ts) == bounds.ts_max && pos == bounds.pos_max);
                            assert!(read || given, "bounds {bounds:?}, target {target}: landed {pos}, {ts}");
                        }
                    }
                }
            }
        }
    }

    /// An allowance counts packets and bytes from start to stop; past
    /// either it fails, and keeps failing, with ResourceExhausted.
    #[test]
    fn an_allowance_runs_out_on_packets_or_bytes() {
        let mut a = Allowance::default();
        assert!(a.spend(u64::MAX, u64::MAX).is_ok(), "inactive");
        a.start();
        assert!(a.spend(SEEK_PACKETS - 1, SEEK_BYTES - 1).is_ok());
        assert!(a.spend(1, 1).is_ok());
        assert!(matches!(a.spend(1, 0), Err(Error::ResourceExhausted(_))));
        assert!(matches!(a.spend(0, 0), Ok(())), "nothing more to spend");
        a.start();
        assert!(matches!(a.spend(0, SEEK_BYTES + 1), Err(Error::ResourceExhausted(_))));
        assert!(matches!(a.spend(1, 0), Err(Error::ResourceExhausted(_))));
        a.stop();
        assert!(a.spend(1, 1).is_ok());
    }

    /// read_on stops at the first key packet after the target, after more
    /// than 1000 others after it, or at a failed read; an exhausted
    /// allowance is its error.
    #[test]
    fn read_on_stops_where_seek_frame_generic_does() {
        let run = |packets: Vec<Result<(bool, Option<i64>)>>| {
            let mut it = packets.into_iter();
            let mut read = 0;
            let r = read_on(10, || {
                read += 1;
                it.next().unwrap_or(Err(Error::Eof))
            });
            (r.map_err(|e| is_exhausted(&e)), read)
        };
        assert_eq!(run(vec![Ok((true, Some(5))), Ok((false, Some(11))), Ok((true, Some(12))), Ok((true, Some(13)))]), (Ok(()), 3));
        let mut many = vec![Ok((true, Some(0)))];
        many.extend((0..2000).map(|n| Ok((false, Some(11 + n)))));
        assert_eq!(run(many), (Ok(()), 1003));
        assert_eq!(run(vec![Ok((true, Some(0))), Err(Error::invalid("damaged"))]), (Ok(()), 2));
        assert_eq!(run(vec![Ok((true, Some(0))), Err(exhausted())]), (Err(true), 2));
    }
}
