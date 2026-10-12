//! MultiTracker (MTM), Composer 669, UltraTracker (ULT) and Scream Tracker
//! 2 (STM) modules, ported from libopenmpt 0.8.9 `soundlib/Load_mtm.cpp`,
//! `Load_669.cpp`, `Load_ult.cpp`, `Load_stm.cpp`, plus the pattern and
//! cell helpers they use from `pattern.cpp` and `modcommand.cpp`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

use crate::command::*;
use crate::defs::pb::*;
use crate::defs::*;
use crate::io::{Channels, Encoding, Reader, SampleIo, le16, le32, read_name};
use crate::load_mod::convert_mod_command;
use crate::sample::ModSample;
use crate::sndfile::{Module, Pattern};
use crate::tables::IMPULSE_TRACKER_PORTA_VOL_CMD;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Retry {
    Ignore,
    NextRow,
    PreviousRow,
}

/// `CPattern::WriteEffect` with `EffectWriter(cmd, param)` (any channel,
/// single effect per row).
fn write_effect(p: &mut Pattern, nc: usize, mut row: u32, mut cmd: u8, mut param: u8, mut is_vol: bool, retry_mode: Retry, is_s3m: bool) -> bool {
    let mut retry = true;
    let mut volcmd: u8 = 0;
    let mut vol: u8 = 0;
    loop {
        if p.data.is_empty() || row >= p.rows {
            return false;
        }
        let base = row as usize * nc;
        let cells = &mut p.data[base..base + nc];
        if cells.iter().any(|m| if !is_vol { m.command == cmd } else { m.volcmd == volcmd }) {
            return true;
        }
        for m in cells.iter_mut() {
            if !is_vol && m.command == CMD_NONE {
                m.command = cmd;
                m.param = param;
                return true;
            }
            if is_vol && m.volcmd == VOLCMD_NONE {
                m.volcmd = volcmd;
                m.vol = vol;
                return true;
            }
        }
        let mut retried = false;
        if retry {
            if !is_vol {
                for m in cells.iter_mut() {
                    match m.command {
                        CMD_VOLUME => {
                            m.volcmd = VOLCMD_VOLUME;
                            m.vol = m.param;
                            m.command = cmd;
                            m.param = param;
                            return true;
                        }
                        CMD_PANNING8 if !(is_s3m && m.param > 0x80) => {
                            m.volcmd = VOLCMD_PANNING;
                            m.command = cmd;
                            m.vol = if is_s3m { ((m.param as u32 + 1) / 2) as u8 } else { ((m.param as u32 + 2) / 4) as u8 };
                            m.param = param;
                            return true;
                        }
                        _ => {}
                    }
                }
            }
            if is_vol {
                let (nc2, np) = match volcmd {
                    VOLCMD_PANNING => (CMD_PANNING8, (vol as u32 * if is_s3m { 2 } else { 4 }).min(255) as u8),
                    VOLCMD_VOLUME => (CMD_VOLUME, vol),
                    _ => (CMD_NONE, vol),
                };
                if nc2 != CMD_NONE {
                    cmd = nc2;
                    param = np;
                    retry = false;
                }
            } else {
                let (nv, nvol) = if cmd == CMD_PANNING8 && is_s3m {
                    if param <= 0x80 { (VOLCMD_PANNING, param / 2) } else { (VOLCMD_NONE, 0) }
                } else {
                    convert_to_vol_command(cmd, param, true)
                };
                if nv != VOLCMD_NONE {
                    volcmd = nv;
                    vol = nvol;
                    retry = false;
                }
            }
            if !retry {
                is_vol = !is_vol;
                retried = true;
            }
        }
        if retried {
            continue;
        }
        match retry_mode {
            Retry::NextRow if row + 1 < p.rows => {
                row += 1;
                retry = true;
            }
            Retry::PreviousRow if row > 0 => {
                row -= 1;
                retry = true;
            }
            _ => return false,
        }
    }
}

/// `ModCommand::ConvertToVolCommand`.
pub fn convert_to_vol_command(effect: u8, mut param: u8, force: bool) -> (u8, u8) {
    match effect {
        CMD_VOLUME => return (VOLCMD_VOLUME, param.min(64)),
        CMD_VOLUME8 => {
            if force || param & 3 == 0 {
                return (VOLCMD_VOLUME, ((param as u32 + 3) / 4) as u8);
            }
        }
        CMD_PORTAMENTOUP => {
            if force || (param & 3 == 0 && param < 0xE0) {
                return (VOLCMD_PORTAUP, param / 4);
            }
        }
        CMD_PORTAMENTODOWN => {
            if force || (param & 3 == 0 && param < 0xE0) {
                return (VOLCMD_PORTADOWN, param / 4);
            }
        }
        CMD_TONEPORTAMENTO => {
            if param >= 0xF0 {
                return (VOLCMD_TONEPORTAMENTO, 9);
            }
            for n in 0..10u8 {
                let t = IMPULSE_TRACKER_PORTA_VOL_CMD[n as usize];
                if if force { param <= t } else { param == t } {
                    return (VOLCMD_TONEPORTAMENTO, n);
                }
            }
        }
        CMD_VIBRATO => {
            if force {
                param = (param & 0x0F).min(9);
            } else if (param & 0x0F) > 9 || (param & 0xF0) != 0 {
                return (VOLCMD_NONE, 0);
            }
            return (VOLCMD_VIBRATODEPTH, param & 0x0F);
        }
        CMD_FINEVIBRATO => {
            if force {
                param = 0;
            } else if param != 0 {
                return (VOLCMD_NONE, 0);
            }
            return (VOLCMD_VIBRATODEPTH, param);
        }
        CMD_PANNING8 => {
            return (VOLCMD_PANNING, if param == 255 { 64 } else { param / 4 });
        }
        CMD_VOLUMESLIDE => {
            if param != 0 {
                if param & 0x0F == 0 {
                    return (VOLCMD_VOLSLIDEUP, param >> 4);
                } else if param & 0xF0 == 0 {
                    return (VOLCMD_VOLSLIDEDOWN, param);
                } else if param & 0x0F == 0x0F {
                    return (VOLCMD_FINEVOLUP, param >> 4);
                } else if param & 0xF0 == 0xF0 {
                    return (VOLCMD_FINEVOLDOWN, param & 0x0F);
                }
            }
        }
        CMD_S3MCMDEX => match param & 0xF0 {
            0x80 => return (VOLCMD_PANNING, ((param & 0x0F) << 2) + 2),
            0x90 if param >= 0x9E && force => return (VOLCMD_PLAYCONTROL, param - 0x9E + 2),
            _ => {}
        },
        CMD_MODCMDEX => match param & 0xF0 {
            0x80 => return (VOLCMD_PANNING, ((param & 0x0F) << 2) + 2),
            0xA0 => return (VOLCMD_FINEVOLUP, param & 0x0F),
            0xB0 => return (VOLCMD_FINEVOLDOWN, param & 0x0F),
            _ => {}
        },
        _ => {}
    }
    (VOLCMD_NONE, 0)
}

