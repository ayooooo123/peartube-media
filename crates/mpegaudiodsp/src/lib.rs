//! FFmpeg's fixed-point MPEG audio synthesis filter: `dct32_fixed`, the
//! windowing with its dither state, and the per-channel ring buffer. The
//! MPEG audio (Layer I) and Musepack decoders share it.
//!
//! Ported from FFmpeg libavcodec/mpegaudiodsp.c, mpegaudiodsp_template.c,
//! dct32_template.c and mpegaudiodsp_data.c (commit 2da55bf),
//! LGPL-2.1-or-later (see LICENSE).
//! Copyright (c) 2001, 2002 Fabrice Bellard; Copyright (c) 2011 Mans Rullgard.

#![forbid(unsafe_code)]

pub const MPA_SYNTH_WINDOW_FIXED: [i32; 512] = [
    0, -1, -1, -1, -1, -1, -1, -2,
    -2, -2, -2, -3, -3, -4, -4, -5,
    -5, -6, -7, -7, -8, -9, -10, -11,
    -13, -14, -16, -17, -19, -21, -24, -26,
    -29, -31, -35, -38, -41, -45, -49, -53,
    -58, -63, -68, -73, -79, -85, -91, -97,
    -104, -111, -117, -125, -132, -139, -147, -154,
    -161, -169, -176, -183, -190, -196, -202, -208,
    213, 218, 222, 225, 227, 228, 228, 227,
    224, 221, 215, 208, 200, 189, 177, 163,
    146, 127, 106, 83, 57, 29, -2, -36,
    -72, -111, -153, -197, -244, -294, -347, -401,
    -459, -519, -581, -645, -711, -779, -848, -919,
    -991, -1064, -1137, -1210, -1283, -1356, -1428, -1498,
    -1567, -1634, -1698, -1759, -1817, -1870, -1919, -1962,
    -2001, -2032, -2057, -2075, -2085, -2087, -2080, -2063,
    2037, 2000, 1952, 1893, 1822, 1739, 1644, 1535,
    1414, 1280, 1131, 970, 794, 605, 402, 185,
    -45, -288, -545, -814, -1095, -1388, -1692, -2006,
    -2330, -2663, -3004, -3351, -3705, -4063, -4425, -4788,
    -5153, -5517, -5879, -6237, -6589, -6935, -7271, -7597,
    -7910, -8209, -8491, -8755, -8998, -9219, -9416, -9585,
    -9727, -9838, -9916, -9959, -9966, -9935, -9863, -9750,
    -9592, -9389, -9139, -8840, -8492, -8092, -7640, -7134,
    6574, 5959, 5288, 4561, 3776, 2935, 2037, 1082,
    70, -998, -2122, -3300, -4533, -5818, -7154, -8540,
    -9975, -11455, -12980, -14548, -16155, -17799, -19478, -21189,
    -22929, -24694, -26482, -28289, -30112, -31947, -33791, -35640,
    -37489, -39336, -41176, -43006, -44821, -46617, -48390, -50137,
    -51853, -53534, -55178, -56778, -58333, -59838, -61289, -62684,
    -64019, -65290, -66494, -67629, -68692, -69679, -70590, -71420,
    -72169, -72835, -73415, -73908, -74313, -74630, -74856, -74992,
    75038, 74992, 74856, 74630, 74313, 73908, 73415, 72835,
    72169, 71420, 70590, 69679, 68692, 67629, 66494, 65290,
    64019, 62684, 61289, 59838, 58333, 56778, 55178, 53534,
    51853, 50137, 48390, 46617, 44821, 43006, 41176, 39336,
    37489, 35640, 33791, 31947, 30112, 28289, 26482, 24694,
    22929, 21189, 19478, 17799, 16155, 14548, 12980, 11455,
    9975, 8540, 7154, 5818, 4533, 3300, 2122, 998,
    -70, -1082, -2037, -2935, -3776, -4561, -5288, -5959,
    6574, 7134, 7640, 8092, 8492, 8840, 9139, 9389,
    9592, 9750, 9863, 9935, 9966, 9959, 9916, 9838,
    9727, 9585, 9416, 9219, 8998, 8755, 8491, 8209,
    7910, 7597, 7271, 6935, 6589, 6237, 5879, 5517,
    5153, 4788, 4425, 4063, 3705, 3351, 3004, 2663,
    2330, 2006, 1692, 1388, 1095, 814, 545, 288,
    45, -185, -402, -605, -794, -970, -1131, -1280,
    -1414, -1535, -1644, -1739, -1822, -1893, -1952, -2000,
    2037, 2063, 2080, 2087, 2085, 2075, 2057, 2032,
    2001, 1962, 1919, 1870, 1817, 1759, 1698, 1634,
    1567, 1498, 1428, 1356, 1283, 1210, 1137, 1064,
    991, 919, 848, 779, 711, 645, 581, 519,
    459, 401, 347, 294, 244, 197, 153, 111,
    72, 36, 2, -29, -57, -83, -106, -127,
    -146, -163, -177, -189, -200, -208, -215, -221,
    -224, -227, -228, -228, -227, -225, -222, -218,
    213, 208, 202, 196, 190, 183, 176, 169,
    161, 154, 147, 139, 132, 125, 117, 111,
    104, 97, 91, 85, 79, 73, 68, 63,
    58, 53, 49, 45, 41, 38, 35, 31,
    29, 26, 24, 21, 19, 17, 16, 14,
    13, 11, 10, 9, 8, 7, 7, 6,
    5, 5, 4, 4, 3, 3, 2, 2,
    2, 2, 1, 1, 1, 1, 1, 1,
];

