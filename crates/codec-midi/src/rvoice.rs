//! The rendering half of a voice: FluidSynth 2.6.1 `rvoice/fluid_rvoice.c`,
//! `fluid_adsr_env.[ch]`, `fluid_lfo.[ch]`, `fluid_phase.h`,
//! `fluid_iir_filter*.c*` and the 4th-order path of `fluid_rvoice_dsp.cpp`.
//! FluidSynth's `fluid_real_t` is `double` here (its default build); its
//! filter coefficients are `float`, as in FluidSynth.

use crate::conv;
use crate::sfont::{SoundFont, sample_at};

/// `FLUID_BUFSIZE`: samples per block.
pub const BUFSIZE: usize = 64;

pub const ENV_DELAY: u32 = 0;
pub const ENV_ATTACK: u32 = 1;
pub const ENV_HOLD: u32 = 2;
pub const ENV_DECAY: u32 = 3;
pub const ENV_SUSTAIN: u32 = 4;
pub const ENV_RELEASE: u32 = 5;
pub const ENV_FINISHED: u32 = 6;

// `enum fluid_loop`.
pub const UNLOOPED: i32 = 0;
pub const LOOP_DURING_RELEASE: i32 = 1;
pub const START_ON_RELEASE: i32 = 2;
pub const LOOP_UNTIL_RELEASE: i32 = 3;

const SANITY_CHECK: u8 = 1;
const SANITY_STARTUP: u8 = 2;
const MIN_LOOP_SIZE: i32 = 2;
const NOISE_FLOOR: f64 = 2.0e-7;

#[derive(Clone, Copy, Debug, Default)]
pub struct EnvData {
    pub count: u32,
    pub coeff: f64,
    pub increment: f64,
    pub min: f64,
    pub max: f64,
}

/// `fluid_adsr_env_t`.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub data: [EnvData; 7],
    pub section: u32,
    pub count: u32,
    pub val: f64,
}

impl Env {
    /// `fluid_adsr_env_calc`: one block's step.
    fn calc(&mut self) {
        let mut d = self.data[self.section as usize];
        while self.count >= d.count {
            if self.section == ENV_DECAY {
                self.val = d.min * d.coeff;
            }
            self.section += 1;
            d = self.data[self.section as usize];
            self.count = 0;
        }
        let mut x = d.coeff * self.val + d.increment;
        if x < d.min {
            x = d.min;
            self.section += 1;
            self.count = 0;
        } else if x > d.max {
            x = d.max;
            self.section += 1;
            self.count = 0;
        } else {
            self.count += 1;
        }
        self.val = x;
    }

    fn reset(&mut self) {
        self.count = 0;
        self.section = ENV_DELAY;
        self.val = 0.0;
    }

    pub fn set_section(&mut self, section: u32) {
        self.section = section;
        self.count = 0;
    }
}

/// `fluid_lfo_t`: a triangle, one step per block.
#[derive(Clone, Copy, Debug, Default)]
pub struct Lfo {
    pub val: f64,
    pub delay: u32,
    pub increment: f64,
}

impl Lfo {
    fn calc(&mut self, cur_delay: u32) {
        if cur_delay < self.delay {
            return;
        }
        self.val += self.increment;
        if self.val > 1.0 {
            self.increment = -self.increment;
            self.val = 2.0 - self.val;
        } else if self.val < -1.0 {
            self.increment = -self.increment;
            self.val = -2.0 - self.val;
        }
    }
}

// ───────────────────────── IIR filter ─────────────────────────

const Q_MIN: f64 = 0.001;
const FRES_MIN: i32 = 1500;
const FRES_MAX: i32 = 13500;
pub const SINCOS_TAB_SIZE: usize = (FRES_MAX - FRES_MIN + 1) as usize;

