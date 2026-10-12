//! FastTracker 2 XM modules, ported from libopenmpt 0.8.9
//! `soundlib/Load_xm.cpp` and `XMTools.cpp`. OggMod (Vorbis-packed)
//! samples are not decoded.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

use crate::command::*;
use crate::defs::pb::*;
use crate::defs::*;
use crate::instrument::{EnvelopeNode, InstrumentEnvelope, ModInstrument};
use crate::io::{Channels, Encoding, Reader, SampleIo, le16, le32, read_name};
use crate::load_mod::convert_mod_command;
use crate::sample::ModSample;
use crate::sndfile::{MixLevels, Module, Pattern};
use crate::version::mpt_v;

const VER_UNKNOWN: u32 = 0;
const VER_OLD_MODPLUG: u32 = 0x01;
const VER_NEW_MODPLUG: u32 = 0x02;
const VER_MODPLUG_BIDI: u32 = 0x04;
const VER_OPENMPT: u32 = 0x08;
const VER_CONFIRMED: u32 = 0x10;
const VER_FT2_GENERIC: u32 = 0x20;
const VER_FT2_CLONE: u32 = 0x80;
const VER_PLAYERPRO: u32 = 0x100;
const VER_DIGITRAKKER: u32 = 0x200;
const VER_EMPTY_ORDERS: u32 = 0x800;

/// The total pattern cells a module may allocate (memory bound).
pub const MAX_PATTERN_CELLS: usize = 4 << 20;

struct Header<'a>(&'a [u8]);

impl Header<'_> {
    fn signature_ok(&self) -> bool {
        &self.0[..17] == b"Extended Module: "
    }
    fn song_name(&self) -> &[u8] {
        &self.0[17..37]
    }
    fn tracker_name(&self) -> &[u8] {
        &self.0[38..58]
    }
    fn version(&self) -> u16 {
        le16(self.0, 58)
    }
    fn size(&self) -> u32 {
        le32(self.0, 60)
    }
    fn orders(&self) -> u16 {
        le16(self.0, 64)
    }
    fn restart_pos(&self) -> u16 {
        le16(self.0, 66)
    }
    fn channels(&self) -> u16 {
        le16(self.0, 68)
    }
    fn patterns(&self) -> u16 {
        le16(self.0, 70)
    }
    fn instruments(&self) -> u16 {
        le16(self.0, 72)
    }
    fn flags(&self) -> u16 {
        le16(self.0, 74)
    }
    fn speed(&self) -> u16 {
        le16(self.0, 76)
    }
    fn tempo(&self) -> u16 {
        le16(self.0, 78)
    }
}

/// `ProbeFileHeaderXM`.
pub fn probe(data: &[u8]) -> bool {
    if data.len() < 80 {
        return false;
    }
    let h = Header(&data[..80]);
    h.channels() != 0 && h.channels() as usize <= MAX_BASECHANNELS && h.signature_ok()
}

/// `XMInstrument` (230 bytes) at offset 29 of the instrument header.
struct XmInstrument<'a>(&'a [u8]);

