//! The tick engine and render loop, ported from libopenmpt 0.8.9
//! `soundlib/Sndmix.cpp` (`Read`, `ReadNote`, `ProcessRow`, envelopes,
//! vibrato, arpeggio, ramping) and `Sndfile.cpp` (`GetTickDuration`).
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

#![allow(dead_code)]

use crate::channel::{ModChannel, RESET_SET_POS_FULL};
use crate::command::*;
use crate::defs::pb::*;
use crate::defs::*;
use crate::player::Player;
use crate::sndfile::{Module, PanningMode, TempoMode};
use crate::tables::*;

const PREAMP_TABLE: [u8; 16] = [0x60, 0x60, 0x60, 0x70, 0x80, 0x88, 0x90, 0x98, 0xA0, 0xA4, 0xA8, 0xAC, 0xB0, 0xB4, 0xB8, 0xBC];

fn xm2mod_finetune(v: i32) -> i32 {
    ((v as u8) >> 4) as i32
}

impl Player {
    /// `NextRow`: returns (ignoreRow, patternTransition).
    fn next_row(&mut self, break_row: bool) -> (bool, bool) {
        let t = self.m.mod_type;
        let ignore_row = self.ps.pattern_delay > 1 && break_row && t == MOD_TYPE_MOD;
        let transition = self.ps.next_row == 0 || break_row;
        if transition && t == MOD_TYPE_S3M {
            for i in 0..self.num_channels() {
                self.ps.chn[i].n_pattern_loop = 0;
            }
        }
        self.ps.pattern_delay = 0;
        self.ps.frame_delay = 0;
        self.ps.tick_count = 0;
        self.ps.row = self.ps.next_row;
        self.ps.current_order = self.ps.next_order;
        (ignore_row, transition)
    }

    /// `SetupNextRow`.
    fn setup_next_row(&mut self, pattern_loop: bool) {
        self.ps.next_row = self.ps.row + 1;
        let rows = self.m.patterns.get(self.ps.pattern as usize).map_or(0, |p| p.rows);
        if self.ps.next_row >= rows {
            if !pattern_loop {
                self.ps.next_order = self.ps.current_order.wrapping_add(1);
            }
            self.ps.next_row = 0;
            if self.m.behaviour(kFT2LoopE60Restart) {
                self.ps.next_row = self.ps.next_pat_start_row;
                self.ps.next_pat_start_row = 0;
            }
        }
    }

    /// `ProcessRow`.
    pub fn process_row(&mut self) -> bool {
        loop {
            if self.visited.too_complex() || self.ps.total_sample_count >= crate::length::MAX_FRAMES {
                return false;
            }
            self.ps.tick_count = self.ps.tick_count.wrapping_add(1);
            if self.ps.tick_count < self.ps.ticks_on_row() {
                break;
            }
            let break_flag = self.ps.flag(SONG_BREAKTOROW);
            let (ignore_row, _transition) = self.next_row(break_flag);

            if !self.ps.flag(SONG_PATTERNLOOP) {
                let song_end = self.m.order.len();
                self.ps.pattern = if (self.ps.current_order as usize) < song_end {
                    self.m.order[self.ps.current_order as usize]
                } else {
                    PATTERNINDEX_INVALID
                };
                if (self.ps.pattern as usize) < self.m.patterns.len() && !self.m.patterns[self.ps.pattern as usize].is_valid() {
                    self.ps.pattern = PATTERNINDEX_SKIP;
                }
                while self.ps.pattern as usize >= self.m.patterns.len() {
                    if self.ps.pattern == PATTERNINDEX_INVALID || self.ps.current_order as usize >= song_end {
                        let mut restart = self.m.restart_pos;
                        if restart == 0 && (self.ps.current_order as usize) <= song_end && self.ps.current_order > 0 {
                            let mut ord = self.ps.current_order - 1;
                            while ord > 0 {
                                if self.m.order.get(ord as usize).copied() == Some(PATTERNINDEX_INVALID) {
                                    restart = ord + 1;
                                    break;
                                }
                                ord -= 1;
                            }
                        }
                        self.ps.flags |= SONG_BREAKTOROW;
                        self.ps.current_order = restart;
                        self.ps.flags &= !SONG_BREAKTOROW;
                        while (self.ps.current_order as usize) < self.m.order.len()
                            && self.m.order[self.ps.current_order as usize] == PATTERNINDEX_SKIP
                        {
                            self.ps.current_order += 1;
                        }
                        if self.ps.current_order as usize >= self.m.order.len() || !self.m.is_valid_order(self.ps.current_order) {
                            let m = &self.m;
                            self.visited.initialize(m, true);
                            return false;
                        }
                    } else {
                        self.ps.current_order += 1;
                    }
                    self.ps.pattern = if (self.ps.current_order as usize) < self.m.order.len() {
                        self.m.order[self.ps.current_order as usize]
                    } else {
                        PATTERNINDEX_INVALID
                    };
                    if (self.ps.pattern as usize) < self.m.patterns.len() && !self.m.patterns[self.ps.pattern as usize].is_valid() {
                        self.ps.pattern = PATTERNINDEX_SKIP;
                    }
                }
                self.ps.next_order = self.ps.current_order;
            }

            if !self.m.is_valid_pat(self.ps.pattern) {
                return false;
            }
            if self.ps.row >= self.m.patterns[self.ps.pattern as usize].rows {
                self.ps.row = 0;
            }

            let override_loop_check = self.repeat_count != -1 && self.ps.flag(SONG_PATTERNLOOP);
            if !override_loop_check {
                let m = &self.m;
                let visited = self.visited.visit(m, self.ps.current_order, self.ps.row, &self.ps.chn, ignore_row);
                if visited {
                    if self.repeat_count != 0 {
                        if self.repeat_count > 0 {
                            self.repeat_count -= 1;
                        }
                        let m = &self.m;
                        self.visited.initialize(m, true);
                        self.visited.visit(m, self.ps.current_order, self.ps.row, &self.ps.chn, ignore_row);
                    } else if self.m.song_flag(SONG_PLAYALLSONGS) {
                        let m = &self.m;
                        match self.visited.first_unvisited_row(m, true) {
                            Some((o, r)) => {
                                self.ps.current_order = o;
                                self.ps.row = r;
                            }
                            None => {
                                self.ps.next_order = 0;
                                self.ps.current_order = 0;
                                self.ps.next_row = 0;
                                self.ps.row = 0;
                                // Only one sequence: playback ends.
                                self.visited.initialize(m, true);
                                return false;
                            }
                        }
                        for i in 0..self.ps.chn.len() {
                            let mut c = std::mem::take(&mut self.ps.chn[i]);
                            if c.has(CHN_ADLIB) {
                                if let Some(opl) = &mut self.opl { opl.note_cut(i, true); }
                            }
                            c.reset(RESET_SET_POS_FULL, &self.m, i, CHN_SYNCMUTE);
                            self.ps.chn[i] = c;
                        }
                        self.ps.music_speed = self.m.default_speed;
                        self.ps.music_tempo = self.m.default_tempo;
                        self.ps.global_volume = self.m.default_global_volume as i32;
                        self.ps.next_order = self.ps.current_order;
                        self.ps.next_row = self.ps.row;
                        if self.m.order.len() > self.ps.current_order as usize {
                            self.ps.pattern = self.m.order[self.ps.current_order as usize];
                        }
                        let m = &self.m;
                        self.visited.visit(m, self.ps.current_order, self.ps.row, &self.ps.chn, ignore_row);
                        if !self.m.is_valid_pat(self.ps.pattern) {
                            return false;
                        }
                    } else {
                        let m = &self.m;
                        self.visited.initialize(m, true);
                        return false;
                    }
                }
            }

            let pattern_loop = self.ps.flag(SONG_PATTERNLOOP);
            self.setup_next_row(pattern_loop);

            // Reset channel values.
            let nc = self.num_channels();
            let pat = self.ps.pattern as usize;
            let row = self.ps.row;
            for ci in 0..nc {
                let mcmd = *self.m.patterns[pat].cell(row, ci, nc);
                let m = &self.m;
                let music_speed = self.ps.music_speed;
                let chn = &mut self.ps.chn[ci];
                if m.behaviour(KST3PortaAfterArpeggio)
                    && chn.n_command == CMD_ARPEGGIO
                    && (mcmd.command == CMD_PORTAMENTOUP || mcmd.command == CMD_PORTAMENTODOWN)
                {
                    chn.n_period =
                        m.period_from_note(chn.n_arpeggio_last_note as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32) as i32;
                }
                if m.behaviour(kMODOutOfRangeNoteDelay)
                    && !mcmd.is_note()
                    && chn.row_command.is_note()
                    && chn.row_command.command == CMD_MODCMDEX
                    && (chn.row_command.param & 0xF0) == 0xD0
                    && (chn.row_command.param & 0x0F) as u32 >= music_speed
                {
                    chn.n_period = m.period_from_note(chn.row_command.note as u32, chn.n_fine_tune as i32, 0) as i32;
                }
                if m.behaviour(kST3TonePortaWithAdlibNote)
                    && !mcmd.is_note()
                    && chn.has(CHN_ADLIB)
                    && chn.n_portamento_dest != 0
                    && chn.row_command.is_note()
                    && chn.row_command.is_tone_portamento()
                {
                    chn.n_period = chn.n_portamento_dest;
                }
                let tempo_fix = m.behaviour(kMODTempoOnSecondTick)
                    && !m.behaviour(kMODVBlankTiming)
                    && music_speed == 1
                    && chn.row_command.command == CMD_TEMPO;
                let tempo_param = chn.row_command.param;
                chn.right_vol = chn.new_right_vol;
                chn.left_vol = chn.new_left_vol;
                chn.reset_flag(CHN_VIBRATO | CHN_TREMOLO);
                if !m.behaviour(kITVibratoTremoloPanbrello) {
                    chn.n_panbrello_offset = 0;
                }
                chn.n_command = CMD_NONE;
                chn.row_command = mcmd;
                if tempo_fix {
                    self.ps.music_tempo = Tempo::new(tempo_param.max(1) as u32, 0);
                }
            }
            let m = &self.m;
            self.ps.update_time_signature(m);

            if ignore_row {
                self.ps.tick_count = self.ps.music_speed;
                continue;
            }
            break;
        }
        if self.ps.music_speed == 0 {
            self.ps.music_speed = 1;
        }
        let t = self.m.mod_type;
        if self.ps.tick_count != 0 {
            self.ps.flags &= !SONG_FIRSTTICK;
            if t & (MOD_TYPE_XM | MOD_TYPE_MT2) == 0
                && (t != MOD_TYPE_MOD || self.m.song_flag(SONG_PT_MODE))
                && self.ps.tick_count < self.ps.ticks_on_row()
            {
                let d = self.ps.music_speed.wrapping_add(self.ps.frame_delay);
                if d != 0 && self.ps.tick_count % d == 0 {
                    self.ps.flags |= SONG_FIRSTTICK;
                }
            }
        } else {
            self.ps.flags |= SONG_FIRSTTICK;
            self.ps.flags &= !SONG_BREAKTOROW;
        }
        self.process_effects()
    }

