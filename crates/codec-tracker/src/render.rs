//! The public face of the player: a loaded song, its rows, and renderers
//! that start at any row.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

use crate::defs::{FADESONGDELAY, OrderIndex, RowIndex};
use crate::length::{self, RowPos};
use crate::player::{MixerSettings, Player};
use crate::sndfile::Module;
use crate::Format;

/// Output rate: openmpt123's default.
pub const SAMPLE_RATE: u32 = 48_000;

/// Internal read size used by openmpt123's render mode. Holding this size
/// fixed keeps integer global-volume ramps independent of caller buffers.
pub const READ_FRAMES: usize = 1024;

/// Frames of libopenmpt's fade-out after the song ends (`FadeSong(100)`).
pub const FADE_FRAMES: u64 = (FADESONGDELAY * SAMPLE_RATE / 1000) as u64;

/// A loaded module.
pub struct Song {
    format: Format,
    module: Module,
    song_frames: u64,
}

impl Song {
    /// Loads a module file; `None` when no supported format accepts it.
    pub fn load(data: &[u8]) -> Option<Song> {
        let (format, module) = crate::load(data)?;
        let song_frames = length::walk_rows(&module, |_| true);
        Some(Song { format, module, song_frames })
    }

    pub fn format(&self) -> Format {
        self.format
    }

    pub fn title(&self) -> &str {
        &self.module.title
    }

    /// Frames a renderer from the start produces: every subsong back to
    /// back, then a 100 ms fade-out, as openmpt123 renders.
    pub fn frames(&self) -> u64 {
        self.song_frames + FADE_FRAMES
    }

    /// The row playing at output frame `frame` (the last row when `frame`
    /// is past the song's end).
    pub fn row_at(&self, frame: u64) -> Option<RowPos> {
        let mut found = None;
        length::walk_rows(&self.module, |r| {
            if r.frame > frame {
                return false;
            }
            found = Some(r);
            true
        });
        found
    }

    /// Where `(order, row)` first plays.
    pub fn row_start(&self, order: OrderIndex, row: RowIndex) -> Option<RowPos> {
        let mut found = None;
        length::walk_rows(&self.module, |r| {
            if r.order == order && r.row == row {
                found = Some(r);
                return false;
            }
            true
        });
        found
    }

    /// Starts at an exact output frame. Replaying the mixer preserves
    /// filter history, sample inversion and click-removal state on seeks.
    pub fn renderer_at(&self, frame: u64) -> Renderer {
        let player = Player::new(self.module.clone(), MixerSettings::default());
        let mut renderer = Renderer {
            player,
            pos: 0,
            buffer: [0.0; READ_FRAMES * 2],
            offset: 0,
            available: 0,
            ended: false,
        };
        let target = frame.min(self.frames());
        let mut discard = [0.0; READ_FRAMES * 2];
        while renderer.pos < target {
            let frames = (target - renderer.pos).min(READ_FRAMES as u64) as usize;
            if renderer.read(&mut discard[..frames * 2]) == 0 {
                break;
            }
        }
        renderer
    }

    /// Seeks to the first playback of an order/row pair. An unreachable
    /// row returns `None` rather than silently seeking somewhere else.
    pub fn seek_order_row(&self, order: OrderIndex, row: RowIndex) -> Option<Renderer> {
        self.row_start(order, row).map(|r| self.renderer_at(r.frame))
    }
}

/// Renders a song from some position on.
pub struct Renderer {
    player: Player,
    pos: u64,
    buffer: [f32; READ_FRAMES * 2],
    offset: usize,
    available: usize,
    ended: bool,
}

impl Renderer {
    /// Output frame the next `read` starts at.
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Renders up to `out.len() / 2` interleaved stereo frames at 48 kHz.
    /// Returns the frames rendered; 0 once the song and its fade ended.
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        let mut frames = 0;
        let wanted = out.len() / 2;
        while frames < wanted {
            if self.offset == self.available {
                if self.ended {
                    break;
                }
                self.available = self.player.read(&mut self.buffer);
                self.offset = 0;
                self.ended = self.available < READ_FRAMES;
                if self.available == 0 {
                    break;
                }
            }
            let n = (wanted - frames).min(self.available - self.offset);
            out[frames * 2..(frames + n) * 2]
                .copy_from_slice(&self.buffer[self.offset * 2..(self.offset + n) * 2]);
            self.offset += n;
            frames += n;
        }
        self.pos += frames as u64;
        frames
    }
}
