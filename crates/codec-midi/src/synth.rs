//! Single-threaded FluidSynth 2.6.1 synthesis: MIDI controls, voice stealing,
//! deferred DSP commands and the four-bus stereo/effects mixer.
use crate::{
    channel::{BankStyle, Channel},
    conv,
    effects::{Chorus, Reverb},
    generator, modulator,
    rvoice::{self, BUFSIZE, Rvoice},
    sfont::SoundFont,
    smf::Kind,
    voice::{Command, Commands, POLYPHONY, Status, Voice},
};
use std::sync::Arc;

pub struct Synth {
    font: Arc<SoundFont>,
    pub rate: f64,
    pub ticks: u64,
    channels: [Channel; 16],
    voices: Vec<Voice>,
    render: Vec<Rvoice>,
    busy: [bool; POLYPHONY * 2],
    active: Vec<usize>,
    finished: Vec<usize>,
    selected: Vec<(usize, usize)>,
    queue: Commands,
    sincos: Vec<(f32, f32)>,
    min_fres: f64,
    reverb: Reverb,
    chorus: Chorus,
    style: BankStyle,
    noteid: u64,
    portamento_lsb: bool,
    pub failed: bool,
}
impl Synth {
    pub fn new(font: Arc<SoundFont>, rate: f64) -> Self {
        Self {
            channels: std::array::from_fn(|i| {
                let mut c = Channel::new(i, &font);
                c.program_change(0, &font);
                c
            }),
            font,
            rate,
            ticks: 0,
            voices: (0..POLYPHONY).map(Voice::new).collect(),
            render: (0..POLYPHONY * 2)
                .map(|_| {
                    let mut r = Rvoice::new(rate);
                    for i in 0..4 {
                        r.set_send_mapping(i, i as i32);
                    }
                    r
                })
                .collect(),
            busy: [false; POLYPHONY * 2],
            active: Vec::with_capacity(POLYPHONY * 2),
            finished: Vec::with_capacity(POLYPHONY * 2),
            selected: Vec::with_capacity(POLYPHONY),
            queue: Commands::default(),
            sincos: rvoice::sincos_table(rate),
            min_fres: conv::hz2ct(5.0),
            reverb: Reverb::new(rate),
            chorus: Chorus::new(rate),
            style: BankStyle::Gs,
            noteid: 0,
            portamento_lsb: false,
            failed: false,
        }
    }
    fn collect_finished(&mut self) {
        for slot in self.finished.drain(..) {
            self.busy[slot] = false;
            let voice = &mut self.voices[slot / 2];
            if voice.slot == slot {
                voice.status = Status::Off;
                voice.chan = 16;
                voice.has_noteoff = true;
            }
        }
    }
    pub fn active_count(&mut self) -> usize {
        self.collect_finished();
        self.busy.iter().filter(|&&v| v).count()
    }
    pub fn begin_block(&mut self) {
        self.failed |= self.queue.exceeded;
        for (slot, command) in self.queue.entries.drain(..) {
            match command {
                Command::Start => self.active.push(slot),
                Command::EffectsReset => {
                    self.reverb.reset();
                    self.chorus.reset();
                }
                c => c.apply(&mut self.render[slot]),
            }
        }
    }
    pub fn render_block(&mut self, output: &mut [f32; BUFSIZE * 2]) {
        self.ticks += BUFSIZE as u64;
        let mut buses = [[0.0; BUFSIZE]; 4];
        let mut buf = [0.0; BUFSIZE];
        // Retire only after every voice has mixed, preserving summation order.
        for &slot in &self.active {
            let voice = &mut self.render[slot];
            let n = voice.write(&self.font, &self.sincos, self.min_fres, &mut buf);
            if n < 0 {
                continue;
            }
            voice.mix(&buf, n as usize, &mut buses);
            if n < BUFSIZE as i32 {
                self.finished.push(slot);
            }
        }
        for &slot in &self.finished {
            if let Some(i) = self.active.iter().position(|&s| s == slot) {
                self.active.swap_remove(i);
            }
        }
        let [mut left, mut right, rev, cho] = buses;
        self.reverb.process(&rev, &mut left, &mut right);
        self.chorus.process(&cho, &mut left, &mut right);
        for i in 0..BUFSIZE {
            output[2 * i] = left[i] as f32;
            output[2 * i + 1] = right[i] as f32;
        }
        self.failed |= self.queue.exceeded;
    }
    fn allocate(&mut self) -> Option<usize> {
        if let Some(i) = self
            .voices
            .iter()
            .position(|v| v.status == Status::Off && !self.busy[v.slot])
        {
            return Some(i);
        }
        let mut best = None;
        let mut score = 999998.0;
        for (i, voice) in self.voices.iter().enumerate() {
            if self.busy[voice.overflow] {
                continue;
            }
            let priority = voice.priority(self.channels[voice.chan].drum, self.ticks, self.rate);
            if priority < score {
                score = priority;
                best = Some(i);
            }
        }
        if let Some(i) = best {
            let voice = &mut self.voices[i];
            self.queue.push(voice.slot, Command::Off);
            std::mem::swap(&mut voice.slot, &mut voice.overflow);
        }
        best
    }
    fn portamento(&mut self, i: usize, from: Option<u8>, key: u8) {
        if let Some(from) = from {
            let v = &self.voices[i];
            let ms = self.channels[v.chan].portamento_ms(from, key, self.portamento_lsb);
            let count =
                (self.rate * f64::from(0.001f32) * f64::from(ms) / BUFSIZE as f64 + 0.5) as u32;
            self.queue.push(
                v.slot,
                Command::Portamento(count, v.pitch(i32::from(from)) - v.pitch(i32::from(key))),
            );
        }
    }
    fn noteon(&mut self, chan: usize, key: u8, vel: u8, remember: bool) {
        if vel == 0 {
            self.noteoff(chan, key);
            return;
        }
        let Some(preset) = self.channels[chan].preset else {
            return;
        };
        let mono = self.channels[chan].mono || self.channels[chan].cc[68] >= 64;
        let previous = self.channels[chan].last_held();
        let from = if self.channels[chan].cc[84] < 128 {
            Some(self.channels[chan].cc[84])
        } else if self.channels[chan].cc[65] >= 64 {
            self.channels[chan].previous_note
        } else {
            None
        };
        self.channels[chan].cc[84] = modulator::INVALID_NOTE;
        self.channels[chan].previous_note = Some(key);
        if remember {
            self.channels[chan].hold(key, vel);
        }
        let mut id = self.noteid;
        self.noteid += 1;
        for voice in &mut self.voices {
            if voice.chan == chan && voice.key == key && voice.status != Status::Off {
                if voice.status == Status::Sostenuto {
                    id = voice.id;
                }
                voice.noteoff(&self.channels[chan], self.rate, &mut self.queue);
            }
        }
        self.selected.clear();
        let p = &self.font.presets[preset];
        for (pi, z) in p.zones.iter().enumerate() {
            for vz in &z.voice_zones {
                if vz.range.inside(i32::from(key), i32::from(vel)) {
                    if self.selected.len() == POLYPHONY {
                        self.failed = true;
                        return;
                    }
                    self.selected.push((pi, vz.inst_zone));
                }
            }
        }
        if mono && previous.is_some() {
            for voice in &mut self.voices {
                if voice.chan == chan && voice.status != Status::Off && !voice.has_noteoff {
                    let s = &self.font.samples[voice.sample];
                    voice.retune(
                        key,
                        vel,
                        &self.channels[chan],
                        s,
                        self.rate,
                        &mut self.queue,
                    );
                }
            }
            let mut reused = false;
            for i in 0..self.voices.len() {
                if self.voices[i].chan == chan && !self.voices[i].has_noteoff {
                    self.portamento(i, from, key);
                    reused = true;
                }
            }
            if reused {
                return;
            }
        } else if mono {
            for v in &mut self.voices {
                if v.chan == chan && !v.has_noteoff {
                    v.release(self.rate, &mut self.queue);
                }
            }
        }
        for z in 0..self.selected.len() {
            let Some(i) = self.allocate() else {
                break;
            };
            let (pi, ii) = self.selected[z];
            let p = &self.font.presets[preset];
            let pz = &p.zones[pi];
            let inst = &self.font.insts[pz.inst.expect("voice zone has an instrument")];
            let iz = &inst.zones[ii];
            let sample = iz.sample.expect("voice zone has a sample");
            let voice = &mut self.voices[i];
            voice.init(
                chan,
                key,
                vel,
                sample,
                id,
                self.ticks,
                &self.channels[chan],
                &self.font,
                &mut self.queue,
            );
            voice.zones(
                iz,
                inst.global.as_ref(),
                pz,
                p.global.as_ref(),
                &mut self.queue,
            );
            let exclusive = voice.value(generator::EXCLUSIVECLASS) as i32;
            if exclusive != 0 {
                for j in 0..self.voices.len() {
                    let v = &mut self.voices[j];
                    if j != i
                        && v.chan == chan
                        && v.status != Status::Off
                        && v.id != id
                        && v.value(generator::EXCLUSIVECLASS) == f64::from(exclusive)
                    {
                        v.kill_exclusive(&self.font.samples[v.sample], self.rate, &mut self.queue);
                    }
                }
            }
            let v = &mut self.voices[i];
            v.start(
                &self.channels[chan],
                &self.font.samples[sample],
                self.rate,
                &mut self.queue,
            );
            self.busy[v.slot] = true;
            self.portamento(i, from, key);
        }
    }
    fn noteoff(&mut self, chan: usize, key: u8) {
        let previous = self.channels[chan].last_held();
        self.channels[chan].unhold(key);
        if (self.channels[chan].mono || self.channels[chan].cc[68] >= 64)
            && previous.is_some_and(|(k, _)| k == key)
        {
            if let Some((next, vel)) = self.channels[chan].last_held() {
                self.noteon(chan, next, vel, false);
                return;
            }
        }
        for v in &mut self.voices {
            if v.chan == chan && v.key == key && v.status == Status::On && !v.has_noteoff {
                v.noteoff(&self.channels[chan], self.rate, &mut self.queue);
            }
        }
    }
    fn modulate(&mut self, chan: usize, cc: bool, ctrl: Option<u8>) {
        for v in &mut self.voices {
            if v.chan == chan {
                v.modulate(
                    &self.channels[chan],
                    cc,
                    ctrl,
                    &self.font.samples[v.sample],
                    self.rate,
                    &mut self.queue,
                );
            }
        }
    }
    fn generator(&mut self, chan: usize, id: usize, value: f64) {
        self.channels[chan].gens[id] = value;
        for v in &mut self.voices {
            if v.chan == chan {
                v.gens[id].nrpn = value;
                v.update(id, &self.font.samples[v.sample], self.rate, &mut self.queue);
            }
        }
    }
    fn damp(&mut self, chan: usize, status: Status) {
        for v in &mut self.voices {
            if v.chan == chan && v.status == status {
                v.release(self.rate, &mut self.queue);
            }
        }
    }
    pub fn control(&mut self, chan: usize, num: u8, value: u8) {
        self.collect_finished();
        self.channels[chan].cc[usize::from(num)] = value;
        match num {
            0 => self.channels[chan].bank_msb(value, self.style),
            32 => self.channels[chan].bank_lsb(value, self.style),
            64 => {
                if value < 64 {
                    self.damp(chan, Status::Sustained);
                }
            }
            66 => {
                if value < 64 {
                    self.damp(chan, Status::Sostenuto);
                } else {
                    self.channels[chan].sostenuto_id = self.noteid;
                }
            }
            65 => {
                if self.channels[chan].held_count == 0 {
                    self.channels[chan].previous_note = None;
                }
            }
            68 => {
                if value < 64 {
                    let last = self.channels[chan].last_held();
                    self.channels[chan].held_count = 0;
                    if let Some((key, vel)) = last {
                        self.channels[chan].hold(key, vel);
                    }
                }
            }
            120 => {
                for v in &self.voices {
                    if v.chan == chan {
                        self.queue.push(v.slot, Command::Off);
                    }
                }
            }
            123 => {
                self.channels[chan].held_count = 0;
                for v in &mut self.voices {
                    if v.chan == chan && v.status == Status::On && !v.has_noteoff {
                        v.noteoff(&self.channels[chan], self.rate, &mut self.queue);
                    }
                }
            }
            121 => {
                self.channels[chan].reset_controllers(true);
                self.damp(chan, Status::Sustained);
                self.damp(chan, Status::Sostenuto);
                self.modulate(chan, false, None);
            }
            122 => {}
            124..=127 => {
                // The default basic-channel group is all 16 channels, owned by channel zero.
                if chan == 0 {
                    for c in 0..16 {
                        self.control(c, 123, 0);
                        if num >= 126 {
                            self.channels[c].mono = num == 126;
                        }
                    }
                }
            }
            99 => {
                let c = &mut self.channels[chan];
                c.cc[98] = 0;
                c.cc[6] = 0;
                c.cc[38] = 0;
                c.nrpn = true;
                c.nrpn_select = 0;
            }
            98 => {
                let c = &mut self.channels[chan];
                c.cc[6] = 0;
                c.cc[38] = 0;
                if c.cc[99] == 120 {
                    c.nrpn_select = c.nrpn_select.saturating_add(match value {
                        100 => 100,
                        101 => 1000,
                        102 => 10000,
                        0..=99 => usize::from(value),
                        _ => 0,
                    });
                }
                c.nrpn = true;
            }
            100 | 101 => {
                let c = &mut self.channels[chan];
                c.nrpn = false;
                c.cc[6] = 0;
                c.cc[38] = 0;
            }
            6 | 38 => {
                let c = &self.channels[chan];
                let msb = c.cc[6];
                let lsb = c.cc[38];
                let data = i32::from(msb) * 128 + i32::from(lsb);
                if c.nrpn {
                    if c.cc[99] == 120 && c.cc[98] < 100 && num == 6 {
                        let id = c.nrpn_select;
                        if id < generator::LAST {
                            self.generator(chan, id, generator::scale_nrpn(id, data));
                        }
                        self.channels[chan].nrpn_select = 0;
                    } else if c.cc[99] == 1 && self.style == BankStyle::Gs {
                        let target = match c.cc[98] {
                            8 => Some(76),
                            9 => Some(77),
                            10 => Some(78),
                            _ => None,
                        };
                        if let Some(n) = target {
                            self.control(chan, n, msb);
                        }
                    }
                } else if c.cc[101] == 0 {
                    match c.cc[100] {
                        0 => {
                            self.channels[chan].bend_range =
                                f32::from(msb) + f32::from(lsb) / 100.0;
                            self.modulate(chan, false, Some(modulator::SRC_PITCHWHEELSENS));
                        }
                        1 => self.generator(
                            chan,
                            generator::FINETUNE,
                            f64::from((data - 8192) as f32 * (100.0f32 / 8192.0)),
                        ),
                        2 => self.generator(
                            chan,
                            generator::COARSETUNE,
                            f64::from(i32::from(msb) - 64),
                        ),
                        5 if !c.drum => {
                            self.channels[chan].modulation_range =
                                f32::from(msb) * 100.0 + f32::from(lsb) * 100.0 / 128.0;
                            self.modulate(chan, true, Some(1));
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                if num == 37 {
                    self.portamento_lsb = true;
                }
                self.modulate(chan, true, Some(num));
            }
        }
    }
    pub fn reset(&mut self) {
        self.collect_finished();
        for v in &self.voices {
            if v.status != Status::Off {
                self.queue.push(v.slot, Command::Off);
            }
        }
        for (i, c) in self.channels.iter_mut().enumerate() {
            *c = Channel::new(i, &self.font);
        }
        self.queue.push(0, Command::EffectsReset);
        self.portamento_lsb = false;
    }
    fn sysex(&mut self, data: &[u8]) {
        if data.len() < 4 || !matches!(data[1], 16 | 127) {
            return;
        }
        if data[0] == 0x7e && data[2] == 9 && matches!(data[3], 1 | 3) {
            self.style = if data[3] == 3 {
                BankStyle::Gm2
            } else {
                BankStyle::Gm
            };
            self.reset();
        } else if data[0] == 0x43
            && data[2] == 0x4c
            && data.len() == 7
            && data[3..5] == [0, 0]
            && matches!(data[5], 0x7e | 0x7f)
            && data[6] == 0
        {
            self.style = BankStyle::Xg;
            self.reset();
        } else if data[0] == 0x41 && data[2..4] == [0x42, 0x12] && data.len() == 9 {
            if data[4..].iter().map(|&v| u32::from(v)).sum::<u32>() & 127 != 0 {
                return;
            }
            let address =
                (u32::from(data[4]) << 16) | (u32::from(data[5]) << 8) | u32::from(data[6]);
            if address == 0x40007f && matches!(data[7], 0 | 127) {
                self.style = if data[7] == 0 {
                    BankStyle::Gs
                } else {
                    BankStyle::Gm
                };
                self.reset();
            } else if self.style == BankStyle::Gs && address & 0xfff0ff == 0x401015 && data[7] <= 2
            {
                let part = usize::from(data[5] & 15);
                let chan = if part == 0 {
                    9
                } else if part >= 10 {
                    part
                } else {
                    part - 1
                };
                self.channels[chan].drum = data[7] != 0;
                self.control(chan, 121, 0);
                self.channels[chan].bank = if data[7] == 0 { 0 } else { 128 };
                self.channels[chan].program_change(0, &self.font);
            }
        }
    }
    pub fn event(&mut self, event: &Kind) {
        self.collect_finished();
        match *event {
            Kind::NoteOn { chan, key, vel } => self.noteon(usize::from(chan), key, vel, true),
            Kind::NoteOff { chan, key, .. } => self.noteoff(usize::from(chan), key),
            Kind::Control { chan, num, value } => self.control(usize::from(chan), num, value),
            Kind::Program { chan, program } => {
                self.channels[usize::from(chan)].program_change(program, &self.font)
            }
            Kind::ChannelPressure { chan, value } => {
                self.channels[usize::from(chan)].pressure = value;
                self.modulate(
                    usize::from(chan),
                    false,
                    Some(modulator::SRC_CHANNELPRESSURE),
                );
            }
            Kind::KeyPressure { chan, key, value } => {
                let c = usize::from(chan);
                self.channels[c].key_pressure[usize::from(key)] = value;
                for v in &mut self.voices {
                    if v.chan == c && v.key == key {
                        v.modulate(
                            &self.channels[c],
                            false,
                            Some(modulator::SRC_KEYPRESSURE),
                            &self.font.samples[v.sample],
                            self.rate,
                            &mut self.queue,
                        );
                    }
                }
            }
            Kind::PitchBend { chan, value } => {
                self.channels[usize::from(chan)].bend = value as i16;
                self.modulate(usize::from(chan), false, Some(modulator::SRC_PITCHWHEEL));
            }
            Kind::Sysex(ref data) => self.sysex(data),
            Kind::Tempo(_) | Kind::End => {}
        }
        self.failed |= self.queue.exceeded;
    }
}
