//! SoundFont modulators: FluidSynth 2.6.1 `synth/fluid_mod.c` and the
//! default modulators of `fluid_synth_init`.

use crate::conv::{self, PEAK_ATTENUATION, VEL_CB_SIZE};
use crate::generator;

// Source flags (`enum fluid_mod_flags`).
pub const NEGATIVE: u8 = 1;
pub const BIPOLAR: u8 = 2;
pub const CONCAVE: u8 = 4;
pub const CONVEX: u8 = 8;
pub const SWITCH: u8 = 12;
pub const CC: u8 = 16;
const SIGN_MASK: u8 = NEGATIVE;
const POLAR_MASK: u8 = BIPOLAR;
const MAP_MASK: u8 = 0x8C;

// General controller sources (`enum fluid_mod_src`).
pub const SRC_NONE: u8 = 0;
pub const SRC_VELOCITY: u8 = 2;
pub const SRC_KEY: u8 = 3;
pub const SRC_KEYPRESSURE: u8 = 10;
pub const SRC_CHANNELPRESSURE: u8 = 13;
pub const SRC_PITCHWHEEL: u8 = 14;
pub const SRC_PITCHWHEELSENS: u8 = 16;

pub const TRANSFORM_LINEAR: u8 = 0;
pub const TRANSFORM_ABS: u8 = 2;

// MIDI controllers the sources and their checks name.
pub const CC_BANK_SELECT_MSB: u8 = 0x00;
pub const CC_MODULATION_MSB: u8 = 0x01;
pub const CC_DATA_ENTRY_MSB: u8 = 0x06;
pub const CC_VOLUME_MSB: u8 = 0x07;
pub const CC_BALANCE_MSB: u8 = 0x08;
pub const CC_PAN_MSB: u8 = 0x0A;
pub const CC_EXPRESSION_MSB: u8 = 0x0B;
pub const CC_BANK_SELECT_LSB: u8 = 0x20;
pub const CC_DATA_ENTRY_LSB: u8 = 0x26;
pub const CC_PORTAMENTO_CTRL: u8 = 0x54;
pub const CC_EFFECTS_DEPTH1: u8 = 0x5B;
pub const CC_EFFECTS_DEPTH3: u8 = 0x5D;
pub const CC_NRPN_LSB: u8 = 0x62;
pub const CC_RPN_MSB: u8 = 0x65;
pub const CC_ALL_SOUND_OFF: u8 = 0x78;

/// `INVALID_NOTE`: no key.
pub const INVALID_NOTE: u8 = 255;

/// `fluid_mod_t`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mod {
    pub dest: u8,
    pub src1: u8,
    pub flags1: u8,
    pub src2: u8,
    pub flags2: u8,
    pub trans: u8,
    pub amount: f64,
}

impl Mod {
    const fn new(src1: u8, flags1: u8, src2: u8, flags2: u8, dest: usize, amount: f64) -> Mod {
        Mod {
            dest: dest as u8,
            src1,
            flags1,
            src2,
            flags2,
            trans: TRANSFORM_LINEAR,
            amount,
        }
    }

    /// `fluid_mod_test_identity`: same sources, flags and destination.
    pub fn same_as(&self, other: &Mod) -> bool {
        self.dest == other.dest
            && self.src1 == other.src1
            && self.src2 == other.src2
            && self.flags1 == other.flags1
            && self.flags2 == other.flags2
    }

    /// `fluid_mod_has_source`: `cc` selects MIDI controllers, else
    /// general controllers.
    pub fn has_source(&self, cc: bool, ctrl: u8) -> bool {
        (self.src1 == ctrl && ((self.flags1 & CC) != 0) == cc)
            || (self.src2 == ctrl && ((self.flags2 & CC) != 0) == cc)
    }

    /// `fluid_mod_check_sources`: the sources a modulator may use.
    pub fn sources_valid(&self) -> bool {
        if !non_cc_source_valid(self.src1, self.flags1) {
            return false;
        }
        if self.flags1 & CC == 0 && self.src1 == SRC_NONE {
            return true;
        }
        non_cc_source_valid(self.src2, self.flags2)
            && cc_source_valid(self.src1, self.flags1)
            && cc_source_valid(self.src2, self.flags2)
    }
}

fn non_cc_source_valid(src: u8, flags: u8) -> bool {
    flags & CC != 0
        || matches!(
            src,
            SRC_NONE
                | SRC_VELOCITY
                | SRC_KEY
                | SRC_KEYPRESSURE
                | SRC_CHANNELPRESSURE
                | SRC_PITCHWHEEL
                | SRC_PITCHWHEELSENS
        )
}

fn cc_source_valid(src: u8, flags: u8) -> bool {
    flags & CC == 0
        || (src != CC_BANK_SELECT_MSB
            && src != CC_BANK_SELECT_LSB
            && src != CC_DATA_ENTRY_MSB
            && src != CC_DATA_ENTRY_LSB
            && !(CC_NRPN_LSB..=CC_RPN_MSB).contains(&src)
            && src < CC_ALL_SOUND_OFF)
}

/// SF2.01 section 8.4.2 velocity to filter cutoff. FluidSynth keeps it in
/// the default list but makes its value 0.
pub const DEFAULT_VEL2FILTER: Mod = Mod::new(
    SRC_VELOCITY,
    NEGATIVE,
    SRC_VELOCITY,
    SWITCH,
    generator::FILTERFC,
    -2400.0,
);