/// `fluid_iir_filter_init_table`: sin and cos of each cent's frequency.
pub fn sincos_table(sample_rate: f64) -> Vec<(f32, f32)> {
    let period = (2.0 * std::f64::consts::PI / sample_rate) as f32;
    (FRES_MIN..=FRES_MAX)
        .map(|cents| {
            let fres = conv::ct2hz(f64::from(cents)) as f32;
            let omega = period * fres;
            (omega.sin(), omega.cos())
        })
        .collect()
}

/// The voice's resonant lowpass: `fluid_iir_filter_t` with `FLUID_IIR_LOWPASS`
/// and no flags. It also applies the voice amplitude.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    b02: f32,
    b1: f32,
    a1: f32,
    a2: f32,
    hist1: f64,
    hist2: f64,
    startup: bool,
    pub fres: f64,
    last_fres: f64,
    fres_incr: f64,
    fres_incr_count: i32,
    last_q: f64,
    q_incr: f64,
    q_incr_count: i32,
    pub amp: f64,
    pub amp_incr: f64,
}

impl Filter {
    /// `fluid_iir_filter_reset`.
    pub fn reset(&mut self) {
        self.hist1 = 0.0;
        self.hist2 = 0.0;
        self.last_fres = -1.0;
        self.last_q = 0.0;
        self.startup = true;
        self.amp = 0.0;
        self.amp_incr = 0.0;
    }

    /// `fluid_iir_filter_set_q`, from the generator's centibels.
    pub fn set_q(&mut self, q_db: f64) {
        let q_db = (q_db / 10.0).clamp(0.0, 96.0) - f64::from(3.01f32);
        let q = 10.0f64.powf(q_db / 20.0);
        if self.startup {
            self.last_q = q;
            self.q_incr_count = 0;
        } else {
            let count = BUFSIZE as f64;
            if q >= Q_MIN && self.last_q < Q_MIN {
                self.last_q = Q_MIN;
            }
            self.q_incr = (q - self.last_q) / count;
            self.q_incr_count = count as i32;
        }
    }

    /// `fluid_iir_filter_calc`: the block's cutoff, `fres_mod` cents off.
    fn calc(&mut self, sincos: &[(f32, f32)], max_fres_ct: f64, fres_mod: f64, min_fres_ct: f64) {
        let mut fres = self.fres + fres_mod;
        if fres > max_fres_ct {
            fres = max_fres_ct;
        } else if fres < min_fres_ct {
            fres = min_fres_ct;
        }
        let fres_diff = fres - self.last_fres;
        let calc_coeff;
        if self.startup {
            calc_coeff = true;
            self.fres_incr_count = 0;
            self.last_fres = fres;
            self.startup = self.last_q < Q_MIN;
        } else if fres_diff.abs() > 1.0 {
            let num_buffers = self.last_q.clamp(1.0, 5.0);
            let count = BUFSIZE as f64 * num_buffers;
            self.fres_incr = fres_diff / count;
            self.fres_incr_count = (count + 0.5) as i32;
            calc_coeff = true;
        } else {
            let last = self.last_fres;
            self.last_fres = fres;
            self.fres_incr_count = 0;
            calc_coeff = last != fres;
        }
        if calc_coeff && !self.startup {
            (self.a1, self.a2, self.b02, self.b1) =
                coefficients(self.last_fres as f32, self.last_q as f32, sincos);
        }
    }

