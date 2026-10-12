//! ProTracker MOD and compatible formats, ported from libopenmpt 0.8.9
//! `soundlib/Load_mod.cpp` and `MODTools.cpp`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

use crate::command::*;
use crate::defs::pb::*;
use crate::defs::*;
use crate::io::{Channels, Encoding, Reader, SampleIo, be16, read_name};
use crate::sample::ModSample;
use crate::sndfile::{Module, Pattern};
use crate::tables::PRO_TRACKER_PERIOD_TABLE;

pub const SAMPLE_HEADER_SIZE: usize = 30;

/// `MODSampleHeader`.
#[derive(Clone, Copy, Default)]
pub struct ModSampleHeader {
    pub name: [u8; 22],
    pub length: u16,
    pub finetune: u8,
    pub volume: u8,
    pub loop_start: u16,
    pub loop_length: u16,
}

impl ModSampleHeader {
    pub fn parse(b: &[u8; 30]) -> Self {
        let mut name = [0u8; 22];
        name.copy_from_slice(&b[..22]);
        ModSampleHeader { name, length: be16(b, 22), finetune: b[24], volume: b[25], loop_start: be16(b, 26), loop_length: be16(b, 28) }
    }
    pub fn read(file: &mut Reader, swap: bool) -> Self {
        let mut b = file.read_array::<30>();
        if swap {
            for i in (0..30).step_by(2) {
                b.swap(i, i + 1);
            }
        }
        Self::parse(&b)
    }

    /// `ConvertToMPT`.
    pub fn convert_to_mpt(&self, s: &mut ModSample, is4chn: bool) {
        s.initialize(MOD_TYPE_MOD);
        s.n_length = self.length as u32 * 2;
        s.n_fine_tune = (((self.finetune & 0x0F) as u8) << 4) as i8;
        s.n_volume = 4 * self.volume.min(64) as u16;
        let mut l_start = self.loop_start as u32 * 2;
        let l_length = self.loop_length as u32 * 2;
        if l_length > 2 && l_start + l_length > s.n_length && l_start / 2 + l_length <= s.n_length {
            l_start /= 2;
        }
        if s.n_length == 2 {
            s.n_length = 0;
        }
        if s.n_length != 0 {
            s.n_loop_start = l_start;
            s.n_loop_end = l_start + l_length;
            if s.n_loop_start >= s.n_length {
                s.n_loop_start = s.n_length - 1;
            }
            if s.n_loop_start > s.n_loop_end || s.n_loop_end < 4 || s.n_loop_end - s.n_loop_start < 4 {
                s.n_loop_start = 0;
                s.n_loop_end = 0;
            }
            if s.n_loop_end <= 8 && s.n_loop_start == 0 && s.n_length > s.n_loop_end && is4chn {
                s.n_loop_end = 0;
            }
            if s.n_loop_end > s.n_loop_start {
                s.u_flags |= CHN_LOOP;
            }
        }
    }

    /// `GetInvalidByteScore`.
    pub fn invalid_byte_score(&self) -> u32 {
        (self.volume > 64) as u32 + (self.finetune > 15) as u32 + ((self.loop_start as u32) > self.length as u32 * 2) as u32
    }
}

/// `ReadMODSample`.
pub fn read_mod_sample(h: &ModSampleHeader, s: &mut ModSample, is4chn: bool) -> u32 {
    h.convert_to_mpt(s, is4chn);
    s.name = read_name(&h.name, true);
    h.invalid_byte_score()
}