impl XmInstrument<'_> {
    fn sample_map(&self, i: usize) -> u8 {
        self.0[i]
    }
    fn vol_env(&self, i: usize) -> u16 {
        le16(self.0, 96 + i * 2)
    }
    fn pan_env(&self, i: usize) -> u16 {
        le16(self.0, 144 + i * 2)
    }
    fn b(&self, o: usize) -> u8 {
        self.0[o]
    }
    fn vol_fade(&self) -> u16 {
        le16(self.0, 206)
    }
    fn midi_enabled(&self) -> u8 {
        self.0[208]
    }
    fn midi_channel(&self) -> u8 {
        self.0[209]
    }
    fn midi_program(&self) -> u16 {
        le16(self.0, 210)
    }
    fn pitch_wheel_range(&self) -> u16 {
        le16(self.0, 212)
    }
    fn mute_computer(&self) -> u8 {
        self.0[214]
    }

    /// `ConvertEnvelopeToMPT`.
    #[allow(clippy::too_many_arguments)]
    fn convert_envelope(&self, env: &mut InstrumentEnvelope, num_points: u8, flags: u8, sustain: u8, loop_start: u8, loop_end: u8, vol: bool) {
        let n = num_points.min(12) as usize;
        env.nodes = vec![EnvelopeNode::default(); n];
        for i in 0..n {
            let (tick, value) = if vol { (self.vol_env(i * 2), self.vol_env(i * 2 + 1)) } else { (self.pan_env(i * 2), self.pan_env(i * 2 + 1)) };
            env.nodes[i].tick = tick;
            env.nodes[i].value = value as u8;
            if i > 0 && env.nodes[i].tick < env.nodes[i - 1].tick && env.nodes[i].tick & 0xFF00 == 0 {
                env.nodes[i].tick |= env.nodes[i - 1].tick & 0xFF00;
                if env.nodes[i].tick < env.nodes[i - 1].tick {
                    env.nodes[i].tick = env.nodes[i].tick.wrapping_add(0x100);
                }
            }
        }
        env.flags = 0;
        if flags & 0x01 != 0 && !env.nodes.is_empty() {
            env.flags |= ENV_ENABLED;
        }
        if sustain < 12 {
            if flags & 0x02 != 0 {
                env.flags |= ENV_SUSTAIN;
            }
            env.sustain_start = sustain;
            env.sustain_end = sustain;
        }
        if loop_end < 12 && loop_end >= loop_start {
            if flags & 0x04 != 0 {
                env.flags |= ENV_LOOP;
            }
            env.loop_start = loop_start;
            env.loop_end = loop_end;
        }
    }

    /// `XMInstrument::ConvertToMPT`.
    fn convert_to_mpt(&self, ins: &mut ModInstrument) {
        ins.n_fade_out = self.vol_fade() as u32;
        self.convert_envelope(&mut ins.vol_env, self.b(192), self.b(200), self.b(194), self.b(195), self.b(196), true);
        self.convert_envelope(&mut ins.pan_env, self.b(193), self.b(201), self.b(197), self.b(198), self.b(199), false);
        for i in 0..96 {
            ins.keyboard[i + 12] = self.sample_map(i) as SampleIndex;
        }
        if self.midi_enabled() != 0 {
            ins.n_midi_channel = (self.midi_channel() + 1).clamp(1, 16);
        }
        ins.midi_pwd = self.pitch_wheel_range() as i8;
    }
}

/// `XMSample` (40 bytes).
struct XmSample<'a>(&'a [u8]);

impl XmSample<'_> {
    fn length(&self) -> u32 {
        le32(self.0, 0)
    }
    fn loop_start(&self) -> u32 {
        le32(self.0, 4)
    }
    fn loop_length(&self) -> u32 {
        le32(self.0, 8)
    }
    fn vol(&self) -> u8 {
        self.0[12]
    }
    fn finetune(&self) -> i8 {
        self.0[13] as i8
    }
    fn flags(&self) -> u8 {
        self.0[14]
    }
    fn pan(&self) -> u8 {
        self.0[15]
    }
    fn relnote(&self) -> i8 {
        self.0[16] as i8
    }
    fn reserved(&self) -> u8 {
        self.0[17]
    }
    fn name(&self) -> &[u8] {
        &self.0[18..40]
    }

    fn convert_to_mpt(&self, s: &mut ModSample) {
        s.initialize(MOD_TYPE_XM);
        s.n_volume = (self.vol() as u16 * 4).min(256);
        s.n_pan = self.pan() as u16;
        s.u_flags = CHN_PANNING;
        s.n_fine_tune = self.finetune();
        s.relative_tone = self.relnote();
        s.n_length = self.length();
        s.n_loop_start = self.loop_start();
        s.n_loop_end = s.n_loop_start.wrapping_add(self.loop_length());
        if self.flags() & 0x10 != 0 {
            s.n_length /= 2;
            s.n_loop_start /= 2;
            s.n_loop_end /= 2;
        }
        if self.flags() & 0x20 != 0 {
            s.n_length /= 2;
            s.n_loop_start /= 2;
            s.n_loop_end /= 2;
        }
        if self.flags() & 0x03 != 0 && s.n_loop_end > s.n_loop_start {
            s.u_flags |= CHN_LOOP;
            if self.flags() & 0x02 != 0 {
                s.u_flags |= CHN_PINGPONGLOOP;
            }
        }
    }

    fn sample_format(&self) -> SampleIo {
        if self.reserved() == 0xAD && self.flags() & (0x10 | 0x20) == 0 {
            return SampleIo::new(8, Channels::Mono, false, Encoding::Adpcm);
        }
        SampleIo::new(
            if self.flags() & 0x10 != 0 { 16 } else { 8 },
            if self.flags() & 0x20 != 0 { Channels::StereoSplit } else { Channels::Mono },
            false,
            Encoding::Delta,
        )
    }
}