    /// `fluid_iir_filter_apply_local<GAIN_NORM, AMPLIFY, LOWPASS>`.
    fn apply(&mut self, sincos: &[(f32, f32)], buf: &mut [f64]) {
        if self.last_q < Q_MIN {
            return;
        }
        let (mut hist1, mut hist2) = (self.hist1, self.hist2);
        let (mut a1, mut a2, mut b02, mut b1) = (self.a1, self.a2, self.b02, self.b1);
        let (mut fres_count, mut q_count) = (self.fres_incr_count, self.q_incr_count);
        let mut amp = self.amp;
        let amp_incr = self.amp_incr;
        let mut fres = self.last_fres as f32;
        let mut q = self.last_q as f32;
        let fres_incr = self.fres_incr as f32;
        let q_incr = self.q_incr as f32;
        for x in buf.iter_mut() {
            let centernode = *x - f64::from(a1) * hist1 - f64::from(a2) * hist2;
            let sample = f64::from(b02) * (centernode + hist2) + f64::from(b1) * hist1;
            hist2 = hist1;
            hist1 = centernode;
            *x = amp * sample;
            amp += amp_incr;
            if fres_count > 0 || q_count > 0 {
                if fres_count > 0 {
                    fres_count -= 1;
                    fres += fres_incr;
                }
                if q_count > 0 {
                    q_count -= 1;
                    q += q_incr;
                    if f64::from(q) < Q_MIN {
                        q_count = 0;
                        q = Q_MIN as f32;
                    }
                }
                (a1, a2, b02, b1) = coefficients(fres, q, sincos);
            }
        }
        (self.a1, self.a2, self.b02, self.b1) = (a1, a2, b02, b1);
        let tiny = f64::from(1e-20f32);
        self.hist1 = if hist1.abs() < tiny { 0.0 } else { hist1 };
        self.hist2 = if hist2.abs() < tiny { 0.0 } else { hist2 };
        self.last_fres = f64::from(fres);
        self.fres_incr_count = fres_count;
        self.last_q = f64::from(q);
        self.q_incr_count = q_count;
        self.amp = amp;
    }
}

/// `fluid_iir_filter_calculate_coefficients<float, true, LOWPASS>`:
/// (a1, a2, b02, b1).
fn coefficients(fres: f32, q: f32, sincos: &[(f32, f32)]) -> (f32, f32, f32, f32) {
    let idx = ((fres as i32) - FRES_MIN).clamp(0, SINCOS_TAB_SIZE as i32 - 1) as usize;
    let (sin, cos) = sincos[idx];
    let alpha = sin / (2.0 * q);
    let a0_inv = 1.0 / (1.0 + alpha);
    let a1 = -2.0 * cos * a0_inv;
    let a2 = (1.0 - alpha) * a0_inv;
    let gain = 1.0 / q.sqrt();
    let b1 = (1.0 - cos) * a0_inv * gain;
    (a1, a2, b1 * 0.5, b1)
}

// ───────────────────────── the rvoice ─────────────────────────

/// `fluid_rvoice_buffers_t`: amplitude and destination of up to four sends.
#[derive(Clone, Copy, Debug, Default)]
pub struct Send {
    pub current_amp: f64,
    pub target_amp: f64,
    pub mapping: i32,
}

#[derive(Clone, Debug, Default)]
pub struct Rvoice {
    // envlfo
    pub ticks: u32,
    pub noteoff_ticks: u32,
    pub volenv: Env,
    pub modenv: Env,
    pub modenv_to_fc: f64,
    pub modenv_to_pitch: f64,
    pub modlfo: Lfo,
    pub modlfo_to_fc: f64,
    pub modlfo_to_pitch: f64,
    pub modlfo_to_vol: f64,
    pub viblfo: Lfo,
    pub viblfo_to_pitch: f64,
    // dsp
    pub samplemode: i32,
    pub has_looped: bool,
    pub sanity: u8,
    pub sample: Option<usize>,
    pub start: i32,
    pub end: i32,
    pub loopstart: i32,
    pub loopend: i32,
    pub pitchoffset: f64,
    pub pitchinc: f64,
    pub pitch: f64,
    pub root_pitch_hz: f64,
    pub max_filter_fres_ct: f64,
    pub attenuation: f64,
    pub prev_attenuation: f64,
    pub min_attenuation_cb: f64,
    pub noise_floor_nonloop: f64,
    pub noise_floor_loop: f64,
    pub synth_gain: f64,
    pub phase: u64,
    pub phase_incr: f64,
    pub filter: Filter,
    pub sends_count: u32,
    pub sends: [Send; 4],
}