/// `CSoundFile::ConvertModCommand`.
pub fn convert_mod_command(m: &mut ModCommand, command: u8, param: u8) {
    const EFF: [u8; 39] = [
        CMD_ARPEGGIO,
        CMD_PORTAMENTOUP,
        CMD_PORTAMENTODOWN,
        CMD_TONEPORTAMENTO,
        CMD_VIBRATO,
        CMD_TONEPORTAVOL,
        CMD_VIBRATOVOL,
        CMD_TREMOLO,
        CMD_PANNING8,
        CMD_OFFSET,
        CMD_VOLUMESLIDE,
        CMD_POSITIONJUMP,
        CMD_VOLUME,
        CMD_PATTERNBREAK,
        CMD_MODCMDEX,
        CMD_TEMPO,
        CMD_GLOBALVOLUME,
        CMD_GLOBALVOLSLIDE,
        CMD_NONE,
        CMD_NONE,
        CMD_KEYOFF,
        CMD_SETENVPOSITION,
        CMD_NONE,
        CMD_NONE,
        CMD_NONE,
        CMD_PANNINGSLIDE,
        CMD_NONE,
        CMD_RETRIG,
        CMD_NONE,
        CMD_TREMOR,
        CMD_NONE,
        CMD_NONE,
        CMD_DUMMY,
        CMD_XFINEPORTAUPDOWN,
        CMD_PANBRELLO,
        CMD_MIDI,
        CMD_SMOOTHMIDI,
        CMD_SMOOTHMIDI,
        CMD_XPARAM,
    ];
    m.command = CMD_NONE;
    m.param = param;
    if command == 0 && param == 0 {
        m.command = CMD_NONE;
    } else if command == 0x0F && param < 0x20 {
        m.command = CMD_SPEED;
    } else if (command as usize) < EFF.len() {
        m.command = EFF[command as usize];
        if m.command == CMD_PATTERNBREAK {
            m.param = ((m.param >> 4).wrapping_mul(10)).wrapping_add(m.param & 0x0F);
        }
    }
}

/// `ReadMODPatternEntry`: fills note and instrument, returns (command, param).
pub fn read_mod_pattern_entry(data: [u8; 4], m: &mut ModCommand) -> (u8, u8) {
    let period = ((data[0] as u16 & 0x0F) << 8) | data[1] as u16;
    let mut note = NOTE_NONE as usize;
    if period > 0 && period != 0xFFF {
        note = PRO_TRACKER_PERIOD_TABLE.len() + 23 + NOTE_MIN as usize;
        for i in 0..PRO_TRACKER_PERIOD_TABLE.len() {
            if period >= PRO_TRACKER_PERIOD_TABLE[i] {
                if period != PRO_TRACKER_PERIOD_TABLE[i] && i != 0 {
                    let p1 = PRO_TRACKER_PERIOD_TABLE[i - 1] as i32;
                    let p2 = PRO_TRACKER_PERIOD_TABLE[i] as i32;
                    if p1 - (period as i32) < (period as i32 - p2) {
                        note = i + 23 + NOTE_MIN as usize;
                        break;
                    }
                }
                note = i + 24 + NOTE_MIN as usize;
                break;
            }
        }
    }
    m.note = note as u8;
    m.instr = (data[2] >> 4) | (data[0] & 0x10);
    m.command = CMD_NONE;
    (data[2] & 0x0F, data[3])
}

/// `CountMalformedMODPatternData`.
pub fn count_malformed_mod_pattern_data(data: &[u8], extended: bool) -> u32 {
    let mask = if extended { 0xE0 } else { 0xF0 };
    let mut bad = 0;
    for cell in data.chunks_exact(4) {
        if cell[0] & mask != 0 {
            bad += 1;
        }
        if !extended {
            let period = ((cell[0] as u16 & 0x0F) << 8) | cell[1] as u16;
            if period != 0 && period != 0xFFF {
                let table = &PRO_TRACKER_PERIOD_TABLE[24..60];
                // std::binary_search with comp(l, r) = l > r + 1 over a descending table.
                let found = {
                    let mut lo = 0usize;
                    let mut count = table.len();
                    while count > 0 {
                        let step = count / 2;
                        let it = lo + step;
                        if table[it] > period + 1 {
                            lo = it + 1;
                            count -= step + 1;
                        } else {
                            count = step;
                        }
                    }
                    lo < table.len() && !(period > table[lo] + 1)
                };
                if !found {
                    bad += 2;
                }
            }
        }
    }
    bad
}

