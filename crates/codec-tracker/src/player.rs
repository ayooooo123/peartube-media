//! The player: note, instrument and new-note-action handling, periods and
//! frequencies, and global effects, ported from libopenmpt 0.8.9
//! `soundlib/Snd_fx.cpp` (`InstrumentChange`, `NoteChange`, `CheckNNA`,
//! `GetPeriodFromNote`, `GetFreqFromPeriod`, `SetTempo`, ...).
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

#![allow(dead_code)]

use crate::channel::ModChannel;
use crate::command::*;
use crate::defs::pb::*;
use crate::defs::*;
use crate::rowvisitor::RowVisitor;
use crate::sndfile::{Module, PlayState};
use crate::tables::*;

/// libopenmpt's `mpt::fast_prng` (`lcg_msvc`), including its constructor
/// pre-step and the low-bit extraction in `mpt::random<int8/int, 7>`.
#[derive(Clone, Debug)]
pub struct Prng(u32);

pub const DEFAULT_SEED: u32 = 0x1234_5678;

impl Prng {
    pub fn new(seed: u32) -> Self {
        Self(seed.wrapping_mul(214013).wrapping_add(2531011))
    }

    fn next_15(&mut self) -> u16 {
        let value = ((self.0 >> 16) & 0x7FFF) as u16;
        self.0 = self.0.wrapping_mul(214013).wrapping_add(2531011);
        value
    }

    pub fn next_i8(&mut self) -> i8 {
        self.next_15() as i8
    }

    pub fn bits7(&mut self) -> i32 {
        (self.next_15() & 127) as i32
    }
}

/// `MixerSettings` plus the resampler mode: openmpt123's defaults.
#[derive(Clone, Debug)]
pub struct MixerSettings {
    pub mixing_freq: u32,
    pub channels: u32,
    pub pre_amp: u32,
    pub stereo_separation: i32,
    pub max_mix_channels: usize,
    pub ramp_up_us: i32,
    pub ramp_down_us: i32,
    pub src_mode: u8,
}

impl Default for MixerSettings {
    fn default() -> Self {
        MixerSettings {
            mixing_freq: 48000,
            channels: 2,
            pre_amp: 128,
            stereo_separation: 128,
            max_mix_channels: MAX_CHANNELS,
            ramp_up_us: 363,
            ramp_down_us: 952,
            src_mode: SRCMODE_SINC8LP,
        }
    }
}

impl MixerSettings {
    pub fn ramp_up_samples(&self) -> i32 {
        muldivr(self.ramp_up_us, self.mixing_freq as i32, 1_000_000)
    }
    pub fn ramp_down_samples(&self) -> i32 {
        muldivr(self.ramp_down_us, self.mixing_freq as i32, 1_000_000)
    }
}

/// `CSoundFile` at play time: the song, its play state and the mixer.
pub struct Player {
    pub m: Module,
    pub ps: PlayState,
    pub visited: RowVisitor,
    pub settings: MixerSettings,
    pub n_mix_channels: usize,
    pub n_mix_stat: usize,
    pub dry_l_ofs: i32,
    pub dry_r_ofs: i32,
    pub mix_buffer: Vec<i32>,
    pub prng: Prng,
    pub opl: Option<Box<crate::opl::Opl>>,
    pub repeat_count: i32,
    pub is_rendering: bool,
    pub freq_factor: u32,
    pub tempo_factor: u32,
}

impl Player {
    /// A player positioned at the start of the song, as openmpt123 sets
    /// one up: `CreateInternal`'s play state, then `select_subsong(-1)`
    /// (all subsongs back to back) at the first playable order.
    pub fn new(mut m: Module, settings: MixerSettings) -> Self {
        m.song_flags |= SONG_PLAYALLSONGS;
        let visited = RowVisitor::new(&m);
        let opl = m.samples.iter().any(|s| s.u_flags & CHN_ADLIB != 0)
            .then(|| Box::new(crate::opl::Opl::new(settings.mixing_freq)));
        let mut p = Player {
            m,
            ps: PlayState::default(),
            visited,
            settings,
            n_mix_channels: 0,
            n_mix_stat: 0,
            dry_l_ofs: 0,
            dry_r_ofs: 0,
            mix_buffer: vec![0; MIXBUFFERSIZE * 2],
            prng: Prng::new(DEFAULT_SEED),
            opl,
            repeat_count: 0,
            is_rendering: false,
            freq_factor: 65536,
            tempo_factor: 65536,
        };
        crate::sndfile::initial_channels(&p.m, &mut p.ps);
        p.reset_play_pos();
        let mut start = 0;
        while (start as usize) < p.m.order.len() && !p.m.is_valid_order(start) {
            start += 1;
        }
        if (start as usize) < p.m.order_length_tail_trimmed() as usize {
            p.ps.current_order = start;
            p.ps.next_order = start;
        }
        if (p.ps.current_order as usize) < p.m.order.len() {
            p.ps.pattern = p.m.order[p.ps.current_order as usize];
        }
        p
    }

    /// `ResetPlayPos`.
    pub fn reset_play_pos(&mut self) {
        if let Some(opl) = &mut self.opl { opl.reset(); }
        for i in 0..self.ps.chn.len() {
            let mut c = std::mem::take(&mut self.ps.chn[i]);
            c.reset(crate::channel::RESET_SET_POS_FULL, &self.m, i, CHN_SYNCMUTE);
            self.ps.chn[i] = c;
        }
        let m = &self.m;
        self.visited.initialize(m, true);
        let ps = &mut self.ps;
        ps.flags &= !(SONG_FADINGSONG | SONG_ENDREACHED);
        ps.global_volume = m.default_global_volume as i32;
        ps.music_speed = m.default_speed;
        ps.music_tempo = m.default_tempo;
        ps.reset_global_volume_ramping();
        ps.next_order = 0;
        ps.next_row = 0;
        ps.tick_count = crate::sndfile::TICKS_ROW_FINISHED;
        ps.buffer_count = 0;
        ps.buffer_diff = 0.0;
        ps.pattern_delay = 0;
        ps.frame_delay = 0;
        ps.next_pat_start_row = 0;
        ps.total_sample_count = 0;
        ps.last_moved_channel = CHANNELINDEX_INVALID;
        ps.current_rows_per_beat = m.default_rows_per_beat;
        ps.current_rows_per_measure = m.default_rows_per_measure;
        ps.samples_per_tick = 0;
    }
}

pub fn min_tempo_param(t: u32) -> Tempo {
    if t & (MOD_TYPE_MDL | MOD_TYPE_MED | MOD_TYPE_XM | MOD_TYPE_MOD) != 0 { Tempo::new(1, 0) } else { Tempo::new(32, 0) }
}

/// Tempo limits of `GetModSpecifications()` (the best save format's).
pub fn spec_tempo(t: u32) -> (u32, u32) {
    match crate::sndfile::best_save_format(t) {
        MOD_TYPE_MPT => (32, 1000),
        MOD_TYPE_IT => (32, 512),
        MOD_TYPE_XM => (32, 1000),
        MOD_TYPE_S3M => (33, 255),
        _ => (32, 255),
    }
}

fn xm2mod_finetune(v: i32) -> i32 {
    ((v as u8) >> 4) as i32
}

