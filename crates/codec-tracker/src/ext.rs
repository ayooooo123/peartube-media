//! OpenMPT's extension chunks in IT and XM files, ported from libopenmpt
//! 0.8.9 `soundlib/Load_it.cpp` (`LoadMixPlugins`,
//! `LoadExtendedSongProperties`) and `InstrumentExtensions.cpp`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

use crate::defs::*;
use crate::instrument::ModInstrument;
use crate::io::Reader;
use crate::sndfile::{MixLevels, Module, TempoMode};

pub const fn magic_be(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | s[3] as u32
}
pub const fn magic_le(s: &[u8; 4]) -> u32 {
    (s[0] as u32) | ((s[1] as u32) << 8) | ((s[2] as u32) << 16) | ((s[3] as u32) << 24)
}

/// `ReadSizedIntLE`: `size` bytes little-endian into a value of `width`
/// bytes, sign-extended when `signed` and shorter, truncated when longer.
fn sized_int(chunk: &mut Reader, size: usize, width: usize, signed: bool) -> i64 {
    if size == 0 || !chunk.can_read(size) {
        return 0;
    }
    let n = size.min(width);
    let mut v: u64 = 0;
    for i in 0..n {
        v |= (chunk.u8() as u64) << (8 * i);
    }
    chunk.skip(size - n);
    let bits = 8 * width as u32;
    if size < width && signed && n > 0 && (v >> (8 * n - 1)) & 1 != 0 {
        v |= !0u64 << (8 * n);
    }
    let masked = if bits >= 64 { v } else { v & ((1u64 << bits) - 1) };
    if signed && bits < 64 && (masked >> (bits - 1)) & 1 != 0 {
        (masked | (!0u64 << bits)) as i64
    } else {
        masked as i64
    }
}

/// `LoadMixPlugins`: skips plugin chunks (there are no plugins here);
/// returns (has plugin chunks, is BeRoTracker).
pub fn load_mix_plugins(file: &mut Reader, m: &mut Module, ignore_channel_count: bool) -> (bool, bool) {
    let mut has_plugin_chunks = false;
    let mut is_bero = false;
    while file.can_read(9) {
        let code = file.read_array::<4>();
        let size = file.u32le() as usize;
        if &code == b"IMPI" || &code == b"IMPS" || &code == b"XTPM" || &code == b"STPM" || !file.can_read(size) {
            file.pos -= 8;
            return (has_plugin_chunks, is_bero);
        }
        let mut chunk = file.read_chunk(size);
        if &code == b"CHFX" {
            if !ignore_channel_count {
                let n = (size / 4).clamp(m.num_channels(), MAX_BASECHANNELS);
                m.chn_settings.resize(n, Default::default());
            }
            for cs in m.chn_settings.iter_mut() {
                cs.n_mix_plugin = chunk.u32le() as u8;
            }
            has_plugin_chunks = true;
        } else if code[0] == b'F' && (code[1] == b'X' || code[1].is_ascii_digit()) && code[2].is_ascii_digit() && code[3].is_ascii_digit() {
            has_plugin_chunks = true;
        } else if &code == b"MODU" {
            is_bero = true;
            m.last_saved_with_version = 0;
        }
    }
    (has_plugin_chunks, is_bero)
}

