//! Pattern cells, ported from libopenmpt 0.8.9 `soundlib/modcommand.h`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

#![allow(dead_code)]

// Notes
pub const NOTE_NONE: u8 = 0;
pub const NOTE_MIN: u8 = 1;
pub const NOTE_MAX: u8 = 128;
pub const NOTE_MIDDLEC: u8 = 5 * 12 + NOTE_MIN;
pub const NOTE_KEYOFF: u8 = 0xFF;
pub const NOTE_NOTECUT: u8 = 0xFE;
pub const NOTE_FADE: u8 = 0xFD;
pub const NOTE_PC: u8 = 0xFC;
pub const NOTE_PCS: u8 = 0xFB;
pub const NOTE_MIN_SPECIAL: u8 = NOTE_PCS;
pub const NOTE_MAX_SPECIAL: u8 = NOTE_KEYOFF;

// Volume column commands
pub const VOLCMD_NONE: u8 = 0;
pub const VOLCMD_VOLUME: u8 = 1;
pub const VOLCMD_PANNING: u8 = 2;
pub const VOLCMD_VOLSLIDEUP: u8 = 3;
pub const VOLCMD_VOLSLIDEDOWN: u8 = 4;
pub const VOLCMD_FINEVOLUP: u8 = 5;
pub const VOLCMD_FINEVOLDOWN: u8 = 6;
pub const VOLCMD_VIBRATOSPEED: u8 = 7;
pub const VOLCMD_VIBRATODEPTH: u8 = 8;
pub const VOLCMD_PANSLIDELEFT: u8 = 9;
pub const VOLCMD_PANSLIDERIGHT: u8 = 10;
pub const VOLCMD_TONEPORTAMENTO: u8 = 11;
pub const VOLCMD_PORTAUP: u8 = 12;
pub const VOLCMD_PORTADOWN: u8 = 13;
pub const VOLCMD_PLAYCONTROL: u8 = 14;
pub const VOLCMD_OFFSET: u8 = 15;

// Effect column commands
pub const CMD_NONE: u8 = 0;
pub const CMD_ARPEGGIO: u8 = 1;
pub const CMD_PORTAMENTOUP: u8 = 2;
pub const CMD_PORTAMENTODOWN: u8 = 3;
pub const CMD_TONEPORTAMENTO: u8 = 4;
pub const CMD_VIBRATO: u8 = 5;
pub const CMD_TONEPORTAVOL: u8 = 6;
pub const CMD_VIBRATOVOL: u8 = 7;
pub const CMD_TREMOLO: u8 = 8;
pub const CMD_PANNING8: u8 = 9;
pub const CMD_OFFSET: u8 = 10;
pub const CMD_VOLUMESLIDE: u8 = 11;
pub const CMD_POSITIONJUMP: u8 = 12;
pub const CMD_VOLUME: u8 = 13;
pub const CMD_PATTERNBREAK: u8 = 14;
pub const CMD_RETRIG: u8 = 15;
pub const CMD_SPEED: u8 = 16;
pub const CMD_TEMPO: u8 = 17;
pub const CMD_TREMOR: u8 = 18;
pub const CMD_MODCMDEX: u8 = 19;
pub const CMD_S3MCMDEX: u8 = 20;
pub const CMD_CHANNELVOLUME: u8 = 21;
pub const CMD_CHANNELVOLSLIDE: u8 = 22;
pub const CMD_GLOBALVOLUME: u8 = 23;
pub const CMD_GLOBALVOLSLIDE: u8 = 24;
pub const CMD_KEYOFF: u8 = 25;
pub const CMD_FINEVIBRATO: u8 = 26;
pub const CMD_PANBRELLO: u8 = 27;
pub const CMD_XFINEPORTAUPDOWN: u8 = 28;
pub const CMD_PANNINGSLIDE: u8 = 29;
pub const CMD_SETENVPOSITION: u8 = 30;
pub const CMD_MIDI: u8 = 31;
pub const CMD_SMOOTHMIDI: u8 = 32;
pub const CMD_DELAYCUT: u8 = 33;
pub const CMD_XPARAM: u8 = 34;
pub const CMD_FINETUNE: u8 = 35;
pub const CMD_FINETUNE_SMOOTH: u8 = 36;
pub const CMD_DUMMY: u8 = 37;
pub const CMD_NOTESLIDEUP: u8 = 38;
pub const CMD_NOTESLIDEDOWN: u8 = 39;
pub const CMD_NOTESLIDEUPRETRIG: u8 = 40;
pub const CMD_NOTESLIDEDOWNRETRIG: u8 = 41;
pub const CMD_REVERSEOFFSET: u8 = 42;
pub const CMD_DBMECHO: u8 = 43;
pub const CMD_OFFSETPERCENTAGE: u8 = 44;
pub const CMD_DIGIREVERSESAMPLE: u8 = 45;
pub const CMD_VOLUME8: u8 = 46;
pub const CMD_HMN_MEGA_ARP: u8 = 47;
pub const CMD_MED_SYNTH_JUMP: u8 = 48;
pub const CMD_AUTO_VOLUMESLIDE: u8 = 49;
pub const CMD_AUTO_PORTAUP: u8 = 50;
pub const CMD_AUTO_PORTADOWN: u8 = 51;
pub const CMD_AUTO_PORTAUP_FINE: u8 = 52;
pub const CMD_AUTO_PORTADOWN_FINE: u8 = 53;
pub const CMD_AUTO_PORTAMENTO_FC: u8 = 54;
pub const CMD_TONEPORTA_DURATION: u8 = 55;
pub const CMD_VOLUMEDOWN_DURATION: u8 = 56;
pub const CMD_VOLUMEDOWN_ETX: u8 = 57;
pub const MAX_EFFECTS: u8 = 58;