    /// `GetVibratoDelta`.
    fn vibrato_delta(&mut self, ty: u8, position: i32) -> i32 {
        let t = self.m.mod_type;
        if self.m.behaviour(kITVibratoTremoloPanbrello) {
            let position = position & 0xFF;
            match ty & 0x03 {
                1 => 64 - (position + 1) / 2,
                2 => {
                    if position < 128 {
                        64
                    } else {
                        0
                    }
                }
                3 => self.prng.bits7() - 0x40,
                _ => ITSINUS_TABLE[position as usize] as i32,
            }
        } else if t & (MOD_TYPE_DIGI | MOD_TYPE_DBM) != 0 {
            const DBM_SINUS: [i8; 32] = [
                33, 52, 69, 84, 96, 107, 116, 122, 125, 127, 125, 122, 116, 107, 96, 84, 69, 52, 33, 13, -8, -31, -54, -79, -104, -128, -104,
                -79, -54, -31, -8, 13,
            ];
            DBM_SINUS[((position as u32 / 2) & 0x1F) as usize] as i32
        } else {
            let position = position & 0x3F;
            match ty & 0x03 {
                1 => (if position < 32 { 0 } else { 255 }) - position * 4,
                2 => {
                    if position < 32 {
                        127
                    } else {
                        -127
                    }
                }
                3 => MOD_RANDOM_TABLE[position as usize] as i32,
                _ => MOD_SINUS_TABLE[position as usize] as i32,
            }
        }
    }

    fn process_tremolo(&mut self, nchn: usize, vol: &mut i32) {
        if !self.ps.chn[nchn].has(CHN_TREMOLO) {
            return;
        }
        let t = self.m.mod_type;
        let first = self.ps.flag(SONG_FIRSTTICK);
        if self.m.song_flag(SONG_PT_MODE) && first {
            return;
        }
        let it_vtp = self.m.behaviour(kITVibratoTremoloPanbrello);
        if *vol > 0 || it_vtp {
            let attenuation = if t & (MOD_TYPE_XM | MOD_TYPE_MOD) != 0 || it_vtp { 5 } else { 6 };
            let (ty, pos) = (self.ps.chn[nchn].n_tremolo_type, self.ps.chn[nchn].n_tremolo_pos);
            let mut delta = self.vibrato_delta(ty, pos as i32);
            let chn = &self.ps.chn[nchn];
            if (chn.n_tremolo_type & 0x03) == 1 && self.m.behaviour(kFT2MODTremoloRampWaveform) {
                let mut ramp = ((chn.n_tremolo_pos as u32 * 4) & 0x7F) as i32;
                let mut vib_pos = chn.n_vibrato_pos as u32;
                if !first && chn.has(CHN_VIBRATO) {
                    vib_pos += chn.n_vibrato_speed as u32;
                }
                if (vib_pos & 0x3F) >= 32 {
                    ramp ^= 0x7F;
                }
                delta = if (chn.n_tremolo_pos & 0x3F) >= 32 { -ramp } else { ramp };
            }
            if t != MOD_TYPE_DMF {
                *vol += (delta * chn.n_tremolo_depth as i32) / (1 << attenuation);
            } else {
                *vol -= (*vol * chn.n_tremolo_depth as i32 * (64 - delta)) / (128 * 64);
            }
        }
        if !first || (t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 && !self.m.song_flag(SONG_ITOLDEFFECTS)) {
            let chn = &mut self.ps.chn[nchn];
            if it_vtp {
                chn.n_tremolo_pos = chn.n_tremolo_pos.wrapping_add((4u32 * chn.n_tremolo_speed as u32) as u8);
            } else {
                chn.n_tremolo_pos = chn.n_tremolo_pos.wrapping_add(chn.n_tremolo_speed);
            }
        }
    }

    fn process_tremor(&mut self, nchn: usize, vol: &mut i32) {
        let t = self.m.mod_type;
        let first = self.ps.flag(SONG_FIRSTTICK);
        let m = &self.m;
        let chn = &mut self.ps.chn[nchn];
        if m.behaviour(kFT2Tremor) {
            if chn.n_tremor_count & 0x80 != 0 {
                if !first && chn.n_command == CMD_TREMOR {
                    chn.n_tremor_count &= !0x20;
                    if chn.n_tremor_count == 0x80 {
                        chn.n_tremor_count = (chn.n_tremor_param >> 4) | 0xC0;
                    } else if chn.n_tremor_count == 0xC0 {
                        chn.n_tremor_count = (chn.n_tremor_param & 0x0F) | 0x80;
                    } else {
                        chn.n_tremor_count = chn.n_tremor_count.wrapping_sub(1);
                    }
                    chn.set(CHN_FASTVOLRAMP);
                }
                if (chn.n_tremor_count & 0xE0) == 0x80 {
                    *vol = 0;
                }
            }
        } else if chn.n_command == CMD_TREMOR {
            if m.behaviour(kITTremor) {
                if chn.n_tremor_count & 0x80 != 0 && chn.n_length != 0 {
                    if chn.n_tremor_count == 0x80 {
                        chn.n_tremor_count = (chn.n_tremor_param >> 4) | 0xC0;
                    } else if chn.n_tremor_count == 0xC0 {
                        chn.n_tremor_count = (chn.n_tremor_param & 0x0F) | 0x80;
                    } else {
                        chn.n_tremor_count = chn.n_tremor_count.wrapping_sub(1);
                    }
                }
                if (chn.n_tremor_count & 0xC0) == 0x80 {
                    *vol = 0;
                }
            } else {
                let mut ontime = chn.n_tremor_param >> 4;
                let mut n = ontime.wrapping_add(chn.n_tremor_param & 0x0F);
                if t & (MOD_TYPE_IT | MOD_TYPE_MPT) == 0 || m.song_flag(SONG_ITOLDEFFECTS) {
                    n = n.wrapping_add(2);
                    ontime = ontime.wrapping_add(1);
                }
                let mut tremcount = chn.n_tremor_count;
                if t & MOD_TYPE_XM == 0 {
                    if tremcount >= n {
                        tremcount = 0;
                    }
                    if tremcount >= ontime {
                        *vol = 0;
                    }
                    chn.n_tremor_count = tremcount.wrapping_add(1);
                } else {
                    if first {
                        if tremcount > 0 {
                            tremcount -= 1;
                        }
                    } else {
                        chn.n_tremor_count = tremcount.wrapping_add(1);
                    }
                    if n != 0 && tremcount % n >= ontime {
                        *vol = 0;
                    }
                }
            }
            chn.set(CHN_FASTVOLRAMP);
        }
    }

    fn is_envelope_processed(m: &Module, chn: &ModChannel, env: EnvelopeType) -> bool {
        let Some(ins) = chn.p_mod_instrument.and_then(|i| m.instrument(i as u32)) else {
            return false;
        };
        let ins_env = ins.envelope(env);
        let play_if_paused = m.behaviour(kITEnvelopePositionHandling) || m.behaviour(kFT2PanSustainRelease);
        (chn.envelope(env).flags & ENV_ENABLED != 0 || (ins_env.has(ENV_ENABLED) && play_if_paused)) && !ins_env.is_empty()
    }

    fn process_volume_envelope(m: &Module, chn: &ModChannel, vol: &mut i32) {
        if !Self::is_envelope_processed(m, chn, EnvelopeType::Volume) {
            return;
        }
        let ins = m.instrument(chn.p_mod_instrument.unwrap() as u32).unwrap();
        let it_env = m.behaviour(kITEnvelopePositionHandling);
        if it_env && chn.vol_env.n_env_position == 0 {
            return;
        }
        let envpos = chn.vol_env.n_env_position as i32 - if it_env { 1 } else { 0 };
        let mut envval = ins.vol_env.value_from_position(envpos, 256, ENVELOPE_MAX as i32);
        if ins.vol_env.release_node != ENV_RELEASE_NODE_UNSET && chn.vol_env.n_env_value_at_release_jump != NOT_YET_RELEASED {
            let at_jump = chn.vol_env.n_env_value_at_release_jump as i32;
            let node = &ins.vol_env.nodes[ins.vol_env.release_node as usize];
            let at_node = node.value as i32 * 4;
            if envpos == node.tick as i32 {
                envval = at_node;
            }
            if m.behaviour(kLegacyReleaseNode) {
                let rel = (envval - at_node) * 2;
                envval = at_jump + rel;
            } else if at_node > 0 {
                envval = at_jump * envval / at_node;
            } else {
                envval = 0;
            }
        }
        *vol = (*vol * envval.clamp(0, 512)) / 256;
    }