impl Rvoice {
    /// A voice slot as `fluid_voice_initialize_rvoice` leaves it.
    pub fn new(output_rate: f64) -> Rvoice {
        let mut r = Rvoice::default();
        for env in [&mut r.volenv, &mut r.modenv] {
            env.data[ENV_SUSTAIN as usize] = EnvData {
                count: 0xffff_ffff,
                coeff: 1.0,
                increment: 0.0,
                min: -1.0,
                max: 2.0,
            };
            env.data[ENV_FINISHED as usize] = EnvData {
                count: 0xffff_ffff,
                coeff: 0.0,
                increment: 0.0,
                min: -1.0,
                max: 1.0,
            };
        }
        r.filter.reset();
        r.max_filter_fres_ct = conv::hz2ct(f64::from(0.45f32) * output_rate);
        r
    }

    /// `fluid_rvoice_reset`.
    pub fn reset(&mut self) {
        self.has_looped = false;
        self.ticks = 0;
        self.noteoff_ticks = 0;
        self.pitchoffset = 0.0;
        self.pitchinc = 0.0;
        self.modenv.reset();
        self.volenv.reset();
        self.viblfo.val = 0.0;
        self.modlfo.val = 0.0;
        self.filter.reset();
        self.sanity |= SANITY_STARTUP;
    }

    pub fn set_sample(&mut self, sample: Option<usize>) {
        self.sample = sample;
        if sample.is_some() {
            self.sanity |= SANITY_STARTUP;
        }
    }

    pub fn set_samplemode(&mut self, mode: i32) {
        self.samplemode = mode;
        self.sanity |= SANITY_CHECK;
    }

    pub fn set_synth_gain(&mut self, gain: f64) {
        self.synth_gain = gain;
        self.noise_floor_nonloop = NOISE_FLOOR / gain;
        self.noise_floor_loop = NOISE_FLOOR / gain;
        self.sanity |= SANITY_CHECK;
    }

    pub fn set_attenuation(&mut self, value: f64) {
        self.prev_attenuation = self.attenuation;
        self.attenuation = value;
    }

    pub fn set_address(&mut self, which: Address, value: i32) {
        match which {
            Address::Start => self.start = value,
            Address::End => self.end = value,
            Address::LoopStart => self.loopstart = value,
            Address::LoopEnd => self.loopend = value,
        }
        self.sanity |= SANITY_CHECK;
    }

    /// `fluid_rvoice_buffers_check_bufnum`.
    fn send_slot(&mut self, bufnum: usize) -> Option<&mut Send> {
        if bufnum >= self.sends.len() {
            return None;
        }
        if bufnum as u32 >= self.sends_count {
            for s in &mut self.sends[self.sends_count as usize..=bufnum] {
                s.target_amp = 0.0;
                s.current_amp = 0.0;
            }
            self.sends_count = bufnum as u32 + 1;
        }
        Some(&mut self.sends[bufnum])
    }

    pub fn set_send_amp(&mut self, bufnum: usize, value: f64) {
        if (bufnum as u32) < self.sends_count {
            self.sends[bufnum].target_amp = value;
        } else if let Some(s) = self.send_slot(bufnum) {
            s.target_amp = value;
        }
    }

    pub fn set_send_mapping(&mut self, bufnum: usize, mapping: i32) {
        if (bufnum as u32) < self.sends_count {
            self.sends[bufnum].mapping = mapping;
        } else if let Some(s) = self.send_slot(bufnum) {
            s.mapping = mapping;
        }
    }

    /// `fluid_rvoice_voiceoff`.
    pub fn voiceoff(&mut self) {
        self.volenv.set_section(ENV_FINISHED);
        self.modenv.set_section(ENV_FINISHED);
    }

