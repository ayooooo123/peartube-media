//! Impulse Tracker modules, ported from libopenmpt 0.8.9
//! `soundlib/Load_it.cpp` and `ITTools.cpp`. MPTM-only data (tunings,
//! external samples, OPL patches) is not read.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

use crate::command::*;
use crate::defs::pb::*;
use crate::defs::*;
use crate::instrument::{EnvelopeNode, InstrumentEnvelope, ModInstrument};
use crate::io::{Channels, Encoding, Reader, SampleIo, le16, le32, read_name};
use crate::load_s3m::s3m_convert;
use crate::load_xm::{MAX_PATTERN_CELLS, read_midi_config};
use crate::sample::ModSample;
use crate::sndfile::{MixLevels, Module, Pattern};
use crate::version::{mpt_v, schism_date, schism_epoch_date};

const AUTO_VIBRATO_IT2XM: [u8; 8] = [VIB_SINE, VIB_RAMP_DOWN, VIB_SQUARE, VIB_RANDOM, VIB_RAMP_UP, 0, 0, 0];

fn nna_from(v: u8) -> NewNoteAction {
    match v {
        1 => NewNoteAction::Continue,
        2 => NewNoteAction::NoteOff,
        3 => NewNoteAction::NoteFade,
        _ => NewNoteAction::NoteCut,
    }
}
fn dct_from(v: u8) -> DuplicateCheckType {
    match v {
        1 => DuplicateCheckType::Note,
        2 => DuplicateCheckType::Sample,
        3 => DuplicateCheckType::Instrument,
        4 => DuplicateCheckType::Plugin,
        _ => DuplicateCheckType::None,
    }
}
fn dna_from(v: u8) -> DuplicateNoteAction {
    match v {
        1 => DuplicateNoteAction::NoteOff,
        2 => DuplicateNoteAction::NoteFade,
        _ => DuplicateNoteAction::NoteCut,
    }
}

/// `ITEnvelope::ConvertToMPT` over the 82-byte envelope at `b`.
fn convert_envelope(b: &[u8], env: &mut InstrumentEnvelope, env_offset: u8, max_nodes: u8) {
    let flags = b[0];
    let num = b[1];
    let set = |f: &mut u8, bit: u8, on: bool| {
        if on {
            *f |= bit;
        } else {
            *f &= !bit;
        }
    };
    set(&mut env.flags, ENV_ENABLED, flags & 0x01 != 0);
    set(&mut env.flags, ENV_LOOP, flags & 0x02 != 0);
    set(&mut env.flags, ENV_SUSTAIN, flags & 0x04 != 0);
    set(&mut env.flags, ENV_CARRY, flags & 0x08 != 0);
    env.nodes = vec![EnvelopeNode::default(); num.min(max_nodes) as usize];
    env.loop_start = b[2].min(max_nodes);
    env.loop_end = b[3].clamp(env.loop_start, max_nodes);
    env.sustain_start = b[4].min(max_nodes);
    env.sustain_end = b[5].clamp(env.sustain_start, max_nodes);
    for ev in 0..(num.min(25) as usize).min(env.nodes.len()) {
        let o = 6 + ev * 3;
        let value = (b[o] as i8).wrapping_add(env_offset as i8);
        env.nodes[ev].value = value.clamp(0, 64) as u8;
        env.nodes[ev].tick = le16(b, o + 1);
        if ev > 0 && env.nodes[ev].tick < env.nodes[ev - 1].tick && env.nodes[ev].tick & 0xFF00 == 0 {
            env.nodes[ev].tick |= env.nodes[ev - 1].tick & 0xFF00;
            if env.nodes[ev].tick < env.nodes[ev - 1].tick {
                env.nodes[ev].tick = env.nodes[ev].tick.wrapping_add(0x100);
            }
        }
    }
}