    fn process_panning_envelope(m: &Module, chn: &mut ModChannel) {
        if !Self::is_envelope_processed(m, chn, EnvelopeType::Panning) {
            return;
        }
        let ins = m.instrument(chn.p_mod_instrument.unwrap() as u32).unwrap();
        let it_env = m.behaviour(kITEnvelopePositionHandling);
        if it_env && chn.pan_env.n_env_position == 0 {
            return;
        }
        let envpos = chn.pan_env.n_env_position as i32 - if it_env { 1 } else { 0 };
        let envval = ins.pan_env.value_from_position(envpos, 64, ENVELOPE_MAX as i32) - 32;
        let mut pan = chn.n_real_pan;
        if pan >= 128 {
            pan += (envval * (256 - pan)) / 32;
        } else {
            pan += (envval * pan) / 32;
        }
        chn.n_real_pan = pan.clamp(0, 256);
    }

    fn process_pitch_filter_envelope(&mut self, nchn: usize, period: &mut i32) -> i32 {
        let m = &self.m;
        let freq = self.settings.mixing_freq;
        let chn = &mut self.ps.chn[nchn];
        if !Self::is_envelope_processed(m, chn, EnvelopeType::Pitch) {
            return -1;
        }
        let ins = m.instrument(chn.p_mod_instrument.unwrap() as u32).unwrap();
        let it_env = m.behaviour(kITEnvelopePositionHandling);
        if it_env && chn.pitch_env.n_env_position == 0 {
            if m.behaviour(kITStoppedFilterEnvAtStart) && chn.pitch_env.flags & ENV_FILTER != 0 {
                let reset = !chn.has(CHN_FILTER);
                return m.setup_channel_filter(freq, chn, reset, 0);
            }
            return -1;
        }
        let envpos = chn.pitch_env.n_env_position as i32 - if it_env { 1 } else { 0 };
        let t = m.mod_type;
        let range = if t == MOD_TYPE_AMS { 255 } else { ENVELOPE_MAX as i32 };
        let amp = match t {
            MOD_TYPE_AMS => 64,
            MOD_TYPE_MDL => 192,
            _ => 512,
        };
        let envval = ins.pitch_env.value_from_position(envpos, amp, range) - amp / 2;
        if chn.pitch_env.flags & ENV_FILTER != 0 {
            let reset = !chn.has(CHN_FILTER);
            return m.setup_channel_filter(freq, chn, reset, envval);
        }
        let use_freq = m.periods_are_frequencies();
        let up: &[u32; 256] = if use_freq { &LINEAR_SLIDE_UP_TABLE } else { &LINEAR_SLIDE_DOWN_TABLE };
        let down: &[u32; 256] = if use_freq { &LINEAR_SLIDE_DOWN_TABLE } else { &LINEAR_SLIDE_UP_TABLE };
        let mut l = envval;
        if l < 0 {
            l = (-l).min(255);
            *period = muldiv(*period, down[l as usize] as i32, 65536);
        } else {
            l = l.min(255);
            *period = muldiv(*period, up[l as usize] as i32, 65536);
        }
        -1
    }

    fn increment_envelope_position(m: &Module, chn: &mut ModChannel, env_type: EnvelopeType) {
        let Some(ins) = chn.p_mod_instrument.and_then(|i| m.instrument(i as u32)) else {
            return;
        };
        if chn.envelope(env_type).flags & ENV_ENABLED == 0 {
            return;
        }
        let it_env = m.behaviour(kITEnvelopePositionHandling);
        let mut position = chn.envelope(env_type).n_env_position.wrapping_add(if it_env { 0 } else { 1 });
        let ins_env = ins.envelope(env_type);
        if ins_env.is_empty() {
            return;
        }
        let t = m.mod_type;
        let mut end_reached = false;
        if !it_env {
            if ins_env.has(ENV_LOOP) {
                let mut end = ins_env.nodes[ins_env.loop_end as usize].tick as u32;
                if t & (MOD_TYPE_XM | MOD_TYPE_MT2) == 0 {
                    end += 1;
                }
                let escape = ins_env.loop_end == ins_env.sustain_end
                    && ins_env.has(ENV_SUSTAIN)
                    && chn.has(CHN_KEYOFF)
                    && m.behaviour(kFT2EnvelopeEscape);
                if position == end && !escape {
                    position = ins_env.nodes[ins_env.loop_start as usize].tick as u32;
                }
            }
            if ins_env.has(ENV_SUSTAIN) && !chn.has(CHN_KEYOFF) {
                if position == ins_env.nodes[ins_env.sustain_end as usize].tick as u32 + 1 {
                    position = ins_env.nodes[ins_env.sustain_start as usize].tick as u32;
                    if m.behaviour(kFT2PanSustainRelease) && env_type == EnvelopeType::Panning && !chn.has(CHN_KEYOFF) {
                        chn.envelope_mut(env_type).flags &= !ENV_ENABLED;
                    }
                }
            } else {
                let last = ins_env.nodes.last().unwrap().tick as u32;
                if position > last {
                    position = last;
                    end_reached = true;
                }
            }
        } else {
            let start;
            let end;
            if ins_env.has(ENV_SUSTAIN)
                && chn.dw_old_flags & CHN_KEYOFF == 0
                && (chn.envelope(env_type).n_env_value_at_release_jump == NOT_YET_RELEASED || m.behaviour(kReleaseNodePastSustainBug))
            {
                start = ins_env.nodes[ins_env.sustain_start as usize].tick as u32;
                end = ins_env.nodes[ins_env.sustain_end as usize].tick as u32 + 1;
            } else if ins_env.has(ENV_LOOP) {
                start = ins_env.nodes[ins_env.loop_start as usize].tick as u32;
                end = ins_env.nodes[ins_env.loop_end as usize].tick as u32 + 1;
            } else {
                let last = ins_env.nodes.last().unwrap().tick as u32;
                start = last;
                end = last;
                if position > end {
                    end_reached = true;
                }
            }
            if position >= end {
                position = start;
            }
        }
        if env_type == EnvelopeType::Volume && end_reached {
            if t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 || (chn.has(CHN_KEYOFF) && t != MOD_TYPE_MDL) {
                chn.set(CHN_NOTEFADE);
            }
            if ins_env.nodes.last().unwrap().value == 0 && (chn.n_master_chn > 0 || t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0) {
                chn.set(CHN_NOTEFADE);
                chn.n_fade_out_vol = 0;
                chn.n_real_volume = 0;
                chn.n_calc_volume = 0;
            }
        }
        chn.envelope_mut(env_type).n_env_position = position + if it_env { 1 } else { 0 };
    }

    fn increment_envelope_positions(m: &Module, chn: &mut ModChannel) {
        if chn.is_first_tick && m.mod_type == MOD_TYPE_MED {
            return;
        }
        Self::increment_envelope_position(m, chn, EnvelopeType::Volume);
        Self::increment_envelope_position(m, chn, EnvelopeType::Panning);
        Self::increment_envelope_position(m, chn, EnvelopeType::Pitch);
    }

    fn process_instrument_fade(m: &Module, chn: &mut ModChannel, vol: &mut i32) {
        if !chn.has(CHN_NOTEFADE) {
            return;
        }
        let Some(ins) = chn.p_mod_instrument.and_then(|i| m.instrument(i as u32)) else {
            return;
        };
        let fadeout = ins.n_fade_out;
        if fadeout != 0 {
            chn.n_fade_out_vol -= (fadeout * 2) as i32;
            if chn.n_fade_out_vol <= 0 {
                chn.n_fade_out_vol = 0;
            }
            *vol = ((*vol as i64 * chn.n_fade_out_vol as i64) / 65536) as i32;
        } else if chn.n_fade_out_vol == 0 {
            *vol = 0;
        }
    }

    fn process_panbrello(&mut self, nchn: usize) {
        let mut pdelta = self.ps.chn[nchn].n_panbrello_offset as i32;
        if self.ps.chn[nchn].row_command.command == CMD_PANBRELLO {
            let it_vtp = self.m.behaviour(kITVibratoTremoloPanbrello);
            let chn = &self.ps.chn[nchn];
            let panpos = if it_vtp { chn.n_panbrello_pos as u32 } else { (chn.n_panbrello_pos as u32 + 0x10) >> 2 };
            let ty = chn.n_panbrello_type;
            pdelta = self.vibrato_delta(ty, panpos as i32);
            let hold = self.m.behaviour(kITSampleAndHoldPanbrello);
            let panbrello_hold = self.m.behaviour(kITPanbrelloHold);
            let chn = &mut self.ps.chn[nchn];
            if hold && chn.n_panbrello_type == 3 {
                if chn.n_panbrello_pos == 0 || chn.n_panbrello_pos >= chn.n_panbrello_speed {
                    chn.n_panbrello_pos = 0;
                    chn.n_panbrello_random_memory = pdelta as i8;
                }
                chn.n_panbrello_pos = chn.n_panbrello_pos.wrapping_add(1);
                pdelta = chn.n_panbrello_random_memory as i32;
            } else {
                chn.n_panbrello_pos = chn.n_panbrello_pos.wrapping_add(chn.n_panbrello_speed);
            }
            if panbrello_hold {
                chn.n_panbrello_offset = pdelta as i8;
            }
        }
        if pdelta != 0 {
            let chn = &mut self.ps.chn[nchn];
            pdelta = ((pdelta * chn.n_panbrello_depth as i32) + 2) / 8;
            pdelta += chn.n_real_pan;
            chn.n_real_pan = pdelta.clamp(0, 256);
        }
    }

