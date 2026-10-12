// Ported from FFmpeg libavcodec/mpc.c, mpc.h, libavutil/lfg.c and libavutil/lfg.h
// (commit 2da55bf), LGPL-2.1-or-later.
// Copyright (c) 2006 Konstantin Shishkov; Copyright (c) 2008 Michael Niedermayer.

use crate::mpc_data::{MPC_CC, MPC_SCF};
use mpegaudiodsp::MpaSynth;

pub const BANDS: usize = 32;
pub const SAMPLES_PER_BAND: usize = 36;
pub const MPC_FRAME_SIZE: usize = BANDS * SAMPLES_PER_BAND; // 1152

#[derive(Clone, Copy, Debug, Default)]
pub struct Band {
    pub msf: i32,
    pub res: [i32; 2],
    pub scfi: [i32; 2],
    pub scf_idx: [[i32; 3]; 2],
}

pub const LFG_SEED_DEADBEEF: [u32; 64] = [
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0x6DCDBA27, 0xE6FA6F02, 0x65299E02, 0x47293B84, 0x9A4C779C, 0x3D077D88, 0x652BE777, 0xDFF65602,
    0x164FFD1C, 0xA9BFA940, 0xCEACA4E3, 0xF8DB1794, 0x1C110953, 0x94A19EB2, 0xDC012EE2, 0x5A43DF7A,
    0x94BACEF2, 0xBCDBAAF7, 0xA1CE55AB, 0x20459B02, 0x10406FDF, 0x0A2CB87F, 0x7034581D, 0x3FEF8054,
    0x20AF0FB5, 0x1D961BE2, 0x5AD3ACB3, 0x376B24FA, 0xDF748C50, 0x7BE21D0D, 0x8A764917, 0x5552FF47,
    0xC19C19CA, 0x6A2512F1, 0x981CE26A, 0x0CE871ED, 0xE751AD17, 0x5F4A0447, 0x914FA4FE, 0x5051F986,
    0x9FD86E34, 0x21AB0D68, 0x6728E7DA, 0x491310CF, 0xC1514654, 0x873AC25D, 0x3DA39BEA, 0x77C7ED17,
    0x19379ED9, 0x2B5E84D2, 0xBA11E841, 0xF4B869AE, 0xAF76C958, 0x9D1A234D, 0x5C0BC890, 0xBA94F5B0,
];

#[derive(Clone, Debug)]
pub struct Lfg {
    state: [u32; 64],
    index: u32,
}

impl Default for Lfg {
    fn default() -> Self {
        Self::new()
    }
}

impl Lfg {
    pub fn new() -> Self {
        Self { state: LFG_SEED_DEADBEEF, index: 0 }
    }

    #[inline]
    pub fn get(&mut self) -> u32 {
        let i = self.index;
        let a = self.state[(i.wrapping_sub(24) & 63) as usize]
            .wrapping_add(self.state[(i.wrapping_sub(55) & 63) as usize]);
        self.state[(i & 63) as usize] = a;
        self.index = i.wrapping_add(1);
        a
    }
}

#[inline]
fn clipf(a: f32, amin: f32, amax: f32) -> f32 {
    if a < amin {
        amin
    } else if a > amax {
        amax
    } else {
        a
    }
}

pub fn dequantize_and_synth(
    synth: &mut MpaSynth,
    bands: &[Band],
    maxband: usize,
    q: &[[i32; MPC_FRAME_SIZE]; 2],
    out: &mut [&mut [i16]],
    channels: usize,
) {
    let mut sb_samples = [[[0i32; BANDS]; SAMPLES_PER_BAND]; 2];
    let maxband = maxband.min(BANDS - 1);

    let mut off = 0;
    for i in 0..=maxband {
        for ch in 0..2 {
            let res = bands[i].res[ch];
            if res != 0 {
                let cc_idx = if res == -1 {
                    0
                } else if res > 0 && (res as usize + 1) < MPC_CC.len() {
                    res as usize + 1
                } else {
                    0
                };
                let cc_val = MPC_CC[cc_idx];

                let scf0 = MPC_SCF[(bands[i].scf_idx[ch][0] & 0xFF) as usize];
                let mul0 = cc_val * scf0;
                for j in 0..12 {
                    let v = clipf(mul0 * (q[ch][j + off] as f32), i32::MIN as f32, i32::MAX as f32);
                    sb_samples[ch][j][i] = v as i32;
                }

                let scf1 = MPC_SCF[(bands[i].scf_idx[ch][1] & 0xFF) as usize];
                let mul1 = cc_val * scf1;
                for j in 12..24 {
                    let v = clipf(mul1 * (q[ch][j + off] as f32), i32::MIN as f32, i32::MAX as f32);
                    sb_samples[ch][j][i] = v as i32;
                }

                let scf2 = MPC_SCF[(bands[i].scf_idx[ch][2] & 0xFF) as usize];
                let mul2 = cc_val * scf2;
                for j in 24..36 {
                    let v = clipf(mul2 * (q[ch][j + off] as f32), i32::MIN as f32, i32::MAX as f32);
                    sb_samples[ch][j][i] = v as i32;
                }
            }
        }
        if bands[i].msf != 0 {
            for j in 0..SAMPLES_PER_BAND {
                let t1 = sb_samples[0][j][i] as u32;
                let t2 = sb_samples[1][j][i] as u32;
                sb_samples[0][j][i] = t1.wrapping_add(t2) as i32;
                sb_samples[1][j][i] = t1.wrapping_sub(t2) as i32;
            }
        }
        off += SAMPLES_PER_BAND;
    }

    // Synthesis filter
    let mut dither_state = 0i32;
    for ch in 0..channels {
        if ch >= out.len() {
            break;
        }
        for i in 0..SAMPLES_PER_BAND {
            synth.filter(
                ch,
                &mut dither_state,
                &mut out[ch][32 * i..32 * i + 32],
                &sb_samples[ch][i],
            );
        }
    }
}
