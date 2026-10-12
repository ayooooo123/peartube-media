//! FluidSynth 2.6.1 Dattorro reverb and sine chorus, at its default settings.
//! Copyright (C) 2003 Peter Hanappe and others; LGPL-2.1-or-later.
use crate::rvoice::BUFSIZE;

struct Delay {
    line: Vec<f32>,
    pos: usize,
    last: f32,
}
impl Delay {
    fn new(seconds: f32, rate: f64) -> Self {
        Self {
            line: vec![0.0; ((f64::from(seconds) * rate + 0.5) as usize).max(1)],
            pos: 0,
            last: 0.0,
        }
    }
    fn process(&mut self, input: f32) -> f32 {
        self.last = self.line[self.pos];
        self.line[self.pos] = input;
        self.pos += 1;
        if self.pos == self.line.len() {
            self.pos = 0;
        }
        self.last
    }
    fn allpass(&mut self, input: f32, feedback: f32) -> f32 {
        let old = self.line[self.pos];
        let value = old.mul_add(feedback, input);
        self.process(value);
        old - value * feedback
    }
    fn tap(&self, offset: usize) -> f32 {
        self.line[(self.pos + offset) % self.line.len()]
    }
    fn reset(&mut self) {
        self.line.fill(0.0);
        self.pos = 0;
        self.last = 0.0;
    }
}