impl Module {
    /// `GetPeriodFromNote`.
    pub fn period_from_note(&self, note: u32, mut fine_tune: i32, mut c5speed: u32) -> u32 {
        if note == NOTE_NONE as u32 || note >= NOTE_MIN_SPECIAL as u32 {
            return 0;
        }
        let note = note - NOTE_MIN as u32;
        let t = self.mod_type;
        if !self.use_finetune_and_transpose() {
            if t == MOD_TYPE_MDL {
                return ((FREQ_S3_MTABLE[(note % 12) as usize] as u32) << 4) >> (note / 12).min(31);
            } else if t == MOD_TYPE_DTM {
                return ((PRO_TRACKER_TUNED_PERIODS[(xm2mod_finetune(fine_tune) as u32 * 12 + note % 12) as usize] as u32) << 5)
                    >> (note / 12).min(31);
            }
            if c5speed == 0 {
                c5speed = 8363;
            }
            if self.periods_are_frequencies() {
                let shifted = (LINEAR_SLIDE_UP_TABLE[((note % 12) * 16) as usize] as u64) << (note / 12);
                let freq = muldiv_u64(c5speed as u64, shifted, 65536 << 5);
                freq.min(i32::MAX as u64) as u32
            } else if self.song_flag(SONG_LINEARSLIDES) {
                ((FREQ_S3_MTABLE[(note % 12) as usize] as u32) << 5) >> (note / 12).min(31)
            } else {
                let oct = note / 12;
                c5speed = c5speed.min(if oct >= 32 { 0 } else { u32::MAX >> oct });
                let div = if oct >= 32 { 0 } else { c5speed.wrapping_shl(oct) };
                muldiv_unsigned(8363, (FREQ_S3_MTABLE[(note % 12) as usize] as u32) << 5, div)
            }
        } else if t & (MOD_TYPE_XM | MOD_TYPE_MTM) != 0 || self.song_flag(SONG_LINEARSLIDES) {
            let mut note = note.max(12);
            note -= 12;
            if t == MOD_TYPE_MTM {
                fine_tune *= 16;
            } else if self.behaviour(kFT2FinetunePrecision) {
                fine_tune &= !7;
            }
            if self.song_flag(SONG_LINEARSLIDES) {
                let mut l = ((120 - note as i32) << 6) - (fine_tune / 2);
                if l < 1 {
                    l = 1;
                }
                l as u32
            } else {
                let mut finetune = fine_tune;
                let rnote = ((note % 12) << 3) as i32;
                let roct = note / 12;
                let mut rfine = finetune / 16;
                let i = (rnote + rfine + 8).clamp(0, 103);
                let mut per1 = XMPERIOD_TABLE[i as usize] as u32;
                if finetune < 0 {
                    rfine -= 1;
                    finetune = -finetune;
                } else {
                    rfine += 1;
                }
                let mut i = rnote + rfine + 8;
                if i < 0 {
                    i = 0;
                }
                if i >= 104 {
                    i = 103;
                }
                let mut per2 = XMPERIOD_TABLE[i as usize] as u32;
                let rfine = (finetune & 0x0F) as u32;
                per1 *= 16 - rfine;
                per2 *= rfine;
                ((per1 + per2) << 1) >> roct.min(31)
            }
        } else {
            let ft = xm2mod_finetune(fine_tune) as u32;
            if ft != 0 || note < 24 || note >= 24 + PRO_TRACKER_PERIOD_TABLE.len() as u32 {
                ((PRO_TRACKER_TUNED_PERIODS[(ft * 12 + note % 12) as usize] as u32) << 5) >> (note / 12).min(31)
            } else {
                (PRO_TRACKER_PERIOD_TABLE[(note - 24) as usize] as u32) << 2
            }
        }
    }

    /// `GetNoteFromPeriod`.
    pub fn note_from_period(&self, period: u32, mut fine_tune: i32, c5speed: u32) -> u32 {
        if period == 0 {
            return 0;
        }
        if self.behaviour(kFT2Periods) {
            fine_tune += 64;
        }
        let mut min_note = NOTE_MIN as u32;
        let max_note = NOTE_MAX as u32;
        let mut count = max_note - min_note + 1;
        let is_freq = self.periods_are_frequencies();
        while count > 0 {
            let step = count / 2;
            let mid = min_note + step;
            let n = self.period_from_note(mid, fine_tune, c5speed);
            if (n > period && !is_freq) || (n < period && is_freq) || n == 0 {
                min_note = mid + 1;
                count -= step + 1;
            } else {
                count = step;
            }
        }
        min_note
    }

    /// `GetFreqFromPeriod`: frequency with FREQ_FRACBITS fractional bits.
    pub fn freq_from_period(&self, mut period: u32, mut c5speed: u32, period_frac: i32) -> u32 {
        if period == 0 {
            return 0;
        }
        let t = self.mod_type;
        if t & (MOD_TYPE_XM | MOD_TYPE_MTM) != 0 || (self.song_flag(SONG_LINEARSLIDES) && self.use_finetune_and_transpose()) {
            if self.behaviour(kFT2Periods) {
                period &= 0xFFFF;
            }
            if self.song_flag(SONG_LINEARSLIDES) {
                let octave;
                if self.behaviour(kFT2Periods) {
                    let div = (9216u32 + 767).wrapping_sub(period) / 768;
                    octave = 14u32.wrapping_sub(div) & 0x1F;
                } else {
                    if period > 29 * 768 {
                        return 0;
                    }
                    octave = (period / 768) + 2;
                }
                (XMLINEAR_TABLE[(period % 768) as usize] << (FREQ_FRACBITS + 2)) >> octave
            } else {
                if period == 0 {
                    period = 1;
                }
                (((8363i64 * 1712) << FREQ_FRACBITS) / period as i64) as u32
            }
        } else if self.use_finetune_and_transpose() {
            (((3546895i64 * 4) << FREQ_FRACBITS) / period as i64) as u32
        } else if t == MOD_TYPE_669 {
            (period.wrapping_add(c5speed).wrapping_sub(8363)) << FREQ_FRACBITS
        } else if t == MOD_TYPE_MDL {
            period = period.min(u32::MAX >> 8);
            if c5speed == 0 {
                c5speed = 8363;
            }
            muldiv_unsigned(c5speed, (1712 << 7) << FREQ_FRACBITS, (period << 8).wrapping_add(period_frac as u32))
        } else {
            period = period.min(u32::MAX >> 8);
            if self.periods_are_frequencies() {
                ((((period as u64) << 8) + period_frac as i64 as u64) >> (8 - FREQ_FRACBITS)) as u32
            } else if self.song_flag(SONG_LINEARSLIDES) || t == MOD_TYPE_DTM {
                if c5speed == 0 {
                    c5speed = 8363;
                }
                muldiv_unsigned(c5speed, (1712 << 8) << FREQ_FRACBITS, (period << 8).wrapping_add(period_frac as u32))
            } else {
                muldiv_unsigned(8363, (1712 << 8) << FREQ_FRACBITS, (period << 8).wrapping_add(period_frac as u32))
            }
        }
    }

    /// `KeyOff`.
    pub fn key_off(&self, chn: &mut ModChannel) {
        let key_is_on = !chn.has(CHN_KEYOFF);
        chn.set(CHN_KEYOFF);
        if let Some(ins) = chn.p_mod_instrument.and_then(|i| self.instrument(i as u32)) {
            if chn.vol_env.flags & ENV_ENABLED == 0 {
                chn.set(CHN_NOTEFADE);
            }
            let _ = ins;
        }
        if chn.n_length == 0 {
            return;
        }
        if chn.has(CHN_SUSTAINLOOP) && key_is_on {
            if let Some(si) = chn.p_mod_sample {
                let smp = &self.samples[si as usize];
                if smp.u_flags & CHN_LOOP != 0 {
                    if smp.u_flags & CHN_PINGPONGLOOP != 0 {
                        chn.set(CHN_PINGPONGLOOP);
                    } else {
                        chn.reset_flag(CHN_PINGPONGLOOP | CHN_PINGPONGFLAG);
                    }
                    chn.set(CHN_LOOP);
                    chn.n_length = smp.n_length;
                    chn.n_loop_start = smp.n_loop_start;
                    chn.n_loop_end = smp.n_loop_end;
                    if chn.n_length > chn.n_loop_end {
                        chn.n_length = chn.n_loop_end;
                    }
                    if chn.position.uint() > chn.n_length {
                        let ls = chn.n_loop_start as i32;
                        let len = (chn.n_loop_end - chn.n_loop_start) as i32;
                        let p = ls + (chn.position.int().wrapping_sub(ls)) % len.max(1);
                        chn.position.set(p, 0);
                    }
                } else {
                    chn.reset_flag(CHN_LOOP | CHN_PINGPONGLOOP | CHN_PINGPONGFLAG);
                    chn.n_length = smp.n_length;
                }
            }
        }
        if let Some(ins) = chn.p_mod_instrument.and_then(|i| self.instrument(i as u32)) {
            if (ins.vol_env.has(ENV_LOOP) || self.mod_type & (MOD_TYPE_XM | MOD_TYPE_MT2 | MOD_TYPE_MDL) != 0) && ins.n_fade_out != 0 {
                chn.set(CHN_NOTEFADE);
            }
            if ins.vol_env.release_node != ENV_RELEASE_NODE_UNSET && chn.vol_env.n_env_value_at_release_jump == NOT_YET_RELEASED {
                let v = ins.vol_env.value_from_position(chn.vol_env.n_env_position as i32, 256, ENVELOPE_MAX as i32);
                chn.vol_env.n_env_value_at_release_jump = v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                chn.vol_env.n_env_position = ins.vol_env.nodes[ins.vol_env.release_node as usize].tick as u32;
            }
        }
    }

