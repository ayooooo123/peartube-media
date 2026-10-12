//! Core definitions, ported from libopenmpt 0.8.9 `soundlib/Snd_defs.h`,
//! `Mixer.h` and `modcommand.h`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

#![allow(dead_code)]

pub type RowIndex = u32;
pub type ChannelIndex = u16;
pub type OrderIndex = u16;
pub type PatternIndex = u16;
pub type SampleIndex = u16;
pub type InstrumentIndex = u16;
pub type SmpLength = u32;

pub const ROWINDEX_INVALID: RowIndex = u32::MAX;
pub const CHANNELINDEX_INVALID: ChannelIndex = u16::MAX;
pub const ORDERINDEX_INVALID: OrderIndex = u16::MAX;
pub const ORDERINDEX_MAX: OrderIndex = u16::MAX - 1;
pub const PATTERNINDEX_INVALID: PatternIndex = u16::MAX;
pub const PATTERNINDEX_SKIP: PatternIndex = u16::MAX - 1;

pub const MAX_SAMPLE_LENGTH: SmpLength = 0x1000_0000;
pub const MAX_PATTERN_ROWS: RowIndex = 4096;
pub const MAX_ORDERS: usize = ORDERINDEX_MAX as usize + 1;
pub const MAX_PATTERNS: usize = 4000;
pub const MAX_SAMPLES: usize = 4000;
pub const MAX_INSTRUMENTS: usize = 256;
pub const MAX_BASECHANNELS: usize = 192;
pub const MAX_CHANNELS: usize = 256;
pub const FREQ_FRACBITS: u32 = 4;
pub const DEFAULT_ROWS_PER_BEAT: RowIndex = 4;
pub const DEFAULT_ROWS_PER_MEASURE: RowIndex = 16;

// MODTYPE
pub const MOD_TYPE_NONE: u32 = 0x00;
pub const MOD_TYPE_MOD: u32 = 0x01;
pub const MOD_TYPE_S3M: u32 = 0x02;
pub const MOD_TYPE_XM: u32 = 0x04;
pub const MOD_TYPE_MED: u32 = 0x08;
pub const MOD_TYPE_MTM: u32 = 0x10;
pub const MOD_TYPE_IT: u32 = 0x20;
pub const MOD_TYPE_669: u32 = 0x40;
pub const MOD_TYPE_ULT: u32 = 0x80;
pub const MOD_TYPE_STM: u32 = 0x100;
pub const MOD_TYPE_FAR: u32 = 0x200;
pub const MOD_TYPE_DTM: u32 = 0x400;
pub const MOD_TYPE_AMF: u32 = 0x800;
pub const MOD_TYPE_AMS: u32 = 0x1000;
pub const MOD_TYPE_DSM: u32 = 0x2000;
pub const MOD_TYPE_MDL: u32 = 0x4000;
pub const MOD_TYPE_OKT: u32 = 0x8000;
pub const MOD_TYPE_MID: u32 = 0x10000;
pub const MOD_TYPE_DMF: u32 = 0x20000;
pub const MOD_TYPE_PTM: u32 = 0x40000;
pub const MOD_TYPE_DBM: u32 = 0x80000;
pub const MOD_TYPE_MT2: u32 = 0x100000;
pub const MOD_TYPE_AMF0: u32 = 0x200000;
pub const MOD_TYPE_PSM: u32 = 0x400000;
pub const MOD_TYPE_J2B: u32 = 0x800000;
pub const MOD_TYPE_MPT: u32 = 0x1000000;
pub const MOD_TYPE_IMF: u32 = 0x2000000;
pub const MOD_TYPE_DIGI: u32 = 0x4000000;
pub const MOD_TYPE_STP: u32 = 0x8000000;
pub const MOD_TYPE_PLM: u32 = 0x10000000;
pub const MOD_TYPE_SFX: u32 = 0x20000000;