/// The synth's default modulators, in `new_fluid_synth`'s order.
pub const DEFAULTS: [Mod; 11] = [
    Mod::new(
        SRC_VELOCITY,
        CONCAVE | NEGATIVE,
        0,
        0,
        generator::ATTENUATION,
        PEAK_ATTENUATION,
    ),
    DEFAULT_VEL2FILTER,
    Mod::new(SRC_CHANNELPRESSURE, 0, 0, 0, generator::VIBLFOTOPITCH, 50.0),
    Mod::new(CC_MODULATION_MSB, CC, 0, 0, generator::VIBLFOTOPITCH, 50.0),
    Mod::new(
        CC_VOLUME_MSB,
        CC | CONCAVE | NEGATIVE,
        0,
        0,
        generator::ATTENUATION,
        PEAK_ATTENUATION,
    ),
    Mod::new(CC_PAN_MSB, CC | BIPOLAR, 0, 0, generator::PAN, 500.0),
    Mod::new(
        CC_EXPRESSION_MSB,
        CC | CONCAVE | NEGATIVE,
        0,
        0,
        generator::ATTENUATION,
        PEAK_ATTENUATION,
    ),
    Mod::new(CC_EFFECTS_DEPTH1, CC, 0, 0, generator::REVERBSEND, 200.0),
    Mod::new(CC_EFFECTS_DEPTH3, CC, 0, 0, generator::CHORUSSEND, 200.0),
    Mod::new(
        SRC_PITCHWHEEL,
        BIPOLAR,
        SRC_PITCHWHEELSENS,
        0,
        generator::FINETUNE,
        12700.0,
    ),
    Mod::new(
        CC_BALANCE_MSB,
        CC | CONCAVE | BIPOLAR,
        0,
        0,
        generator::CUSTOM_BALANCE,
        PEAK_ATTENUATION,
    ),
];

/// What a modulator reads: the voice's channel state and note.
pub struct Sources<'a> {
    pub cc: &'a [u8; 128],
    pub key_pressure: &'a [u8; 128],
    pub channel_pressure: u8,
    pub pitch_bend: i16,
    pub pitch_wheel_sensitivity: f32,
    pub modulation_depth_range: f32,
    /// `voice->key`.
    pub key: u8,
    /// `fluid_voice_get_actual_key` and `_velocity`.
    pub actual_key: i32,
    pub actual_velocity: i32,
}

fn source_value(src: u8, flags: u8, range: &mut f64, s: &Sources) -> f64 {
    if flags & CC != 0 {
        let val = s.cc[usize::from(src & 0x7F)];
        if src == CC_PORTAMENTO_CTRL && val == INVALID_NOTE {
            return 0.0;
        }
        return f64::from(val);
    }
    match src {
        SRC_NONE => *range,
        SRC_VELOCITY => f64::from(s.actual_velocity),
        SRC_KEY => f64::from(s.actual_key),
        SRC_KEYPRESSURE => f64::from(s.key_pressure[usize::from(s.key & 0x7F)]),
        SRC_CHANNELPRESSURE => f64::from(s.channel_pressure),
        SRC_PITCHWHEEL => {
            *range = f64::from(0x4000);
            f64::from(s.pitch_bend)
        }
        SRC_PITCHWHEELSENS => f64::from(s.pitch_wheel_sensitivity),
        _ => 0.0,
    }
}

fn transform(m: &Mod, val: f64, range: f64, is_src1: bool, s: &Sources) -> f64 {
    let val_norm = val / range;
    let inv_norm = 1.0 - 1.0 / range - val_norm;
    let (src, mut flags) = if is_src1 {
        (m.src1, m.flags1)
    } else {
        (m.src2, m.flags2)
    };
    let apply_depth = is_src1
        && flags & CC != 0
        && src == CC_MODULATION_MSB
        && (usize::from(m.dest) == generator::VIBLFOTOPITCH
            || usize::from(m.dest) == generator::MODLFOTOPITCH);
    flags &= !CC;
    if src == SRC_NONE {
        return 1.0;
    }
    let top = (range - 1.0) / range;
    let cb = VEL_CB_SIZE as f64;
    let mut val;
    if flags & POLAR_MASK == 0 {
        val = if flags & SIGN_MASK == NEGATIVE {
            inv_norm
        } else {
            val_norm
        };
        match flags & MAP_MASK {
            SWITCH => val = if val >= 0.5 { 1.0 } else { 0.0 },
            CONCAVE => val = conv::concave(cb * val).min(top),
            CONVEX => val = conv::convex(cb * val).min(top),
            _ => {}
        }
    } else {
        let norm = if flags & SIGN_MASK == NEGATIVE {
            inv_norm
        } else {
            val_norm
        };
        val = if norm == top { norm } else { -1.0 + 2.0 * norm };
        match flags & MAP_MASK {
            SWITCH => val = if val >= 0.0 { 1.0 } else { -1.0 },
            CONCAVE => {
                val = if val >= 0.0 {
                    conv::concave(cb * val).min(top)
                } else {
                    -conv::concave(cb * -val)
                };
            }
            CONVEX => {
                val = if val >= 0.0 {
                    conv::convex(cb * val).min(top)
                } else {
                    -conv::convex(cb * -val)
                };
            }
            _ => {}
        }
    }
    if apply_depth {
        val *= f64::from(s.modulation_depth_range / 50.0);
    }
    val
}

/// `fluid_mod_get_value`.
pub fn value(m: &Mod, s: &Sources) -> f64 {
    if m.same_as(&DEFAULT_VEL2FILTER) {
        return 0.0;
    }
    let (mut range1, mut range2) = (128.0, 128.0);
    let v1 = source_value(m.src1, m.flags1, &mut range1, s);
    let v1 = transform(m, v1, range1, true, s);
    let v2 = source_value(m.src2, m.flags2, &mut range2, s);
    let v2 = transform(m, v2, range2, false, s);
    let v = m.amount * v1 * v2;
    if m.trans == TRANSFORM_ABS { v.abs() } else { v }
}