/// `ValidateMODPatternData` on 64 rows x 4 channels.
pub fn validate_mod_pattern_data(file: &mut Reader, threshold: u32, extended: bool) -> bool {
    match file.read_slice(64 * 4 * 4) {
        Some(d) => count_malformed_mod_pattern_data(d, extended) <= threshold,
        None => false,
    }
}

/// `GetNumPatterns`.
pub fn get_num_patterns(
    file: &mut Reader,
    m: &mut Module,
    num_orders: OrderIndex,
    total_sample_len: SmpLength,
    wow_sample_len: SmpLength,
    validate_hidden: bool,
) -> PatternIndex {
    let mut num_patterns: PatternIndex = 0;
    let mut official: PatternIndex = 0;
    let mut illegal: PatternIndex = 0;
    for ord in 0..128usize {
        let pat = m.order[ord];
        if pat < 128 && num_patterns <= pat {
            num_patterns = pat + 1;
            if ord < num_orders as usize {
                official = num_patterns;
            }
        }
        if pat >= illegal {
            illegal = pat + 1;
        }
    }
    m.order.resize(num_orders as usize, 0);
    let start = file.pos;
    let size_without = total_sample_len as usize + start;
    let nch = m.num_channels();
    let size_official = size_without + official as usize * nch * 256;
    let file_size = file.len() & !1;
    if wow_sample_len != 0 && (wow_sample_len as usize + start) + num_patterns as usize * 8 * 256 == file_size {
        file.seek(start + num_patterns as usize * 4 * 256);
        if validate_mod_pattern_data(file, 16, true) {
            m.chn_settings.resize(8, Default::default());
        }
        file.seek(start);
    } else if num_patterns != official && (validate_hidden || size_official == file_size) {
        file.seek(start + official as usize * nch * 256);
        if !validate_mod_pattern_data(file, 64, true) {
            num_patterns = official;
        }
        file.seek(start);
    }
    let nch = m.num_channels();
    if illegal > num_patterns && size_without + illegal as usize * nch * 256 == file_size {
        num_patterns = illegal;
    } else if illegal >= 0xFF {
        for o in m.order.iter_mut() {
            if *o == 0xFE {
                *o = PATTERNINDEX_SKIP;
            } else if *o == 0xFF {
                *o = PATTERNINDEX_INVALID;
            }
        }
    }
    num_patterns
}

struct Magic {
    invalid_byte_threshold: u32,
    pattern_data_offset: usize,
    num_channels: usize,
    is_noise_tracker: bool,
    is_startrekker: bool,
    is_generic_multi_channel: bool,
    set_vblank: bool,
    swap_bytes: bool,
}

