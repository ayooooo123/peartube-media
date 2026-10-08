//! Scream Tracker 3 modules, ported from libopenmpt 0.8.9
//! `soundlib/Load_s3m.cpp` and `S3MTools.cpp`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

use crate::command::*;
use crate::defs::pb::*;
use crate::defs::*;
use crate::io::{Channels, Encoding, Reader, SampleIo, le16, le32, read_name};
use crate::sample::ModSample;
use crate::sndfile::{Module, Pattern};
use crate::version::{mpt_v, schism_epoch_date};

const TRK_MASK: u16 = 0xF000;
const TRK_SCREAM: u16 = 0x1000;
const TRK_IMAGO: u16 = 0x2000;
const TRK_IT: u16 = 0x3000;
const TRK_SCHISM: u16 = 0x4000;
const TRK_OPENMPT: u16 = 0x5000;
const TRK_BERO: u16 = 0x6000;
const TRK_AKORD: u16 = 0x0208;
const TRK_ST3_00: u16 = 0x1300;
const TRK_ST3_01: u16 = 0x1301;
const TRK_ST3_20: u16 = 0x1320;
const TRK_IT2_14: u16 = 0x3214;
const TRK_BERO_OLD: u16 = 0x4100;
const TRK_GRAOUMF: u16 = 0x5447;
const TRK_NESMUSA: u16 = 0x5700;

// Work counts attempted pattern-byte reads, including the ignored length word
// and zero-filled EOF recovery. Shared/overlapping parapointers do not make
// another parse free. A dense 64-row, 32-channel pattern needs only 12,354
// bytes; all 255 such patterns fit below the aggregate limit.
const MAX_PATTERN_WORK: usize = (1 << 16) + 2;
const MAX_MODULE_PATTERN_WORK: usize = 4 << 20;

/// `S3MConvert`.
pub fn s3m_convert(m: &mut ModCommand, command: u8, param: u8, from_it: bool) {
    m.param = param;
    m.command = match command | 0x40 {
        b'@' => {
            if m.param != 0 {
                CMD_DUMMY
            } else {
                CMD_NONE
            }
        }
        b'A' => CMD_SPEED,
        b'B' => CMD_POSITIONJUMP,
        b'C' => {
            if !from_it {
                m.param = (m.param >> 4).wrapping_mul(10).wrapping_add(m.param & 0x0F);
            }
            CMD_PATTERNBREAK
        }
        b'D' => CMD_VOLUMESLIDE,
        b'E' => CMD_PORTAMENTODOWN,
        b'F' => CMD_PORTAMENTOUP,
        b'G' => CMD_TONEPORTAMENTO,
        b'H' => CMD_VIBRATO,
        b'I' => CMD_TREMOR,
        b'J' => CMD_ARPEGGIO,
        b'K' => CMD_VIBRATOVOL,
        b'L' => CMD_TONEPORTAVOL,
        b'M' => CMD_CHANNELVOLUME,
        b'N' => CMD_CHANNELVOLSLIDE,
        b'O' => CMD_OFFSET,
        b'P' => CMD_PANNINGSLIDE,
        b'Q' => CMD_RETRIG,
        b'R' => CMD_TREMOLO,
        b'S' => CMD_S3MCMDEX,
        b'T' => CMD_TEMPO,
        b'U' => CMD_FINEVIBRATO,
        b'V' => CMD_GLOBALVOLUME,
        b'W' => CMD_GLOBALVOLSLIDE,
        b'X' => CMD_PANNING8,
        b'Y' => CMD_PANBRELLO,
        b'Z' => CMD_MIDI,
        b'\\' => {
            if from_it {
                CMD_SMOOTHMIDI
            } else {
                CMD_MIDI
            }
        }
        b']' => {
            if from_it {
                CMD_DELAYCUT
            } else {
                CMD_NONE
            }
        }
        b'[' => {
            if from_it {
                CMD_XPARAM
            } else {
                CMD_NONE
            }
        }
        b'^' => {
            if from_it {
                CMD_FINETUNE
            } else {
                CMD_NONE
            }
        }
        b'_' => {
            if from_it {
                CMD_FINETUNE_SMOOTH
            } else {
                CMD_NONE
            }
        }
        0x72 => {
            if from_it {
                CMD_KEYOFF
            } else {
                CMD_NONE
            }
        }
        0x73 => {
            if from_it {
                CMD_SETENVPOSITION
            } else {
                CMD_NONE
            }
        }
        _ => CMD_NONE,
    };
}

