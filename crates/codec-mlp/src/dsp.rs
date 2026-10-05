// Ported from FFmpeg libavcodec/mlpdsp.c and libavcodec/mlpdsp.h
// (commit 2da55bf). Licensed under LGPL-2.1-or-later.

//! The MLP inner loop: prediction filtering, rematrixing and output packing.
//! Integer-exact port — the fixed-point arithmetic must match FFmpeg
//! bit-for-bit for the lossless reference tests to pass.

use crate::common::{MAX_CHANNELS, MAX_FIR_ORDER, MAX_IIR_ORDER};

/// `msb_mask(bits) = -(1 << bits)` as a wrapping i32 mask.
#[inline]
pub fn msb_mask(bits: u32) -> i32 {
    (1i32).wrapping_shl(bits).wrapping_neg()
}

/// `mlp_filter_channel`: run one channel's block through the FIR/IIR
/// prediction filters, updating filter state.
///
/// `state` is `firbuf` followed by `iirbuf` in one array of
/// `MAX_BLOCKSIZE + MAX_FIR_ORDER` entries (FFmpeg splits one
/// `[2][MAX_BLOCKSIZE + MAX_FIR_ORDER]` buffer). `samples` is the
/// block-strided column of the sample buffer for this channel.
#[allow(clippy::too_many_arguments)]
pub fn mlp_filter_channel(
    firbuf: &mut [i32],
    iirbuf: &mut [i32],
    fircoeff: &[i32; MAX_FIR_ORDER],
    iircoeff: &[i32; MAX_FIR_ORDER],
    firorder: usize,
    iirorder: usize,
    filter_shift: u32,
    mask: i32,
    blocksize: usize,
    // (block, sample) indexing: row-major [MAX_BLOCKSIZE][MAX_CHANNELS]
    sample_buffer: &mut [[i32; MAX_CHANNELS]],
    channel: usize,
    blockpos: usize,
) {
    // FFmpeg walks `sample_buffer += MAX_CHANNELS` per sample from
    // &sample_buffer[blockpos][channel].
    for i in 0..blocksize {
        let residual = sample_buffer[blockpos + i][channel];
        let mut accum: i64 = 0;

        // FFmpeg: *--firbuf walks DOWN from firbuf = state + MAX_BLOCKSIZE,
        // i.e. the newest samples sit below MAX_BLOCKSIZE and the filter
        // state above. We model firbuf[i] = state[MAX_BLOCKSIZE + i] where
        // index 0 is the most recent sample (see the caller's buffer setup).
        for order in 0..firorder {
            accum += (firbuf[order] as i64) * (fircoeff[order] as i64);
        }
        for order in 0..iirorder {
            accum += (iirbuf[order] as i64) * (iircoeff[order] as i64);
        }

        accum >>= filter_shift;
        let result = (accum as i32).wrapping_add(residual) & mask;

        // FFmpeg's pointer walk (*--firbuf) shifts the WHOLE window every
        // sample, regardless of the active filter order — the saved state
        // must reproduce that, or a later order increase would see a
        // different history than FFmpeg's.
        for order in (1..MAX_FIR_ORDER).rev() {
            firbuf[order] = firbuf[order - 1];
        }
        firbuf[0] = result;
        let diff = result.wrapping_sub(accum as i32);
        for order in (1..MAX_IIR_ORDER).rev() {
            iirbuf[order] = iirbuf[order - 1];
        }
        iirbuf[0] = diff;

        sample_buffer[blockpos + i][channel] = result;
    }
}

