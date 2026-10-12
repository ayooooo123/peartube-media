//! Instruments and envelopes, ported from libopenmpt 0.8.9
//! `soundlib/ModInstrument.h/.cpp`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

#![allow(dead_code)]

use crate::command::{NOTE_MAX, NOTE_MIDDLEC, NOTE_MIN};
use crate::defs::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EnvelopeNode {
    pub tick: u16,
    pub value: u8,
}

#[derive(Clone, Debug)]
pub struct InstrumentEnvelope {
    pub nodes: Vec<EnvelopeNode>,
    pub flags: u8,
    pub loop_start: u8,
    pub loop_end: u8,
    pub sustain_start: u8,
    pub sustain_end: u8,
    pub release_node: u8,
}

impl Default for InstrumentEnvelope {
    fn default() -> Self {
        InstrumentEnvelope {
            nodes: Vec::new(),
            flags: 0,
            loop_start: 0,
            loop_end: 0,
            sustain_start: 0,
            sustain_end: 0,
            release_node: ENV_RELEASE_NODE_UNSET,
        }
    }
}

impl InstrumentEnvelope {
    pub fn size(&self) -> u32 {
        self.nodes.len() as u32
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
    pub fn last_point(&self) -> u8 {
        (self.size().max(1) - 1) as u8
    }
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    /// `GetValueFromPosition`.
    pub fn value_from_position(&self, position: i32, range_out: i32, range_in: i32) -> i32 {
        if self.nodes.is_empty() {
            return 0;
        }
        let mut pt = self.last_point() as usize;
        const ENV_PRECISION: i32 = 1 << 16;
        for i in 0..self.last_point() as usize {
            if position <= self.nodes[i].tick as i32 {
                pt = i;
                break;
            }
        }
        let x2 = self.nodes[pt].tick as i32;
        let mut value: i32;
        if position >= x2 {
            value = self.nodes[pt].value as i32 * ENV_PRECISION / range_in;
        } else {
            let mut x1 = 0;
            value = 0;
            if pt > 0 {
                value = self.nodes[pt - 1].value as i32 * ENV_PRECISION / range_in;
                x1 = self.nodes[pt - 1].tick as i32;
            }
            if x2 > x1 && position > x1 {
                value += muldiv(position - x1, self.nodes[pt].value as i32 * ENV_PRECISION / range_in - value, x2 - x1);
            }
        }
        value = value.clamp(0, ENV_PRECISION);
        ((value as i64 * range_out as i64 + (ENV_PRECISION / 2) as i64) / ENV_PRECISION as i64) as i32
    }

    /// `Sanitize`.
    pub fn sanitize(&mut self, max_value: u8) {
        if !self.nodes.is_empty() {
            self.nodes[0].tick = 0;
            self.nodes[0].value = self.nodes[0].value.min(max_value);
            for i in 1..self.nodes.len() {
                self.nodes[i].tick = self.nodes[i].tick.max(self.nodes[i - 1].tick);
                self.nodes[i].value = self.nodes[i].value.min(max_value);
            }
            let lp = self.last_point();
            self.loop_end = self.loop_end.min(lp);
            self.loop_start = self.loop_start.min(self.loop_end);
            self.sustain_end = self.sustain_end.min(lp);
            self.sustain_start = self.sustain_start.min(self.sustain_end);
            if self.release_node != ENV_RELEASE_NODE_UNSET {
                self.release_node = self.release_node.min(lp);
            }
        } else {
            self.loop_start = 0;
            self.loop_end = 0;
            self.sustain_start = 0;
            self.sustain_end = 0;
            self.release_node = ENV_RELEASE_NODE_UNSET;
        }
    }
}

#[derive(Clone, Debug)]
pub struct ModInstrument {
    pub n_fade_out: u32,
    pub n_global_vol: u32,
    pub n_pan: u32,
    pub n_vol_ramp_up: u16,
    pub resampling: u8,
    pub flags: u8,
    pub nna: NewNoteAction,
    pub dct: DuplicateCheckType,
    pub dna: DuplicateNoteAction,
    pub n_pan_swing: u8,
    pub n_vol_swing: u8,
    pub n_ifc: u8,
    pub n_ifr: u8,
    pub n_cut_swing: u8,
    pub n_res_swing: u8,
    pub filter_mode: FilterMode,
    pub n_pps: i8,
    pub n_ppc: u8,
    pub n_midi_channel: u8,
    pub n_mix_plug: u8,
    pub midi_pwd: i8,
    pub vol_env: InstrumentEnvelope,
    pub pan_env: InstrumentEnvelope,
    pub pitch_env: InstrumentEnvelope,
    pub note_map: [u8; 128],
    pub keyboard: [SampleIndex; 128],
    pub name: String,
}

impl ModInstrument {
    pub fn new(sample: SampleIndex) -> Self {
        let mut note_map = [0u8; 128];
        for (i, n) in note_map.iter_mut().enumerate() {
            *n = NOTE_MIN + i as u8;
        }
        ModInstrument {
            n_fade_out: 256,
            n_global_vol: 64,
            n_pan: 32 * 4,
            n_vol_ramp_up: 0,
            resampling: SRCMODE_DEFAULT,
            flags: 0,
            nna: NewNoteAction::NoteCut,
            dct: DuplicateCheckType::None,
            dna: DuplicateNoteAction::NoteCut,
            n_pan_swing: 0,
            n_vol_swing: 0,
            n_ifc: 0,
            n_ifr: 0,
            n_cut_swing: 0,
            n_res_swing: 0,
            filter_mode: FilterMode::Unchanged,
            n_pps: 0,
            n_ppc: NOTE_MIDDLEC - NOTE_MIN,
            n_midi_channel: 0,
            n_mix_plug: 0,
            midi_pwd: 2,
            vol_env: InstrumentEnvelope::default(),
            pan_env: InstrumentEnvelope::default(),
            pitch_env: InstrumentEnvelope::default(),
            note_map,
            keyboard: [sample; 128],
            name: String::new(),
        }
    }