/// `ModCommand::GetEffectWeight`.
fn effect_weight(cmd: u8) -> usize {
    const W: [u8; 58] = [
        CMD_NONE,
        CMD_DUMMY,
        CMD_XPARAM,
        CMD_SETENVPOSITION,
        CMD_MED_SYNTH_JUMP,
        CMD_KEYOFF,
        CMD_TREMOLO,
        CMD_FINEVIBRATO,
        CMD_VIBRATO,
        CMD_XFINEPORTAUPDOWN,
        CMD_FINETUNE,
        CMD_FINETUNE_SMOOTH,
        CMD_PANBRELLO,
        CMD_S3MCMDEX,
        CMD_MODCMDEX,
        CMD_DELAYCUT,
        CMD_MIDI,
        CMD_SMOOTHMIDI,
        CMD_PANNINGSLIDE,
        CMD_PANNING8,
        CMD_NOTESLIDEUPRETRIG,
        CMD_NOTESLIDEUP,
        CMD_NOTESLIDEDOWNRETRIG,
        CMD_NOTESLIDEDOWN,
        CMD_PORTAMENTOUP,
        CMD_AUTO_PORTAMENTO_FC,
        CMD_AUTO_PORTAUP_FINE,
        CMD_AUTO_PORTAUP,
        CMD_PORTAMENTODOWN,
        CMD_AUTO_PORTADOWN_FINE,
        CMD_AUTO_PORTADOWN,
        CMD_VOLUMESLIDE,
        CMD_AUTO_VOLUMESLIDE,
        CMD_VIBRATOVOL,
        CMD_VOLUME,
        CMD_VOLUME8,
        CMD_DIGIREVERSESAMPLE,
        CMD_REVERSEOFFSET,
        CMD_OFFSETPERCENTAGE,
        CMD_OFFSET,
        CMD_TREMOR,
        CMD_RETRIG,
        CMD_HMN_MEGA_ARP,
        CMD_ARPEGGIO,
        CMD_TONEPORTA_DURATION,
        CMD_TONEPORTAMENTO,
        CMD_TONEPORTAVOL,
        CMD_DBMECHO,
        CMD_VOLUMEDOWN_DURATION,
        CMD_VOLUMEDOWN_ETX,
        CMD_CHANNELVOLSLIDE,
        CMD_CHANNELVOLUME,
        CMD_GLOBALVOLSLIDE,
        CMD_GLOBALVOLUME,
        CMD_TEMPO,
        CMD_SPEED,
        CMD_POSITIONJUMP,
        CMD_PATTERNBREAK,
    ];
    W.iter().position(|&c| c == cmd).unwrap_or(0)
}

/// `ModCommand::IsGlobalCommand`.
fn is_global_command(command: u8, param: u8) -> bool {
    match command {
        CMD_POSITIONJUMP | CMD_PATTERNBREAK | CMD_SPEED | CMD_TEMPO | CMD_GLOBALVOLUME | CMD_GLOBALVOLSLIDE | CMD_MIDI | CMD_SMOOTHMIDI | CMD_DBMECHO => true,
        CMD_MODCMDEX => matches!(param & 0xF0, 0x00 | 0x60 | 0xE0),
        CMD_XFINEPORTAUPDOWN | CMD_S3MCMDEX => matches!(param & 0xF0, 0x60 | 0x90 | 0xB0 | 0xE0),
        _ => false,
    }
}

/// `ModCommand::CombineEffects`.
fn combine_effects(eff1: &mut u8, param1: &mut u8, eff2: &mut u8, param2: &mut u8) {
    if *eff1 == CMD_VOLUMESLIDE && (*eff2 == CMD_VIBRATO || *eff2 == CMD_TONEPORTAVOL) && *param2 == 0 {
        *eff1 = if *eff2 == CMD_VIBRATO { CMD_VIBRATOVOL } else { CMD_TONEPORTAVOL };
        *eff2 = CMD_NONE;
    } else if *eff2 == CMD_VOLUMESLIDE && (*eff1 == CMD_VIBRATO || *eff1 == CMD_TONEPORTAVOL) && *param1 == 0 {
        *eff1 = if *eff1 == CMD_VIBRATO { CMD_VIBRATOVOL } else { CMD_TONEPORTAVOL };
        *param1 = *param2;
        *eff2 = CMD_NONE;
    } else if *eff1 == CMD_OFFSET && *eff2 == CMD_S3MCMDEX && *param2 == 0x9F {
        *eff1 = CMD_REVERSEOFFSET;
        *eff2 = CMD_NONE;
    } else if *eff1 == CMD_S3MCMDEX && *param1 == 0x9F && *eff2 == CMD_OFFSET {
        *eff1 = CMD_REVERSEOFFSET;
        *param1 = *param2;
        *eff2 = CMD_NONE;
    }
}