    /// `fluid_rvoice_noteoff_LOCAL`: the release, or a later one when the
    /// note has not lasted `min_ticks`.
    pub fn noteoff(&mut self, min_ticks: u32) {
        if min_ticks > self.ticks {
            self.noteoff_ticks = min_ticks;
            return;
        }
        self.noteoff_ticks = 0;
        if self.volenv.section == ENV_ATTACK && self.volenv.val > 0.0 {
            // Releases from the attack's level in decibels.
            let lfo = self.modlfo.val * -self.modlfo_to_vol;
            let amp = self.volenv.val * conv::cb2amp(lfo);
            let env_value = -(((-200.0 / std::f64::consts::LN_10) * amp.ln() - lfo)
                / conv::PEAK_ATTENUATION
                - 1.0);
            self.volenv.val = env_value.clamp(0.0, 1.0);
        }
        if self.modenv.section == ENV_ATTACK && self.modenv.val > 0.0 {
            let env_value = conv::convex(127.0 * self.modenv.val);
            self.modenv.val = env_value.clamp(0.0, 1.0);
        }
        self.volenv.set_section(ENV_RELEASE);
        self.modenv.set_section(ENV_RELEASE);
    }

    /// `fluid_rvoice_multi_retrigger_attack`: retain the current amplitude
    /// while restarting a legato note's attack.
    pub fn retrigger(&mut self) {
        if self.volenv.section >= ENV_HOLD {
            self.volenv.val =
                conv::cb2amp(conv::PEAK_ATTENUATION * (1.0 - self.volenv.val)).clamp(0.0, 1.0);
        }
        self.volenv.set_section(ENV_ATTACK);
        self.volenv.val =
            self.volenv.val * conv::cb2amp(self.prev_attenuation) / conv::cb2amp(self.attenuation);
        let data = &mut self.volenv.data[ENV_ATTACK as usize];
        if self.volenv.val <= 1.0 {
            data.increment = f64::from(1.0f32 / data.count.max(1) as f32);
            data.min = -1.0;
            data.max = 1.0;
        } else {
            data.increment = -self.volenv.val / f64::from(data.count.max(1));
            data.min = 1.0;
            data.max = self.volenv.val;
        }
        if self.modenv.section >= ENV_HOLD {
            self.modenv.val = conv::cb2amp((1.0 - self.modenv.val) * conv::PEAK_ATTENUATION / 2.0)
                .clamp(0.0, 1.0);
        }
        self.modenv.set_section(ENV_ATTACK);
    }

    /// `fluid_rvoice_set_portamento`.
    pub fn set_portamento(&mut self, countinc: u32, pitchoffset: f64) {
        if countinc != 0 {
            self.pitchoffset += pitchoffset;
            self.pitchinc = -self.pitchoffset / f64::from(countinc);
        }
    }

    /// `fluid_rvoice_check_sample_sanity`.
    fn check_sample_sanity(&mut self, font: &SoundFont) {
        let s = &font.samples[self.sample.unwrap_or_default()];
        let (min_nonloop, max_nonloop) = (s.start as i32, s.end as i32);
        let (min_loop, max_loop) = (s.start as i32, s.end as i32 + 1);
        self.start = self.start.clamp(min_nonloop, max_nonloop.max(min_nonloop));
        self.end = self.end.clamp(min_nonloop, max_nonloop.max(min_nonloop));
        if self.start > self.end {
            std::mem::swap(&mut self.start, &mut self.end);
        }
        if self.start == self.end {
            self.voiceoff();
            return;
        }
        if self.samplemode == LOOP_UNTIL_RELEASE || self.samplemode == LOOP_DURING_RELEASE {
            self.loopstart = self.loopstart.clamp(min_loop, max_loop);
            self.loopend = self.loopend.clamp(min_loop, max_loop);
            if self.loopstart > self.loopend {
                std::mem::swap(&mut self.loopstart, &mut self.loopend);
            }
            if self.loopend < self.loopstart + MIN_LOOP_SIZE {
                self.samplemode = UNLOOPED;
            }
            if self.loopstart >= s.loopstart as i32 && self.loopend <= s.loopend as i32 {
                self.noise_floor_loop = match s.noise_floor_amplitude {
                    Some(a) if self.samplemode == LOOP_DURING_RELEASE => a / self.synth_gain,
                    _ => self.noise_floor_nonloop,
                };
            }
        }
        if self.sanity & SANITY_STARTUP != 0 {
            if max_loop - min_loop < MIN_LOOP_SIZE
                && (self.samplemode == LOOP_UNTIL_RELEASE || self.samplemode == LOOP_DURING_RELEASE)
            {
                self.samplemode = UNLOOPED;
            }
            self.phase = (self.start as u32 as u64) << 32;
        }
        if (self.samplemode == LOOP_UNTIL_RELEASE && self.volenv.section < ENV_RELEASE)
            || self.samplemode == LOOP_DURING_RELEASE
        {
            let index = (self.phase >> 32) as u32 as i32;
            if index >= self.loopend {
                self.phase = (self.loopstart as u32 as u64) << 32;
            }
        }
        self.sanity = 0;
    }