    pub fn envelope(&self, t: EnvelopeType) -> &InstrumentEnvelope {
        match t {
            EnvelopeType::Volume => &self.vol_env,
            EnvelopeType::Panning => &self.pan_env,
            EnvelopeType::Pitch => &self.pitch_env,
        }
    }

    pub fn is_cutoff_enabled(&self) -> bool {
        self.n_ifc & 0x80 != 0
    }
    pub fn is_resonance_enabled(&self) -> bool {
        self.n_ifr & 0x80 != 0
    }
    pub fn cutoff(&self) -> u8 {
        self.n_ifc & 0x7F
    }
    pub fn resonance(&self) -> u8 {
        self.n_ifr & 0x7F
    }
    pub fn has_valid_midi_channel(&self) -> bool {
        (1..=17).contains(&self.n_midi_channel)
    }

    /// `Sanitize`.
    pub fn sanitize(&mut self, mod_type: u32) {
        self.n_fade_out = self.n_fade_out.min(65536);
        self.n_global_vol = self.n_global_vol.min(64);
        self.n_pan = self.n_pan.min(256);
        self.n_midi_channel = self.n_midi_channel.min(17);
        self.n_pan_swing = self.n_pan_swing.min(64);
        self.n_vol_swing = self.n_vol_swing.min(100);
        self.n_pps = self.n_pps.clamp(-32, 32);
        self.n_cut_swing = self.n_cut_swing.min(64);
        self.n_res_swing = self.n_res_swing.min(64);
        let range = if mod_type == MOD_TYPE_AMS { u8::MAX } else { ENVELOPE_MAX };
        self.vol_env.sanitize(ENVELOPE_MAX);
        self.pan_env.sanitize(ENVELOPE_MAX);
        self.pitch_env.sanitize(range);
        for i in 0..128 {
            if self.note_map[i] < NOTE_MIN || self.note_map[i] > NOTE_MAX {
                self.note_map[i] = i as u8 + NOTE_MIN;
            }
        }
        if self.resampling >= SRCMODE_DEFAULT {
            self.resampling = SRCMODE_DEFAULT;
        }
    }
}