/// `ModCommand::FillInTwoCommands`: returns the command that did not fit.
fn fill_in_two_commands(m: &mut ModCommand, mut e1: u8, mut p1: u8, mut e2: u8, mut p2: u8) -> (u8, u8) {
    if e1 == e2
        && matches!(
            e1,
            CMD_ARPEGGIO
                | CMD_PANNING8
                | CMD_OFFSET
                | CMD_POSITIONJUMP
                | CMD_VOLUME
                | CMD_PATTERNBREAK
                | CMD_SPEED
                | CMD_TEMPO
                | CMD_CHANNELVOLUME
                | CMD_GLOBALVOLUME
                | CMD_KEYOFF
                | CMD_SETENVPOSITION
                | CMD_MIDI
                | CMD_SMOOTHMIDI
                | CMD_DELAYCUT
                | CMD_FINETUNE
                | CMD_FINETUNE_SMOOTH
                | CMD_DUMMY
                | CMD_REVERSEOFFSET
                | CMD_DBMECHO
                | CMD_OFFSETPERCENTAGE
                | CMD_DIGIREVERSESAMPLE
                | CMD_VOLUME8
                | CMD_HMN_MEGA_ARP
                | CMD_MED_SYNTH_JUMP
        )
    {
        e2 = CMD_NONE;
    }
    for n in 0..4 {
        let vc = convert_to_vol_command(e1, p1, n > 1);
        if e1 == CMD_NONE || vc.0 != VOLCMD_NONE {
            m.volcmd = vc.0;
            m.vol = vc.1;
            m.command = e2;
            m.param = p2;
            return (CMD_NONE, 0);
        }
        std::mem::swap(&mut e1, &mut e2);
        std::mem::swap(&mut p1, &mut p2);
    }
    if effect_weight(e1) > effect_weight(e2) {
        std::mem::swap(&mut e1, &mut e2);
        std::mem::swap(&mut p1, &mut p2);
    }
    if e2 == CMD_OFFSET && p2 == 0 {
        m.volcmd = VOLCMD_OFFSET;
        m.vol = 0;
        m.command = e1;
        m.param = p1;
        return (CMD_NONE, 0);
    }
    m.volcmd = VOLCMD_NONE;
    m.vol = 0;
    m.command = e2;
    m.param = p2;
    (e1, p1)
}

fn read_orders(src: &[u8], stop: u16, ignore: u16) -> Vec<PatternIndex> {
    src.iter()
        .map(|&p| {
            let p = p as u16;
            if p == stop {
                PATTERNINDEX_INVALID
            } else if p == ignore {
                PATTERNINDEX_SKIP
            } else {
                p
            }
        })
        .collect()
}

fn empty_patterns(m: &mut Module, n: usize, rows: u32) {
    let nc = m.num_channels();
    m.patterns = (0..n).map(|_| Pattern { rows, data: vec![ModCommand::default(); rows as usize * nc], ..Default::default() }).collect();
}

// ---------------------------------------------------------------- MTM

fn mtm_header_ok(h: &[u8]) -> bool {
    &h[..3] == b"MTM" && h[3] < 0x20 && h[27] <= 127 && h[32] <= 64 && h[33] <= 32 && h[33] != 0
}

pub fn probe_mtm(data: &[u8]) -> bool {
    data.len() >= 66 && mtm_header_ok(&data[..66])
}