    /// `fluid_rvoice_calc_amp`: -1 silent, 0 finished, 1 audible.
    fn calc_amp(&mut self) -> i32 {
        if self.volenv.section == ENV_DELAY {
            return -1;
        }
        let target_amp = if self.volenv.section == ENV_ATTACK {
            conv::cb2amp(self.attenuation)
                * conv::cb2amp(self.modlfo.val * -self.modlfo_to_vol)
                * self.volenv.val
        } else {
            let cb = conv::PEAK_ATTENUATION
                .mul_add(1.0 - self.volenv.val, self.modlfo.val * -self.modlfo_to_vol);
            let target = conv::cb2amp(self.attenuation) * conv::cb2amp(cb);
            let floor = if self.has_looped {
                self.noise_floor_loop
            } else {
                self.noise_floor_nonloop
            };
            let amp_max = conv::cb2amp(self.min_attenuation_cb) * self.volenv.val;
            if amp_max < floor {
                return 0;
            }
            target
        };
        self.filter.amp_incr = (target_amp - self.filter.amp) / BUFSIZE as f64;
        if self.filter.amp == 0.0 && self.filter.amp_incr == 0.0 {
            return -1;
        }
        1
    }

    /// `fluid_rvoice_write`: renders one block into `buf`; returns the
    /// samples rendered (fewer than a block: the voice ended) or -1 for a
    /// silent block.
    pub fn write(
        &mut self,
        font: &SoundFont,
        sincos: &[(f32, f32)],
        min_fres_ct: f64,
        buf: &mut [f64; BUFSIZE],
    ) -> i32 {
        let ticks = self.ticks;
        if self.sample.is_none() {
            return 0;
        }
        if self.sanity != 0 {
            self.check_sample_sanity(font);
        }
        if self.noteoff_ticks != 0 && self.ticks >= self.noteoff_ticks {
            self.noteoff(0);
        }
        self.ticks = self.ticks.wrapping_add(BUFSIZE as u32);
        self.volenv.calc();
        if self.volenv.section == ENV_FINISHED {
            return 0;
        }
        self.modenv.calc();
        self.modlfo.calc(ticks);
        self.viblfo.calc(ticks);
        let count = self.calc_amp();
        if count == 0 {
            return 0;
        }
        let modenv_val = if self.modenv.section == ENV_ATTACK {
            conv::convex(127.0 * self.modenv.val)
        } else {
            self.modenv.val
        };
        let cents = modenv_val.mul_add(
            self.modenv_to_pitch,
            self.viblfo.val.mul_add(
                self.viblfo_to_pitch,
                self.modlfo
                    .val
                    .mul_add(self.modlfo_to_pitch, self.pitch + self.pitchoffset),
            ),
        );
        self.phase_incr = conv::ct2hz_real(cents) / self.root_pitch_hz;
        if self.pitchinc > 0.0 {
            self.pitchoffset += self.pitchinc;
            if self.pitchoffset > 0.0 {
                self.pitchoffset = 0.0;
                self.pitchinc = 0.0;
            }
        } else if self.pitchinc < 0.0 {
            self.pitchoffset += self.pitchinc;
            if self.pitchoffset < 0.0 {
                self.pitchoffset = 0.0;
                self.pitchinc = 0.0;
            }
        }
        if self.phase_incr == 0.0 {
            self.phase_incr = 1.0;
        }
        if self.samplemode == START_ON_RELEASE && self.volenv.section < ENV_RELEASE {
            return -1;
        }
        let looping = self.samplemode == LOOP_DURING_RELEASE
            || (self.samplemode == LOOP_UNTIL_RELEASE && self.volenv.section < ENV_RELEASE);
        let fmod = modenv_val.mul_add(self.modenv_to_fc, self.modlfo.val * self.modlfo_to_fc);
        self.filter
            .calc(sincos, self.max_filter_fres_ct, fmod, min_fres_ct);
        if count < 0 {
            return self.silence(buf, looping);
        }
        let n = self.interpolate(font, buf, looping);
        if n == 0 {
            return 0;
        }
        self.filter.apply(sincos, &mut buf[..n as usize]);
        n
    }

