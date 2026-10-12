//! FluidSynth's sample-timer player: integer milliseconds, 64-sample
//! callbacks, track-order dispatch and a two-second effects tail.
use crate::{
    smf::{Division, Event, Kind, Song},
    synth::Synth,
};

struct Track {
    events: Vec<Event>,
    cursor: usize,
    ticks: u64,
}
pub struct Player {
    tracks: Vec<Track>,
    division: Division,
    delta_ms: f32,
    start_ms: u64,
    start_ticks: u64,
    end_ms: Option<u64>,
    pedals_disabled: bool,
    pub done: bool,
}
impl Player {
    pub fn new(song: Song) -> Self {
        let delta_ms = match song.division {
            Division::Ppq(d) => 500_000.0f32 / f32::from(d) / 1000.0,
            Division::Smpte(ms) => ms,
        };
        Self {
            tracks: song
                .tracks
                .into_iter()
                .map(|events| Track {
                    events,
                    cursor: 0,
                    ticks: 0,
                })
                .collect(),
            division: song.division,
            delta_ms,
            start_ms: 0,
            start_ticks: 0,
            end_ms: None,
            pedals_disabled: false,
            done: false,
        }
    }
    pub fn callback(&mut self, synth: &mut Synth) -> Result<(), &'static str> {
        let ms = (synth.ticks as f64 * 1000.0 / synth.rate) as u64;
        if ms > 24 * 60 * 60 * 1000 {
            return Err("MIDI playback exceeds 24 hours");
        }
        let tick = self.start_ticks
            + ((ms - self.start_ms) as f64 / f64::from(self.delta_ms) + 0.5) as u64;
        let mut pending = false;
        let mut dispatched = 0;
        for track in &mut self.tracks {
            while let Some(event) = track.events.get(track.cursor) {
                if track.ticks + u64::from(event.dtime) > tick {
                    break;
                }
                dispatched += 1;
                if dispatched > 4096 {
                    return Err("MIDI has too many events in one audio block");
                }
                track.ticks += u64::from(event.dtime);
                if event.kind != Kind::End {
                    synth.event(&event.kind);
                }
                if let (Kind::Tempo(tempo), Division::Ppq(d)) = (&event.kind, self.division) {
                    self.delta_ms = *tempo as f32 / f32::from(d) / 1000.0;
                    self.start_ms = ms;
                    self.start_ticks = tick;
                }
                track.cursor += 1;
                if synth.failed {
                    return Err("MIDI exceeds the bounded voice or DSP command capacity");
                }
            }
            pending |= track.cursor < track.events.len();
        }
        if !pending {
            if synth.active_count() > 0 {
                if !self.pedals_disabled {
                    for c in 0..16 {
                        synth.control(c, 64, 0);
                        synth.control(c, 66, 0);
                        synth.control(c, 123, 0);
                    }
                    self.pedals_disabled = true;
                }
            } else {
                let end = *self.end_ms.get_or_insert(ms + 2000);
                if ms >= end {
                    synth.reset();
                    self.done = true;
                }
            }
        }
        Ok(())
    }
}