    fn process_arpeggio(&mut self, nchn: usize, period: &mut i32) {
        let m = &self.m;
        let t = m.mod_type;
        let tick_count = self.ps.tick_count;
        let music_speed = self.ps.music_speed;
        let frame_delay = self.ps.frame_delay;
        let first = self.ps.flag(SONG_FIRSTTICK);
        let chn = &mut self.ps.chn[nchn];
        if chn.n_command == CMD_ARPEGGIO {
            if m.behaviour(kITArpeggio) {
                let d = music_speed.wrapping_add(frame_delay);
                let tick = if d != 0 { tick_count % d } else { 0 };
                if chn.n_arpeggio != 0 {
                    let ratio = match tick % 3 {
                        1 => LINEAR_SLIDE_UP_TABLE[((chn.n_arpeggio >> 4) as usize) * 16],
                        2 => LINEAR_SLIDE_UP_TABLE[((chn.n_arpeggio & 0x0F) as usize) * 16],
                        _ => 65536,
                    };
                    if m.periods_are_frequencies() {
                        *period = muldivr(*period, ratio as i32, 65536);
                    } else {
                        *period = muldivr(*period, 65536, ratio as i32);
                    }
                }
            } else if m.behaviour(kFT2Arpeggio) {
                if !first {
                    let mut note: u32 = 0;
                    let mut arp_pos = music_speed as i32 - (tick_count % music_speed.max(1)) as i32;
                    if arp_pos > 16 {
                        arp_pos = 2;
                    } else if arp_pos == 16 {
                        arp_pos = 0;
                    } else {
                        arp_pos %= 3;
                    }
                    match arp_pos {
                        1 => note = (chn.n_arpeggio >> 4) as u32,
                        2 => note = (chn.n_arpeggio & 0x0F) as u32,
                        _ => {}
                    }
                    if arp_pos != 0 {
                        note += m.note_from_period(*period as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32);
                        *period = m.period_from_note(note, chn.n_fine_tune as i32, chn.n_c5_speed as u32) as i32;
                        if note >= 108 + NOTE_MIN as u32 {
                            let lim = m.period_from_note(108 + NOTE_MIN as u32, 0, chn.n_c5_speed as u32);
                            *period = (*period as u32).max(lim) as i32;
                        }
                    }
                }
            } else {
                let mut tick = tick_count;
                let mut note: u8 = if t != MOD_TYPE_MOD {
                    chn.n_note
                } else {
                    m.note_from_period(*period as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32) as u8
                };
                if t & (MOD_TYPE_DBM | MOD_TYPE_DIGI) != 0 {
                    tick += 2;
                }
                if t == MOD_TYPE_SFX && tick > 3 {
                    tick ^= 3;
                }
                match tick % 3 {
                    1 => note = note.wrapping_add(chn.n_arpeggio >> 4),
                    2 => note = note.wrapping_add(chn.n_arpeggio & 0x0F),
                    _ => {}
                }
                if note != chn.n_note || t & (MOD_TYPE_DBM | MOD_TYPE_DIGI | MOD_TYPE_STM) != 0 || m.behaviour(KST3PortaAfterArpeggio) {
                    if m.song_flag(SONG_PT_MODE) {
                        if note == NOTE_MIDDLEC + 24 {
                            *period = 65536;
                            return;
                        } else if note > NOTE_MIDDLEC + 24 {
                            note -= 37;
                        }
                    }
                    *period = m.period_from_note(note as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32) as i32;
                    if t & (MOD_TYPE_DBM | MOD_TYPE_DIGI | MOD_TYPE_PSM | MOD_TYPE_STM | MOD_TYPE_OKT | MOD_TYPE_SFX) != 0 {
                        chn.n_period = *period;
                    } else if m.behaviour(KST3PortaAfterArpeggio) {
                        chn.n_arpeggio_last_note = note;
                    }
                }
            }
        } else if chn.row_command.command == CMD_HMN_MEGA_ARP {
            let mut note = m.note_from_period(*period as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32) as u8;
            note = note.wrapping_add(HIS_MASTERS_NOISE_MEGA_ARP[(chn.row_command.param & 0x0F) as usize][(chn.n_arpeggio & 0x0F) as usize] as u8);
            chn.n_arpeggio = chn.n_arpeggio.wrapping_add(1);
            *period = m.period_from_note(note as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32) as i32;
        }
    }

    fn process_vibrato(&mut self, nchn: usize, period: &mut i32) {
        if !self.ps.chn[nchn].has(CHN_VIBRATO) {
            return;
        }
        let t = self.m.mod_type;
        let first = self.ps.flag(SONG_FIRSTTICK);
        let advance = !first || (t & (MOD_TYPE_IT | MOD_TYPE_MPT | MOD_TYPE_MED) != 0 && !self.m.song_flag(SONG_ITOLDEFFECTS));
        if t == MOD_TYPE_669 {
            let chn = &mut self.ps.chn[nchn];
            if chn.n_vibrato_pos % 2 != 0 {
                *period += chn.n_vibrato_depth as i32 * 167;
            }
            chn.n_vibrato_pos = chn.n_vibrato_pos.wrapping_add(1);
            return;
        }
        let it_vtp = self.m.behaviour(kITVibratoTremoloPanbrello);
        if advance && it_vtp {
            let chn = &mut self.ps.chn[nchn];
            chn.n_vibrato_pos = chn.n_vibrato_pos.wrapping_add((4u32 * chn.n_vibrato_speed as u32) as u8);
        }
        let (ty, pos) = (self.ps.chn[nchn].n_vibrato_type, self.ps.chn[nchn].n_vibrato_pos);
        let mut vdelta = self.vibrato_delta(ty, pos as i32);
        if (self.m.song_flag(SONG_PT_MODE) || t & (MOD_TYPE_DIGI | MOD_TYPE_DBM) != 0) && first {
            return;
        } else if t & (MOD_TYPE_XM | MOD_TYPE_MOD) != 0 && (ty & 0x03) == 1 {
            vdelta = -vdelta;
        }
        let vdepth: u32;
        if it_vtp {
            if self.m.song_flag(SONG_ITOLDEFFECTS) {
                vdepth = 5;
            } else {
                vdepth = 6;
                vdelta = -vdelta;
            }
        } else {
            let mut d = if self.m.song_flag(SONG_S3MOLDVIBRATO) {
                5
            } else if t == MOD_TYPE_DTM {
                8
            } else if t & (MOD_TYPE_DBM | MOD_TYPE_MTM) != 0 || (t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 && !self.m.song_flag(SONG_ITOLDEFFECTS)) {
                7
            } else {
                6
            };
            if self.m.behaviour(kST3VibratoMemory) && self.ps.chn[nchn].row_command.command == CMD_FINEVIBRATO {
                d += 2;
            }
            vdepth = d;
        }
        let m = &self.m;
        let chn = &mut self.ps.chn[nchn];
        vdelta = (-vdelta * chn.n_vibrato_depth as i32) / (1 << vdepth);
        *period = m.do_freq_slide(chn, *period, vdelta, false);
        if advance && !it_vtp {
            chn.n_vibrato_pos = chn.n_vibrato_pos.wrapping_add(chn.n_vibrato_speed);
        }
    }