    /// `ApplyInstrumentPanning`.
    pub fn apply_instrument_panning(&self, ps_flags: u16, chn: &mut ModChannel, ins: Option<InstrumentIndex>, smp: Option<SampleIndex>) {
        let mut new_pan = i32::MIN;
        if let Some(i) = ins.and_then(|i| self.instrument(i as u32)) {
            if i.flags & INS_SETPANNING != 0 {
                new_pan = i.n_pan as i32;
            }
        }
        if let Some(s) = smp {
            let s = &self.samples[s as usize];
            if s.u_flags & CHN_PANNING != 0 {
                new_pan = s.n_pan as i32;
            }
        }
        if new_pan != i32::MIN {
            chn.set_instrument_pan(new_pan, self);
            if self.behaviour(kPanOverride) && ps_flags & SONG_SURROUNDPAN == 0 {
                chn.reset_flag(CHN_SURROUND);
            }
        }
    }

    /// `ProcessPitchPanSeparation`.
    pub fn pitch_pan_separation(pan: &mut i32, note: i32, ins: &crate::instrument::ModInstrument) {
        if ins.n_pps == 0 || note == NOTE_NONE as i32 {
            return;
        }
        let delta = (note - ins.n_ppc as i32 - NOTE_MIN as i32) * ins.n_pps as i32 / 2;
        *pan = (*pan + delta).clamp(0, 256);
    }