/// `ITInstrToMPT`.
fn read_instrument(file: &mut Reader, ins: &mut ModInstrument, trkvers: u16, mod_type: u32) {
    if trkvers < 0x0200 {
        let Some(b) = file.read_slice(554) else {
            return;
        };
        if &b[..4] != b"IMPI" {
            return;
        }
        // ITOldInstrument: id[4] filename[13] flags vls vle sls sle
        // reserved1[2] fadeout nna dnc trkvers nos reserved2 name[26]
        // reserved3[6] keyboard[240] volenv[200] nodes[50].
        ins.name = read_name(&b[32..58], true);
        ins.n_fade_out = (le16(b, 0x18) as u32) << 6;
        ins.n_global_vol = 64;
        ins.n_pan = 128;
        ins.nna = nna_from(b[0x1A]);
        ins.dct = dct_from(b[0x1B]);
        for i in 0..120 {
            let note = b[64 + i * 2];
            let smp = b[64 + i * 2 + 1] as SampleIndex;
            if (smp as usize) < MAX_SAMPLES {
                ins.keyboard[i] = smp;
            }
            ins.note_map[i] = if note < 120 { note + 1 } else { i as u8 + 1 };
        }
        let flags = b[0x11];
        let set = |f: &mut u8, bit: u8, on: bool| {
            if on {
                *f |= bit;
            } else {
                *f &= !bit;
            }
        };
        set(&mut ins.vol_env.flags, ENV_ENABLED, flags & 0x01 != 0);
        set(&mut ins.vol_env.flags, ENV_LOOP, flags & 0x02 != 0);
        set(&mut ins.vol_env.flags, ENV_SUSTAIN, flags & 0x04 != 0);
        ins.vol_env.loop_start = b[0x12];
        ins.vol_env.loop_end = b[0x13];
        ins.vol_env.sustain_start = b[0x14];
        ins.vol_env.sustain_end = b[0x15];
        let _ = mod_type;
        // Node data
        let nodes = &b[504..554];
        ins.vol_env.nodes = vec![EnvelopeNode::default(); 25];
        for i in 0..25 {
            ins.vol_env.nodes[i].tick = nodes[i * 2] as u16;
            if nodes[i * 2] == 0xFF {
                ins.vol_env.nodes.truncate(i);
                break;
            }
            ins.vol_env.nodes[i].value = nodes[i * 2 + 1];
        }
        let n = ins.vol_env.nodes.len();
        if (ins.vol_env.loop_start.max(ins.vol_env.loop_end) as usize) >= n {
            ins.vol_env.flags &= !ENV_LOOP;
        }
        if (ins.vol_env.sustain_start.max(ins.vol_env.sustain_end) as usize) >= n {
            ins.vol_env.flags &= !ENV_SUSTAIN;
        }
        return;
    }
    let offset = file.pos;
    // ReadStructPartial(ITInstrumentEx): missing bytes are zero.
    let mut b = [0u8; 554 + 120];
    let avail = b.len().min(file.bytes_left());
    b[..avail].copy_from_slice(&file.data[offset..offset + avail]);
    if &b[..4] != b"IMPI" {
        return;
    }
    let mut size = 554;
    ins.name = read_name(&b[32..58], true);
    ins.n_fade_out = (le16(&b, 0x14) as u32) << 5;
    ins.n_global_vol = (b[0x18] as u32 / 2).min(64);
    let dfp = b[0x19];
    ins.n_pan = (dfp & 0x7F) as u32 * 4;
    if ins.n_pan > 256 {
        ins.n_pan = 128;
    }
    if dfp & 0x80 == 0 {
        ins.flags |= INS_SETPANNING;
    } else {
        ins.flags &= !INS_SETPANNING;
    }
    ins.n_vol_swing = b[0x1A].min(100);
    ins.n_pan_swing = b[0x1B].min(64);
    ins.nna = nna_from(b[0x11]);
    ins.dct = dct_from(b[0x12]);
    ins.dna = dna_from(b[0x13]);
    ins.n_pps = b[0x16] as i8;
    ins.n_ppc = b[0x17];
    let ifc = b[0x3A];
    let ifr = b[0x3B];
    ins.n_ifc = (ifc & 0x7F) | (ifc & 0x80);
    ins.n_ifr = (ifr & 0x7F) | (ifr & 0x80);
    let mch = b[0x3C];
    ins.n_midi_channel = mch;
    if mch >= 128 {
        ins.n_mix_plug = mch - 128;
        ins.n_midi_channel = 0;
    }
    let max_nodes = if mod_type & MOD_TYPE_MPT != 0 { MAX_ENVPOINTS as u8 } else { 25 };
    convert_envelope(&b[0x130..0x130 + 82], &mut ins.vol_env, 0, max_nodes);
    convert_envelope(&b[0x182..0x182 + 82], &mut ins.pan_env, 32, max_nodes);
    convert_envelope(&b[0x1D4..0x1D4 + 82], &mut ins.pitch_env, 32, max_nodes);
    if b[0x1D4] & 0x80 != 0 {
        ins.pitch_env.flags |= ENV_FILTER;
    } else {
        ins.pitch_env.flags &= !ENV_FILTER;
    }
    for i in 0..120 {
        let note = b[0x40 + i * 2];
        let smp = b[0x40 + i * 2 + 1] as SampleIndex;
        if (smp as usize) < MAX_SAMPLES {
            ins.keyboard[i] = smp;
        }
        ins.note_map[i] = if note < 120 { note + NOTE_MIN } else { i as u8 + NOTE_MIN };
    }
    let dummy = &b[0x226..0x22A];
    if dummy == b"MPTX" || dummy == b"XTPM" {
        for i in 0..120 {
            ins.keyboard[i] |= (b[554 + i] as SampleIndex) << 8;
        }
        size = 554 + 120;
    }
    file.seek(offset + size);
    if file.read_magic(b"MSNI") {
        let len = file.u32le() as usize;
        let mut modular = file.read_chunk(len);
        if modular.read_magic(b"GULP") {
            ins.n_mix_plug = modular.u8();
            if ins.n_mix_plug as usize > 250 {
                ins.n_mix_plug = 0;
            }
        }
    }
}

