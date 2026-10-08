//! Mixing channels, ported from libopenmpt 0.8.9 `soundlib/ModChannel.h/.cpp`.
//!
//! Pointers become indices: `p_mod_sample` into `Module::samples`,
//! `p_mod_instrument` into `Module::instruments`, and `p_current_sample`
//! names the sample whose buffer is being read plus a frame offset into it
//! (the loop lookahead areas sit behind the waveform).
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

#![allow(dead_code)]

use crate::command::*;
use crate::defs::*;
use crate::sndfile::Module;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CurrentSample {
    pub sample: SampleIndex,
    /// Frames from the sample's first sampling point.
    pub offset: isize,
}

/// `AutoSlideCommand`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoSlide {
    TonePortamento,
    TonePortamentoWithDuration,
    PortamentoUp,
    PortamentoDown,
    FinePortamentoUp,
    FinePortamentoDown,
    PortamentoFC,
    FineVolumeSlideUp,
    FineVolumeSlideDown,
    VolumeDownETX,
    VolumeSlideSTK,
    VolumeDownWithDuration,
    GlobalVolumeSlide,
    Vibrato,
    Tremolo,
}

/// `ModChannel::AutoSlideStatus`.
#[derive(Clone, Copy, Debug, Default)]
pub struct AutoSlideStatus(u16);