fn check_mod_magic(magic: &[u8; 4]) -> Option<Magic> {
    let mut r = Magic {
        invalid_byte_threshold: 40,
        pattern_data_offset: 1084,
        num_channels: 0,
        is_noise_tracker: false,
        is_startrekker: false,
        is_generic_multi_channel: false,
        set_vblank: false,
        swap_bytes: false,
    };
    let is = |s: &[u8; 4]| magic == s;
    if is(b"M.K.") || is(b"M!K!") || is(b"PATT") || is(b"NSMS") || is(b"LARD") {
        r.num_channels = 4;
    } else if is(b"M&K!") || is(b"FEST") || is(b"N.T.") {
        r.is_noise_tracker = true;
        r.set_vblank = true;
        r.num_channels = 4;
    } else if is(b"OKTA") || is(b"OCTA") {
        r.num_channels = 8;
    } else if is(b"CD81") || is(b"CD61") {
        r.num_channels = (magic[2] - b'0') as usize;
    } else if is(b"M\0\0\0") || is(b"8\0\0\0") {
        r.invalid_byte_threshold = 1;
        r.num_channels = if magic[0] == b'8' { 8 } else { 4 };
    } else if &magic[..3] == b"FA0" && (b'4'..=b'8').contains(&magic[3]) {
        r.num_channels = (magic[3] - b'0') as usize;
        r.pattern_data_offset = 1088;
    } else if (&magic[..3] == b"FLT" || &magic[..3] == b"EXO") && (magic[3] == b'4' || magic[3] == b'8') {
        r.is_startrekker = true;
        r.set_vblank = true;
        r.num_channels = (magic[3] - b'0') as usize;
    } else if (b'1'..=b'9').contains(&magic[0]) && &magic[1..] == b"CHN" {
        r.is_generic_multi_channel = true;
        r.num_channels = (magic[0] - b'0') as usize;
    } else if (b'1'..=b'9').contains(&magic[0]) && magic[1].is_ascii_digit() && (&magic[2..] == b"CH" || &magic[2..] == b"CN") {
        r.is_generic_multi_channel = true;
        r.num_channels = ((magic[0] - b'0') * 10 + magic[1] - b'0') as usize;
    } else if &magic[..3] == b"TDZ" && (b'1'..=b'9').contains(&magic[3]) {
        r.num_channels = (magic[3] - b'0') as usize;
    } else if is(b".M.K") {
        r.num_channels = 4;
        r.swap_bytes = true;
    } else if is(b"WARD") {
        r.is_generic_multi_channel = true;
        r.num_channels = 8;
    } else {
        return None;
    }
    Some(r)
}

/// `ProbeFileHeaderMOD`.
pub fn probe(data: &[u8]) -> bool {
    if data.len() < 1084 {
        return false;
    }
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&data[1080..1084]);
    let Some(r) = check_mod_magic(&magic) else {
        return false;
    };
    let mut f = Reader::new(data);
    f.seek(20);
    let mut invalid = 0;
    for _ in 0..31 {
        invalid += ModSampleHeader::read(&mut f, r.swap_bytes).invalid_byte_score();
    }
    invalid <= r.invalid_byte_threshold
}