pub fn read_mtm(data: &[u8]) -> Option<Module> {
    let mut file = Reader::new(data);
    let h = file.read_slice(66)?;
    if !mtm_header_ok(h) {
        return None;
    }
    let num_tracks = le16(h, 24) as usize;
    let last_pattern = h[26] as usize;
    let last_order = h[27] as usize;
    let comment_size = le16(h, 28) as usize;
    let num_samples = h[30] as usize;
    let beats = h[32];
    let num_channels = h[33] as usize;
    if !file.can_read(37 * num_samples + 128 + 192 * num_tracks + 64 * (last_pattern + 1) + comment_size) {
        return None;
    }
    let mut m = Module::new(MOD_TYPE_MTM, num_channels);
    m.title = read_name(&h[4..24], false);
    m.num_samples = num_samples as SampleIndex;
    m.samples.resize_with(num_samples + 1, || ModSample::new(MOD_TYPE_MTM));
    m.format_name = "MultiTracker".into();
    for smp in 1..=num_samples {
        let sb = file.read_array::<37>();
        let s = &mut m.samples[smp];
        s.initialize(MOD_TYPE_NONE);
        s.name = read_name(&sb[..22], false);
        s.n_volume = (sb[35] as u16 * 4).min(256);
        let length = le32(&sb, 22);
        if length > 2 {
            s.n_length = length;
            s.n_loop_start = le32(&sb, 26);
            s.n_loop_end = le32(&sb, 30).max(1) - 1;
            s.n_loop_end = s.n_loop_end.min(s.n_length);
            if s.n_loop_start.wrapping_add(4) >= s.n_loop_end {
                s.n_loop_start = 0;
                s.n_loop_end = 0;
            }
            if s.n_loop_end > 2 {
                s.u_flags |= CHN_LOOP;
            }
            let ft = sb[34] as i8;
            s.n_fine_tune = ft;
            s.n_c5_speed = ModSample::transpose_to_frequency(0, ft as i32 * 16);
            if sb[36] & 0x01 != 0 {
                s.u_flags |= CHN_16BIT;
                s.n_length /= 2;
                s.n_loop_start /= 2;
                s.n_loop_end /= 2;
            }
        }
    }
    for chn in 0..num_channels {
        m.chn_settings[chn].n_pan = (((h[34 + chn] & 0x0F) as u16) << 4) + 8;
    }
    let orders = file.read_array::<128>();
    m.order = read_orders(&orders[..last_order + 1], 0xFF, 0xFE);
    let rows = if beats != 0 { beats as u32 } else { 64 };
    let mut tracks = file.read_chunk(192 * num_tracks);
    empty_patterns(&mut m, last_pattern + 1, rows);
    let nc = m.num_channels();
    let (mut has_speed, mut has_tempo) = (false, false);
    for pat in 0..=last_pattern {
        for chn in 0..32usize {
            let track = file.u16le() as usize;
            if track == 0 || track > num_tracks || chn >= nc {
                continue;
            }
            tracks.seek(192 * (track - 1));
            for row in 0..rows as usize {
                let [note_instr, instr_cmd, par] = tracks.read_array::<3>();
                let mc = &mut m.patterns[pat].data[row * nc + chn];
                if note_instr & 0xFC != 0 {
                    mc.note = (note_instr >> 2) + 36 + NOTE_MIN;
                }
                mc.instr = ((note_instr & 0x03) << 4) | (instr_cmd >> 4);
                let mut cmd = instr_cmd & 0x0F;
                let mut param = par;
                if cmd == 0x0A {
                    if param & 0xF0 != 0 {
                        param &= 0xF0;
                    } else {
                        param &= 0x0F;
                    }
                } else if cmd == 0x08 {
                    cmd = 0;
                    param = 0;
                } else if cmd == 0x0E && matches!(param & 0xF0, 0x00 | 0x30 | 0x40 | 0x60 | 0x70 | 0xF0) {
                    cmd = 0;
                    param = 0;
                }
                if cmd != 0 || param != 0 {
                    convert_mod_command(mc, cmd, param);
                    if mc.command == CMD_SPEED {
                        has_speed = true;
                    } else if mc.command == CMD_TEMPO {
                        has_tempo = true;
                    }
                }
            }
        }
    }
    if has_speed && has_tempo {
        let same_row = m.patterns.iter().any(|p| {
            (0..p.rows as usize).any(|r| {
                let row = &p.data[r * nc..(r + 1) * nc];
                row.iter().any(|c| c.command == CMD_SPEED) && row.iter().any(|c| c.command == CMD_TEMPO)
            })
        });
        if !same_row {
            for p in m.patterns.iter_mut() {
                for r in 0..p.rows {
                    let found = p.data[r as usize * nc..(r as usize + 1) * nc].iter().find(|c| c.command == CMD_SPEED || c.command == CMD_TEMPO).map(|c| c.command);
                    if let Some(c) = found {
                        let write_tempo = c == CMD_SPEED;
                        write_effect(p, nc, r, if write_tempo { CMD_TEMPO } else { CMD_SPEED }, if write_tempo { 125 } else { 6 }, false, Retry::Ignore, true);
                    }
                }
            }
        }
    }
    file.skip(comment_size);
    for smp in 1..=num_samples {
        let bits = if m.samples[smp].u_flags & CHN_16BIT != 0 { 16 } else { 8 };
        SampleIo::new(bits, Channels::Mono, false, Encoding::Unsigned).read_sample(&mut m.samples[smp], &mut file);
    }
    m.min_period = 64;
    m.max_period = 32767;
    Some(m)
}

// ---------------------------------------------------------------- 669

fn header_669_ok(h: &[u8]) -> bool {
    if (&h[..2] != b"if" && &h[..2] != b"JN") || h[110] > 64 || h[112] >= 128 || h[111] > 128 {
        return false;
    }
    let mut invalid = 0;
    for &c in &h[2..110] {
        if c > 0 && c <= 31 {
            invalid += 1;
            if invalid > 40 {
                return false;
            }
        }
    }
    for i in 0..128 {
        let (o, t, b) = (h[113 + i], h[241 + i], h[369 + i]);
        if (128..0xFE).contains(&o) || (o < 128 && t == 0) || t > 15 || b >= 64 {
            return false;
        }
    }
    true
}

pub fn probe_669(data: &[u8]) -> bool {
    data.len() >= 497 && header_669_ok(&data[..497])
}