/// `S3MSampleHeader` (80 bytes).
pub struct S3mSampleHeader<'a>(pub &'a [u8]);

impl S3mSampleHeader<'_> {
    pub fn sample_type(&self) -> u8 {
        self.0[0]
    }
    pub fn length(&self) -> u32 {
        le32(self.0, 16)
    }
    pub fn loop_start(&self) -> u32 {
        le32(self.0, 20)
    }
    pub fn loop_end(&self) -> u32 {
        le32(self.0, 24)
    }
    pub fn default_volume(&self) -> u8 {
        self.0[28]
    }
    pub fn pack(&self) -> u8 {
        self.0[30]
    }
    pub fn flags(&self) -> u8 {
        self.0[31]
    }
    pub fn c5speed(&self) -> u32 {
        le32(self.0, 32)
    }
    pub fn gus_address(&self) -> u16 {
        le16(self.0, 40)
    }
    pub fn name(&self) -> &[u8] {
        &self.0[48..76]
    }
    /// `GetSampleOffset`.
    pub fn sample_offset(&self) -> usize {
        ((self.0[14] as usize) << 4) | ((self.0[15] as usize) << 12) | ((self.0[13] as usize) << 20)
    }

    /// `ConvertToMPT`.
    pub fn convert_to_mpt(&self, s: &mut ModSample, is_st3: bool) {
        s.initialize(MOD_TYPE_S3M);
        let t = self.sample_type();
        if t == 1 || t == 0 {
            if t == 1 {
                s.n_length = self.length();
                s.n_loop_start = self.loop_start().min(s.n_length.wrapping_sub(1));
                s.n_loop_end = self.loop_end().min(s.n_length);
                if self.flags() & 0x01 != 0 {
                    s.u_flags |= CHN_LOOP;
                } else {
                    s.u_flags &= !CHN_LOOP;
                }
            }
            if s.n_loop_end < 2 || s.n_loop_start >= s.n_loop_end || s.n_loop_end - s.n_loop_start < 1 {
                s.n_loop_start = 0;
                s.n_loop_end = 0;
                s.u_flags = 0;
            }
        } else if t == 2 {
            s.adlib.copy_from_slice(&self.0[16..28]);
            s.u_flags |= CHN_ADLIB;
            s.u_flags &= !(CHN_16BIT | CHN_STEREO);
        }
        s.n_volume = self.default_volume().min(64) as u16 * 4;
        s.n_c5_speed = self.c5speed();
        if is_st3 {
            if t == 2 {
                s.n_c5_speed &= 0xFFFF;
            } else {
                s.n_c5_speed = s.n_c5_speed.min(u16::MAX as u32);
            }
        }
        if s.n_c5_speed == 0 {
            s.n_c5_speed = 8363;
        } else if s.n_c5_speed < 1024 {
            s.n_c5_speed = 1024;
        }
    }

    /// `GetSampleFormat`.
    pub fn sample_format(&self, signed: bool) -> SampleIo {
        if self.pack() == 0x04 && self.flags() & 0x04 == 0 && self.flags() & 0x02 == 0 {
            SampleIo::new(8, Channels::Mono, false, Encoding::Adpcm)
        } else {
            SampleIo::new(
                if self.flags() & 0x04 != 0 { 16 } else { 8 },
                if self.flags() & 0x02 != 0 { Channels::StereoSplit } else { Channels::Mono },
                false,
                if signed { Encoding::Signed } else { Encoding::Unsigned },
            )
        }
    }
}

fn header_ok(h: &[u8]) -> bool {
    let format_version = le16(h, 0x2A);
    &h[0x2C..0x30] == b"SCRM" && h[0x1D] == 0x10 && (format_version == 1 || format_version == 2)
}

/// `ProbeFileHeaderS3M`.
pub fn probe(data: &[u8]) -> bool {
    data.len() >= 96 && header_ok(&data[..96])
}

/// `ReadS3M`.
pub fn read(data: &[u8]) -> Option<Module> {
    read_with_work_limits(data, MAX_MODULE_PATTERN_WORK, MAX_PATTERN_WORK)
}

