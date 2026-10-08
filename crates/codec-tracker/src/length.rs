//! Song timing without mixing: tick lengths depend on pattern data only, so
//! a player over a copy of the song steps through ticks and counts their
//! frames. Used for the song length, ProTracker's VBlank heuristic and
//! finding the row a seek lands on.
//!
//! This replaces libopenmpt's `GetLength`, a separate simulation of the
//! same rules; running the player itself gives the length the player
//! renders.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

use crate::defs::{OrderIndex, RowIndex};
use crate::player::{MixerSettings, Player};
use crate::sndfile::Module;

/// Longest song this crate renders: 2 hours at 48 kHz. Nested pattern
/// loops can make a song play for an effectively unbounded time;
/// libopenmpt plays those to the end, this crate stops here.
pub const MAX_FRAMES: u64 = 48_000 * 60 * 60 * 2;

/// A row as played: its order, its row in the pattern and the output frame
/// it starts at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowPos {
    pub order: OrderIndex,
    pub row: RowIndex,
    pub frame: u64,
}

/// Plays the song's rows in order, up to `MAX_FRAMES`, calling `f` at the
/// start of each until it returns false. Returns the frames played (the
/// song without its end fade).
pub fn walk_rows(m: &Module, mut f: impl FnMut(RowPos) -> bool) -> u64 {
    let mut p = Player::new(m.clone(), MixerSettings::default());
    let mut frames: u64 = 0;
    loop {
        if !p.read_note() {
            return frames;
        }
        if p.ps.tick_count == 0 && !f(RowPos { order: p.ps.current_order, row: p.ps.row, frame: frames }) {
            return frames;
        }
        frames += p.ps.buffer_count as u64;
        p.ps.total_sample_count += p.ps.buffer_count as u64;
        p.ps.buffer_count = 0;
        if frames >= MAX_FRAMES {
            return frames;
        }
    }
}

/// `GetLength(eNoAdjust).front().duration` in seconds.
pub fn song_length_seconds(m: &Module) -> f64 {
    walk_rows(m, |_| true) as f64 / 48_000.0
}

/// Whether the song plays for at least `seconds` (`GetLength` with a time
/// target reports `targetReached`).
pub fn reaches_time(m: &Module, seconds: f64) -> bool {
    let target = (seconds * 48_000.0) as u64;
    walk_rows(m, |r| r.frame < target) >= target
}