/// `ITSample::ConvertToMPT`; returns the sample data pointer.
fn convert_sample(b: &[u8], s: &mut ModSample) -> u32 {
    s.initialize(MOD_TYPE_IT);
    s.set_default_cue_points();
    s.n_volume = (b[0x13] as u16 * 4).min(256);
    s.n_global_vol = (b[0x11] as u16).min(64);
    let dfp = b[0x2F];
    s.n_pan = ((dfp & 0x7F) as u16 * 4).min(256);
    let flags = b[0x12];
    if dfp & 0x80 != 0 {
        s.u_flags |= CHN_PANNING;
    }
    if flags & 0x10 != 0 {
        s.u_flags |= CHN_LOOP;
    }
    if flags & 0x20 != 0 {
        s.u_flags |= CHN_SUSTAINLOOP;
    }
    if flags & 0x40 != 0 {
        s.u_flags |= CHN_PINGPONGLOOP;
    }
    if flags & 0x80 != 0 {
        s.u_flags |= CHN_PINGPONGSUSTAIN;
    }
    s.n_c5_speed = le32(b, 0x3C);
    if s.n_c5_speed == 0 {
        s.n_c5_speed = 8363;
    }
    if s.n_c5_speed < 256 {
        s.n_c5_speed = 256;
    }
    s.n_length = le32(b, 0x30);
    s.n_loop_start = le32(b, 0x34);
    s.n_loop_end = le32(b, 0x38);
    s.n_sustain_start = le32(b, 0x40);
    s.n_sustain_end = le32(b, 0x44);
    s.sanitize_loops();
    s.n_vib_type = AUTO_VIBRATO_IT2XM[(b[0x4F] & 7) as usize];
    s.n_vib_rate = b[0x4C];
    s.n_vib_depth = b[0x4D] & 0x7F;
    s.n_vib_sweep = b[0x4E];
    let cvt = b[0x2E];
    if cvt == 0x40 {
        s.u_flags |= CHN_ADLIB;
    } else if cvt == 0x80 {
        s.u_flags |= SMP_KEEPONDISK;
    }
    le32(b, 0x48)
}

/// `ITSample::GetSampleFormat`.
fn sample_format(b: &[u8], cwtv: u16) -> SampleIo {
    let flags = b[0x12];
    let cvt = b[0x2E];
    let mut io = SampleIo::new(
        if flags & 0x02 != 0 { 16 } else { 8 },
        Channels::Mono,
        false,
        if cvt & 0x01 != 0 { Encoding::Signed } else { Encoding::Unsigned },
    );
    if flags & 0x04 != 0 && cwtv >= 0x214 {
        io.channels = Channels::StereoSplit;
    }
    if flags & 0x08 != 0 {
        io.encoding = if cvt & 0x04 != 0 { Encoding::It215 } else { Encoding::It214 };
    } else if flags & 0x02 == 0 && cvt == 0xFF {
        io.encoding = Encoding::Adpcm;
    } else {
        if cvt & 0x02 != 0 {
            io.big_endian = true;
        }
        if cvt & 0x04 != 0 {
            io.encoding = Encoding::Delta;
        }
    }
    io
}

fn header_ok(h: &[u8]) -> bool {
    (&h[..4] == b"IMPM" || &h[..4] == b"tpm.") && le16(h, 0x22) <= 0xFF && (le16(h, 0x24) as usize) < MAX_SAMPLES
}