fn read_instrument_field(ins: &mut ModInstrument, code: u32, chunk: &mut Reader) {
    let size = chunk.len();
    macro_rules! int {
        ($w:expr, $s:expr) => {
            sized_int(chunk, size, $w, $s)
        };
    }
    let env_flags = |env: &mut crate::instrument::InstrumentEnvelope, flags: u32, pitch: bool| {
        let mut f = 0u8;
        if flags & 0x01 != 0 {
            f |= ENV_ENABLED;
        }
        if flags & 0x02 != 0 {
            f |= ENV_LOOP;
        }
        if flags & 0x04 != 0 {
            f |= ENV_SUSTAIN;
        }
        if flags & 0x08 != 0 {
            f |= ENV_CARRY;
        }
        if pitch && flags & 0x10 != 0 {
            f |= ENV_FILTER;
        }
        env.flags = (env.flags & !(ENV_ENABLED | ENV_LOOP | ENV_SUSTAIN | ENV_CARRY | ENV_FILTER)) | f;
    };
    let set = |flags: &mut u8, bit: u8, on: bool| {
        if on {
            *flags |= bit;
        } else {
            *flags &= !bit;
        }
    };
    match code {
        c if c == magic_be(b"FO..") => ins.n_fade_out = int!(4, false) as u32,
        c if c == magic_be(b"GV..") => ins.n_global_vol = int!(4, false) as u32,
        c if c == magic_be(b"P...") => ins.n_pan = int!(4, false) as u32,
        c if c == magic_be(b"VLS.") => ins.vol_env.loop_start = int!(1, false) as u8,
        c if c == magic_be(b"VLE.") => ins.vol_env.loop_end = int!(1, false) as u8,
        c if c == magic_be(b"VSB.") => ins.vol_env.sustain_start = int!(1, false) as u8,
        c if c == magic_be(b"VSE.") => ins.vol_env.sustain_end = int!(1, false) as u8,
        c if c == magic_be(b"PLS.") => ins.pan_env.loop_start = int!(1, false) as u8,
        c if c == magic_be(b"PLE.") => ins.pan_env.loop_end = int!(1, false) as u8,
        c if c == magic_be(b"PSB.") => ins.pan_env.sustain_start = int!(1, false) as u8,
        c if c == magic_be(b"PSE.") => ins.pan_env.sustain_end = int!(1, false) as u8,
        c if c == magic_be(b"PiLS") => ins.pitch_env.loop_start = int!(1, false) as u8,
        c if c == magic_be(b"PiLE") => ins.pitch_env.loop_end = int!(1, false) as u8,
        c if c == magic_be(b"PiSB") => ins.pitch_env.sustain_start = int!(1, false) as u8,
        c if c == magic_be(b"PiSE") => ins.pitch_env.sustain_end = int!(1, false) as u8,
        c if c == magic_be(b"NNA.") => {
            ins.nna = match int!(1, false) as u8 {
                0 => NewNoteAction::NoteCut,
                1 => NewNoteAction::Continue,
                2 => NewNoteAction::NoteOff,
                3 => NewNoteAction::NoteFade,
                _ => NewNoteAction::NoteCut,
            }
        }
        c if c == magic_be(b"DCT.") => {
            ins.dct = match int!(1, false) as u8 {
                1 => DuplicateCheckType::Note,
                2 => DuplicateCheckType::Sample,
                3 => DuplicateCheckType::Instrument,
                4 => DuplicateCheckType::Plugin,
                _ => DuplicateCheckType::None,
            }
        }
        c if c == magic_be(b"DNA.") => {
            ins.dna = match int!(1, false) as u8 {
                1 => DuplicateNoteAction::NoteOff,
                2 => DuplicateNoteAction::NoteFade,
                _ => DuplicateNoteAction::NoteCut,
            }
        }
        c if c == magic_be(b"PS..") => ins.n_pan_swing = int!(1, false) as u8,
        c if c == magic_be(b"VS..") => ins.n_vol_swing = int!(1, false) as u8,
        c if c == magic_be(b"IFC.") => ins.n_ifc = int!(1, false) as u8,
        c if c == magic_be(b"IFR.") => ins.n_ifr = int!(1, false) as u8,
        c if c == magic_be(b"MC..") => ins.n_midi_channel = int!(1, false) as u8,
        c if c == magic_be(b"PPS.") => ins.n_pps = int!(1, true) as i8,
        c if c == magic_be(b"PPC.") => ins.n_ppc = int!(1, false) as u8,
        c if c == magic_be(b"VP[.") || c == magic_be(b"PP[.") || c == magic_be(b"PiP[") => {
            let env = if c == magic_be(b"VP[.") {
                &mut ins.vol_env
            } else if c == magic_be(b"PP[.") {
                &mut ins.pan_env
            } else {
                &mut ins.pitch_env
            };
            let points = env.nodes.len().min(size / 2);
            for i in 0..points {
                env.nodes[i].tick = chunk.u16le();
            }
        }
        c if c == magic_be(b"VE[.") || c == magic_be(b"PE[.") || c == magic_be(b"PiE[") => {
            let env = if c == magic_be(b"VE[.") {
                &mut ins.vol_env
            } else if c == magic_be(b"PE[.") {
                &mut ins.pan_env
            } else {
                &mut ins.pitch_env
            };
            let points = env.nodes.len().min(size);
            for i in 0..points {
                env.nodes[i].value = chunk.u8();
            }
        }
        c if c == magic_be(b"MiP.") => ins.n_mix_plug = int!(1, false) as u8,
        c if c == magic_be(b"VR..") => ins.n_vol_ramp_up = int!(2, false) as u16,
        c if c == magic_be(b"CS..") => ins.n_cut_swing = int!(1, false) as u8,
        c if c == magic_be(b"RS..") => ins.n_res_swing = int!(1, false) as u8,
        c if c == magic_be(b"FM..") => {
            ins.filter_mode = match int!(1, false) as u8 {
                0 => FilterMode::LowPass,
                1 => FilterMode::HighPass,
                _ => FilterMode::Unchanged,
            }
        }
        c if c == magic_be(b"PERN") => ins.pitch_env.release_node = int!(1, false) as u8,
        c if c == magic_be(b"AERN") => ins.pan_env.release_node = int!(1, false) as u8,
        c if c == magic_be(b"VERN") => ins.vol_env.release_node = int!(1, false) as u8,
        c if c == magic_be(b"MPWD") => ins.midi_pwd = int!(1, true) as i8,
        c if c == magic_be(b"dF..") => {
            let flags = int!(4, false) as u32;
            let v = &mut ins.vol_env.flags;
            set(v, ENV_ENABLED, flags & 0x0001 != 0);
            set(v, ENV_SUSTAIN, flags & 0x0002 != 0);
            set(v, ENV_LOOP, flags & 0x0004 != 0);
            set(v, ENV_CARRY, flags & 0x0800 != 0);
            let p = &mut ins.pan_env.flags;
            set(p, ENV_ENABLED, flags & 0x0008 != 0);
            set(p, ENV_SUSTAIN, flags & 0x0010 != 0);
            set(p, ENV_LOOP, flags & 0x0020 != 0);
            set(p, ENV_CARRY, flags & 0x1000 != 0);
            let pi = &mut ins.pitch_env.flags;
            set(pi, ENV_ENABLED, flags & 0x0040 != 0);
            set(pi, ENV_SUSTAIN, flags & 0x0080 != 0);
            set(pi, ENV_LOOP, flags & 0x0100 != 0);
            set(pi, ENV_CARRY, flags & 0x2000 != 0);
            set(pi, ENV_FILTER, flags & 0x0400 != 0);
            set(&mut ins.flags, INS_SETPANNING, flags & 0x0200 != 0);
            set(&mut ins.flags, INS_MUTE, flags & 0x4000 != 0);
        }
        c if c == magic_be(b"VFLG") => {
            let f = int!(4, false) as u32;
            env_flags(&mut ins.vol_env, f, false);
        }
        c if c == magic_be(b"AFLG") => {
            let f = int!(4, false) as u32;
            env_flags(&mut ins.pan_env, f, false);
        }
        c if c == magic_be(b"PFLG") => {
            let f = int!(4, false) as u32;
            env_flags(&mut ins.pitch_env, f, true);
        }
        c if c == magic_be(b"NM[.") => {
            for i in 0..size.min(128) {
                ins.note_map[i] = chunk.u8();
            }
        }
        c if c == magic_be(b"R...") => {
            let r = int!(4, false) as u32;
            if r < SRCMODE_DEFAULT as u32 {
                ins.resampling = r as u8;
            }
        }
        c if c == magic_be(b"VE..") => {
            let n = (int!(4, false) as u32).min(MAX_ENVPOINTS as u32) as usize;
            ins.vol_env.nodes.resize(n, Default::default());
        }
        c if c == magic_be(b"PE..") => {
            let n = (int!(4, false) as u32).min(MAX_ENVPOINTS as u32) as usize;
            ins.pan_env.nodes.resize(n, Default::default());
        }
        c if c == magic_be(b"PiE.") => {
            let n = (int!(4, false) as u32).min(MAX_ENVPOINTS as u32) as usize;
            ins.pitch_env.nodes.resize(n, Default::default());
        }
        _ => {}
    }
}