// ChannelFlags
pub const CHN_16BIT: u32 = 0x01;
pub const CHN_LOOP: u32 = 0x02;
pub const CHN_PINGPONGLOOP: u32 = 0x04;
pub const CHN_SUSTAINLOOP: u32 = 0x08;
pub const CHN_PINGPONGSUSTAIN: u32 = 0x10;
pub const CHN_PANNING: u32 = 0x20;
pub const CHN_STEREO: u32 = 0x40;
pub const CHN_REVERSE: u32 = 0x80;
pub const CHN_SURROUND: u32 = 0x100;
pub const CHN_ADLIB: u32 = 0x200;
pub const CHN_PINGPONGFLAG: u32 = 0x80;
pub const CHN_MUTE: u32 = 0x400;
pub const CHN_KEYOFF: u32 = 0x800;
pub const CHN_NOTEFADE: u32 = 0x1000;
pub const CHN_WRAPPED_LOOP: u32 = 0x2000;
pub const CHN_AMIGAFILTER: u32 = 0x4000;
pub const CHN_FILTER: u32 = 0x8000;
pub const CHN_VOLUMERAMP: u32 = 0x10000;
pub const CHN_VIBRATO: u32 = 0x20000;
pub const CHN_TREMOLO: u32 = 0x40000;
pub const CHN_PORTAMENTO: u32 = 0x80000;
pub const CHN_GLISSANDO: u32 = 0x100000;
pub const CHN_FASTVOLRAMP: u32 = 0x200000;
pub const CHN_EXTRALOUD: u32 = 0x400000;
pub const CHN_REVERB: u32 = 0x800000;
pub const CHN_NOREVERB: u32 = 0x1000000;
pub const CHN_NOFX: u32 = 0x2000000;
pub const CHN_SYNCMUTE: u32 = 0x4000000;
pub const SMP_MODIFIED: u32 = 0x2000;
pub const SMP_KEEPONDISK: u32 = 0x4000;
pub const SMP_NODEFAULTVOLUME: u32 = 0x8000;

pub const CHN_SAMPLEFLAGS: u32 = CHN_16BIT
    | CHN_LOOP
    | CHN_PINGPONGLOOP
    | CHN_SUSTAINLOOP
    | CHN_PINGPONGSUSTAIN
    | CHN_PANNING
    | CHN_STEREO
    | CHN_PINGPONGFLAG
    | CHN_REVERSE
    | CHN_SURROUND
    | CHN_ADLIB;
pub const CHN_CHANNELFLAGS: u32 = !CHN_SAMPLEFLAGS | CHN_SURROUND;

// EnvelopeFlags
pub const ENV_ENABLED: u8 = 0x01;
pub const ENV_LOOP: u8 = 0x02;
pub const ENV_SUSTAIN: u8 = 0x04;
pub const ENV_CARRY: u8 = 0x08;
pub const ENV_FILTER: u8 = 0x10;

pub const ENVELOPE_MIN: u8 = 0;
pub const ENVELOPE_MID: u8 = 32;
pub const ENVELOPE_MAX: u8 = 64;
pub const MAX_ENVPOINTS: usize = 240;
pub const ENV_RELEASE_NODE_UNSET: u8 = 0xFF;
pub const NOT_YET_RELEASED: i16 = -1;