/// `ReadMOD`.
pub fn read(data: &[u8]) -> Option<Module> {
    let mut file = Reader::new(data);
    if !file.seek(1080) {
        return None;
    }
    let magic = file.read_bytes::<4>()?;
    let mut r = check_mod_magic(&magic)?;
    if r.num_channels < 1 || r.num_channels > MAX_BASECHANNELS {
        return None;
    }
    let mut m = Module::new(MOD_TYPE_MOD, r.num_channels);
    let mut is_noise_tracker = r.is_noise_tracker;
    let is_startrekker = r.is_startrekker;
    let is_generic_multi = r.is_generic_multi_channel;
    let is_inconexia = &magic == b"M\0\0\0" || &magic == b"8\0\0\0";
    let mut has_rep_len0 = false;
    let mut has_empty_sample_with_volume = false;
    if r.set_vblank {
        m.play_behaviour[kMODVBlankTiming] = true;
    }
    let is_flt8 = is_startrekker && m.num_channels() == 8;
    let is_mdkd = &magic == b"M.K.";
    let is_hmnt = &magic == b"M&K!" || &magic == b"FEST";
    let mut maybe_wow = is_mdkd;

    file.seek(0);
    let mut title = file.read_array::<20>();
    if r.swap_bytes {
        for i in (0..20).step_by(2) {
            title.swap(i, i + 1);
        }
    }
    m.title = read_name(&title, true);

    let mut total_sample_len: SmpLength = 0;
    let mut wow_sample_len: SmpLength = 0;
    m.num_samples = 31;
    m.samples.resize_with(32, || ModSample::new(MOD_TYPE_MOD));
    let mut invalid = 0;
    let mut has_long_samples = false;
    let is4 = m.num_channels() == 4;
    for smp in 1..=31usize {
        let h = ModSampleHeader::read(&mut file, r.swap_bytes);
        invalid += read_mod_sample(&h, &mut m.samples[smp], is4);
        total_sample_len += m.samples[smp].n_length;
        if is_hmnt {
            m.samples[smp].n_fine_tune = (((h.finetune as i32) << 3) as i8).wrapping_neg();
        } else if m.samples[smp].n_length > 65535 {
            has_long_samples = true;
        }
        if h.length != 0 && h.loop_length == 0 {
            has_rep_len0 = true;
        } else if h.length == 0 && h.volume == 64 {
            has_empty_sample_with_volume = true;
        }
        if maybe_wow {
            wow_sample_len += h.length as u32 * 2;
            if h.finetune != 0 || (h.length > 0 && h.volume != 64) {
                maybe_wow = false;
            }
        }
    }
    if invalid > r.invalid_byte_threshold {
        return None;
    }

    let mut header = file.read_array::<130>();
    if r.swap_bytes {
        for i in (0..130).step_by(2) {
            header.swap(i, i + 1);
        }
    }
    let num_orders_raw = header[0];
    let restart_pos = header[1];
    file.seek(r.pattern_data_offset);
    if restart_pos > 0 {
        maybe_wow = false;
    }
    if !maybe_wow {
        wow_sample_len = 0;
    }
    m.order = header[2..130].iter().map(|&p| p as PatternIndex).collect();
    let mut real_orders = num_orders_raw as OrderIndex;
    if real_orders > 128 {
        real_orders = 128;
    } else if real_orders == 0 {
        real_orders = 128;
        while real_orders > 1 && m.order[real_orders as usize - 1] == 0 {
            real_orders -= 1;
        }
    }
    let mut num_patterns = get_num_patterns(&mut file, &mut m, real_orders, total_sample_len, wow_sample_len, false);
    let mut is_generic_multi = is_generic_multi;
    if maybe_wow && m.num_channels() == 8 {
        is_generic_multi = true;
    }
    if is_flt8 {
        for p in m.order.iter_mut() {
            *p /= 2;
        }
    }
    real_orders -= 1;
    m.restart_pos = restart_pos as OrderIndex;
    if restart_pos as OrderIndex > real_orders || (restart_pos == 0x78 && m.num_channels() == 4) {
        m.restart_pos = 0;
    }
    m.default_speed = 6;
    m.default_tempo = Tempo::new(125, 0);
    m.min_period = 14 * 4;
    m.max_period = 3424 * 4;
    m.sample_pre_amp = (256 / m.num_channels() as u32).clamp(32, 128);
    m.song_flags = SONG_FORMAT_NO_VOLCOL;
    setup_mod_panning(&mut m);

    let mut only_amiga_notes = true;
    let mut fix7bit = false;
    let mut max_panning: u8 = 0;
    const PAN_THRESHOLD: u8 = 0x30;
    let nch = m.num_channels();
    if !is_noise_tracker {
        let pattern_length = nch * 64;
        let (mut left_panning, mut extended_panning) = (false, false);
        is_noise_tracker = is_mdkd && !has_empty_sample_with_volume && !has_long_samples;
        for pat in 0..num_patterns {
            let mut breaks = 0u16;
            for _ in 0..pattern_length {
                let mut d = file.read_array::<4>();
                if r.swap_bytes && pat == 0 {
                    d.swap(0, 1);
                    d.swap(2, 3);
                }
                let mut mc = ModCommand::default();
                let (command, param) = read_mod_pattern_entry(d, &mut mc);
                if !ModCommand::is_amiga_note_of(mc.note) {
                    is_noise_tracker = false;
                    only_amiga_notes = false;
                }
                if (command > 0x06 && command < 0x0A) || (command == 0x0E && param > 0x01) || (command == 0x0F && param > 0x1F) || (command == 0x0D && {
                    breaks += 1;
                    breaks > 1
                }) {
                    is_noise_tracker = false;
                }
                if command == 0x08 {
                    max_panning = max_panning.max(param);
                    if param < 0x80 {
                        left_panning = true;
                    } else if param > 0x8F && param != 0xA4 {
                        extended_panning = true;
                    }
                } else if command == 0x0E && (param & 0xF0) == 0x80 {
                    max_panning = max_panning.max((param & 0x0F) << 4);
                }
            }
        }
        fix7bit = left_panning && !extended_panning && max_panning >= PAN_THRESHOLD;
    }

    file.seek(r.pattern_data_offset);
    let read_channels = if is_flt8 { 4 } else { nch };
    if is_flt8 {
        num_patterns += 1;
    }
    let mut has_tempo_commands = false;
    let mut definitely_cia = has_long_samples;
    let mut filter_state = false;
    let mut filter_transitions = 0;
    let mut referenced = [false; 32];
    let real_patterns = if is_flt8 { num_patterns.div_ceil(2) } else { num_patterns };
    m.patterns = Vec::with_capacity(real_patterns as usize);
    for pat in 0..num_patterns {
        let (actual, col0) = if is_flt8 { (pat / 2, if pat % 2 == 0 { 0 } else { 4 }) } else { (pat, 0) };
        if !is_flt8 || pat % 2 == 0 {
            if m.patterns.len() <= actual as usize {
                m.patterns.resize(actual as usize + 1, Pattern::default());
            }
            m.patterns[actual as usize] = Pattern { rows: 64, data: vec![ModCommand::default(); 64 * nch], ..Default::default() };
        }
        if actual as usize >= m.patterns.len() {
            break;
        }
        let mut last_instrument = vec![0u8; nch];
        let mut instr_without_note = vec![0u8; nch];
        for row in 0..64usize {
            let (mut has_speed, mut has_tempo) = (false, false);
            for chn in 0..read_channels {
                let mut d = file.read_array::<4>();
                if r.swap_bytes && pat == 0 {
                    d.swap(0, 1);
                    d.swap(2, 3);
                }
                let mut mc = ModCommand::default();
                let (mut command, mut param) = read_mod_pattern_entry(d, &mut mc);
                if command != 0 || param != 0 {
                    if is_startrekker && command == 0x0E {
                        command = 0;
                        param = 0;
                    } else if is_startrekker && command == 0x0F && param > 0x1F {
                        param = 0x1F;
                    }
                    convert_mod_command(&mut mc, command, param);
                }
                if mc.command == CMD_TEMPO {
                    has_tempo = true;
                    if mc.param < 100 {
                        has_tempo_commands = true;
                    }
                } else if mc.command == CMD_SPEED {
                    has_speed = true;
                } else if mc.command == CMD_PATTERNBREAK && is_noise_tracker {
                    mc.param = 0;
                } else if mc.command == CMD_TREMOLO && is_hmnt {
                    mc.command = CMD_HMN_MEGA_ARP;
                } else if mc.command == CMD_PANNING8 && fix7bit {
                    if mc.param == 0xA4 {
                        mc.command = CMD_S3MCMDEX;
                        mc.param = 0x91;
                    } else {
                        mc.param = (mc.param as u32 * 2).min(255) as u8;
                    }
                } else if mc.command == CMD_MODCMDEX && mc.param < 0x10 {
                    let new_state = mc.param & 0x01 == 0;
                    if new_state != filter_state {
                        filter_state = new_state;
                        filter_transitions += 1;
                    }
                }
                if mc.note == NOTE_NONE && mc.instr > 0 && !is_flt8 {
                    if last_instrument[chn] > 0 && last_instrument[chn] != mc.instr {
                        instr_without_note[chn] += 1;
                        if instr_without_note[chn] >= 4 {
                            m.play_behaviour[kMODSampleSwap] = true;
                        }
                    }
                } else if mc.note != NOTE_NONE {
                    instr_without_note[chn] = 0;
                }
                if mc.instr != 0 {
                    last_instrument[chn] = mc.instr;
                    if is_startrekker {
                        referenced[(mc.instr & 0x1F) as usize] = true;
                    }
                }
                m.patterns[actual as usize].data[row * nch + col0 + chn] = mc;
            }
            if has_speed && has_tempo {
                definitely_cia = true;
            }
        }
    }
    if only_amiga_notes && !has_rep_len0 && (is_mdkd || &magic == b"M!K!" || &magic == b"PATT") {
        m.song_flags |= SONG_AMIGALIMITS | SONG_PT_MODE;
        m.play_behaviour[kMODSampleSwap] = true;
        m.play_behaviour[kMODOutOfRangeNoteDelay] = true;
        m.play_behaviour[kMODTempoOnSecondTick] = true;
        if max_panning < PAN_THRESHOLD {
            m.play_behaviour[kMODIgnorePanning] = true;
            if restart_pos != 0x7F {
                m.play_behaviour[kMODOneShotLoops] = true;
            }
        }
    }
    if only_amiga_notes && !is_generic_multi && filter_transitions < 7 {
        m.song_flags |= SONG_ISAMIGA;
    }
    if is_generic_multi || is_mdkd || &magic == b"M!K!" {
        m.play_behaviour[kFT2MODTremoloRampWaveform] = true;
    }
    if is_inconexia {
        m.play_behaviour[kMODIgnorePanning] = true;
    }

    // Samples
    file.seek(r.pattern_data_offset + (read_channels * 64 * 4) * num_patterns as usize);
    for smp in 1..=31usize {
        if m.samples[smp].n_length == 0 {
            continue;
        }
        let mut encoding = Encoding::Signed;
        if is_inconexia {
            encoding = Encoding::Delta;
        } else if file.read_magic(b"ADPCM") {
            encoding = Encoding::Adpcm;
        }
        let io = SampleIo::new(8, Channels::Mono, false, encoding);
        let next = file.pos + io.encoded_size(m.samples[smp].n_length);
        let s = &mut m.samples[smp];
        if is_mdkd && only_amiga_notes && !has_empty_sample_with_volume {
            s.n_length = s.n_length.max(s.n_loop_end);
        }
        io.read_sample(s, &mut file);
        file.seek(next.min(file.len()));
    }

    // For "the ultimate beeper.mod"
    {
        let s = &mut m.samples[0];
        s.initialize(MOD_TYPE_MOD);
        s.n_length = 2;
        s.n_loop_start = 0;
        s.n_loop_end = 2;
        s.n_volume = 0;
        s.u_flags |= CHN_LOOP;
        s.allocate();
        s.precompute_loops(false);
    }

    m.format_name = format!("ProTracker MOD ({})", magic.iter().map(|&c| if c < b' ' { ' ' } else { c as char }).collect::<String>());
    m.tracker = String::new();

    // Fix VBlank MODs: a CIA reading longer than 8 minutes means the Fxx
    // commands were meant as speeds.
    if (is_mdkd || &magic == b"M!K!") && has_tempo_commands && !definitely_cia {
        let song_time = crate::length::song_length_seconds(&m);
        if song_time >= 480.0 {
            m.play_behaviour[kMODVBlankTiming] = true;
            if crate::length::reaches_time(&m, song_time) {
                m.play_behaviour[kMODVBlankTiming] = false;
            }
        }
    }
    Some(m)
}

/// `SetupMODPanning`.
pub fn setup_mod_panning(m: &mut Module) {
    for (chn, cs) in m.chn_settings.iter_mut().enumerate() {
        cs.dw_flags &= !CHN_SURROUND;
        cs.n_pan = if (chn & 3) == 1 || (chn & 3) == 2 { 0xC0 } else { 0x40 };
    }
}
