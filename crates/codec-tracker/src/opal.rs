//! Opal OPL3 emulator, ported from libopenmpt 0.8.9 `soundlib/opal.h`.
//! Shayde/Reality released Opal into the public domain. This includes the
//! OpenMPT envelope fixes and JP Cimalando's corrections. The tracker uses
//! melodic two-operator voices; Opal also implements its four-operator modes.
//! As upstream, hardware timers, CSM and percussion mode are not emulated.

const CHIP_RATE: i32 = 49716;
const CHANNEL_OP: [usize; 18] = [0, 1, 2, 6, 7, 8, 12, 13, 14, 18, 19, 20, 24, 25, 26, 30, 31, 32];
const RATE_TABLES: [[u16; 8]; 4] = [
    [1, 0, 1, 0, 1, 0, 1, 0],
    [1, 0, 1, 0, 0, 0, 1, 0],
    [1, 0, 0, 0, 1, 0, 0, 0],
    [1, 0, 0, 0, 0, 0, 0, 0],
];

#[derive(Clone, Copy)]
struct Rate {
    shift: u16,
    mask: u16,
    add: u16,
    table: usize,
}

impl Rate {
    fn new(rate: u16, key_scale: u16, attack: bool) -> Self {
        let combined = rate * 4 + key_scale;
        let high = combined >> 2;
        let shift = 12u16.saturating_sub(high);
        Self {
            shift,
            mask: (1 << shift) - 1,
            add: if attack && rate == 15 { 0xFFF } else { 1 << high.saturating_sub(12) },
            table: (combined & 3) as usize,
        }
    }

