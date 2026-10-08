//! Samples, ported from libopenmpt 0.8.9 `soundlib/ModSample.h/.cpp`.
//!
//! The waveform sits in one buffer with libopenmpt's lookahead areas: 16
//! frames before the first sampling point (copies of it), then after the
//! last one 16 frames holding it, 64 frames of the normal loop's
//! wrap-around and 64 of the sustain loop's (`PrecomputeLoops`). The mixer
//! addresses them exactly as libopenmpt's pointer arithmetic does.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

#![allow(dead_code)]

use std::sync::Arc;

use crate::defs::*;

/// Frames before the first sampling point.
pub const PRE_FRAMES: usize = INTERPOLATION_LOOKAHEAD_BUFFER_SIZE as usize;
/// Frames after the last one: end hold, loop and sustain wrap-around.
pub const POST_FRAMES: usize = (1 + 4 + 4) * INTERPOLATION_LOOKAHEAD_BUFFER_SIZE as usize;

/// Sample frames, shared between copies of a song (a seek restarts from a
/// copy); writers go through `Arc::make_mut`.
#[derive(Clone, Debug, Default)]
pub enum SampleData {
    #[default]
    None,
    I8(Arc<Vec<i8>>),
    I16(Arc<Vec<i16>>),
}

impl SampleData {
    /// The 8-bit frames for writing, copied first if shared.
    pub fn i8_mut(&mut self) -> Option<&mut [i8]> {
        match self {
            SampleData::I8(d) => Some(Arc::make_mut(d).as_mut_slice()),
            _ => None,
        }
    }

