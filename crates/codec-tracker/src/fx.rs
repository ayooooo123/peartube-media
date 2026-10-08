//! Pattern effects, ported from libopenmpt 0.8.9 `soundlib/Snd_fx.cpp`
//! (`ProcessEffects` and the effect helpers) and `Snd_flt.cpp`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

#![allow(dead_code)]

use crate::channel::{AutoSlide, ModChannel};
use crate::command::*;
use crate::defs::pb::*;
use crate::defs::*;
use crate::player::Player;
use crate::sndfile::Module;
use crate::tables::*;

/// The play-state values the channel effect helpers read.
#[derive(Clone, Copy, Debug)]
pub struct Tk {
    pub flags: u16,
    pub tick_count: u32,
    pub music_speed: u32,
    pub frame_delay: u32,
    pub pattern_delay: u32,
    pub samples_per_tick: u32,
    pub mixing_freq: u32,
}

impl Tk {
    pub fn first_tick(&self) -> bool {
        self.flags & SONG_FIRSTTICK != 0
    }
    pub fn ticks_on_row(&self) -> u32 {
        (self.music_speed.wrapping_add(self.frame_delay)).wrapping_mul(self.pattern_delay.max(1))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PanBits {
    Pan4,
    Pan6,
    Pan8,
}

const GLOBALVOL_7BIT_FORMATS: u32 = MOD_TYPE_IT
    | MOD_TYPE_MPT
    | MOD_TYPE_IMF
    | MOD_TYPE_J2B
    | MOD_TYPE_MID
    | MOD_TYPE_AMS
    | MOD_TYPE_DBM
    | MOD_TYPE_PTM
    | MOD_TYPE_MDL
    | MOD_TYPE_DTM;

fn mod2xm_finetune(v: i32) -> i16 {
    ((v as u8) << 4) as i8 as i16
}

fn sat_i32(v: i64) -> i32 {
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

impl Module {
    fn lin_down(&self, i: usize) -> u32 {
        if self.behaviour(kPeriodsAreHertz) { LINEAR_SLIDE_DOWN_TABLE[i] } else { LINEAR_SLIDE_UP_TABLE[i] }
    }
    fn lin_up(&self, i: usize) -> u32 {
        if self.behaviour(kPeriodsAreHertz) { LINEAR_SLIDE_UP_TABLE[i] } else { LINEAR_SLIDE_DOWN_TABLE[i] }
    }
    fn fine_lin_down(&self, i: usize) -> u32 {
        if self.behaviour(kPeriodsAreHertz) { FINE_LINEAR_SLIDE_DOWN_TABLE[i] } else { FINE_LINEAR_SLIDE_UP_TABLE[i] }
    }
    fn fine_lin_up(&self, i: usize) -> u32 {
        if self.behaviour(kPeriodsAreHertz) { FINE_LINEAR_SLIDE_UP_TABLE[i] } else { FINE_LINEAR_SLIDE_DOWN_TABLE[i] }
    }

    /// `DoFreqSlide`: returns the slid period.
    pub fn do_freq_slide(&self, chn: &mut ModChannel, mut period: i32, amount: i32, is_tone_porta: bool) -> i32 {
        if period == 0 || amount == 0 {
            return period;
        }
        let t = self.mod_type;
        if t == MOD_TYPE_669 {
            period = period.wrapping_add(amount.wrapping_mul(20));
        } else if t == MOD_TYPE_FAR {
            period = period.wrapping_add(amount.wrapping_mul(36318) / 1024);
        } else if self.song_flag(SONG_LINEARSLIDES) && t & (MOD_TYPE_XM | MOD_TYPE_MOD) == 0 {
            let old = period;
            let mut abs = amount.unsigned_abs();
            if abs < 16 {
                if amount > 0 {
                    period = muldivr(period, self.fine_lin_up(abs as usize) as i32, 65536);
                } else {
                    period = muldivr(period, self.fine_lin_down(abs as usize) as i32, 65536);
                }
            } else {
                abs /= 4;
                while abs > 0 {
                    let n = abs.min(255);
                    if amount > 0 {
                        period = muldivr(period, self.lin_up(n as usize) as i32, 65536);
                    } else {
                        period = muldivr(period, self.lin_down(n as usize) as i32, 65536);
                    }
                    abs -= n;
                }
            }
            if period == old {
                let inc = self.behaviour(kPeriodsAreHertz) == (amount > 0);
                if inc && period < i32::MAX {
                    period += 1;
                } else if !inc && period > 1 {
                    period -= 1;
                }
            }
        } else if !self.song_flag(SONG_LINEARSLIDES) && self.behaviour(kPeriodsAreHertz) {
            if amount < 0 {
                let num = (1712u64 * 8363) * (period as u32 as u64);
                let den = (period as u32 as u64) * ((-(amount as i64)) as u64) + 1712 * 8363;
                period = (num / den).min(i32::MAX as u64) as i32;
            } else if amount > 0 {
                let div = 1712i64 * 8363 - (period as i64) * (amount as i64);
                if div <= 0 {
                    if is_tone_porta {
                        return i32::MAX;
                    }
                    chn.n_fade_out_vol = 0;
                    chn.set(CHN_NOTEFADE | CHN_FASTVOLRAMP);
                    return 0;
                }
                period = sat_i32(((1712u64 * 8363) * (period as u32 as u64) / div as u64) as i64);
            }
        } else {
            period = period.wrapping_sub(amount);
        }
        if period < 1 {
            period = 1;
            if t == MOD_TYPE_S3M && !is_tone_porta {
                chn.n_fade_out_vol = 0;
                chn.set(CHN_NOTEFADE | CHN_FASTVOLRAMP);
            }
        }
        period
    }

    pub fn fine_portamento_up(&self, chn: &mut ModChannel, mut param: u8) {
        if self.mod_type == MOD_TYPE_XM {
            if param != 0 {
                chn.n_old_fine_porta_up_down = (chn.n_old_fine_porta_up_down & 0x0F) | (param << 4);
            } else {
                param = chn.n_old_fine_porta_up_down >> 4;
            }
        } else if self.mod_type == MOD_TYPE_MT2 {
            if param != 0 {
                chn.n_old_fine_porta_up_down = param;
            } else {
                param = chn.n_old_fine_porta_up_down;
            }
        }
        if chn.is_first_tick && chn.n_period != 0 && param != 0 {
            chn.n_period = self.do_freq_slide(chn, chn.n_period, param as i32 * 4, false);
        }
    }

    pub fn fine_portamento_down(&self, chn: &mut ModChannel, mut param: u8) {
        if self.mod_type == MOD_TYPE_XM {
            if param != 0 {
                chn.n_old_fine_porta_up_down = (chn.n_old_fine_porta_up_down & 0xF0) | (param & 0x0F);
            } else {
                param = chn.n_old_fine_porta_up_down & 0x0F;
            }
        } else if self.mod_type == MOD_TYPE_MT2 {
            if param != 0 {
                chn.n_old_fine_porta_up_down = param;
            } else {
                param = chn.n_old_fine_porta_up_down;
            }
        }
        if chn.is_first_tick && chn.n_period != 0 && param != 0 {
            chn.n_period = self.do_freq_slide(chn, chn.n_period, -(param as i32) * 4, false);
            if chn.n_period > 0xFFFF
                && !self.behaviour(kPeriodsAreHertz)
                && (!self.song_flag(SONG_LINEARSLIDES) || self.mod_type == MOD_TYPE_XM)
            {
                chn.n_period = 0xFFFF;
            }
        }
    }

    pub fn extra_fine_portamento_up(&self, chn: &mut ModChannel, mut param: u8) {
        if self.mod_type == MOD_TYPE_XM {
            if param != 0 {
                chn.n_old_extra_fine_porta_up_down = (chn.n_old_extra_fine_porta_up_down & 0x0F) | (param << 4);
            } else {
                param = chn.n_old_extra_fine_porta_up_down >> 4;
            }
        } else if self.mod_type == MOD_TYPE_MT2 {
            if param != 0 {
                chn.n_old_fine_porta_up_down = param;
            } else {
                param = chn.n_old_fine_porta_up_down;
            }
        }
        if chn.is_first_tick && chn.n_period != 0 && param != 0 {
            chn.n_period = self.do_freq_slide(chn, chn.n_period, param as i32, false);
        }
    }

    pub fn extra_fine_portamento_down(&self, chn: &mut ModChannel, mut param: u8) {
        if self.mod_type == MOD_TYPE_XM {
            if param != 0 {
                chn.n_old_extra_fine_porta_up_down = (chn.n_old_extra_fine_porta_up_down & 0xF0) | (param & 0x0F);
            } else {
                param = chn.n_old_extra_fine_porta_up_down & 0x0F;
            }
        } else if self.mod_type == MOD_TYPE_MT2 {
            if param != 0 {
                chn.n_old_fine_porta_up_down = param;
            } else {
                param = chn.n_old_fine_porta_up_down;
            }
        }
        if chn.is_first_tick && chn.n_period != 0 && param != 0 {
            chn.n_period = self.do_freq_slide(chn, chn.n_period, -(param as i32), false);
            if chn.n_period > 0xFFFF
                && !self.behaviour(kPeriodsAreHertz)
                && (!self.song_flag(SONG_LINEARSLIDES) || self.mod_type == MOD_TYPE_XM)
            {
                chn.n_period = 0xFFFF;
            }
        }
    }

    /// `PortamentoUp` (the PlayState overload).
    pub fn portamento_up(&self, tk: Tk, chn: &mut ModChannel, mut param: u8, do_fine_as_regular: bool) {
        if param != 0 && !self.behaviour(kITDoublePortamentoSlides) {
            if !self.behaviour(kFT2PortaUpDownMemory) {
                chn.n_old_porta_down = param;
            }
            chn.n_old_porta_up = param;
        } else {
            param = chn.n_old_porta_up;
        }
        let do_fine = !do_fine_as_regular && self.use_combined_portamento_commands();
        if self.mod_type == MOD_TYPE_PLM {
            chn.n_portamento_dest = 1;
        }
        if do_fine && param >= 0xE0 {
            if param & 0x0F != 0 {
                if param & 0xF0 == 0xF0 {
                    self.fine_portamento_up(chn, param & 0x0F);
                    return;
                } else if param & 0xF0 == 0xE0 && self.mod_type != MOD_TYPE_DBM {
                    self.extra_fine_portamento_up(chn, param & 0x0F);
                    return;
                }
            }
            if self.mod_type != MOD_TYPE_DBM {
                return;
            }
        }
        if !chn.is_first_tick || (tk.music_speed == 1 && self.behaviour(kSlidesAtSpeed1)) || self.song_flag(SONG_FASTPORTAS) {
            chn.n_period = self.do_freq_slide(chn, chn.n_period, param as i32 * 4, false);
        }
    }

    /// `PortamentoDown` (the PlayState overload).
    pub fn portamento_down(&self, tk: Tk, chn: &mut ModChannel, mut param: u8, do_fine_as_regular: bool) {
        if param != 0 && !self.behaviour(kITDoublePortamentoSlides) {
            if !self.behaviour(kFT2PortaUpDownMemory) {
                chn.n_old_porta_up = param;
            }
            chn.n_old_porta_down = param;
        } else {
            param = chn.n_old_porta_down;
        }
        let do_fine = !do_fine_as_regular && self.use_combined_portamento_commands();
        if self.mod_type == MOD_TYPE_PLM {
            chn.n_portamento_dest = 65535;
        }
        if do_fine && param >= 0xE0 {
            if param & 0x0F != 0 {
                if param & 0xF0 == 0xF0 {
                    self.fine_portamento_down(chn, param & 0x0F);
                    return;
                } else if param & 0xF0 == 0xE0 && self.mod_type != MOD_TYPE_DBM {
                    self.extra_fine_portamento_down(chn, param & 0x0F);
                    return;
                }
            }
            if self.mod_type != MOD_TYPE_DBM {
                return;
            }
        }
        if !chn.is_first_tick || (tk.music_speed == 1 && self.behaviour(kSlidesAtSpeed1)) || self.song_flag(SONG_FASTPORTAS) {
            chn.n_period = self.do_freq_slide(chn, chn.n_period, param as i32 * -4, false);
        }
    }

    pub fn portamento_fc(&self, chn: &mut ModChannel) {
        chn.fc_porta_tick = !chn.fc_porta_tick;
        if !chn.fc_porta_tick {
            return;
        }
        chn.n_period -= (chn.n_old_porta_up as i8 as i32) * 4;
    }

    pub fn tone_portamento_shares_effect_memory(&self) -> bool {
        (!self.song_flag(SONG_ITCOMPATGXX) && self.behaviour(kITPortaMemoryShare)) || self.mod_type == MOD_TYPE_PLM
    }

    pub fn init_tone_portamento(&self, chn: &mut ModChannel, mut param: u16) {
        if self.tone_portamento_shares_effect_memory() {
            if param == 0 {
                param = chn.n_old_porta_up as u16;
            }
            chn.n_old_porta_up = param as u8;
            chn.n_old_porta_down = param as u8;
        }
        if param != 0 {
            chn.portamento_slide = param;
        }
    }

    /// `TonePortamento` (the PlayState overload).
    pub fn tone_portamento(&self, tk: Tk, chn: &mut ModChannel, param: u16) -> i32 {
        chn.set(CHN_PORTAMENTO);
        if self.song_flag(SONG_AUTO_TONEPORTA) {
            chn.auto_slide
                .set_active(AutoSlide::TonePortamento, param != 0 || self.song_flag(SONG_AUTO_TONEPORTA_CONT));
        }
        if !self.behaviour(kITDoublePortamentoSlides) {
            self.init_tone_portamento(chn, param);
        }
        let mut delta = chn.portamento_slide as i32;
        if self.behaviour(kST3TonePortaWithAdlibNote) && chn.has(CHN_ADLIB) && chn.row_command.is_note() {
            return 0;
        }
        let t = self.mod_type;
        let mut do_porta = !chn.is_first_tick
            || t == MOD_TYPE_DBM
            || (tk.music_speed == 1 && self.behaviour(kSlidesAtSpeed1))
            || self.song_flag(SONG_FASTPORTAS);
        if t == MOD_TYPE_PLM && delta >= 0xF0 {
            delta -= 0xF0;
            do_porta = chn.is_first_tick;
        }
        delta *= if t == MOD_TYPE_669 { 2 } else { 4 };
        if chn.n_period != 0 && chn.n_portamento_dest != 0 && do_porta {
            let actual = if self.periods_are_frequencies() { delta } else { -delta };
            if self.behaviour(kITDoublePortamentoSlides) && delta == 0 && chn.row_command.command == CMD_TONEPORTAVOL {
                if chn.n_period > 1 && self.song_flag(SONG_LINEARSLIDES) {
                    chn.n_period -= 1;
                }
                if chn.n_period < chn.n_portamento_dest {
                    chn.n_period = chn.n_portamento_dest;
                }
            } else if chn.n_period < chn.n_portamento_dest || chn.porta_target_reached {
                chn.n_period = self.do_freq_slide(chn, chn.n_period, actual, true);
                if chn.n_period > chn.n_portamento_dest {
                    chn.n_period = chn.n_portamento_dest;
                }
            } else if chn.n_period > chn.n_portamento_dest {
                chn.n_period = self.do_freq_slide(chn, chn.n_period, -actual, true);
                if chn.n_period < chn.n_portamento_dest {
                    chn.n_period = chn.n_portamento_dest;
                }
                if chn.n_period == chn.n_portamento_dest && self.behaviour(kFT2PortaResetDirection) {
                    chn.porta_target_reached = true;
                }
            }
        }
        if chn.n_period == chn.n_portamento_dest && (self.behaviour(kITPortaTargetReached) || t == MOD_TYPE_MOD) {
            chn.n_portamento_dest = 0;
        }
        if do_porta { delta } else { 0 }
    }

    /// `TonePortamentoWithDuration`; `param == None` runs the slide.
    pub fn tone_portamento_with_duration(&self, tk: Tk, chn: &mut ModChannel, param: Option<u16>) {
        if let Some(param) = param {
            if !chn.row_command.is_note() {
                return;
            }
            chn.auto_slide.set_active(AutoSlide::TonePortamentoWithDuration, param != 0);
            if param == 0 {
                chn.n_period = chn.n_portamento_dest;
                return;
            }
            let source = self.note_from_period(chn.n_period as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32);
            let diff = (chn.row_command.note as i32 - source as i32).unsigned_abs();
            chn.portamento_slide = muldivr_unsigned(diff, 64, tk.music_speed.wrapping_mul(param as u32)) as u16;
        } else if chn.n_period != 0 && chn.n_portamento_dest != 0 {
            chn.set(CHN_PORTAMENTO);
            let slide = chn.portamento_slide as i32;
            let actual = if self.periods_are_frequencies() { slide } else { -slide };
            if chn.n_period < chn.n_portamento_dest {
                chn.n_period = self.do_freq_slide(chn, chn.n_period, actual, true);
                if chn.n_period >= chn.n_portamento_dest {
                    chn.n_period = chn.n_portamento_dest;
                    chn.n_portamento_dest = 0;
                }
            } else if chn.n_period > chn.n_portamento_dest {
                chn.n_period = self.do_freq_slide(chn, chn.n_period, -actual, true);
                if chn.n_period <= chn.n_portamento_dest {
                    chn.n_period = chn.n_portamento_dest;
                    chn.n_portamento_dest = 0;
                }
            }
        }
    }

    pub fn vibrato(&self, chn: &mut ModChannel, param: u32) {
        if param & 0x0F != 0 {
            chn.n_vibrato_depth = ((param & 0x0F) * 4) as u8;
        }
        if param & 0xF0 != 0 {
            chn.n_vibrato_speed = ((param >> 4) & 0x0F) as u8;
        }
        if self.song_flag(SONG_AUTO_VIBRATO) {
            chn.auto_slide.set_active(AutoSlide::Vibrato, param != 0);
        } else {
            chn.set(CHN_VIBRATO);
        }
    }

    pub fn fine_vibrato(&self, chn: &mut ModChannel, param: u32) {
        if param & 0x0F != 0 {
            chn.n_vibrato_depth = (param & 0x0F) as u8;
        }
        if param & 0xF0 != 0 {
            chn.n_vibrato_speed = ((param >> 4) & 0x0F) as u8;
        }
        if self.song_flag(SONG_AUTO_VIBRATO) {
            chn.auto_slide.set_active(AutoSlide::Vibrato, param != 0);
        } else {
            chn.set(CHN_VIBRATO);
        }
        if self.behaviour(kST3VibratoMemory) && (param & 0x0F) != 0 {
            chn.n_vibrato_depth = chn.n_vibrato_depth.wrapping_mul(4);
        }
    }

    pub fn panbrello(&self, chn: &mut ModChannel, param: u32) {
        if param & 0x0F != 0 {
            chn.n_panbrello_depth = (param & 0x0F) as u8;
        }
        if param & 0xF0 != 0 {
            chn.n_panbrello_speed = ((param >> 4) & 0x0F) as u8;
        }
    }

    pub fn tremolo(&self, chn: &mut ModChannel, param: u32) {
        if param & 0x0F != 0 {
            chn.n_tremolo_depth = ((param & 0x0F) << 2) as u8;
        }
        if param & 0xF0 != 0 {
            chn.n_tremolo_speed = ((param >> 4) & 0x0F) as u8;
        }
        if self.song_flag(SONG_AUTO_TREMOLO) {
            chn.auto_slide.set_active(AutoSlide::Tremolo, (param & 0x0F) != 0);
        } else {
            chn.set(CHN_TREMOLO);
        }
    }

    /// `Panning`.
    pub fn panning(&self, ps_flags: u16, chn: &mut ModChannel, mut param: u32, bits: PanBits) {
        if self.behaviour(kMODIgnorePanning) {
            return;
        }
        if ps_flags & SONG_SURROUNDPAN == 0 && (bits == PanBits::Pan8 || self.behaviour(kPanOverride)) {
            chn.reset_flag(CHN_SURROUND);
        }
        match bits {
            PanBits::Pan4 => chn.n_pan = ((param * 256 + 8) / 15) as i32,
            PanBits::Pan6 => {
                if param > 64 {
                    param = 64;
                }
                chn.n_pan = (param * 4) as i32;
            }
            PanBits::Pan8 => {
                if self.mod_type & (MOD_TYPE_S3M | MOD_TYPE_DSM | MOD_TYPE_AMF0 | MOD_TYPE_AMF | MOD_TYPE_MTM) == 0 {
                    chn.n_pan = param as i32;
                } else if param <= 0x80 {
                    chn.n_pan = (param << 1) as i32;
                } else if param == 0xA4 {
                    chn.set(CHN_SURROUND);
                    chn.n_pan = 0x80;
                }
            }
        }
        chn.set(CHN_FASTVOLRAMP);
        chn.n_restore_pan_on_new_note = 0;
        if self.behaviour(kPanOverride) {
            chn.n_pan_swing = 0;
            chn.n_panbrello_offset = 0;
        }
    }

    pub fn auto_volume_slide(&self, chn: &mut ModChannel, param: u8) {
        if self.song_flag(SONG_AUTO_VOLSLIDE_STK) {
            chn.n_old_volume_slide = param;
            chn.auto_slide.set_active(AutoSlide::VolumeSlideSTK, true);
        } else if param & 0x0F != 0 {
            self.fine_volume_down(chn, param, false);
            chn.auto_slide.set_active(AutoSlide::FineVolumeSlideDown, true);
        } else {
            self.fine_volume_up(chn, param, false);
            chn.auto_slide.set_active(AutoSlide::FineVolumeSlideUp, true);
        }
    }

    pub fn volume_down_etx(&self, tk: Tk, chn: &mut ModChannel, param: u8) {
        chn.auto_slide.set_active(AutoSlide::VolumeDownETX, param != 0);
        if param == 0 || tk.samples_per_tick == 0 {
            return;
        }
        let dur = muldivr_unsigned(tk.mixing_freq, 600, 1000) / param as u32;
        let needed = ((dur + tk.samples_per_tick / 2) / tk.samples_per_tick).max(1);
        chn.n_old_volume_slide = (256 / needed).min(255) as u8;
    }

    /// `VolumeSlide`.
    pub fn volume_slide(&self, tk: Tk, chn: &mut ModChannel, mut param: u8, vol_col: bool) {
        if !vol_col {
            if param != 0 {
                chn.n_old_volume_slide = param;
            } else {
                param = chn.n_old_volume_slide;
            }
        }
        let t = self.mod_type;
        if t & (MOD_TYPE_MOD | MOD_TYPE_XM | MOD_TYPE_MT2 | MOD_TYPE_MED | MOD_TYPE_DIGI | MOD_TYPE_STP | MOD_TYPE_DTM) != 0 {
            if param & 0xF0 != 0 {
                param &= 0xF0;
            } else {
                param &= 0x0F;
            }
        }
        let mut new_volume = chn.n_volume;
        if t & (MOD_TYPE_MOD | MOD_TYPE_XM | MOD_TYPE_AMF0 | MOD_TYPE_MED | MOD_TYPE_DIGI) == 0 {
            if param & 0x0F == 0x0F {
                if param & 0xF0 != 0 {
                    self.fine_volume_up(chn, param >> 4, false);
                    return;
                } else if chn.is_first_tick && !self.song_flag(SONG_FASTVOLSLIDES) {
                    new_volume -= 0x0F * 4;
                }
            } else if param & 0xF0 == 0xF0 {
                if param & 0x0F != 0 {
                    self.fine_volume_down(chn, param & 0x0F, false);
                    return;
                } else if chn.is_first_tick && !self.song_flag(SONG_FASTVOLSLIDES) {
                    new_volume += 0x0F * 4;
                }
            }
        }
        if !chn.is_first_tick || self.song_flag(SONG_FASTVOLSLIDES) || (tk.music_speed == 1 && t == MOD_TYPE_DBM) {
            if param & 0x0F != 0 {
                if t & (MOD_TYPE_IT | MOD_TYPE_MPT) == 0 || (param & 0xF0) == 0 {
                    new_volume -= (param & 0x0F) as i32 * 4;
                }
            } else {
                new_volume += ((param & 0xF0) >> 2) as i32;
            }
            if t == MOD_TYPE_MOD {
                chn.set(CHN_FASTVOLRAMP);
            }
        }
        chn.n_volume = new_volume.clamp(0, 256);
    }

    /// `PanningSlide`.
    pub fn panning_slide(&self, tk: Tk, chn: &mut ModChannel, mut param: u8, memory: bool) {
        if memory {
            if param != 0 {
                chn.n_old_pan_slide = param;
            } else {
                param = chn.n_old_pan_slide;
            }
        }
        let t = self.mod_type;
        if t & (MOD_TYPE_XM | MOD_TYPE_MT2) != 0 {
            if param & 0xF0 != 0 {
                param &= 0xF0;
            } else {
                param &= 0x0F;
            }
        }
        let first = tk.first_tick();
        let mut slide: i32 = 0;
        if t & (MOD_TYPE_XM | MOD_TYPE_MT2) == 0 {
            if (param & 0x0F) == 0x0F && (param & 0xF0) != 0 {
                if first {
                    slide = -(((param & 0xF0) / 4) as i32);
                }
            } else if (param & 0xF0) == 0xF0 && (param & 0x0F) != 0 {
                if first {
                    slide = (param & 0x0F) as i32 * 4;
                }
            } else if !first {
                if param & 0x0F != 0 {
                    if t & (MOD_TYPE_IT | MOD_TYPE_MPT) == 0 || (param & 0xF0) == 0 {
                        slide = (param & 0x0F) as i32 * 4;
                    }
                } else {
                    slide = -(((param & 0xF0) / 4) as i32);
                }
            }
        } else if !first {
            if param & 0xF0 != 0 {
                slide = ((param & 0xF0) / 4) as i32;
            } else {
                slide = -((param & 0x0F) as i32 * 4);
            }
            if self.behaviour(kFT2PanSlide) {
                slide /= 4;
            }
        }
        if slide != 0 {
            chn.n_pan = (slide + chn.n_pan).clamp(0, 256);
            chn.n_restore_pan_on_new_note = 0;
        }
    }

    pub fn fine_volume_up(&self, chn: &mut ModChannel, mut param: u8, vol_col: bool) {
        if self.mod_type == MOD_TYPE_XM {
            if param != 0 {
                chn.n_old_fine_vol_up_down = (param << 4) | (chn.n_old_fine_vol_up_down & 0x0F);
            } else {
                param = chn.n_old_fine_vol_up_down >> 4;
            }
        } else if vol_col {
            if param != 0 {
                chn.n_old_vol_param = param;
            } else {
                param = chn.n_old_vol_param;
            }
        } else if param != 0 {
            chn.n_old_fine_vol_up_down = param;
        } else {
            param = chn.n_old_fine_vol_up_down;
        }
        if chn.is_first_tick {
            chn.n_volume += param as i32 * 4;
            if chn.n_volume > 256 {
                chn.n_volume = 256;
            }
            if self.mod_type & MOD_TYPE_MOD != 0 {
                chn.set(CHN_FASTVOLRAMP);
            }
        }
    }

    pub fn fine_volume_down(&self, chn: &mut ModChannel, mut param: u8, vol_col: bool) {
        if self.mod_type == MOD_TYPE_XM {
            if param != 0 {
                chn.n_old_fine_vol_up_down = param | (chn.n_old_fine_vol_up_down & 0xF0);
            } else {
                param = chn.n_old_fine_vol_up_down & 0x0F;
            }
        } else if vol_col {
            if param != 0 {
                chn.n_old_vol_param = param;
            } else {
                param = chn.n_old_vol_param;
            }
        } else if param != 0 {
            chn.n_old_fine_vol_up_down = param;
        } else {
            param = chn.n_old_fine_vol_up_down;
        }
        if chn.is_first_tick {
            chn.n_volume -= param as i32 * 4;
            if chn.n_volume < 0 {
                chn.n_volume = 0;
            }
            if self.mod_type & MOD_TYPE_MOD != 0 {
                chn.set(CHN_FASTVOLRAMP);
            }
        }
    }

    pub fn channel_vol_slide(&self, tk: Tk, chn: &mut ModChannel, mut param: u8) {
        let mut slide: i32 = 0;
        if param != 0 {
            chn.n_old_chn_vol_slide = param;
        } else {
            param = chn.n_old_chn_vol_slide;
        }
        let first = tk.first_tick();
        if (param & 0x0F) == 0x0F && (param & 0xF0) != 0 {
            if first {
                slide = (param >> 4) as i32;
            }
        } else if (param & 0xF0) == 0xF0 && (param & 0x0F) != 0 {
            if first {
                slide = -((param & 0x0F) as i32);
            }
        } else if !first {
            if param & 0x0F != 0 {
                if self.mod_type & (MOD_TYPE_IT | MOD_TYPE_MPT | MOD_TYPE_J2B | MOD_TYPE_DBM) == 0 || (param & 0xF0) == 0 {
                    slide = -((param & 0x0F) as i32);
                }
            } else {
                slide = ((param & 0xF0) >> 4) as i32;
            }
        }
        if slide != 0 {
            chn.n_global_vol = (slide + chn.n_global_vol as i32).clamp(0, 64) as u8;
        }
    }

    /// `ChannelVolumeDownWithDuration`; `param == None` runs the slide.
    pub fn channel_volume_down_with_duration(&self, tk: Tk, chn: &mut ModChannel, param: Option<u16>) {
        if let Some(param) = param {
            chn.auto_slide.set_active(AutoSlide::VolumeDownWithDuration, param != 0);
            if param == 0 {
                chn.n_global_vol = 0;
                return;
            }
            chn.vol_slide_down_start = chn.n_global_vol;
            let total = (param as u32 * tk.music_speed).min(u16::MAX as u32) as u16;
            chn.vol_slide_down_total = total;
            chn.vol_slide_down_remain = total;
        } else if chn.vol_slide_down_total != 0 {
            if chn.vol_slide_down_remain != 0 {
                chn.vol_slide_down_remain -= 1;
                chn.n_global_vol =
                    muldivr(chn.vol_slide_down_start as i32, chn.vol_slide_down_remain as i32, chn.vol_slide_down_total as i32) as u8;
            } else {
                chn.n_global_vol = 0;
            }
        }
    }

    /// `NoteSlide`.
    pub fn note_slide(&self, tk: Tk, chn: &mut ModChannel, param: u32, slide_up: bool, retrig: bool) {
        if chn.is_first_tick {
            if param & 0xF0 != 0 {
                chn.note_slide_param = (param & 0xF0) as u8 | (chn.note_slide_param & 0x0F);
            }
            if param & 0x0F != 0 {
                chn.note_slide_param = (chn.note_slide_param & 0xF0) | (param & 0x0F) as u8;
            }
            chn.note_slide_counter = chn.note_slide_param >> 4;
        }
        let do_trigger = if self.mod_type == MOD_TYPE_OKT {
            (chn.note_slide_param & 0xF0) == 0x10 || tk.first_tick()
        } else if !chn.is_first_tick {
            chn.note_slide_counter = chn.note_slide_counter.wrapping_sub(1);
            chn.note_slide_counter == 0
        } else {
            false
        };
        if do_trigger {
            let speed = chn.note_slide_param >> 4;
            let steps = (chn.note_slide_param & 0x0F) as i32;
            chn.note_slide_counter = speed;
            let delta = if slide_up { steps } else { -steps };
            let base = self.note_from_period(chn.n_period as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32) as i32;
            chn.n_period = self.period_from_note((delta + base) as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32) as i32;
            if retrig {
                chn.position = SamplePosition(0);
            }
        }
    }

    /// `ExtendedChannelEffect` (S9x / X9x).
    pub fn extended_channel_effect(&self, chn: &mut ModChannel, param: u32, flags: &mut u16) {
        match param & 0x0F {
            0x00 => chn.reset_flag(CHN_SURROUND),
            0x01 => {
                chn.set(CHN_SURROUND);
                chn.n_pan = 128;
            }
            0x08 => {
                chn.reset_flag(CHN_REVERB);
                chn.set(CHN_NOREVERB);
            }
            0x09 => {
                chn.reset_flag(CHN_NOREVERB);
                chn.set(CHN_REVERB);
            }
            0x0A => *flags &= !SONG_SURROUNDPAN,
            0x0B => *flags |= SONG_SURROUNDPAN,
            0x0C => *flags &= !SONG_MPTFILTERMODE,
            0x0D => *flags |= SONG_MPTFILTERMODE,
            0x0E => chn.reset_flag(CHN_PINGPONGFLAG),
            0x0F => {
                if chn.position.is_zero() && chn.n_length != 0 && (chn.row_command.is_note() || !chn.has(CHN_LOOP)) {
                    chn.position.set((chn.n_length - 1) as i32, u32::MAX);
                }
                chn.set(CHN_PINGPONGFLAG);
            }
            _ => {}
        }
    }

    /// `SampleOffset`.
    pub fn sample_offset(&self, chn: &mut ModChannel, mut param: SmpLength) {
        param = param.min(MAX_SAMPLE_LENGTH);
        let t = self.mod_type;
        if self.behaviour(kST3OffsetWithoutInstrument) || t == MOD_TYPE_MED {
            chn.prev_note_offset = 0;
        }
        chn.prev_note_offset = chn.prev_note_offset.wrapping_add(param);
        if param >= chn.n_loop_end && t & (MOD_TYPE_S3M | MOD_TYPE_MTM) != 0 && chn.has(CHN_LOOP) && chn.n_loop_end > 0 {
            let len = chn.n_loop_end - chn.n_loop_start;
            if len > 0 {
                param = (param - chn.n_loop_start) % len + chn.n_loop_start;
            }
        }
        if t & (MOD_TYPE_MDL | MOD_TYPE_PTM) != 0 && chn.has(CHN_16BIT) {
            param /= 2;
        }
        let note = if self.behaviour(kITOffsetWithInstrNumber) && chn.row_command.instr != 0 {
            chn.n_new_note
        } else {
            chn.row_command.note
        };
        if ModCommand::is_note_of(note) || self.behaviour(kApplyOffsetWithoutNote) {
            if ModCommand::is_note_of(note) {
                if let Some(ins) = chn.p_mod_instrument.and_then(|i| self.instrument(i as u32)) {
                    let smp = ins.keyboard[(note - NOTE_MIN) as usize];
                    if smp == 0 || smp > self.num_samples {
                        return;
                    }
                }
            }
            if self.song_flag(SONG_PT_MODE) {
                chn.position.set(chn.prev_note_offset as i32, 0);
                chn.prev_note_offset = chn.prev_note_offset.wrapping_add(param);
            } else {
                chn.position.set(param as i32, 0);
            }
            if chn.position.uint() >= chn.n_length || (chn.has(CHN_LOOP) && chn.position.uint() >= chn.n_loop_end) {
                if self.behaviour(kFT2ST3OffsetOutOfRange) || t == MOD_TYPE_MTM {
                    chn.set(CHN_FASTVOLRAMP);
                    chn.n_period = 0;
                } else if t & (MOD_TYPE_XM | MOD_TYPE_MT2 | MOD_TYPE_MOD) == 0 {
                    if self.behaviour(kITOffset) {
                        if self.song_flag(SONG_ITOLDEFFECTS) {
                            chn.position.set(chn.n_length as i32, 0);
                        } else {
                            chn.position = SamplePosition(0);
                        }
                    } else {
                        chn.position.set(chn.n_loop_start as i32, 0);
                        if self.song_flag(SONG_ITOLDEFFECTS) && chn.n_length > 4 {
                            chn.position.set((chn.n_length - 2) as i32, 0);
                        }
                    }
                } else if t == MOD_TYPE_MOD && chn.has(CHN_LOOP) {
                    chn.position.set(chn.n_loop_start as i32, 0);
                }
            }
        } else if param < chn.n_length && t & (MOD_TYPE_MTM | MOD_TYPE_DMF | MOD_TYPE_MDL | MOD_TYPE_PLM) != 0 {
            chn.position.set(param as i32, 0);
        }
    }

    pub fn reverse_sample_offset(&self, chn: &mut ModChannel, param: u8) {
        if let Some(s) = chn.p_mod_sample {
            let len = self.samples[s as usize].n_length;
            if len > 0 {
                chn.set(CHN_PINGPONGFLAG);
                chn.reset_flag(CHN_LOOP);
                chn.n_length = len;
                let mut offset = (param as u32) << 8;
                if self.mod_type == MOD_TYPE_PTM && chn.has(CHN_16BIT) {
                    offset /= 2;
                }
                chn.position.set(((chn.n_length - 1) - offset.min(chn.n_length - 1)) as i32, 0);
            }
        }
    }

    /// `CalculateXParam`: the parameter extended by following `CMD_XPARAM`
    /// rows; returns (value, extended rows).
    pub fn calculate_xparam(&self, pat: PatternIndex, row: RowIndex, chn: usize) -> (u32, u32) {
        if !self.is_valid_pat(pat) {
            return (0, 0);
        }
        let p = &self.patterns[pat as usize];
        let nc = self.num_channels();
        let m = p.cell(row, chn, nc);
        let start_cmd = m.command;
        let mut val = m.param as u32;
        let max_commands = match m.command {
            CMD_OFFSET => 2,
            CMD_TEMPO | CMD_PATTERNBREAK | CMD_POSITIONJUMP | CMD_FINETUNE | CMD_FINETUNE_SMOOTH => 1,
            _ => return (val, 0),
        };
        let xm_tempo_fix = m.command == CMD_TEMPO && self.mod_type == MOD_TYPE_XM;
        let mut num_rows = (p.rows - row - 1).min(max_commands);
        let mut ext = 0;
        let mut r = row;
        while num_rows > 0 {
            r += 1;
            let mm = p.cell(r, chn, nc);
            if mm.command != CMD_XPARAM {
                break;
            }
            if xm_tempo_fix && (0x20..256).contains(&val) {
                val -= 0x20;
            }
            val = (val << 8) | mm.param as u32;
            num_rows -= 1;
            ext += 1;
        }
        if (start_cmd == CMD_FINETUNE || start_cmd == CMD_FINETUNE_SMOOTH) && ext == 0 {
            val <<= 8;
        }
        (val, ext)
    }

    /// `GetVolCmdTonePorta`.
    pub fn vol_cmd_tone_porta(&self, m: &ModCommand, start_tick: u32) -> (u16, bool) {
        if self.mod_type
            & (MOD_TYPE_IT
                | MOD_TYPE_MPT
                | MOD_TYPE_AMS
                | MOD_TYPE_DMF
                | MOD_TYPE_DBM
                | MOD_TYPE_IMF
                | MOD_TYPE_PSM
                | MOD_TYPE_J2B
                | MOD_TYPE_ULT
                | MOD_TYPE_OKT
                | MOD_TYPE_MT2
                | MOD_TYPE_MDL)
            != 0
        {
            (IMPULSE_TRACKER_PORTA_VOL_CMD[(m.vol & 0x0F) as usize] as u16, false)
        } else {
            let mut clear = false;
            let mut vol = m.vol as u16;
            if m.command == CMD_TONEPORTAMENTO && self.mod_type == MOD_TYPE_XM {
                clear = true;
                vol *= 2;
            }
            if self.behaviour(kFT2PortaDelay) && start_tick != 0 { (0, clear) } else { (vol * 16, clear) }
        }
    }

    /// `CutOffToFrequency`.
    pub fn cutoff_to_frequency(&self, mixing_freq: u32, cutoff: u32, env_modifier: i32) -> f32 {
        let computed = (cutoff as i32 * (env_modifier + 256)) as f32;
        let mut frequency = if self.mod_type != MOD_TYPE_IMF {
            let div = if self.song_flag(SONG_EXFILTERRANGE) { 20.0f32 * 512.0 } else { 24.0f32 * 512.0 };
            110.0f32 * 2.0f32.powf(0.25f32 + computed / div)
        } else {
            125.0f32 * 2.0f32.powf(computed * 6.0f32 / (127.0f32 * 512.0f32))
        };
        frequency = frequency.clamp(120.0, 20000.0);
        frequency.min(mixing_freq as f32 * 0.5)
    }

    /// `SetupChannelFilter`: returns the computed cutoff, or -1 if no filter.
    pub fn setup_channel_filter(&self, mixing_freq: u32, chn: &mut ModChannel, reset: bool, env_modifier: i32) -> i32 {
        let cutoff = (chn.n_cut_off as i32 + chn.n_cut_swing as i32).clamp(0, 127);
        let resonance = ((chn.n_resonance & 0x7F) as i32 + chn.n_res_swing as i32).clamp(0, 127);
        if !self.behaviour(kMPTOldSwingBehaviour) {
            chn.n_cut_off = cutoff as u8;
            chn.n_cut_swing = 0;
            chn.n_resonance = resonance as u8;
            chn.n_res_swing = 0;
        }
        let computed_cutoff = cutoff * (env_modifier + 256) / 256;
        if self.behaviour(kITFilterBehaviour) && resonance == 0 && computed_cutoff >= 254 {
            if chn.trigger_note {
                chn.reset_flag(CHN_FILTER);
            }
            return -1;
        }
        chn.set(CHN_FILTER);
        let dmpfac = 10.0f32.powf((-resonance) as f32 * ((24.0f32 / 128.0f32) / 20.0f32));
        let fc = self.cutoff_to_frequency(mixing_freq, cutoff as u32, env_modifier) * (2.0f32 * core::f32::consts::PI);
        let d;
        let e;
        if self.behaviour(kITFilterBehaviour) && !self.song_flag(SONG_EXFILTERRANGE) {
            let r = mixing_freq as f32 / fc;
            // clang contracts `dmpfac * r + dmpfac` into a fused multiply-add.
            d = dmpfac.mul_add(r, dmpfac) - 1.0f32;
            e = r * r;
        } else {
            let r = fc / mixing_freq as f32;
            let mut dd = (1.0f32 - 2.0f32 * dmpfac) * r;
            if dd > 2.0 {
                dd = 2.0;
            }
            d = (2.0f32 * dmpfac - dd) / r;
            e = 1.0f32 / (r * r);
        }
        let fg = 1.0f32 / (1.0f32 + d + e);
        let fb0 = (d + e + e) / (1.0f32 + d + e);
        let fb1 = -e / (1.0f32 + d + e);
        let conv = |x: f32| -> i32 {
            let v = (x * (1u32 << MIXING_FILTER_PRECISION) as f32).round();
            if v >= i32::MAX as f32 {
                i32::MAX
            } else if v <= i32::MIN as f32 {
                i32::MIN
            } else {
                v as i32
            }
        };
        match chn.n_filter_mode {
            FilterMode::HighPass => {
                chn.n_filter_a0 = conv(1.0f32 - fg);
                chn.n_filter_b0 = conv(fb0);
                chn.n_filter_b1 = conv(fb1);
                chn.n_filter_hp = -1;
            }
            _ => {
                chn.n_filter_a0 = conv(fg);
                chn.n_filter_b0 = conv(fb0);
                chn.n_filter_b1 = conv(fb1);
                if chn.n_filter_a0 == 0 {
                    chn.n_filter_a0 = 1;
                }
                chn.n_filter_hp = 0;
            }
        }
        if reset {
            chn.n_filter_y = [[0; 2]; 2];
        }
        computed_cutoff
    }
}

impl Player {
    pub fn tk(&self) -> Tk {
        Tk {
            flags: self.ps.flags,
            tick_count: self.ps.tick_count,
            music_speed: self.ps.music_speed,
            frame_delay: self.ps.frame_delay,
            pattern_delay: self.ps.pattern_delay,
            samples_per_tick: self.ps.samples_per_tick,
            mixing_freq: self.settings.mixing_freq,
        }
    }

    /// `ProcessSampleOffset`.
    fn process_sample_offset(&mut self, nchn: usize) {
        let (xval, ext) = self.m.calculate_xparam(self.ps.pattern, self.ps.row, nchn);
        let m = &self.m;
        let chn = &mut self.ps.chn[nchn];
        let cmd = chn.row_command;
        let mut offset: SmpLength = xval;
        let mut high_offset: SmpLength = 0;
        if ext == 0 {
            let is_pct = cmd.volcmd == VOLCMD_OFFSET && cmd.vol == 0;
            offset <<= 8;
            if offset != 0 && (!m.behaviour(kFT2OffsetMemoryRequiresNote) || cmd.is_note()) {
                chn.old_offset = offset;
            } else if cmd.volcmd != VOLCMD_OFFSET {
                offset = chn.old_offset;
            }
            if !is_pct {
                high_offset = (chn.n_old_hi_offset as SmpLength) << 16;
            }
        }
        if cmd.volcmd == VOLCMD_OFFSET {
            if cmd.vol == 0 {
                let shift = 8 * ext.max(1);
                let den = if shift >= 24 { u32::MAX } else { 256u32 << shift };
                offset = muldivr_unsigned(chn.n_length, offset, den);
            } else if cmd.vol as usize <= 9 {
                if let Some(s) = chn.p_mod_sample {
                    let smp = &m.samples[s as usize];
                    if smp.u_flags & CHN_ADLIB == 0 {
                        offset = offset.wrapping_add(smp.cues[(cmd.vol - 1) as usize]);
                    }
                }
            }
            chn.old_offset = offset;
        }
        m.sample_offset(chn, offset.wrapping_add(high_offset));
    }

    /// `PositionJump`.
    fn position_jump(&mut self, chn: usize) {
        self.ps.next_pat_start_row = 0;
        self.ps.pos_jump = self.m.calculate_xparam(self.ps.pattern, self.ps.row, chn).0 as OrderIndex;
        if self.m.mod_type & (MOD_TYPE_MOD | MOD_TYPE_XM) != 0 && self.ps.break_row != ROWINDEX_INVALID {
            self.ps.break_row = 0;
        }
    }

    /// `PatternBreak`.
    fn pattern_break(&mut self, chn: usize, param: u8) -> RowIndex {
        if param >= 64 && self.m.mod_type & MOD_TYPE_S3M != 0 {
            return ROWINDEX_INVALID;
        }
        self.ps.next_pat_start_row = 0;
        self.m.calculate_xparam(self.ps.pattern, self.ps.row, chn).0
    }

    /// `HandleNextRow`.
    pub fn handle_next_row(&mut self) -> bool {
        let m = &self.m;
        let ps = &mut self.ps;
        let do_pattern_loop = ps.pat_loop_row != ROWINDEX_INVALID;
        let do_break_row = ps.break_row != ROWINDEX_INVALID;
        let do_pos_jump = ps.pos_jump != ORDERINDEX_INVALID;
        let mut break_to_row = false;
        if (do_break_row || do_pos_jump)
            && (!do_pattern_loop
                || m.behaviour(kFT2PatternLoopWithJumps)
                || (m.behaviour(kITPatternLoopWithJumps) && do_pos_jump)
                || (m.behaviour(kITPatternLoopWithJumpsOld) && do_pos_jump))
        {
            if !do_pos_jump {
                ps.pos_jump = ps.current_order.wrapping_add(1);
            }
            if !do_break_row {
                ps.break_row = 0;
            }
            break_to_row = true;
            if ps.pos_jump as usize >= m.order.len() {
                ps.pos_jump = m.restart_pos;
            }
            if ps.pos_jump != ps.current_order
                && !m.behaviour(kITPatternLoopBreak)
                && !m.behaviour(kFT2PatternLoopWithJumps)
                && m.mod_type != MOD_TYPE_MOD
            {
                for i in 0..m.num_channels() {
                    ps.chn[i].n_pattern_loop_count = 0;
                }
            }
            ps.next_row = ps.break_row;
            if !ps.flag(SONG_PATTERNLOOP) {
                ps.next_order = ps.pos_jump;
            }
        } else if do_pattern_loop {
            ps.next_order = ps.current_order;
            ps.next_row = ps.pat_loop_row;
            if ps.pattern_delay != 0
                && (m.mod_type != MOD_TYPE_IT || !m.behaviour(kITPatternLoopWithJumps))
                && m.mod_type != MOD_TYPE_S3M
            {
                ps.next_row += 1;
            }
            let rows = m.patterns.get(ps.pattern as usize).map_or(0, |p| p.rows);
            if ps.pat_loop_row >= rows {
                ps.next_order = ps.next_order.wrapping_add(1);
                ps.next_row = 0;
            }
        }
        break_to_row
    }

    /// `ResetAutoSlides`.
    fn reset_auto_slides(&mut self, nchn: usize) {
        let t = self.m.mod_type;
        let chn = &mut self.ps.chn[nchn];
        let cmd = chn.row_command.command;
        let volcmd = chn.row_command.volcmd;
        if cmd != CMD_NONE && t == MOD_TYPE_669 {
            chn.auto_slide.reset();
            return;
        }
        if (cmd == CMD_NONE || chn.row_command.param == 0) && chn.auto_slide.is_active(AutoSlide::VolumeSlideSTK) {
            chn.auto_slide.set_active(AutoSlide::VolumeSlideSTK, false);
        }
        if (cmd == CMD_CHANNELVOLUME || cmd == CMD_CHANNELVOLSLIDE) && chn.auto_slide.is_active(AutoSlide::VolumeDownWithDuration) {
            chn.auto_slide.set_active(AutoSlide::VolumeDownWithDuration, false);
        }
        let a = &chn.auto_slide;
        if (a.is_active(AutoSlide::FinePortamentoDown)
            || a.is_active(AutoSlide::PortamentoDown)
            || a.is_active(AutoSlide::FinePortamentoUp)
            || a.is_active(AutoSlide::PortamentoUp))
            && !chn.row_command.is_tone_portamento()
            && chn.row_command.is_any_pitch_slide()
        {
            for s in [AutoSlide::FinePortamentoDown, AutoSlide::PortamentoDown, AutoSlide::FinePortamentoUp, AutoSlide::PortamentoUp] {
                chn.auto_slide.set_active(s, false);
            }
        }
        let a = &chn.auto_slide;
        if (a.is_active(AutoSlide::FineVolumeSlideUp) || a.is_active(AutoSlide::FineVolumeSlideDown) || a.is_active(AutoSlide::VolumeDownETX))
            && (cmd == CMD_VOLUME
                || cmd == CMD_AUTO_VOLUMESLIDE
                || cmd == CMD_VOLUMEDOWN_ETX
                || chn.row_command.is_normal_volume_slide()
                || matches!(volcmd, VOLCMD_VOLUME | VOLCMD_VOLSLIDEUP | VOLCMD_VOLSLIDEDOWN | VOLCMD_FINEVOLUP | VOLCMD_FINEVOLDOWN))
        {
            for s in [AutoSlide::FineVolumeSlideUp, AutoSlide::FineVolumeSlideDown, AutoSlide::VolumeDownETX] {
                chn.auto_slide.set_active(s, false);
            }
        }
    }

    /// `ProcessAutoSlides`.
    fn process_auto_slides(&mut self, nchn: usize) {
        let tk = self.tk();
        let m = &self.m;
        let a = self.ps.chn[nchn].auto_slide;
        {
            let chn = &mut self.ps.chn[nchn];
            if a.is_active(AutoSlide::TonePortamento) && !chn.row_command.is_tone_portamento() {
                let s = chn.portamento_slide;
                m.tone_portamento(tk, chn, s);
            } else if a.is_active(AutoSlide::TonePortamentoWithDuration) {
                m.tone_portamento_with_duration(tk, chn, None);
            }
            if a.is_active(AutoSlide::PortamentoUp) {
                let p = chn.n_old_porta_up;
                m.portamento_up(tk, chn, p, true);
            } else if a.is_active(AutoSlide::PortamentoDown) {
                let p = chn.n_old_porta_down;
                m.portamento_down(tk, chn, p, true);
            } else if a.is_active(AutoSlide::FinePortamentoUp) {
                let p = chn.n_old_fine_porta_up_down;
                m.fine_portamento_up(chn, p);
            } else if a.is_active(AutoSlide::FinePortamentoDown) {
                let p = chn.n_old_fine_porta_up_down;
                m.fine_portamento_down(chn, p);
            }
            if a.is_active(AutoSlide::PortamentoFC) {
                m.portamento_fc(chn);
            }
            if a.is_active(AutoSlide::FineVolumeSlideUp) && chn.row_command.command != CMD_AUTO_VOLUMESLIDE {
                m.fine_volume_up(chn, 0, false);
            }
            if a.is_active(AutoSlide::FineVolumeSlideDown) && chn.row_command.command != CMD_AUTO_VOLUMESLIDE {
                m.fine_volume_down(chn, 0, false);
            }
            if a.is_active(AutoSlide::VolumeDownETX) {
                chn.n_volume = (chn.n_volume - chn.n_old_volume_slide as i32).max(0);
            }
            if a.is_active(AutoSlide::VolumeSlideSTK) {
                m.volume_slide(tk, chn, 0, false);
            }
        }
        if a.is_active(AutoSlide::GlobalVolumeSlide) && self.ps.chn[nchn].row_command.command != CMD_GLOBALVOLSLIDE {
            let p = self.ps.chn[nchn].n_old_global_vol_slide;
            self.global_vol_slide(p, nchn);
        }
        let m = &self.m;
        let chn = &mut self.ps.chn[nchn];
        if a.is_active(AutoSlide::VolumeDownWithDuration) {
            m.channel_volume_down_with_duration(tk, chn, None);
        }
        if a.is_active(AutoSlide::Vibrato) {
            chn.set(CHN_VIBRATO);
        }
        if a.is_active(AutoSlide::Tremolo) {
            chn.set(CHN_TREMOLO);
        }
    }

    /// `UpdateS3MEffectMemory`.
    fn update_s3m_effect_memory(chn: &mut ModChannel, param: u8) {
        chn.n_old_volume_slide = param;
        chn.n_old_porta_up = param;
        chn.n_old_porta_down = param;
        chn.n_tremor_param = param;
        chn.n_arpeggio = param;
        chn.n_retrig_param = param;
        chn.n_tremolo_depth = (param & 0x0F) << 2;
        chn.n_tremolo_speed = (param >> 4) & 0x0F;
        chn.n_old_cmd_ex = param;
    }

    /// `SetFinetune`.
    fn set_finetune(&mut self, nchn: usize, is_smooth: bool) {
        let (v, _) = self.m.calculate_xparam(self.ps.pattern, self.ps.row, nchn);
        let mut new_tuning = (v as i32 - 0x8000).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let ticks_left = self.ps.ticks_on_row() as i32 - self.ps.tick_count as i32;
        let chn = &mut self.ps.chn[nchn];
        if is_smooth && ticks_left > 1 {
            let step = (new_tuning as i32 - chn.micro_tuning as i32) / ticks_left;
            new_tuning = (chn.micro_tuning as i32 + step).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
        chn.micro_tuning = new_tuning;
    }

    /// `InvertLoop` (MOD EFx): trashes the sample loop, as ProTracker does.
    fn invert_loop(&mut self, nchn: usize) {
        if self.m.mod_type != MOD_TYPE_MOD || self.ps.chn[nchn].n_efx_speed == 0 {
            return;
        }
        let Some(si) = self.ps.chn[nchn].p_mod_sample else {
            return;
        };
        let smp = &self.m.samples[si as usize];
        if !smp.has_sample_data() || smp.u_flags & (CHN_LOOP | CHN_SUSTAINLOOP) == 0 {
            return;
        }
        let chn = &mut self.ps.chn[nchn];
        chn.n_efx_delay = chn.n_efx_delay.wrapping_add(MOD_EFX_TABLE[(chn.n_efx_speed & 0x0F) as usize]);
        if chn.n_efx_delay < 128 {
            return;
        }
        chn.n_efx_delay = 0;
        let (ls, le) = if smp.u_flags & CHN_LOOP != 0 { (smp.n_loop_start, smp.n_loop_end) } else { (smp.n_sustain_start, smp.n_sustain_end) };
        chn.n_efx_offset += 1;
        if chn.n_efx_offset >= le.wrapping_sub(ls) {
            chn.n_efx_offset = 0;
        }
        let frame = (ls + chn.n_efx_offset) as usize;
        let it_pp = self.m.behaviour(kITPingPongMode);
        let smp = &mut self.m.samples[si as usize];
        let nch = smp.num_channels() as usize;
        // Invert every byte of one sampling point.
        let base = (crate::sample::PRE_FRAMES + frame) * nch;
        if let Some(d) = smp.data.i8_mut() {
            for c in 0..nch {
                if let Some(x) = d.get_mut(base + c) {
                    *x = !*x;
                }
            }
        } else if let Some(d) = smp.data.i16_mut() {
            for c in 0..nch {
                if let Some(x) = d.get_mut(base + c) {
                    *x = !*x;
                }
            }
        }
        smp.precompute_loops(it_pp);
    }

    /// `ExtendedMODCommands`.
    fn extended_mod_commands(&mut self, nchn: usize, param: u8) {
        let tk = self.tk();
        let command = param & 0xF0;
        let param = param & 0x0F;
        let t = self.m.mod_type;
        match command {
            0x00 => {
                for c in 0..self.num_channels() {
                    self.ps.chn[c].set_to(CHN_AMIGAFILTER, param & 1 == 0);
                }
            }
            0x10 => {
                if param != 0 || t & (MOD_TYPE_XM | MOD_TYPE_MT2) != 0 {
                    self.m.fine_portamento_up(&mut self.ps.chn[nchn], param);
                }
            }
            0x20 => {
                if param != 0 || t & (MOD_TYPE_XM | MOD_TYPE_MT2) != 0 {
                    self.m.fine_portamento_down(&mut self.ps.chn[nchn], param);
                }
            }
            0x30 => self.ps.chn[nchn].set_to(CHN_GLISSANDO, param != 0),
            0x40 => self.ps.chn[nchn].n_vibrato_type = param & 0x07,
            0x50 => {
                if !tk.first_tick() {
                    return;
                }
                let m = &self.m;
                let chn = &mut self.ps.chn[nchn];
                if t & (MOD_TYPE_MOD | MOD_TYPE_DIGI | MOD_TYPE_AMF0 | MOD_TYPE_MED) != 0 {
                    chn.n_fine_tune = mod2xm_finetune(param as i32);
                    if chn.n_period != 0 && chn.row_command.is_note() {
                        chn.n_period = m.period_from_note(chn.n_note as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32) as i32;
                    }
                } else if t == MOD_TYPE_MTM {
                    if chn.row_command.is_note() {
                        if let Some(s) = chn.p_mod_sample {
                            chn.n_fine_tune = param as i16;
                            self.m.samples[s as usize].n_fine_tune = param as i8;
                            let m = &self.m;
                            let chn = &mut self.ps.chn[nchn];
                            if chn.n_period != 0 {
                                chn.n_period = m.period_from_note(chn.n_note as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32) as i32;
                            }
                        }
                    }
                } else if chn.row_command.is_note() {
                    chn.n_fine_tune = mod2xm_finetune(param as i32 - 8);
                    if chn.n_period != 0 {
                        chn.n_period = m.period_from_note(chn.n_note as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32) as i32;
                    }
                }
            }
            0x60 => {
                if tk.first_tick() {
                    self.pattern_loop(nchn, param & 0x0F);
                }
            }
            0x70 => self.ps.chn[nchn].n_tremolo_type = param & 0x07,
            0x80 => {
                if tk.first_tick() {
                    self.m.panning(tk.flags, &mut self.ps.chn[nchn], param as u32, PanBits::Pan4);
                }
            }
            0x90 => self.retrig_note(nchn, param as i32, 0),
            0xA0 => {
                if param != 0 || t & (MOD_TYPE_XM | MOD_TYPE_MT2) != 0 {
                    self.m.fine_volume_up(&mut self.ps.chn[nchn], param, false);
                }
            }
            0xB0 => {
                if param != 0 || t & (MOD_TYPE_XM | MOD_TYPE_MT2) != 0 {
                    self.m.fine_volume_down(&mut self.ps.chn[nchn], param, false);
                }
            }
            0xC0 => self.note_cut(nchn, param as u32, false),
            0xF0 => {
                if t == MOD_TYPE_MOD {
                    self.ps.chn[nchn].n_efx_speed = param;
                    if tk.first_tick() {
                        self.invert_loop(nchn);
                    }
                } else {
                    self.ps.chn[nchn].n_active_macro = param;
                }
            }
            _ => {}
        }
    }

    /// `ExtendedS3MCommands`.
    fn extended_s3m_commands(&mut self, nchn: usize, param: u8) {
        let tk = self.tk();
        let command = param & 0xF0;
        let mut param = param & 0x0F;
        let t = self.m.mod_type;
        match command {
            0x10 => self.ps.chn[nchn].set_to(CHN_GLISSANDO, param != 0),
            0x20 => {
                if !tk.first_tick() {
                    return;
                }
                let m = &self.m;
                let chn = &mut self.ps.chn[nchn];
                if t != MOD_TYPE_669 {
                    chn.n_c5_speed = S3_MFINE_TUNE_TABLE[param as usize] as i32;
                    chn.n_fine_tune = mod2xm_finetune(param as i32);
                    if chn.n_period != 0 {
                        chn.n_period = m.period_from_note(chn.n_note as u32, chn.n_fine_tune as i32, chn.n_c5_speed as u32) as i32;
                    }
                } else if let Some(s) = chn.p_mod_sample {
                    chn.n_c5_speed = m.samples[s as usize].n_c5_speed as i32 + param as i32 * 80;
                }
            }
            0x30 => {
                let chn = &mut self.ps.chn[nchn];
                if t == MOD_TYPE_S3M {
                    chn.n_vibrato_type = param & 0x03;
                } else if self.m.behaviour(kITVibratoTremoloPanbrello) {
                    chn.n_vibrato_type = if param < 0x04 { param } else { 0 };
                } else {
                    chn.n_vibrato_type = param & 0x07;
                }
            }
            0x40 => {
                let chn = &mut self.ps.chn[nchn];
                if t == MOD_TYPE_S3M {
                    chn.n_tremolo_type = param & 0x03;
                } else if self.m.behaviour(kITVibratoTremoloPanbrello) {
                    chn.n_tremolo_type = if param < 0x04 { param } else { 0 };
                } else {
                    chn.n_tremolo_type = param & 0x07;
                }
            }
            0x50 => {
                let chn = &mut self.ps.chn[nchn];
                if self.m.behaviour(kITVibratoTremoloPanbrello) {
                    chn.n_panbrello_type = if param < 0x04 { param } else { 0 };
                    chn.n_panbrello_pos = 0;
                } else {
                    chn.n_panbrello_type = param & 0x07;
                }
            }
            0x60 => {
                if tk.first_tick() && tk.tick_count == 0 {
                    self.ps.frame_delay += param as u32;
                }
            }
            0x70 => {
                if !tk.first_tick() {
                    return;
                }
                match param {
                    0..=2 => {
                        let nc = self.num_channels();
                        for i in nc..self.ps.chn.len() {
                            if self.ps.chn[i].n_master_chn as usize == nchn + 1 {
                                let m = &self.m;
                                let bk = &mut self.ps.chn[i];
                                if param == 1 {
                                    m.key_off(bk);
                                } else if param == 2 {
                                    bk.set(CHN_NOTEFADE);
                                } else {
                                    bk.set(CHN_NOTEFADE);
                                    bk.n_fade_out_vol = 0;
                                }
                            }
                        }
                    }
                    _ => self.ps.chn[nchn].instrument_control(param, t),
                }
            }
            0x80 => {
                if tk.first_tick() {
                    self.m.panning(tk.flags, &mut self.ps.chn[nchn], param as u32, PanBits::Pan4);
                }
            }
            0x90 => {
                if tk.first_tick() {
                    let mut flags = self.ps.flags;
                    self.m.extended_channel_effect(&mut self.ps.chn[nchn], param as u32, &mut flags);
                    self.ps.flags = flags;
                }
            }
            0xA0 => {
                if tk.first_tick() {
                    let it_high = self.m.behaviour(kITHighOffsetNoRetrig);
                    let chn = &mut self.ps.chn[nchn];
                    chn.n_old_hi_offset = param;
                    if !it_high && chn.row_command.is_note() {
                        let pos = (param as u32) << 16;
                        if pos < chn.n_length {
                            chn.position.set_int(pos as i32);
                        }
                    }
                }
            }
            0xB0 => {
                if tk.first_tick() {
                    self.pattern_loop(nchn, param & 0x0F);
                }
            }
            0xC0 => {
                if param == 0 {
                    if t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 {
                        param = 1;
                    } else if t == MOD_TYPE_S3M {
                        return;
                    }
                }
                let cut = self.m.behaviour(kITSCxStopsSample) || t == MOD_TYPE_S3M;
                self.note_cut(nchn, param as u32, cut);
            }
            0xF0 => {
                if t != MOD_TYPE_S3M {
                    self.ps.chn[nchn].n_active_macro = param;
                }
            }
            _ => {}
        }
    }

    /// `RetrigNote`.
    pub fn retrig_note(&mut self, nchn: usize, param: i32, mut offset: i32) {
        let tk = self.tk();
        let t = self.m.mod_type;
        let mut retrig_speed = param & 0x0F;
        let mut retrig_count = self.ps.chn[nchn].n_retrig_count;
        let mut do_retrig = false;
        {
            let m = &self.m;
            let chn = &mut self.ps.chn[nchn];
            if m.behaviour(kITRetrigger) {
                if tk.tick_count == 0 && chn.row_command.note != 0 {
                    chn.n_retrig_count = (param & 0x0F) as u8;
                } else {
                    let reached = if chn.n_retrig_count == 0 {
                        true
                    } else {
                        chn.n_retrig_count -= 1;
                        chn.n_retrig_count == 0
                    };
                    if reached {
                        chn.n_retrig_count = (param & 0x0F) as u8;
                        do_retrig = true;
                    }
                }
            } else if m.behaviour(kFT2Retrigger) && (param & 0x100) != 0 {
                if tk.first_tick() {
                    if chn.row_command.instr > 0 && chn.row_command.is_note_or_empty() {
                        retrig_count = 1;
                    }
                    if chn.row_command.volcmd == VOLCMD_VOLUME && chn.row_command.vol != 0 {
                        chn.n_retrig_count = retrig_count;
                        return;
                    }
                }
                if retrig_count as i32 >= retrig_speed && (!tk.first_tick() || !chn.row_command.is_note()) {
                    do_retrig = true;
                    retrig_count = 0;
                }
            } else if t & (MOD_TYPE_S3M | MOD_TYPE_IT | MOD_TYPE_MPT) != 0 {
                if retrig_speed == 0 {
                    retrig_speed = 1;
                }
                if retrig_count != 0 && (retrig_count as i32 % retrig_speed) == 0 {
                    do_retrig = true;
                }
                retrig_count = retrig_count.wrapping_add(1);
            } else if t == MOD_TYPE_MOD {
                let tick = if tk.music_speed != 0 { tk.tick_count % tk.music_speed } else { 0 };
                if tick == 0 && chn.row_command.is_note() {
                    return;
                }
                if retrig_speed != 0 && (tick % retrig_speed as u32) == 0 {
                    do_retrig = true;
                }
            } else if t == MOD_TYPE_MTM {
                do_retrig = tk.tick_count == (param & 0x0F) as u32 && retrig_speed != 0;
            } else {
                let mut realspeed = retrig_speed;
                if (param & 0x100) != 0 && chn.row_command.volcmd == VOLCMD_VOLUME && (chn.row_command.param & 0xF0) != 0 {
                    realspeed += 1;
                }
                if !tk.first_tick() || (param & 0x100) != 0 {
                    if realspeed == 0 {
                        realspeed = 1;
                    }
                    if (param & 0x100) == 0 && tk.music_speed != 0 && (tk.tick_count % realspeed as u32) == 0 {
                        do_retrig = true;
                    }
                    retrig_count = retrig_count.wrapping_add(1);
                } else if t & (MOD_TYPE_XM | MOD_TYPE_MT2) != 0 {
                    retrig_count = 0;
                }
                if retrig_count as i32 >= realspeed && (tk.tick_count != 0 || ((param & 0x100) != 0 && chn.row_command.note == 0)) {
                    do_retrig = true;
                }
                if m.behaviour(kFT2Retrigger) && param == 0 {
                    do_retrig = tk.tick_count == 0;
                }
            }
            if chn.n_length == 0 && m.behaviour(kITShortSampleRetrig) {
                return;
            }
            if m.behaviour(kST3RetrigAfterNoteCut) && chn.n_fade_out_vol == 0 {
                return;
            }
        }
        if do_retrig {
            let dv = ((param >> 4) & 0x0F) as usize;
            let mut vol = self.ps.chn[nchn].n_volume;
            if dv != 0 {
                let chn = &mut self.ps.chn[nchn];
                if !self.m.behaviour(kFT2Retrigger) || chn.row_command.volcmd != VOLCMD_VOLUME {
                    if RETRIG_TABLE1[dv] != 0 {
                        vol = (vol * RETRIG_TABLE1[dv] as i32) / 16;
                    } else {
                        vol += RETRIG_TABLE2[dv] as i32 * 4;
                    }
                }
                vol = vol.clamp(0, 256);
                chn.set(CHN_FASTVOLRAMP);
            }
            let note = self.ps.chn[nchn].n_new_note as u32;
            let old_period = self.ps.chn[nchn].n_period;
            if note >= NOTE_MIN as u32 && note <= NOTE_MAX as u32 && self.ps.chn[nchn].n_length != 0 && t != MOD_TYPE_S3M {
                self.check_nna(nchn, 0, note as i32, true);
            }
            let mut reset_env = false;
            if t & (MOD_TYPE_XM | MOD_TYPE_MT2) != 0 {
                let instr = self.ps.chn[nchn].row_command.instr;
                if instr != 0 && param < 0x100 {
                    self.instrument_change(nchn, instr as u32, false, false, true);
                    reset_env = true;
                }
                if param < 0x100 {
                    reset_env = true;
                }
            }
            if self.m.behaviour(kMODSampleSwap) && self.ps.chn[nchn].row_command.instr != 0 {
                let old_ft = self.ps.chn[nchn].n_fine_tune;
                let instr = self.ps.chn[nchn].row_command.instr;
                self.instrument_change(nchn, instr as u32, false, false, true);
                self.ps.chn[nchn].n_fine_tune = old_ft;
            }
            let fading = self.ps.chn[nchn].has(CHN_NOTEFADE);
            let old_prev = self.ps.chn[nchn].prev_note_offset;
            if t == MOD_TYPE_S3M {
                self.ps.chn[nchn].prev_note_offset = 0;
            }
            let it_s3m_style = self.m.behaviour(kITRetrigger) || (t == MOD_TYPE_S3M && self.ps.chn[nchn].n_length != 0);
            let flags = self.ps.flags;
            self.m.note_change(flags, &mut self.prng, &mut self.ps.chn[nchn], note as i32, it_s3m_style, reset_env, false);
            let m = &self.m;
            let num_instruments = m.num_instruments;
            let chn = &mut self.ps.chn[nchn];
            if chn.row_command.instr == 0 {
                chn.prev_note_offset = old_prev;
            }
            if fading && t == MOD_TYPE_XM {
                chn.set(CHN_NOTEFADE);
            }
            chn.n_volume = vol;
            if num_instruments != 0 {
                chn.row_command.note = note as u8;
            }
            if t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 && chn.row_command.note == NOTE_NONE && old_period != 0 {
                chn.n_period = old_period;
            }
            if t & (MOD_TYPE_S3M | MOD_TYPE_IT | MOD_TYPE_MPT) == 0 {
                retrig_count = 0;
            }
            if it_s3m_style {
                chn.position = SamplePosition(0);
            }
            offset -= 1;
            if let Some(s) = chn.p_mod_sample {
                let smp = &m.samples[s as usize];
                if smp.u_flags & CHN_ADLIB == 0 && offset >= 0 && offset <= 9 {
                    let off = if offset == 0 {
                        chn.old_offset
                    } else {
                        chn.old_offset = smp.cues[(offset - 1) as usize];
                        chn.old_offset
                    };
                    m.sample_offset(chn, off);
                }
            }
        }
        if self.m.behaviour(kFT2Retrigger) && (param & 0x100) != 0 {
            retrig_count = retrig_count.wrapping_add(1);
        }
        if !self.m.behaviour(kITRetrigger) {
            self.ps.chn[nchn].n_retrig_count = retrig_count;
        }
    }

    /// `ProcessMIDIMacro` for one channel (internal device only).
    pub fn process_midi_macro(&mut self, nchn: usize, is_smooth: bool, mac: &crate::midimacro::Macro, param: u8) {
        use crate::midimacro::*;
        let m = &self.m;
        let ps = &mut self.ps;
        let ticks_on_row = ps.ticks_on_row();
        let tick_count = ps.tick_count;
        let global_volume = ps.global_volume;
        let nc = m.num_channels();
        let chn = &ps.chn[nchn];
        let swing = if m.behaviour(kITSwingBehaviour) || m.behaviour(kMPTOldSwingBehaviour) { chn.n_vol_swing as i32 } else { 0 };
        let vel = muldiv((chn.n_volume + swing) * global_volume, chn.n_global_vol as i32 * chn.n_ins_vol as i32, 1 << 20);
        let calc = muldiv(chn.n_calc_volume * global_volume, chn.n_global_vol as i32 * chn.n_ins_vol as i32, 1 << 26);
        let vars = MacroVars {
            midi_channel: 0,
            last_note: if ModCommand::is_note_of(chn.n_last_note) { chn.n_last_note - NOTE_MIN } else { 0 },
            velocity: (vel / 2).clamp(1, 127) as u8,
            calc_volume: (calc / 2).clamp(1, 127) as u8,
            pan: (chn.n_pan / 2).min(127) as u8,
            real_pan: (chn.n_real_pan / 2).min(127) as u8,
            offset: ((chn.old_offset >> 8) & 0xFF) as u8,
            host_channel: ((if nchn >= nc { chn.n_master_chn as usize - 1 } else { nchn }) & 0x7F) as u8,
            loop_dir: if chn.has(CHN_PINGPONGFLAG) { 1 } else { 0 },
            bank_hi: 0,
            bank_lo: 0,
            program: 0,
        };
        let mut last_zxx = ps.chn[nchn].last_zxx_param;
        let start_z = last_zxx;
        let mut smooth = |target: u8| -> u8 { smooth_change(ticks_on_row, tick_count, start_z as f32, target as f32) as u8 };
        let bytes = parse_macro(mac, &vars, param, if is_smooth { Some(&mut smooth) } else { None }, &mut last_zxx);
        ps.chn[nchn].last_zxx_param = last_zxx;
        for msg in split_messages(&bytes) {
            self.send_midi_data(nchn, is_smooth, &msg);
        }
    }

    /// `SendMIDIData` (internal device only).
    fn send_midi_data(&mut self, nchn: usize, is_smooth: bool, msg: &[u8]) {
        if msg.is_empty() {
            return;
        }
        let freq = self.settings.mixing_freq;
        if msg[0] == 0xFA || msg[0] == 0xFC || msg[0] == 0xFF {
            for c in 0..self.num_channels() {
                self.ps.chn[c].n_cut_off = 0x7F;
                self.ps.chn[c].n_resonance = 0;
            }
        }
        if msg.len() == 4 && msg[0] == 0xF0 && (msg[1] == 0xF0 || msg[1] == 0xF1) {
            let extended = msg[1] == 0xF1;
            let code = msg[2];
            let param = msg[3];
            let ticks_on_row = self.ps.ticks_on_row();
            let tick_count = self.ps.tick_count;
            let m = &self.m;
            let chn = &mut self.ps.chn[nchn];
            if code == 0x00 && !extended && param < 0x80 {
                if !is_smooth {
                    chn.n_cut_off = param;
                } else {
                    let v = smooth_change(ticks_on_row, tick_count, chn.n_cut_off as f32, param as f32);
                    chn.n_cut_off = v.round().clamp(0.0, 255.0) as u8;
                }
                chn.n_restore_cutoff_on_new_note = 0;
                let reset = !chn.has(CHN_FILTER);
                m.setup_channel_filter(freq, chn, reset, 256);
            } else if code == 0x01 && !extended && param < 0x80 {
                if !is_smooth {
                    chn.n_resonance = param;
                } else {
                    let v = smooth_change(ticks_on_row, tick_count, chn.n_resonance as f32, param as f32);
                    chn.n_resonance = v.round().clamp(0.0, 255.0) as u8;
                }
                chn.n_restore_resonance_on_new_note = 0;
                let reset = !chn.has(CHN_FILTER);
                m.setup_channel_filter(freq, chn, reset, 256);
            } else if code == 0x02 && !extended && param < 0x20 {
                chn.n_filter_mode = if param >> 4 == 0 { FilterMode::LowPass } else { FilterMode::HighPass };
                let reset = !chn.has(CHN_FILTER);
                m.setup_channel_filter(freq, chn, reset, 256);
            }
        }
    }

    /// `ProcessEffects`.
    pub fn process_effects(&mut self) -> bool {
        self.ps.break_row = ROWINDEX_INVALID;
        self.ps.pat_loop_row = ROWINDEX_INVALID;
        self.ps.pos_jump = ORDERINDEX_INVALID;
        let t = self.m.mod_type;
        let nc = self.num_channels();
        for nchn in 0..nc {
            let tk = self.tk();
            let speed_plus = self.ps.music_speed.wrapping_add(self.ps.frame_delay);
            let tick_count = if speed_plus != 0 { self.ps.tick_count % speed_plus } else { 0 };
            let rc = self.ps.chn[nchn].row_command;
            let mut instr = rc.instr as u32;
            let mut volcmd = rc.volcmd;
            let mut vol = rc.vol as u32;
            let mut cmd = rc.command;
            let mut param = rc.param as u32;
            let mut b_porta = rc.is_tone_portamento();
            let mut start_tick: u32 = 0;
            self.ps.chn[nchn].is_first_tick = tk.first_tick();

            if ModCommand::is_pc_note_of(rc.note) {
                self.ps.chn[nchn].row_command.clear();
                instr = 0;
                volcmd = VOLCMD_NONE;
                vol = 0;
                cmd = CMD_NONE;
                param = 0;
                b_porta = false;
            }

            if self.m.behaviour(kITEmptyNoteMapSlotIgnoreCell) && instr > 0 {
                if let Some(ins) = self.m.instrument(instr) {
                    if !ins.has_valid_midi_channel() {
                        let chn = &self.ps.chn[nchn];
                        let note = if chn.row_command.note != NOTE_NONE { chn.row_command.note } else { chn.n_new_note };
                        if ModCommand::is_note_of(note) && ins.keyboard[(note - NOTE_MIN) as usize] == 0 {
                            let chn = &mut self.ps.chn[nchn];
                            chn.n_new_note = note;
                            chn.n_last_note = note;
                            chn.n_new_ins = instr as u8;
                            chn.row_command.clear();
                            continue;
                        }
                    }
                }
            }

            let continue_note = !b_porta
                && self.m.behaviour(kContinueSampleWithoutInstr)
                && self.ps.chn[nchn].row_command.instr == 0
                && self.ps.chn[nchn].has(CHN_LOOP)
                && self.ps.chn[nchn].p_current_sample.is_some();
            if continue_note {
                b_porta = true;
            }

            if !tk.first_tick() {
                self.invert_loop(nchn);
            } else if instr != 0 {
                self.ps.chn[nchn].n_efx_offset = 0;
            }

            if cmd == CMD_DELAYCUT {
                start_tick = (param & 0xF0) >> 4;
                let cut_at = start_tick + (param & 0x0F);
                let cut = self.m.behaviour(kITSCxStopsSample);
                self.note_cut(nchn, cut_at, cut);
            } else if cmd == CMD_MODCMDEX || cmd == CMD_S3MCMDEX {
                if param == 0 && t & (MOD_TYPE_S3M | MOD_TYPE_IT | MOD_TYPE_MPT) != 0 {
                    param = self.ps.chn[nchn].n_old_cmd_ex as u32;
                } else {
                    self.ps.chn[nchn].n_old_cmd_ex = param as u8;
                }
                if (param & 0xF0) == 0xD0 {
                    start_tick = param & 0x0F;
                    if start_tick == 0 {
                        if t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 {
                            start_tick = 1;
                        } else if t == MOD_TYPE_S3M {
                            continue;
                        }
                    } else if start_tick >= speed_plus && self.m.behaviour(kITOutOfRangeDelay) {
                        if instr != 0 {
                            self.ps.chn[nchn].n_new_ins = instr as u8;
                        }
                        continue;
                    }
                } else if tk.first_tick() && (param & 0xF0) == 0xE0 {
                    if t & (MOD_TYPE_S3M | MOD_TYPE_IT | MOD_TYPE_MPT) == 0 || self.ps.pattern_delay == 0 {
                        if t & MOD_TYPE_S3M == 0 || (param & 0x0F) != 0 {
                            self.ps.pattern_delay = 1 + (param & 0x0F);
                        }
                    }
                }
            }
            if t == MOD_TYPE_MTM && cmd == CMD_MODCMDEX && (param & 0xF0) == 0xD0 {
                start_tick = 0;
                param = 0x90 | (param & 0x0F);
            }
            if start_tick != 0
                && self.ps.chn[nchn].row_command.note == NOTE_KEYOFF
                && self.ps.chn[nchn].row_command.volcmd == VOLCMD_PANNING
                && self.m.behaviour(kFT2PanWithDelayedNoteOff)
            {
                self.ps.chn[nchn].row_command.volcmd = VOLCMD_NONE;
            }

            let mut trigger_note = self.ps.tick_count == start_tick;
            if self.m.behaviour(kFT2OutOfRangeDelay) && start_tick >= self.ps.music_speed {
                trigger_note = false;
            } else if self.m.behaviour(kRowDelayWithNoteDelay) && start_tick > 0 && tick_count == start_tick {
                trigger_note = true;
            }
            if self.m.behaviour(kITFirstTickHandling) {
                self.ps.chn[nchn].is_first_tick = tick_count == start_tick;
            }
            self.ps.chn[nchn].trigger_note = false;
            if self.m.behaviour(kFT2PortaDelay) && start_tick != 0 {
                b_porta = false;
            }

            if self.m.song_flag(SONG_PT_MODE) && instr != 0 && self.ps.tick_count == 0 {
                let swap = self.m.sample_index(self.ps.chn[nchn].n_last_note, instr);
                let m = &self.m;
                let chn = &mut self.ps.chn[nchn];
                chn.prev_note_offset = 0;
                if !trigger_note && chn.is_sample_playing() {
                    chn.n_new_ins = instr as u8;
                    chn.swap_sample_index = swap;
                    if instr <= m.num_samples as u32 {
                        chn.n_volume = m.samples[instr as usize].n_volume as i32;
                        chn.n_fine_tune = m.samples[instr as usize].n_fine_tune as i16;
                    }
                }
            }

            if trigger_note {
                let mut note = self.ps.chn[nchn].row_command.note;
                if instr != 0 {
                    let n = if ModCommand::is_note_of(note) { note } else { self.ps.chn[nchn].n_last_note };
                    let swap = self.m.sample_index(n, instr);
                    let chn = &mut self.ps.chn[nchn];
                    chn.n_new_ins = instr as u8;
                    chn.swap_sample_index = swap;
                }
                if ModCommand::is_note_of(note) && self.m.behaviour(kFT2Transpose) {
                    let mut transpose = self.ps.chn[nchn].n_transpose as i32;
                    if instr != 0 && !b_porta {
                        let sample = self.m.sample_index(note, instr);
                        if sample > 0 {
                            transpose = self.m.samples[sample as usize].relative_tone as i32;
                        }
                    }
                    let computed = note as i32 + transpose;
                    if computed < NOTE_MIN as i32 + 11 || computed > NOTE_MIN as i32 + 130 {
                        note = NOTE_NONE;
                    }
                } else if t & (MOD_TYPE_IT | MOD_TYPE_MPT | MOD_TYPE_J2B) != 0 && self.m.num_instruments != 0 && (note == NOTE_NONE || ModCommand::is_note_of(note)) {
                    let to_check = if instr != 0 { instr } else { self.ps.chn[nchn].n_old_ins as u32 };
                    if to_check != 0 && self.m.instrument(to_check).is_none() {
                        note = NOTE_NONE;
                        instr = 0;
                    }
                }
                if cmd == CMD_KEYOFF && param == 0 && self.m.behaviour(kFT2KeyOff) {
                    note = NOTE_NONE;
                    instr = 0;
                }
                let mut retrig_env = note == NOTE_NONE && instr != 0;
                let mut reload_sample_settings = self.m.behaviour(kFT2ReloadSampleSettings) && instr != 0;
                let mut keep_instr = t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 || self.m.behaviour(kST3SampleSwap);
                if self.m.behaviour(kMODSampleSwap)
                    && !self.ps.chn[nchn].is_sample_playing()
                    && instr <= self.m.num_samples as u32
                    && self.m.samples[instr as usize].u_flags & CHN_LOOP != 0
                {
                    keep_instr = true;
                }
                if t & (MOD_TYPE_XM | MOD_TYPE_MT2) != 0 {
                    let has_vol_env = self.ps.chn[nchn]
                        .p_mod_instrument
                        .and_then(|i| self.m.instrument(i as u32))
                        .is_some_and(|i| i.vol_env.has(ENV_ENABLED));
                    if note == NOTE_KEYOFF
                        && ((instr == 0 && volcmd != VOLCMD_VOLUME && cmd != CMD_VOLUME) || !self.m.behaviour(kFT2KeyOff))
                        && !has_vol_env
                    {
                        let chn = &mut self.ps.chn[nchn];
                        chn.set(CHN_FASTVOLRAMP);
                        chn.n_volume = 0;
                        note = NOTE_NONE;
                        instr = 0;
                        retrig_env = false;
                        if tk.first_tick() && self.m.behaviour(kFT2NoteOffFlags) {
                            chn.set(CHN_NOTEFADE);
                        }
                    } else if self.m.behaviour(kFT2RetrigWithNoteDelay) && !tk.first_tick() {
                        retrig_env = true;
                        b_porta = false;
                        if note == NOTE_NONE {
                            let chn = &self.ps.chn[nchn];
                            note = (chn.n_note as i32 - chn.n_transpose as i32) as u8;
                        } else if note >= NOTE_MIN_SPECIAL {
                            note = NOTE_NONE;
                            keep_instr = false;
                            reload_sample_settings = true;
                        } else if instr != 0 || !self.m.behaviour(kFT2NoteDelayWithoutInstr) {
                            keep_instr = true;
                            reload_sample_settings = true;
                        }
                    }
                }
                if (retrig_env && !self.m.behaviour(kFT2ReloadSampleSettings)) || reload_sample_settings {
                    let old_sample = if self.m.num_instruments != 0 {
                        self.ps.chn[nchn].p_mod_sample
                    } else if instr <= self.m.num_samples as u32 {
                        Some(instr as SampleIndex)
                    } else {
                        None
                    };
                    if let Some(os) = old_sample {
                        let m = &self.m;
                        let smp = &m.samples[os as usize];
                        let chn = &mut self.ps.chn[nchn];
                        if smp.u_flags & SMP_NODEFAULTVOLUME == 0 && (t != MOD_TYPE_S3M || smp.has_sample_data()) {
                            chn.n_volume = smp.n_volume as i32;
                            chn.set(CHN_FASTVOLRAMP);
                        }
                        if reload_sample_settings {
                            chn.set_instrument_pan(smp.n_pan as i32, m);
                        }
                    }
                }
                if self.m.behaviour(kFT2Tremor) && instr != 0 {
                    self.ps.chn[nchn].n_tremor_count = 0x20;
                }
                if self.m.num_instruments != 0
                    && self.m.behaviour(kITInstrWithNoteOffOldEffects)
                    && instr != 0
                    && !ModCommand::is_note_of(note)
                    && ((b_porta && self.m.song_flag(SONG_ITCOMPATGXX)) || (!b_porta && self.m.song_flag(SONG_ITOLDEFFECTS)))
                {
                    let chn = &mut self.ps.chn[nchn];
                    chn.reset_envelopes();
                    chn.set(CHN_FASTVOLRAMP);
                    chn.n_fade_out_vol = 65536;
                }
                if retrig_env {
                    if self.m.behaviour(kITInstrWithoutNote) || t == MOD_TYPE_PLM {
                        let chn = &self.ps.chn[nchn];
                        let trigger_after = self.m.behaviour(kITMultiSampleInstrumentNumber) && !chn.is_sample_playing();
                        if self.m.num_instruments != 0 {
                            if instr <= self.m.num_instruments as u32
                                && (chn.p_mod_instrument != self.m.instrument_index(instr) || trigger_after)
                            {
                                note = chn.n_note;
                            }
                        } else if (instr as usize) < MAX_SAMPLES && (chn.p_mod_sample != Some(instr as SampleIndex) || trigger_after) {
                            note = chn.n_note;
                        }
                    }
                    if self.m.num_instruments != 0 && t & (MOD_TYPE_XM | MOD_TYPE_MT2 | MOD_TYPE_MED) != 0 {
                        let ft2_flags = self.m.behaviour(kFT2NoteOffFlags);
                        let chn = &mut self.ps.chn[nchn];
                        chn.reset_envelopes();
                        chn.set(CHN_FASTVOLRAMP);
                        chn.reset_flag(CHN_NOTEFADE);
                        chn.n_auto_vib_depth = 0;
                        chn.n_auto_vib_pos = 0;
                        chn.n_fade_out_vol = 65536;
                        if ft2_flags {
                            chn.reset_flag(CHN_KEYOFF);
                        }
                    }
                    if !keep_instr {
                        instr = 0;
                    }
                }
                if note >= NOTE_MIN_SPECIAL {
                    if self.m.behaviour(kITInstrWithNoteOff) && instr != 0 {
                        let smp = self.m.sample_index(self.ps.chn[nchn].n_last_note, instr);
                        if smp > 0 && self.m.samples[smp as usize].u_flags & SMP_NODEFAULTVOLUME == 0 {
                            self.ps.chn[nchn].n_volume = self.m.samples[smp as usize].n_volume as i32;
                        }
                    }
                    if !self.m.behaviour(kITInstrWithNoteOffOldEffects) || !self.m.song_flag(SONG_ITOLDEFFECTS) {
                        instr = 0;
                    }
                }

                let previous_new_note = self.ps.chn[nchn].n_new_note;
                if ModCommand::is_note_of(note) {
                    self.ps.chn[nchn].n_new_note = note;
                    self.ps.chn[nchn].n_last_note = note;
                    if !b_porta {
                        self.check_nna(nchn, instr, note as i32, false);
                    }
                    self.ps.chn[nchn].restore_pan_and_filter();
                }

                if instr != 0 {
                    let old_sample = self.ps.chn[nchn].p_mod_sample;
                    self.instrument_change(nchn, instr, b_porta, true, true);
                    let m = &self.m;
                    let chn = &mut self.ps.chn[nchn];
                    if t == MOD_TYPE_MOD {
                        if !b_porta || !m.behaviour(kMODSampleSwap) {
                            chn.n_new_ins = 0;
                        }
                    } else if !m.behaviour(kITInstrWithNoteOff) || ModCommand::is_note_of(note) {
                        chn.n_new_ins = 0;
                    }
                    if old_sample.is_some() && old_sample != chn.p_mod_sample {
                        self.dry_l_ofs = self.dry_l_ofs.wrapping_add(chn.n_l_ofs);
                        self.dry_r_ofs = self.dry_r_ofs.wrapping_add(chn.n_r_ofs);
                        chn.n_l_ofs = 0;
                        chn.n_r_ofs = 0;
                    }
                    if m.behaviour(kITPortamentoSwapResetsPos) {
                        if ModCommand::is_note_of(note) && old_sample != chn.p_mod_sample {
                            chn.position = SamplePosition(0);
                        }
                    } else if t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 && old_sample != chn.p_mod_sample && ModCommand::is_note_of(note) {
                        b_porta = false;
                    } else if m.behaviour(kST3SampleSwap)
                        && old_sample != chn.p_mod_sample
                        && (b_porta || !ModCommand::is_note_of(note))
                        && chn.position.uint() > chn.n_length
                    {
                        chn.n_length = 0;
                    } else if m.behaviour(kMODSampleSwap) && !chn.is_sample_playing() {
                        chn.position = SamplePosition(0);
                    }
                }

                if note != NOTE_NONE {
                    let chn = &self.ps.chn[nchn];
                    let instr_change = instr == 0 && chn.n_new_ins != 0 && ModCommand::is_note_of(note);
                    if instr_change {
                        if self.m.behaviour(kITEmptyNoteMapSlotIgnoreCell) && ModCommand::is_note_of(previous_new_note) {
                            self.ps.chn[nchn].n_new_note = previous_new_note;
                        }
                        let chn = &self.ps.chn[nchn];
                        let new_ins = chn.n_new_ins as u32;
                        let upd_vol = chn.p_mod_sample.is_none() && chn.p_mod_instrument.is_none();
                        self.instrument_change(nchn, new_ins, b_porta, upd_vol, t & (MOD_TYPE_XM | MOD_TYPE_MT2) == 0);
                        let chn = &mut self.ps.chn[nchn];
                        chn.n_new_note = note;
                        chn.swap_sample_index = 0;
                        chn.n_new_ins = 0;
                    }
                    let flags = self.ps.flags;
                    self.m.note_change(
                        flags,
                        &mut self.prng,
                        &mut self.ps.chn[nchn],
                        note as i32,
                        b_porta,
                        t & (MOD_TYPE_XM | MOD_TYPE_MT2) == 0,
                        false,
                    );
                    let chn = &mut self.ps.chn[nchn];
                    if continue_note {
                        chn.n_period = chn.n_portamento_dest;
                    }
                    if b_porta && t & (MOD_TYPE_XM | MOD_TYPE_MT2) != 0 && instr != 0 {
                        chn.set(CHN_FASTVOLRAMP);
                        chn.reset_envelopes();
                        chn.n_auto_vib_depth = 0;
                        chn.n_auto_vib_pos = 0;
                    }
                }

                if volcmd == VOLCMD_VOLUME {
                    if vol > 64 {
                        vol = 64;
                    }
                    let chn = &mut self.ps.chn[nchn];
                    chn.n_volume = (vol << 2) as i32;
                    chn.set(CHN_FASTVOLRAMP);
                } else if volcmd == VOLCMD_PANNING {
                    let flags = self.ps.flags;
                    self.m.panning(flags, &mut self.ps.chn[nchn], vol, PanBits::Pan6);
                }
            }

            if self.m.behaviour(kST3NoMutedChannels) && self.m.chn_settings[nchn].dw_flags & CHN_MUTE != 0 {
                continue;
            }
            if self.ps.tick_count == 0 {
                self.reset_auto_slides(nchn);
            }

            let mut do_volume_column = self.ps.tick_count >= start_tick;
            if self.m.behaviour(kFT2VolColDelay) && start_tick != 0 {
                let chn = &self.ps.chn[nchn];
                do_volume_column = self.ps.tick_count != 0
                    && (self.ps.tick_count != start_tick || (chn.row_command.instr == 0 && volcmd != VOLCMD_TONEPORTAMENTO));
            }
            if self.m.behaviour(kITDoublePortamentoSlides) && self.ps.chn[nchn].is_first_tick {
                let m = &self.m;
                let rc = self.ps.chn[nchn].row_command;
                let shares = m.tone_portamento_shares_effect_memory();
                let chn = &mut self.ps.chn[nchn];
                let effect_tone_porta = cmd == CMD_TONEPORTAMENTO || cmd == CMD_TONEPORTAVOL;
                if effect_tone_porta {
                    m.init_tone_portamento(chn, if cmd == CMD_TONEPORTAVOL { 0 } else { param as u16 });
                }
                if volcmd == VOLCMD_TONEPORTAMENTO {
                    m.init_tone_portamento(chn, m.vol_cmd_tone_porta(&rc, start_tick).0);
                }
                if vol != 0 && (volcmd == VOLCMD_PORTAUP || volcmd == VOLCMD_PORTADOWN) {
                    chn.n_old_porta_up = (vol << 2) as u8;
                    chn.n_old_porta_down = (vol << 2) as u8;
                    if !effect_tone_porta && shares {
                        chn.portamento_slide = (vol << 2) as u16;
                    }
                }
                if param != 0 && (cmd == CMD_PORTAMENTOUP || cmd == CMD_PORTAMENTODOWN) {
                    chn.n_old_porta_up = param as u8;
                    chn.n_old_porta_down = param as u8;
                    if shares {
                        chn.portamento_slide = param as u16;
                    }
                }
            }

            if volcmd > VOLCMD_PANNING && do_volume_column {
                if volcmd == VOLCMD_TONEPORTAMENTO {
                    let rc = self.ps.chn[nchn].row_command;
                    let (porta, clear) = self.m.vol_cmd_tone_porta(&rc, start_tick);
                    if clear {
                        cmd = CMD_NONE;
                    }
                    let tk = self.tk();
                    self.m.tone_portamento(tk, &mut self.ps.chn[nchn], porta);
                } else {
                    let tk = self.tk();
                    let m = &self.m;
                    if m.behaviour(kFT2VolColMemory) && vol == 0 {
                        match volcmd {
                            VOLCMD_VOLUME | VOLCMD_PANNING | VOLCMD_VIBRATODEPTH => {}
                            VOLCMD_PANSLIDELEFT => {
                                if !tk.first_tick() {
                                    self.ps.chn[nchn].n_pan = 0;
                                }
                                volcmd = VOLCMD_NONE;
                            }
                            _ => volcmd = VOLCMD_NONE,
                        }
                    } else if !m.behaviour(kITVolColMemory) && volcmd != VOLCMD_PLAYCONTROL {
                        let chn = &mut self.ps.chn[nchn];
                        if vol != 0 {
                            chn.n_old_vol_param = vol as u8;
                        } else {
                            vol = chn.n_old_vol_param as u32;
                        }
                    }
                    let m = &self.m;
                    match volcmd {
                        VOLCMD_VOLSLIDEUP | VOLCMD_VOLSLIDEDOWN => {
                            let chn = &mut self.ps.chn[nchn];
                            let mut go = true;
                            if vol == 0 && m.behaviour(kITVolColMemory) {
                                vol = chn.n_old_vol_param as u32;
                                if vol == 0 {
                                    go = false;
                                }
                            } else {
                                chn.n_old_vol_param = vol as u8;
                            }
                            if go {
                                let p = if volcmd == VOLCMD_VOLSLIDEUP { (vol << 4) as u8 } else { vol as u8 };
                                m.volume_slide(tk, chn, p, m.behaviour(kITVolColNoSlidePropagation));
                            }
                        }
                        VOLCMD_FINEVOLUP => {
                            if self.ps.tick_count == start_tick || !m.behaviour(kITVolColMemory) {
                                m.fine_volume_up(&mut self.ps.chn[nchn], vol as u8, m.behaviour(kITVolColMemory));
                            }
                        }
                        VOLCMD_FINEVOLDOWN => {
                            if self.ps.tick_count == start_tick || !m.behaviour(kITVolColMemory) {
                                m.fine_volume_down(&mut self.ps.chn[nchn], vol as u8, m.behaviour(kITVolColMemory));
                            }
                        }
                        VOLCMD_VIBRATOSPEED => {
                            if m.behaviour(kFT2VolColVibrato) {
                                self.ps.chn[nchn].n_vibrato_speed = (vol & 0x0F) as u8;
                            } else {
                                m.vibrato(&mut self.ps.chn[nchn], vol << 4);
                            }
                        }
                        VOLCMD_VIBRATODEPTH => m.vibrato(&mut self.ps.chn[nchn], vol),
                        VOLCMD_PANSLIDELEFT => m.panning_slide(tk, &mut self.ps.chn[nchn], vol as u8, !m.behaviour(kFT2VolColMemory)),
                        VOLCMD_PANSLIDERIGHT => {
                            m.panning_slide(tk, &mut self.ps.chn[nchn], (vol << 4) as u8, !m.behaviour(kFT2VolColMemory))
                        }
                        VOLCMD_PORTAUP => m.portamento_up(tk, &mut self.ps.chn[nchn], (vol << 2) as u8, m.behaviour(kITVolColFinePortamento)),
                        VOLCMD_PORTADOWN => {
                            m.portamento_down(tk, &mut self.ps.chn[nchn], (vol << 2) as u8, m.behaviour(kITVolColFinePortamento))
                        }
                        VOLCMD_OFFSET => {
                            let chn = &mut self.ps.chn[nchn];
                            if trigger_note && vol <= 9 {
                                if let Some(s) = chn.p_mod_sample {
                                    let smp = &m.samples[s as usize];
                                    if smp.u_flags & CHN_ADLIB == 0 {
                                        let off = if vol == 0 {
                                            chn.old_offset
                                        } else {
                                            chn.old_offset = smp.cues[(vol - 1) as usize];
                                            chn.old_offset
                                        };
                                        m.sample_offset(chn, off);
                                    }
                                }
                            }
                        }
                        VOLCMD_PLAYCONTROL => {
                            let chn = &mut self.ps.chn[nchn];
                            if chn.is_first_tick {
                                chn.play_control(vol as u8);
                            }
                        }
                        _ => {}
                    }
                }
            }

            let tk = self.tk();
            match cmd {
                CMD_NONE => {}
                CMD_VOLUME => {
                    if tk.first_tick() {
                        let chn = &mut self.ps.chn[nchn];
                        chn.n_volume = if param < 64 { (param * 4) as i32 } else { 256 };
                        chn.set(CHN_FASTVOLRAMP);
                    }
                }
                CMD_VOLUME8 => {
                    if tk.first_tick() {
                        let chn = &mut self.ps.chn[nchn];
                        chn.n_volume = param as i32;
                        chn.set(CHN_FASTVOLRAMP);
                    }
                }
                CMD_PORTAMENTOUP => {
                    if param != 0 || t & MOD_TYPE_MOD == 0 {
                        self.m.portamento_up(tk, &mut self.ps.chn[nchn], param as u8, false);
                    }
                }
                CMD_PORTAMENTODOWN => {
                    if param != 0 || t & MOD_TYPE_MOD == 0 {
                        self.m.portamento_down(tk, &mut self.ps.chn[nchn], param as u8, false);
                    }
                }
                CMD_AUTO_PORTAUP => {
                    let chn = &mut self.ps.chn[nchn];
                    chn.auto_slide.set_active(AutoSlide::PortamentoUp, param != 0);
                    chn.n_old_porta_up = param as u8;
                }
                CMD_AUTO_PORTADOWN => {
                    let chn = &mut self.ps.chn[nchn];
                    chn.auto_slide.set_active(AutoSlide::PortamentoDown, param != 0);
                    chn.n_old_porta_down = param as u8;
                }
                CMD_AUTO_PORTAUP_FINE => {
                    let chn = &mut self.ps.chn[nchn];
                    chn.auto_slide.set_active(AutoSlide::FinePortamentoUp, param != 0);
                    chn.n_old_fine_porta_up_down = param as u8;
                }
                CMD_AUTO_PORTADOWN_FINE => {
                    let chn = &mut self.ps.chn[nchn];
                    chn.auto_slide.set_active(AutoSlide::FinePortamentoDown, param != 0);
                    chn.n_old_fine_porta_up_down = param as u8;
                }
                CMD_AUTO_PORTAMENTO_FC => {
                    let chn = &mut self.ps.chn[nchn];
                    chn.auto_slide.set_active(AutoSlide::PortamentoFC, param != 0);
                    chn.n_old_porta_up = param as u8;
                    chn.n_old_porta_down = param as u8;
                }
                CMD_VOLUMESLIDE => {
                    if param != 0 || t != MOD_TYPE_MOD {
                        self.m.volume_slide(tk, &mut self.ps.chn[nchn], param as u8, false);
                    }
                }
                CMD_TONEPORTAMENTO => {
                    self.m.tone_portamento(tk, &mut self.ps.chn[nchn], param as u16);
                }
                CMD_TONEPORTAVOL => {
                    let m = &self.m;
                    let chn = &mut self.ps.chn[nchn];
                    if (param != 0 || t != MOD_TYPE_MOD) && (!chn.is_first_tick || !m.behaviour(kS3MIgnoreCombinedFineSlides)) {
                        m.volume_slide(tk, chn, param as u8, false);
                    }
                    m.tone_portamento(tk, chn, 0);
                }
                CMD_VIBRATO => self.m.vibrato(&mut self.ps.chn[nchn], param),
                CMD_VIBRATOVOL => {
                    let m = &self.m;
                    let chn = &mut self.ps.chn[nchn];
                    if (param != 0 || t != MOD_TYPE_MOD) && (!chn.is_first_tick || !m.behaviour(kS3MIgnoreCombinedFineSlides)) {
                        m.volume_slide(tk, chn, param as u8, false);
                    }
                    m.vibrato(chn, 0);
                }
                CMD_SPEED => {
                    if tk.first_tick() {
                        self.set_speed(param);
                    }
                }
                CMD_TEMPO => {
                    if self.m.behaviour(kMODVBlankTiming) {
                        if tk.first_tick() && param != 0 {
                            self.set_speed(param);
                        }
                    } else {
                        param = self.m.calculate_xparam(self.ps.pattern, self.ps.row, nchn).0;
                        if t & (MOD_TYPE_S3M | MOD_TYPE_IT | MOD_TYPE_MPT) != 0 {
                            let chn = &mut self.ps.chn[nchn];
                            if param != 0 {
                                chn.n_old_tempo = param as u8;
                            } else {
                                param = chn.n_old_tempo as u32;
                            }
                        }
                        self.set_tempo(Tempo::new(param, 0));
                    }
                }
                CMD_OFFSET => {
                    if trigger_note && !(b_porta && t & (MOD_TYPE_XM | MOD_TYPE_DBM) != 0) {
                        self.process_sample_offset(nchn);
                    }
                }
                CMD_OFFSETPERCENTAGE => {
                    if trigger_note {
                        let chn = &mut self.ps.chn[nchn];
                        let off = muldiv_unsigned(chn.n_length, param, 256);
                        self.m.sample_offset(chn, off);
                    }
                }
                CMD_ARPEGGIO => {
                    if self.ps.tick_count != 0 {
                    } else {
                        let m = &self.m;
                        let chn = &mut self.ps.chn[nchn];
                        let skip_it = (chn.n_period == 0 || chn.n_note == 0)
                            && !m.behaviour(kITArpeggio)
                            && t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0;
                        if !skip_it && !(param == 0 && t & (MOD_TYPE_XM | MOD_TYPE_MOD) != 0) {
                            chn.n_command = CMD_ARPEGGIO;
                            if param != 0 {
                                chn.n_arpeggio = param as u8;
                            }
                        }
                    }
                }
                CMD_RETRIG => {
                    if t & (MOD_TYPE_XM | MOD_TYPE_MT2) != 0 {
                        let rp = self.ps.chn[nchn].n_retrig_param as u32;
                        if param & 0xF0 == 0 {
                            param |= rp & 0xF0;
                        }
                        if param & 0x0F == 0 {
                            param |= rp & 0x0F;
                        }
                        param |= 0x100;
                    }
                    let off = if volcmd == VOLCMD_OFFSET { vol as i32 + 1 } else { 0 };
                    if self.m.behaviour(kITRetrigger) {
                        if param != 0 {
                            self.ps.chn[nchn].n_retrig_param = (param & 0xFF) as u8;
                        }
                        let rp = self.ps.chn[nchn].n_retrig_param as i32;
                        self.retrig_note(nchn, rp, off);
                    } else {
                        if param != 0 {
                            self.ps.chn[nchn].n_retrig_param = (param & 0xFF) as u8;
                        } else {
                            param = self.ps.chn[nchn].n_retrig_param as u32;
                        }
                        self.retrig_note(nchn, param as i32, off);
                    }
                }
                CMD_TREMOR => {
                    if tk.first_tick() {
                        let m = &self.m;
                        let chn = &mut self.ps.chn[nchn];
                        if m.behaviour(kITTremor) {
                            if param != 0 && !m.song_flag(SONG_ITOLDEFFECTS) {
                                if param & 0xF0 != 0 {
                                    param -= 0x10;
                                }
                                if param & 0x0F != 0 {
                                    param -= 0x01;
                                }
                                chn.n_tremor_param = param as u8;
                            }
                            chn.n_tremor_count |= 0x80;
                        } else if m.behaviour(kFT2Tremor) {
                            chn.n_tremor_count |= 0x80;
                        }
                        chn.n_command = CMD_TREMOR;
                        if param != 0 {
                            chn.n_tremor_param = param as u8;
                        }
                    }
                }
                CMD_GLOBALVOLUME => {
                    if tk.first_tick() {
                        if t & GLOBALVOL_7BIT_FORMATS == 0 {
                            param *= 2;
                        }
                        if param <= 128 {
                            self.ps.global_volume = (param * 2) as i32;
                        } else if t & (MOD_TYPE_IT | MOD_TYPE_MPT | MOD_TYPE_S3M) == 0 {
                            self.ps.global_volume = 256;
                        }
                        let ci = if self.m.behaviour(kPerChannelGlobalVolSlide) { nchn } else { 0 };
                        self.ps.chn[ci].auto_slide.set_active(AutoSlide::GlobalVolumeSlide, false);
                    }
                }
                CMD_GLOBALVOLSLIDE => {
                    let ci = if self.m.behaviour(kPerChannelGlobalVolSlide) { nchn } else { 0 };
                    self.global_vol_slide(param as u8, ci);
                }
                CMD_PANNING8 => {
                    if tk.first_tick() {
                        self.m.panning(tk.flags, &mut self.ps.chn[nchn], param, PanBits::Pan8);
                    }
                }
                CMD_PANNINGSLIDE => self.m.panning_slide(tk, &mut self.ps.chn[nchn], param as u8, true),
                CMD_TREMOLO => self.m.tremolo(&mut self.ps.chn[nchn], param),
                CMD_FINEVIBRATO => self.m.fine_vibrato(&mut self.ps.chn[nchn], param),
                CMD_MODCMDEX => self.extended_mod_commands(nchn, param as u8),
                CMD_S3MCMDEX => self.extended_s3m_commands(nchn, param as u8),
                CMD_KEYOFF => {
                    let m = &self.m;
                    let tick = self.ps.tick_count;
                    let chn = &mut self.ps.chn[nchn];
                    if m.behaviour(kFT2KeyOff) {
                        if tick == param {
                            let has_env = chn.p_mod_instrument.and_then(|i| m.instrument(i as u32)).is_some_and(|i| i.vol_env.has(ENV_ENABLED));
                            if !has_env {
                                if param == 0 && (chn.row_command.instr != 0 || chn.row_command.volcmd != VOLCMD_NONE) {
                                    chn.set(CHN_NOTEFADE);
                                } else {
                                    chn.set(CHN_FASTVOLRAMP);
                                    chn.n_volume = 0;
                                }
                            }
                            m.key_off(chn);
                        }
                    } else if tk.first_tick() {
                        m.key_off(chn);
                    }
                }
                CMD_XFINEPORTAUPDOWN => match param & 0xF0 {
                    0x10 => self.m.extra_fine_portamento_up(&mut self.ps.chn[nchn], (param & 0x0F) as u8),
                    0x20 => self.m.extra_fine_portamento_down(&mut self.ps.chn[nchn], (param & 0x0F) as u8),
                    0x50 | 0x60 | 0x70 | 0x90 | 0xA0 => {
                        if !self.m.behaviour(kFT2RestrictXCommand) {
                            self.extended_s3m_commands(nchn, param as u8);
                        }
                    }
                    _ => {}
                },
                CMD_FINETUNE | CMD_FINETUNE_SMOOTH => {
                    if tk.first_tick() || cmd == CMD_FINETUNE_SMOOTH {
                        self.set_finetune(nchn, cmd == CMD_FINETUNE_SMOOTH);
                    }
                }
                CMD_CHANNELVOLUME => {
                    if tk.first_tick() && param <= 64 {
                        let chn = &mut self.ps.chn[nchn];
                        chn.n_global_vol = param as u8;
                        chn.set(CHN_FASTVOLRAMP);
                    }
                }
                CMD_CHANNELVOLSLIDE => self.m.channel_vol_slide(tk, &mut self.ps.chn[nchn], param as u8),
                CMD_PANBRELLO => self.m.panbrello(&mut self.ps.chn[nchn], param),
                CMD_SETENVPOSITION => {
                    if tk.first_tick() {
                        let ft2 = self.m.behaviour(kFT2SetPanEnvPos);
                        let chn = &mut self.ps.chn[nchn];
                        chn.vol_env.n_env_position = param;
                        if !ft2 || chn.vol_env.flags & ENV_SUSTAIN != 0 {
                            chn.pan_env.n_env_position = param;
                            chn.pitch_env.n_env_position = param;
                        }
                    }
                }
                CMD_POSITIONJUMP => self.position_jump(nchn),
                CMD_PATTERNBREAK => {
                    let row = self.pattern_break(nchn, param as u8);
                    if row != ROWINDEX_INVALID {
                        self.ps.break_row = row;
                        if self.ps.flag(SONG_PATTERNLOOP) {
                            self.ps.pos_jump = self.ps.current_order;
                        }
                    }
                }
                CMD_NOTESLIDEUP | CMD_NOTESLIDEDOWN | CMD_NOTESLIDEUPRETRIG | CMD_NOTESLIDEDOWNRETRIG => {
                    let up = cmd == CMD_NOTESLIDEUP || cmd == CMD_NOTESLIDEUPRETRIG;
                    let retrig = cmd == CMD_NOTESLIDEUPRETRIG || cmd == CMD_NOTESLIDEDOWNRETRIG;
                    self.m.note_slide(tk, &mut self.ps.chn[nchn], param, up, retrig);
                }
                CMD_REVERSEOFFSET => self.m.reverse_sample_offset(&mut self.ps.chn[nchn], param as u8),
                CMD_AUTO_VOLUMESLIDE => self.m.auto_volume_slide(&mut self.ps.chn[nchn], param as u8),
                CMD_VOLUMEDOWN_ETX => {
                    if self.ps.chn[nchn].is_first_tick {
                        self.m.volume_down_etx(tk, &mut self.ps.chn[nchn], param as u8);
                    }
                }
                CMD_TONEPORTA_DURATION => {
                    if self.ps.chn[nchn].row_command.is_note() && trigger_note {
                        self.m.tone_portamento_with_duration(tk, &mut self.ps.chn[nchn], Some(param as u16));
                    }
                }
                CMD_VOLUMEDOWN_DURATION => {
                    if self.ps.tick_count == 0 {
                        self.m.channel_volume_down_with_duration(tk, &mut self.ps.chn[nchn], Some(param as u16));
                    }
                }
                _ => {}
            }
            if self.m.behaviour(kST3EffectMemory) && cmd != CMD_NONE && param != 0 {
                Player::update_s3m_effect_memory(&mut self.ps.chn[nchn], param as u8);
            }
            let chn = &mut self.ps.chn[nchn];
            if chn.row_command.instr != 0 {
                chn.n_old_ins = chn.row_command.instr;
            }
            self.process_auto_slides(nchn);
        }
        if self.ps.flag(SONG_FIRSTTICK) && self.handle_next_row() {
            self.ps.flags |= SONG_BREAKTOROW;
        }
        true
    }
}

/// `CalculateSmoothParamChange`.
pub fn smooth_change(ticks_on_row: u32, tick_count: u32, current: f32, param: f32) -> f32 {
    let ticks_left = ticks_on_row.wrapping_sub(tick_count);
    if ticks_left > 1 {
        let step = (param - current) / ticks_left as f32;
        current + step
    } else {
        param
    }
}