    fn step(self, clock: u16) -> u16 {
        self.add >> RATE_TABLES[self.table][((clock >> self.shift) & 7) as usize]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage { Off, Attack, Decay, Sustain, Release }

#[derive(Clone, Copy)]
struct Operator {
    phase: u32,
    waveform: u8,
    multiplier: u16,
    stage: Stage,
    level: i16,
    output_level: u16,
    attack_rate: u16,
    decay_rate: u16,
    sustain_level: i16,
    release_rate: u16,
    attack: Rate,
    decay: Rate,
    release: Rate,
    key_scale_shift: u8,
    key_scale_level: u16,
    history: [i16; 2],
    key_on: bool,
    key_scale_rate: bool,
    sustain: bool,
    tremolo: bool,
    vibrato: bool,
}

impl Default for Operator {
    fn default() -> Self {
        Self {
            phase: 0, waveform: 0, multiplier: 1, stage: Stage::Off,
            level: 0x1FF, output_level: 0, attack_rate: 0, decay_rate: 0,
            sustain_level: 0, release_rate: 0,
            attack: Rate::new(0, 0, true), decay: Rate::new(0, 0, false),
            release: Rate::new(0, 0, false), key_scale_shift: 0,
            key_scale_level: 0, history: [0; 2], key_on: false,
            key_scale_rate: false, sustain: false, tremolo: false, vibrato: false,
        }
    }
}

impl Operator {
    fn rates(&mut self, key_scale: u16) {
        let key_scale = key_scale >> if self.key_scale_rate { 0 } else { 2 };
        self.attack = Rate::new(self.attack_rate, key_scale, true);
        self.decay = Rate::new(self.decay_rate, key_scale, false);
        self.release = Rate::new(self.release_rate, key_scale, false);
    }

    fn key_on(&mut self, on: bool) {
        if self.key_on == on { return; }
        self.key_on = on;
        if on {
            if self.attack_rate == 15 {
                self.stage = Stage::Decay;
                self.level = 0;
            } else {
                self.stage = Stage::Attack;
            }
            self.phase = 0;
        } else if self.stage != Stage::Off && self.stage != Stage::Release {
            self.stage = Stage::Release;
        }
    }

    fn output(&mut self, mut step: u32, vibrato: i16, mut modulation: i16, feedback: u16, clock: u16, tremolo: u16) -> i16 {
        if self.vibrato { step = step.wrapping_add(vibrato as u32); }
        self.phase = self.phase.wrapping_add(step.wrapping_mul(self.multiplier as u32) / 2);
        // Opal computes attenuation before advancing the envelope.
        let level = (self.level as u16 + self.output_level + self.key_scale_level
            + if self.tremolo { tremolo } else { 0 }) << 3;
        match self.stage {
            Stage::Attack => {
                let add = if self.attack_rate == 0 || clock & self.attack.mask != 0 { 0 }
                    else { ((self.attack.step(clock) as i32 * !(self.level as i32)) >> 3) as i16 };
                self.level = self.level.wrapping_add(add);
                if self.level <= 0 { self.level = 0; self.stage = Stage::Decay; }
            }
            Stage::Decay => {
                if self.decay_rate != 0 && clock & self.decay.mask == 0 {
                    self.level += self.decay.step(clock) as i16;
                }
                if self.level >= self.sustain_level {
                    self.level = self.sustain_level;
                    self.stage = Stage::Sustain;
                }
            }
            Stage::Sustain if self.sustain => {}
            Stage::Sustain | Stage::Release => {
                if self.release_rate != 0 && clock & self.release.mask == 0 {
                    self.level += self.release.step(clock) as i16;
                }
                if self.level >= 0x1FF {
                    self.level = 0x1FF;
                    self.stage = Stage::Off;
                    self.history = [0; 2];
                    return 0;
                }
            }
            Stage::Off => { self.history = [0; 2]; return 0; }
        }
        if feedback != 0 {
            modulation = modulation.wrapping_add(((self.history[0] as i32 + self.history[1] as i32) >> feedback) as i16);
        }
        let phase = ((self.phase >> 10) as u16).wrapping_add(modulation as u16);
        let mut offset = (phase & 0xFF) as usize;
        let mut negate = false;
        let logsin = match self.waveform {
            0 => {
                if phase & 0x100 != 0 { offset ^= 0xFF; }
                negate = phase & 0x200 != 0;
                LOG_SIN[offset]
            }
            1 => {
                if phase & 0x200 != 0 { offset = 0; }
                else if phase & 0x100 != 0 { offset ^= 0xFF; }
                LOG_SIN[offset]
            }
            2 => {
                if phase & 0x100 != 0 { offset ^= 0xFF; }
                LOG_SIN[offset]
            }
            3 => {
                if phase & 0x100 != 0 { offset = 0; }
                LOG_SIN[offset]
            }
            4 => {
                if phase & 0x200 != 0 { offset = 0; }
                else {
                    if phase & 0x80 != 0 { offset ^= 0xFF; }
                    offset = (offset * 2) & 0xFF;
                    negate = phase & 0x100 != 0;
                }
                LOG_SIN[offset]
            }
            5 => {
                if phase & 0x200 != 0 { offset = 0; }
                else {
                    offset = (offset * 2) & 0xFF;
                    if phase & 0x80 != 0 { offset ^= 0xFF; }
                }
                LOG_SIN[offset]
            }
            6 => { negate = phase & 0x200 != 0; 0 }
            _ => {
                let mut v = phase & 0x1FF;
                if phase & 0x200 != 0 { v ^= 0x1FF; negate = true; }
                v << 3
            }
        };
        let mix = (logsin + level).min(0x1FFF);
        let mut value = (((EXP[(mix & 0xFF) as usize] as u32 + 1024) >> (mix >> 8)) * 2) as i16;
        if negate { value = !value; }
        self.history[1] = self.history[0];
        self.history[0] = value;
        value
    }
}

#[derive(Clone, Copy)]
struct Channel {
    frequency: u16,
    octave: u16,
    phase_step: u32,
    key_scale: u16,
    feedback: u16,
    additive: bool,
    pair: bool,
    enabled: bool,
    left: bool,
    right: bool,
}

impl Default for Channel {
    fn default() -> Self {
        Self { frequency: 0, octave: 0, phase_step: 0, key_scale: 0,
            feedback: 0, additive: false, pair: false, enabled: true, left: true, right: true }
    }
}

pub struct Opal {
    channels: [Channel; 18],
    operators: [Operator; 36],
    sample_rate: i32,
    sample_accum: i32,
    previous: [i16; 2],
    current: [i16; 2],
    clock: u16,
    tremolo_clock: u16,
    tremolo_level: u16,
    vibrato_tick: u16,
    vibrato_clock: u16,
    note_select: bool,
    tremolo_depth: bool,
    vibrato_depth: bool,
}

impl Opal {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            channels: [Channel::default(); 18], operators: [Operator::default(); 36],
            sample_rate: if sample_rate == 0 { CHIP_RATE } else { sample_rate as i32 },
            sample_accum: 0, previous: [0; 2], current: [0; 2], clock: 0,
            tremolo_clock: 0, tremolo_level: 0, vibrato_tick: 0, vibrato_clock: 0,
            note_select: false, tremolo_depth: false, vibrato_depth: false,
        }
    }

