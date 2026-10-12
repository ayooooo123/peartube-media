//! FluidSynth 2.6.1 voice allocation state and SoundFont parameter conversion.
use crate::{
    channel::Channel,
    conv,
    generator::{self, Gen},
    modulator::{self, Mod, Sources},
    rvoice::{self, Address, BUFSIZE, EnvData, Rvoice},
    sfont::{InstZone, NUM_MOD, PresetZone, Sample, SoundFont},
};

pub const POLYPHONY: usize = 256;
const MAX_COMMANDS: usize = 65_536;

#[derive(Clone, Copy)]
pub enum Command {
    Reset,
    Sample(usize),
    Mode(i32),
    Gain(f64),
    Scalar(usize, f64),
    Minimum(f64),
    Send(usize, f64),
    Address(Address, i32),
    Envelope(bool, usize, EnvData),
    Noteoff(u32),
    Off,
    Portamento(u32, f64),
    Retrigger,
    Start,
    EffectsReset,
}
impl Command {
    pub fn apply(self, r: &mut Rvoice) {
        match self {
            Self::Reset => r.reset(),
            Self::Sample(s) => r.set_sample(Some(s)),
            Self::Mode(m) => r.set_samplemode(m),
            Self::Gain(g) => r.set_synth_gain(g),
            Self::Minimum(a) => r.min_attenuation_cb = a,
            Self::Send(i, a) => r.set_send_amp(i, a),
            Self::Address(a, v) => r.set_address(a, v),
            Self::Envelope(vol, stage, data) => {
                let env = if vol { &mut r.volenv } else { &mut r.modenv };
                env.data[stage] = data;
            }
            Self::Noteoff(t) => r.noteoff(t),
            Self::Off => r.voiceoff(),
            Self::Portamento(n, offset) => r.set_portamento(n, offset),
            Self::Retrigger => r.retrigger(),
            Self::Scalar(id, v) => match id {
                generator::ATTENUATION => r.set_attenuation(v),
                generator::PITCH => r.pitch = v,
                generator::OVERRIDEROOTKEY => r.root_pitch_hz = v,
                generator::FILTERFC => r.filter.fres = v,
                generator::FILTERQ => r.filter.set_q(v),
                generator::MODLFOTOPITCH => r.modlfo_to_pitch = v,
                generator::MODLFOTOVOL => r.modlfo_to_vol = v,
                generator::MODLFOTOFILTERFC => r.modlfo_to_fc = v,
                generator::VIBLFOTOPITCH => r.viblfo_to_pitch = v,
                generator::MODENVTOPITCH => r.modenv_to_pitch = v,
                generator::MODENVTOFILTERFC => r.modenv_to_fc = v,
                generator::MODLFODELAY => r.modlfo.delay = v as u32,
                generator::MODLFOFREQ => r.modlfo.increment = v,
                generator::VIBLFODELAY => r.viblfo.delay = v as u32,
                generator::VIBLFOFREQ => r.viblfo.increment = v,
                _ => unreachable!("only converted DSP parameters are queued"),
            },
            Self::Start | Self::EffectsReset => unreachable!("mixer command"),
        }
    }
}