/// `LoadExtendedInstrumentProperties` for instruments 1..=num_instruments.
pub fn load_extended_instrument_properties(file: &mut Reader, m: &mut Module) -> bool {
    if !file.read_magic(b"XTPM") {
        return false;
    }
    let n = m.num_instruments as usize;
    while file.can_read(6) {
        let code = file.u32le();
        if code == magic_be(b"MPTS") || code == magic_le(b"228\x04") || (code & 0x8080_8080) != 0 || (code & 0x6060_6060) == 0 {
            file.pos -= 4;
            break;
        }
        let size = file.u16le() as usize;
        for i in 1..=n {
            let mut chunk = file.read_chunk(size);
            if chunk.len() == size {
                if let Some(Some(ins)) = m.instruments.get_mut(i) {
                    read_instrument_field(ins, code, &mut chunk);
                }
            }
        }
    }
    true
}

/// `LoadExtendedSongProperties`.
pub fn load_extended_song_properties(file: &mut Reader, m: &mut Module, ignore_channel_count: bool) -> bool {
    if !file.read_magic(b"STPM") {
        return false;
    }
    m.play_behaviour = [false; crate::defs::pb::kMaxPlayBehaviours];
    let mut mix_levels: i64 = m.mix_levels as i64;
    let mut tempo_mode: i64 = m.tempo_mode as i64;
    while file.can_read(7) {
        let code = file.u32le();
        let size = file.u16le() as usize;
        if code == magic_le(b"228\x04") {
            file.pos -= 6;
            break;
        } else if (code & 0x8080_8080) != 0 || (code & 0x6060_6060) == 0 || !file.can_read(size) {
            break;
        }
        let mut chunk = file.read_chunk(size);
        let c = &mut chunk;
        match code {
            x if x == magic_be(b"DT..") => {
                let t = sized_int(c, size, 4, false) as u32;
                m.default_tempo = Tempo::new(t, m.default_tempo.fract());
            }
            x if x == magic_le(b"DTFR") => {
                let f = sized_int(c, size, 4, false) as u32;
                m.default_tempo = Tempo::new(m.default_tempo.int(), f);
            }
            x if x == magic_be(b"RPB.") => m.default_rows_per_beat = sized_int(c, size, 4, false) as u32,
            x if x == magic_be(b"RPM.") => m.default_rows_per_measure = sized_int(c, size, 4, false) as u32,
            x if x == magic_be(b"C...") => {
                if !ignore_channel_count {
                    let n = sized_int(c, size, 2, false) as usize;
                    let n = n.clamp(m.num_channels(), MAX_BASECHANNELS);
                    m.chn_settings.resize(n, Default::default());
                }
            }
            x if x == magic_be(b"TM..") => tempo_mode = sized_int(c, size, 4, true),
            x if x == magic_be(b"PMM.") => mix_levels = sized_int(c, size, 4, true),
            x if x == magic_be(b"CWV.") => m.created_with_version = sized_int(c, size, 4, false) as u32,
            x if x == magic_be(b"LSWV") => {
                let v = sized_int(c, size, 4, false) as u32;
                if v != 0 {
                    m.last_saved_with_version = v;
                }
            }
            x if x == magic_be(b"SPA.") => m.sample_pre_amp = sized_int(c, size, 4, false) as u32,
            x if x == magic_be(b"VSTV") => m.vsti_volume = sized_int(c, size, 4, false) as u32,
            x if x == magic_be(b"DGV.") => m.default_global_volume = sized_int(c, size, 4, false) as u32,
            x if x == magic_be(b"RP..") => {
                if m.mod_type != MOD_TYPE_XM {
                    m.restart_pos = sized_int(c, size, 2, false) as u16;
                }
            }
            x if x == magic_le(b"RSMP") => {
                let r = sized_int(c, size, 1, false) as u8;
                m.resampling = if r < SRCMODE_DEFAULT { r } else { SRCMODE_DEFAULT };
            }
            x if x == magic_be(b"ChnS") => {
                if size <= (MAX_BASECHANNELS - 64) * 2 && size % 2 == 0 && m.mod_type & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 {
                    let in_file = 64 + size / 2;
                    if !ignore_channel_count {
                        let n = m.num_channels().clamp(in_file, MAX_BASECHANNELS);
                        m.chn_settings.resize(n, Default::default());
                    }
                    let n = in_file.min(m.num_channels());
                    for chn in 64..n {
                        let [mut pan, vol] = c.read_array::<2>();
                        if pan != 0xFF {
                            let cs = &mut m.chn_settings[chn];
                            cs.n_volume = vol;
                            cs.n_pan = 128;
                            cs.dw_flags = 0;
                            if pan & 0x80 != 0 {
                                cs.dw_flags |= CHN_MUTE;
                            }
                            pan &= 0x7F;
                            if pan <= 64 {
                                cs.n_pan = (pan as u16) << 2;
                            }
                            if pan == 100 {
                                cs.dw_flags |= CHN_SURROUND;
                            }
                        }
                    }
                }
            }
            x if x == magic_le(b"CUES") => {
                if size > 2 {
                    let smp = c.u16le() as usize;
                    if smp > 0 && smp <= m.num_samples as usize {
                        for cue in m.samples[smp].cues.iter_mut() {
                            *cue = if c.can_read(4) { c.u32le() } else { MAX_SAMPLE_LENGTH };
                        }
                    }
                }
            }
            x if x == magic_be(b"MSF.") => {
                m.play_behaviour = [false; crate::defs::pb::kMaxPlayBehaviours];
                let mut bit = 0usize;
                while c.can_read(1) && bit < m.play_behaviour.len() {
                    let b = c.u8();
                    for i in 0..8 {
                        if b & (1 << i) != 0 && bit < m.play_behaviour.len() {
                            m.play_behaviour[bit] = true;
                        }
                        bit += 1;
                    }
                }
            }
            _ => {}
        }
    }
    let (tmin, tmax) = crate::player::spec_tempo(m.mod_type);
    m.default_tempo = m.default_tempo.clamp(Tempo::new(tmin, 0), Tempo::new(tmax, 0));
    m.tempo_mode = match tempo_mode as u8 {
        1 => TempoMode::Alternative,
        2 => TempoMode::Modern,
        _ => TempoMode::Classic,
    };
    let levels = match mix_levels as u8 {
        0 => MixLevels::Original,
        1 => MixLevels::V117RC1,
        2 => MixLevels::V117RC2,
        3 => MixLevels::V117RC3,
        4 => MixLevels::Compatible,
        5 => MixLevels::CompatibleFT2,
        _ => MixLevels::Original,
    };
    m.set_mix_levels(levels);
    true
}