    /// `NoteChange`.
    #[allow(clippy::too_many_arguments)]
    pub fn note_change(
        &self,
        ps_flags: u16,
        prng: &mut Prng,
        chn: &mut ModChannel,
        mut note: i32,
        mut b_porta: bool,
        b_reset_env: bool,
        b_manual: bool,
        opl: Option<&mut crate::opl::Opl>,
        channel: usize,
    ) {
        if note < NOTE_MIN as i32 {
            return;
        }
        let t = self.mod_type;
        let orig_note = note;
        let mut p_smp = chn.p_mod_sample;
        let p_ins_idx = chn.p_mod_instrument;
        let p_ins = p_ins_idx.and_then(|i| self.instrument(i as u32));
        let realnote = note;
        if let Some(ins) = p_ins {
            if ((note - NOTE_MIN as i32) as usize) < ins.keyboard.len() {
                let n = ins.keyboard[(note - NOTE_MIN as i32) as usize] as u32;
                if n > 0 {
                    p_smp = Some(if n <= self.num_samples as u32 { n as SampleIndex } else { 0 });
                } else if self.behaviour(kITEmptyNoteMapSlot) {
                    return;
                }
                note = ins.note_map[(note - NOTE_MIN as i32) as usize] as i32;
            }
        }
        // Key Off
        if note > NOTE_MAX as i32 {
            if note == NOTE_KEYOFF as i32 || t & (MOD_TYPE_IT | MOD_TYPE_MPT) == 0 {
                self.key_off(chn);
                if !b_porta && self.behaviour(kITInstrWithNoteOffOldEffects) && self.song_flag(SONG_ITOLDEFFECTS) && chn.row_command.instr != 0 {
                    chn.reset_flag(CHN_NOTEFADE | CHN_KEYOFF);
                }
            } else if self.num_instruments > 0 {
                chn.set(CHN_NOTEFADE);
            }
            if note == NOTE_NOTECUT as i32 {
                if chn.has(CHN_ADLIB) && t == MOD_TYPE_S3M {
                    chn.set(CHN_KEYOFF);
                } else {
                    chn.set(CHN_NOTEFADE | CHN_FASTVOLRAMP);
                    if t & (MOD_TYPE_IT | MOD_TYPE_MPT) == 0 || (self.num_instruments != 0 && !self.behaviour(kITInstrWithNoteOff)) {
                        chn.n_volume = 0;
                    }
                    if self.behaviour(kITInstrWithNoteOff) {
                        chn.increment = SamplePosition(0);
                    }
                    chn.n_fade_out_vol = 0;
                }
            }
            if self.behaviour(kITClearOldNoteAfterCut) {
                chn.n_note = NOTE_NONE;
                chn.n_new_note = NOTE_NONE;
            }
            return;
        }
        if !b_porta && t & (MOD_TYPE_XM | MOD_TYPE_MED | MOD_TYPE_MT2) != 0 {
            if let Some(s) = p_smp {
                let s = &self.samples[s as usize];
                chn.n_transpose = s.relative_tone;
                chn.n_fine_tune = s.n_fine_tune as i16;
            }
        }
        if !b_porta && self.behaviour(kITMultiSampleBehaviour) {
            if let Some(s) = p_smp {
                chn.n_c5_speed = self.samples[s as usize].n_c5_speed as i32;
            }
        }
        if b_porta && !chn.is_sample_playing() {
            if self.behaviour(kFT2PortaNoNote) {
                chn.n_period = 0;
                return;
            } else if self.behaviour(kITPortaNoNote) {
                b_porta = false;
            }
        }
        if self.use_finetune_and_transpose() {
            note += chn.n_transpose as i32;
            note = note.clamp(NOTE_MIN as i32 + 11, NOTE_MIN as i32 + 130);
        } else {
            note = note.clamp(NOTE_MIN as i32, NOTE_MAX as i32);
        }
        if self.behaviour(kITRealNoteMapping) {
            chn.n_note = realnote.clamp(NOTE_MIN as i32, NOTE_MAX as i32) as u8;
        } else {
            chn.n_note = note as u8;
        }
        chn.is_paused = false;
        if !b_porta || t & (MOD_TYPE_S3M | MOD_TYPE_IT | MOD_TYPE_MPT) != 0 {
            chn.swap_sample_index = 0;
            chn.n_new_ins = 0;
        }
        let period = self.period_from_note(note as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32);
        chn.n_panbrello_offset = 0;
        if self.behaviour(kITPanningReset) {
            self.apply_instrument_panning(ps_flags, chn, p_ins_idx, p_smp);
        }
        if self.behaviour(kITPitchPanSeparation) {
            if let Some(ins) = p_ins {
                if ins.n_pps != 0 {
                    if chn.n_restore_pan_on_new_note == 0 {
                        chn.n_restore_pan_on_new_note = (chn.n_pan + 1) as u16;
                    }
                    Module::pitch_pan_separation(&mut chn.n_pan, orig_note, ins);
                }
            }
        }
        if b_reset_env && !b_porta {
            chn.n_vol_swing = 0;
            chn.n_pan_swing = 0;
            chn.n_res_swing = 0;
            chn.n_cut_swing = 0;
            if let Some(ins) = p_ins {
                if self.behaviour(kITNNAReset) {
                    chn.n_nna = ins.nna;
                }
                if !ins.vol_env.has(ENV_CARRY) {
                    chn.vol_env.reset();
                }
                if !ins.pan_env.has(ENV_CARRY) {
                    chn.pan_env.reset();
                }
                if !ins.pitch_env.has(ENV_CARRY) {
                    chn.pitch_env.reset();
                }
                if ins.n_vol_swing != 0 {
                    let base = if self.behaviour(kITSwingBehaviour) { chn.n_ins_vol as i32 } else { (chn.n_volume + 1) / 2 };
                    let r = prng.next_i8() as i32;
                    chn.n_vol_swing = (((r * ins.n_vol_swing as i32) / 64 + 1) * base / 199) as i16;
                }
                if ins.n_pan_swing != 0 {
                    let r = prng.next_i8() as i32;
                    chn.n_pan_swing = ((r * ins.n_pan_swing as i32 * 4) / 128) as i16;
                    if !self.behaviour(kITSwingBehaviour) && chn.n_restore_pan_on_new_note == 0 {
                        chn.n_restore_pan_on_new_note = (chn.n_pan + 1) as u16;
                    }
                }
                if ins.n_cut_swing != 0 {
                    let r = prng.next_i8() as i32;
                    let d = (ins.n_cut_swing as i32 * (r + 1)) / 128;
                    chn.n_cut_swing = ((d * chn.n_cut_off as i32 + 1) / 128) as i16;
                    chn.n_restore_cutoff_on_new_note = chn.n_cut_off.wrapping_add(1);
                }
                if ins.n_res_swing != 0 {
                    let r = prng.next_i8() as i32;
                    let d = (ins.n_res_swing as i32 * (r + 1)) / 128;
                    chn.n_res_swing = ((d * chn.n_resonance as i32 + 1) / 128) as i16;
                    chn.n_restore_resonance_on_new_note = chn.n_resonance.wrapping_add(1);
                }
            }
        }
        let Some(smp_idx) = p_smp else {
            return;
        };
        let smp = &self.samples[smp_idx as usize];
        if period != 0 {
            if !b_porta || chn.n_period == 0 {
                chn.n_period = period as i32;
            }
            if b_porta || !(self.behaviour(kFT2PortaTargetNoReset) || self.behaviour(kITClearPortaTarget) || t == MOD_TYPE_MOD) {
                chn.n_portamento_dest = period as i32;
                chn.porta_target_reached = false;
            }
            if !b_porta || (chn.n_length == 0 && t & MOD_TYPE_S3M == 0) {
                chn.p_mod_sample = Some(smp_idx);
                chn.n_length = smp.n_length;
                chn.n_loop_end = smp.n_length;
                chn.n_loop_start = 0;
                chn.position = SamplePosition(0);
                if (self.song_flag(SONG_PT_MODE) || self.behaviour(kST3OffsetWithoutInstrument) || t == MOD_TYPE_MED) && chn.row_command.instr == 0 {
                    let p = chn.prev_note_offset.min(chn.n_length.wrapping_sub(1));
                    chn.position.set_int(p as i32);
                } else {
                    chn.prev_note_offset = 0;
                }
                chn.dw_flags = (chn.dw_flags & CHN_CHANNELFLAGS) | (smp.u_flags & CHN_SAMPLEFLAGS);
                chn.reset_flag(CHN_PORTAMENTO);
                if chn.has(CHN_SUSTAINLOOP) {
                    chn.n_loop_start = smp.n_sustain_start;
                    chn.n_loop_end = smp.n_sustain_end;
                    let pp = chn.has(CHN_PINGPONGSUSTAIN);
                    chn.set_to(CHN_PINGPONGLOOP, pp);
                    chn.set(CHN_LOOP);
                    if chn.n_length > chn.n_loop_end {
                        chn.n_length = chn.n_loop_end;
                    }
                } else if chn.has(CHN_LOOP) {
                    chn.n_loop_start = smp.n_loop_start;
                    chn.n_loop_end = smp.n_loop_end;
                    if chn.n_length > chn.n_loop_end {
                        chn.n_length = chn.n_loop_end;
                    }
                }
                if self.behaviour(kMODOneShotLoops) && chn.n_loop_start == 0 {
                    chn.n_loop_end = smp.n_length;
                    chn.n_length = smp.n_length;
                }
                if chn.has(CHN_REVERSE) && chn.n_length > 0 {
                    chn.set(CHN_PINGPONGFLAG);
                    chn.position.set_int((chn.n_length - 1) as i32);
                }
                if chn.n_vibrato_type < 4 {
                    if !self.behaviour(kITVibratoTremoloPanbrello) && t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 && !self.song_flag(SONG_ITOLDEFFECTS) {
                        chn.n_vibrato_pos = 0x10;
                    } else if t == MOD_TYPE_MTM {
                        chn.n_vibrato_pos = 0x20;
                    } else if t & (MOD_TYPE_DIGI | MOD_TYPE_DBM) == 0 {
                        chn.n_vibrato_pos = 0;
                    }
                }
                if !self.behaviour(kITVibratoTremoloPanbrello) && chn.n_tremolo_type < 4 {
                    chn.n_tremolo_pos = 0;
                }
            }
            if chn.position.uint() >= chn.n_length {
                chn.position.set_int(chn.n_loop_start as i32);
            }
        } else {
            b_porta = false;
        }
        if !b_porta
            || t & (MOD_TYPE_IT | MOD_TYPE_MPT | MOD_TYPE_DBM) == 0
            || (chn.has(CHN_NOTEFADE) && chn.n_fade_out_vol == 0)
            || (self.song_flag(SONG_ITCOMPATGXX) && chn.row_command.instr != 0)
        {
            if t & (MOD_TYPE_IT | MOD_TYPE_MPT | MOD_TYPE_DBM) != 0 && chn.has(CHN_NOTEFADE) && chn.n_fade_out_vol == 0 {
                chn.reset_envelopes();
                if !self.behaviour(kITVibratoTremoloPanbrello) {
                    chn.n_auto_vib_depth = 0;
                    chn.n_auto_vib_pos = 0;
                }
                chn.reset_flag(CHN_NOTEFADE);
                chn.n_fade_out_vol = 65536;
            }
            if !b_porta || !self.song_flag(SONG_ITCOMPATGXX) || chn.row_command.instr != 0 {
                if t & (MOD_TYPE_XM | MOD_TYPE_MT2) == 0 || chn.row_command.instr != 0 {
                    chn.reset_flag(CHN_NOTEFADE);
                    chn.n_fade_out_vol = 65536;
                }
            }
        }
        if self.behaviour(kITFT2DontResetNoteOffOnPorta) && b_porta && (!self.song_flag(SONG_ITCOMPATGXX) || chn.row_command.instr == 0) {
            chn.reset_flag(CHN_EXTRALOUD);
        } else {
            chn.reset_flag(CHN_EXTRALOUD | CHN_KEYOFF);
        }
        if !b_porta {
            chn.trigger_note = true;
            chn.n_left_vu = 0xFF;
            chn.n_right_vu = 0xFF;
            chn.reset_flag(CHN_FILTER);
            chn.set(CHN_FASTVOLRAMP);
            if !self.behaviour(kITRetrigger) && !self.behaviour(kITTremor) && !self.behaviour(kFT2Retrigger) && !self.behaviour(kFT2Tremor) {
                chn.n_retrig_count = 0;
                chn.n_tremor_count = 0;
            }
            if b_reset_env {
                chn.n_auto_vib_depth = 0;
                chn.n_auto_vib_pos = 0;
            }
            chn.right_vol = 0;
            chn.left_vol = 0;
            if chn.has(CHN_ADLIB) {
                if let Some(opl) = opl {
                    if self.behaviour(kOPLNoteOffOnNoteChange) {
                        opl.note_off(channel);
                    } else if self.behaviour(kOPLNoteStopWith0Hz) {
                        opl.frequency(channel, 0, true, false);
                    }
                }
            }
        }
        if b_manual {
            chn.reset_flag(CHN_MUTE);
        }
        if (chn.p_mod_sample.is_some_and(|s| self.samples[s as usize].u_flags & CHN_MUTE != 0) && !b_manual)
            || (chn.p_mod_instrument.and_then(|i| self.instrument(i as u32)).is_some_and(|i| i.flags & INS_MUTE != 0) && !b_manual)
        {
            chn.n_period = 0;
        }
        let was_global = chn.auto_slide.is_active(crate::channel::AutoSlide::GlobalVolumeSlide);
        let was_chn_vol = chn.auto_slide.is_active(crate::channel::AutoSlide::VolumeDownWithDuration);
        chn.auto_slide.reset();
        chn.auto_slide.set_active(crate::channel::AutoSlide::GlobalVolumeSlide, was_global);
        chn.auto_slide.set_active(crate::channel::AutoSlide::VolumeDownWithDuration, was_chn_vol);
    }
}