const COS0_0: i32 = 0x4013C251 as i32;
const COS0_1: i32 = 0x40B345BD as i32;
const COS0_2: i32 = 0x41FA2D6D as i32;
const COS0_3: i32 = 0x43F93421 as i32;
const COS0_4: i32 = 0x46CC1BC4 as i32;
const COS0_5: i32 = 0x4A9D9CF0 as i32;
const COS0_6: i32 = 0x4FAE3711 as i32;
const COS0_7: i32 = 0x56601EA7 as i32;
const COS0_8: i32 = 0x5F4CF6EB as i32;
const COS0_9: i32 = 0x6B6FCF26 as i32;
const COS0_10: i32 = 0x7C7D1DB3 as i32;
const COS0_11: i32 = 0x4AD81A97 as i32;
const COS0_12: i32 = 0x5EFC8D96 as i32;
const COS0_13: i32 = 0x41D95790 as i32;
const COS0_14: i32 = 0x6D0B20CF as i32;
const COS0_15: i32 = 0x518522FB as i32;

const COS1_0: i32 = 0x404F4672 as i32;
const COS1_1: i32 = 0x42E13C10 as i32;
const COS1_2: i32 = 0x48919F44 as i32;
const COS1_3: i32 = 0x52CB0E63 as i32;
const COS1_4: i32 = 0x64E2402E as i32;
const COS1_5: i32 = 0x43E224A9 as i32;
const COS1_6: i32 = 0x6E3C92C1 as i32;
const COS1_7: i32 = 0x519E4E04 as i32;

const COS2_0: i32 = 0x4140FB46 as i32;
const COS2_1: i32 = 0x4CF8DE88 as i32;
const COS2_2: i32 = 0x73326BBF as i32;
const COS2_3: i32 = 0x52036742 as i32;

const COS3_0: i32 = 0x4545E9EF as i32;
const COS3_1: i32 = 0x539EBA45 as i32;

const COS4_0: i32 = 0x5A82799A as i32;

#[inline]
fn mulh3(tmp1: u32, c: i32, s: u32) -> u32 {
    let scaled = (tmp1 as i32).wrapping_mul(1 << s);
    (((scaled as i64) * (c as i64)) >> 32) as u32
}