impl AutoSlideStatus {
    pub fn any_active(&self) -> bool {
        self.0 != 0
    }
    pub fn is_active(&self, c: AutoSlide) -> bool {
        self.0 & (1 << c as u16) != 0
    }
    pub fn set_active(&mut self, c: AutoSlide, on: bool) {
        if on {
            self.0 |= 1 << c as u16;
        } else {
            self.0 &= !(1 << c as u16);
        }
    }
    pub fn reset(&mut self) {
        self.0 = 0;
    }
    pub fn any_pitch_slide_active(&self) -> bool {
        self.is_active(AutoSlide::TonePortamento)
            || self.is_active(AutoSlide::PortamentoUp)
            || self.is_active(AutoSlide::PortamentoDown)
            || self.is_active(AutoSlide::FinePortamentoUp)
            || self.is_active(AutoSlide::FinePortamentoDown)
            || self.is_active(AutoSlide::PortamentoFC)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EnvInfo {
    pub n_env_position: u32,
    pub n_env_value_at_release_jump: i16,
    pub flags: u8,
}

impl Default for EnvInfo {
    fn default() -> Self {
        EnvInfo { n_env_position: 0, n_env_value_at_release_jump: NOT_YET_RELEASED, flags: 0 }
    }
}

impl EnvInfo {
    pub fn reset(&mut self) {
        self.n_env_position = 0;
        self.n_env_value_at_release_jump = NOT_YET_RELEASED;
    }
}

#[derive(Clone, Debug)]
pub struct ModChannel {
    pub position: SamplePosition,
    pub increment: SamplePosition,
    pub p_current_sample: Option<CurrentSample>,
    pub left_vol: i32,
    pub right_vol: i32,
    pub left_ramp: i32,
    pub right_ramp: i32,
    pub ramp_left_vol: i32,
    pub ramp_right_vol: i32,
    pub n_filter_y: [[i32; 2]; 2],
    pub n_filter_a0: i32,
    pub n_filter_b0: i32,
    pub n_filter_b1: i32,
    pub n_filter_hp: i32,
    pub n_length: SmpLength,
    pub n_loop_start: SmpLength,
    pub n_loop_end: SmpLength,
    pub dw_flags: u32,
    pub n_r_ofs: i32,
    pub n_l_ofs: i32,
    pub n_ramp_length: u32,
    pub p_mod_sample: Option<SampleIndex>,
    pub p_mod_instrument: Option<InstrumentIndex>,
    pub prev_note_offset: SmpLength,
    pub old_offset: SmpLength,
    pub dw_old_flags: u32,
    pub new_left_vol: i32,
    pub new_right_vol: i32,
    pub n_real_volume: i32,
    pub n_real_pan: i32,
    pub n_volume: i32,
    pub n_pan: i32,
    pub n_fade_out_vol: i32,
    pub n_period: i32,
    pub n_c5_speed: i32,
    pub n_portamento_dest: i32,
    pub cached_period: i32,
    pub glissando_period: i32,
    pub n_calc_volume: i32,
    pub vol_env: EnvInfo,
    pub pan_env: EnvInfo,
    pub pitch_env: EnvInfo,
    pub n_auto_vib_depth: i32,
    pub n_efx_offset: u32,
    pub n_pattern_loop: RowIndex,
    pub portamento_slide: u16,
    pub n_fine_tune: i16,
    pub micro_tuning: i16,
    pub n_vol_swing: i16,
    pub n_pan_swing: i16,
    pub n_cut_swing: i16,
    pub n_res_swing: i16,
    pub vol_slide_down_remain: u16,
    pub vol_slide_down_total: u16,
    /// `nRestorePanOnNewNote` / `nnaChannelAge` (a union in libopenmpt).
    pub n_restore_pan_on_new_note: u16,
    pub nna_generation: u16,
    pub n_master_chn: ChannelIndex,
    pub swap_sample_index: SampleIndex,
    pub row_command: ModCommand,
    pub n_global_vol: u8,
    pub n_ins_vol: u8,
    pub n_transpose: i8,
    pub resampling_mode: u8,
    pub n_restore_resonance_on_new_note: u8,
    pub n_restore_cutoff_on_new_note: u8,
    pub n_note: u8,
    pub n_nna: NewNoteAction,
    pub n_last_note: u8,
    pub n_arpeggio_last_note: u8,
    pub last_midi_note_without_arp: u8,
    pub n_new_note: u8,
    pub n_new_ins: u8,
    pub n_old_ins: u8,
    pub n_command: u8,
    pub n_arpeggio: u8,
    pub n_retrig_param: u8,
    pub n_retrig_count: u8,
    pub n_old_volume_slide: u8,
    pub n_old_fine_vol_up_down: u8,
    pub n_old_porta_up: u8,
    pub n_old_porta_down: u8,
    pub n_old_fine_porta_up_down: u8,
    pub n_old_extra_fine_porta_up_down: u8,
    pub n_old_pan_slide: u8,
    pub n_old_chn_vol_slide: u8,
    pub n_old_global_vol_slide: u8,
    pub n_auto_vib_pos: u8,
    pub n_vibrato_pos: u8,
    pub n_tremolo_pos: u8,
    pub n_panbrello_pos: u8,
    pub n_vibrato_type: u8,
    pub n_vibrato_speed: u8,
    pub n_vibrato_depth: u8,
    pub n_tremolo_type: u8,
    pub n_tremolo_speed: u8,
    pub n_tremolo_depth: u8,
    pub n_panbrello_type: u8,
    pub n_panbrello_speed: u8,
    pub n_panbrello_depth: u8,
    pub n_panbrello_offset: i8,
    pub n_panbrello_random_memory: i8,
    pub n_old_cmd_ex: u8,
    pub n_old_vol_param: u8,
    pub n_old_tempo: u8,
    pub n_old_hi_offset: u8,
    pub n_cut_off: u8,
    pub n_resonance: u8,
    pub n_tremor_count: u8,
    pub n_tremor_param: u8,
    pub n_pattern_loop_count: u8,
    pub n_left_vu: u8,
    pub n_right_vu: u8,
    pub n_active_macro: u8,
    pub vol_slide_down_start: u8,
    pub n_filter_mode: FilterMode,
    pub n_efx_speed: u8,
    pub n_efx_delay: u8,
    pub note_slide_param: u8,
    pub note_slide_counter: u8,
    pub last_zxx_param: u8,
    pub is_first_tick: bool,
    pub trigger_note: bool,
    pub is_preview_note: bool,
    pub is_paused: bool,
    pub porta_target_reached: bool,
    pub fc_porta_tick: bool,
    pub m_portamento_fine_steps: i32,
    pub m_portamento_tick_slide: i32,
    pub auto_slide: AutoSlideStatus,
}

impl Default for ModChannel {
    fn default() -> Self {
        ModChannel {
            position: SamplePosition(0),
            increment: SamplePosition(0),
            p_current_sample: None,
            left_vol: 0,
            right_vol: 0,
            left_ramp: 0,
            right_ramp: 0,
            ramp_left_vol: 0,
            ramp_right_vol: 0,
            n_filter_y: [[0; 2]; 2],
            n_filter_a0: 0,
            n_filter_b0: 0,
            n_filter_b1: 0,
            n_filter_hp: 0,
            n_length: 0,
            n_loop_start: 0,
            n_loop_end: 0,
            dw_flags: 0,
            n_r_ofs: 0,
            n_l_ofs: 0,
            n_ramp_length: 0,
            p_mod_sample: None,
            p_mod_instrument: None,
            prev_note_offset: 0,
            old_offset: 0,
            dw_old_flags: 0,
            new_left_vol: 0,
            new_right_vol: 0,
            n_real_volume: 0,
            n_real_pan: 0,
            n_volume: 0,
            n_pan: 0,
            n_fade_out_vol: 0,
            n_period: 0,
            n_c5_speed: 0,
            n_portamento_dest: 0,
            cached_period: 0,
            glissando_period: 0,
            n_calc_volume: 0,
            vol_env: EnvInfo::default(),
            pan_env: EnvInfo::default(),
            pitch_env: EnvInfo::default(),
            n_auto_vib_depth: 0,
            n_efx_offset: 0,
            n_pattern_loop: 0,
            portamento_slide: 0,
            n_fine_tune: 0,
            micro_tuning: 0,
            n_vol_swing: 0,
            n_pan_swing: 0,
            n_cut_swing: 0,
            n_res_swing: 0,
            vol_slide_down_remain: 0,
            vol_slide_down_total: 0,
            n_restore_pan_on_new_note: 0,
            nna_generation: 0,
            n_master_chn: 0,
            swap_sample_index: 0,
            row_command: ModCommand::default(),
            n_global_vol: 0,
            n_ins_vol: 0,
            n_transpose: 0,
            resampling_mode: 0,
            n_restore_resonance_on_new_note: 0,
            n_restore_cutoff_on_new_note: 0,
            n_note: 0,
            n_nna: NewNoteAction::NoteCut,
            n_last_note: 0,
            n_arpeggio_last_note: 0,
            last_midi_note_without_arp: 0,
            n_new_note: 0,
            n_new_ins: 0,
            n_old_ins: 0,
            n_command: 0,
            n_arpeggio: 0,
            n_retrig_param: 0,
            n_retrig_count: 0,
            n_old_volume_slide: 0,
            n_old_fine_vol_up_down: 0,
            n_old_porta_up: 0,
            n_old_porta_down: 0,
            n_old_fine_porta_up_down: 0,
            n_old_extra_fine_porta_up_down: 0,
            n_old_pan_slide: 0,
            n_old_chn_vol_slide: 0,
            n_old_global_vol_slide: 0,
            n_auto_vib_pos: 0,
            n_vibrato_pos: 0,
            n_tremolo_pos: 0,
            n_panbrello_pos: 0,
            n_vibrato_type: 0,
            n_vibrato_speed: 0,
            n_vibrato_depth: 0,
            n_tremolo_type: 0,
            n_tremolo_speed: 0,
            n_tremolo_depth: 0,
            n_panbrello_type: 0,
            n_panbrello_speed: 0,
            n_panbrello_depth: 0,
            n_panbrello_offset: 0,
            n_panbrello_random_memory: 0,
            n_old_cmd_ex: 0,
            n_old_vol_param: 0,
            n_old_tempo: 0,
            n_old_hi_offset: 0,
            n_cut_off: 0,
            n_resonance: 0,
            n_tremor_count: 0,
            n_tremor_param: 0,
            n_pattern_loop_count: 0,
            n_left_vu: 0,
            n_right_vu: 0,
            n_active_macro: 0,
            vol_slide_down_start: 0,
            n_filter_mode: FilterMode::LowPass,
            n_efx_speed: 0,
            n_efx_delay: 0,
            note_slide_param: 0,
            note_slide_counter: 0,
            last_zxx_param: 0,
            is_first_tick: false,
            trigger_note: false,
            is_preview_note: false,
            is_paused: false,
            porta_target_reached: false,
            fc_porta_tick: false,
            m_portamento_fine_steps: 0,
            m_portamento_tick_slide: 0,
            auto_slide: AutoSlideStatus::default(),
        }
    }
}

pub const RESET_CHANNEL_SETTINGS: u32 = 1;
pub const RESET_SET_POS_BASIC: u32 = 2;
pub const RESET_SET_POS_ADVANCED: u32 = 4;
pub const RESET_SET_POS_FULL: u32 = RESET_SET_POS_BASIC | RESET_SET_POS_ADVANCED | RESET_CHANNEL_SETTINGS;
pub const RESET_TOTAL: u32 = RESET_SET_POS_FULL;

impl ModChannel {
    pub fn has(&self, f: u32) -> bool {
        self.dw_flags & f != 0
    }
    pub fn set(&mut self, f: u32) {
        self.dw_flags |= f;
    }
    pub fn reset_flag(&mut self, f: u32) {
        self.dw_flags &= !f;
    }
    pub fn set_to(&mut self, f: u32, on: bool) {
        if on {
            self.dw_flags |= f;
        } else {
            self.dw_flags &= !f;
        }
    }

    pub fn envelope(&self, t: EnvelopeType) -> &EnvInfo {
        match t {
            EnvelopeType::Volume => &self.vol_env,
            EnvelopeType::Panning => &self.pan_env,
            EnvelopeType::Pitch => &self.pitch_env,
        }
    }
    pub fn envelope_mut(&mut self, t: EnvelopeType) -> &mut EnvInfo {
        match t {
            EnvelopeType::Volume => &mut self.vol_env,
            EnvelopeType::Panning => &mut self.pan_env,
            EnvelopeType::Pitch => &mut self.pitch_env,
        }
    }
    pub fn reset_envelopes(&mut self) {
        self.vol_env.reset();
        self.pan_env.reset();
        self.pitch_env.reset();
    }

    /// `ModChannel::Reset`.
    pub fn reset(&mut self, mask: u32, m: &Module, source_channel: usize, mute_flag: u32) {
        use crate::defs::pb::*;
        // For "the ultimate beeper.mod"
        let default_sample =
            if m.mod_type == MOD_TYPE_MOD && m.samples[0].has_sample_data() { Some(0) } else { None };
        if mask & RESET_SET_POS_BASIC != 0 {
            let initial = if m.behaviour(kITInitialNoteMemory) { NOTE_MIN } else { NOTE_NONE };
            self.n_note = initial;
            self.n_new_note = initial;
            self.n_arpeggio_last_note = NOTE_NONE;
            self.last_midi_note_without_arp = NOTE_NONE;
            self.n_new_ins = 0;
            self.n_old_ins = 0;
            self.swap_sample_index = 0;
            self.p_mod_sample = default_sample;
            self.p_mod_instrument = None;
            self.n_portamento_dest = 0;
            self.n_command = CMD_NONE;
            self.n_pattern_loop_count = 0;
            self.n_pattern_loop = 0;
            self.n_fade_out_vol = 0;
            self.dw_flags |= CHN_KEYOFF | CHN_NOTEFADE;
            self.dw_old_flags = 0;
            self.auto_slide.reset();
            self.n_ins_vol = 64;
            self.nna_generation = 0;
            if m.behaviour(kITRetrigger) {
                self.n_retrig_param = 1;
                self.n_retrig_count = 0;
            }
            self.micro_tuning = 0;
            self.n_tremor_count = 0;
            self.n_efx_speed = 0;
            self.prev_note_offset = 0;
            self.last_zxx_param = 0xFF;
            self.is_first_tick = false;
            self.trigger_note = false;
            self.is_preview_note = false;
            self.is_paused = false;
            self.porta_target_reached = false;
            self.row_command = ModCommand::default();
        }
        if mask & RESET_SET_POS_ADVANCED != 0 {
            self.increment = SamplePosition(0);
            self.n_period = 0;
            self.position = SamplePosition(0);
            self.n_length = 0;
            self.n_loop_start = 0;
            self.n_loop_end = 0;
            self.n_r_ofs = 0;
            self.n_l_ofs = 0;
            self.p_mod_sample = default_sample;
            self.p_mod_instrument = None;
            self.n_cut_off = 0x7F;
            self.n_resonance = 0;
            self.n_filter_mode = FilterMode::LowPass;
            self.right_vol = 0;
            self.left_vol = 0;
            self.new_right_vol = 0;
            self.new_left_vol = 0;
            self.right_ramp = 0;
            self.left_ramp = 0;
            self.n_volume = 0;
            self.n_vibrato_pos = 0;
            self.n_tremolo_pos = 0;
            self.n_panbrello_pos = 0;
            self.n_old_hi_offset = 0;
            self.n_left_vu = 0;
            self.n_right_vu = 0;
            self.n_old_extra_fine_porta_up_down = 0;
            self.n_old_fine_porta_up_down = 0;
            self.n_old_porta_down = 0;
            self.n_old_porta_up = 0;
            self.portamento_slide = 0;
            self.n_master_chn = 0;
            self.m_portamento_fine_steps = 0;
            self.m_portamento_tick_slide = 0;
        }
        if mask & RESET_CHANNEL_SETTINGS != 0 {
            if source_channel < m.chn_settings.len() {
                let s = &m.chn_settings[source_channel];
                self.dw_flags = s.dw_flags;
                self.n_pan = s.n_pan as i32;
                self.n_global_vol = s.n_volume;
                if self.dw_flags & CHN_MUTE != 0 {
                    self.dw_flags &= !CHN_MUTE;
                    self.dw_flags |= mute_flag;
                }
            } else {
                self.dw_flags = 0;
                self.n_pan = 128;
                self.n_global_vol = 64;
            }
            self.n_restore_pan_on_new_note = 0;
            self.n_restore_cutoff_on_new_note = 0;
            self.n_restore_resonance_on_new_note = 0;
        }
    }

    /// `ModChannel::Stop`.
    pub fn stop(&mut self) {
        self.n_period = 0;
        self.increment = SamplePosition(0);
        self.position = SamplePosition(0);
        self.n_left_vu = 0;
        self.n_right_vu = 0;
        self.n_volume = 0;
        self.p_current_sample = None;
    }

    pub fn is_sample_playing(&self) -> bool {
        !self.increment.is_zero()
    }

    /// `UpdateInstrumentVolume`.
    pub fn update_instrument_volume(&mut self, smp_global_vol: Option<u16>, ins_global_vol: Option<u32>) {
        self.n_ins_vol = 64;
        if let Some(v) = smp_global_vol {
            self.n_ins_vol = v as u8;
        }
        if let Some(v) = ins_global_vol {
            self.n_ins_vol = ((self.n_ins_vol as u32 * v) / 64) as u8;
        }
    }

    /// `InSustainLoop`.
    pub fn in_sustain_loop(&self, m: &Module) -> bool {
        (self.dw_flags & (CHN_LOOP | CHN_KEYOFF)) == CHN_LOOP
            && self.p_mod_sample.is_some_and(|s| m.samples[s as usize].u_flags & CHN_SUSTAINLOOP != 0)
    }

    /// `SetInstrumentPan`.
    pub fn set_instrument_pan(&mut self, pan: i32, m: &Module) {
        if m.behaviour(crate::defs::pb::kITDoNotOverrideChannelPan) {
            self.n_restore_pan_on_new_note = (self.n_pan + 1) as u16;
            if self.dw_flags & CHN_SURROUND != 0 {
                self.n_restore_pan_on_new_note |= 0x8000;
            }
        }
        self.n_pan = pan;
    }

    /// `RestorePanAndFilter`.
    pub fn restore_pan_and_filter(&mut self) {
        if self.n_restore_pan_on_new_note > 0 {
            self.n_pan = (self.n_restore_pan_on_new_note & 0x7FFF) as i32 - 1;
            if self.n_restore_pan_on_new_note & 0x8000 != 0 {
                self.dw_flags |= CHN_SURROUND;
            }
            self.n_restore_pan_on_new_note = 0;
        }
        if self.n_restore_resonance_on_new_note > 0 {
            self.n_resonance = self.n_restore_resonance_on_new_note - 1;
            self.n_restore_resonance_on_new_note = 0;
        }
        if self.n_restore_cutoff_on_new_note > 0 {
            self.n_cut_off = self.n_restore_cutoff_on_new_note - 1;
            self.n_restore_cutoff_on_new_note = 0;
        }
    }

    /// IT command S73-S7E.
    pub fn instrument_control(&mut self, param: u8, mod_type: u32) {
        match param & 0x0F {
            0x3 => self.n_nna = NewNoteAction::NoteCut,
            0x4 => self.n_nna = NewNoteAction::Continue,
            0x5 => self.n_nna = NewNoteAction::NoteOff,
            0x6 => self.n_nna = NewNoteAction::NoteFade,
            0x7 => self.vol_env.flags &= !ENV_ENABLED,
            0x8 => self.vol_env.flags |= ENV_ENABLED,
            0x9 => self.pan_env.flags &= !ENV_ENABLED,
            0xA => self.pan_env.flags |= ENV_ENABLED,
            0xB => self.pitch_env.flags &= !ENV_ENABLED,
            0xC => self.pitch_env.flags |= ENV_ENABLED,
            p @ (0xD | 0xE) => {
                if mod_type == MOD_TYPE_MPT {
                    self.pitch_env.flags |= ENV_ENABLED;
                    if p != 0xD {
                        self.pitch_env.flags |= ENV_FILTER;
                    } else {
                        self.pitch_env.flags &= !ENV_FILTER;
                    }
                }
            }
            _ => {}
        }
    }

    /// Volume command `:xx`.
    pub fn play_control(&mut self, param: u8) {
        match param {
            0 => self.is_paused = true,
            1 => self.is_paused = false,
            2 => self.dw_flags &= !CHN_PINGPONGFLAG,
            3 => self.dw_flags |= CHN_PINGPONGFLAG,
            4 => self.dw_flags ^= CHN_PINGPONGFLAG,
            5 => self.old_offset = self.position.uint(),
            6 => self.position.set(self.old_offset as i32, 0),
            _ => {}
        }
    }
}

/// `ModChannelSettings`.
#[derive(Clone, Debug)]
pub struct ModChannelSettings {
    pub dw_flags: u32,
    pub n_pan: u16,
    pub n_volume: u8,
    pub n_mix_plugin: u8,
}

impl Default for ModChannelSettings {
    fn default() -> Self {
        ModChannelSettings { dw_flags: 0, n_pan: 128, n_volume: 64, n_mix_plugin: 0 }
    }
}