    fn process_sample_auto_vibrato(&mut self, nchn: usize, period: &mut i32, period_frac: &mut i32) {
        let Some(si) = self.ps.chn[nchn].p_mod_sample else {
            return;
        };
        let (vib_depth, vib_sweep, vib_rate, vib_type) = {
            let s = &self.m.samples[si as usize];
            (s.n_vib_depth, s.n_vib_sweep, s.n_vib_rate, s.n_vib_type)
        };
        if vib_depth == 0 {
            return;
        }
        let t = self.m.mod_type;
        let use_freq = self.m.periods_are_frequencies();
        let up: &[u32; 256] = if use_freq { &LINEAR_SLIDE_UP_TABLE } else { &LINEAR_SLIDE_DOWN_TABLE };
        let down: &[u32; 256] = if use_freq { &LINEAR_SLIDE_DOWN_TABLE } else { &LINEAR_SLIDE_UP_TABLE };
        let fine_up: &[u32; 16] = if use_freq { &FINE_LINEAR_SLIDE_UP_TABLE } else { &FINE_LINEAR_SLIDE_DOWN_TABLE };
        let fine_down: &[u32; 16] = if use_freq { &FINE_LINEAR_SLIDE_DOWN_TABLE } else { &FINE_LINEAR_SLIDE_UP_TABLE };
        if self.m.behaviour(kITVibratoTremoloPanbrello) && t != MOD_TYPE_MT2 {
            if vib_rate == 0 {
                return;
            }
            let chn = &mut self.ps.chn[nchn];
            let vibpos = (chn.n_auto_vib_pos & 0xFF) as i32;
            let mut adepth = chn.n_auto_vib_depth;
            adepth += vib_sweep as i32;
            adepth = adepth.min(vib_depth as i32 * 256);
            chn.n_auto_vib_depth = adepth;
            adepth /= 256;
            chn.n_auto_vib_pos = chn.n_auto_vib_pos.wrapping_add(vib_rate);
            let mut vdelta = match vib_type {
                VIB_RANDOM => self.prng.bits7() - 0x40,
                VIB_RAMP_DOWN => 64 - (vibpos + 1) / 2,
                VIB_RAMP_UP => ((vibpos + 1) / 2) - 64,
                VIB_SQUARE => {
                    if vibpos < 128 {
                        64
                    } else {
                        0
                    }
                }
                _ => ITSINUS_TABLE[vibpos as usize] as i32,
            };
            vdelta = (vdelta * adepth) / 64;
            let l = vdelta.unsigned_abs();
            *period = (*period).min(i32::MAX / 256);
            *period *= 256;
            if vdelta < 0 {
                vdelta = muldiv(*period, down[(l / 4) as usize] as i32, 0x10000) - *period;
                if l & 0x03 != 0 {
                    vdelta += muldiv(*period, fine_down[(l & 0x03) as usize] as i32, 0x10000) - *period;
                }
            } else {
                vdelta = muldiv(*period, up[(l / 4) as usize] as i32, 0x10000) - *period;
                if l & 0x03 != 0 {
                    vdelta += muldiv(*period, fine_up[(l & 0x03) as usize] as i32, 0x10000) - *period;
                }
            }
            if i32::MAX - *period >= vdelta {
                *period = (*period + vdelta) / 256;
                *period_frac = vdelta & 0xFF;
            } else {
                *period = i32::MAX / 256;
                *period_frac = 0;
            }
        } else {
            let m = &self.m;
            let chn = &mut self.ps.chn[nchn];
            let mut auto_vib_depth = chn.n_auto_vib_depth;
            let full_depth = vib_depth as i32 * 256;
            if vib_sweep == 0 && t & (MOD_TYPE_IT | MOD_TYPE_MPT) == 0 {
                auto_vib_depth = full_depth;
            } else if t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 {
                auto_vib_depth += vib_sweep as i32 * 2;
                auto_vib_depth = auto_vib_depth.min(full_depth);
                chn.n_auto_vib_depth = auto_vib_depth;
            } else {
                if !chn.has(CHN_KEYOFF) && auto_vib_depth <= full_depth {
                    auto_vib_depth += full_depth / vib_sweep as i32;
                    chn.n_auto_vib_depth = auto_vib_depth;
                }
                if auto_vib_depth > full_depth {
                    auto_vib_depth = full_depth;
                } else if chn.has(CHN_KEYOFF) && m.behaviour(kFT2AutoVibratoAbortSweep) {
                    auto_vib_depth = full_depth / vib_sweep as i32;
                }
            }
            chn.n_auto_vib_pos = chn.n_auto_vib_pos.wrapping_add(vib_rate);
            let vdelta: i32 = match vib_type {
                VIB_RANDOM => {
                    let v = MOD_RANDOM_TABLE[(chn.n_auto_vib_pos & 0x3F) as usize] as i32;
                    chn.n_auto_vib_pos = chn.n_auto_vib_pos.wrapping_add(1);
                    v
                }
                VIB_RAMP_DOWN => ((0x40i32.wrapping_sub((chn.n_auto_vib_pos as u32 / 2) as i32)) & 0x7F) - 0x40,
                VIB_RAMP_UP => ((0x40 + (chn.n_auto_vib_pos as u32 / 2) as i32) & 0x7F) - 0x40,
                VIB_SQUARE => {
                    if chn.n_auto_vib_pos & 128 != 0 {
                        64
                    } else {
                        -64
                    }
                }
                _ => {
                    if t != MOD_TYPE_MT2 {
                        -(ITSINUS_TABLE[(chn.n_auto_vib_pos & 0xFF) as usize] as i32)
                    } else {
                        (-(ITSINUS_TABLE[((chn.n_auto_vib_pos as u32 + 192) & 0xFF) as usize] as i32) + 64) / 2
                    }
                }
            };
            let mut n = (vdelta * auto_vib_depth) / 256;
            if t != MOD_TYPE_XM {
                let (df1, df2);
                if n < 0 {
                    n = -n;
                    let n1 = (n / 256) as usize;
                    df1 = down[n1.min(255)] as i32;
                    df2 = down[(n1 + 1).min(255)] as i32;
                } else {
                    let n1 = (n / 256) as usize;
                    df1 = up[n1.min(255)] as i32;
                    df2 = up[(n1 + 1).min(255)] as i32;
                }
                n /= 4;
                *period = muldiv(*period, df1 + ((df2 - df1) * (n & 0x3F) / 64), 256);
                *period_frac = *period & 0xFF;
                *period /= 256;
            } else {
                *period += n / 64;
            }
        }
    }

    /// `ProcessRamping`.
    fn process_ramping(&mut self, nchn: usize) {
        let rup = self.settings.ramp_up_samples();
        let rdown = self.settings.ramp_down_samples();
        let freq = self.settings.mixing_freq;
        let buffer_count = self.ps.buffer_count as i32;
        let m = &self.m;
        let chn = &mut self.ps.chn[nchn];
        chn.left_ramp = 0;
        chn.right_ramp = 0;
        chn.new_left_vol = chn.new_left_vol.min(i32::MAX >> VOLUMERAMPPRECISION);
        chn.new_right_vol = chn.new_right_vol.min(i32::MAX >> VOLUMERAMPPRECISION);
        if chn.has(CHN_VOLUMERAMP) && (chn.left_vol != chn.new_left_vol || chn.right_vol != chn.new_right_vol) {
            let ramp_up = chn.new_left_vol > chn.left_vol || chn.new_right_vol > chn.right_vol;
            let mut ramp_length;
            let mut global_ramp_length;
            ramp_length = if ramp_up { rup } else { rdown };
            global_ramp_length = ramp_length;
            if m.behaviour(kFT2VolumeRamping) && m.mod_type & MOD_TYPE_XM != 0 {
                ramp_length = muldivr(5, freq as i32, 1000);
                global_ramp_length = ramp_length;
            }
            let mut instr_ramp_length = 0i32;
            if ramp_up {
                if let Some(ins) = chn.p_mod_instrument.and_then(|i| m.instrument(i as u32)) {
                    instr_ramp_length = ins.n_vol_ramp_up as i32;
                    ramp_length = if instr_ramp_length != 0 {
                        ((freq as u64 * instr_ramp_length as u64) / 100000) as i32
                    } else {
                        global_ramp_length
                    };
                }
            }
            let custom = instr_ramp_length > 0;
            if ramp_length == 0 {
                ramp_length = 1;
            }
            let left_delta = (chn.new_left_vol - chn.left_vol).wrapping_mul(1 << VOLUMERAMPPRECISION);
            let right_delta = (chn.new_right_vol - chn.right_vol).wrapping_mul(1 << VOLUMERAMPPRECISION);
            if !custom && (chn.left_vol | chn.right_vol) != 0 && (chn.new_left_vol | chn.new_right_vol) != 0 && !chn.has(CHN_FASTVOLRAMP) {
                ramp_length = buffer_count.clamp(global_ramp_length, 1 << (VOLUMERAMPPRECISION - 1));
            }
            chn.left_ramp = left_delta / ramp_length;
            chn.right_ramp = right_delta / ramp_length;
            chn.left_vol = chn.new_left_vol - ((chn.left_ramp.wrapping_mul(ramp_length)) / (1 << VOLUMERAMPPRECISION));
            chn.right_vol = chn.new_right_vol - ((chn.right_ramp.wrapping_mul(ramp_length)) / (1 << VOLUMERAMPPRECISION));
            if (chn.left_ramp | chn.right_ramp) != 0 {
                chn.n_ramp_length = ramp_length as u32;
            } else {
                chn.reset_flag(CHN_VOLUMERAMP);
                chn.left_vol = chn.new_left_vol;
                chn.right_vol = chn.new_right_vol;
            }
        } else {
            chn.reset_flag(CHN_VOLUMERAMP);
            chn.left_vol = chn.new_left_vol;
            chn.right_vol = chn.new_right_vol;
        }
        chn.ramp_left_vol = chn.left_vol.wrapping_mul(1 << VOLUMERAMPPRECISION);
        chn.ramp_right_vol = chn.right_vol.wrapping_mul(1 << VOLUMERAMPPRECISION);
        chn.reset_flag(CHN_FASTVOLRAMP);
    }

    /// `HandleNoteChangeFilter`.
    fn handle_note_change_filter(&mut self, nchn: usize) -> i32 {
        let m = &self.m;
        let freq = self.settings.mixing_freq;
        let mpt_filter = self.ps.flag(SONG_MPTFILTERMODE);
        let chn = &mut self.ps.chn[nchn];
        let mut cutoff = -1;
        if !chn.trigger_note {
            return cutoff;
        }
        let mut use_filter = !mpt_filter;
        if let Some(ins) = chn.p_mod_instrument.and_then(|i| m.instrument(i as u32)) {
            if ins.is_resonance_enabled() {
                chn.n_resonance = ins.resonance();
                use_filter = true;
            }
            if ins.is_cutoff_enabled() {
                chn.n_cut_off = ins.cutoff();
                use_filter = true;
            }
            if use_filter && ins.filter_mode != FilterMode::Unchanged {
                chn.n_filter_mode = ins.filter_mode;
            }
        } else {
            chn.n_vol_swing = 0;
            chn.n_pan_swing = 0;
            chn.n_cut_swing = 0;
            chn.n_res_swing = 0;
        }
        if (chn.n_cut_off < 0x7F || m.behaviour(kITFilterBehaviour)) && use_filter {
            cutoff = m.setup_channel_filter(freq, chn, true, 256);
            if cutoff >= 0 {
                cutoff = (chn.n_cut_off / 2) as i32;
            }
        }
        cutoff
    }

