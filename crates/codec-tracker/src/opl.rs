//! OpenMPT's melodic OPL voice allocation and tracker register mapping.
//! Ported from libopenmpt 0.8.9 `soundlib/OPL.cpp`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors.
//! Includes Schism Tracker contributions (bisqwit, JosepMa, Malvineous),
//! relicensed to BSD with permission by their authors. BSD-3-Clause; see LICENSE.

use crate::defs::MAX_CHANNELS;
use crate::opal::Opal;

const INVALID: u8 = 0xFF;
const CUT: u8 = 0x80;
const KEY_ON: u8 = 0x20;

pub struct Opl {
    chip: Opal,
    key_on_block: [u8; 18],
    voice_to_channel: [usize; 18],
    channel_to_voice: [u8; MAX_CHANNELS],
    patches: [[u8; 12]; 18],
    active: bool,
}

impl Opl {
    pub fn new(sample_rate: u32) -> Self {
        let mut opl = Self {
            chip: Opal::new(sample_rate), key_on_block: [0; 18],
            voice_to_channel: [MAX_CHANNELS; 18], channel_to_voice: [INVALID; MAX_CHANNELS],
            patches: [[0; 12]; 18], active: false,
        };
        opl.reset();
        opl
    }

    fn channel_register(voice: usize) -> u16 {
        if voice < 9 { voice as u16 } else { (voice - 9) as u16 | 0x100 }
    }

    fn operator_register(voice: usize) -> u16 {
        const OPERATORS: [u16; 9] = [0, 1, 2, 8, 9, 10, 16, 17, 18];
        OPERATORS[voice % 9] | if voice < 9 { 0 } else { 0x100 }
    }

    fn voice(&self, channel: usize) -> Option<usize> {
        let voice = self.channel_to_voice[channel];
        if voice & CUT != 0 { None } else { Some(voice as usize) }
    }

    pub fn is_active(&self, channel: usize) -> bool { self.voice(channel).is_some() }

    fn allocate(&mut self, channel: usize) -> Option<usize> {
        let previous = self.channel_to_voice[channel];
        if previous != INVALID {
            if previous & CUT == 0 { return Some(previous as usize); }
            let voice = (previous & !CUT) as usize;
            if self.voice_to_channel[voice] == MAX_CHANNELS || self.voice_to_channel[voice] == channel {
                self.voice_to_channel[voice] = channel;
                self.channel_to_voice[channel] = voice as u8;
                return Some(voice);
            }
        }
        let mut released = None;
        let mut released_cut = None;
        for voice in 0..18 {
            let owner = self.voice_to_channel[voice];
            if owner == MAX_CHANNELS {
                self.voice_to_channel[voice] = channel;
                self.channel_to_voice[channel] = voice as u8;
                return Some(voice);
            } else if self.key_on_block[voice] & KEY_ON == 0 {
                released = Some(voice);
                if self.channel_to_voice[owner] & CUT != 0 { released_cut = Some(voice); }
            }
        }
        if let Some(voice) = released_cut.or(released) {
            self.channel_to_voice[self.voice_to_channel[voice]] = INVALID;
            self.voice_to_channel[voice] = channel;
            self.channel_to_voice[channel] = voice as u8;
        }
        self.voice(channel)
    }

    pub fn note_off(&mut self, channel: usize) {
        let Some(voice) = self.voice(channel) else { return; };
        if self.key_on_block[voice] & KEY_ON == 0 { return; }
        self.key_on_block[voice] &= !KEY_ON;
        self.chip.port(0xB0 | Self::channel_register(voice), self.key_on_block[voice]);
    }

    pub fn note_cut(&mut self, channel: usize, unassign: bool) {
        let Some(voice) = self.voice(channel) else { return; };
        self.note_off(channel);
        // Zero tracker volume is -48 dB, not a replacement silent waveform.
        self.volume(channel, 0, false);
        if unassign {
            self.voice_to_channel[voice] = MAX_CHANNELS;
            self.channel_to_voice[channel] |= CUT;
        }
    }

