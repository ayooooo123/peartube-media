//! Standard MIDI File parsing: FluidSynth 2.6.1 `midi/fluid_midi.c`
//! (`fluid_midi_file_*`), with what FluidSynth refuses handled per the SMF
//! spec (RP-001): format 2 files play their tracks one after another,
//! SMPTE division runs at frames per second times ticks per frame, unknown
//! chunks are skipped, and `F7` escapes are ignored.

/// A channel or system message kept for the player, `fluid_midi_event_t`.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Ticks after the previous event of the track.
    pub dtime: u32,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    NoteOff {
        chan: u8,
        key: u8,
        vel: u8,
    },
    NoteOn {
        chan: u8,
        key: u8,
        vel: u8,
    },
    KeyPressure {
        chan: u8,
        key: u8,
        value: u8,
    },
    Control {
        chan: u8,
        num: u8,
        value: u8,
    },
    Program {
        chan: u8,
        program: u8,
    },
    ChannelPressure {
        chan: u8,
        value: u8,
    },
    /// 14 bits, 0x2000 centre.
    PitchBend {
        chan: u8,
        value: u16,
    },
    /// Without `F0` and a trailing `F7`.
    Sysex(Box<[u8]>),
    /// Microseconds per quarter note.
    Tempo(u32),
    /// End of track.
    End,
}

/// How long a tick lasts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Division {
    /// Ticks per quarter note; tempo events set the quarter note.
    Ppq(u16),
    /// Milliseconds per tick, fixed (SMPTE frames times ticks per frame).
    Smpte(f32),
}

#[derive(Clone, Debug)]
pub struct Song {
    pub division: Division,
    pub tracks: Vec<Vec<Event>>,
}

#[derive(Debug)]
pub struct ParseError(pub &'static str);

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl Cursor<'_> {
    fn byte(&mut self) -> Result<u8, ParseError> {
        if self.pos >= self.end {
            return Err(ParseError("unexpected end of a track"));
        }
        self.pos += 1;
        Ok(self.data[self.pos - 1])
    }

    fn data_byte(&mut self) -> Result<u8, ParseError> {
        let b = self.byte()?;
        if b >= 128 {
            return Err(ParseError("MIDI data byte has its status bit set"));
        }
        Ok(b)
    }

    fn bytes(&mut self, n: usize) -> Result<&[u8], ParseError> {
        if n > self.end - self.pos {
            return Err(ParseError("unexpected end of a track"));
        }
        self.pos += n;
        Ok(&self.data[self.pos - n..self.pos])
    }

    /// `fluid_midi_file_read_varlen`: at most four bytes.
    fn varlen(&mut self) -> Result<u32, ParseError> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.byte()?;
            if c & 0x80 != 0 {
                v |= u32::from(c & 0x7F);
                v <<= 7;
            } else {
                return Ok(v + u32::from(c));
            }
        }
        Err(ParseError("invalid variable length number"))
    }
}

/// Parses an SMF (`MThd` + `MTrk` chunks).
pub fn parse(data: &[u8]) -> Result<Song, ParseError> {
    if data.len() > 16 * 1024 * 1024 {
        return Err(ParseError("MIDI file exceeds 16 MiB"));
    }
    if data.len() < 14 || &data[0..4] != b"MThd" || data[4..8] != [0, 0, 0, 6] {
        return Err(ParseError("not a MIDI file: bad MThd header"));
    }
    let format = u16::from_be_bytes([data[8], data[9]]);
    if format > 2 {
        return Err(ParseError("unknown MIDI file format"));
    }
    let ntracks = usize::from(u16::from_be_bytes([data[10], data[11]]));
    if ntracks == 0 || ntracks > 128 || (format == 0 && ntracks != 1) {
        return Err(ParseError("invalid MIDI track count (maximum 128)"));
    }
    let division = if (data[12] as i8) < 0 {
        let fps = (data[12] as i8).unsigned_abs();
        if !matches!(fps, 24 | 25 | 29 | 30) {
            return Err(ParseError("invalid SMPTE frame rate"));
        }
        let fps = if fps == 29 { 29.97f32 } else { f32::from(fps) };
        let res = f32::from(data[13]);
        if res == 0.0 {
            return Err(ParseError("SMPTE division without ticks per frame"));
        }
        Division::Smpte(1000.0 / (fps * res))
    } else {
        let d = u16::from_be_bytes([data[12], data[13]]);
        if d == 0 {
            return Err(ParseError("zero ticks per quarter note"));
        }
        Division::Ppq(d)
    };
    let mut pos = 14;
    let mut tracks = Vec::with_capacity(ntracks);
    let mut remaining = 262_144usize;
    while tracks.len() < ntracks && pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let len = u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
            as usize;
        pos += 8;
        let end = pos.saturating_add(len).min(data.len());
        if id == b"MTrk" {
            tracks.push(read_track(data, pos, end, &mut remaining)?);
        }
        pos = end;
    }
    if tracks.is_empty() {
        return Err(ParseError("no tracks"));
    }
    if format == 2 {
        tracks = vec![concatenate(tracks)];
    }
    Ok(Song { division, tracks })
}