    /// `GetChannelIncrement`: (increment, frequency).
    fn channel_increment(&self, chn: &ModChannel, period: u32, period_frac: i32) -> (SamplePosition, u32) {
        let mut freq = self.m.freq_from_period(period, chn.n_c5_speed as u32, period_frac);
        let ins = chn.p_mod_instrument.and_then(|i| self.m.instrument(i as u32));
        let mut finetune = chn.micro_tuning as i32;
        if finetune != 0 {
            if let Some(ins) = ins {
                finetune *= ins.midi_pwd as i32;
            }
            if finetune != 0 {
                let f = (freq as f64 * 2.0f64.powf(finetune as f64 / (12.0 * 256.0 * 128.0))).round();
                freq = f.clamp(0.0, u32::MAX as f64) as u32;
            }
        }
        freq = freq.min(i32::MAX as u32);
        (SamplePosition::ratio(freq, self.settings.mixing_freq << FREQ_FRACBITS), freq)
    }

    /// `ProcessMacroOnChannel`.
    fn process_macro_on_channel(&mut self, nchn: usize) {
        if nchn >= self.num_channels() {
            return;
        }
        let rc = self.ps.chn[nchn].row_command;
        if (rc.command == CMD_MIDI && self.ps.flag(SONG_FIRSTTICK)) || rc.command == CMD_SMOOTHMIDI {
            let smooth = rc.command == CMD_SMOOTHMIDI;
            let mac = if rc.param < 0x80 {
                self.m.midi_cfg.sfx[(self.ps.chn[nchn].n_active_macro & 0x0F) as usize]
            } else {
                self.m.midi_cfg.zxx[(rc.param & 0x7F) as usize]
            };
            self.process_midi_macro(nchn, smooth, &mac, rc.param);
        }
    }

    /// `GetTickDuration`.
    pub fn tick_duration(&mut self) -> u32 {
        let freq = self.settings.mixing_freq;
        let tempo = self.ps.music_tempo.raw();
        let mut ret = match self.m.tempo_mode {
            TempoMode::Classic => muldiv_unsigned(freq, 5 * Tempo::FRACT_FACT, (tempo << 1).max(1)),
            TempoMode::Alternative => muldiv_unsigned(freq, Tempo::FRACT_FACT, tempo.max(1)),
            TempoMode::Modern => {
                let rpb = self.ps.current_rows_per_beat as u64;
                let acc = freq as f64 * (60.0 / (self.ps.music_tempo.to_double() * (self.ps.music_speed as u64 * rpb) as f64));
                let mut bc = acc as i32 as u32;
                self.ps.buffer_diff += acc - bc as f64;
                if self.ps.buffer_diff >= 1.0 {
                    bc = bc.wrapping_add(1);
                    self.ps.buffer_diff -= 1.0;
                } else if self.ps.buffer_diff <= -1.0 {
                    bc = bc.wrapping_sub(1);
                    self.ps.buffer_diff += 1.0;
                }
                bc
            }
        };
        ret = muldivr_unsigned(ret, self.tempo_factor, 65536);
        if ret == 0 {
            ret = 1;
        }
        ret
    }

