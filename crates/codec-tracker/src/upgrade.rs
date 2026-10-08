//! PCM playback migrations from libopenmpt 0.8.9 `UpgradeModule.cpp`.
//! Plugin-only migrations do not apply: this player has no plugin host.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

use crate::command::*;
use crate::defs::{pb::*, *};
use crate::instrument::EnvelopeNode;
use crate::sndfile::{MixLevels, Module};

pub fn upgrade(m: &mut Module) {
    let v = m.last_saved_with_version;
    if v == 0 { return; }
    if v < 0x01170246 && v != 0x01170000 { m.play_behaviour[MSF_COMPATIBLE_PLAY] = false; }
    let compat = m.behaviour(MSF_COMPATIBLE_PLAY);
    let it = m.mod_type & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0;
    let xm = m.mod_type == MOD_TYPE_XM;
    if v < 0x01200000 {
        for ins in m.instruments.iter_mut().flatten() {
            ins.n_vol_swing = (ins.n_vol_swing as u32 * 100 / 64).min(100) as u8;
            if !(compat && it) || v < 0x01180000 {
                ins.n_pps = ((ins.n_pps as i16 + if ins.n_pps >= 0 { 1 } else { -1 }) / 2) as i8;
            }
            if !(compat && it) || v < 0x01170302 {
                let env = &mut ins.pitch_env;
                if env.sustain_start > env.loop_end && env.has(ENV_LOOP) { env.flags &= !ENV_SUSTAIN; }
                if !env.has(ENV_LOOP | ENV_SUSTAIN) {
                    env.flags |= ENV_SUSTAIN;
                    env.sustain_start = env.last_point();
                    env.sustain_end = env.last_point();
                }
                if env.loop_end > env.loop_start && env.has(ENV_LOOP) && (env.loop_end as usize) < env.nodes.len() {
                    let end = env.loop_end as usize;
                    let tick = env.nodes[end].tick.saturating_sub(1);
                    if tick > env.nodes[end - 1].tick {
                        let value = env.value_from_position(tick as i32, 64, 64) as u8;
                        env.nodes.insert(end, EnvelopeNode { tick, value });
                    } else { env.loop_end -= 1; }
                }
                if m.mod_type != MOD_TYPE_MPT { env.release_node = ENV_RELEASE_NODE_UNSET; }
            }
            if v < 0x01170250 && ins.n_vol_swing | ins.n_pan_swing | ins.n_cut_swing | ins.n_res_swing != 0 {
                m.play_behaviour[kMPTOldSwingBehaviour] = true;
                break;
            }
        }
        if it && (v < 0x01170302 || !compat) {
            for smp in &mut m.samples {
                if smp.n_vib_sweep == 0 && smp.n_vib_depth | smp.n_vib_rate != 0 { smp.n_vib_sweep = 255; }
            }
        }
        m.midi_cfg.upgrade_macros();
    }
    if v < 0x01220312 && v != 0x01220000 && it && (compat || m.behaviour(kMPTOldSwingBehaviour)) {
        for ins in m.instruments.iter_mut().flatten() {
            if ins.pan_env.has(ENV_ENABLED) { ins.n_pan_swing = 0; }
        }
    }
    if xm && (0x01220719..0x01230104).contains(&v) && m.mix_levels == MixLevels::Compatible {
        m.set_mix_levels(MixLevels::CompatibleFT2);
    }
    if v < 0x01260000 {
        for ins in m.instruments.iter_mut().flatten() {
            ins.n_pps = ((ins.n_pps as i16 + if ins.n_pps >= 0 { 1 } else { -1 }) / 2) as i8;
            if !(compat && it) || v < 0x01180000 { ins.n_pan_swing = ((ins.n_pan_swing as u16 + 3) / 4) as u8; }
        }
    }
    if v < 0x01280012 && m.instruments.iter().flatten().any(|i| i.vol_env.release_node != ENV_RELEASE_NODE_UNSET) {
        m.play_behaviour[kLegacyReleaseNode] = true;
    }
    if v < 0x01300054 && m.samples.iter().any(|s| s.u_flags & (CHN_PINGPONGLOOP | CHN_PINGPONGSUSTAIN) != 0 && s.has_sample_data()) {
        m.play_behaviour[kImprecisePingPongLoops] = true;
    }
    upgrade_patterns(m, compat);
    if compat && v < 0x01260000 && (it || xm) {
        let table = if it { HISTORIC_IT } else { HISTORIC_XM };
        for &(bit, since) in table {
            m.play_behaviour[bit] = v >= since || (it && v == since & 0xffff0000);
        }
    }
    let table = if it { MODERN_IT } else if xm { MODERN_XM } else if m.mod_type == MOD_TYPE_S3M { MODERN_S3M } else { &[] };
    for &(bit, since) in table {
        if v < since && (!it || v != since & 0xffff0000) { m.play_behaviour[bit] = false; }
    }
    if xm && v < 0x01190000 { m.play_behaviour[kFT2NoteDelayWithoutInstr] = true; }
    if (0x01270027..0x01270049).contains(&v) {
        for i in 0..5 {
            m.play_behaviour[kFT2NoteOffFlags + i] = m.play_behaviour[kST3NoMutedChannels + i];
            m.play_behaviour[kST3NoMutedChannels + i] = false;
        }
    }
    if v < 0x01170000 { m.play_behaviour[kTempoClamp] = true; }
    else if v <= 0x01200103 && v != 0x01200000 { m.play_behaviour[kSlidesAtSpeed1] = true; }
    if m.song_flag(SONG_LINEARSLIDES) {
        if v < 0x01240000 { m.play_behaviour[kPeriodsAreHertz] = false; }
        else if v < 0x01260000 && it { m.play_behaviour[kPeriodsAreHertz] = true; }
    } else if v < 0x01300036 && v != 0x01300000 { m.play_behaviour[kPeriodsAreHertz] = false; }
    if m.behaviour(kITEnvelopePositionHandling) && (0x01230102..0x01280043).contains(&v)
        && m.instruments.iter().flatten().any(|i| i.vol_env.release_node != ENV_RELEASE_NODE_UNSET
            && i.vol_env.has(ENV_SUSTAIN) && i.vol_env.release_node > i.vol_env.sustain_end) {
        m.play_behaviour[kReleaseNodePastSustainBug] = true;
    }
}