    fn owner(operator: usize) -> usize { operator / 6 * 3 + operator % 3 }

    fn update_operator(&mut self, i: usize) {
        let ch = &self.channels[Self::owner(i)];
        let op = &mut self.operators[i];
        op.rates(ch.key_scale);
        op.key_scale_level = KEY_SCALE[(ch.octave * 16 + (ch.frequency >> 6)) as usize] >> op.key_scale_shift;
    }

    fn update_channel(&mut self, i: usize) {
        let ch = &mut self.channels[i];
        ch.phase_step = (ch.frequency as u32) << ch.octave;
        ch.key_scale = ch.octave * 2 | if self.note_select { ch.frequency >> 9 } else { (ch.frequency >> 8) & 1 };
        let first = CHANNEL_OP[i];
        self.update_operator(first);
        self.update_operator(first + 3);
        if i % 9 < 3 {
            self.update_operator(first + 6);
            self.update_operator(first + 9);
        }
    }

    pub fn port(&mut self, reg: u16, value: u8) {
        const OP_LOOKUP: [i8; 32] = [0, 1, 2, 3, 4, 5, -1, -1, 6, 7, 8, 9, 10, 11, -1, -1,
            12, 13, 14, 15, 16, 17, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1];
        let kind = reg & 0xE0;
        if reg == 0xBD {
            self.tremolo_depth = value & 0x80 != 0;
            self.vibrato_depth = value & 0x40 != 0;
        } else if kind == 0 {
            if reg == 0x104 {
                for n in 0..6 {
                    let c = if n < 3 { n } else { n + 6 };
                    self.channels[c].pair = value & (1 << n) != 0;
                    self.channels[c + 3].enabled = !self.channels[c].pair;
                }
            } else if reg == 8 {
                self.note_select = value & 0x40 != 0;
                for c in 0..18 { self.update_channel(c); }
            }
        } else if (0xA0..=0xC0).contains(&kind) {
            let mut c = (reg & 15) as usize;
            if c >= 9 { return; }
            if reg & 0x100 != 0 { c += 9; }
            let count = if self.channels[c].pair { 2 } else { 1 };
            match reg & 0xF0 {
                0xA0 | 0xB0 => {
                    for n in 0..count {
                        let i = c + n * 3;
                        if reg & 0xF0 == 0xA0 {
                            let ch = &mut self.channels[i];
                            ch.frequency = (ch.frequency & 0x300) | value as u16;
                            ch.phase_step = (ch.frequency as u32) << ch.octave;
                        } else {
                            let op = CHANNEL_OP[i];
                            self.operators[op].key_on(value & 0x20 != 0);
                            self.operators[op + 3].key_on(value & 0x20 != 0);
                            self.channels[i].octave = ((value >> 2) & 7) as u16;
                            self.update_channel(i);
                            self.channels[i].frequency = (self.channels[i].frequency & 0xFF) | (((value & 3) as u16) << 8);
                            self.update_channel(i);
                        }
                    }
                }
                0xC0 => {
                    let ch = &mut self.channels[c];
                    ch.right = value & 0x20 != 0;
                    ch.left = value & 0x10 != 0;
                    let feedback = ((value >> 1) & 7) as u16;
                    ch.feedback = if feedback == 0 { 0 } else { 9 - feedback };
                    ch.additive = value & 1 != 0;
                }
                _ => {}
            }
        } else if (0x20..=0x80).contains(&kind) || kind == 0xE0 {
            let i = OP_LOOKUP[(reg & 31) as usize];
            if i < 0 { return; }
            let i = i as usize + if reg & 0x100 != 0 { 18 } else { 0 };
            let op = &mut self.operators[i];
            match kind {
                0x20 => {
                    const MULT: [u16; 16] = [1, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 20, 24, 24, 30, 30];
                    op.tremolo = value & 0x80 != 0;
                    op.vibrato = value & 0x40 != 0;
                    op.sustain = value & 0x20 != 0;
                    op.key_scale_rate = value & 0x10 != 0;
                    op.multiplier = MULT[(value & 15) as usize];
                }
                0x40 => {
                    op.key_scale_shift = [8, 1, 2, 0][(value >> 6) as usize];
                    op.output_level = (value & 63) as u16 * 4;
                }
                0x60 => { op.attack_rate = (value >> 4) as u16; op.decay_rate = (value & 15) as u16; }
                0x80 => {
                    let level = value >> 4;
                    op.sustain_level = if level == 15 { 31 * 16 } else { level as i16 * 16 };
                    op.release_rate = (value & 15) as u16;
                }
                0xE0 => op.waveform = value & 7,
                _ => {}
            }
            self.update_operator(i);
        }
    }