    /// `ReadNote`.
    pub fn read_note(&mut self) -> bool {
        if !self.process_row() {
            return false;
        }
        if self.ps.music_tempo.raw() == 0 {
            return false;
        }
        self.ps.samples_per_tick = self.tick_duration();
        self.ps.buffer_count = self.ps.samples_per_tick;

        let nc = self.num_channels();
        let master_vol: u32 = {
            let nchn32 = nc.clamp(1, 31) as u32;
            let mut mastervol;
            if self.m.play_config.use_global_pre_amp {
                let mut real = self.settings.pre_amp as i32;
                if real > 0x80 {
                    real = 0x80 + ((real - 0x80) * (nchn32 as i32 + 4)) / 16;
                }
                mastervol = (real as u32 * self.m.sample_pre_amp) / 64;
            } else {
                mastervol = self.m.sample_pre_amp;
            }
            if self.m.play_config.use_global_pre_amp {
                let mut att = PREAMP_TABLE[(nchn32 / 2) as usize] as u32;
                if att < 1 {
                    att = 1;
                }
                mastervol = (mastervol << 7) / att;
            }
            mastervol
        };

        self.n_mix_channels = 0;
        for nchn in 0..self.ps.chn.len() {
            {
                let chn = &mut self.ps.chn[nchn];
                if chn.n_master_chn != 0 && nchn < nc && chn.n_restore_pan_on_new_note < u16::MAX {
                    chn.n_restore_pan_on_new_note += 1; // nnaChannelAge
                }
                if chn.has(CHN_MUTE) || (nchn >= nc && chn.n_length == 0) {
                    chn.n_left_vu = 0;
                    chn.n_right_vu = 0;
                    if nchn < nc {
                        self.process_macro_on_channel(nchn);
                    }
                    continue;
                }
                chn.increment = SamplePosition(0);
                chn.n_real_volume = 0;
                chn.n_calc_volume = 0;
                chn.n_ramp_length = 0;
            }
            let p_ins = self.ps.chn[nchn].p_mod_instrument;
            let mut period: i32 = 0;
            let sample_playing = self.ps.chn[nchn].n_period != 0 && self.ps.chn[nchn].n_length != 0;
            if sample_playing {
                let it_swing = self.m.behaviour(kITSwingBehaviour);
                let mut vol;
                let mut ins_vol;
                {
                    let m = &self.m;
                    let chn = &mut self.ps.chn[nchn];
                    vol = chn.n_volume;
                    ins_vol = chn.n_ins_vol as i32;
                    // ProcessVolumeSwing
                    if it_swing {
                        ins_vol += chn.n_vol_swing as i32;
                        ins_vol = ins_vol.clamp(0, 64);
                    } else if m.behaviour(kMPTOldSwingBehaviour) {
                        vol += chn.n_vol_swing as i32;
                        vol = vol.clamp(0, 256);
                    } else {
                        chn.n_volume += chn.n_vol_swing as i32;
                        chn.n_volume = chn.n_volume.clamp(0, 256);
                        vol = chn.n_volume;
                        chn.n_vol_swing = 0;
                    }
                    // ProcessPanningSwing
                    if it_swing || m.behaviour(kMPTOldSwingBehaviour) {
                        chn.n_real_pan = (chn.n_pan + chn.n_pan_swing as i32).clamp(0, 256);
                    } else {
                        chn.n_pan += chn.n_pan_swing as i32;
                        chn.n_pan = chn.n_pan.clamp(0, 256);
                        chn.n_pan_swing = 0;
                        chn.n_real_pan = chn.n_pan;
                    }
                }
                self.process_tremolo(nchn, &mut vol);
                self.process_tremor(nchn, &mut vol);
                vol = vol.clamp(0, 256);
                vol <<= 6;
                let first_tick = self.ps.flag(SONG_FIRSTTICK);
                let global_volume = self.ps.global_volume;
                {
                    let m = &self.m;
                    let chn = &mut self.ps.chn[nchn];
                    if p_ins.is_some() {
                        if m.behaviour(kITEnvelopePositionHandling) {
                            Self::increment_envelope_positions(m, chn);
                        }
                        Self::process_volume_envelope(m, chn, &mut vol);
                        Self::process_instrument_fade(m, chn, &mut vol);
                        Self::process_panning_envelope(m, chn);
                        if !m.behaviour(kITPitchPanSeparation) && chn.n_note != NOTE_NONE {
                            if let Some(ins) = chn.p_mod_instrument.and_then(|i| m.instrument(i as u32)) {
                                if ins.n_pps != 0 {
                                    let note = chn.n_note as i32;
                                    Module::pitch_pan_separation(&mut chn.n_real_pan, note, ins);
                                }
                            }
                        }
                    } else if chn.has(CHN_NOTEFADE) {
                        chn.n_fade_out_vol = 0;
                        vol = 0;
                    }
                    if chn.is_paused {
                        vol = 0;
                    }
                    if vol != 0 {
                        if chn.has(CHN_SYNCMUTE) {
                            chn.n_real_volume = 0;
                        } else if m.play_config.global_volume_applies_to_master {
                            chn.n_real_volume = muldiv(vol * MAX_GLOBAL_VOLUME as i32, chn.n_global_vol as i32 * ins_vol, 1 << 20);
                        } else {
                            chn.n_real_volume = muldiv(vol * global_volume, chn.n_global_vol as i32 * ins_vol, 1 << 20);
                        }
                    }
                    chn.n_calc_volume = vol;
                    let t = m.mod_type;
                    let pf = m.periods_are_frequencies();
                    if chn.n_period < m.min_period && t != MOD_TYPE_S3M && !pf {
                        chn.n_period = m.min_period;
                    } else if chn.n_period >= m.max_period && m.behaviour(kApplyUpperPeriodLimit) && !pf {
                        chn.n_period = m.max_period;
                    }
                    period = chn.n_period;
                    if (chn.dw_flags & (CHN_GLISSANDO | CHN_PORTAMENTO)) == (CHN_GLISSANDO | CHN_PORTAMENTO)
                        && (!m.song_flag(SONG_PT_MODE) || (chn.row_command.is_tone_portamento() && !first_tick))
                    {
                        if period != chn.cached_period {
                            chn.cached_period = period;
                            let n = m.note_from_period(period as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32);
                            chn.glissando_period = m.period_from_note(n, chn.n_fine_tune as i32, chn.n_c5_speed as u32) as i32;
                        }
                        period = chn.glissando_period;
                    }
                }
                self.process_arpeggio(nchn, &mut period);
                {
                    let m = &self.m;
                    let chn = &mut self.ps.chn[nchn];
                    if (m.song_flag(SONG_AMIGALIMITS) || m.song_flag(SONG_PT_MODE)) && period != i32::MAX {
                        let mut limit_low = 113 * 4;
                        let mut limit_high = 856 * 4;
                        if m.mod_type != MOD_TYPE_S3M {
                            let off = (xm2mod_finetune(chn.n_fine_tune as i32) * 12) as usize;
                            limit_low = PRO_TRACKER_TUNED_PERIODS[off + 11] as i32 / 2;
                            limit_high = PRO_TRACKER_TUNED_PERIODS[off] as i32 * 2;
                            if limit_low < 113 * 4 {
                                limit_low = 113 * 4;
                            }
                        }
                        period = period.clamp(limit_low, limit_high);
                        chn.n_period = chn.n_period.clamp(limit_low, limit_high);
                    }
                }
                self.process_panbrello(nchn);
            }

            if self.ps.chn[nchn].has(CHN_SURROUND) && !self.ps.flag(SONG_SURROUNDPAN) && self.m.behaviour(kITNoSurroundPan) {
                self.ps.chn[nchn].n_real_pan = 128;
            }

            let cutoff = self.handle_note_change_filter(nchn);
            if cutoff >= 0 && self.ps.chn[nchn].has(CHN_ADLIB) {
                if let Some(opl) = &mut self.opl { opl.volume(nchn, cutoff as u8, true); }
            }
            self.process_macro_on_channel(nchn);

            if sample_playing {
                let cutoff = self.process_pitch_filter_envelope(nchn, &mut period);
                if cutoff >= 0 && self.ps.chn[nchn].has(CHN_ADLIB) {
                    if let Some(opl) = &mut self.opl { opl.volume(nchn, (cutoff / 4) as u8, true); }
                }
            }

            {
                let rc = self.ps.chn[nchn].row_command;
                if rc.volcmd == VOLCMD_VIBRATODEPTH && (rc.command == CMD_VIBRATO || rc.command == CMD_VIBRATOVOL || rc.command == CMD_FINEVIBRATO) {
                    let t = self.m.mod_type;
                    if t == MOD_TYPE_XM {
                        if !self.ps.flag(SONG_FIRSTTICK) {
                            let chn = &mut self.ps.chn[nchn];
                            chn.n_vibrato_pos = chn.n_vibrato_pos.wrapping_add(chn.n_vibrato_speed);
                        }
                    } else if t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 {
                        self.m.vibrato(&mut self.ps.chn[nchn], rc.vol as u32);
                        self.process_vibrato(nchn, &mut period);
                    }
                }
            }
            self.process_vibrato(nchn, &mut period);

            if sample_playing {
                let mut period_frac = 0;
                self.process_sample_auto_vibrato(nchn, &mut period, &mut period_frac);
                let m = &self.m;
                if period <= m.min_period {
                    if m.behaviour(kST3LimitPeriod) {
                        self.ps.chn[nchn].n_length = 0;
                    }
                    period = m.min_period;
                }
                let (mut ninc, freq) = self.channel_increment(&self.ps.chn[nchn], period as u32, period_frac);
                ninc.mul_div(self.freq_factor, 65536);
                if ninc.is_zero() {
                    ninc = SamplePosition::new(0, 1);
                }
                self.ps.chn[nchn].increment = ninc;
                let chn = &mut self.ps.chn[nchn];
                if chn.dw_flags & (CHN_ADLIB | CHN_MUTE | CHN_SYNCMUTE) == CHN_ADLIB {
                    if let Some(opl) = &mut self.opl {
                        let process = m.behaviour(kOPLFlexibleNoteOff) || !chn.has(CHN_NOTEFADE) || m.mod_type == MOD_TYPE_S3M;
                        if process && !(m.mod_type == MOD_TYPE_S3M && chn.has(CHN_KEYOFF)) {
                            let pitch = muldivr_unsigned(freq, 261625, 8363 << FREQ_FRACBITS);
                            let pitch = muldivr_unsigned(pitch, self.freq_factor, 65536);
                            let key_off = chn.has(CHN_KEYOFF) || (chn.has(CHN_NOTEFADE) && chn.n_fade_out_vol == 0);
                            if !m.behaviour(kOPLNoteStopWith0Hz) || !key_off {
                                opl.frequency(nchn, pitch, key_off, m.behaviour(kOPLBeatingOscillators));
                            }
                        }
                        if process {
                            let vol = chn.n_calc_volume as u32 * chn.n_global_vol as u32 * chn.n_ins_vol as u32;
                            opl.volume(nchn, muldivr_unsigned(vol, 63, 1 << 26) as u8, false);
                            chn.n_real_pan = opl.pan(nchn, chn.n_real_pan) * 128 + 128;
                        }
                    }
                }
            }

            {
                let m = &self.m;
                let chn = &mut self.ps.chn[nchn];
                if p_ins.is_some() && !m.behaviour(kITEnvelopePositionHandling) {
                    Self::increment_envelope_positions(m, chn);
                }
                if chn.has(CHN_NOTEFADE) && (chn.n_fade_out_vol | chn.left_vol | chn.right_vol) == 0 && !m.behaviour(kFT2ProcessSilentChannels) {
                    chn.n_length = 0;
                    chn.n_r_ofs = 0;
                    chn.n_l_ofs = 0;
                }
                let ramp = (chn.n_real_volume | chn.right_vol | chn.left_vol) != 0 && !chn.has(CHN_ADLIB);
                chn.set_to(CHN_VOLUMERAMP, ramp);
                chn.n_left_vu = chn.n_left_vu.saturating_sub(4);
                chn.n_right_vu = chn.n_right_vu.saturating_sub(4);
                chn.new_left_vol = 0;
                chn.new_right_vol = 0;
                let has_data = chn.p_mod_sample.is_some_and(|s| m.samples[s as usize].has_sample_data());
                chn.p_current_sample = if has_data && chn.n_length != 0 && chn.is_sample_playing() {
                    Some(crate::channel::CurrentSample { sample: chn.p_mod_sample.unwrap(), offset: 0 })
                } else {
                    None
                };
            }

            if self.ps.chn[nchn].p_current_sample.is_some() {
                let stereo = self.settings.channels >= 2;
                let src_mode = self.settings.src_mode;
                let extra = self.m.play_config.extra_sample_attenuation;
                let panning_mode = self.m.play_config.panning_mode;
                let use_pre_amp = self.m.play_config.use_global_pre_amp;
                {
                    let m = &self.m;
                    let chn = &mut self.ps.chn[nchn];
                    let mut pan = if stereo { chn.n_real_pan.clamp(0, 256) } else { 128 };
                    let mut realvol = ((chn.n_real_volume as i64 * master_vol as i64) / 128) as i32;
                    if !use_pre_amp {
                        realvol /= 2;
                    }
                    match panning_mode {
                        PanningMode::SoftPanning => {
                            if pan < 128 {
                                chn.new_left_vol = (realvol * 128) / 256;
                                chn.new_right_vol = (realvol * pan) / 256;
                            } else {
                                chn.new_left_vol = (realvol * (256 - pan)) / 256;
                                chn.new_right_vol = (realvol * 128) / 256;
                            }
                        }
                        PanningMode::FT2Panning => {
                            pan = pan.min(255);
                            let pan_l = if pan > 0 { XMPANNING_TABLE[(256 - pan) as usize] as i64 } else { 65536 };
                            let pan_r = XMPANNING_TABLE[pan as usize] as i64;
                            chn.new_left_vol = ((realvol as i64 * pan_l) / 65536) as i32;
                            chn.new_right_vol = ((realvol as i64 * pan_r) / 65536) as i32;
                        }
                        _ => {
                            chn.new_left_vol = (realvol * (256 - pan)) / 256;
                            chn.new_right_vol = (realvol * pan) / 256;
                        }
                    }
                    let ins_mode = chn.p_mod_instrument.and_then(|i| m.instrument(i as u32)).map(|i| i.resampling);
                    chn.resampling_mode = if let Some(r) = ins_mode.filter(|r| *r < SRCMODE_DEFAULT) {
                        r
                    } else if m.resampling < SRCMODE_DEFAULT {
                        m.resampling
                    } else {
                        src_mode
                    };
                    if chn.increment.is_unity() && !(chn.has(CHN_VIBRATO) || chn.n_auto_vib_depth != 0) {
                        chn.resampling_mode = SRCMODE_NEAREST;
                    }
                    chn.new_left_vol /= 1 << extra;
                    chn.new_right_vol /= 1 << extra;
                    if chn.has(CHN_SURROUND) && self.settings.channels == 2 {
                        chn.new_right_vol = -chn.new_right_vol;
                    }
                    if chn.has(CHN_PINGPONGFLAG) {
                        chn.increment.negate();
                    }
                }
                self.process_ramping(nchn);
                if !self.ps.chn[nchn].has(CHN_ADLIB) {
                    self.ps.chn_mix[self.n_mix_channels] = nchn as ChannelIndex;
                    self.n_mix_channels += 1;
                }
                let chn = &mut self.ps.chn[nchn];
                if nchn >= nc && !(chn.n_volume != 0 && chn.n_global_vol != 0 && chn.n_ins_vol != 0) {
                    chn.n_length = 0;
                }
            } else if !self.ps.chn[nchn].has(CHN_ADLIB) {
                let chn = &mut self.ps.chn[nchn];
                chn.right_vol = 0;
                chn.left_vol = 0;
                chn.n_length = 0;
                if chn.n_l_ofs != 0 || chn.n_r_ofs != 0 {
                    self.ps.chn_mix[self.n_mix_channels] = nchn as ChannelIndex;
                    self.n_mix_channels += 1;
                }
            }
            let chn = &mut self.ps.chn[nchn];
            chn.dw_old_flags = chn.dw_flags;
            chn.trigger_note = false;
        }
        if self.n_mix_channels >= self.settings.max_mix_channels {
            let chn = &self.ps.chn;
            self.ps.chn_mix[..self.n_mix_channels].sort_by(|&a, &b| chn[b as usize].n_real_volume.cmp(&chn[a as usize].n_real_volume));
        }
        true
    }