pub struct Reverb {
    pre: Delay,
    input: [Delay; 4],
    tank: [Delay; 4],
    delay: [Delay; 4],
    taps: [usize; 14],
    bandwidth: f32,
    damping: [f32; 2],
    damp: f32,
    decay: f32,
    wet: [f64; 2],
}
impl Reverb {
    pub fn new(rate: f64) -> Self {
        let times = [
            142, 107, 379, 277, 672, 4453, 1800, 3720, 908, 4217, 2656, 3163,
        ];
        let delay = |i: usize| Delay::new((f64::from(times[i]) / 29761.0) as f32, rate);
        let taps = [
            266, 2974, 1913, 1996, 1990, 187, 1066, 353, 3627, 1228, 2673, 2111, 335, 121,
        ];
        let width = f64::from(0.8f32);
        let level = f64::from(0.7f32);
        let wet = level / (1.0 + width * f64::from(0.2f32));
        Self {
            // The reference divides its 4-ms constant by 1000 twice.
            pre: Delay::new((4.0f32 / 1000.0) / 1000.0, rate),
            input: std::array::from_fn(delay),
            tank: std::array::from_fn(|i| delay(4 + i * 2)),
            delay: std::array::from_fn(|i| delay(5 + i * 2)),
            taps: taps.map(|n| {
                ((f64::from((f64::from(n) / 29761.0) as f32) * rate + 0.5) as usize).max(1)
            }),
            bandwidth: 0.0,
            damping: [0.0; 2],
            damp: 0.2,
            decay: (f64::from(0.2f32) + 0.5 * f64::from(0.78f32)) as f32,
            wet: [wet * (width / 2.0 + 0.5), wet * ((1.0 - width) / 2.0)],
        }
    }
    pub fn reset(&mut self) {
        self.pre.reset();
        for line in self
            .input
            .iter_mut()
            .chain(&mut self.tank)
            .chain(&mut self.delay)
        {
            line.reset();
        }
        self.bandwidth = 0.0;
        self.damping = [0.0; 2];
    }
    pub fn process(
        &mut self,
        input: &[f64; BUFSIZE],
        left: &mut [f64; BUFSIZE],
        right: &mut [f64; BUFSIZE],
    ) {
        for i in 0..BUFSIZE {
            let pre = self.pre.process(input[i] as f32 * 0.6);
            self.bandwidth = 0.9999f32.mul_add(pre, (1.0 - 0.9999f32) * self.bandwidth);
            let mut split = self.bandwidth;
            for (j, ap) in self.input.iter_mut().enumerate() {
                split = ap.allpass(split, if j < 2 { 0.75 } else { 0.625 });
            }
            let mut l = self.decay.mul_add(self.delay[3].last, split);
            l = self.tank[0].allpass(l, 0.7);
            l = self.delay[0].process(l);
            self.damping[0] = (1.0 - self.damp).mul_add(l, self.damp * self.damping[0]);
            l = self.tank[1].allpass(self.decay * self.damping[0], 0.5);
            self.delay[1].process(l);
            let mut r = self.decay.mul_add(self.delay[1].last, split);
            r = self.tank[2].allpass(r, 0.7);
            r = self.delay[2].process(r);
            self.damping[1] = (1.0 - self.damp).mul_add(r, self.damp * self.damping[1]);
            r = self.tank[3].allpass(self.decay * self.damping[1], 0.5);
            self.delay[3].process(r);
            let t = &self.taps;
            let l = self.delay[2].tap(t[0]) + self.delay[2].tap(t[1]) - self.tank[3].tap(t[2])
                + self.delay[3].tap(t[3])
                - self.delay[0].tap(t[4])
                - self.tank[1].tap(t[5])
                - self.delay[1].tap(t[6]);
            let r = self.delay[0].tap(t[7]) + self.delay[0].tap(t[8]) - self.tank[1].tap(t[9])
                + self.delay[1].tap(t[10])
                - self.delay[2].tap(t[11])
                - self.tank[3].tap(t[12])
                - self.delay[3].tap(t[13]);
            left[i] += f64::from(l).mul_add(self.wet[0], f64::from(r) * self.wet[1]);
            right[i] += f64::from(r).mul_add(self.wet[0], f64::from(l) * self.wet[1]);
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Modulator {
    a1: f64,
    b1: f64,
    b2: f64,
    reset: f64,
    pos: usize,
    fraction: f64,
    previous: f64,
}
impl Modulator {
    fn new(freq: f32, rate: f32, phase: f32) -> Self {
        let w = 2.0 * std::f64::consts::PI * f64::from(freq) / f64::from(rate);
        let a = (2.0 * std::f64::consts::PI / 360.0) * f64::from(phase);
        Self {
            a1: 2.0 * w.cos(),
            b1: a.sin(),
            b2: (a - w).sin(),
            reset: (std::f64::consts::FRAC_PI_2 - w).sin(),
            ..Self::default()
        }
    }
    fn next(&mut self) -> f64 {
        let mut out = self.a1.mul_add(self.b1, -self.b2);
        self.b2 = self.b1;
        if out >= 1.0 {
            out = 1.0;
            self.b2 = self.reset;
        }
        if out <= -1.0 {
            out = -1.0;
            self.b2 = -self.reset;
        }
        self.b1 = out;
        out
    }
}
pub struct Chorus {
    line: [f64; 2049],
    input: usize,
    center: f64,
    depth: i32,
    rate: i32,
    index: i32,
    mods: [Modulator; 3],
    wet: [f64; 2],
}
impl Chorus {
    pub fn new(sample_rate: f64) -> Self {
        let depth = ((4.25 / 1000.0 * sample_rate) as i32).min(2048) / 2;
        let rate = 5 + if depth > 176 {
            -(depth - 176) / (1024 - 176)
        } else {
            0
        };
        let wet = f64::from(0.6f32) / (1.0 + 10.0 * f64::from(0.2f32));
        Self {
            line: [0.0; 2049],
            input: 0,
            center: f64::from(2049 - 1 - depth),
            depth,
            rate,
            index: rate,
            mods: std::array::from_fn(|i| {
                Modulator::new(
                    (0.2 * f64::from(rate)) as f32,
                    sample_rate as f32,
                    (360.0f32 / 3.0) * i as f32,
                )
            }),
            wet: [wet * 5.5, wet * -4.5],
        }
    }
    pub fn reset(&mut self) {
        self.line.fill(0.0);
        for m in &mut self.mods {
            m.previous = 0.0;
            m.fraction = 0.0;
        }
    }
    pub fn process(
        &mut self,
        input: &[f64; BUFSIZE],
        left: &mut [f64; BUFSIZE],
        right: &mut [f64; BUFSIZE],
    ) {
        for i in 0..BUFSIZE {
            self.index += 1;
            let mut stereo = [0.0; 2];
            let mut last = 0.0;
            for (j, m) in self.mods.iter_mut().enumerate() {
                if self.index >= self.rate {
                    let pos = m.next().mul_add(f64::from(self.depth), self.center);
                    let integer = if pos >= 0.0 {
                        pos as i32
                    } else {
                        (pos - 1.0) as i32
                    };
                    m.pos = integer.rem_euclid(2049) as usize;
                    m.fraction = pos - f64::from(integer);
                }
                let out = self.line[m.pos];
                m.pos += 1;
                if m.pos == self.line.len() {
                    m.pos = 0;
                }
                last = m.fraction.mul_add(self.line[m.pos] - m.previous, out);
                m.previous = last;
                stereo[j & 1] += last;
            }
            if self.index >= self.rate {
                self.index = 0;
                self.center += f64::from(self.rate);
                if self.center >= 2049.0 {
                    self.center -= 2049.0;
                }
            }
            stereo[1] += last;
            self.line[self.input] = input[i];
            self.input += 1;
            if self.input == self.line.len() {
                self.input = 0;
            }
            left[i] += stereo[0].mul_add(self.wet[0], stereo[1] * self.wet[1]);
            right[i] += stereo[1].mul_add(self.wet[0], stereo[0] * self.wet[1]);
        }
    }
}