    fn channel_output(&mut self, i: usize) -> [i16; 2] {
        let ch = &self.channels[i];
        if !ch.enabled { return [0; 2]; }
        let mut vibrato = ((ch.frequency >> 7) & 7) as i16;
        if !self.vibrato_depth { vibrato >>= 1; }
        if self.vibrato_clock & 3 == 0 { vibrato = 0; }
        else {
            if self.vibrato_clock & 1 != 0 { vibrato >>= 1; }
            vibrato <<= ch.octave;
            if self.vibrato_clock & 4 != 0 { vibrato = -vibrato; }
        }
        let first = CHANNEL_OP[i];
        let mut op = |n: usize, modulation: i16, feedback: u16| {
            self.operators[first + n * 3].output(ch.phase_step, vibrato, modulation, feedback, self.clock, self.tremolo_level)
        };
        let mut out = op(0, 0, ch.feedback);
        if ch.pair {
            let second_additive = self.channels[i + 3].additive;
            match (ch.additive, second_additive) {
                (false, false) => { out = op(1, out, 0); out = op(2, out, 0); out = op(3, out, 0); }
                (true, false) => { let a = op(1, 0, 0); let a = op(2, a, 0); out = out.wrapping_add(op(3, a, 0)); }
                (false, true) => { out = op(1, out, 0); let a = op(2, 0, 0); out = out.wrapping_add(op(3, a, 0)); }
                (true, true) => { let a = op(1, 0, 0); out = out.wrapping_add(op(2, a, 0)); out = out.wrapping_add(op(3, 0, 0)); }
            }
        } else if ch.additive { out = out.wrapping_add(op(1, 0, 0)); }
        else { out = op(1, out, 0); }
        [if ch.left { out } else { 0 }, if ch.right { out } else { 0 }]
    }

    fn output(&mut self) -> [i16; 2] {
        let mut sums = [0i32; 2];
        for i in 0..18 {
            let value = self.channel_output(i);
            sums[0] += value[0] as i32;
            sums[1] += value[1] as i32;
        }
        self.clock = self.clock.wrapping_add(1);
        self.tremolo_clock = (self.tremolo_clock + 1) % 13440;
        self.tremolo_level = self.tremolo_clock.min(13440 - self.tremolo_clock) / 256;
        if !self.tremolo_depth { self.tremolo_level >>= 2; }
        self.vibrato_tick += 1;
        if self.vibrato_tick >= 1024 {
            self.vibrato_tick = 0;
            self.vibrato_clock = (self.vibrato_clock + 1) & 7;
        }
        [sums[0].clamp(-32768, 32767) as i16, sums[1].clamp(-32768, 32767) as i16]
    }

    pub fn sample(&mut self) -> [i16; 2] {
        while self.sample_accum >= self.sample_rate {
            self.previous = self.current;
            self.current = self.output();
            self.sample_accum -= self.sample_rate;
        }
        let fraction = ((self.sample_accum as i64 * 65536 + self.sample_rate as i64 / 2) / self.sample_rate as i64) as i32;
        let mut out = [0; 2];
        for (i, v) in out.iter_mut().enumerate() {
            let delta = self.current[i] as i32 - self.previous[i] as i32;
            *v = (self.previous[i] as i32 + fraction.wrapping_mul(delta) / 65536) as i16;
        }
        self.sample_accum += CHIP_RATE;
        out
    }
}