// InstrumentFlags
pub const INS_SETPANNING: u8 = 0x01;
pub const INS_MUTE: u8 = 0x02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvelopeType {
    Volume,
    Panning,
    Pitch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterMode {
    Unchanged = 0xFF,
    LowPass = 0,
    HighPass = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewNoteAction {
    NoteCut = 0,
    Continue = 1,
    NoteOff = 2,
    NoteFade = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DuplicateCheckType {
    None = 0,
    Note = 1,
    Sample = 2,
    Instrument = 3,
    Plugin = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DuplicateNoteAction {
    NoteCut = 0,
    NoteOff = 1,
    NoteFade = 2,
}

// PlayFlags
pub const SONG_PATTERNLOOP: u16 = 0x01;
pub const SONG_STEP: u16 = 0x02;
pub const SONG_PAUSED: u16 = 0x04;
pub const SONG_FADINGSONG: u16 = 0x08;
pub const SONG_ENDREACHED: u16 = 0x10;
pub const SONG_FIRSTTICK: u16 = 0x20;
pub const SONG_MPTFILTERMODE: u16 = 0x40;
pub const SONG_SURROUNDPAN: u16 = 0x80;
pub const SONG_POSJUMP: u16 = 0x100;
pub const SONG_BREAKTOROW: u16 = 0x200;
pub const SONG_POSITIONCHANGED: u16 = 0x400;

// SongFlags
pub const SONG_FASTPORTAS: u32 = 0x01;
pub const SONG_FASTVOLSLIDES: u32 = 0x02;
pub const SONG_ITOLDEFFECTS: u32 = 0x04;
pub const SONG_ITCOMPATGXX: u32 = 0x08;
pub const SONG_LINEARSLIDES: u32 = 0x10;
pub const SONG_EXFILTERRANGE: u32 = 0x20;
pub const SONG_AMIGALIMITS: u32 = 0x40;
pub const SONG_S3MOLDVIBRATO: u32 = 0x80;
pub const SONG_PT_MODE: u32 = 0x100;
pub const SONG_ISAMIGA: u32 = 0x200;
pub const SONG_IMPORTED: u32 = 0x400;
pub const SONG_PLAYALLSONGS: u32 = 0x800;
pub const SONG_AUTO_TONEPORTA: u32 = 0x1000;
pub const SONG_AUTO_TONEPORTA_CONT: u32 = 0x2000;
pub const SONG_AUTO_GLOBALVOL: u32 = 0x4000;
pub const SONG_AUTO_VIBRATO: u32 = 0x8000;
pub const SONG_AUTO_TREMOLO: u32 = 0x1_8000;
pub const SONG_AUTO_VOLSLIDE_STK: u32 = 0x2_0000;
pub const SONG_FORMAT_NO_VOLCOL: u32 = 0x4_0000;

pub const MAX_GLOBAL_VOLUME: u32 = 256;
pub const MAX_PREAMP: u32 = 2000;
pub const FADESONGDELAY: u32 = 100;

// ResamplingMode
pub const SRCMODE_NEAREST: u8 = 0;
pub const SRCMODE_LINEAR: u8 = 1;
pub const SRCMODE_CUBIC: u8 = 2;
pub const SRCMODE_SINC8: u8 = 4;
pub const SRCMODE_SINC8LP: u8 = 3;
pub const SRCMODE_DEFAULT: u8 = 5;
pub const SRCMODE_AMIGA: u8 = 0xFF;

// VibratoType
pub const VIB_SINE: u8 = 0;
pub const VIB_SQUARE: u8 = 1;
pub const VIB_RAMP_UP: u8 = 2;
pub const VIB_RAMP_DOWN: u8 = 3;
pub const VIB_RANDOM: u8 = 4;

// Mixer.h
pub const MIXING_ATTENUATION: u32 = 4;
pub const MIXING_FRACTIONAL_BITS: u32 = 27;
pub const MIXING_FILTER_PRECISION: u32 = 24;
pub const MIXBUFFERSIZE: usize = 512;
pub const VOLUMERAMPPRECISION: u32 = 12;
pub const INTERPOLATION_MAX_LOOKAHEAD: u32 = 4;
pub const INTERPOLATION_LOOKAHEAD_BUFFER_SIZE: u32 = 16;
pub const MAX_SAMPLING_POINT_SIZE: u32 = 4;

/// Fixed-point 32.32 sample position (`SamplePosition`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct SamplePosition(pub i64);

impl SamplePosition {
    pub const fn new(int_part: i32, fract: u32) -> Self {
        SamplePosition(((int_part as i64) << 32) | fract as i64)
    }
    pub fn ratio(dividend: u32, divisor: u32) -> Self {
        SamplePosition(((dividend as i64) << 32) / divisor as i64)
    }
    pub fn from_double(pos: f64) -> Self {
        SamplePosition((pos * 4294967296.0) as i64)
    }
    pub fn to_double(self) -> f64 {
        self.0 as f64 / 4294967296.0
    }
    pub fn set(&mut self, int_part: i32, fract: u32) {
        self.0 = ((int_part as i64) << 32) | fract as i64;
    }
    pub fn set_int(&mut self, int_part: i32) {
        self.0 = ((int_part as i64) << 32) | self.fract() as i64;
    }
    pub fn uint(self) -> SmpLength {
        ((self.0 as u64) >> 32) as u32
    }
    pub fn int(self) -> i32 {
        ((self.0 as u64) >> 32) as i32
    }
    pub fn fract(self) -> u32 {
        self.0 as u32
    }
    pub fn inverted_fract(self) -> Self {
        SamplePosition(0x1_0000_0000i64 - self.fract() as i64)
    }
    pub fn raw(self) -> i64 {
        self.0
    }
    pub fn negate(&mut self) {
        self.0 = self.0.wrapping_neg();
    }
    pub fn mul_div(&mut self, mul: u32, div: u32) {
        self.0 = self.0.wrapping_mul(mul as i64) / div as i64;
    }
    pub fn remove_int(&mut self) {
        self.0 &= 0xFFFF_FFFF;
    }
    pub fn is_unity(self) -> bool {
        self.0 == 0x1_0000_0000
    }
    pub fn is_zero(self) -> bool {
        self.0 == 0
    }
    pub fn is_positive(self) -> bool {
        self.0 > 0
    }
    pub fn is_negative(self) -> bool {
        self.0 < 0
    }
    pub fn mul(self, other: i64) -> Self {
        SamplePosition(self.0.wrapping_mul(other))
    }
    pub fn div_pos(self, other: SamplePosition) -> i64 {
        self.0 / other.0
    }
    pub fn div(self, d: i64) -> Self {
        SamplePosition(self.0 / d)
    }
}

impl core::ops::Add for SamplePosition {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        SamplePosition(self.0.wrapping_add(o.0))
    }
}
impl core::ops::Sub for SamplePosition {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        SamplePosition(self.0.wrapping_sub(o.0))
    }
}
impl core::ops::AddAssign for SamplePosition {
    fn add_assign(&mut self, o: Self) {
        self.0 = self.0.wrapping_add(o.0);
    }
}
impl core::ops::SubAssign for SamplePosition {
    fn sub_assign(&mut self, o: Self) {
        self.0 = self.0.wrapping_sub(o.0);
    }
}

/// `TEMPO` = `FPInt<10000, uint32>`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tempo(pub u32);

impl Tempo {
    pub const FRACT_FACT: u32 = 10000;
    pub const fn new(int_part: u32, fract: u32) -> Self {
        Tempo(int_part * 10000 + fract % 10000)
    }
    pub fn from_double(f: f64) -> Self {
        let v = (f * 10000.0).round();
        Tempo(if v < 0.0 { 0 } else if v > u32::MAX as f64 { u32::MAX } else { v as u32 })
    }
    pub fn int(self) -> u32 {
        self.0 / 10000
    }
    pub fn fract(self) -> u32 {
        self.0 % 10000
    }
    pub fn raw(self) -> u32 {
        self.0
    }
    pub fn to_double(self) -> f64 {
        self.0 as f64 / 10000.0
    }
}

impl core::ops::Add for Tempo {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Tempo(self.0.wrapping_add(o.0))
    }
}
impl core::ops::Sub for Tempo {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Tempo(self.0.wrapping_sub(o.0))
    }
}