/// A bounded queue preserves FluidSynth's one-block DSP event delay.
#[derive(Default)]
pub struct Commands {
    pub entries: Vec<(usize, Command)>,
    pub exceeded: bool,
}
impl Commands {
    pub fn push(&mut self, target: usize, command: Command) {
        if self.entries.len() == MAX_COMMANDS {
            self.exceeded = true;
            return;
        }
        self.entries.push((target, command));
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Status {
    Off,
    On,
    Sustained,
    Sostenuto,
}
pub struct Voice {
    pub slot: usize,
    pub overflow: usize,
    pub status: Status,
    pub chan: usize,
    pub key: u8,
    pub vel: u8,
    pub id: u64,
    pub start_tick: u64,
    pub has_noteoff: bool,
    pub sample: usize,
    pub gens: [Gen; generator::LAST],
    mods: Vec<Mod>,
    pub attenuation: f64,
    root_key: f64,
    gain: f64,
}
impl Voice {
    pub fn new(index: usize) -> Self {
        Self {
            slot: index * 2,
            overflow: index * 2 + 1,
            status: Status::Off,
            chan: 16,
            key: 0,
            vel: 0,
            id: 0,
            start_tick: 0,
            has_noteoff: true,
            sample: 0,
            gens: generator::init(None),
            mods: Vec::with_capacity(NUM_MOD),
            attenuation: 0.0,
            root_key: 0.0,
            gain: f64::from(0.2f32),
        }
    }
    pub fn value(&self, id: usize) -> f64 {
        let g = self.gens[id];
        g.val + g.modv + g.nrpn
    }
    fn actual_key(&self) -> i32 {
        let x = self.value(generator::KEYNUM);
        if x >= 0.0 {
            x as i32
        } else {
            i32::from(self.key)
        }
    }
    fn actual_velocity(&self) -> i32 {
        let x = self.value(generator::VELOCITY);
        if x > 0.0 {
            x as i32
        } else {
            i32::from(self.vel)
        }
    }
    fn sources<'a>(&self, c: &'a Channel) -> Sources<'a> {
        Sources {
            cc: &c.cc,
            key_pressure: &c.key_pressure,
            channel_pressure: c.pressure,
            pitch_bend: c.bend,
            pitch_wheel_sensitivity: c.bend_range,
            modulation_depth_range: c.modulation_range,
            key: self.key,
            actual_key: self.actual_key(),
            actual_velocity: self.actual_velocity(),
        }
    }
    pub fn init(
        &mut self,
        chan: usize,
        key: u8,
        vel: u8,
        sample: usize,
        id: u64,
        ticks: u64,
        c: &Channel,
        font: &SoundFont,
        q: &mut Commands,
    ) {
        self.chan = chan;
        self.key = key;
        self.vel = vel;
        self.sample = sample;
        self.id = id;
        self.start_tick = ticks;
        self.has_noteoff = false;
        self.gens = generator::init(Some(&c.gens));
        self.mods.clear();
        for m in font.default_mods.as_deref().unwrap_or(&modulator::DEFAULTS) {
            if m.sources_valid() && self.mods.len() < NUM_MOD {
                self.mods.push(*m);
            }
        }
        q.push(self.slot, Command::Reset);
        q.push(self.slot, Command::Sample(sample));
        q.push(self.slot, Command::Mode(0));
        q.push(self.slot, Command::Gain(self.gain));
    }
    pub fn zones(
        &mut self,
        inst: &InstZone,
        global_inst: Option<&InstZone>,
        preset: &PresetZone,
        global_preset: Option<&PresetZone>,
        q: &mut Commands,
    ) {
        for i in 0..generator::LAST {
            let local = inst.gens[i];
            let global = global_inst.map_or((false, 0.0), |g| g.gens[i]);
            if local.0 || global.0 {
                self.gens[i].val = f64::from((if local.0 { local.1 } else { global.1 }) as f32);
                self.gens[i].set = true;
                if i == generator::SAMPLEMODE {
                    q.push(self.slot, Command::Mode(self.gens[i].val as i32));
                }
            }
        }
        self.add_mods(&inst.mods, global_inst.map_or(&[], |g| &g.mods), false);
        for i in 0..generator::LAST {
            let local = preset.gens[i];
            let global = global_preset.map_or((false, 0.0), |g| g.gens[i]);
            if local.0 || global.0 {
                self.gens[i].val += f64::from((if local.0 { local.1 } else { global.1 }) as f32);
                self.gens[i].set = true;
            }
        }
        self.add_mods(&preset.mods, global_preset.map_or(&[], |g| &g.mods), true);
    }
    fn add_mods(&mut self, local: &[Mod], global: &[Mod], add: bool) {
        let limit = self.mods.len();
        for m in local
            .iter()
            .chain(
                global
                    .iter()
                    .filter(|m| !local.iter().any(|l| l.same_as(m))),
            )
            .take(NUM_MOD)
        {
            if (add && m.amount == 0.0) || !m.sources_valid() {
                continue;
            }
            if let Some(existing) = self.mods[..limit].iter_mut().find(|x| x.same_as(m)) {
                if add {
                    existing.amount += m.amount;
                } else {
                    existing.amount = m.amount;
                }
            } else if self.mods.len() < NUM_MOD {
                self.mods.push(*m);
            }
        }
    }
    pub fn start(&mut self, c: &Channel, sample: &Sample, rate: f64, q: &mut Commands) {
        for i in 0..self.mods.len() {
            let m = self.mods[i];
            self.gens[usize::from(m.dest)].modv += modulator::value(&m, &self.sources(c));
        }
        const INIT: &[usize] = &[
            0, 1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 13, 15, 16, 17, 21, 22, 23, 24, 25, 26, 27, 28, 30,
            33, 34, 35, 36, 38, 46, 47, 48, 58, 59, 60,
        ];
        for &i in INIT {
            self.update(i, sample, rate, q);
        }
        let mut reduction = 0.0;
        for m in &self.mods {
            if usize::from(m.dest) != generator::ATTENUATION {
                continue;
            }
            if m.flags1 & modulator::CC == 0
                && m.flags2 & modulator::CC == 0
                && !matches!(m.src1, 10 | 13 | 14)
                && !matches!(m.src2, 10 | 13 | 14)
            {
                continue;
            }
            let current = modulator::value(m, &self.sources(c));
            let minimum = if (m.flags1 | m.flags2) & modulator::BIPOLAR != 0 || m.amount < 0.0 {
                -m.amount.abs()
            } else {
                0.0
            };
            if current > minimum {
                reduction += current - minimum;
            }
        }
        q.push(
            self.slot,
            Command::Minimum((self.attenuation - reduction).max(0.0)),
        );
        self.status = Status::On;
        q.push(self.slot, Command::Start);
    }
    pub fn modulate(
        &mut self,
        c: &Channel,
        cc: bool,
        ctrl: Option<u8>,
        sample: &Sample,
        rate: f64,
        q: &mut Commands,
    ) {
        let mut updated = 0u64;
        for i in 0..self.mods.len() {
            let m = self.mods[i];
            let dest = usize::from(m.dest);
            if ctrl.is_none_or(|n| m.has_source(cc, n)) && updated & (1u64 << dest) == 0 {
                let value = self
                    .mods
                    .iter()
                    .filter(|m| usize::from(m.dest) == dest)
                    .map(|m| modulator::value(m, &self.sources(c)))
                    .sum();
                self.gens[dest].modv = value;
                self.update(dest, sample, rate, q);
                updated |= 1u64 << dest;
            }
        }
    }
    pub fn release(&mut self, rate: f64, q: &mut Commands) {
        q.push(self.slot, Command::Noteoff((rate * 10.0 / 1000.0) as u32));
        self.has_noteoff = true;
    }
    pub fn noteoff(&mut self, c: &Channel, rate: f64, q: &mut Commands) {
        if c.cc[66] >= 64 && c.sostenuto_id > self.id {
            self.status = Status::Sostenuto;
        } else if c.cc[64] >= 64 {
            self.status = Status::Sustained;
        } else {
            self.release(rate, q);
        }
    }
    pub fn kill_exclusive(&mut self, sample: &Sample, rate: f64, q: &mut Commands) {
        self.gens[generator::EXCLUSIVECLASS].val = 0.0;
        self.gens[generator::VOLENVRELEASE].val = -2000.0;
        self.update(generator::VOLENVRELEASE, sample, rate, q);
        q.push(self.slot, Command::Noteoff((rate * 10.0 / 1000.0) as u32));
    }
    pub fn priority(&self, drum: bool, ticks: u64, rate: f64) -> f32 {
        let mut priority = if drum {
            4000.0f32
        } else if self.has_noteoff {
            -2000.0
        } else if self.status != Status::On {
            -1000.0
        } else {
            0.0
        };
        priority = (f64::from(priority)
            + 1000.0 * rate / ticks.saturating_sub(self.start_tick).max(1) as f64)
            as f32;
        (f64::from(priority) + 500.0 / self.attenuation.max(f64::from(0.1f32))) as f32
    }
    pub fn pitch(&self, key: i32) -> f64 {
        self.gens[generator::SCALETUNE].val * (f64::from(key) - self.root_key / 100.0)
            + self.root_key
    }
    pub fn retune(
        &mut self,
        key: u8,
        vel: u8,
        c: &Channel,
        sample: &Sample,
        rate: f64,
        q: &mut Commands,
    ) {
        self.key = key;
        self.vel = vel;
        self.modulate(c, false, Some(modulator::SRC_VELOCITY), sample, rate, q);
        for id in [
            generator::KEYTOMODENVHOLD,
            generator::KEYTOMODENVDECAY,
            generator::KEYTOVOLENVHOLD,
            generator::KEYTOVOLENVDECAY,
        ] {
            self.update(id, sample, rate, q);
        }
        self.gens[generator::PITCH].val = self.pitch(self.actual_key());
        self.update(generator::PITCH, sample, rate, q);
        self.has_noteoff = false;
        self.status = Status::On;
        q.push(self.slot, Command::Retrigger);
    }
    fn hold_decay(&self, base: usize, key: usize, decay: bool, rate: f64) -> u32 {
        let tc = self
            .value(key)
            .mul_add(60.0 - f64::from(self.actual_key()), self.value(base));
        if !decay && tc <= -32768.0 {
            return 0;
        }
        (rate * conv::tc2sec(tc.clamp(-12000.0, if decay { 8000.0 } else { 5000.0 }))
            / BUFSIZE as f64
            + 0.5) as u32
    }
    pub fn update(&mut self, id: usize, s: &Sample, rate: f64, q: &mut Commands) {
        use generator::*;
        let x = self.value(id);
        let scalar = |q: &mut Commands, id, x| q.push(self.slot, Command::Scalar(id, x));
        match id {
            PAN | CUSTOM_BALANCE => {
                for (i, left) in [true, false].into_iter().enumerate() {
                    let amp = conv::pan(self.value(PAN), left)
                        * conv::balance(self.value(CUSTOM_BALANCE), left);
                    q.push(self.slot, Command::Send(i, amp * self.gain / 8388607.0));
                }
            }
            ATTENUATION => {
                self.attenuation = x.clamp(0.0, 1440.0);
                scalar(q, id, self.attenuation);
            }
            PITCH | COARSETUNE | FINETUNE => scalar(
                q,
                PITCH,
                100.0f64.mul_add(self.value(COARSETUNE), self.value(PITCH)) + self.value(FINETUNE),
            ),
            REVERBSEND | CHORUSSEND => q.push(
                self.slot,
                Command::Send(
                    if id == REVERBSEND { 2 } else { 3 },
                    (x / 1000.0).clamp(0.0, 1.0) * self.gain / 8388607.0,
                ),
            ),
            OVERRIDEROOTKEY => {
                self.root_key = if self.gens[id].val > -1.0 {
                    self.gens[id].val * 100.0
                } else {
                    f64::from(s.origpitch) * 100.0
                };
                let hz = conv::ct2hz_real(self.root_key - f64::from(s.pitchadj))
                    * (rate / f64::from(s.samplerate));
                self.gens[PITCH].val = self.pitch(self.actual_key());
                q.push(self.slot, Command::Scalar(id, hz));
            }
            FILTERFC => scalar(q, id, x.clamp(1500.0, 13500.0)),
            FILTERQ => scalar(q, id, x.clamp(0.0, 960.0)),
            MODLFOTOPITCH | VIBLFOTOPITCH | MODENVTOPITCH | MODLFOTOFILTERFC | MODENVTOFILTERFC => {
                scalar(q, id, x.clamp(-12000.0, 12000.0))
            }
            MODLFOTOVOL => scalar(q, id, x.clamp(-960.0, 960.0)),
            MODLFODELAY | VIBLFODELAY => scalar(
                q,
                id,
                (rate * conv::tc2sec_delay(x.clamp(-12000.0, 5000.0))) as u32 as f64,
            ),
            MODLFOFREQ | VIBLFOFREQ => scalar(
                q,
                id,
                4.0 * BUFSIZE as f64 * conv::ct2hz_real(x.clamp(-16000.0, 4500.0)) / rate,
            ),
            STARTADDROFS
            | STARTADDRCOARSEOFS
            | ENDADDROFS
            | ENDADDRCOARSEOFS
            | STARTLOOPADDROFS
            | STARTLOOPADDRCOARSEOFS
            | ENDLOOPADDROFS
            | ENDLOOPADDRCOARSEOFS => {
                let (base, fine, coarse, which) = match id {
                    STARTADDROFS | STARTADDRCOARSEOFS => {
                        (s.start, STARTADDROFS, STARTADDRCOARSEOFS, Address::Start)
                    }
                    ENDADDROFS | ENDADDRCOARSEOFS => {
                        (s.end, ENDADDROFS, ENDADDRCOARSEOFS, Address::End)
                    }
                    STARTLOOPADDROFS | STARTLOOPADDRCOARSEOFS => (
                        s.loopstart,
                        STARTLOOPADDROFS,
                        STARTLOOPADDRCOARSEOFS,
                        Address::LoopStart,
                    ),
                    _ => (
                        s.loopend,
                        ENDLOOPADDROFS,
                        ENDLOOPADDRCOARSEOFS,
                        Address::LoopEnd,
                    ),
                };
                let pos = i64::from(base)
                    .saturating_add(self.value(fine) as i64)
                    .saturating_add((self.value(coarse) as i64).saturating_mul(32768));
                q.push(
                    self.slot,
                    Command::Address(
                        which,
                        pos.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
                    ),
                );
            }
            MODENVDELAY..=KEYTOMODENVDECAY | VOLENVDELAY..=KEYTOVOLENVDECAY => {
                let vol = id >= VOLENVDELAY;
                let base = if vol { VOLENVDELAY } else { MODENVDELAY };
                let (stage, count, coeff, increment, min, max) = match id {
                    MODENVDELAY | VOLENVDELAY => (
                        rvoice::ENV_DELAY,
                        (rate * conv::tc2sec_delay(x.clamp(-12000.0, 5000.0)) / BUFSIZE as f64)
                            as u32,
                        0.0,
                        0.0,
                        -1.0,
                        1.0,
                    ),
                    MODENVATTACK | VOLENVATTACK => {
                        let n = 1
                            + (rate * conv::tc2sec_attack(x.clamp(-12000.0, 8000.0))
                                / BUFSIZE as f64) as u32;
                        (
                            rvoice::ENV_ATTACK,
                            n,
                            1.0,
                            f64::from(1.0f32 / n as f32),
                            -1.0,
                            1.0,
                        )
                    }
                    MODENVHOLD | KEYTOMODENVHOLD | VOLENVHOLD | KEYTOVOLENVHOLD => (
                        rvoice::ENV_HOLD,
                        self.hold_decay(base + 2, base + 6, false, rate),
                        1.0,
                        0.0,
                        -1.0,
                        2.0,
                    ),
                    MODENVDECAY | MODENVSUSTAIN | KEYTOMODENVDECAY | VOLENVDECAY
                    | VOLENVSUSTAIN | KEYTOVOLENVDECAY => {
                        let n = self.hold_decay(base + 3, base + 7, true, rate);
                        let sustain =
                            (1.0 - f64::from(0.001f32) * self.value(base + 4)).clamp(0.0, 1.0);
                        (
                            rvoice::ENV_DECAY,
                            n,
                            1.0,
                            if n == 0 {
                                0.0
                            } else {
                                f64::from(-1.0f32 / n as f32)
                            },
                            sustain,
                            2.0,
                        )
                    }
                    MODENVRELEASE | VOLENVRELEASE => {
                        let n = 1
                            + (rate
                                * conv::tc2sec_attack(
                                    x.clamp(if vol { -7200.0 } else { -12000.0 }, 8000.0),
                                )
                                / BUFSIZE as f64) as u32;
                        (
                            rvoice::ENV_RELEASE,
                            n,
                            1.0,
                            f64::from(-1.0f32 / n as f32),
                            0.0,
                            if vol { 1.0 } else { 2.0 },
                        )
                    }
                    _ => unreachable!("only envelope generators reach this branch"),
                };
                q.push(
                    self.slot,
                    Command::Envelope(
                        vol,
                        stage as usize,
                        EnvData {
                            count,
                            coeff,
                            increment,
                            min,
                            max,
                        },
                    ),
                );
            }
            _ => {}
        }
    }
}