fn upgrade_patterns(module: &mut Module, compat: bool) {
    let v = module.last_saved_with_version;
    let t = module.mod_type;
    let it_tremor = module.behaviour(kITTremor) && !module.song_flag(SONG_ITOLDEFFECTS);
    let nc = module.num_channels();
    let instruments = module.num_instruments;
    for pattern in &mut module.patterns {
        for row in pattern.data.chunks_mut(nc) {
            for ci in 0..row.len() {
                let mut m = row[ci];
                if t == MOD_TYPE_S3M {
                    if v < 0x01190000 && m.command == CMD_GLOBALVOLUME { m.param = m.param.min(64); }
                } else if t & (MOD_TYPE_IT | MOD_TYPE_MPT) != 0 {
                    if v < 0x01170302 || (!compat && v < 0x01200000) {
                        if m.command == CMD_GLOBALVOLUME { m.param = m.param.min(128); }
                        else if m.command == CMD_S3MCMDEX {
                            if m.param == 0xc0 { m.command = CMD_NONE; m.note = NOTE_NOTECUT; }
                            else if m.param == 0xd0 { m.command = CMD_NONE; }
                        }
                    }
                    let note_slide = (v < 0x01180000 || (!compat && v < 0x01200000))
                        && matches!(m.command, CMD_VOLUMESLIDE | CMD_VIBRATOVOL | CMD_TONEPORTAVOL | CMD_PANNINGSLIDE);
                    let chan_slide = v < 0x01200000 && matches!(m.command, CMD_GLOBALVOLSLIDE | CMD_CHANNELVOLSLIDE);
                    if (note_slide || chan_slide) && !matches!(m.param & 15, 0 | 15) && !matches!(m.param & 0xf0, 0 | 0xf0) {
                        m.param &= if m.command == CMD_GLOBALVOLSLIDE { 0xf0 } else { 15 };
                    }
                    if v < 0x01220104 && v != 0x01220000 && instruments != 0 && m.instr as u16 > instruments && !compat {
                        m.volcmd = VOLCMD_VOLUME; m.vol = 0;
                    }
                    if m.command == CMD_TREMOR && m.param == 0x11 && v < 0x01291202 && it_tremor { m.param = 0; }
                } else if t == MOD_TYPE_XM {
                    if ((v >= 0x01170302 && compat) || v >= 0x01200000) && v < 0x01240202 && m.command == CMD_GLOBALVOLUME && m.param > 64 {
                        m.command = CMD_NONE;
                    }
                    if (v < 0x01190000 || (!compat && v < 0x01200000)) && m.command == CMD_OFFSET && m.volcmd == VOLCMD_TONEPORTAMENTO { m.command = CMD_NONE; }
                    if v < 0x01200110 && m.volcmd == VOLCMD_TONEPORTAMENTO && m.command == CMD_TONEPORTAMENTO && (m.vol != 0 || compat) && m.param != 0 {
                        m.volcmd = VOLCMD_NONE; m.param = (m.param as u16 + ((m.vol as u16) << 4)).min(255) as u8;
                    }
                    if v < 0x01220709 && m.command == CMD_SPEED && m.param == 0 { m.command = CMD_NONE; }
                }
                if v < 0x01200000 {
                    let fine_delay = (m.command == CMD_S3MCMDEX && m.param & 0xf0 == 0x60)
                        || (m.command == CMD_XFINEPORTAUPDOWN && m.param & 0xf0 == 0x60 && (!(compat && t == MOD_TYPE_XM) || v < 0x01180000));
                    let row_delay = m.command == CMD_S3MCMDEX && m.param & 0xf0 == 0xe0;
                    for earlier in &mut row[..ci] {
                        if (fine_delay && matches!(earlier.command, CMD_S3MCMDEX | CMD_XFINEPORTAUPDOWN) && earlier.param & 0xf0 == 0x60)
                            || (row_delay && earlier.command == CMD_S3MCMDEX && earlier.param & 0xf0 == 0xe0) { earlier.command = CMD_NONE; }
                    }
                }
                if m.volcmd == VOLCMD_VIBRATODEPTH && v < 0x01270037 && v != 0x01270000 {
                    if m.command == CMD_VIBRATOVOL && m.vol > 0 { m.command = CMD_VOLUMESLIDE; }
                    else if matches!(m.command, CMD_VIBRATO | CMD_FINEVIBRATO) && m.param & 15 == 0 { m.command = CMD_VIBRATO; m.param |= m.vol & 15; m.volcmd = VOLCMD_NONE; }
                    else if matches!(m.command, CMD_VIBRATO | CMD_VIBRATOVOL | CMD_FINEVIBRATO) { m.volcmd = VOLCMD_NONE; }
                }
                if t != MOD_TYPE_MPT && m.volcmd == VOLCMD_OFFSET && m.command == CMD_NONE { m.command = CMD_OFFSET; m.param = m.vol.wrapping_shl(3); m.volcmd = VOLCMD_NONE; }
                if m.volcmd == VOLCMD_OFFSET && m.command == CMD_OFFSET && v < 0x01300014 {
                    if m.param != 0 || m.vol == 0 { m.volcmd = VOLCMD_NONE; } else { m.command = CMD_NONE; }
                }
                row[ci] = m;
            }
        }
    }
}