fn muldiv_u64(a: u64, b: u64, c: u64) -> u64 {
    if c == 0 {
        return 0;
    }
    ((a as u128 * b as u128) / c as u128).min(u64::MAX as u128) as u64
}

impl Player {
    pub fn num_channels(&self) -> usize {
        self.m.num_channels()
    }

    /// `InstrumentChange`.
    pub fn instrument_change(&mut self, channel: usize, mut instr: u32, b_porta: bool, b_upd_vol: bool, b_reset_env: bool) {
        let m = &self.m;
        let t = m.mod_type;
        let last_moved = self.ps.last_moved_channel as usize;
        let last_env = if last_moved < self.ps.chn.len() {
            let c = &self.ps.chn[last_moved];
            Some((c.vol_env.n_env_position, c.pan_env.n_env_position, c.pitch_env.n_env_position))
        } else {
            None
        };
        let ps_flags = self.ps.flags;
        let chn = &mut self.ps.chn[channel];
        let mut p_ins = if instr <= m.num_instruments as u32 { m.instrument_index(instr) } else { None };
        let mut p_smp: Option<SampleIndex> = Some(if instr <= m.num_samples as u32 { instr as SampleIndex } else { 0 });
        let old_ins_vol = chn.n_ins_vol;
        let note = chn.n_new_note;
        if note == NOTE_NONE && m.behaviour(kITInstrWithoutNote) {
            return;
        }
        if let (Some(ii), true) = (p_ins, ModCommand::is_note_of(note)) {
            let ins = m.instrument(ii as u32).unwrap();
            if ins.keyboard[(note - NOTE_MIN) as usize] == 0 && m.behaviour(kITEmptyNoteMapSlot) && !ins.has_valid_midi_channel() {
                chn.p_mod_instrument = p_ins;
                return;
            }
            if ins.note_map[(note - NOTE_MIN) as usize] > NOTE_MAX {
                return;
            }
            let n = ins.keyboard[(note - NOTE_MIN) as usize] as u32;
            p_smp = if n != 0 { Some(if n <= m.num_samples as u32 { n as SampleIndex } else { 0 }) } else { None };
        } else if m.num_instruments > 0 {
            if note >= NOTE_MIN_SPECIAL {
                return;
            }
            if m.behaviour(kITEmptyNoteMapSlot) && p_ins.is_none_or(|i| !m.instrument(i as u32).unwrap().has_valid_midi_channel()) {
                chn.p_mod_instrument = None;
                chn.swap_sample_index = 0;
                chn.n_new_ins = 0;
                return;
            }
            p_smp = None;
        }
        let mut return_after_volume_adjust = false;
        let mut instrument_changed = p_ins != chn.p_mod_instrument;
        let sample_changed = chn.p_mod_sample.is_some() && p_smp != chn.p_mod_sample;
        if !b_porta || instrument_changed || sample_changed {
            chn.micro_tuning = 0;
        }
        if sample_changed && b_porta {
            if m.behaviour(kITPortamentoInstrument) && m.song_flag(SONG_ITCOMPATGXX) && !chn.increment.is_zero() {
                p_smp = chn.p_mod_sample;
            }
            if (!instrument_changed && t & (MOD_TYPE_XM | MOD_TYPE_MT2) != 0 && p_ins.is_some())
                || t == MOD_TYPE_PLM
                || (t == MOD_TYPE_MOD && chn.is_sample_playing())
                || (m.behaviour(kST3PortaSampleChange) && chn.is_sample_playing())
            {
                return_after_volume_adjust = true;
            }
            if m.behaviour(kITResetFilterOnPortaSmpChange) && m.num_instruments == 0 {
                chn.trigger_note = true;
            }
        }
        if m.num_instruments > 0
            && !instrument_changed
            && sample_changed
            && chn.p_current_sample.is_some()
            && m.behaviour(kITMultiSampleInstrumentNumber)
            && !chn.row_command.is_note()
        {
            return_after_volume_adjust = true;
        }
        if !chn.is_sample_playing() && t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 && p_ins.is_none_or(|i| !m.instrument(i as u32).unwrap().has_valid_midi_channel()) {
            instrument_changed = true;
        }
        if (instrument_changed || sample_changed) && b_porta && m.behaviour(kFT2PortaIgnoreInstr) && (chn.p_mod_instrument.is_some() || chn.p_mod_sample.is_some()) {
            p_ins = chn.p_mod_instrument;
            p_smp = chn.p_mod_sample;
            instrument_changed = false;
        } else {
            chn.p_mod_instrument = p_ins;
        }
        let ins_ref = p_ins.and_then(|i| m.instrument(i as u32));
        // Update Volume
        if b_upd_vol && (t & (MOD_TYPE_MOD | MOD_TYPE_S3M) == 0 || p_smp.is_some_and(|s| m.samples[s as usize].has_playback_source())) {
            if let Some(s) = p_smp {
                let s = &m.samples[s as usize];
                if s.u_flags & SMP_NODEFAULTVOLUME == 0 {
                    chn.n_volume = s.n_volume as i32;
                }
            } else {
                chn.n_volume = 0;
            }
        }
        if return_after_volume_adjust && sample_changed {
            if let Some(s) = p_smp {
                let s = &m.samples[s as usize];
                if m.behaviour(kMODSampleSwap) {
                    chn.n_fine_tune = s.n_fine_tune as i16;
                }
                if t == MOD_TYPE_S3M && s.has_playback_source() {
                    chn.n_c5_speed = s.n_c5_speed as i32;
                }
            }
        }
        if return_after_volume_adjust {
            return;
        }
        chn.swap_sample_index = 0;
        chn.n_new_ins = 0;
        if let Some(ins) = ins_ref {
            if (!m.behaviour(kITNNAReset) && p_smp.is_some()) || ins.n_mix_plug != 0 || instrument_changed {
                chn.n_nna = ins.nna;
            }
        }
        chn.update_instrument_volume(p_smp.map(|s| m.samples[s as usize].n_global_vol), ins_ref.map(|i| i.n_global_vol));
        if (b_upd_vol || t & (MOD_TYPE_XM | MOD_TYPE_MT2) == 0) && !m.behaviour(kITPanningReset) {
            m.apply_instrument_panning(ps_flags, chn, p_ins, p_smp);
        }
        if b_reset_env {
            let reset;
            let reset_always;
            if m.behaviour(kITEnvelopeReset) {
                let ins_number = instr != 0;
                reset = chn.n_length == 0
                    || (ins_number && b_porta && m.song_flag(SONG_ITCOMPATGXX))
                    || (ins_number && !b_porta && chn.has(CHN_NOTEFADE | CHN_KEYOFF) && m.song_flag(SONG_ITOLDEFFECTS));
                reset_always = chn.n_fade_out_vol == 0
                    || instrument_changed
                    || if m.behaviour(kITCarryAfterNoteOff) { !chn.row_command.is_note() } else { chn.has(CHN_KEYOFF) };
            } else {
                reset = !b_porta
                    || t & (MOD_TYPE_IT | MOD_TYPE_MPT | MOD_TYPE_DBM) == 0
                    || m.song_flag(SONG_ITCOMPATGXX)
                    || chn.n_length == 0
                    || (chn.has(CHN_NOTEFADE) && chn.n_fade_out_vol == 0);
                reset_always = t & (MOD_TYPE_IT | MOD_TYPE_MPT | MOD_TYPE_DBM) == 0 || instrument_changed || ins_ref.is_none() || chn.has(CHN_KEYOFF | CHN_NOTEFADE);
            }
            if reset {
                chn.set(CHN_FASTVOLRAMP);
                if let Some(ins) = ins_ref {
                    if reset_always {
                        chn.reset_envelopes();
                    } else {
                        let compat = m.behaviour(kITCompatGxxCarryPortaWithIns) && b_porta && m.song_flag(SONG_ITCOMPATGXX);
                        if !ins.vol_env.has(ENV_CARRY) {
                            chn.vol_env.reset();
                        } else if compat {
                            chn.vol_env.n_env_position = last_env.map_or(0, |e| e.0);
                        }
                        if !ins.pan_env.has(ENV_CARRY) {
                            chn.pan_env.reset();
                        } else if compat {
                            chn.pan_env.n_env_position = last_env.map_or(0, |e| e.1);
                        }
                        if !ins.pitch_env.has(ENV_CARRY) {
                            chn.pitch_env.reset();
                        } else if compat {
                            chn.pitch_env.n_env_position = last_env.map_or(0, |e| e.2);
                        }
                    }
                }
                if !m.behaviour(kITVibratoTremoloPanbrello) {
                    chn.n_auto_vib_depth = 0;
                    chn.n_auto_vib_pos = 0;
                }
            } else if let Some(ins) = ins_ref {
                if !ins.vol_env.has(ENV_ENABLED) {
                    if m.behaviour(kITPortamentoInstrument) {
                        chn.vol_env.reset();
                    } else {
                        chn.reset_envelopes();
                    }
                }
            }
        }
        // Invalid sample?
        if p_smp.is_none() && ins_ref.is_none_or(|i| !i.has_valid_midi_channel()) {
            chn.p_mod_sample = None;
            chn.n_ins_vol = 0;
            return;
        }
        let was_key_off = chn.has(CHN_KEYOFF);
        if b_porta && p_smp == chn.p_mod_sample && p_smp.is_some() {
            if instrument_changed && ins_ref.is_some() && m.behaviour(kITNoSustainOnPortamento) {
                chn.reset_flag(CHN_KEYOFF | CHN_NOTEFADE);
            }
            if t & (MOD_TYPE_S3M | MOD_TYPE_IT | MOD_TYPE_MPT) != 0 && chn.n_length != 0 {
                return;
            }
            if t != MOD_TYPE_XM || !m.behaviour(kITFT2DontResetNoteOffOnPorta) || chn.row_command.instr != 0 {
                chn.reset_flag(CHN_KEYOFF | CHN_NOTEFADE);
            }
            chn.dw_flags &= CHN_CHANNELFLAGS | CHN_PINGPONGFLAG;
        } else {
            chn.reset_flag(CHN_KEYOFF | CHN_NOTEFADE);
            if (m.behaviour(kITPingPongNoReset) || t & (MOD_TYPE_IT | MOD_TYPE_MPT) == 0) && p_smp == chn.p_mod_sample && !instrument_changed {
                chn.dw_flags &= CHN_CHANNELFLAGS | CHN_PINGPONGFLAG;
            } else {
                chn.dw_flags &= CHN_CHANNELFLAGS;
            }
            if let Some(ins) = ins_ref {
                chn.vol_env.flags = ins.vol_env.flags;
                chn.pan_env.flags = ins.pan_env.flags;
                chn.pitch_env.flags = ins.pitch_env.flags;
                if (ins.pitch_env.flags & (ENV_ENABLED | ENV_FILTER)) == (ENV_ENABLED | ENV_FILTER) && !m.behaviour(kITFilterBehaviour) && chn.n_cut_off == 0 {
                    chn.n_cut_off = 0x7F;
                }
                if ins.is_cutoff_enabled() {
                    chn.n_cut_off = ins.cutoff();
                }
                if ins.is_resonance_enabled() {
                    chn.n_resonance = ins.resonance();
                }
            }
        }
        let Some(smp_idx) = p_smp else {
            chn.p_mod_sample = None;
            chn.n_length = 0;
            return;
        };
        let smp = &m.samples[smp_idx as usize];
        if b_porta && chn.n_length == 0 && (m.behaviour(kFT2PortaNoNote) || m.behaviour(kITPortaNoNote)) {
            chn.increment = SamplePosition(0);
        }
        if chn.row_command.note == NOTE_KEYOFF && m.behaviour(kITInstrWithNoteOffOldEffects) && m.song_flag(SONG_ITOLDEFFECTS) && sample_changed {
            if let Some(old) = chn.p_mod_sample {
                chn.dw_flags |= m.samples[old as usize].u_flags & CHN_SAMPLEFLAGS;
            }
            chn.n_ins_vol = old_ins_vol;
            chn.n_volume = smp.n_volume as i32;
            if smp.u_flags & CHN_PANNING != 0 {
                chn.set_instrument_pan(smp.n_pan as i32, m);
            }
            return;
        }
        chn.p_mod_sample = Some(smp_idx);
        chn.n_length = smp.n_length;
        chn.n_loop_start = smp.n_loop_start;
        chn.n_loop_end = smp.n_loop_end;
        if m.behaviour(kMODOneShotLoops) && chn.n_loop_start == 0 {
            chn.n_loop_end = smp.n_length;
        }
        chn.dw_flags |= smp.u_flags & CHN_SAMPLEFLAGS;
        if m.behaviour(kITVibratoTremoloPanbrello) {
            chn.n_auto_vib_depth = 0;
            chn.n_auto_vib_pos = 0;
        }
        if !b_porta || sample_changed || t & (MOD_TYPE_MOD | MOD_TYPE_XM) == 0 {
            chn.n_c5_speed = smp.n_c5_speed as i32;
            chn.n_fine_tune = smp.n_fine_tune as i16;
        }
        chn.n_transpose = if m.use_finetune_and_transpose() { smp.relative_tone } else { 0 };
        if !m.behaviour(kFT2PortaTargetNoReset) && t != MOD_TYPE_MOD {
            chn.n_portamento_dest = 0;
        }
        chn.m_portamento_fine_steps = 0;
        if chn.has(CHN_SUSTAINLOOP) && (!m.behaviour(kITNoSustainOnPortamento) || !b_porta || (ins_ref.is_some() && !was_key_off)) {
            chn.n_loop_start = smp.n_sustain_start;
            chn.n_loop_end = smp.n_sustain_end;
            if chn.has(CHN_PINGPONGSUSTAIN) {
                chn.set(CHN_PINGPONGLOOP);
            }
            chn.set(CHN_LOOP);
        }
        if chn.has(CHN_LOOP) && chn.n_loop_end < chn.n_length {
            chn.n_length = chn.n_loop_end;
        }
        if chn.position.uint() >= chn.n_length && t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 {
            chn.position = SamplePosition(0);
        }
        instr = 0;
        let _ = instr;
    }