/// `ReadXMPatterns`.
fn read_xm_patterns(file: &mut Reader, version: u16, num_patterns: usize, tracker_name: &[u8], m: &mut Module) {
    let is_nitro = &tracker_name[..13.min(tracker_name.len())] == b"NitroTracker\0";
    let nc = m.num_channels();
    m.patterns = vec![Pattern::default(); num_patterns];
    let mut cells = 0usize;
    for pat in 0..num_patterns {
        let cur = file.pos;
        let header_size = file.u32le() as usize;
        if header_size < 8 || !file.can_read(header_size - 4) {
            break;
        }
        file.skip(1);
        let mut num_rows: u32 = if version == 0x0102 { file.u8() as u32 + 1 } else { file.u16le() as u32 };
        let packed_size = file.u16le() as usize;
        if num_rows == 0 {
            num_rows = 64;
        } else if num_rows > MAX_PATTERN_ROWS {
            num_rows = MAX_PATTERN_ROWS;
        }
        file.seek(cur + header_size);
        let mut chunk = file.read_chunk(packed_size);
        if pat >= MAX_PATTERNS || cells + num_rows as usize * nc > MAX_PATTERN_CELLS {
            continue;
        }
        cells += num_rows as usize * nc;
        m.patterns[pat] = Pattern { rows: num_rows, data: vec![ModCommand::default(); num_rows as usize * nc], ..Default::default() };
        if packed_size == 0 {
            continue;
        }
        for mc in m.patterns[pat].data.iter_mut() {
            if !file.can_read(1) {
                break;
            }
            let mut info = chunk.u8();
            let (mut vol, mut command) = (0u8, 0u8);
            if info & 0x80 != 0 {
                if info & 0x01 != 0 {
                    mc.note = chunk.u8();
                }
            } else {
                mc.note = info;
                info = 0xFF;
            }
            if info & 0x02 != 0 {
                mc.instr = chunk.u8();
            }
            if info & 0x04 != 0 {
                vol = chunk.u8();
            }
            if info & 0x08 != 0 {
                command = chunk.u8();
            }
            if info & 0x10 != 0 {
                mc.param = chunk.u8();
            }
            if mc.note == 97 {
                mc.note = NOTE_KEYOFF;
            } else if mc.note > 0 && mc.note < 97 {
                mc.note += 12;
            } else {
                mc.note = NOTE_NONE;
            }
            if command | mc.param != 0 {
                let p = mc.param;
                convert_mod_command(mc, command, p);
            } else {
                mc.command = CMD_NONE;
            }
            if mc.instr == 0xFF {
                mc.instr = 0;
            } else if is_nitro && mc.instr != 0 && !mc.is_note() {
                mc.instr = 0;
            }
            if (0x10..=0x50).contains(&vol) {
                mc.volcmd = VOLCMD_VOLUME;
                mc.vol = vol - 0x10;
            } else if vol >= 0x60 {
                const TRANS: [u8; 10] = [
                    VOLCMD_VOLSLIDEDOWN,
                    VOLCMD_VOLSLIDEUP,
                    VOLCMD_FINEVOLDOWN,
                    VOLCMD_FINEVOLUP,
                    VOLCMD_VIBRATOSPEED,
                    VOLCMD_VIBRATODEPTH,
                    VOLCMD_PANNING,
                    VOLCMD_PANSLIDELEFT,
                    VOLCMD_PANSLIDERIGHT,
                    VOLCMD_TONEPORTAMENTO,
                ];
                mc.volcmd = TRANS[((vol - 0x60) >> 4) as usize];
                mc.vol = vol & 0x0F;
                if mc.volcmd == VOLCMD_PANNING {
                    mc.vol *= 4;
                }
            }
        }
    }
}