const EXP: [u16; 256] = [
    1018, 1013, 1007, 1002, 996, 991, 986, 980, 975, 969, 964, 959, 953, 948, 942, 937,
    932, 927, 921, 916, 911, 906, 900, 895, 890, 885, 880, 874, 869, 864, 859, 854,
    849, 844, 839, 834, 829, 824, 819, 814, 809, 804, 799, 794, 789, 784, 779, 774,
    770, 765, 760, 755, 750, 745, 741, 736, 731, 726, 722, 717, 712, 708, 703, 698,
    693, 689, 684, 680, 675, 670, 666, 661, 657, 652, 648, 643, 639, 634, 630, 625,
    621, 616, 612, 607, 603, 599, 594, 590, 585, 581, 577, 572, 568, 564, 560, 555,
    551, 547, 542, 538, 534, 530, 526, 521, 517, 513, 509, 505, 501, 496, 492, 488,
    484, 480, 476, 472, 468, 464, 460, 456, 452, 448, 444, 440, 436, 432, 428, 424,
    420, 416, 412, 409, 405, 401, 397, 393, 389, 385, 382, 378, 374, 370, 367, 363,
    359, 355, 352, 348, 344, 340, 337, 333, 329, 326, 322, 318, 315, 311, 308, 304,
    300, 297, 293, 290, 286, 283, 279, 276, 272, 268, 265, 262, 258, 255, 251, 248,
    244, 241, 237, 234, 231, 227, 224, 220, 217, 214, 210, 207, 204, 200, 197, 194,
    190, 187, 184, 181, 177, 174, 171, 168, 164, 161, 158, 155, 152, 148, 145, 142,
    139, 136, 133, 130, 126, 123, 120, 117, 114, 111, 108, 105, 102, 99, 96, 93,
    90, 87, 84, 81, 78, 75, 72, 69, 66, 63, 60, 57, 54, 51, 48, 45,
    42, 40, 37, 34, 31, 28, 25, 22, 20, 17, 14, 11, 8, 6, 3, 0,
];

const LOG_SIN: [u16; 256] = [
    2137, 1731, 1543, 1419, 1326, 1252, 1190, 1137, 1091, 1050, 1013, 979, 949, 920, 894, 869,
    846, 825, 804, 785, 767, 749, 732, 717, 701, 687, 672, 659, 646, 633, 621, 609,
    598, 587, 576, 566, 556, 546, 536, 527, 518, 509, 501, 492, 484, 476, 468, 461,
    453, 446, 439, 432, 425, 418, 411, 405, 399, 392, 386, 380, 375, 369, 363, 358,
    352, 347, 341, 336, 331, 326, 321, 316, 311, 307, 302, 297, 293, 289, 284, 280,
    276, 271, 267, 263, 259, 255, 251, 248, 244, 240, 236, 233, 229, 226, 222, 219,
    215, 212, 209, 205, 202, 199, 196, 193, 190, 187, 184, 181, 178, 175, 172, 169,
    167, 164, 161, 159, 156, 153, 151, 148, 146, 143, 141, 138, 136, 134, 131, 129,
    127, 125, 122, 120, 118, 116, 114, 112, 110, 108, 106, 104, 102, 100, 98, 96,
    94, 92, 91, 89, 87, 85, 83, 82, 80, 78, 77, 75, 74, 72, 70, 69,
    67, 66, 64, 63, 62, 60, 59, 57, 56, 55, 53, 52, 51, 49, 48, 47,
    46, 45, 43, 42, 41, 40, 39, 38, 37, 36, 35, 34, 33, 32, 31, 30,
    29, 28, 27, 26, 25, 24, 23, 23, 22, 21, 20, 20, 19, 18, 17, 17,
    16, 15, 15, 14, 13, 13, 12, 12, 11, 10, 10, 9, 9, 8, 8, 7,
    7, 7, 6, 6, 5, 5, 5, 4, 4, 4, 3, 3, 3, 2, 2, 2,
    2, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0,
];

const KEY_SCALE: [u16; 128] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 8, 12, 16, 20, 24, 28, 32,
    0, 0, 0, 0, 0, 12, 20, 28, 32, 40, 44, 48, 52, 56, 60, 64,
    0, 0, 0, 20, 32, 44, 52, 60, 64, 72, 76, 80, 84, 88, 92, 96,
    0, 0, 32, 52, 64, 76, 84, 92, 96, 104, 108, 112, 116, 120, 124, 128,
    0, 32, 64, 84, 96, 108, 116, 124, 128, 136, 140, 144, 148, 152, 156, 160,
    0, 64, 96, 116, 128, 140, 148, 156, 160, 168, 172, 176, 180, 184, 188, 192,
    0, 96, 128, 148, 160, 172, 180, 188, 192, 200, 204, 208, 212, 216, 220, 224,
];