const HISTORIC_IT: &[(usize, u32)] = &[
    (kTempoClamp, 0x01170302),
    (kPerChannelGlobalVolSlide, 0x01170302),
    (kPanOverride, 0x01170302),
    (kITInstrWithoutNote, 0x01170246),
    (kITVolColFinePortamento, 0x01170249),
    (kITArpeggio, 0x01170249),
    (kITOutOfRangeDelay, 0x01170249),
    (kITPortaMemoryShare, 0x01170249),
    (kITPatternLoopTargetReset, 0x01170249),
    (kITFT2PatternLoop, 0x01170249),
    (kITPingPongNoReset, 0x01170251),
    (kITEnvelopeReset, 0x01170251),
    (kITClearOldNoteAfterCut, 0x01170252),
    (kITVibratoTremoloPanbrello, 0x01170302),
    (kITTremor, 0x01170302),
    (kITRetrigger, 0x01170302),
    (kITMultiSampleBehaviour, 0x01170302),
    (kITPortaTargetReached, 0x01170302),
    (kITPatternLoopBreak, 0x01170302),
    (kITOffset, 0x01170302),
    (kITSwingBehaviour, 0x01180000),
    (kITNNAReset, 0x01180000),
    (kITSCxStopsSample, 0x01180001),
    (kITEnvelopePositionHandling, 0x01180100),
    (kITPortamentoInstrument, 0x01190001),
    (kITPingPongMode, 0x01190021),
    (kITRealNoteMapping, 0x01190030),
    (kITHighOffsetNoRetrig, 0x01200014),
    (kITFilterBehaviour, 0x01200035),
    (kITNoSurroundPan, 0x01200053),
    (kITShortSampleRetrig, 0x01200054),
    (kITPortaNoNote, 0x01200056),
    (kRowDelayWithNoteDelay, 0x01200076),
    (kITFT2DontResetNoteOffOnPorta, 0x01200206),
    (kITVolColMemory, 0x01210116),
    (kITPortamentoSwapResetsPos, 0x01210125),
    (kITEmptyNoteMapSlot, 0x01210125),
    (kITFirstTickHandling, 0x01220709),
    (kITSampleAndHoldPanbrello, 0x01220719),
    (kITClearPortaTarget, 0x01230403),
    (kITPanbrelloHold, 0x01240106),
    (kITPanningReset, 0x01240106),
    (kITPatternLoopWithJumpsOld, 0x01250019),
];