/// One pattern cell (`ModCommand`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModCommand {
    pub note: u8,
    pub instr: u8,
    pub volcmd: u8,
    pub command: u8,
    pub vol: u8,
    pub param: u8,
}

impl ModCommand {
    pub fn is_empty(&self) -> bool {
        self.note == NOTE_NONE && self.instr == 0 && self.volcmd == VOLCMD_NONE && self.command == CMD_NONE
    }
    pub fn is_pc_note_of(note: u8) -> bool {
        note == NOTE_PC || note == NOTE_PCS
    }
    pub fn is_pc_note(&self) -> bool {
        Self::is_pc_note_of(self.note)
    }
    pub fn is_note_of(note: u8) -> bool {
        (NOTE_MIN..=NOTE_MAX).contains(&note)
    }
    pub fn is_note(&self) -> bool {
        Self::is_note_of(self.note)
    }
    pub fn is_special_note_of(note: u8) -> bool {
        (NOTE_MIN_SPECIAL..=NOTE_MAX_SPECIAL).contains(&note)
    }
    pub fn is_special_note(&self) -> bool {
        Self::is_special_note_of(self.note)
    }
    pub fn is_note_or_empty(&self) -> bool {
        self.note == NOTE_NONE || self.is_note()
    }
    pub fn is_tone_portamento(&self) -> bool {
        self.command == CMD_TONEPORTAMENTO
            || self.command == CMD_TONEPORTAVOL
            || self.command == CMD_TONEPORTA_DURATION
            || self.volcmd == VOLCMD_TONEPORTAMENTO
    }
    pub fn is_normal_volume_slide(&self) -> bool {
        self.command == CMD_VOLUMESLIDE || self.command == CMD_VIBRATOVOL || self.command == CMD_TONEPORTAVOL
    }
    pub fn is_amiga_note_of(note: u8) -> bool {
        !Self::is_note_of(note) || (note >= NOTE_MIDDLEC - 12 && note < NOTE_MIDDLEC + 24)
    }
    pub fn is_any_pitch_slide(&self) -> bool {
        match self.command {
            CMD_PORTAMENTOUP | CMD_PORTAMENTODOWN | CMD_TONEPORTAMENTO | CMD_TONEPORTAVOL | CMD_NOTESLIDEUP | CMD_NOTESLIDEDOWN
            | CMD_NOTESLIDEUPRETRIG | CMD_NOTESLIDEDOWNRETRIG | CMD_AUTO_PORTAUP | CMD_AUTO_PORTADOWN | CMD_AUTO_PORTAUP_FINE
            | CMD_AUTO_PORTADOWN_FINE | CMD_AUTO_PORTAMENTO_FC | CMD_TONEPORTA_DURATION => return true,
            CMD_MODCMDEX | CMD_XFINEPORTAUPDOWN => {
                if (0x10..=0x2F).contains(&self.param) {
                    return true;
                }
            }
            _ => {}
        }
        matches!(self.volcmd, VOLCMD_TONEPORTAMENTO | VOLCMD_PORTAUP | VOLCMD_PORTADOWN)
    }
    pub fn clear(&mut self) {
        *self = ModCommand::default();
    }
}