// Explicit limits keep resource-bound regressions small and deterministic.
fn read_with_work_limits(data: &[u8], mut work_left: usize, pattern_limit: usize) -> Option<Module> {
    let mut file = Reader::new(data);
    let h = file.read_slice(96)?;
    if !header_ok(h) {
        return None;
    }
    let ord_num = le16(h, 0x20) as usize;
    let smp_num = le16(h, 0x22) as usize;
    let pat_num = le16(h, 0x24) as usize;
    let flags = le16(h, 0x26);
    let cwtv = le16(h, 0x28);
    let format_version = le16(h, 0x2A);
    let global_vol = h[0x30];
    let speed = h[0x31];
    let tempo = h[0x32];
    let master_volume = h[0x33];
    let ultra_clicks = h[0x34];
    let use_panning_table = h[0x35] == 0xFC;
    let reserved2 = le16(h, 0x36);
    let reserved3 = le32(h, 0x38);
    let special = le16(h, 0x3E);
    let channels = &h[0x40..0x60];
    if !file.can_read(ord_num + (smp_num + pat_num) * 2) {
        return None;
    }
    let mut num_channels = 4;
    for (i, &c) in channels.iter().enumerate() {
        if c != 0xFF {
            num_channels = i + 1;
        }
    }
    let mut m = Module::new(MOD_TYPE_S3M, num_channels);
    m.min_period = 64;
    m.max_period = 32767;
    m.order = file
        .read_slice(ord_num)?
        .iter()
        .map(|&p| match p {
            0xFF => PATTERNINDEX_INVALID,
            0xFE => PATTERNINDEX_SKIP,
            p => p as PatternIndex,
        })
        .collect();
    let sample_offsets: Vec<u16> = (0..smp_num).map(|_| file.u16le()).collect();
    let pattern_offsets: Vec<u16> = (0..pat_num).map(|_| file.u16le()).collect();

    let mut keep_midi_macros = false;
    let mut non_compat = false;
    let mut is_st3 = false;
    let mut is_schism = false;
    let offsets_canonical = !pattern_offsets.is_empty() && !sample_offsets.is_empty() && pattern_offsets[0] > sample_offsets[0];
    let schism_date = schism_epoch_date() + if cwtv == 0x4FFF { reserved2 as i32 } else { cwtv as i32 - 0x4050 };
    match cwtv & TRK_MASK {
        x if x == TRK_AKORD & TRK_MASK => {}
        TRK_SCREAM => {
            if &h[0x36..0x3E] == b"SCLUB2.0" {
            } else if cwtv == TRK_ST3_20
                && special == 0
                && (ord_num & 1) == 0
                && ultra_clicks == 0
                && (flags & !0x50) == 0
                && use_panning_table
                && offsets_canonical
            {
                if (ord_num & 0x0F) == 0 {
                    m.last_saved_with_version = if master_volume & 0x80 != 0 { mpt_v("1.16") } else { mpt_v("1.00.00.A0") };
                }
                keep_midi_macros = true;
                non_compat = true;
                m.play_behaviour[kST3LimitPeriod] = true;
            } else if cwtv == TRK_ST3_20 && special == 0 && ultra_clicks == 0 && (flags == 0 || flags == 8) && !use_panning_table {
            } else {
                is_st3 = true;
            }
        }
        TRK_IMAGO => non_compat = true,
        TRK_IT => {
            non_compat = true;
            m.play_behaviour[kPeriodsAreHertz] = true;
            m.play_behaviour[kITRetrigger] = true;
            m.play_behaviour[kITShortSampleRetrig] = true;
            m.play_behaviour[kST3SampleSwap] = true;
            m.play_behaviour[kITPortaNoNote] = true;
            m.play_behaviour[kITPortamentoSwapResetsPos] = true;
            m.min_period = 1;
        }
        TRK_SCHISM => {
            if cwtv == TRK_BERO_OLD {
                m.play_behaviour[kST3LimitPeriod] = true;
            } else {
                m.min_period = 1;
                is_schism = true;
                if schism_date >= crate::version::schism_date(2021, 5, 2) {
                    m.play_behaviour[kPeriodsAreHertz] = true;
                }
                if schism_date >= crate::version::schism_date(2016, 5, 13) {
                    m.play_behaviour[kITShortSampleRetrig] = true;
                }
                m.play_behaviour[kST3TonePortaWithAdlibNote] = false;
            }
            non_compat = true;
        }
        TRK_OPENMPT => {
            if (cwtv & 0xFF00) == TRK_NESMUSA {
            } else if reserved2 == 0 && ultra_clicks == 16 && channels[1] != 1 {
            } else if cwtv != TRK_GRAOUMF {
                let mut v = ((cwtv & 0x0FFF) as u32) << 16;
                if v >= 0x0129_0000 {
                    v |= reserved2 as u32;
                }
                m.last_saved_with_version = v;
            }
        }
        TRK_BERO => m.play_behaviour[kST3LimitPeriod] = true,
        _ => {}
    }
    let _ = reserved3;
    m.format_name = "Scream Tracker 3".into();
    if non_compat {
        for b in [
            kST3NoMutedChannels,
            kST3EffectMemory,
            kST3PortaSampleChange,
            kST3VibratoMemory,
            KST3PortaAfterArpeggio,
            kST3OffsetWithoutInstrument,
            kApplyUpperPeriodLimit,
        ] {
            m.play_behaviour[b] = false;
        }
    }
    if cwtv <= TRK_ST3_01 {
        m.play_behaviour[kST3TonePortaWithAdlibNote] = false;
    }
    if (cwtv & TRK_MASK) > TRK_SCREAM && ((cwtv & TRK_MASK) != TRK_IT || cwtv >= TRK_IT2_14) {
        keep_midi_macros = true;
    }
    m.midi_cfg = crate::midimacro::MidiMacroConfig::default();
    if !keep_midi_macros {
        m.midi_cfg.clear_zxx();
    }
    m.title = read_name(&h[..28], false);
    if flags & 0x10 != 0 {
        m.song_flags |= SONG_AMIGALIMITS;
    }
    if flags & 0x01 != 0 {
        m.song_flags |= SONG_S3MOLDVIBRATO;
    }
    if cwtv == TRK_ST3_00 || (flags & 0x40) != 0 {
        m.song_flags |= SONG_FASTVOLSLIDES;
    }
    m.default_speed = if speed == 0 || (speed == 255 && is_st3) { 6 } else { speed as u32 };
    m.default_tempo = if tempo < 33 { Tempo::new(if is_st3 { 125 } else { 32 }, 0) } else { Tempo::new(tempo as u32, 0) };
    m.default_global_volume = global_vol.min(64) as u32 * 4;
    if m.default_global_volume == 0 && cwtv < TRK_ST3_20 {
        m.default_global_volume = MAX_GLOBAL_VOLUME;
    }
    if format_version == 1 && master_volume < 8 {
        m.sample_pre_amp = ((master_volume as u32 + 1) * 0x10).min(0x7F);
    } else if master_volume == 2 || master_volume == (2 | 0x10) {
        m.sample_pre_amp = 0x20;
    } else if master_volume & 0x7F == 0 {
        m.sample_pre_amp = 48;
    } else {
        m.sample_pre_amp = ((master_volume & 0x7F) as u32).max(0x10);
    }
    m.vsti_volume = 36;
    if is_schism && schism_date < crate::version::schism_date(2018, 11, 12) {
        m.vsti_volume = 64;
    }
    let is_stereo = (master_volume & 0x80) != 0 || m.last_saved_with_version != 0;
    if !is_stereo {
        m.sample_pre_amp = muldivr_unsigned(m.sample_pre_amp, 8, 11);
        m.vsti_volume = muldivr_unsigned(m.vsti_volume, 8, 11);
    }
    let mut is_adlib_channel = [false; 32];
    for i in 0..m.num_channels() {
        let ctype = channels[i] & !0x80;
        if channels[i] != 0xFF && is_stereo {
            m.chn_settings[i].n_pan = if ctype & 8 != 0 { 0xCC } else { 0x33 };
        }
        if channels[i] & 0x80 != 0 {
            m.chn_settings[i].dw_flags = CHN_MUTE;
        }
        if (16..=29).contains(&ctype) {
            m.chn_settings[i].n_pan = 128;
            is_adlib_channel[i] = true;
        }
    }
    if use_panning_table {
        let pan = file.read_array::<32>();
        for i in 0..m.num_channels() {
            if (pan[i] & 0x20) != 0 && (!is_st3 || !is_adlib_channel[i]) {
                m.chn_settings[i].n_pan = (((pan[i] & 0x0F) as u16) * 256 + 8) / 15;
            }
        }
    }

    m.num_samples = smp_num.min(MAX_SAMPLES - 1) as SampleIndex;
    m.samples.resize_with(m.num_samples as usize + 1, || ModSample::new(MOD_TYPE_S3M));
    let mut any_samples = false;
    let mut gus_addresses: u16 = 0;
    for smp in 0..m.num_samples as usize {
        if !file.seek(sample_offsets[smp] as usize * 16) {
            continue;
        }
        let Some(hb) = file.read_slice(80) else {
            continue;
        };
        let sh = S3mSampleHeader(hb);
        let s = &mut m.samples[smp + 1];
        sh.convert_to_mpt(s, is_st3);
        s.name = read_name(sh.name(), false);
        if sh.sample_type() < 2 {
            if sh.length() != 0 {
                let io = sh.sample_format(format_version == 1);
                if file.seek(sh.sample_offset()) {
                    io.read_sample(s, &mut file);
                }
                any_samples = true;
            }
            gus_addresses |= sh.gus_address();
        }
    }
    let use_gus = gus_addresses > 1;
    if is_st3 && any_samples && gus_addresses == 0 && cwtv != TRK_ST3_00 {
        is_st3 = false;
    } else if is_st3 {
        m.play_behaviour[kST3PortaSampleChange] = use_gus;
        m.play_behaviour[kST3SampleSwap] = !use_gus;
        m.play_behaviour[kITShortSampleRetrig] = !use_gus;
        if use_gus {
            m.sample_pre_amp = 48;
        }
    }
    if is_st3 {
        m.play_behaviour[kS3MIgnoreCombinedFineSlides] = true;
    }

    let mut pix_play = cwtv < TRK_ST3_20;
    let (mut zxx_right, mut zxx_left) = (0i32, 0i32);
    let read_patterns = pat_num.min(255);
    let nc = m.num_channels();
    m.patterns = vec![Pattern::default(); read_patterns];
    for pat in 0..read_patterns {
        m.patterns[pat] = Pattern { rows: 64, data: vec![ModCommand::default(); 64 * nc], ..Default::default() };
        if pattern_offsets[pat] == 0 || !file.seek(pattern_offsets[pat] as usize * 16) {
            continue;
        }
        // OpenMPT ignores incorrect packed lengths (Load_s3m.cpp:652-657).
        // Bound recovery independently, without trusting the length or the
        // next parapointer: patterns can share or overlap their source bytes.
        work_left = work_left.checked_sub(2)?;
        let mut pattern_work_left = pattern_limit.checked_sub(2)?;
        file.skip(2);
        let mut row = 0usize;
        let mut dummy = ModCommand::default();
        while row < 64 {
            if work_left == 0 || pattern_work_left == 0 {
                return None;
            }
            let info = file.u8();
            // Charge even inactive-channel tokens, row ends and missing
            // operands which Reader zero-fills at EOF. No operand is decoded
            // unless both budgets cover the complete token.
            let token_work = 1 + 2 * usize::from(info & 0x20 != 0)
                + usize::from(info & 0x40 != 0) + 2 * usize::from(info & 0x80 != 0);
            work_left = work_left.checked_sub(token_work)?;
            pattern_work_left = pattern_work_left.checked_sub(token_work)?;
            if info == 0 {
                row += 1;
                continue;
            }
            let channel = (info & 0x1F) as usize;
            let mc = if channel < nc { &mut m.patterns[pat].data[row * nc + channel] } else { &mut dummy };
            if info & 0x20 != 0 {
                let [note, instr] = file.read_array::<2>();
                if note < 0xF0 {
                    mc.note = ((note & 0x0F) as i32 + 12 * (note >> 4) as i32 + 12 + NOTE_MIN as i32).clamp(NOTE_MIN as i32, NOTE_MAX as i32) as u8;
                } else if note == 0xFE {
                    mc.note = NOTE_NOTECUT;
                } else if note == 0xFF {
                    mc.note = NOTE_NONE;
                }
                mc.instr = instr;
            }
            if info & 0x40 != 0 {
                let volume = file.u8();
                if (128..=192).contains(&volume) {
                    mc.volcmd = VOLCMD_PANNING;
                    mc.vol = volume - 128;
                } else {
                    mc.volcmd = VOLCMD_VOLUME;
                    mc.vol = volume.min(64);
                }
            }
            if info & 0x80 != 0 {
                let [command, param] = file.read_array::<2>();
                s3m_convert(mc, command, param, false);
                if mc.command == CMD_S3MCMDEX && (mc.param & 0xF0) == 0xA0 && cwtv < TRK_ST3_20 {
                    let ctype = channels.get(channel).copied().unwrap_or(0) & 0x7F;
                    if use_gus || ctype >= 0x10 {
                        mc.command = CMD_DUMMY;
                    } else if mc.param == 0xA0 || mc.param == 0xA2 {
                        mc.param = if ctype & 8 != 0 { 0x8C } else { 0x83 };
                    } else if mc.param == 0xA1 || mc.param == 0xA3 {
                        mc.param = if ctype & 8 != 0 { 0x83 } else { 0x8C };
                    } else if mc.param <= 0xA7 {
                        mc.param = 0x88;
                    } else {
                        mc.command = CMD_DUMMY;
                    }
                } else if mc.command == CMD_MIDI {
                    if mc.param > 0x0F {
                        pix_play = false;
                    } else if mc.param < 0x08 {
                        zxx_left += 1;
                    } else if mc.param > 0x08 {
                        zxx_right += 1;
                    }
                } else if mc.command == CMD_OFFSET && mc.param == 0 && is_st3 && cwtv <= TRK_ST3_01 {
                    mc.command = CMD_DUMMY;
                }
            }
        }
    }
    if pix_play && zxx_left + zxx_right >= nc as i32 && (-zxx_left + zxx_right) < nc as i32 {
        for p in m.patterns.iter_mut() {
            for c in p.data.iter_mut() {
                if c.command == CMD_MIDI {
                    c.command = CMD_S3MCMDEX;
                    c.param |= 0x80;
                }
            }
        }
    }
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_patterns(second_parapointer: u16) -> Vec<u8> {
        let mut data = include_bytes!("../tests/fixtures/tone.s3m").to_vec();
        data[36..38].copy_from_slice(&2u16.to_le_bytes());
        // The fixture leaves room for another pointer before its sample.
        data[102..104].copy_from_slice(&second_parapointer.to_le_bytes());
        data
    }

    #[test]
    fn s3m_pattern_budget_counts_shared_and_overlapping_regions() {
        // The first pattern costs 2 header bytes + 70 packed bytes. An alias
        // repeats that work; offset 13 points into its empty rows (2 + 64).
        for (pointer, work, second_note) in [(12, 144, NOTE_MIN + 60), (13, 138, NOTE_NONE)] {
            let data = two_patterns(pointer);
            let module = read_with_work_limits(&data, work, 72).unwrap();
            assert_eq!(module.patterns[0].data[0].note, NOTE_MIN + 60);
            assert_eq!(module.patterns[1].data[0].note, second_note);
            assert!(
                read_with_work_limits(&data, work - 1, 72).is_none(),
                "shared or overlapping pattern work must debit the module budget"
            );
        }
    }

    #[test]
    fn s3m_pattern_budget_bounds_each_recovery() {
        let data = include_bytes!("../tests/fixtures/tone.s3m");
        // Enough aggregate work is not permission for unbounded recovery of
        // one pattern. Both payload operands and row markers consume work.
        assert!(read_with_work_limits(data, 144, 71).is_none());
        let module = read_with_work_limits(data, 144, 72).unwrap();
        assert_eq!(module.patterns[0].data[0].instr, 1);
        assert_eq!(module.patterns[0].data[module.num_channels()].command, CMD_PATTERNBREAK);
    }

    #[test]
    fn s3m_pattern_budget_counts_zero_filled_eof_reads() {
        // Retain native recovery at physical EOF: the note token is present,
        // its two operands are absent, and 64 implicit row ends follow.
        let data = &include_bytes!("../tests/fixtures/tone.s3m")[..195];
        let module = read_with_work_limits(data, 69, 69).unwrap();
        assert_eq!(module.patterns[0].data[0].note, NOTE_MIN + 12);
        assert!(read_with_work_limits(data, 68, 69).is_none());
        assert!(read_with_work_limits(data, 69, 68).is_none());
    }

    #[test]
    fn s3m_incorrect_lengths_keep_complete_pattern_commands() {
        for length in [0u16, 1, u16::MAX] {
            let mut data = include_bytes!("../tests/fixtures/tone.s3m").to_vec();
            data[192..194].copy_from_slice(&length.to_le_bytes());
            let module = read_with_work_limits(&data, 72, 72).unwrap();
            assert_eq!(module.patterns[0].data[0].note, NOTE_MIN + 60);
            assert_eq!(module.patterns[0].data[0].instr, 1);
            assert_eq!(module.patterns[0].data[module.num_channels()].command, CMD_PATTERNBREAK);
        }
    }
}