    /// `fluid_rvoice_dsp_silence_local`: the phase moves on, the block is 0.
    fn silence(&mut self, buf: &mut [f64; BUFSIZE], looping: bool) -> i32 {
        let incr = phase_from_float(self.phase_incr);
        let end_index = if looping {
            (self.loopend - 1) as u32
        } else {
            self.end as u32
        };
        let mut phase = self.phase;
        let mut i = 0usize;
        loop {
            let n = steps(phase, incr, end_index, i);
            for _ in 0..n {
                buf[i] = 0.0;
                phase = phase.wrapping_add(incr);
                i += 1;
            }
            if !looping {
                break;
            }
            if index_round(phase) > end_index {
                let start = (self.loopstart as u64) << 32;
                let length = ((self.loopend - self.loopstart) as u64) << 32;
                phase = start + phase.wrapping_sub(start) % length;
                self.has_looped = true;
            }
            if i >= BUFSIZE {
                break;
            }
        }
        self.phase = phase;
        i as i32
    }

    /// `fluid_rvoice_dsp_interpolate_4th_order_local`.
    fn interpolate(&mut self, font: &SoundFont, buf: &mut [f64; BUFSIZE], looping: bool) -> i32 {
        let data = &font.data[..];
        let data24 = font.data24.as_deref();
        let at = |idx: u32| -> f64 {
            let idx = idx as usize;
            if idx < data.len() {
                f64::from(sample_at(data, data24, idx))
            } else {
                0.0
            }
        };
        let incr = phase_from_float(self.phase_incr);
        let mut phase = self.phase;
        let mut end_index =
            (if looping { self.loopend - 1 } else { self.end } as u32).wrapping_sub(2);
        let (mut start_index, mut start_point) = if self.has_looped {
            (self.loopstart as u32, at((self.loopend - 1) as u32))
        } else {
            (self.start as u32, at(self.start as u32))
        };
        let (end_point1, end_point2) = if looping {
            (at(self.loopstart as u32), at((self.loopstart + 1) as u32))
        } else {
            let p = at(self.end as u32);
            (p, p)
        };
        let cubic = |phase: u64, s0: f64, s1: f64, s2: f64, s3: f64| -> f64 {
            let c = conv::interp_coeff(((phase as u32 & 0xff00_0000) >> 24) as usize);
            c[3].mul_add(s3, c[2].mul_add(s2, c[1].mul_add(s1, c[0] * s0)))
        };
        let mut i = 0usize;
        loop {
            let mut idx = (phase >> 32) as u32;
            for _ in 0..steps(phase, incr, start_index, i) {
                buf[i] = cubic(
                    phase,
                    start_point,
                    at(idx),
                    at(idx.wrapping_add(1)),
                    at(idx.wrapping_add(2)),
                );
                phase = phase.wrapping_add(incr);
                idx = (phase >> 32) as u32;
                i += 1;
            }
            for _ in 0..steps(phase, incr, end_index, i) {
                buf[i] = cubic(
                    phase,
                    at(idx.wrapping_sub(1)),
                    at(idx),
                    at(idx.wrapping_add(1)),
                    at(idx.wrapping_add(2)),
                );
                phase = phase.wrapping_add(incr);
                idx = (phase >> 32) as u32;
                i += 1;
            }
            if i >= BUFSIZE {
                break;
            }
            end_index = end_index.wrapping_add(1);
            for _ in 0..steps(phase, incr, end_index, i) {
                buf[i] = cubic(
                    phase,
                    at(idx.wrapping_sub(1)),
                    at(idx),
                    at(idx.wrapping_add(1)),
                    end_point1,
                );
                phase = phase.wrapping_add(incr);
                idx = (phase >> 32) as u32;
                i += 1;
            }
            end_index = end_index.wrapping_add(1);
            for _ in 0..steps(phase, incr, end_index, i) {
                buf[i] = cubic(
                    phase,
                    at(idx.wrapping_sub(1)),
                    at(idx),
                    end_point1,
                    end_point2,
                );
                phase = phase.wrapping_add(incr);
                idx = (phase >> 32) as u32;
                i += 1;
            }
            if !looping {
                break;
            }
            if idx > end_index {
                let start = (self.loopstart as u64) << 32;
                let length = ((self.loopend - self.loopstart) as u64) << 32;
                phase = start + phase.wrapping_sub(start) % length;
                if !self.has_looped {
                    self.has_looped = true;
                    start_index = self.loopstart as u32;
                    start_point = at((self.loopend - 1) as u32);
                }
            }
            if i >= BUFSIZE {
                break;
            }
            end_index = end_index.wrapping_sub(2);
        }
        self.phase = phase;
        i as i32
    }