/// `AllocateXMSamples` (without the unused-sample reclamation past 4000 slots).
fn allocate_xm_samples(m: &mut Module, num: usize) -> Vec<SampleIndex> {
    let num = num.min(32);
    let mut found = Vec::with_capacity(num);
    for _ in 0..num {
        let mut candidate = m.num_samples as usize + 1;
        if candidate >= MAX_SAMPLES {
            for j in 1..=m.num_samples as usize {
                if m.samples[j].has_sample_data() || found.contains(&(j as SampleIndex)) {
                    continue;
                }
                candidate = j;
                for ins in m.instruments.iter_mut().flatten() {
                    for k in ins.keyboard.iter_mut() {
                        if *k as usize == candidate {
                            *k = 0;
                        }
                    }
                }
                break;
            }
        }
        if candidate >= MAX_SAMPLES {
            break;
        }
        found.push(candidate as SampleIndex);
        if candidate > m.num_samples as usize {
            m.num_samples = candidate as SampleIndex;
            m.samples.resize_with(candidate + 1, || ModSample::new(MOD_TYPE_XM));
        }
    }
    found
}

/// `ReadXM`.
pub fn read(data: &[u8]) -> Option<Module> {
    let mut file = Reader::new(data);
    let hb = file.read_slice(80)?;
    let h = Header(hb);
    if h.channels() == 0 || h.channels() as usize > MAX_BASECHANNELS || !h.signature_ok() {
        return None;
    }
    if !file.can_read(h.orders() as usize + 4 * (h.patterns() as usize + h.instruments() as usize)) {
        return None;
    }
    let mut m = Module::new(MOD_TYPE_XM, h.channels() as usize);
    m.set_mix_levels(MixLevels::Compatible);
    let mut mix_levels = MixLevels::Compatible;
    let mut made_with = VER_UNKNOWN;
    let tracker = h.tracker_name();
    if tracker == b"FastTracker v2.00   " && h.size() == 276 {
        let song_name = h.song_name();
        if h.version() < 0x0104 {
            made_with = VER_FT2_GENERIC | VER_CONFIRMED;
        } else if let Some(first_null) = song_name.iter().position(|&c| c == 0) {
            if h.restart_pos() != 0 {
                made_with = VER_FT2_CLONE | VER_NEW_MODPLUG | VER_EMPTY_ORDERS;
            } else if first_null == song_name.len() - 1 {
                made_with = VER_FT2_CLONE | VER_NEW_MODPLUG | VER_PLAYERPRO | VER_EMPTY_ORDERS;
            } else if song_name[first_null + 1..].iter().all(|&c| c == b' ') {
                made_with = VER_PLAYERPRO | VER_CONFIRMED;
            } else {
                made_with = VER_FT2_CLONE | VER_NEW_MODPLUG | VER_EMPTY_ORDERS;
            }
        } else if h.restart_pos() != 0 {
            made_with = VER_FT2_GENERIC | VER_NEW_MODPLUG;
        } else {
            made_with = VER_FT2_GENERIC | VER_NEW_MODPLUG | VER_PLAYERPRO;
        }
    } else if tracker == b"FastTracker v 2.00  " {
        made_with = VER_OLD_MODPLUG;
    } else {
        made_with = VER_UNKNOWN | VER_CONFIRMED;
        if &tracker[..8] == b"OpenMPT " {
            made_with = VER_OPENMPT | VER_CONFIRMED | VER_EMPTY_ORDERS;
        } else if &tracker[..12] == b"MilkyTracker" && tracker[12] == b' ' {
            if &tracker[12..20] != b"        " {
                mix_levels = MixLevels::CompatibleFT2;
            }
        } else if tracker == b"Fasttracker II clone" {
            made_with = VER_FT2_GENERIC | VER_CONFIRMED;
        } else if &tracker[..15] == b"MadTracker 2.0\0" {
            m.play_behaviour[kFT2PortaNoNote] = false;
            m.play_behaviour[kFT2Arpeggio] = false;
        } else if &tracker[..14] == b"Skale Tracker\0" || &tracker[..14] == b"Sk@le Tracker\0" {
            m.play_behaviour[kFT2ST3OffsetOutOfRange] = false;
            m.play_behaviour[kFT2Arpeggio] = false;
        } else if &tracker[..11] == b"*Converted " && &tracker[14..20] == b"-File*" {
            made_with = VER_DIGITRAKKER | VER_CONFIRMED;
        }
    }
    m.title = read_name(h.song_name(), true);
    m.min_period = 1;
    m.max_period = 31999;
    m.restart_pos = h.restart_pos();
    m.num_instruments = h.instruments().min((MAX_INSTRUMENTS - 1) as u16);
    if h.speed() != 0 {
        m.default_speed = h.speed() as u32;
    }
    if h.tempo() != 0 {
        m.default_tempo = Tempo::new(h.tempo() as u32, 0).clamp(Tempo::new(32, 0), Tempo::new(1000, 0));
    }
    m.song_flags = 0;
    if h.flags() & 0x01 != 0 {
        m.song_flags |= SONG_LINEARSLIDES;
    }
    if h.flags() & 0x1000 != 0 {
        m.song_flags |= SONG_EXFILTERRANGE;
    }
    if m.song_flag(SONG_EXFILTERRANGE) && made_with & VER_NEW_MODPLUG != 0 {
        made_with = VER_FT2_CLONE | VER_NEW_MODPLUG | VER_CONFIRMED | VER_EMPTY_ORDERS;
    }
    let orders = h.orders() as usize;
    if file.can_read(orders) {
        m.order = file.read_slice(orders)?.iter().map(|&p| p as PatternIndex).collect();
    }
    if orders == 0 && made_with & VER_EMPTY_ORDERS == 0 {
        m.order = vec![0];
    }
    file.seek((h.size() as usize).saturating_add(60));
    if h.version() >= 0x0104 {
        read_xm_patterns(&mut file, h.version(), h.patterns() as usize, tracker, &mut m);
    }

    let mut sample_flags: Vec<SampleIo> = Vec::new();
    let mut sample_reserved: u8 = 0;
    let mut last_instr_type: i16 = -1;
    let mut last_sample_reserved: i16 = -1;
    let mut last_sample_header_size: i64 = -1;
    let mut instrument_with_samples = false;
    m.instruments = vec![None; m.num_instruments as usize + 1];
    for instr in 1..=m.num_instruments as usize {
        m.instruments[instr] = Some(Box::new(ModInstrument::new(0)));
        if !file.can_read(4) {
            continue;
        }
        let mut header_size = file.u32le() as usize;
        if header_size == 0 {
            header_size = 263;
        }
        file.pos -= 4;
        // ReadStructPartial: missing bytes are zero.
        let mut ih = [0u8; 263];
        let avail = header_size.min(263).min(file.bytes_left());
        ih[..avail].copy_from_slice(&file.data[file.pos..file.pos + avail]);
        file.skip(header_size.min(file.bytes_left()));
        let size = le32(&ih, 0);
        let itype = ih[26];
        let num_samples = le16(&ih, 27) as usize;
        let sample_header_size = le32(&ih, 29);
        let xi = XmInstrument(&ih[33..263]);

        if made_with == VER_OLD_MODPLUG {
            made_with |= VER_CONFIRMED;
            if size == 245 {
                m.last_saved_with_version = mpt_v("1.00.00.A5");
            } else if size == 263 {
                m.last_saved_with_version = mpt_v("1.00.00.B3");
            } else {
                made_with = VER_UNKNOWN | VER_CONFIRMED;
            }
        } else if num_samples == 0 {
            if size == 263 && sample_header_size == 0 && made_with & VER_NEW_MODPLUG != 0 {
                made_with |= VER_CONFIRMED;
            } else if size != 29 && made_with & VER_DIGITRAKKER != 0 {
                made_with &= !VER_DIGITRAKKER;
            } else if made_with & (VER_FT2_CLONE | VER_FT2_GENERIC) != 0 && size != 33 {
                made_with = VER_UNKNOWN;
            }
            if size != 33 {
                made_with &= !VER_PLAYERPRO;
            } else if sample_header_size > 40 && made_with & VER_PLAYERPRO != 0 {
                if instrument_with_samples || (last_sample_header_size != -1 && sample_header_size as i64 != last_sample_header_size) {
                    made_with = VER_PLAYERPRO | VER_CONFIRMED;
                }
                last_sample_header_size = sample_header_size as i64;
            }
        }
        {
            let ins = m.instruments[instr].as_mut().unwrap();
            xi.convert_to_mpt(ins);
            for i in 0..96 {
                ins.keyboard[i + 12] = if (xi.sample_map(i) as usize) < num_samples { xi.sample_map(i) as SampleIndex } else { 0 };
            }
            ins.name = read_name(&ih[4..26], true);
        }
        if last_instr_type == -1 {
            last_instr_type = itype as i16;
        } else if last_instr_type != itype as i16 && made_with & VER_FT2_GENERIC != 0 {
            made_with &= !VER_FT2_GENERIC;
            made_with |= VER_FT2_CLONE;
        }
        if num_samples > 0 {
            instrument_with_samples = true;
            if (xi.midi_enabled() as u16 | xi.midi_channel() as u16 | xi.midi_program() | xi.mute_computer() as u16) != 0 {
                made_with &= !(VER_OLD_MODPLUG | VER_NEW_MODPLUG | VER_PLAYERPRO);
            }
            if size != 263 || itype != 0 {
                made_with &= !VER_PLAYERPRO;
            }
            if made_with & VER_CONFIRMED == 0
                && made_with & VER_PLAYERPRO != 0
                && ((xi.b(200) & 0x04 == 0 && xi.b(195) == 0xFF && xi.b(196) == 0xFF) || (xi.b(201) & 0x04 == 0 && xi.b(198) == 0xFF && xi.b(199) == 0xFF))
            {
                made_with |= VER_CONFIRMED;
                made_with &= !VER_NEW_MODPLUG;
            }
            let slots = allocate_xm_samples(&mut m, num_samples);
            {
                let ins = m.instruments[instr].as_mut().unwrap();
                for k in 12..108 {
                    if (ins.keyboard[k] as usize) < slots.len() {
                        ins.keyboard[k] = slots[ins.keyboard[k] as usize];
                    }
                }
            }
            if h.version() >= 0x0104 {
                sample_flags.clear();
            }
            let mut sample_size = vec![0u32; num_samples];
            for (sample, ss) in sample_size.iter_mut().enumerate() {
                let sb = file.read_array::<40>();
                let sh = XmSample(&sb);
                sample_flags.push(sh.sample_format());
                *ss = sh.length();
                sample_reserved |= sh.reserved();
                if sh.reserved() != 0 && sh.reserved() != 0xAD {
                    made_with &= !(VER_OLD_MODPLUG | VER_NEW_MODPLUG | VER_OPENMPT);
                }
                if last_sample_reserved == -1 {
                    last_sample_reserved = sh.reserved() as i16;
                } else if last_sample_reserved != sh.reserved() as i16 {
                    made_with &= !VER_PLAYERPRO;
                }
                if sh.pan() != 128 {
                    made_with &= !VER_PLAYERPRO;
                }
                if (sh.finetune() as u8 & 0x0F) != 0 && sh.finetune() != 127 {
                    made_with &= !VER_PLAYERPRO;
                }
                if sample < slots.len() {
                    let si = slots[sample] as usize;
                    sh.convert_to_mpt(&mut m.samples[si]);
                    // ApplyAutoVibratoToMPT
                    let s = &mut m.samples[si];
                    s.n_vib_type = xi.b(202);
                    s.n_vib_sweep = xi.b(203);
                    s.n_vib_depth = xi.b(204);
                    s.n_vib_rate = xi.b(205);
                    s.name = read_name(sh.name(), true);
                    if made_with & (VER_FT2_GENERIC | VER_FT2_CLONE) != 0
                        && made_with & (VER_NEW_MODPLUG | VER_PLAYERPRO) != 0
                        && made_with & VER_CONFIRMED == 0
                        && (sh.reserved() > 22 || sh.name()[(sh.reserved() as usize).min(22)..].iter().any(|&c| c != b' '))
                    {
                        made_with &= !VER_FT2_GENERIC;
                        made_with |= VER_FT2_CLONE | VER_CONFIRMED;
                    }
                    if (sh.flags() & 3) == 3 && made_with & VER_NEW_MODPLUG != 0 {
                        made_with |= VER_MODPLUG_BIDI;
                    }
                }
            }
            if h.version() >= 0x0104 {
                for sample in 0..num_samples {
                    let len = if sample_flags[sample].encoding != Encoding::Adpcm {
                        sample_size[sample] as usize
                    } else {
                        16 + (sample_size[sample] as usize).div_ceil(2)
                    };
                    let mut chunk = file.read_chunk(len);
                    if sample < slots.len() {
                        sample_flags[sample].read_sample(&mut m.samples[slots[sample] as usize], &mut chunk);
                    }
                }
            }
        }
    }
    if sample_reserved == 0 && made_with & VER_NEW_MODPLUG != 0 && h.song_name().contains(&0) {
        made_with |= VER_CONFIRMED;
    }
    if h.version() < 0x0104 {
        read_xm_patterns(&mut file, h.version(), h.patterns() as usize, tracker, &mut m);
        for sample in 1..=m.num_samples as usize {
            if let Some(io) = sample_flags.get(sample - 1).copied() {
                io.read_sample(&mut m.samples[sample], &mut file);
            }
        }
    }
    let mut has_midi_config = false;
    if file.read_magic(b"text") {
        let len = file.u32le() as usize;
        file.skip(len);
        made_with |= VER_CONFIRMED;
        made_with &= !VER_PLAYERPRO;
    }
    if file.read_magic(b"MIDI") {
        let len = file.u32le() as usize;
        let mut chunk = file.read_chunk(len);
        read_midi_config(&mut chunk, &mut m.midi_cfg);
        m.midi_cfg.sanitize();
        has_midi_config = true;
        made_with |= VER_CONFIRMED;
        made_with &= !VER_PLAYERPRO;
    }
    if file.read_magic(b"PNAM") {
        let len = file.u32le() as usize;
        file.skip(len.min(m.patterns.len() * 32) / 32 * 32);
        made_with |= VER_CONFIRMED;
        made_with &= !VER_PLAYERPRO;
    }
    if file.read_magic(b"CNAM") {
        let len = file.u32le() as usize;
        file.skip((len / 20).min(m.num_channels()) * 20);
        made_with |= VER_CONFIRMED;
        made_with &= !VER_PLAYERPRO;
    }
    if file.can_read(8) {
        let old = file.pos;
        crate::ext::load_mix_plugins(&mut file, &mut m, true);
        if file.pos != old {
            made_with |= VER_CONFIRMED;
            made_with &= !VER_PLAYERPRO;
        }
    }
    if made_with & VER_CONFIRMED != 0 {
        if made_with & VER_MODPLUG_BIDI != 0 {
            m.last_saved_with_version = mpt_v("1.11");
        } else if made_with & VER_NEW_MODPLUG != 0 {
            m.last_saved_with_version = mpt_v("1.16");
        }
    }
    if &tracker[..8] == b"OpenMPT " {
        m.last_saved_with_version = parse_version_string(&tracker[8..20]);
        made_with = VER_OPENMPT | VER_CONFIRMED;
        mix_levels = if m.last_saved_with_version < mpt_v("1.22.07.19") { MixLevels::Compatible } else { MixLevels::CompatibleFT2 };
    }
    if m.last_saved_with_version != 0 && made_with & VER_OPENMPT == 0 {
        mix_levels = MixLevels::Original;
        m.play_behaviour = [false; kMaxPlayBehaviours];
    }
    if made_with & VER_FT2_GENERIC != 0 {
        mix_levels = MixLevels::CompatibleFT2;
        if !has_midi_config {
            m.midi_cfg.clear_zxx();
        }
        if h.version() >= 0x0104 {
            m.play_behaviour[kFT2VolumeRamping] = true;
        }
    }
    m.set_mix_levels(mix_levels);
    let mut is_openmpt_made = false;
    if m.num_instruments > 0 {
        is_openmpt_made = crate::ext::load_extended_instrument_properties(&mut file, &mut m);
    }
    if crate::ext::load_extended_song_properties(&mut file, &mut m, true) {
        is_openmpt_made = true;
    }
    if is_openmpt_made && m.last_saved_with_version < mpt_v("1.17") {
        m.last_saved_with_version = mpt_v("1.17");
    }
    if m.last_saved_with_version != 0 && m.last_saved_with_version < mpt_v("1.22.02.02") {
        if !m.is_valid_pat(0xFE) {
            m.order.retain(|&p| p != 0xFE);
        }
        if !m.is_valid_pat(0xFF) {
            for p in m.order.iter_mut() {
                if *p == 0xFF {
                    *p = PATTERNINDEX_INVALID;
                }
            }
        }
    }
    m.format_name = format!("FastTracker 2 v{}.{:02X}", h.version() >> 8, h.version() & 0xFF);
    Some(m)
}