pub fn read_669(data: &[u8]) -> Option<Module> {
    let mut file = Reader::new(data);
    let h = file.read_slice(497)?;
    if !header_669_ok(h) {
        return None;
    }
    let num_samples = h[110] as usize;
    let num_patterns = h[111] as usize;
    let restart = h[112] as usize;
    if !file.can_read(num_samples * 25 + num_patterns * 1536) {
        return None;
    }
    let mut m = Module::new(MOD_TYPE_669, 8);
    m.min_period = 28 << 2;
    m.max_period = 1712 << 3;
    m.default_tempo = Tempo::new(78, 0);
    m.default_speed = 4;
    m.play_behaviour[kPeriodsAreHertz] = true;
    m.song_flags |= SONG_FASTPORTAS | SONG_AUTO_TONEPORTA;
    let is_extended = &h[..2] == b"JN";
    m.format_name = "Composer 669".into();
    m.num_samples = num_samples as SampleIndex;
    m.samples.resize_with(num_samples + 1, || ModSample::new(MOD_TYPE_669));
    for smp in 1..=num_samples {
        let sb = file.read_array::<25>();
        let length = le32(&sb, 13);
        if length >= 0x400_0000 {
            return None;
        }
        let s = &mut m.samples[smp];
        s.initialize(MOD_TYPE_NONE);
        s.n_c5_speed = 8363;
        s.n_length = length;
        s.n_loop_start = le32(&sb, 17);
        s.n_loop_end = le32(&sb, 21);
        if s.n_loop_end > s.n_length && s.n_loop_start == 0 {
            s.n_loop_end = 0;
        } else if s.n_loop_end != 0 {
            s.u_flags = CHN_LOOP;
            s.sanitize_loops();
        }
        s.name = read_name(&sb[..13], false);
    }
    m.title = read_name(&h[2..38], true);
    m.order = read_orders(&h[113..241], 0xFF, 0xFE);
    if m.order[restart] < num_patterns as u16 {
        m.restart_pos = restart as OrderIndex;
    }
    for chn in 0..8 {
        m.chn_settings[chn].n_pan = if chn & 1 != 0 { 0xD0 } else { 0x30 };
    }
    empty_patterns(&mut m, num_patterns, 64);
    const EFF: [u8; 8] = [CMD_AUTO_PORTAUP, CMD_AUTO_PORTADOWN, CMD_TONEPORTAMENTO, CMD_S3MCMDEX, CMD_VIBRATO, CMD_SPEED, CMD_PANNINGSLIDE, CMD_RETRIG];
    for pat in 0..num_patterns {
        let mut effect = [0xFFu8; 8];
        for row in 0..64usize {
            for chn in 0..8usize {
                let [note_instr, instr_vol, eff_param] = file.read_array::<3>();
                let mc = &mut m.patterns[pat].data[row * 8 + chn];
                let note = note_instr >> 2;
                let instr = ((note_instr & 0x03) << 4) | (instr_vol >> 4);
                let vol = instr_vol & 0x0F;
                if note_instr < 0xFE {
                    mc.note = note + 36 + NOTE_MIN;
                    mc.instr = instr + 1;
                    effect[chn] = 0xFF;
                }
                if note_instr <= 0xFE {
                    mc.volcmd = VOLCMD_VOLUME;
                    mc.vol = ((vol as u32 * 64 + 8) / 15) as u8;
                }
                if eff_param != 0xFF {
                    effect[chn] = eff_param;
                }
                if effect[chn] == 0xFF {
                    continue;
                }
                let command = effect[chn] >> 4;
                if (command as usize) < EFF.len() {
                    mc.command = EFF[command as usize];
                    mc.param = effect[chn] & 0x0F;
                } else {
                    mc.command = CMD_NONE;
                    continue;
                }
                if mc.command != CMD_PANNINGSLIDE {
                    effect[chn] = 0xFF;
                }
                match command {
                    3 => mc.param |= 0x20,
                    4 => mc.param |= mc.param << 4,
                    6 => match mc.param {
                        0 => mc.param = 0x4F,
                        1 => mc.param = 0xF4,
                        _ => mc.command = CMD_NONE,
                    },
                    7 => {
                        if !mc.is_note() || !is_extended {
                            mc.command = CMD_NONE;
                        }
                    }
                    _ => {}
                }
            }
        }
        let brk = h[369 + pat];
        if brk < 63 {
            write_effect(&mut m.patterns[pat], 8, brk as u32, CMD_PATTERNBREAK, 0, false, Retry::NextRow, true);
        }
        write_effect(&mut m.patterns[pat], 8, 0, CMD_SPEED, h[241 + pat], false, Retry::NextRow, true);
    }
    for smp in 1..=num_samples {
        SampleIo::new(8, Channels::Mono, false, Encoding::Unsigned).read_sample(&mut m.samples[smp], &mut file);
    }
    Some(m)
}

// ---------------------------------------------------------------- ULT

fn ult_header_ok(h: &[u8]) -> bool {
    (b'1'..=b'4').contains(&h[14]) && &h[..14] == b"MAS_UTrack_V00"
}

pub fn probe_ult(data: &[u8]) -> bool {
    data.len() >= 48 && ult_header_ok(&data[..48])
}

/// `TranslateULTCommands`.
fn translate_ult(e: u8, mut param: u8, version: u8) -> (u8, u8) {
    const T: [u8; 16] = [
        CMD_ARPEGGIO,
        CMD_PORTAMENTOUP,
        CMD_PORTAMENTODOWN,
        CMD_TONEPORTAMENTO,
        CMD_VIBRATO,
        CMD_NONE,
        CMD_NONE,
        CMD_TREMOLO,
        CMD_NONE,
        CMD_OFFSET,
        CMD_VOLUMESLIDE,
        CMD_PANNING8,
        CMD_VOLUME8,
        CMD_PATTERNBREAK,
        CMD_NONE,
        CMD_SPEED,
    ];
    let mut effect = T[(e & 0x0F) as usize];
    match e & 0x0F {
        0x00 => {
            if param == 0 || version < b'3' {
                effect = CMD_NONE;
            }
        }
        0x05 => {
            if (param & 0x0F) == 0x02 || (param & 0xF0) == 0x20 {
                effect = CMD_S3MCMDEX;
                param = 0x9F;
            }
            if ((param & 0x0F) == 0x0C || (param & 0xF0) == 0xC0) && version >= b'3' {
                effect = CMD_KEYOFF;
                param = 0;
            }
        }
        0x07 => {
            if version < b'4' {
                effect = CMD_NONE;
            }
        }
        0x0A => {
            if param & 0xF0 != 0 {
                param &= 0xF0;
            }
        }
        0x0B => param = (param & 0x0F) * 0x11,
        0x0D => param = 10u8.wrapping_mul(param >> 4).wrapping_add(param & 0x0F),
        0x0E => match param >> 4 {
            0x01 => {
                effect = CMD_PORTAMENTOUP;
                param = 0xF0 | (param & 0x0F);
            }
            0x02 => {
                effect = CMD_PORTAMENTODOWN;
                param = 0xF0 | (param & 0x0F);
            }
            0x08 => {
                if version >= b'4' {
                    effect = CMD_S3MCMDEX;
                    param = 0x60 | (param & 0x0F);
                }
            }
            0x09 => {
                effect = CMD_RETRIG;
                param &= 0x0F;
            }
            0x0A => {
                effect = CMD_VOLUMESLIDE;
                param = ((param & 0x0F) << 4) | 0x0F;
            }
            0x0B => {
                effect = CMD_VOLUMESLIDE;
                param = 0xF0 | (param & 0x0F);
            }
            0x0C | 0x0D => effect = CMD_S3MCMDEX,
            _ => {}
        },
        0x0F => {
            if param > 0x2F {
                effect = CMD_TEMPO;
            }
        }
        _ => {}
    }
    (effect, param)
}

