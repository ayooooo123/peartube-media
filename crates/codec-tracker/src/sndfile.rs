//! The song model and play state, ported from libopenmpt 0.8.9
//! `soundlib/Sndfile.h/.cpp`, `PlayState.h/.cpp`, `ModSequence.h/.cpp`,
//! `pattern.h` and `SoundFilePlayConfig.cpp`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

#![allow(dead_code)]

use crate::channel::{ModChannel, ModChannelSettings, RESET_TOTAL};
use crate::command::ModCommand;
use crate::defs::pb::*;
use crate::defs::*;
use crate::instrument::ModInstrument;
use crate::midimacro::MidiMacroConfig;
use crate::sample::ModSample;

pub const TICKS_ROW_FINISHED: u32 = u32::MAX - 1;

/// One pattern: `rows` rows of `num_channels` cells. Empty data marks a
/// pattern that does not exist (`CPattern::IsValid`).
#[derive(Clone, Debug, Default)]
pub struct Pattern {
    pub rows: RowIndex,
    pub data: Vec<ModCommand>,
    pub rows_per_beat: RowIndex,
    pub rows_per_measure: RowIndex,
}

impl Pattern {
    pub fn is_valid(&self) -> bool {
        !self.data.is_empty()
    }
    pub fn cell(&self, row: RowIndex, chn: usize, nchn: usize) -> &ModCommand {
        &self.data[row as usize * nchn + chn]
    }
    pub fn override_signature(&self) -> bool {
        self.rows_per_beat + self.rows_per_measure > 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TempoMode {
    Classic,
    Alternative,
    Modern,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixLevels {
    Original,
    V117RC1,
    V117RC2,
    V117RC3,
    Compatible,
    CompatibleFT2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanningMode {
    Undetermined,
    SoftPanning,
    NoSoftPanning,
    FT2Panning,
}

/// `CSoundFilePlayConfig`: the parts the integer mixer reads.
#[derive(Clone, Copy, Debug)]
pub struct PlayConfig {
    pub global_volume_applies_to_master: bool,
    pub use_global_pre_amp: bool,
    pub panning_mode: PanningMode,
    pub extra_sample_attenuation: i32,
}

impl PlayConfig {
    pub fn new(levels: MixLevels) -> Self {
        match levels {
            MixLevels::Original => PlayConfig {
                global_volume_applies_to_master: false,
                use_global_pre_amp: true,
                panning_mode: PanningMode::Undetermined,
                extra_sample_attenuation: MIXING_ATTENUATION as i32,
            },
            MixLevels::V117RC1 => PlayConfig {
                global_volume_applies_to_master: false,
                use_global_pre_amp: true,
                panning_mode: PanningMode::Undetermined,
                extra_sample_attenuation: MIXING_ATTENUATION as i32,
            },
            MixLevels::V117RC2 => PlayConfig {
                global_volume_applies_to_master: true,
                use_global_pre_amp: true,
                panning_mode: PanningMode::Undetermined,
                extra_sample_attenuation: MIXING_ATTENUATION as i32,
            },
            MixLevels::V117RC3 => PlayConfig {
                global_volume_applies_to_master: true,
                use_global_pre_amp: false,
                panning_mode: PanningMode::SoftPanning,
                extra_sample_attenuation: 0,
            },
            MixLevels::Compatible | MixLevels::CompatibleFT2 => PlayConfig {
                global_volume_applies_to_master: true,
                use_global_pre_amp: false,
                panning_mode: if levels == MixLevels::Compatible {
                    PanningMode::NoSoftPanning
                } else {
                    PanningMode::FT2Panning
                },
                extra_sample_attenuation: 1,
            },
        }
    }
}

pub type Behaviours = [bool; kMaxPlayBehaviours];

/// `CSoundFile::UseFinetuneAndTranspose(type)`.
pub fn use_finetune_and_transpose(t: u32) -> bool {
    t & (MOD_TYPE_AMF0
        | MOD_TYPE_DIGI
        | MOD_TYPE_MED
        | MOD_TYPE_MOD
        | MOD_TYPE_MTM
        | MOD_TYPE_OKT
        | MOD_TYPE_SFX
        | MOD_TYPE_STP
        | MOD_TYPE_XM)
        != 0
}

/// `CSoundFile::UseCombinedPortamentoCommands(type)`.
pub fn use_combined_portamento_commands(t: u32) -> bool {
    t & (MOD_TYPE_MOD
        | MOD_TYPE_XM
        | MOD_TYPE_MT2
        | MOD_TYPE_MED
        | MOD_TYPE_AMF0
        | MOD_TYPE_DIGI
        | MOD_TYPE_STP
        | MOD_TYPE_DTM)
        == 0
}

fn set(b: &mut Behaviours, list: &[usize]) {
    for &i in list {
        b[i] = true;
    }
}

/// `GetSupportedPlaybackBehaviour`.
pub fn supported_playback_behaviour(t: u32) -> Behaviours {
    let mut b = [false; kMaxPlayBehaviours];
    match t {
        MOD_TYPE_MPT | MOD_TYPE_IT => {
            if t == MOD_TYPE_MPT {
                set(&mut b, &[kOPLFlexibleNoteOff, kOPLwithNNA, kOPLNoteOffOnNoteChange]);
            }
            set(
                &mut b,
                &[
                    MSF_COMPATIBLE_PLAY,
                    kPeriodsAreHertz,
                    kTempoClamp,
                    kPerChannelGlobalVolSlide,
                    kPanOverride,
                    kITInstrWithoutNote,
                    kITVolColFinePortamento,
                    kITArpeggio,
                    kITOutOfRangeDelay,
                    kITPortaMemoryShare,
                    kITPatternLoopTargetReset,
                    kITFT2PatternLoop,
                    kITPingPongNoReset,
                    kITEnvelopeReset,
                    kITClearOldNoteAfterCut,
                    kITVibratoTremoloPanbrello,
                    kITTremor,
                    kITRetrigger,
                    kITMultiSampleBehaviour,
                    kITPortaTargetReached,
                    kITPatternLoopBreak,
                    kITOffset,
                    kITSwingBehaviour,
                    kITNNAReset,
                    kITSCxStopsSample,
                    kITEnvelopePositionHandling,
                    kITPortamentoInstrument,
                    kITPingPongMode,
                    kITRealNoteMapping,
                    kITHighOffsetNoRetrig,
                    kITFilterBehaviour,
                    kITNoSurroundPan,
                    kITShortSampleRetrig,
                    kITPortaNoNote,
                    kITFT2DontResetNoteOffOnPorta,
                    kITVolColMemory,
                    kITPortamentoSwapResetsPos,
                    kITEmptyNoteMapSlot,
                    kITFirstTickHandling,
                    kITSampleAndHoldPanbrello,
                    kITClearPortaTarget,
                    kITPanbrelloHold,
                    kITPanningReset,
                    kITPatternLoopWithJumps,
                    kITInstrWithNoteOff,
                    kITMultiSampleInstrumentNumber,
                    kRowDelayWithNoteDelay,
                    kITInstrWithNoteOffOldEffects,
                    kITDoNotOverrideChannelPan,
                    kITDCTBehaviour,
                    kITPitchPanSeparation,
                    kITResetFilterOnPortaSmpChange,
                    kITInitialNoteMemory,
                    kITNoSustainOnPortamento,
                    kITEmptyNoteMapSlotIgnoreCell,
                    kITOffsetWithInstrNumber,
                    kITDoublePortamentoSlides,
                    kITCarryAfterNoteOff,
                    kITNoteCutWithPorta,
                    kITVolColNoSlidePropagation,
                    kITStoppedFilterEnvAtStart,
                    kITCompatGxxCarryPortaWithIns,
                ],
            );
        }
        MOD_TYPE_XM => set(
            &mut b,
            &[
                MSF_COMPATIBLE_PLAY,
                kFT2VolumeRamping,
                kTempoClamp,
                kPerChannelGlobalVolSlide,
                kPanOverride,
                kITFT2PatternLoop,
                kITFT2DontResetNoteOffOnPorta,
                kFT2Arpeggio,
                kFT2Retrigger,
                kFT2VolColVibrato,
                kFT2PortaNoNote,
                kFT2KeyOff,
                kFT2PanSlide,
                kFT2ST3OffsetOutOfRange,
                kFT2RestrictXCommand,
                kFT2RetrigWithNoteDelay,
                kFT2SetPanEnvPos,
                kFT2PortaIgnoreInstr,
                kFT2VolColMemory,
                kFT2LoopE60Restart,
                kFT2ProcessSilentChannels,
                kFT2ReloadSampleSettings,
                kFT2PortaDelay,
                kFT2Transpose,
                kFT2PatternLoopWithJumps,
                kFT2PortaTargetNoReset,
                kFT2EnvelopeEscape,
                kFT2Tremor,
                kFT2OutOfRangeDelay,
                kFT2Periods,
                kFT2PanWithDelayedNoteOff,
                kFT2VolColDelay,
                kFT2FinetunePrecision,
                kFT2NoteOffFlags,
                kRowDelayWithNoteDelay,
                kFT2MODTremoloRampWaveform,
                kFT2PortaUpDownMemory,
                kFT2PanSustainRelease,
                kFT2NoteDelayWithoutInstr,
                kFT2PortaResetDirection,
                kFT2AutoVibratoAbortSweep,
                kFT2OffsetMemoryRequiresNote,
            ],
        ),
        MOD_TYPE_S3M => set(
            &mut b,
            &[
                MSF_COMPATIBLE_PLAY,
                kTempoClamp,
                kPanOverride,
                kITPanbrelloHold,
                kFT2ST3OffsetOutOfRange,
                kST3NoMutedChannels,
                kST3PortaSampleChange,
                kST3EffectMemory,
                kST3VibratoMemory,
                KST3PortaAfterArpeggio,
                kRowDelayWithNoteDelay,
                kST3OffsetWithoutInstrument,
                kST3RetrigAfterNoteCut,
                kST3SampleSwap,
                kOPLNoteOffOnNoteChange,
                kApplyUpperPeriodLimit,
                kST3TonePortaWithAdlibNote,
                kS3MIgnoreCombinedFineSlides,
            ],
        ),
        MOD_TYPE_MOD => set(
            &mut b,
            &[
                kMODVBlankTiming,
                kMODOneShotLoops,
                kMODIgnorePanning,
                kMODSampleSwap,
                kMODOutOfRangeNoteDelay,
                kMODTempoOnSecondTick,
                kRowDelayWithNoteDelay,
                kFT2MODTremoloRampWaveform,
            ],
        ),
        _ => set(&mut b, &[MSF_COMPATIBLE_PLAY, kPeriodsAreHertz, kTempoClamp, kPanOverride]),
    }
    b
}

/// `GetDefaultPlaybackBehaviour`.
pub fn default_playback_behaviour(t: u32) -> Behaviours {
    let mut b = [false; kMaxPlayBehaviours];
    match t {
        MOD_TYPE_MPT => set(
            &mut b,
            &[
                kPeriodsAreHertz,
                kPerChannelGlobalVolSlide,
                kPanOverride,
                kITArpeggio,
                kITPortaMemoryShare,
                kITPatternLoopTargetReset,
                kITFT2PatternLoop,
                kITPingPongNoReset,
                kITClearOldNoteAfterCut,
                kITVibratoTremoloPanbrello,
                kITMultiSampleBehaviour,
                kITPortaTargetReached,
                kITPatternLoopBreak,
                kITSwingBehaviour,
                kITSCxStopsSample,
                kITEnvelopePositionHandling,
                kITPingPongMode,
                kITRealNoteMapping,
                kITPortaNoNote,
                kITVolColMemory,
                kITFirstTickHandling,
                kITClearPortaTarget,
                kITSampleAndHoldPanbrello,
                kITPanbrelloHold,
                kITPanningReset,
                kITInstrWithNoteOff,
                kOPLFlexibleNoteOff,
                kITDoNotOverrideChannelPan,
                kITDCTBehaviour,
                kOPLwithNNA,
                kITPitchPanSeparation,
            ],
        ),
        MOD_TYPE_S3M => {
            b = supported_playback_behaviour(t);
            b[kST3SampleSwap] = false;
            b[kS3MIgnoreCombinedFineSlides] = false;
        }
        MOD_TYPE_XM => {
            b = supported_playback_behaviour(t);
            b[kFT2VolumeRamping] = false;
        }
        MOD_TYPE_MOD => b[kRowDelayWithNoteDelay] = true,
        _ => b = supported_playback_behaviour(t),
    }
    b
}

/// The song (`CSoundFile` minus the play state).
#[derive(Clone, Debug)]
pub struct Module {
    pub mod_type: u32,
    /// Index 0 is unused (except as the MOD default sample); 1..=num_samples.
    pub samples: Vec<ModSample>,
    pub num_samples: SampleIndex,
    /// Index 0 is unused; 1..=num_instruments.
    pub instruments: Vec<Option<Box<ModInstrument>>>,
    pub num_instruments: InstrumentIndex,
    pub patterns: Vec<Pattern>,
    pub order: Vec<PatternIndex>,
    pub restart_pos: OrderIndex,
    pub default_speed: u32,
    pub default_tempo: Tempo,
    pub chn_settings: Vec<ModChannelSettings>,
    pub song_flags: u32,
    pub play_behaviour: Behaviours,
    pub mix_levels: MixLevels,
    pub play_config: PlayConfig,
    pub sample_pre_amp: u32,
    pub vsti_volume: u32,
    pub default_global_volume: u32,
    pub min_period: i32,
    pub max_period: i32,
    pub resampling: u8,
    pub tempo_mode: TempoMode,
    pub default_rows_per_beat: RowIndex,
    pub default_rows_per_measure: RowIndex,
    pub midi_cfg: MidiMacroConfig,
    pub title: String,
    pub artist: String,
    pub message: String,
    pub format_name: String,
    pub tracker: String,
    pub last_saved_with_version: u32,
    pub created_with_version: u32,
}

impl Module {
    /// An empty song of `mod_type` with `channels` channels
    /// (`InitializeGlobals`).
    pub fn new(mod_type: u32, channels: usize) -> Self {
        let channels = channels.min(MAX_BASECHANNELS);
        let best = best_save_format(mod_type);
        let mut play_behaviour = default_playback_behaviour(best);
        if best == MOD_TYPE_IT && mod_type != best {
            play_behaviour[kITInitialNoteMemory] = false;
        }
        let mut song_flags = 0;
        if mod_type
            & (MOD_TYPE_DIGI | MOD_TYPE_MED | MOD_TYPE_MOD | MOD_TYPE_OKT | MOD_TYPE_SFX | MOD_TYPE_STP)
            != 0
        {
            song_flags |= SONG_ISAMIGA;
        }
        if mod_type & (MOD_TYPE_AMF0 | MOD_TYPE_DIGI | MOD_TYPE_MTM) != 0 {
            song_flags |= SONG_FORMAT_NO_VOLCOL;
        }
        Module {
            mod_type,
            samples: vec![ModSample::new(mod_type)],
            num_samples: 0,
            instruments: vec![None],
            num_instruments: 0,
            patterns: Vec::new(),
            order: Vec::new(),
            restart_pos: 0,
            default_speed: 6,
            default_tempo: Tempo::new(125, 0),
            chn_settings: vec![ModChannelSettings::default(); channels],
            song_flags,
            play_behaviour,
            mix_levels: MixLevels::Compatible,
            play_config: PlayConfig::new(MixLevels::Compatible),
            sample_pre_amp: 48,
            vsti_volume: 48,
            default_global_volume: MAX_GLOBAL_VOLUME,
            min_period: 16,
            max_period: 32767,
            resampling: SRCMODE_DEFAULT,
            tempo_mode: TempoMode::Classic,
            default_rows_per_beat: 0,
            default_rows_per_measure: 0,
            midi_cfg: MidiMacroConfig::default(),
            title: String::new(),
            artist: String::new(),
            message: String::new(),
            format_name: String::new(),
            tracker: String::new(),
            last_saved_with_version: 0,
            created_with_version: 0,
        }
    }

    pub fn num_channels(&self) -> usize {
        self.chn_settings.len()
    }
    pub fn behaviour(&self, i: usize) -> bool {
        self.play_behaviour[i]
    }
    pub fn song_flag(&self, f: u32) -> bool {
        self.song_flags & f != 0
    }
    pub fn use_finetune_and_transpose(&self) -> bool {
        use_finetune_and_transpose(self.mod_type)
    }
    pub fn periods_are_frequencies(&self) -> bool {
        self.behaviour(kPeriodsAreHertz) && !self.use_finetune_and_transpose()
    }
    pub fn use_combined_portamento_commands(&self) -> bool {
        use_combined_portamento_commands(self.mod_type)
    }
    pub fn set_mix_levels(&mut self, levels: MixLevels) {
        self.mix_levels = levels;
        self.play_config = PlayConfig::new(levels);
    }

    pub fn is_valid_pat(&self, pat: PatternIndex) -> bool {
        (pat as usize) < self.patterns.len() && self.patterns[pat as usize].is_valid()
    }
    pub fn is_valid_order(&self, ord: OrderIndex) -> bool {
        (ord as usize) < self.order.len() && self.is_valid_pat(self.order[ord as usize])
    }
    /// `ModSequence::GetLengthTailTrimmed`.
    pub fn order_length_tail_trimmed(&self) -> OrderIndex {
        let mut n = self.order.len();
        while n > 0 && self.order[n - 1] == PATTERNINDEX_INVALID {
            n -= 1;
        }
        n as OrderIndex
    }

    /// The instrument at `i`, if `i` names an existing one.
    pub fn instrument(&self, i: u32) -> Option<&ModInstrument> {
        if i <= self.num_instruments as u32 {
            self.instruments.get(i as usize).and_then(|x| x.as_deref())
        } else {
            None
        }
    }
    pub fn instrument_index(&self, i: u32) -> Option<InstrumentIndex> {
        self.instrument(i).map(|_| i as InstrumentIndex)
    }

    /// `GetSampleIndex`.
    pub fn sample_index(&self, note: u8, instr: u32) -> SampleIndex {
        let mut smp: u32 = 0;
        if self.num_instruments > 0 {
            if ModCommand::is_note_of(note) {
                if let Some(ins) = self.instrument(instr) {
                    smp = ins.keyboard[(note - 1) as usize] as u32;
                }
            }
        } else {
            smp = instr;
        }
        if smp <= self.num_samples as u32 {
            smp as SampleIndex
        } else {
            0
        }
    }

    /// The finishing part of `CreateInternal`: sanitising and loop
    /// precomputation after a loader filled in the song.
    pub fn finish_load(&mut self) {
        for cs in &mut self.chn_settings {
            cs.n_volume = cs.n_volume.min(64);
            if cs.n_pan > 256 {
                cs.n_pan = 128;
            }
        }
        let it_ping_pong = self.behaviour(kITPingPongMode);
        for s in self.samples.iter_mut().skip(1) {
            s.n_length = s.n_length.min(MAX_SAMPLE_LENGTH);
            s.sanitize_loops();
            if s.has_sample_data() {
                s.precompute_loops(it_ping_pong);
            } else {
                // FM voices have no PCM allocation; a nonzero logical length
                // keeps their note/effect state active in the tick processor.
                s.n_length = u32::from(s.u_flags & CHN_ADLIB != 0);
                s.n_loop_start = 0;
                s.n_loop_end = 0;
                s.n_sustain_start = 0;
                s.n_sustain_end = 0;
                s.u_flags &= !(CHN_LOOP | CHN_PINGPONGLOOP | CHN_SUSTAINLOOP | CHN_PINGPONGSUSTAIN);
            }
            if s.n_global_vol > 64 {
                s.n_global_vol = 64;
            }
        }
        let mut max_instr = 0;
        let t = self.mod_type;
        for (i, ins) in self.instruments.iter_mut().enumerate() {
            if let Some(ins) = ins {
                max_instr = i;
                ins.sanitize(t);
            }
        }
        self.num_instruments = max_instr as InstrumentIndex;
        self.instruments.truncate(max_instr + 1);
        if self.default_rows_per_beat == 0 && self.tempo_mode == TempoMode::Modern {
            self.default_rows_per_beat = 1;
        }
        if self.default_rows_per_measure < self.default_rows_per_beat {
            self.default_rows_per_measure = self.default_rows_per_beat;
        }
        self.default_rows_per_beat = self.default_rows_per_beat.min(MAX_ROWS_PER_BEAT);
        self.default_rows_per_measure = self.default_rows_per_measure.min(MAX_ROWS_PER_BEAT);
        self.default_global_volume = self.default_global_volume.min(MAX_GLOBAL_VOLUME);
        self.sample_pre_amp = self.sample_pre_amp.min(MAX_PREAMP);
        self.vsti_volume = self.vsti_volume.min(MAX_PREAMP);
        if self.use_finetune_and_transpose() {
            self.play_behaviour[kPeriodsAreHertz] = false;
        }
        let trimmed = self.order_length_tail_trimmed() as usize;
        self.order.truncate(trimmed);
        if self.restart_pos as usize >= self.order.len() {
            self.restart_pos = 0;
        }
        let levels = self.mix_levels;
        self.set_mix_levels(levels);
    }
}

pub const MAX_ROWS_PER_BEAT: RowIndex = 65536;

/// `GetBestSaveFormat` for the formats this crate loads.
pub fn best_save_format(t: u32) -> u32 {
    match t {
        MOD_TYPE_MOD | MOD_TYPE_S3M | MOD_TYPE_XM | MOD_TYPE_IT | MOD_TYPE_MPT => t,
        MOD_TYPE_AMF0 | MOD_TYPE_DIGI | MOD_TYPE_SFX | MOD_TYPE_STP => MOD_TYPE_MOD,
        MOD_TYPE_669 | MOD_TYPE_FAR | MOD_TYPE_STM | MOD_TYPE_DSM | MOD_TYPE_AMF | MOD_TYPE_MTM => {
            MOD_TYPE_S3M
        }
        MOD_TYPE_MID => MOD_TYPE_MPT,
        _ => MOD_TYPE_IT,
    }
}

/// `PlayState`.
#[derive(Clone, Debug)]
pub struct PlayState {
    pub total_sample_count: u64,
    pub buffer_count: u32,
    pub buffer_diff: f64,
    pub tick_count: u32,
    pub pattern_delay: u32,
    pub frame_delay: u32,
    pub samples_per_tick: u32,
    pub current_rows_per_beat: RowIndex,
    pub current_rows_per_measure: RowIndex,
    pub music_speed: u32,
    pub music_tempo: Tempo,
    pub row: RowIndex,
    pub next_row: RowIndex,
    pub next_pat_start_row: RowIndex,
    pub break_row: RowIndex,
    pub pat_loop_row: RowIndex,
    pub pos_jump: OrderIndex,
    pub pattern: PatternIndex,
    pub current_order: OrderIndex,
    pub next_order: OrderIndex,
    pub seq_override: OrderIndex,
    pub last_moved_channel: ChannelIndex,
    pub global_volume: i32,
    pub samples_to_global_vol_ramp_dest: i32,
    pub global_volume_ramp_amount: i32,
    pub global_volume_destination: i32,
    pub high_res_ramping_global_volume: i32,
    pub flags: u16,
    pub chn_mix: Vec<ChannelIndex>,
    pub chn: Vec<ModChannel>,
}

impl Default for PlayState {
    fn default() -> Self {
        PlayState {
            total_sample_count: 0,
            buffer_count: 0,
            buffer_diff: 0.0,
            tick_count: 0,
            pattern_delay: 0,
            frame_delay: 0,
            samples_per_tick: 0,
            current_rows_per_beat: 0,
            current_rows_per_measure: 0,
            music_speed: 0,
            music_tempo: Tempo::default(),
            row: 0,
            next_row: 0,
            next_pat_start_row: 0,
            break_row: 0,
            pat_loop_row: 0,
            pos_jump: 0,
            pattern: 0,
            current_order: 0,
            next_order: 0,
            seq_override: ORDERINDEX_INVALID,
            last_moved_channel: CHANNELINDEX_INVALID,
            global_volume: MAX_GLOBAL_VOLUME as i32,
            samples_to_global_vol_ramp_dest: 0,
            global_volume_ramp_amount: 0,
            global_volume_destination: 0,
            high_res_ramping_global_volume: 0,
            flags: SONG_POSITIONCHANGED,
            chn_mix: vec![0; MAX_CHANNELS],
            chn: vec![ModChannel::default(); MAX_CHANNELS],
        }
    }
}

impl PlayState {
    pub fn flag(&self, f: u16) -> bool {
        self.flags & f != 0
    }
    pub fn set_flag(&mut self, f: u16, on: bool) {
        if on {
            self.flags |= f;
        } else {
            self.flags &= !f;
        }
    }
    pub fn ticks_on_row(&self) -> u32 {
        (self.music_speed.wrapping_add(self.frame_delay)).wrapping_mul(self.pattern_delay.max(1))
    }
    pub fn reset_global_volume_ramping(&mut self) {
        self.high_res_ramping_global_volume = self.global_volume << VOLUMERAMPPRECISION;
        self.global_volume_destination = self.global_volume;
        self.samples_to_global_vol_ramp_dest = 0;
        self.global_volume_ramp_amount = 0;
    }
    pub fn update_time_signature(&mut self, m: &Module) {
        let pat = m.patterns.get(self.pattern as usize);
        match pat {
            Some(p) if p.override_signature() => {
                self.current_rows_per_beat = p.rows_per_beat;
                self.current_rows_per_measure = p.rows_per_measure;
            }
            _ => {
                self.current_rows_per_beat = m.default_rows_per_beat;
                self.current_rows_per_measure = m.default_rows_per_measure;
            }
        }
    }
}

/// Fresh channels for a loaded song (`CreateInternal`'s channel reset).
pub fn initial_channels(m: &Module, ps: &mut PlayState) {
    for i in 0..m.num_channels() {
        let mut c = std::mem::take(&mut ps.chn[i]);
        c.reset(RESET_TOTAL, m, i, CHN_SYNCMUTE);
        ps.chn[i] = c;
    }
}