fn sat_i32(v: i64) -> i32 {
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

fn sat_u32(v: u64) -> u32 {
    v.min(u32::MAX as u64) as u32
}

/// `Util::muldiv`: saturate_cast<int32>(a * b / c), 64-bit intermediate.
pub fn muldiv(a: i32, b: i32, c: i32) -> i32 {
    if c == 0 {
        return 0;
    }
    sat_i32((a as i64 * b as i64) / c as i64)
}

/// `Util::muldivr`: saturate_cast<int32>((a * b + c / 2) / c).
pub fn muldivr(a: i32, b: i32, c: i32) -> i32 {
    if c == 0 {
        return 0;
    }
    sat_i32((a as i64 * b as i64 + (c / 2) as i64) / c as i64)
}

/// `Util::muldiv_unsigned`.
pub fn muldiv_unsigned(a: u32, b: u32, c: u32) -> u32 {
    if c == 0 {
        return 0;
    }
    sat_u32((a as u64 * b as u64) / c as u64)
}

/// `Util::muldivr_unsigned`: (a * b + c / 2) / c.
pub fn muldivr_unsigned(a: u32, b: u32, c: u32) -> u32 {
    if c == 0 {
        return 0;
    }
    sat_u32((a as u64 * b as u64 + (c / 2) as u64) / c as u64)
}

/// `Util::muldivrfloor`.
pub fn muldivrfloor(a: i64, b: u32, c: u32) -> i32 {
    if c == 0 {
        return 0;
    }
    let a = a.wrapping_mul(b as i64).wrapping_add((c / 2) as i64);
    if a >= 0 {
        sat_i32(a / c as i64)
    } else {
        sat_i32((a - (c as i64 - 1)) / c as i64)
    }
}

/// `mpt::rshift_signed`: arithmetic right shift.
pub const fn rshift_signed(x: i64, s: u32) -> i64 {
    x >> s
}

/// libopenmpt's `PlayBehaviour` flags (Snd_defs.h), at their fixed indices,
/// under their C++ names.
#[allow(non_upper_case_globals, dead_code)]
pub mod pb {
    pub const MSF_COMPATIBLE_PLAY: usize = 0;
    pub const kMPTOldSwingBehaviour: usize = 1;
    pub const kMIDICCBugEmulation: usize = 2;
    pub const kOldMIDIPitchBends: usize = 3;
    pub const kFT2VolumeRamping: usize = 4;
    pub const kMODVBlankTiming: usize = 5;
    pub const kSlidesAtSpeed1: usize = 6;
    pub const kPeriodsAreHertz: usize = 7;
    pub const kTempoClamp: usize = 8;
    pub const kPerChannelGlobalVolSlide: usize = 9;
    pub const kPanOverride: usize = 10;
    pub const kITInstrWithoutNote: usize = 11;
    pub const kITVolColFinePortamento: usize = 12;
    pub const kITArpeggio: usize = 13;
    pub const kITOutOfRangeDelay: usize = 14;
    pub const kITPortaMemoryShare: usize = 15;
    pub const kITPatternLoopTargetReset: usize = 16;
    pub const kITFT2PatternLoop: usize = 17;
    pub const kITPingPongNoReset: usize = 18;
    pub const kITEnvelopeReset: usize = 19;
    pub const kITClearOldNoteAfterCut: usize = 20;
    pub const kITVibratoTremoloPanbrello: usize = 21;
    pub const kITTremor: usize = 22;
    pub const kITRetrigger: usize = 23;
    pub const kITMultiSampleBehaviour: usize = 24;
    pub const kITPortaTargetReached: usize = 25;
    pub const kITPatternLoopBreak: usize = 26;
    pub const kITOffset: usize = 27;
    pub const kITSwingBehaviour: usize = 28;
    pub const kITNNAReset: usize = 29;
    pub const kITSCxStopsSample: usize = 30;
    pub const kITEnvelopePositionHandling: usize = 31;
    pub const kITPortamentoInstrument: usize = 32;
    pub const kITPingPongMode: usize = 33;
    pub const kITRealNoteMapping: usize = 34;
    pub const kITHighOffsetNoRetrig: usize = 35;
    pub const kITFilterBehaviour: usize = 36;
    pub const kITNoSurroundPan: usize = 37;
    pub const kITShortSampleRetrig: usize = 38;
    pub const kITPortaNoNote: usize = 39;
    pub const kITFT2DontResetNoteOffOnPorta: usize = 40;
    pub const kITVolColMemory: usize = 41;
    pub const kITPortamentoSwapResetsPos: usize = 42;
    pub const kITEmptyNoteMapSlot: usize = 43;
    pub const kITFirstTickHandling: usize = 44;
    pub const kITSampleAndHoldPanbrello: usize = 45;
    pub const kITClearPortaTarget: usize = 46;
    pub const kITPanbrelloHold: usize = 47;
    pub const kITPanningReset: usize = 48;
    pub const kITPatternLoopWithJumpsOld: usize = 49;
    pub const kITInstrWithNoteOff: usize = 50;
    pub const kFT2Arpeggio: usize = 51;
    pub const kFT2Retrigger: usize = 52;
    pub const kFT2VolColVibrato: usize = 53;
    pub const kFT2PortaNoNote: usize = 54;
    pub const kFT2KeyOff: usize = 55;
    pub const kFT2PanSlide: usize = 56;
    pub const kFT2ST3OffsetOutOfRange: usize = 57;
    pub const kFT2RestrictXCommand: usize = 58;
    pub const kFT2RetrigWithNoteDelay: usize = 59;
    pub const kFT2SetPanEnvPos: usize = 60;
    pub const kFT2PortaIgnoreInstr: usize = 61;
    pub const kFT2VolColMemory: usize = 62;
    pub const kFT2LoopE60Restart: usize = 63;
    pub const kFT2ProcessSilentChannels: usize = 64;
    pub const kFT2ReloadSampleSettings: usize = 65;
    pub const kFT2PortaDelay: usize = 66;
    pub const kFT2Transpose: usize = 67;
    pub const kFT2PatternLoopWithJumps: usize = 68;
    pub const kFT2PortaTargetNoReset: usize = 69;
    pub const kFT2EnvelopeEscape: usize = 70;
    pub const kFT2Tremor: usize = 71;
    pub const kFT2OutOfRangeDelay: usize = 72;
    pub const kFT2Periods: usize = 73;
    pub const kFT2PanWithDelayedNoteOff: usize = 74;
    pub const kFT2VolColDelay: usize = 75;
    pub const kFT2FinetunePrecision: usize = 76;
    pub const kST3NoMutedChannels: usize = 77;
    pub const kST3EffectMemory: usize = 78;
    pub const kST3PortaSampleChange: usize = 79;
    pub const kST3VibratoMemory: usize = 80;
    pub const kST3LimitPeriod: usize = 81;
    pub const KST3PortaAfterArpeggio: usize = 82;
    pub const kMODOneShotLoops: usize = 83;
    pub const kMODIgnorePanning: usize = 84;
    pub const kMODSampleSwap: usize = 85;
    pub const kFT2NoteOffFlags: usize = 86;
    pub const kITMultiSampleInstrumentNumber: usize = 87;
    pub const kRowDelayWithNoteDelay: usize = 88;
    pub const kFT2MODTremoloRampWaveform: usize = 89;
    pub const kFT2PortaUpDownMemory: usize = 90;
    pub const kMODOutOfRangeNoteDelay: usize = 91;
    pub const kMODTempoOnSecondTick: usize = 92;
    pub const kFT2PanSustainRelease: usize = 93;
    pub const kLegacyReleaseNode: usize = 94;
    pub const kOPLBeatingOscillators: usize = 95;
    pub const kST3OffsetWithoutInstrument: usize = 96;
    pub const kReleaseNodePastSustainBug: usize = 97;
    pub const kFT2NoteDelayWithoutInstr: usize = 98;
    pub const kOPLFlexibleNoteOff: usize = 99;
    pub const kITInstrWithNoteOffOldEffects: usize = 100;
    pub const kMIDIVolumeOnNoteOffBug: usize = 101;
    pub const kITDoNotOverrideChannelPan: usize = 102;
    pub const kITPatternLoopWithJumps: usize = 103;
    pub const kITDCTBehaviour: usize = 104;
    pub const kOPLwithNNA: usize = 105;
    pub const kST3RetrigAfterNoteCut: usize = 106;
    pub const kST3SampleSwap: usize = 107;
    pub const kOPLRealRetrig: usize = 108;
    pub const kOPLNoResetAtEnvelopeEnd: usize = 109;
    pub const kOPLNoteStopWith0Hz: usize = 110;
    pub const kOPLNoteOffOnNoteChange: usize = 111;
    pub const kFT2PortaResetDirection: usize = 112;
    pub const kApplyUpperPeriodLimit: usize = 113;
    pub const kApplyOffsetWithoutNote: usize = 114;
    pub const kITPitchPanSeparation: usize = 115;
    pub const kImprecisePingPongLoops: usize = 116;
    pub const kPluginIgnoreTonePortamento: usize = 117;
    pub const kST3TonePortaWithAdlibNote: usize = 118;
    pub const kITResetFilterOnPortaSmpChange: usize = 119;
    pub const kITInitialNoteMemory: usize = 120;
    pub const kPluginDefaultProgramAndBank1: usize = 121;
    pub const kITNoSustainOnPortamento: usize = 122;
    pub const kITEmptyNoteMapSlotIgnoreCell: usize = 123;
    pub const kITOffsetWithInstrNumber: usize = 124;
    pub const kContinueSampleWithoutInstr: usize = 125;
    pub const kMIDINotesFromChannelPlugin: usize = 126;
    pub const kITDoublePortamentoSlides: usize = 127;
    pub const kS3MIgnoreCombinedFineSlides: usize = 128;
    pub const kFT2AutoVibratoAbortSweep: usize = 129;
    pub const kLegacyPPQpos: usize = 130;
    pub const kLegacyPluginNNABehaviour: usize = 131;
    pub const kITCarryAfterNoteOff: usize = 132;
    pub const kFT2OffsetMemoryRequiresNote: usize = 133;
    pub const kITNoteCutWithPorta: usize = 134;
    pub const kITVolColNoSlidePropagation: usize = 135;
    pub const kITStoppedFilterEnvAtStart: usize = 136;
    pub const kITCompatGxxCarryPortaWithIns: usize = 137;
    pub const kMaxPlayBehaviours: usize = 138;
}