/// `ReadULTEvent`: (repeat, lost command, lost param).
fn read_ult_event(m: &mut ModCommand, file: &mut Reader, version: u8) -> (u8, u8, u8) {
    let mut repeat = 1u8;
    let mut b = file.u8();
    if b == 0xFC {
        repeat = file.u8();
        b = file.u8();
    }
    m.note = if b > 0 && b < 97 { b + 23 + NOTE_MIN } else { NOTE_NONE };
    let [instr, cmd, para1, para2] = file.read_array::<4>();
    m.instr = instr;
    let (mut c1, mut p1) = translate_ult(cmd & 0x0F, para1, version);
    let (mut c2, mut p2) = translate_ult(cmd >> 4, para2, version);
    if c1 == CMD_OFFSET && c2 == CMD_OFFSET {
        let offset = (((p2 as u32) << 8) | p1 as u32) >> 6;
        m.command = CMD_OFFSET;
        m.param = offset as u8;
        if offset > 0xFF {
            m.volcmd = VOLCMD_OFFSET;
            m.vol = (offset >> 8) as u8;
        }
        return (repeat, CMD_NONE, 0);
    } else if c1 == CMD_OFFSET {
        let offset = p1 as u32 * 4;
        p1 = offset.min(255) as u8;
        if offset > 0xFF && effect_weight(c2) < effect_weight(CMD_OFFSET) {
            m.command = CMD_OFFSET;
            m.param = offset as u8;
            m.volcmd = VOLCMD_OFFSET;
            m.vol = (offset >> 8) as u8;
            return (repeat, CMD_NONE, 0);
        }
    } else if c2 == CMD_OFFSET {
        let offset = p2 as u32 * 4;
        p2 = offset.min(255) as u8;
        if offset > 0xFF && effect_weight(c1) < effect_weight(CMD_OFFSET) {
            m.command = CMD_OFFSET;
            m.param = offset as u8;
            m.volcmd = VOLCMD_OFFSET;
            m.vol = (offset >> 8) as u8;
            return (repeat, CMD_NONE, 0);
        }
    } else if c1 == c2 {
        c2 = CMD_NONE;
    }
    if c2 == CMD_VOLUME || (c2 == CMD_NONE && c1 != CMD_VOLUME) {
        std::mem::swap(&mut c1, &mut c2);
        std::mem::swap(&mut p1, &mut p2);
    }
    combine_effects(&mut c2, &mut p2, &mut c1, &mut p1);
    let lost = fill_in_two_commands(m, c1, p1, c2, p2);
    (repeat, lost.0, lost.1)
}