    /// `GetNNAChannel`.
    pub fn get_nna_channel(&self, nchn: usize) -> ChannelIndex {
        let nc = self.num_channels();
        for i in nc..self.ps.chn.len() {
            let c = &self.ps.chn[i];
            if c.n_length != 0 {
                continue;
            }
            return i as ChannelIndex;
        }
        let mut vol: i32 = 0x800100;
        if nchn < self.ps.chn.len() {
            let src = &self.ps.chn[nchn];
            if src.n_fade_out_vol == 0 && src.n_length != 0 {
                return CHANNELINDEX_INVALID;
            }
            vol = (src.n_real_volume << 9) | src.n_volume;
        }
        let mut result = CHANNELINDEX_INVALID;
        let mut envpos = 0u32;
        for i in nc..self.ps.chn.len() {
            let c = &self.ps.chn[i];
            if c.has(CHN_ADLIB) {
                return i as ChannelIndex;
            }
            if c.n_length != 0 && c.n_fade_out_vol == 0 {
                return i as ChannelIndex;
            }
            let mut v = (c.n_real_volume << 9) | c.n_volume;
            if c.has(CHN_LOOP) {
                v /= 2;
            }
            if c.n_length == 0 && c.n_master_chn != 0 {
                let age = c.n_restore_pan_on_new_note as u32;
                v -= ((age * age).min(i32::MAX as u32 / 16) * 16) as i32;
            }
            if v < vol || (v == vol && (c.vol_env.n_env_position > envpos || c.vol_env.flags & ENV_ENABLED == 0)) {
                envpos = c.vol_env.n_env_position;
                vol = v;
                result = i as ChannelIndex;
            }
        }
        result
    }