pub fn dct32_fixed(out: &mut [i32], tab: &[i32]) {
    let mut val = [0u32; 32];

    macro_rules! bf {
        ($a:expr, $b:expr, $c:expr, $s:expr) => {{
            let t0 = val[$a].wrapping_add(val[$b]);
            let t1 = val[$a].wrapping_sub(val[$b]);
            val[$a] = t0;
            val[$b] = mulh3(t1, $c, $s);
        }};
    }

    macro_rules! bf0 {
        ($a:expr, $b:expr, $ca:expr, $cb:expr, $c:expr, $s:expr) => {{
            let t0 = (tab[$ca] as u32).wrapping_add(tab[$cb] as u32);
            let t1 = (tab[$ca] as u32).wrapping_sub(tab[$cb] as u32);
            val[$a] = t0;
            val[$b] = mulh3(t1, $c, $s);
        }};
    }

    macro_rules! bf1 {
        ($a:expr, $b:expr, $c:expr, $d:expr) => {{
            bf!($a, $b, COS4_0, 1);
            bf!($c, $d, -COS4_0, 1);
            val[$c] = val[$c].wrapping_add(val[$d]);
        }};
    }

    macro_rules! bf2 {
        ($a:expr, $b:expr, $c:expr, $d:expr) => {{
            bf!($a, $b, COS4_0, 1);
            bf!($c, $d, -COS4_0, 1);
            val[$c] = val[$c].wrapping_add(val[$d]);
            val[$a] = val[$a].wrapping_add(val[$c]);
            val[$c] = val[$c].wrapping_add(val[$b]);
            val[$b] = val[$b].wrapping_add(val[$d]);
        }};
    }

    macro_rules! add {
        ($a:expr, $b:expr) => {{
            val[$a] = val[$a].wrapping_add(val[$b]);
        }};
    }

    /* pass 1 & 2 */
    bf0!(0, 31, 0, 31, COS0_0, 1);
    bf0!(15, 16, 15, 16, COS0_15, 5);
    bf!(0, 15, COS1_0, 1);
    bf!(16, 31, -COS1_0, 1);
    bf0!(7, 24, 7, 24, COS0_7, 1);
    bf0!(8, 23, 8, 23, COS0_8, 1);
    bf!(7, 8, COS1_7, 4);
    bf!(23, 24, -COS1_7, 4);
    bf!(0, 7, COS2_0, 1);
    bf!(8, 15, -COS2_0, 1);
    bf!(16, 23, COS2_0, 1);
    bf!(24, 31, -COS2_0, 1);
    bf0!(3, 28, 3, 28, COS0_3, 1);
    bf0!(12, 19, 12, 19, COS0_12, 2);
    bf!(3, 12, COS1_3, 1);
    bf!(19, 28, -COS1_3, 1);
    bf0!(4, 27, 4, 27, COS0_4, 1);
    bf0!(11, 20, 11, 20, COS0_11, 2);
    bf!(4, 11, COS1_4, 1);
    bf!(20, 27, -COS1_4, 1);
    bf!(3, 4, COS2_3, 3);
    bf!(11, 12, -COS2_3, 3);
    bf!(19, 20, COS2_3, 3);
    bf!(27, 28, -COS2_3, 3);
    bf!(0, 3, COS3_0, 1);
    bf!(4, 7, -COS3_0, 1);
    bf!(8, 11, COS3_0, 1);
    bf!(12, 15, -COS3_0, 1);
    bf!(16, 19, COS3_0, 1);
    bf!(20, 23, -COS3_0, 1);
    bf!(24, 27, COS3_0, 1);
    bf!(28, 31, -COS3_0, 1);

    bf0!(1, 30, 1, 30, COS0_1, 1);
    bf0!(14, 17, 14, 17, COS0_14, 3);
    bf!(1, 14, COS1_1, 1);
    bf!(17, 30, -COS1_1, 1);
    bf0!(6, 25, 6, 25, COS0_6, 1);
    bf0!(9, 22, 9, 22, COS0_9, 1);
    bf!(6, 9, COS1_6, 2);
    bf!(22, 25, -COS1_6, 2);
    bf!(1, 6, COS2_1, 1);
    bf!(9, 14, -COS2_1, 1);
    bf!(17, 22, COS2_1, 1);
    bf!(25, 30, -COS2_1, 1);

    bf0!(2, 29, 2, 29, COS0_2, 1);
    bf0!(13, 18, 13, 18, COS0_13, 3);
    bf!(2, 13, COS1_2, 1);
    bf!(18, 29, -COS1_2, 1);
    bf0!(5, 26, 5, 26, COS0_5, 1);
    bf0!(10, 21, 10, 21, COS0_10, 1);
    bf!(5, 10, COS1_5, 2);
    bf!(21, 26, -COS1_5, 2);
    bf!(2, 5, COS2_2, 1);
    bf!(10, 13, -COS2_2, 1);
    bf!(18, 21, COS2_2, 1);
    bf!(26, 29, -COS2_2, 1);
    bf!(1, 2, COS3_1, 2);
    bf!(5, 6, -COS3_1, 2);
    bf!(9, 10, COS3_1, 2);
    bf!(13, 14, -COS3_1, 2);
    bf!(17, 18, COS3_1, 2);
    bf!(21, 22, -COS3_1, 2);
    bf!(25, 26, COS3_1, 2);
    bf!(29, 30, -COS3_1, 2);

    /* pass 5 */
    bf1!(0, 1, 2, 3);
    bf2!(4, 5, 6, 7);
    bf1!(8, 9, 10, 11);
    bf2!(12, 13, 14, 15);
    bf1!(16, 17, 18, 19);
    bf2!(20, 21, 22, 23);
    bf1!(24, 25, 26, 27);
    bf2!(28, 29, 30, 31);

    /* pass 6 */
    add!(8, 12);
    add!(12, 10);
    add!(10, 14);
    add!(14, 9);
    add!(9, 13);
    add!(13, 11);
    add!(11, 15);

    out[0] = val[0] as i32;
    out[16] = val[1] as i32;
    out[8] = val[2] as i32;
    out[24] = val[3] as i32;
    out[4] = val[4] as i32;
    out[20] = val[5] as i32;
    out[12] = val[6] as i32;
    out[28] = val[7] as i32;
    out[2] = val[8] as i32;
    out[18] = val[9] as i32;
    out[10] = val[10] as i32;
    out[26] = val[11] as i32;
    out[6] = val[12] as i32;
    out[22] = val[13] as i32;
    out[14] = val[14] as i32;
    out[30] = val[15] as i32;

    add!(24, 28);
    add!(28, 26);
    add!(26, 30);
    add!(30, 25);
    add!(25, 29);
    add!(29, 27);
    add!(27, 31);

    out[1] = val[16].wrapping_add(val[24]) as i32;
    out[17] = val[17].wrapping_add(val[25]) as i32;
    out[9] = val[18].wrapping_add(val[26]) as i32;
    out[25] = val[19].wrapping_add(val[27]) as i32;
    out[5] = val[20].wrapping_add(val[28]) as i32;
    out[21] = val[21].wrapping_add(val[29]) as i32;
    out[13] = val[22].wrapping_add(val[30]) as i32;
    out[29] = val[23].wrapping_add(val[31]) as i32;
    out[3] = val[24].wrapping_add(val[20]) as i32;
    out[19] = val[25].wrapping_add(val[21]) as i32;
    out[11] = val[26].wrapping_add(val[22]) as i32;
    out[27] = val[27].wrapping_add(val[23]) as i32;
    out[7] = val[28].wrapping_add(val[18]) as i32;
    out[23] = val[29].wrapping_add(val[19]) as i32;
    out[15] = val[30].wrapping_add(val[17]) as i32;
    out[31] = val[31] as i32;
}