/// `ff_mlp_rematrix_channel`: apply one primitive matrix to the samples.
///
/// `samples` is the whole sample buffer `[MAX_BLOCKSIZE][MAX_CHANNELS]`;
/// `bypassed_lsbs` likewise (LSBs for matrix `mat` are at index `mat`).
/// `index` is FFmpeg's `num_primitive_matrices - mat` (drives the noise
/// stepping).
#[allow(clippy::too_many_arguments)]
pub fn mlp_rematrix_channel(
    samples: &mut [[i32; MAX_CHANNELS]],
    coeffs: &[i32; MAX_CHANNELS],
    bypassed_lsbs: &[[u8; MAX_CHANNELS]],
    mat: usize,
    noise_buffer: &[i8],
    mut index: usize,
    dest_ch: usize,
    blockpos: usize,
    maxchan: usize,
    matrix_noise_shift: u8,
    access_unit_size_pow2: usize,
    mask: i32,
) {
    let index2 = 2 * index + 1;
    for i in 0..blockpos {
        let mut accum: i64 = 0;

        for src_ch in 0..=maxchan {
            accum += (samples[i][src_ch] as i64) * (coeffs[src_ch] as i64);
        }

        if matrix_noise_shift != 0 {
            index &= access_unit_size_pow2 - 1;
            accum += (noise_buffer[index] as i64) * (1 << (matrix_noise_shift as u32 + 7));
            index += index2;
        }

        samples[i][dest_ch] = (((accum >> 14) as i32) & mask) + bypassed_lsbs[i][mat] as i32;
    }
}

/// `ff_mlp_pack_output`: interleave the sample buffer into `data` with the
/// `ch_assign` channel permutation, applying output shifts, and update the
/// running lossless check data.
#[allow(clippy::too_many_arguments)]
pub fn pack_output(
    mut lossless_check_data: i32,
    blockpos: usize,
    sample_buffer: &[[i32; MAX_CHANNELS]],
    data: &mut [u8],
    ch_assign: &[u8; MAX_CHANNELS],
    output_shift: &[i8; MAX_CHANNELS],
    max_matrix_channel: usize,
    is32: bool,
) -> i32 {
    let mut w = 0usize;
    for i in 0..blockpos {
        for out_ch in 0..=max_matrix_channel {
            let mat_ch = ch_assign[out_ch] as usize;
            let sample = sample_buffer[i][mat_ch]
                .wrapping_mul(1u32.wrapping_shl(output_shift[mat_ch] as u32) as i32);
            lossless_check_data ^= (sample & 0xff_ffff).wrapping_shl(mat_ch as u32);
            if is32 {
                // FFmpeg: *data_32++ = sample * 256U (wrapping 32-bit).
                let v = sample.wrapping_mul(256).to_le_bytes();
                data[w..w + 4].copy_from_slice(&v);
                w += 4;
            } else {
                // FFmpeg: *data_16++ = sample >> 8.
                let v = ((sample >> 8) as i16).to_le_bytes();
                data[w..w + 2].copy_from_slice(&v);
                w += 2;
            }
        }
    }
    lossless_check_data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_output_matches_ffmpeg_semantics() {
        // Two samples, stereo, no shift: S16 packing little-endian.
        let mut sb = [[0i32; MAX_CHANNELS]; 16];
        sb[0][0] = 0x1234;
        sb[0][1] = -2;
        let mut data = [0u8; 4];
        let lc = pack_output(
            -1i32,
            1,
            &sb,
            &mut data,
            &[0, 1, 0, 0, 0, 0, 0, 0],
            &[0; MAX_CHANNELS],
            1,
            false,
        );
        // FFmpeg packs the TOP 16 bits of the 24-bit sample into s16:
        // 0x1234 >> 8 = 0x12; -2 >> 8 = -1.
        assert_eq!(&data, &[0x12, 0x00, 0xff, 0xff]);
        // lossless check: XOR of (sample & 0xffffff) << mat_ch
        assert_eq!(
            lc as u32,
            0xffffffffu32 ^ (0x1234u32 << 0) ^ ((-2i32 as u32 & 0xffffff) << 1)
        );
    }

    #[test]
    fn filter_passthrough() {
        // Zero-order filters: output = residual & mask; state records it.
        let mut firbuf = [0i32; MAX_FIR_ORDER];
        let mut iirbuf = [0i32; MAX_FIR_ORDER];
        let mut sb = [[0i32; MAX_CHANNELS]; 16];
        mlp_filter_channel(
            &mut firbuf, &mut iirbuf,
            &[0; MAX_FIR_ORDER], &[0; MAX_FIR_ORDER],
            0, 0, 0, !0, 2, &mut sb, 0, 0,
        );
        // mask = -1 → identity
    }
}
