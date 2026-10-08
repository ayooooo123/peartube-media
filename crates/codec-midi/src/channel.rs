//! MIDI channel state from FluidSynth 2.6.1 `synth/fluid_chan.c`.
use crate::{conv, generator, modulator, sfont::SoundFont};

#[derive(Clone, Copy, PartialEq)]
pub enum BankStyle {
    Gs,
    Gm,
    Gm2,
    Xg,
}

pub struct Channel {
    pub cc: [u8; 128],
    pub key_pressure: [u8; 128],
    pub pressure: u8,
    pub bend: i16,
    pub bend_range: f32,
    pub modulation_range: f32,
    pub gens: [f64; generator::LAST],
    pub bank: i32,
    pub program: u8,
    pub preset: Option<usize>,
    pub drum: bool,
    pub sostenuto_id: u64,
    pub nrpn: bool,
    pub nrpn_select: usize,
    pub mono: bool,
    pub previous_note: Option<u8>,
    pub held: [(u8, u8); 128],
    pub held_count: usize,
}
impl Channel {
    pub fn new(index: usize, font: &SoundFont) -> Self {
        let drum = index == 9;
        let mut c = Self {
            cc: [0; 128],
            key_pressure: [0; 128],
            pressure: 0,
            bend: 8192,
            bend_range: 2.0,
            modulation_range: 50.0,
            gens: [0.0; generator::LAST],
            bank: if drum { 128 } else { 0 },
            program: 0,
            preset: None,
            drum,
            sostenuto_id: 0,
            nrpn: false,
            nrpn_select: 0,
            mono: false,
            previous_note: None,
            held: [(0, 0); 128],
            held_count: 0,
        };
        c.reset_controllers(false);
        c.preset = font.preset(c.bank, 0);
        c
    }
    pub fn reset_controllers(&mut self, partial: bool) {
        self.pressure = 0;
        self.bend = 8192;
        self.gens.fill(0.0);
        self.key_pressure.fill(0);
        if partial {
            for i in 0..120 {
                if matches!(i, 0 | 32 | 7 | 39 | 10 | 42 | 8 | 40 | 70..=79 | 91..=95) {
                    continue;
                }
                self.cc[i] = 0;
            }
        } else {
            self.cc.fill(0);
            self.cc[70..=79].fill(64);
            self.cc[7] = 100;
            self.cc[10] = 64;
            self.cc[8] = 64;
            self.bend_range = 2.0;
            self.modulation_range = 50.0;
        }
        self.cc[84] = modulator::INVALID_NOTE;
        self.cc[98..=101].fill(127);
        self.cc[11] = 127;
        self.cc[43] = 127;
    }
    pub fn program_change(&mut self, program: u8, font: &SoundFont) {
        self.program = program;
        let fallback = if self.drum { 128 } else { 0 };
        self.preset = font
            .preset(self.bank, i32::from(program))
            .or_else(|| font.preset(fallback, i32::from(program)))
            .or_else(|| font.preset(fallback, 0));
    }
    pub fn bank_msb(&mut self, value: u8, style: BankStyle) {
        match style {
            BankStyle::Gm => {}
            BankStyle::Gs => self.bank = i32::from(value) + if self.drum { 128 } else { 0 },
            BankStyle::Xg => {
                self.drum = matches!(value, 120 | 126 | 127);
                if self.drum {
                    self.bank = 128;
                }
            }
            BankStyle::Gm2 => match value {
                120 => {
                    self.drum = true;
                    self.bank = 128;
                }
                121 => {
                    self.drum = false;
                    self.bank &= 127;
                }
                _ => {}
            },
        }
    }
    pub fn bank_lsb(&mut self, value: u8, style: BankStyle) {
        if matches!(style, BankStyle::Xg | BankStyle::Gm2) && !self.drum {
            self.bank = i32::from(value);
        }
    }
    pub fn hold(&mut self, key: u8, vel: u8) {
        self.unhold(key);
        if self.held_count < self.held.len() {
            self.held[self.held_count] = (key, vel);
            self.held_count += 1;
        }
    }
    pub fn unhold(&mut self, key: u8) {
        if let Some(i) = self.held[..self.held_count]
            .iter()
            .position(|&(k, _)| k == key)
        {
            self.held.copy_within(i + 1..self.held_count, i);
            self.held_count -= 1;
        }
    }
    pub fn last_held(&self) -> Option<(u8, u8)> {
        self.held_count.checked_sub(1).map(|i| self.held[i])
    }
    pub fn portamento_ms(&self, from: u8, to: u8, lsb_seen: bool) -> u32 {
        if lsb_seen {
            return u32::from(self.cc[5]) * 128 + u32::from(self.cc[37]);
        }
        let tmp = conv::concave(f64::from(self.cc[5]));
        let ms = (600000.0 * tmp * conv::concave(128.0 * tmp)
            + 400.0 * conv::convex(f64::from(self.cc[5]) / 4.0)) as i32;
        ((ms.min(480000) * (i32::from(to) - i32::from(from)).abs()) as f32 / 36.0 + 0.5) as u32
    }
}