    /// `CheckNNA`.
    pub fn check_nna(&mut self, nchn: usize, mut instr: u32, note: i32, force_cut: bool) -> ChannelIndex {
        if !(note >= NOTE_MIN as i32 && note <= NOTE_MAX as i32) {
            return CHANNELINDEX_INVALID;
        }
        let t = self.m.mod_type;
        if t & (MOD_TYPE_IT | MOD_TYPE_MPT | MOD_TYPE_MT2) == 0 || self.m.num_instruments == 0 || force_cut {
            let src = &self.ps.chn[nchn];
            if src.has(CHN_MUTE) {
                return CHANNELINDEX_INVALID;
            }
            if src.has(CHN_ADLIB) {
                if let Some(opl) = &mut self.opl { opl.note_cut(nchn, false); }
                return CHANNELINDEX_INVALID;
            }
            if src.n_length == 0 || (src.right_vol | src.left_vol) == 0 {
                return CHANNELINDEX_INVALID;
            }
            let nna = self.get_nna_channel(nchn);
            if self.ps.last_moved_channel as usize == nchn {
                self.ps.last_moved_channel = nna;
            }
            if nna == CHANNELINDEX_INVALID {
                return CHANNELINDEX_INVALID;
            }
            self.ps.chn[nchn].nna_generation = self.ps.chn[nchn].nna_generation.wrapping_add(1);
            let mut c = self.ps.chn[nchn].clone();
            c.reset_flag(CHN_VIBRATO | CHN_TREMOLO | CHN_MUTE | CHN_PORTAMENTO);
            c.n_panbrello_offset = 0;
            c.n_master_chn = (nchn + 1) as ChannelIndex;
            c.n_command = CMD_NONE;
            c.row_command.clear();
            c.n_fade_out_vol = 0;
            c.set(CHN_NOTEFADE | CHN_FASTVOLRAMP);
            c.n_restore_pan_on_new_note = 0; // nnaChannelAge
            self.ps.chn[nna as usize] = c;
            let src = &mut self.ps.chn[nchn];
            src.n_length = 0;
            src.position = SamplePosition(0);
            src.n_r_ofs = 0;
            src.n_l_ofs = 0;
            src.right_vol = 0;
            src.left_vol = 0;
            return nna;
        }
        if instr > self.m.num_instruments as u32 {
            instr = 0;
        }
        let m = &self.m;
        let mut p_sample = self.ps.chn[nchn].p_mod_sample;
        let p_ins = if instr > 0 { m.instrument_index(instr) } else { self.ps.chn[nchn].p_mod_instrument };
        let mut dna_note = note;
        if let Some(ins) = p_ins.and_then(|i| m.instrument(i as u32)) {
            let smp = ins.keyboard[(note - NOTE_MIN as i32) as usize] as u32;
            if !m.behaviour(kITDCTBehaviour) || !m.behaviour(kITRealNoteMapping) {
                dna_note = ins.note_map[(note - NOTE_MIN as i32) as usize] as i32;
            }
            if smp > 0 {
                p_sample = Some(if smp <= m.num_samples as u32 { smp as SampleIndex } else { 0 });
            } else if m.behaviour(kITEmptyNoteMapSlot) && !ins.has_valid_midi_channel() {
                return CHANNELINDEX_INVALID;
            }
        }
        if self.ps.chn[nchn].has(CHN_MUTE) {
            return CHANNELINDEX_INVALID;
        }
        let nc = m.num_channels();
        for i in nchn..self.ps.chn.len() {
            if i < nc && i != nchn {
                continue;
            }
            let chn = &mut self.ps.chn[i];
            if !((chn.n_master_chn as usize == nchn + 1 || i == nchn) && chn.p_mod_instrument.is_some()) {
                continue;
            }
            let Some(cins) = chn.p_mod_instrument.and_then(|ci| m.instrument(ci as u32)) else {
                continue;
            };
            let mut apply_dna = false;
            match cins.dct {
                DuplicateCheckType::None => {}
                DuplicateCheckType::Note => {
                    if dna_note != NOTE_NONE as i32 && chn.n_note as i32 == dna_note && p_ins == chn.p_mod_instrument {
                        apply_dna = true;
                    }
                }
                DuplicateCheckType::Sample => {
                    if p_sample.is_some() && p_sample == chn.p_mod_sample && (p_ins == chn.p_mod_instrument || !m.behaviour(kITDCTBehaviour)) {
                        apply_dna = true;
                    }
                }
                DuplicateCheckType::Instrument => {
                    if p_ins == chn.p_mod_instrument {
                        apply_dna = true;
                    }
                }
                DuplicateCheckType::Plugin => {
                    if let Some(pi) = p_ins.and_then(|x| m.instrument(x as u32)) {
                        if pi.n_mix_plug != 0 && pi.n_mix_plug == cins.n_mix_plug {
                            apply_dna = true;
                        }
                    }
                }
            }
            if apply_dna {
                match cins.dna {
                    DuplicateNoteAction::NoteCut => {
                        m.key_off(chn);
                        chn.n_volume = 0;
                    }
                    DuplicateNoteAction::NoteOff => m.key_off(chn),
                    DuplicateNoteAction::NoteFade => chn.set(CHN_NOTEFADE),
                }
                if chn.n_volume == 0 {
                    chn.n_fade_out_vol = 0;
                    chn.set(CHN_NOTEFADE | CHN_FASTVOLRAMP);
                }
            }
        }
        self.ps.last_moved_channel = CHANNELINDEX_INVALID;
        if !self.ps.chn[nchn].is_sample_playing() {
            return CHANNELINDEX_INVALID;
        }
        let nna = self.get_nna_channel(nchn);
        if self.ps.chn[nchn].p_mod_instrument == p_ins {
            self.ps.last_moved_channel = nna;
        }
        if nna == CHANNELINDEX_INVALID {
            return CHANNELINDEX_INVALID;
        }
        self.ps.chn[nchn].nna_generation = self.ps.chn[nchn].nna_generation.wrapping_add(1);
        let mut c = self.ps.chn[nchn].clone();
        c.reset_flag(CHN_VIBRATO | CHN_TREMOLO | CHN_PORTAMENTO);
        c.n_panbrello_offset = 0;
        c.n_master_chn = if nchn < nc { (nchn + 1) as ChannelIndex } else { 0 };
        c.n_command = CMD_NONE;
        c.n_restore_pan_on_new_note = 0; // nnaChannelAge
        match self.ps.chn[nchn].n_nna {
            NewNoteAction::NoteOff => self.m.key_off(&mut c),
            NewNoteAction::NoteCut => {
                c.n_fade_out_vol = 0;
                c.set(CHN_NOTEFADE);
            }
            NewNoteAction::NoteFade => c.set(CHN_NOTEFADE),
            NewNoteAction::Continue => {}
        }
        if c.n_volume == 0 {
            c.n_fade_out_vol = 0;
            c.set(CHN_NOTEFADE | CHN_FASTVOLRAMP);
        }
        self.ps.chn[nna as usize] = c;
        let src = &mut self.ps.chn[nchn];
        src.n_length = 0;
        src.position = SamplePosition(0);
        src.n_r_ofs = 0;
        src.n_l_ofs = 0;
        nna
    }