pub fn read_ult(data: &[u8]) -> Option<Module> {
    let mut file = Reader::new(data);
    let h = file.read_slice(48)?;
    if !ult_header_ok(h) {
        return None;
    }
    let version = h[14];
    let message_length = h[47] as usize;
    if !file.can_read(message_length * 32 + 3 + 256) {
        return None;
    }
    let mut m = Module::new(MOD_TYPE_ULT, 0);
    m.title = read_name(&h[15..47], true);
    m.format_name = "UltraTracker".into();
    m.song_flags = SONG_AUTO_TONEPORTA | SONG_AUTO_TONEPORTA_CONT | SONG_ITCOMPATGXX | SONG_ITOLDEFFECTS;
    m.play_behaviour[kITClearPortaTarget] = false;
    m.play_behaviour[kITPortaTargetReached] = false;
    m.play_behaviour[kITNoSustainOnPortamento] = false;
    m.play_behaviour[kFT2PortaTargetNoReset] = true;
    file.skip(message_length * 32);
    let num_samples = file.u8() as usize;
    m.num_samples = num_samples as SampleIndex;
    m.samples.resize_with(num_samples + 1, || ModSample::new(MOD_TYPE_ULT));
    for smp in 1..=num_samples {
        let mut sb = [0u8; 66];
        if version >= b'4' {
            sb = file.read_array::<66>();
        } else {
            let part = file.read_array::<64>();
            sb[..64].copy_from_slice(&part);
            sb[64] = sb[62];
            sb[65] = sb[63];
            sb[62..64].copy_from_slice(&8363u16.to_le_bytes());
        }
        let s = &mut m.samples[smp];
        s.initialize(MOD_TYPE_NONE);
        for i in 0..9 {
            s.cues[i] = ((i as u32) + 1) << 16;
        }
        s.name = read_name(&sb[..32], true);
        let loop_start = le32(&sb, 44);
        let loop_end = le32(&sb, 48);
        let size_start = le32(&sb, 52);
        let size_end = le32(&sb, 56);
        if size_end <= size_start {
            continue;
        }
        s.n_length = size_end - size_start;
        s.n_sustain_start = loop_start;
        s.n_sustain_end = loop_end;
        s.n_volume = sb[60] as u16;
        s.n_c5_speed = le16(&sb, 62) as u32 * 2;
        let finetune = le16(&sb, 64) as i16;
        if finetune != 0 {
            let f = (s.n_c5_speed as f64 * 2.0f64.powf(finetune as f64 / (12.0 * 32768.0))).round();
            s.n_c5_speed = f.clamp(0.0, u32::MAX as f64) as u32;
        }
        let flags = sb[61];
        if flags & 8 != 0 {
            s.u_flags |= CHN_SUSTAINLOOP;
        }
        if flags & 16 != 0 {
            s.u_flags |= CHN_PINGPONGSUSTAIN;
        }
        if flags & 4 != 0 {
            s.u_flags |= CHN_16BIT;
            s.n_sustain_start /= 2;
            s.n_sustain_end /= 2;
        }
    }
    let orders = file.read_slice(256)?;
    m.order = read_orders(orders, 0xFF, 0xFE);
    let num_channels = file.u8() as usize + 1;
    if num_channels > MAX_BASECHANNELS {
        return None;
    }
    m.chn_settings.resize(num_channels, Default::default());
    let num_pats = file.u8() as usize + 1;
    for chn in 0..num_channels {
        m.chn_settings[chn].n_pan = if version >= b'3' { (((file.u8() & 0x0F) as u16) << 4) + 8 } else if chn & 1 != 0 { 192 } else { 64 };
    }
    empty_patterns(&mut m, num_pats, 64);
    let nc = num_channels;
    let mut post_fix_speed = false;
    for chn in 0..nc {
        let mut ev = ModCommand::default();
        let mut pat = 0;
        while pat < num_pats && file.can_read(5) {
            let mut row = 0u32;
            while row < 64 {
                let (rep, lost_cmd, lost_param) = read_ult_event(&mut ev, &mut file, version);
                if lost_cmd != CMD_NONE && is_global_command(lost_cmd, lost_param) {
                    write_effect(&mut m.patterns[pat], nc, row, lost_cmd, lost_param, false, Retry::NextRow, false);
                }
                let mut repeat = rep as u32;
                if repeat + row > 64 {
                    repeat = 64 - row;
                }
                if repeat == 0 {
                    break;
                }
                if ev.command == CMD_SPEED && ev.param == 0 {
                    post_fix_speed = true;
                }
                for _ in 0..repeat {
                    m.patterns[pat].data[row as usize * nc + chn] = ev;
                    row += 1;
                }
            }
            pat += 1;
        }
    }
    if post_fix_speed {
        for p in m.patterns.iter_mut() {
            for row in 0..p.rows {
                for c in 0..nc {
                    let mc = &mut p.data[row as usize * nc + c];
                    if mc.command == CMD_SPEED && mc.param == 0 {
                        mc.param = 6;
                        write_effect(p, nc, row, CMD_TEMPO, 125, false, Retry::NextRow, false);
                    }
                }
            }
        }
    }
    for smp in 1..=num_samples {
        let bits = if m.samples[smp].u_flags & CHN_16BIT != 0 { 16 } else { 8 };
        SampleIo::new(bits, Channels::Mono, false, Encoding::Signed).read_sample(&mut m.samples[smp], &mut file);
    }
    Some(m)
}

// ---------------------------------------------------------------- STM

fn stm_header_ok(h: &[u8]) -> bool {
    let (dos_eof, filetype, ver_major, ver_minor, num_patterns, global_volume) = (h[28], h[29], h[30], h[31], h[33], h[34]);
    if filetype != 2
        || (dos_eof != 0x1A && dos_eof != 2)
        || ver_major != 2
        || !matches!(ver_minor, 0 | 10 | 20 | 21)
        || num_patterns > 64
        || (global_volume > 64 && global_volume != 0x58)
    {
        return false;
    }
    h[20..28].iter().all(|&c| (0x20..0x7F).contains(&c))
}

pub fn probe_stm(data: &[u8]) -> bool {
    data.len() >= 48 && stm_header_ok(&data[..48])
}