    pub fn frequency(&mut self, channel: usize, milli_hertz: u32, key_off: bool, beating: bool) {
        let Some(voice) = self.voice(channel) else { return; };
        let (mut fnum, block) = if milli_hertz <= 6208431 {
            let block = if milli_hertz > 3104215 { 7 } else if milli_hertz > 1552107 { 6 }
                else if milli_hertz > 776053 { 5 } else if milli_hertz > 388026 { 4 }
                else if milli_hertz > 194013 { 3 } else if milli_hertz > 97006 { 2 }
                else if milli_hertz > 48503 { 1 } else { 0 };
            let denominator = 49716u64 * 1000;
            (((milli_hertz as u64 * (1u64 << (20 - block)) + denominator / 2) / denominator) as u16, block)
        } else { (1023, 7) };
        if beating { fnum = (fnum + (channel & 3) as u16).min(1023); }
        fnum |= block << 10;
        self.key_on_block[voice] = if key_off { 0 } else { KEY_ON } | (fnum >> 8) as u8;
        let reg = Self::channel_register(voice);
        self.chip.port(0xA0 | reg, fnum as u8);
        self.chip.port(0xB0 | reg, self.key_on_block[voice]);
        self.active = true;
    }

    fn volume_level(mut tracker: u8, level: u8) -> u8 {
        if tracker >= 63 { return level; }
        if tracker != 0 { tracker += 1; }
        (level & 0xC0) | (63 - (63 - (level & 63)) as u16 * tracker as u16 / 64) as u8
    }

    pub fn volume(&mut self, channel: usize, volume: u8, modulator_only: bool) {
        let Some(voice) = self.voice(channel) else { return; };
        let patch = &self.patches[voice];
        let reg = Self::operator_register(voice);
        if patch[10] & 1 != 0 || modulator_only {
            self.chip.port(0x40 + reg, Self::volume_level(volume, patch[2]));
        }
        if !modulator_only { self.chip.port(0x40 + reg + 3, Self::volume_level(volume, patch[3])); }
    }

    pub fn pan(&mut self, channel: usize, pan: i32) -> i32 {
        let Some(voice) = self.voice(channel) else { return 0; };
        let mut value = self.patches[voice][10] & !0x30;
        if pan <= 170 { value |= 0x10; }
        if pan >= 85 { value |= 0x20; }
        self.chip.port(0xC0 | Self::channel_register(voice), value);
        i32::from(value & 0x20 != 0) - i32::from(value & 0x10 != 0)
    }

    pub fn patch(&mut self, channel: usize, patch: &[u8; 12]) {
        let Some(voice) = self.allocate(channel) else { return; };
        self.patches[voice] = *patch;
        let base = Self::operator_register(voice);
        for op in 0..2 {
            let reg = base + op as u16 * 3;
            for (i, bank) in [0x20, 0x40, 0x60, 0x80, 0xE0].into_iter().enumerate() {
                self.chip.port(bank | reg, patch[i * 2 + op]);
            }
        }
        self.chip.port(0xC0 | Self::channel_register(voice), patch[10]);
    }

    pub fn reset(&mut self) {
        if self.active {
            for channel in 0..MAX_CHANNELS { self.note_cut(channel, true); }
            self.active = false;
        }
        self.key_on_block.fill(0);
        self.voice_to_channel.fill(MAX_CHANNELS);
        self.channel_to_voice.fill(INVALID);
        self.chip.port(0x105, 1);
        self.chip.port(0x104, 0);
    }

    pub fn mix(&mut self, output: &mut [i32], vsti_volume: u32) {
        if !self.active { return; }
        let volume_q16 = 65536u64 * vsti_volume as u64 / 48;
        let factor = (volume_q16 * 6169 / 65536) as i32;
        for frame in output.chunks_exact_mut(2) {
            let sample = self.chip.sample();
            frame[0] = frame[0].wrapping_add((sample[0] as i32).wrapping_mul(factor));
            frame[1] = frame[1].wrapping_add((sample[1] as i32).wrapping_mul(factor));
        }
    }
}