    /// `NoteCut`.
    pub fn note_cut(&mut self, nchn: usize, n_tick: u32, cut_sample: bool) {
        let m = &self.m;
        let ps = &mut self.ps;
        let mut tick_count = ps.tick_count;
        let row_length = ps.music_speed.wrapping_add(ps.frame_delay);
        let pattern_delay = ps.pattern_delay;
        let chn = &mut ps.chn[nchn];
        if m.behaviour(kITNoteCutWithPorta) && chn.row_command.is_note() && chn.row_command.is_tone_portamento() {
            if tick_count < row_length {
                return;
            }
            if pattern_delay != 0 && tick_count >= row_length && row_length != 0 {
                tick_count %= row_length;
            }
        }
        if tick_count == n_tick {
            if cut_sample {
                if m.behaviour(kITNoteCutWithPorta) {
                    chn.n_period = 0;
                }
                chn.increment = SamplePosition(0);
                chn.n_fade_out_vol = 0;
                chn.set(CHN_NOTEFADE);
            } else {
                chn.n_volume = 0;
            }
            chn.set(CHN_FASTVOLRAMP);
            if chn.has(CHN_ADLIB) {
                if let Some(opl) = &mut self.opl { opl.note_cut(nchn, false); }
            }
        }
    }

    /// `SetSpeed`.
    pub fn set_speed(&mut self, param: u32) {
        if param > 0 {
            self.ps.music_speed = param;
        }
        if self.m.mod_type == MOD_TYPE_STM && param > 0 {
            self.ps.music_speed = (param >> 4).max(1);
            self.ps.music_tempo = convert_st2_tempo(param as u8);
        }
    }

    /// `SetTempo`.
    pub fn set_tempo(&mut self, param: Tempo) {
        let t = self.m.mod_type;
        let min_tempo = min_tempo_param(t);
        let (spec_min, spec_max) = spec_tempo(t);
        let mut max_tempo = Tempo::new(spec_max, 0);
        if t & (MOD_TYPE_XM | MOD_TYPE_IT | MOD_TYPE_MPT) == 0 {
            max_tempo = Tempo::new(1000, 0);
        }
        if self.m.behaviour(kTempoClamp) {
            max_tempo = Tempo::new(255, 0);
        }
        let first_tick = self.ps.flag(SONG_FIRSTTICK);
        if param >= min_tempo && first_tick == !self.m.behaviour(kMODTempoOnSecondTick) {
            self.ps.music_tempo = param.min(max_tempo);
        } else if param < min_tempo && !first_tick {
            let diff = Tempo::new(param.int() & 0x0F, 0);
            if (param.int() & 0xF0) == 0x10 {
                self.ps.music_tempo = self.ps.music_tempo + diff;
            } else {
                self.ps.music_tempo = self.ps.music_tempo - diff;
            }
            let tempo_min = Tempo::new(spec_min, 0);
            // Limit() on an unsigned value that wrapped below zero clamps to the maximum.
            self.ps.music_tempo = self.ps.music_tempo.clamp(tempo_min, max_tempo);
        }
    }

    /// `PatternLoop`.
    pub fn pattern_loop(&mut self, nchn: usize, param: u8) {
        let m = &self.m;
        if m.behaviour(kST3NoMutedChannels) && self.ps.chn[nchn].has(CHN_MUTE | CHN_SYNCMUTE) {
            return;
        }
        let ci = if m.mod_type == MOD_TYPE_S3M { 0 } else { nchn };
        let row = self.ps.row;
        if param == 0 {
            self.ps.chn[ci].n_pattern_loop = row;
            return;
        }
        if self.ps.chn[ci].n_pattern_loop_count != 0 {
            self.ps.chn[ci].n_pattern_loop_count -= 1;
            if self.ps.chn[ci].n_pattern_loop_count == 0 {
                if m.behaviour(kITPatternLoopTargetReset) || m.mod_type == MOD_TYPE_S3M {
                    self.ps.chn[ci].n_pattern_loop = row + 1;
                }
                return;
            }
        } else {
            if !m.behaviour(kITFT2PatternLoop) && m.mod_type & (MOD_TYPE_MOD | MOD_TYPE_S3M) == 0 {
                for (i, other) in self.ps.chn[..m.num_channels()].iter().enumerate() {
                    if i != ci && other.n_pattern_loop_count != 0 {
                        return;
                    }
                }
            }
            self.ps.chn[ci].n_pattern_loop_count = param;
        }
        self.ps.next_pat_start_row = self.ps.chn[ci].n_pattern_loop;
        let target = self.ps.chn[ci].n_pattern_loop;
        if target != ROWINDEX_INVALID {
            if self.ps.break_row != ROWINDEX_INVALID && m.behaviour(kFT2PatternLoopWithJumps) {
                self.ps.break_row = target;
            }
            self.ps.pat_loop_row = target;
            if m.behaviour(kITPatternLoopWithJumps) {
                self.ps.pos_jump = ORDERINDEX_INVALID;
            }
        }
    }

    /// `GlobalVolSlide`.
    pub fn global_vol_slide(&mut self, mut param: u8, chn: usize) {
        let t = self.m.mod_type;
        if self.m.song_flag(SONG_AUTO_GLOBALVOL) {
            self.ps.chn[chn].auto_slide.set_active(crate::channel::AutoSlide::GlobalVolumeSlide, param != 0);
        }
        if param != 0 {
            self.ps.chn[chn].n_old_global_vol_slide = param;
        } else {
            param = self.ps.chn[chn].n_old_global_vol_slide;
        }
        if t & (MOD_TYPE_XM | MOD_TYPE_MT2) != 0 {
            if param & 0xF0 != 0 {
                param &= 0xF0;
            } else {
                param &= 0x0F;
            }
        }
        let first = self.ps.flag(SONG_FIRSTTICK);
        let it_like = MOD_TYPE_IT | MOD_TYPE_MPT | MOD_TYPE_IMF | MOD_TYPE_J2B | MOD_TYPE_MID | MOD_TYPE_AMS | MOD_TYPE_DBM;
        let mut slide: i32 = 0;
        if (param & 0x0F) == 0x0F && (param & 0xF0) != 0 {
            if first {
                slide = (param >> 4) as i32 * 2;
            }
        } else if (param & 0xF0) == 0xF0 && (param & 0x0F) != 0 {
            if first {
                slide = -((param & 0x0F) as i32 * 2);
            }
        } else if !first {
            if param & 0xF0 != 0 {
                if t & it_like == 0 || (param & 0x0F) == 0 {
                    slide = ((param & 0xF0) >> 4) as i32 * 2;
                }
            } else {
                slide = -((param & 0x0F) as i32 * 2);
            }
        }
        if slide != 0 {
            if t & it_like == 0 {
                slide *= 2;
            }
            slide += self.ps.global_volume;
            self.ps.global_volume = slide.clamp(0, 256);
        }
    }
}

/// `ConvertST2Tempo`.
pub fn convert_st2_tempo(tempo: u8) -> Tempo {
    const FACTOR: [u8; 16] = [140, 50, 25, 15, 10, 7, 6, 4, 3, 3, 2, 2, 2, 2, 1, 1];
    const RATE: i32 = 23863;
    let mut spt = RATE / (50 - ((FACTOR[(tempo >> 4) as usize] as i32 * (tempo & 0x0F) as i32) >> 4));
    if spt <= 0 {
        spt += 65536;
    }
    Tempo(muldivrfloor(RATE as i64, 5 * Tempo::FRACT_FACT, (spt * 2) as u32) as u32)
}