pub fn read_stm(data: &[u8]) -> Option<Module> {
    let mut file = Reader::new(data);
    let h = file.read_slice(48)?;
    if !stm_header_ok(h) {
        return None;
    }
    let ver_minor = h[31];
    let num_patterns = h[33] as usize;
    if !file.can_read(31 * 32 + if ver_minor == 0 { 64 } else { 128 } + num_patterns * 64 * 4) {
        return None;
    }
    let mut m = Module::new(MOD_TYPE_STM, 4);
    m.title = read_name(&h[..20], false);
    m.format_name = "Scream Tracker 2".into();
    m.play_behaviour[kST3SampleSwap] = true;
    m.num_samples = 31;
    m.samples.resize_with(32, || ModSample::new(MOD_TYPE_STM));
    m.min_period = 64;
    m.max_period = 0x7FFF;
    let mut init_tempo = h[32];
    if ver_minor < 21 {
        init_tempo = ((init_tempo / 10) << 4) + init_tempo % 10;
    }
    if init_tempo == 0 {
        init_tempo = 0x60;
    }
    m.default_tempo = crate::player::convert_st2_tempo(init_tempo);
    m.default_speed = (init_tempo >> 4) as u32;
    if ver_minor > 10 {
        m.default_global_volume = h[34].min(64) as u32 * 4;
    }
    let mut sample_offsets = [0u16; 31];
    for smp in 1..=31usize {
        let sb = file.read_array::<32>();
        if sb[12] != 0 && sb[12] != 46 {
            return None;
        }
        let s = &mut m.samples[smp];
        s.initialize(MOD_TYPE_NONE);
        s.name = read_name(&sb[..12], false);
        s.n_c5_speed = le16(&sb, 24) as u32;
        s.n_volume = sb[22].min(64) as u16 * 4;
        s.n_length = le16(&sb, 16) as u32;
        s.n_loop_start = le16(&sb, 18) as u32;
        s.n_loop_end = le16(&sb, 20) as u32;
        if s.n_length < 2 {
            s.n_length = 0;
        }
        if s.n_loop_start < s.n_length && s.n_loop_end > s.n_loop_start && s.n_loop_end != 0xFFFF {
            s.u_flags = CHN_LOOP;
            s.n_length = s.n_loop_end.max(s.n_length);
        }
        sample_offsets[smp - 1] = le16(&sb, 14);
    }
    let order_len = if ver_minor == 0 { 64 } else { 128 };
    let orders = file.read_slice(order_len)?;
    let mut order = Vec::with_capacity(order_len);
    for &p in orders {
        if p == 99 || p == 255 {
            order.push(PATTERNINDEX_INVALID);
        } else if p > 63 {
            return None;
        } else {
            order.push(p as PatternIndex);
        }
    }
    m.order = order;
    empty_patterns(&mut m, num_patterns, 64);
    for pat in 0..num_patterns {
        let mut break_pos = ORDERINDEX_INVALID;
        let mut break_row: u32 = 63;
        for row in 0..64u32 {
            for chn in 0..4usize {
                let mc = &mut m.patterns[pat].data[row as usize * 4 + chn];
                let mut note = file.u8();
                let (ins_vol, vol_cmd, cmd_inf);
                match note {
                    0xFB => {
                        note = 0;
                        ins_vol = 0;
                        vol_cmd = 0;
                        cmd_inf = 0;
                    }
                    0xFC => continue,
                    0xFD => {
                        mc.note = NOTE_NOTECUT;
                        continue;
                    }
                    _ => {
                        let d = file.read_array::<3>();
                        ins_vol = d[0];
                        vol_cmd = d[1];
                        cmd_inf = d[2];
                    }
                }
                if note == 0xFE {
                    mc.note = NOTE_NOTECUT;
                } else if note < 0x60 {
                    mc.note = (note >> 4) * 12 + (note & 0x0F) + 36 + NOTE_MIN;
                }
                mc.instr = ins_vol >> 3;
                if mc.instr > 31 {
                    mc.instr = 0;
                }
                let vol = (ins_vol & 0x07) | ((vol_cmd & 0xF0) >> 1);
                if vol <= 64 {
                    mc.volcmd = VOLCMD_VOLUME;
                    mc.vol = vol;
                }
                mc.param = cmd_inf;
                convert_stm_command(mc, vol_cmd & 0x0F, row, ver_minor, &mut break_pos, &mut break_row);
            }
        }
        if break_pos != ORDERINDEX_INVALID {
            write_effect(&mut m.patterns[pat], 4, break_row, CMD_POSITIONJUMP, break_pos as u8, false, Retry::PreviousRow, true);
        }
    }
    for smp in 1..=31usize {
        let s = &mut m.samples[smp];
        if s.n_length != 0 && s.n_volume > 0 {
            let off = (sample_offsets[smp - 1] as usize) << 4;
            if off > 48 && file.seek(off) {
                SampleIo::new(8, Channels::Mono, false, Encoding::Signed).read_sample(s, &mut file);
            }
        }
    }
    Some(m)
}

/// `ConvertSTMCommand`.
fn convert_stm_command(m: &mut ModCommand, command: u8, row: u32, ver_minor: u8, break_pos: &mut OrderIndex, break_row: &mut u32) {
    const EFF: [u8; 16] = [
        CMD_NONE,
        CMD_SPEED,
        CMD_POSITIONJUMP,
        CMD_PATTERNBREAK,
        CMD_VOLUMESLIDE,
        CMD_PORTAMENTODOWN,
        CMD_PORTAMENTOUP,
        CMD_TONEPORTAMENTO,
        CMD_VIBRATO,
        CMD_TREMOR,
        CMD_ARPEGGIO,
        CMD_NONE,
        CMD_NONE,
        CMD_NONE,
        CMD_NONE,
        CMD_NONE,
    ];
    m.command = EFF[(command & 0x0F) as usize];
    match m.command {
        CMD_VOLUMESLIDE => {
            if m.param & 0x0F != 0 {
                m.param &= 0x0F;
            } else {
                m.param &= 0xF0;
            }
        }
        CMD_PATTERNBREAK => {
            m.param = ((m.param & 0xF0) as u32 * 10 + (m.param & 0x0F) as u32) as u8;
            if *break_pos != ORDERINDEX_INVALID && m.param == 0 {
                m.command = CMD_POSITIONJUMP;
                m.param = *break_pos as u8;
                *break_pos = ORDERINDEX_INVALID;
            }
            *break_row = (*break_row).min(row);
        }
        CMD_POSITIONJUMP => {
            *break_pos = m.param as OrderIndex;
            *break_row = 63;
            m.command = CMD_NONE;
        }
        CMD_TREMOR => {}
        CMD_SPEED => {
            if ver_minor < 21 {
                m.param = ((m.param / 10) << 4) + m.param % 10;
            }
            if m.param == 0 {
                m.command = CMD_NONE;
            }
        }
        _ => {
            if m.param == 0 {
                m.command = CMD_NONE;
            }
        }
    }
}