/// `ProbeFileHeaderIT`.
pub fn probe(data: &[u8]) -> bool {
    data.len() >= 192 && header_ok(&data[..192])
}

/// `ReadIT`.
pub fn read(data: &[u8]) -> Option<Module> {
    let mut file = Reader::new(data);
    let h = file.read_slice(192)?;
    if !header_ok(h) {
        return None;
    }
    let ordnum = le16(h, 0x20) as usize;
    let insnum = le16(h, 0x22) as usize;
    let smpnum = le16(h, 0x24) as usize;
    let patnum = le16(h, 0x26) as usize;
    let cwtv = le16(h, 0x28);
    let cmwt = le16(h, 0x2A);
    let flags = le16(h, 0x2C);
    let special = le16(h, 0x2E);
    let globalvol = h[0x30];
    let mv = h[0x31];
    let speed = h[0x32];
    let tempo = h[0x33];
    let sep = h[0x34];
    let pwd = h[0x35];
    let msglength = le16(h, 0x36);
    let msgoffset = le32(h, 0x38);
    let reserved = le32(h, 0x3C);
    let chnpan = &h[0x40..0x80];
    let chnvol = &h[0x80..0xC0];
    if !file.can_read(ordnum + (insnum + smpnum + patnum) * 4) {
        return None;
    }
    let mut m = Module::new(MOD_TYPE_IT, 0);
    let mut interpret_modplug_made = false;
    if &h[..4] == b"tpm." {
        // Legacy MPTM: played as IT here.
    } else if cwtv & 0xF000 == 0x5000 {
        let mut v = ((cwtv & 0x0FFF) as u32) << 16;
        if &h[0x3C..0x40] == b"OMPT" {
            interpret_modplug_made = true;
        } else if v >= 0x0129_0000 {
            v |= reserved & 0xFFFF;
        }
        m.last_saved_with_version = v;
    } else if cmwt == 0x888 || cwtv == 0x888 {
        interpret_modplug_made = true;
        m.last_saved_with_version = mpt_v("1.17.00.00");
    } else if cwtv == 0x0214 && cmwt == 0x0202 && reserved == 0 {
        m.last_saved_with_version = mpt_v("1.09.00.00");
        interpret_modplug_made = true;
    } else if cwtv == 0x0300 && cmwt == 0x0300 && reserved == 0 && ordnum == 256 && sep == 128 && pwd == 0 {
        m.last_saved_with_version = mpt_v("1.17.02.20");
        interpret_modplug_made = true;
    }
    if flags & 0x08 != 0 {
        m.song_flags |= SONG_LINEARSLIDES;
    }
    if flags & 0x10 != 0 {
        m.song_flags |= SONG_ITOLDEFFECTS;
    }
    if flags & 0x20 != 0 {
        m.song_flags |= SONG_ITCOMPATGXX;
    }
    if flags & 0x1000 != 0 {
        m.song_flags |= SONG_EXFILTERRANGE;
    }
    m.title = read_name(&h[4..30], true);
    if special & 0x04 != 0 && (m.last_saved_with_version == 0 || m.last_saved_with_version >= mpt_v("1.17.03.02")) {
        m.default_rows_per_beat = h[0x1E] as u32;
        m.default_rows_per_measure = h[0x1F] as u32;
    }
    m.default_global_volume = (globalvol as u32) << 1;
    if m.default_global_volume > MAX_GLOBAL_VOLUME {
        m.default_global_volume = MAX_GLOBAL_VOLUME;
    }
    if speed != 0 {
        m.default_speed = speed as u32;
    }
    m.default_tempo = Tempo::new(tempo.max(31) as u32, 0);
    m.sample_pre_amp = mv.min(128) as u32;

    file.seek(192);
    m.order = file
        .read_slice(ordnum)?
        .iter()
        .map(|&p| match p {
            0xFF => PATTERNINDEX_INVALID,
            0xFE => PATTERNINDEX_SKIP,
            p => p as PatternIndex,
        })
        .collect();
    let ins_pos: Vec<u32> = (0..insnum).map(|_| file.u32le()).collect();
    let smp_pos: Vec<u32> = (0..smpnum).map(|_| file.u32le()).collect();
    let pat_pos: Vec<u32> = (0..patnum).map(|_| file.u32le()).collect();
    let mut min_ptr = u32::MAX;
    for &p in ins_pos.iter().chain(&smp_pos).chain(&pat_pos) {
        if p > 0 && p < min_ptr {
            min_ptr = p;
        }
    }
    if special & 0x01 != 0 {
        min_ptr = min_ptr.min(msgoffset);
    }
    let possibly_unmo3 = cmwt == 0x0214
        && (cwtv == 0x0214 || cwtv == 0)
        && h[0x1F] == 0
        && h[0x1E] == 0
        && pwd == 0
        && reserved == 0
        && flags & (0x40 | 0x80) == 0;
    if possibly_unmo3 && insnum == 0 && smpnum > 0 && (file.pos + 4 * smp_pos.len() + 2) as u64 <= min_ptr as u64 {
        for i in 0..smpnum {
            if file.u32le() != 0 {
                file.pos -= 4 + i * 4;
                break;
            }
        }
    }
    if special & 0x02 != 0 {
        let nflt = file.u16le() as usize;
        if file.can_read(nflt * 8) && (file.pos + nflt * 8) as u64 <= min_ptr as u64 {
            file.skip(nflt * 8);
        } else {
            file.pos -= 2;
        }
    } else if possibly_unmo3 && special <= 1 && file.u16le() != 0 {
        file.pos -= 2;
    }
    let has_midi_config = flags & 0x80 != 0 || special & 0x08 != 0;
    if has_midi_config && file.can_read((9 + 16 + 128) * 32) {
        read_midi_config(&mut file, &mut m.midi_cfg);
        m.midi_cfg.sanitize();
    }
    let mut has_modplug_extensions = false;
    if file.read_magic(b"PNAM") {
        let len = file.u32le() as usize;
        file.read_chunk(len);
        has_modplug_extensions = true;
    }
    if file.read_magic(b"CNAM") {
        let len = file.u32le() as usize;
        file.read_chunk(len);
        m.chn_settings.resize((len / 20).min(MAX_BASECHANNELS), Default::default());
        has_modplug_extensions = true;
    }
    let plugin_len = if (min_ptr as usize) >= file.pos { min_ptr as usize - file.pos } else { file.bytes_left() };
    let mut plugin_chunk = file.read_chunk(plugin_len);
    let (has_plugin_chunks, is_bero) = crate::ext::load_mix_plugins(&mut plugin_chunk, &mut m, false);
    if has_plugin_chunks {
        has_modplug_extensions = true;
    }
    if cwtv == 0x0217 && cmwt == 0x0200 && reserved == 0 && !is_bero {
        if has_modplug_extensions || m.order.last() == Some(&PATTERNINDEX_INVALID) || chnpan.contains(&0xFF) {
            m.last_saved_with_version = mpt_v("1.16");
        } else {
            m.last_saved_with_version = mpt_v("1.17");
        }
        interpret_modplug_made = true;
    }

    // Instruments
    m.num_instruments = 0;
    if flags & 0x04 != 0 {
        m.num_instruments = insnum.min(MAX_INSTRUMENTS - 1) as InstrumentIndex;
    }
    m.instruments = vec![None; m.num_instruments as usize + 1];
    for i in 0..m.num_instruments as usize {
        if ins_pos[i] > 0 && file.seek(ins_pos[i] as usize) && file.can_read(554) {
            let mut ins = Box::new(ModInstrument::new(0));
            read_instrument(&mut file, &mut ins, cmwt, m.mod_type);
            ins.midi_pwd = pwd as i8;
            m.instruments[i + 1] = Some(ins);
        }
    }

    let mut last_sample_offset: usize = 0;
    if smpnum > 0 {
        last_sample_offset = smp_pos[smpnum - 1] as usize + 80;
    }
    let mute_buggy = !interpret_modplug_made && (0x0100..=0x0217).contains(&cwtv) && (cwtv < 0x0207 || reserved != 0);
    m.num_samples = smpnum.min(MAX_SAMPLES - 1) as SampleIndex;
    m.samples.resize_with(m.num_samples as usize + 1, || ModSample::new(MOD_TYPE_IT));
    for i in 0..m.num_samples as usize {
        if smp_pos[i] == 0 || !file.seek(smp_pos[i] as usize) {
            continue;
        }
        let Some(sb) = file.read_slice(80) else {
            continue;
        };
        let s = &mut m.samples[i + 1];
        let offset = convert_sample(sb, s);
        if mute_buggy && sb[0x12] & 0x01 == 0 {
            s.n_length = 0;
        }
        s.name = read_name(&sb[0x14..0x2E], true);
        if !file.seek(offset as usize) {
            continue;
        }
        if s.u_flags & CHN_ADLIB != 0 {
            file.skip(12);
        } else if s.u_flags & SMP_KEEPONDISK == 0 {
            let io = sample_format(sb, cwtv);
            io.read_sample(s, &mut file);
        } else {
            s.n_length = 0;
        }
        last_sample_offset = last_sample_offset.max(file.pos);
    }
    m.num_samples = m.num_samples.max(1);
    if m.samples.len() < 2 {
        m.samples.resize_with(2, || ModSample::new(MOD_TYPE_IT));
    }
    m.min_period = 0;
    m.max_period = i32::MAX;

    let num_pats = pat_pos.len().min(4000);
    // Channel count: the highest channel any pattern uses.
    let mut num_channels = m.num_channels().max(1);
    for &pp in pat_pos.iter().take(num_pats) {
        if pp == 0 || !file.seek(pp as usize) {
            continue;
        }
        let len = file.u16le() as usize;
        let num_rows = file.u16le() as u32;
        if num_rows < 1 || num_rows > MAX_PATTERN_ROWS || !file.skip(4) {
            continue;
        }
        let mut pd = file.read_chunk(len);
        let mut row = 0;
        let mut chn_mask: Vec<u8> = vec![0; num_channels];
        while row < num_rows && pd.can_read(1) {
            let b = pd.u8();
            if b == 0 {
                row += 1;
                continue;
            }
            let mut ch = (b & 0x7F) as usize;
            if ch != 0 {
                ch -= 1;
            }
            if ch >= chn_mask.len() {
                chn_mask.resize(ch + 1, 0);
            }
            if b & 0x80 != 0 {
                chn_mask[ch] = pd.u8();
            }
            if chn_mask[ch] & 0x0F != 0 {
                if ch >= num_channels && ch < MAX_BASECHANNELS {
                    num_channels = ch + 1;
                }
                const SKIPS: [usize; 16] = [0, 1, 1, 2, 1, 2, 2, 3, 2, 3, 3, 4, 3, 4, 4, 5];
                pd.skip(SKIPS[(chn_mask[ch] & 0x0F) as usize]);
            }
        }
        last_sample_offset = last_sample_offset.max(file.pos);
    }
    m.chn_settings.resize(num_channels, Default::default());
    if last_sample_offset > 0 {
        file.seek(last_sample_offset.min(file.len()));
    }
    let has_ext_ins = crate::ext::load_extended_instrument_properties(&mut file, &mut m);
    if interpret_modplug_made && !is_bero {
        m.play_behaviour = [false; kMaxPlayBehaviours];
        m.set_mix_levels(MixLevels::Original);
    }
    let has_ext_song = crate::ext::load_extended_song_properties(&mut file, &mut m, false);
    let _ = (has_ext_ins, has_ext_song);
    let header_channels = m.num_channels().min(64);
    for i in 0..header_channels {
        if chnpan[i] == 0xFF {
            continue;
        }
        let cs = &mut m.chn_settings[i];
        cs.n_volume = chnvol[i].min(64);
        if chnpan[i] & 0x80 != 0 {
            cs.dw_flags |= CHN_MUTE;
        }
        let n = chnpan[i] & 0x7F;
        if n <= 64 {
            cs.n_pan = n as u16 * 4;
        }
        if n == 100 {
            cs.dw_flags |= CHN_SURROUND;
        }
    }

    // Patterns
    let nc = m.num_channels();
    m.patterns = vec![Pattern::default(); num_pats];
    let mut cells = 0usize;
    let mut has_vol_col_offset = false;
    for pat in 0..num_pats {
        if pat_pos[pat] == 0 || !file.seek(pat_pos[pat] as usize) {
            if cells + 64 * nc <= MAX_PATTERN_CELLS {
                cells += 64 * nc;
                m.patterns[pat] = Pattern { rows: 64, data: vec![ModCommand::default(); 64 * nc], ..Default::default() };
            }
            continue;
        }
        let len = file.u16le() as usize;
        let num_rows = file.u16le() as u32;
        if !file.skip(4) || num_rows == 0 || num_rows > MAX_PATTERN_ROWS || cells + num_rows as usize * nc > MAX_PATTERN_CELLS {
            continue;
        }
        cells += num_rows as usize * nc;
        m.patterns[pat] = Pattern { rows: num_rows, data: vec![ModCommand::default(); num_rows as usize * nc], ..Default::default() };
        let mut pd = file.read_chunk(len);
        let mut chn_mask: Vec<u8> = vec![0; nc];
        let mut last: Vec<ModCommand> = vec![ModCommand::default(); nc];
        let mut row = 0u32;
        let mut dummy = ModCommand::default();
        while row < num_rows && pd.can_read(1) {
            let b = pd.u8();
            if b == 0 {
                row += 1;
                continue;
            }
            let mut ch = (b & 0x7F) as usize;
            if ch != 0 {
                ch -= 1;
            }
            if ch >= chn_mask.len() {
                chn_mask.resize(ch + 1, 0);
                last.resize(ch + 1, ModCommand::default());
            }
            if b & 0x80 != 0 {
                chn_mask[ch] = pd.u8();
            }
            let mask = chn_mask[ch];
            let mc = if ch < nc { &mut m.patterns[pat].data[row as usize * nc + ch] } else { &mut dummy };
            let lv = &mut last[ch];
            if mask & 0x10 != 0 {
                mc.note = lv.note;
            }
            if mask & 0x20 != 0 {
                mc.instr = lv.instr;
            }
            if mask & 0x40 != 0 {
                mc.volcmd = lv.volcmd;
                mc.vol = lv.vol;
            }
            if mask & 0x80 != 0 {
                mc.command = lv.command;
                mc.param = lv.param;
            }
            if mask & 1 != 0 {
                let mut note = pd.u8();
                if note < 0x80 {
                    note += NOTE_MIN;
                } else if note == 0xFF {
                    note = NOTE_KEYOFF;
                } else if note == 0xFE {
                    note = NOTE_NOTECUT;
                } else if note == 0xFD && m.mod_type != MOD_TYPE_MPT {
                    note = NOTE_NONE;
                } else {
                    note = NOTE_FADE;
                }
                mc.note = note;
                lv.note = note;
            }
            if mask & 2 != 0 {
                let i = pd.u8();
                mc.instr = i;
                lv.instr = i;
            }
            if mask & 4 != 0 {
                let vol = pd.u8();
                if vol <= 64 {
                    mc.volcmd = VOLCMD_VOLUME;
                    mc.vol = vol;
                } else if (128..=192).contains(&vol) {
                    mc.volcmd = VOLCMD_PANNING;
                    mc.vol = vol - 128;
                } else if vol < 75 {
                    mc.volcmd = VOLCMD_FINEVOLUP;
                    mc.vol = vol - 65;
                } else if vol < 85 {
                    mc.volcmd = VOLCMD_FINEVOLDOWN;
                    mc.vol = vol - 75;
                } else if vol < 95 {
                    mc.volcmd = VOLCMD_VOLSLIDEUP;
                    mc.vol = vol - 85;
                } else if vol < 105 {
                    mc.volcmd = VOLCMD_VOLSLIDEDOWN;
                    mc.vol = vol - 95;
                } else if vol < 115 {
                    mc.volcmd = VOLCMD_PORTADOWN;
                    mc.vol = vol - 105;
                } else if vol < 125 {
                    mc.volcmd = VOLCMD_PORTAUP;
                    mc.vol = vol - 115;
                } else if (193..=202).contains(&vol) {
                    mc.volcmd = VOLCMD_TONEPORTAMENTO;
                    mc.vol = vol - 193;
                } else if (203..=212).contains(&vol) {
                    mc.volcmd = VOLCMD_VIBRATODEPTH;
                    mc.vol = vol - 203;
                    if mc.vol != 0 && m.last_saved_with_version != 0 && m.last_saved_with_version <= mpt_v("1.17.02.54") {
                        mc.volcmd = VOLCMD_VIBRATOSPEED;
                    }
                } else if (223..=232).contains(&vol) {
                    mc.volcmd = VOLCMD_OFFSET;
                    mc.vol = vol - 223;
                    has_vol_col_offset = true;
                }
                lv.volcmd = mc.volcmd;
                lv.vol = mc.vol;
            }
            if mask & 8 != 0 {
                let [command, param] = pd.read_array::<2>();
                s3m_convert(mc, command, param, true);
                if mc.command == CMD_S3MCMDEX && (mc.param & 0xF0) == 0xA0 && cwtv < 0x0200 {
                    mc.command = CMD_DUMMY;
                } else if mc.command == CMD_GLOBALVOLUME && mc.param > 0x80 && (0x1000..=0x1050).contains(&cwtv) {
                    mc.param = 0x80;
                }
                lv.command = mc.command;
                lv.param = mc.param;
            }
        }
    }
    if !has_vol_col_offset && (m.mod_type != MOD_TYPE_MPT || m.last_saved_with_version < mpt_v("1.24.02.06")) {
        for s in m.samples.iter_mut().skip(1) {
            s.remove_all_cue_points();
        }
    }
    if m.last_saved_with_version == 0 && cwtv == 0x0888 {
        m.last_saved_with_version = mpt_v("1.17.00.00");
    }
    if m.last_saved_with_version == 0 {
        let schism_v = schism_epoch_date() + if cwtv == 0x1FFF { reserved as i32 } else { cwtv as i32 - 0x1050 };
        match cwtv >> 12 {
            0 => {
                if is_bero {
                } else if cwtv == 0x0202 && cmwt == 0x0200 && h[0x1F] == 0 && h[0x1E] == 0 && reserved == 0 && !pat_pos.is_empty() && !smp_pos.is_empty() && pat_pos[0] != 0 && pat_pos[0] < smp_pos[0] {
                    m.last_saved_with_version = mpt_v("1.00.00.A0");
                } else if cwtv == 0x0214 && cmwt == 0x0200 && h[0x1F] == 0 && h[0x1E] == 0 && reserved == 0 {
                    if special & (0x04 | 0x02) != 0 {
                        m.last_saved_with_version = if ins_pos.len() >= 2 && ins_pos[1].wrapping_sub(ins_pos[0]) == 557 {
                            mpt_v("1.00.00.B2")
                        } else {
                            mpt_v("1.00.00.B1")
                        };
                    } else {
                        m.last_saved_with_version = mpt_v("1.00.00.A5");
                    }
                } else if cwtv == 0x0214 && cmwt == 0x0214 && &h[0x3C..0x40] == b"CHBI" {
                    m.play_behaviour[kITShortSampleRetrig] = false;
                    m.sample_pre_amp /= 2;
                }
            }
            1 => {
                let quirks: [(i32, usize); 21] = [
                    (schism_date(2015, 1, 29), kPeriodsAreHertz),
                    (schism_date(2016, 5, 13), kITShortSampleRetrig),
                    (schism_date(2021, 5, 2), kITDoNotOverrideChannelPan),
                    (schism_date(2021, 5, 2), kITPanningReset),
                    (schism_date(2021, 11, 1), kITPitchPanSeparation),
                    (schism_date(2022, 4, 30), kITEmptyNoteMapSlot),
                    (schism_date(2022, 4, 30), kITPortamentoSwapResetsPos),
                    (schism_date(2022, 4, 30), kITMultiSampleInstrumentNumber),
                    (schism_date(2023, 3, 9), kITInitialNoteMemory),
                    (schism_date(2023, 10, 17), kITDCTBehaviour),
                    (schism_date(2023, 10, 19), kITSampleAndHoldPanbrello),
                    (schism_date(2023, 10, 19), kITPortaNoNote),
                    (schism_date(2023, 10, 22), kITFirstTickHandling),
                    (schism_date(2023, 10, 22), kITMultiSampleInstrumentNumber),
                    (schism_date(2024, 3, 9), kITPanbrelloHold),
                    (schism_date(2024, 5, 12), kITNoSustainOnPortamento),
                    (schism_date(2024, 5, 12), kITEmptyNoteMapSlotIgnoreCell),
                    (schism_date(2024, 5, 27), kITOffsetWithInstrNumber),
                    (schism_date(2024, 10, 13), kITDoublePortamentoSlides),
                    (schism_date(2025, 1, 8), kITCarryAfterNoteOff),
                    (schism_date(2026, 7, 13), kITCompatGxxCarryPortaWithIns),
                ];
                for (date, b) in quirks {
                    if schism_v < date {
                        m.play_behaviour[b] = false;
                    }
                }
                if schism_v < schism_date(2021, 5, 2) && !m.song_flag(SONG_LINEARSLIDES) {
                    m.play_behaviour[kPeriodsAreHertz] = false;
                }
                if schism_v < schism_date(2021, 11, 1) {
                    m.play_behaviour[kImprecisePingPongLoops] = true;
                }
            }
            _ => {}
        }
    }
    if (cwtv < 0x0214 && cmwt < 0x0214) || (m.last_saved_with_version != 0 && m.last_saved_with_version <= mpt_v("1.00.00.A6")) {
        m.midi_cfg.clear_zxx();
    }
    let _ = msglength;
    m.format_name = format!("Impulse Tracker {}.{:02X}", cmwt >> 8, cmwt & 0xFF);
    Some(m)
}