/// A format 2 file's tracks, each starting where the one before ends.
fn concatenate(tracks: Vec<Vec<Event>>) -> Vec<Event> {
    let mut out = Vec::new();
    let mut carry = 0u32;
    let last = tracks.len() - 1;
    for (i, track) in tracks.into_iter().enumerate() {
        for mut ev in track {
            ev.dtime = ev.dtime.saturating_add(carry);
            carry = 0;
            if ev.kind == Kind::End && i != last {
                carry = ev.dtime;
                continue;
            }
            out.push(ev);
        }
    }
    out
}

/// `fluid_midi_file_read_track`: the events up to the track's end or its
/// end-of-track event.
fn read_track(
    data: &[u8],
    start: usize,
    end: usize,
    remaining: &mut usize,
) -> Result<Vec<Event>, ParseError> {
    let mut c = Cursor {
        data,
        pos: start,
        end,
    };
    let mut events = Vec::new();
    let mut running_status = 0u8;
    // Ticks since the last kept event: dropped events pass theirs on.
    let mut dtime = 0u32;
    while c.pos < c.end {
        *remaining = remaining
            .checked_sub(1)
            .ok_or(ParseError("MIDI exceeds 262144 events"))?;
        dtime = dtime.saturating_add(c.varlen()?);
        let mut status = c.byte()?;
        if status & 0x80 == 0 {
            if running_status & 0x80 == 0 {
                return Err(ParseError("undefined status and invalid running status"));
            }
            c.pos -= 1;
            status = running_status;
        }
        let kind = match status {
            0xF0 => {
                let len = c.varlen()? as usize;
                if len == 0 {
                    continue;
                }
                let mut msg = c.bytes(len)?;
                if msg.last() == Some(&0xF7) {
                    msg = &msg[..msg.len() - 1];
                }
                Kind::Sysex(msg.into())
            }
            0xF7 => {
                // An escape: raw bytes FluidSynth would not interpret.
                let len = c.varlen()? as usize;
                c.bytes(len)?;
                continue;
            }
            0xFF => {
                let kind = c.byte()?;
                let len = c.varlen()? as usize;
                let meta = c.bytes(len)?;
                match kind {
                    0x2F => {
                        events.push(Event {
                            dtime,
                            kind: Kind::End,
                        });
                        break;
                    }
                    0x51 if len == 3 => {
                        let tempo = u32::from_be_bytes([0, meta[0], meta[1], meta[2]]);
                        if tempo == 0 {
                            return Err(ParseError("zero MIDI tempo"));
                        }
                        Kind::Tempo(tempo)
                    }
                    // Text, lyrics and the rest do nothing in the synth.
                    _ => continue,
                }
            }
            0x80..=0xEF => {
                running_status = status;
                let chan = status & 0x0F;
                let p1 = c.data_byte()?;
                match status & 0xF0 {
                    0x80 => Kind::NoteOff {
                        chan,
                        key: p1,
                        vel: c.data_byte()?,
                    },
                    0x90 => Kind::NoteOn {
                        chan,
                        key: p1,
                        vel: c.data_byte()?,
                    },
                    0xA0 => Kind::KeyPressure {
                        chan,
                        key: p1,
                        value: c.data_byte()?,
                    },
                    0xB0 => Kind::Control {
                        chan,
                        num: p1,
                        value: c.data_byte()?,
                    },
                    0xC0 => Kind::Program { chan, program: p1 },
                    0xD0 => Kind::ChannelPressure { chan, value: p1 },
                    _ => {
                        let p2 = c.data_byte()?;
                        Kind::PitchBend {
                            chan,
                            value: (u16::from(p2 & 0x7F) << 7) | u16::from(p1 & 0x7F),
                        }
                    }
                }
            }
            _ => return Err(ParseError("unrecognized MIDI event")),
        };
        events.push(Event { dtime, kind });
        dtime = 0;
    }
    Ok(events)
}

/// The song's length: the time of its last end of track, in
/// microseconds, through the tempo map. Formats 0 and 1 share one map;
/// format 2 is one track here.
pub fn duration_us(song: &Song) -> u64 {
    // (absolute tick, tempo) changes, all tracks merged.
    let mut tempos: Vec<(u64, u32)> = Vec::new();
    let mut last_tick = 0u64;
    for track in &song.tracks {
        let mut tick = 0u64;
        for ev in track {
            tick += u64::from(ev.dtime);
            if let Kind::Tempo(t) = ev.kind {
                tempos.push((tick, t));
            }
        }
        last_tick = last_tick.max(tick);
    }
    match song.division {
        Division::Smpte(ms) => (last_tick as f64 * f64::from(ms) * 1000.0) as u64,
        Division::Ppq(div) => {
            tempos.sort_by_key(|&(t, _)| t);
            let (mut us, mut at, mut tempo) = (0u128, 0u64, 500_000u32);
            for &(tick, t) in tempos.iter().filter(|&&(tick, _)| tick <= last_tick) {
                us += u128::from(tick - at) * u128::from(tempo);
                at = tick;
                tempo = t;
            }
            us += u128::from(last_tick - at) * u128::from(tempo);
            (us / u128::from(div)).min(u128::from(u64::MAX)) as u64
        }
    }
}