    /// `fluid_rvoice_buffers_mix`: `count` samples of `dsp` (from block
    /// `start_block`) into the send buffers, ramping each send's amplitude.
    pub fn mix(&mut self, dsp: &[f64; BUFSIZE], count: usize, dest: &mut [[f64; BUFSIZE]; 4]) {
        if count == 0 {
            return;
        }
        for send in &mut self.sends[..self.sends_count as usize] {
            let (target, mut current) = (send.target_amp, send.current_amp);
            let Some(buf) = usize::try_from(send.mapping)
                .ok()
                .and_then(|m| dest.get_mut(m))
            else {
                continue;
            };
            if current == 0.0 && target == 0.0 {
                continue;
            }
            let incr = (target - current) / BUFSIZE as f64;
            if count < BUFSIZE {
                for (b, &x) in buf[..count].iter_mut().zip(&dsp[..count]) {
                    *b = current.mul_add(x, *b);
                    current += incr;
                }
            } else {
                for (j, (b, &x)) in buf.iter_mut().zip(dsp.iter()).enumerate() {
                    *b = incr.mul_add(j as f64, current).mul_add(x, *b);
                }
            }
            send.current_amp = target;
        }
    }
}

/// Which address generator an event sets.
#[derive(Clone, Copy, Debug)]
pub enum Address {
    Start,
    End,
    LoopStart,
    LoopEnd,
}

/// `fluid_phase_set_float`: 32.32 fixed point.
fn phase_from_float(b: f64) -> u64 {
    ((b as u64) << 32) | ((b - f64::from(b as i32)) * 4_294_967_296.0) as u32 as u64
}

fn index_round(phase: u64) -> u32 {
    (phase.wrapping_add(0x8000_0000) >> 32) as u32
}

/// `compute_interpolation_steps`: the samples left before the phase passes
/// `end_index`, at most up to the block's end.
fn steps(phase: u64, incr: u64, end_index: u32, i: usize) -> usize {
    let boundary = u64::from(end_index.wrapping_add(1)) << 32;
    // A zero increment divides to 0 on arm64, where FluidSynth runs here.
    let n = if phase >= boundary {
        0
    } else if incr == 0 {
        (BUFSIZE - i) as u64
    } else {
        (boundary - phase).div_ceil(incr)
    };
    n.min((BUFSIZE - i) as u64) as usize
}