    /// `FadeSong`.
    pub fn fade_song(&mut self, msec: u32) -> bool {
        let mut nsamples = muldiv(msec as i32, self.settings.mixing_freq as i32, 1000);
        if nsamples <= 0 {
            return false;
        }
        if nsamples > 0x100000 {
            nsamples = 0x100000;
        }
        self.ps.buffer_count = nsamples as u32;
        let ramp_length = nsamples;
        for i in 0..self.n_mix_channels {
            let ci = self.ps.chn_mix[i] as usize;
            let c = &mut self.ps.chn[ci];
            c.new_right_vol = 0;
            c.new_left_vol = 0;
            c.left_ramp = -c.left_vol.wrapping_mul(1 << VOLUMERAMPPRECISION) / ramp_length;
            c.right_ramp = -c.right_vol.wrapping_mul(1 << VOLUMERAMPPRECISION) / ramp_length;
            c.ramp_left_vol = c.left_vol.wrapping_mul(1 << VOLUMERAMPPRECISION);
            c.ramp_right_vol = c.right_vol.wrapping_mul(1 << VOLUMERAMPPRECISION);
            c.n_ramp_length = ramp_length as u32;
            c.set(CHN_VOLUMERAMP);
        }
        true
    }

    /// `ProcessGlobalVolume` (stereo).
    fn process_global_volume(&mut self, count: usize) {
        let ps = &mut self.ps;
        if ps.total_sample_count == 0 {
            ps.global_volume_destination = ps.global_volume;
            ps.samples_to_global_vol_ramp_dest = 0;
            ps.global_volume_ramp_amount = 0;
        } else if ps.global_volume_destination != ps.global_volume {
            let ramp_up = ps.global_volume > ps.global_volume_destination;
            ps.global_volume_destination = ps.global_volume;
            let n = if ramp_up { self.settings.ramp_up_samples() } else { self.settings.ramp_down_samples() };
            ps.samples_to_global_vol_ramp_dest = n;
            ps.global_volume_ramp_amount = n;
        }
        let mut step = 0;
        if ps.samples_to_global_vol_ramp_dest > 0 {
            let dest = ps.global_volume_destination << VOLUMERAMPPRECISION;
            let delta = dest - ps.high_res_ramping_global_volume;
            step = delta / ps.samples_to_global_vol_ramp_dest;
            if self.m.mix_levels == crate::sndfile::MixLevels::V117RC2 {
                let max_step = 50i32.max(10000 / (ps.global_volume_ramp_amount + 1));
                while step.abs() > max_step {
                    ps.samples_to_global_vol_ramp_dest += ps.global_volume_ramp_amount;
                    step = delta / ps.samples_to_global_vol_ramp_dest;
                }
            }
        }
        let gv = ps.global_volume;
        let buf = &mut self.mix_buffer[..count * 2];
        for frame in buf.chunks_exact_mut(2) {
            if ps.samples_to_global_vol_ramp_dest > 0 {
                ps.high_res_ramping_global_volume += step;
                let v = ps.high_res_ramping_global_volume;
                let den = (MAX_GLOBAL_VOLUME as i32) << VOLUMERAMPPRECISION;
                frame[0] = muldiv(frame[0], v, den);
                frame[1] = muldiv(frame[1], v, den);
                ps.samples_to_global_vol_ramp_dest -= 1;
            } else {
                frame[0] = muldiv(frame[0], gv, MAX_GLOBAL_VOLUME as i32);
                frame[1] = muldiv(frame[1], gv, MAX_GLOBAL_VOLUME as i32);
                ps.high_res_ramping_global_volume = gv << VOLUMERAMPPRECISION;
            }
        }
    }

    /// `Read`: renders up to `out.len() / 2` stereo frames as float
    /// (`int * 2^-27`, libopenmpt's float conversion). Returns frames
    /// rendered; fewer than asked means the song (and its fade) ended.
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        let count = out.len() / 2;
        let mut rendered = 0usize;
        let mut to_render = count;
        while !self.ps.flag(SONG_ENDREACHED) && to_render > 0 {
            if self.ps.buffer_count == 0 {
                if self.ps.flag(SONG_FADINGSONG) {
                    self.ps.flags |= SONG_ENDREACHED;
                } else if self.read_note() {
                } else if self.is_rendering {
                    self.ps.flags |= SONG_ENDREACHED;
                } else if self.fade_song(FADESONGDELAY) {
                    self.ps.flags |= SONG_FADINGSONG;
                } else {
                    self.ps.flags |= SONG_ENDREACHED;
                }
            }
            if self.ps.flag(SONG_ENDREACHED) {
                self.ps.tick_count = self.ps.ticks_on_row();
                break;
            }
            let chunk = MIXBUFFERSIZE.min(self.ps.buffer_count as usize).min(to_render);
            self.create_stereo_mix(chunk, false);
            if let Some(opl) = &mut self.opl {
                opl.mix(&mut self.mix_buffer[..chunk * 2], self.m.vsti_volume);
            }
            if self.m.play_config.global_volume_applies_to_master {
                self.process_global_volume(chunk);
            }
            if self.settings.stereo_separation != 128 {
                apply_stereo_separation(&mut self.mix_buffer[..chunk * 2], self.settings.stereo_separation);
            }
            const SCALE: f32 = 1.0 / (1u32 << MIXING_FRACTIONAL_BITS) as f32;
            for (o, &v) in out[rendered * 2..(rendered + chunk) * 2].iter_mut().zip(&self.mix_buffer[..chunk * 2]) {
                *o = v as f32 * SCALE;
            }
            rendered += chunk;
            to_render -= chunk;
            self.ps.buffer_count -= chunk as u32;
            self.ps.total_sample_count += chunk as u64;
        }
        rendered
    }

    /// Plays the rest of the current tick without output, for seeking:
    /// every channel advances as when silent (`MixChannel`'s unmixed path)
    /// and the global volume ramp steps as in `read`.
    pub fn skip_buffer(&mut self) {
        while self.ps.buffer_count > 0 {
            let chunk = MIXBUFFERSIZE.min(self.ps.buffer_count as usize);
            self.create_stereo_mix(chunk, true);
            if let Some(opl) = &mut self.opl {
                opl.mix(&mut self.mix_buffer[..chunk * 2], self.m.vsti_volume);
            }
            if self.m.play_config.global_volume_applies_to_master {
                self.process_global_volume(chunk);
            }
            self.ps.buffer_count -= chunk as u32;
            self.ps.total_sample_count += chunk as u64;
        }
    }
}

/// `ApplyStereoSeparation` (integer mixer).
fn apply_stereo_separation(buf: &mut [i32], separation: i32) {
    for f in buf.chunks_exact_mut(2) {
        let (l, r) = (f[0], f[1]);
        let mut mid = l.wrapping_add(r);
        let mut side = l.wrapping_sub(r);
        mid /= 2;
        side = muldiv(side, separation, 128 * 2);
        f[0] = mid.wrapping_add(side);
        f[1] = mid.wrapping_sub(side);
    }
}

/// `HisMastersNoiseMegaArp` (Tables.cpp).
pub static HIS_MASTERS_NOISE_MEGA_ARP: [[i8; 16]; 16] = [
    [0, 3, 7, 12, 15, 12, 7, 3, 0, 3, 7, 12, 15, 12, 7, 3],
    [0, 4, 7, 12, 16, 12, 7, 4, 0, 4, 7, 12, 16, 12, 7, 4],
    [0, 3, 8, 12, 15, 12, 8, 3, 0, 3, 8, 12, 15, 12, 8, 3],
    [0, 4, 8, 12, 16, 12, 8, 4, 0, 4, 8, 12, 16, 12, 8, 4],
    [0, 5, 8, 12, 17, 12, 8, 5, 0, 5, 8, 12, 17, 12, 8, 5],
    [0, 5, 9, 12, 17, 12, 9, 5, 0, 5, 9, 12, 17, 12, 9, 5],
    [12, 0, 7, 0, 3, 0, 7, 0, 12, 0, 7, 0, 3, 0, 7, 0],
    [12, 0, 7, 0, 4, 0, 7, 0, 12, 0, 7, 0, 4, 0, 7, 0],
    [0, 3, 7, 3, 7, 12, 7, 12, 15, 12, 7, 12, 7, 3, 7, 3],
    [0, 4, 7, 4, 7, 12, 7, 12, 16, 12, 7, 12, 7, 4, 7, 4],
    [31, 27, 24, 19, 15, 12, 7, 3, 0, 3, 7, 12, 15, 19, 24, 27],
    [31, 28, 24, 19, 16, 12, 7, 4, 0, 4, 7, 12, 16, 19, 24, 28],
    [0, 12, 0, 12, 0, 12, 0, 12, 0, 12, 0, 12, 0, 12, 0, 12],
    [0, 12, 24, 12, 0, 12, 24, 12, 0, 12, 24, 12, 0, 12, 24, 12],
    [0, 3, 0, 3, 0, 3, 0, 3, 0, 3, 0, 3, 0, 3, 0, 3],
    [0, 4, 0, 4, 0, 4, 0, 4, 0, 4, 0, 4, 0, 4, 0, 4],
];