#[inline]
fn round_sample(sum: &mut i64) -> i16 {
    let sum1 = (*sum >> 24) as i32;
    *sum &= (1 << 24) - 1;
    sum1.clamp(-32768, 32767) as i16
}

pub fn apply_window_fixed(synth_buf: &mut [i32], dither_state: &mut i32, samples: &mut [i16]) {
    debug_assert!(synth_buf.len() >= 544);
    debug_assert!(samples.len() >= 32);

    let (src, dst) = synth_buf.split_at_mut(512);
    dst[..32].copy_from_slice(&src[..32]);

    let mut sum = *dither_state as i64;
    let window = &MPA_SYNTH_WINDOW_FIXED;

    for k in 0..8 {
        sum += (window[k * 64] as i64) * (synth_buf[16 + k * 64] as i64);
    }
    for k in 0..8 {
        sum -= (window[32 + k * 64] as i64) * (synth_buf[48 + k * 64] as i64);
    }

    samples[0] = round_sample(&mut sum);
    let mut w = 1usize;
    let mut w2 = 31usize;

    for j in 1..16 {
        let mut sum2 = 0i64;
        for k in 0..8 {
            let tmp = synth_buf[16 + j + k * 64] as i64;
            sum += (window[w + k * 64] as i64) * tmp;
            sum2 -= (window[w2 + k * 64] as i64) * tmp;
        }
        for k in 0..8 {
            let tmp = synth_buf[48 - j + k * 64] as i64;
            sum -= (window[w + 32 + k * 64] as i64) * tmp;
            sum2 -= (window[w2 + 32 + k * 64] as i64) * tmp;
        }

        samples[j] = round_sample(&mut sum);
        sum += sum2;
        samples[32 - j] = round_sample(&mut sum);
        w += 1;
        w2 -= 1;
    }

    for k in 0..8 {
        sum -= (window[w + 32 + k * 64] as i64) * (synth_buf[32 + k * 64] as i64);
    }

    samples[16] = round_sample(&mut sum);
    *dither_state = sum as i32;
}

#[derive(Clone)]
pub struct MpaSynth {
    pub synth_buf: [[i32; 1024]; 2],
    pub synth_buf_offset: [usize; 2],
}

impl Default for MpaSynth {
    fn default() -> Self {
        Self::new()
    }
}

impl MpaSynth {
    pub fn new() -> Self {
        Self {
            synth_buf: [[0; 1024]; 2],
            synth_buf_offset: [0; 2],
        }
    }

    pub fn reset(&mut self) {
        self.synth_buf = [[0; 1024]; 2];
        self.synth_buf_offset = [0; 2];
    }

    pub fn filter(
        &mut self,
        ch: usize,
        dither_state: &mut i32,
        out: &mut [i16],
        sb_samples: &[i32; 32],
    ) {
        let offset = self.synth_buf_offset[ch];
        dct32_fixed(&mut self.synth_buf[ch][offset..offset + 32], sb_samples);
        apply_window_fixed(&mut self.synth_buf[ch][offset..], dither_state, out);
        self.synth_buf_offset[ch] = (offset.wrapping_sub(32)) & 511;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dct32_known() {
        let in_vec: [i32; 32] = std::array::from_fn(|i| (i as i32) * 1000 - 15000);
        let mut out_vec = [0i32; 32];
        dct32_fixed(&mut out_vec, &in_vec);
        let expected_first8 = [16000, -207437, 0, -22983, 0, -8224, 0, -4153];
        assert_eq!(&out_vec[..8], &expected_first8);
    }

}