    /// The 16-bit frames for writing, copied first if shared.
    pub fn i16_mut(&mut self) -> Option<&mut [i16]> {
        match self {
            SampleData::I16(d) => Some(Arc::make_mut(d).as_mut_slice()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ModSample {
    pub n_length: SmpLength,
    pub n_loop_start: SmpLength,
    pub n_loop_end: SmpLength,
    pub n_sustain_start: SmpLength,
    pub n_sustain_end: SmpLength,
    pub data: SampleData,
    pub n_c5_speed: u32,
    pub n_pan: u16,
    pub n_volume: u16,
    pub n_global_vol: u16,
    pub u_flags: u32,
    pub relative_tone: i8,
    pub n_fine_tune: i8,
    pub n_vib_type: u8,
    pub n_vib_sweep: u8,
    pub n_vib_depth: u8,
    pub n_vib_rate: u8,
    pub cues: [SmpLength; 9],
    pub name: String,
}

impl Default for ModSample {
    fn default() -> Self {
        let mut s = ModSample {
            n_length: 0,
            n_loop_start: 0,
            n_loop_end: 0,
            n_sustain_start: 0,
            n_sustain_end: 0,
            data: SampleData::None,
            n_c5_speed: 8363,
            n_pan: 128,
            n_volume: 256,
            n_global_vol: 64,
            u_flags: 0,
            relative_tone: 0,
            n_fine_tune: 0,
            n_vib_type: VIB_SINE,
            n_vib_sweep: 0,
            n_vib_depth: 0,
            n_vib_rate: 0,
            cues: [MAX_SAMPLE_LENGTH; 9],
            name: String::new(),
        };
        s.initialize(MOD_TYPE_NONE);
        s
    }
}

impl ModSample {
    pub fn new(mod_type: u32) -> Self {
        let mut s = ModSample::default();
        s.initialize(mod_type);
        s
    }

    /// `ModSample::Initialize`.
    pub fn initialize(&mut self, mod_type: u32) {
        self.data = SampleData::None;
        self.n_length = 0;
        self.n_loop_start = 0;
        self.n_loop_end = 0;
        self.n_sustain_start = 0;
        self.n_sustain_end = 0;
        self.n_c5_speed = 8363;
        self.n_pan = 128;
        self.n_volume = 256;
        self.n_global_vol = 64;
        self.u_flags &= !(CHN_PANNING
            | CHN_SUSTAINLOOP
            | CHN_LOOP
            | CHN_PINGPONGLOOP
            | CHN_PINGPONGSUSTAIN
            | CHN_REVERSE
            | CHN_ADLIB
            | SMP_MODIFIED
            | SMP_KEEPONDISK);
        if mod_type == MOD_TYPE_XM {
            self.u_flags |= CHN_PANNING;
        }
        self.relative_tone = 0;
        self.n_fine_tune = 0;
        self.n_vib_type = VIB_SINE;
        self.n_vib_sweep = 0;
        self.n_vib_depth = 0;
        self.n_vib_rate = 0;
        self.name.clear();
        if mod_type & (MOD_TYPE_DBM | MOD_TYPE_IMF | MOD_TYPE_MED) != 0 {
            for i in 1..10u32 {
                self.cues[(i - 1) as usize] = muldiv_unsigned(i, 255 * 256, 9);
            }
        } else {
            self.remove_all_cue_points();
        }
    }

    pub fn has_sample_data(&self) -> bool {
        !matches!(self.data, SampleData::None) && self.n_length != 0
    }
    pub fn elementary_sample_size(&self) -> u32 {
        if self.u_flags & CHN_16BIT != 0 {
            2
        } else {
            1
        }
    }
    pub fn num_channels(&self) -> u32 {
        if self.u_flags & CHN_STEREO != 0 {
            2
        } else {
            1
        }
    }
    pub fn bytes_per_sample(&self) -> u32 {
        self.elementary_sample_size() * self.num_channels()
    }

    /// Allocates a silent buffer for `n_length` frames in the current
    /// format. Returns false (and frees) when the length is 0 or too long.
    pub fn allocate(&mut self) -> bool {
        self.data = SampleData::None;
        if self.n_length == 0 || self.n_length > MAX_SAMPLE_LENGTH {
            return false;
        }
        let frames = PRE_FRAMES + self.n_length as usize + POST_FRAMES;
        let elems = frames * self.num_channels() as usize;
        self.data = if self.u_flags & CHN_16BIT != 0 {
            SampleData::I16(Arc::new(vec![0; elems]))
        } else {
            SampleData::I8(Arc::new(vec![0; elems]))
        };
        true
    }

    /// Element index of sampling point `frame`, channel `c`.
    pub fn elem(&self, frame: isize, c: usize) -> usize {
        ((PRE_FRAMES as isize + frame) as usize) * self.num_channels() as usize + c
    }

    pub fn set_i16(&mut self, frame: usize, c: usize, v: i16) {
        let i = self.elem(frame as isize, c);
        match &mut self.data {
            SampleData::I16(d) => Arc::make_mut(d)[i] = v,
            SampleData::I8(d) => Arc::make_mut(d)[i] = (v >> 8) as i8,
            SampleData::None => {}
        }
    }
    pub fn set_i8(&mut self, frame: usize, c: usize, v: i8) {
        let i = self.elem(frame as isize, c);
        match &mut self.data {
            SampleData::I8(d) => Arc::make_mut(d)[i] = v,
            SampleData::I16(d) => Arc::make_mut(d)[i] = (v as i16) << 8,
            SampleData::None => {}
        }
    }

    /// `SanitizeLoops`.
    pub fn sanitize_loops(&mut self) {
        self.n_sustain_end = self.n_sustain_end.min(self.n_length);
        self.n_loop_end = self.n_loop_end.min(self.n_length);
        if self.n_sustain_start >= self.n_sustain_end {
            self.n_sustain_start = 0;
            self.n_sustain_end = 0;
            self.u_flags &= !(CHN_SUSTAINLOOP | CHN_PINGPONGSUSTAIN);
        }
        if self.n_loop_start >= self.n_loop_end {
            self.n_loop_start = 0;
            self.n_loop_end = 0;
            self.u_flags &= !(CHN_LOOP | CHN_PINGPONGLOOP);
        }
    }

    /// `PrecomputeLoops` (without the channel update: loading happens before
    /// playback).
    pub fn precompute_loops(&mut self, it_ping_pong_mode: bool) {
        if !self.has_sample_data() {
            return;
        }
        self.sanitize_loops();
        let nch = self.num_channels() as usize;
        let len = self.n_length as usize;
        let lb = INTERPOLATION_LOOKAHEAD_BUFFER_SIZE as usize;
        let data_start = PRE_FRAMES * nch;
        let after = data_start + len * nch;
        let loop_la = after + lb * nch;
        let sus_la = loop_la + 4 * lb * nch;
        let (ls, le, ploop) = (self.n_loop_start as usize, self.n_loop_end as usize, self.u_flags & CHN_LOOP != 0);
        let ping = self.u_flags & CHN_PINGPONGLOOP != 0;
        let (ss, se, psus) = (self.n_sustain_start as usize, self.n_sustain_end as usize, self.u_flags & CHN_SUSTAINLOOP != 0);
        let sping = self.u_flags & CHN_PINGPONGSUSTAIN != 0;
        fn run<T: Copy>(
            d: &mut [T],
            nch: usize,
            lb: usize,
            data_start: usize,
            after: usize,
            loops: [(bool, usize, usize, usize, bool); 2],
            it_mode: bool,
        ) {
            // Hold the first and last sampling points around the waveform.
            for i in 0..lb {
                for c in 0..nch {
                    d[after + i * nch + c] = d[after - nch + c];
                    d[data_start - (i + 1) * nch + c] = d[data_start + c];
                }
            }
            for (enabled, target, start, len, pingpong) in loops {
                if enabled && len > 0 {
                    copy_loop(d, nch, lb, target, data_start + start * nch, len, pingpong, it_mode, true);
                    copy_loop(d, nch, lb, target, data_start + start * nch, len, pingpong, it_mode, false);
                }
            }
        }
        let loops = [
            (ploop, loop_la, ls, le.saturating_sub(ls), ping),
            (psus, sus_la, ss, se.saturating_sub(ss), sping),
        ];
        if let Some(d) = self.data.i8_mut() {
            run(d, nch, lb, data_start, after, loops, it_ping_pong_mode);
        } else if let Some(d) = self.data.i16_mut() {
            run(d, nch, lb, data_start, after, loops, it_ping_pong_mode);
        }
    }

    pub fn has_loop(&self) -> bool {
        self.u_flags & CHN_LOOP != 0 && self.n_loop_end > self.n_loop_start
    }
    pub fn has_sustain_loop(&self) -> bool {
        self.u_flags & CHN_SUSTAINLOOP != 0 && self.n_sustain_end > self.n_sustain_start
    }

    pub fn remove_all_cue_points(&mut self) {
        if self.u_flags & CHN_ADLIB == 0 {
            self.cues = [MAX_SAMPLE_LENGTH; 9];
        }
    }
    pub fn set_default_cue_points(&mut self) {
        for i in 0..9 {
            self.cues[i] = ((i as u32) + 1) << 11;
        }
    }

    /// `TransposeToFrequency(transpose, finetune)`.
    pub fn transpose_to_frequency(transpose: i32, finetune: i32) -> u32 {
        let v = (2.0f64.powf((transpose as f64 * 128.0 + finetune as f64) * (1.0 / (12.0 * 128.0))) * 8363.0).round();
        if v < 0.0 {
            0
        } else if v > u32::MAX as f64 {
            u32::MAX
        } else {
            v as u32
        }
    }
    pub fn transpose_to_frequency_self(&mut self) {
        self.n_c5_speed = Self::transpose_to_frequency(self.relative_tone as i32, self.n_fine_tune as i32);
    }

    /// `FrequencyToTranspose(freq)` -> (transpose, finetune).
    pub fn frequency_to_transpose(freq: u32) -> (i8, i8) {
        if freq == 0 {
            return (0, 0);
        }
        let f2t = (((freq as f64) * (1.0 / 8363.0)).ln() * (12.0 * 128.0 * (1.0 / core::f64::consts::LN_2))).round();
        let f2t = f2t.clamp(i32::MIN as f64, i32::MAX as f64) as i32;
        let f2t = f2t.clamp(-16384, 16383);
        ((f2t / 128) as i8, (f2t % 128) as i8)
    }
    pub fn frequency_to_transpose_self(&mut self) {
        let (t, f) = Self::frequency_to_transpose(self.n_c5_speed);
        self.relative_tone = t;
        self.n_fine_tune = f;
    }

    /// `GetSampleRate(type)`.
    pub fn sample_rate(&self, mod_type: u32) -> u32 {
        let mut rate = if crate::sndfile::use_finetune_and_transpose(mod_type) {
            Self::transpose_to_frequency(self.relative_tone as i32, self.n_fine_tune as i32)
        } else {
            self.n_c5_speed
        };
        if mod_type == MOD_TYPE_MOD {
            rate = muldivr_unsigned(rate, 8287, 8363);
        }
        if rate > 0 {
            rate
        } else {
            8363
        }
    }
}

/// `PrecomputeLoop::CopyLoop`: one direction of a loop's wrap-around copy.
#[allow(clippy::too_many_arguments)]
fn copy_loop<T: Copy>(
    d: &mut [T],
    nch: usize,
    lb: usize,
    target: usize,
    loop_data: usize,
    loop_end: usize,
    pingpong: bool,
    it_ping_pong_mode: bool,
    direction: bool,
) {
    let num_samples = 2 * lb + usize::from(direction);
    let mut dest = (target + nch * (2 * lb - 1)) as isize;
    let mut read_position = loop_end - 1;
    let write_increment: isize = if direction { 1 } else { -1 };
    let mut read_increment = write_increment;
    for _ in 0..num_samples {
        for c in 0..nch {
            d[dest as usize + c] = d[loop_data + read_position * nch + c];
        }
        dest += write_increment * nch as isize;
        if read_position == loop_end - 1 && read_increment > 0 {
            if pingpong {
                read_increment = -1;
                if it_ping_pong_mode && read_position > 0 {
                    read_position -= 1;
                }
            } else {
                read_position = 0;
            }
        } else if read_position == 0 && read_increment < 0 {
            if pingpong {
                read_increment = 1;
            } else {
                read_position = loop_end - 1;
            }
        } else {
            read_position = (read_position as isize + read_increment) as usize;
        }
    }
}