/// `ReadStructPartial<MIDIMacroConfigData>`: 9 global, 16 SFx and 128 Zxx
/// macros of 32 bytes each.
pub fn read_midi_config(chunk: &mut Reader, cfg: &mut crate::midimacro::MidiMacroConfig) {
    let mut all = vec![0u8; (9 + 16 + 128) * 32];
    let n = all.len().min(chunk.bytes_left());
    all[..n].copy_from_slice(&chunk.data[chunk.pos..chunk.pos + n]);
    chunk.skip(n);
    for (i, mac) in all.chunks_exact(32).enumerate() {
        let target = if i < 9 {
            &mut cfg.global[i]
        } else if i < 25 {
            &mut cfg.sfx[i - 9]
        } else {
            &mut cfg.zxx[i - 25]
        };
        target.copy_from_slice(mac);
    }
}

/// `Version::Parse` of "a.bb.cc.dd" text (hex components).
pub fn parse_version_string(b: &[u8]) -> u32 {
    let s: String = b.iter().take_while(|&&c| c != 0).map(|&c| c as char).collect();
    let s = s.trim();
    let mut parts = [0u32; 4];
    for (i, p) in s.split('.').take(4).enumerate() {
        parts[i] = u32::from_str_radix(p.trim(), 16).unwrap_or(0) & 0xFF;
    }
    (parts[0] << 24) | (parts[1] << 16) | (parts[2] << 8) | parts[3]
}