const HISTORIC_XM: &[(usize, u32)] = &[
    (kTempoClamp, 0x01170302),
    (kPerChannelGlobalVolSlide, 0x01170302),
    (kPanOverride, 0x01170302),
    (kITFT2PatternLoop, 0x01170302),
    (kFT2Arpeggio, 0x01170302),
    (kFT2Retrigger, 0x01170302),
    (kFT2VolColVibrato, 0x01170302),
    (kFT2PortaNoNote, 0x01170302),
    (kFT2KeyOff, 0x01170302),
    (kFT2PanSlide, 0x01170302),
    (kFT2ST3OffsetOutOfRange, 0x01170302),
    (kFT2RestrictXCommand, 0x01180000),
    (kFT2RetrigWithNoteDelay, 0x01180000),
    (kFT2SetPanEnvPos, 0x01180000),
    (kFT2PortaIgnoreInstr, 0x01180001),
    (kFT2VolColMemory, 0x01180100),
    (kFT2LoopE60Restart, 0x01180201),
    (kFT2ProcessSilentChannels, 0x01180201),
    (kFT2ReloadSampleSettings, 0x01200036),
    (kFT2PortaDelay, 0x01200040),
    (kFT2Transpose, 0x01200062),
    (kFT2PatternLoopWithJumps, 0x01200069),
    (kFT2PortaTargetNoReset, 0x01200069),
    (kFT2EnvelopeEscape, 0x01200077),
    (kFT2Tremor, 0x01200111),
    (kFT2OutOfRangeDelay, 0x01200202),
    (kFT2Periods, 0x01220301),
    (kFT2PanWithDelayedNoteOff, 0x01220302),
    (kFT2VolColDelay, 0x01220719),
    (kFT2FinetunePrecision, 0x01220719),
];

const MODERN_IT: &[(usize, u32)] = &[
    (kITInstrWithNoteOff, 0x01260001),
    (kITMultiSampleInstrumentNumber, 0x01270027),
    (kITInstrWithNoteOffOldEffects, 0x01280206),
    (kITDoNotOverrideChannelPan, 0x01290022),
    (kITPatternLoopWithJumps, 0x01290032),
    (kITDCTBehaviour, 0x01290057),
    (kITPitchPanSeparation, 0x01300053),
    (kITResetFilterOnPortaSmpChange, 0x01300802),
    (kITInitialNoteMemory, 0x01310025),
    (kITNoSustainOnPortamento, 0x01320013),
    (kITEmptyNoteMapSlotIgnoreCell, 0x01320013),
    (kITOffsetWithInstrNumber, 0x01320015),
    (kITDoublePortamentoSlides, 0x01320027),
    (kITCarryAfterNoteOff, 0x01320040),
    (kITNoteCutWithPorta, 0x01320102),
    (kITVolColNoSlidePropagation, 0x01320203),
    (kITStoppedFilterEnvAtStart, 0x01320304),
    (kITCompatGxxCarryPortaWithIns, 0x01321002),
];

const MODERN_XM: &[(usize, u32)] = &[
    (kFT2NoteOffFlags, 0x01270027),
    (kRowDelayWithNoteDelay, 0x01270037),
    (kFT2MODTremoloRampWaveform, 0x01270037),
    (kFT2PortaUpDownMemory, 0x01270037),
    (kFT2PanSustainRelease, 0x01280009),
    (kFT2NoteDelayWithoutInstr, 0x01280044),
    (kITFT2DontResetNoteOffOnPorta, 0x01290034),
    (kFT2PortaResetDirection, 0x01300040),
    (kFT2AutoVibratoAbortSweep, 0x01320029),
    (kFT2OffsetMemoryRequiresNote, 0x01320043),
];

const MODERN_S3M: &[(usize, u32)] = &[
    (kST3NoMutedChannels, 0x01180000),
    (kST3EffectMemory, 0x01200000),
    (kRowDelayWithNoteDelay, 0x01200000),
    (kST3PortaSampleChange, 0x01220000),
    (kST3VibratoMemory, 0x01260000),
    (kITPanbrelloHold, 0x01260000),
    (KST3PortaAfterArpeggio, 0x01270000),
    (kST3OffsetWithoutInstrument, 0x01280000),
    (kST3RetrigAfterNoteCut, 0x01290000),
    (kFT2ST3OffsetOutOfRange, 0x01290000),
    (kApplyUpperPeriodLimit, 0x01300045),
    (kST3TonePortaWithAdlibNote, 0x01310013),
];
