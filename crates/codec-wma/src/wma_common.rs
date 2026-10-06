// Ported from FFmpeg (commit 2da55bf): libavcodec/wma_common.c, libavcodec/wma_common.h
// GNU Lesser General Public License 2.1 or later

/// Get the samples per frame for this stream (log2).
pub fn wma_get_frame_len_bits(sample_rate: u32, version: u32, decode_flags: u32) -> u32 {
    let mut frame_len_bits: u32 = if sample_rate <= 16000 {
        9
    } else if sample_rate <= 22050 || (sample_rate <= 32000 && version == 1) {
        10
    } else if sample_rate <= 48000 || version < 3 {
        11
    } else if sample_rate <= 96000 {
        12
    } else {
        13
    };

    if version == 3 {
        let tmp = decode_flags & 0x6;
        if tmp == 0x2 {
            frame_len_bits += 1;
        } else if tmp == 0x4 {
            frame_len_bits = frame_len_bits.saturating_sub(1);
        } else if tmp == 0x6 {
            frame_len_bits = frame_len_bits.saturating_sub(2);
        }
    }

    frame_len_bits
}

/// `av_log2` (libavutil/common.h): floor(log2(v)), with `av_log2(0) == 0`.
#[inline]
pub fn av_log2(v: u32) -> u32 {
    (v | 1).ilog2()
}

/// `av_ceil_log2` (libavutil/common.h): ceil(log2(x)), 0 for x <= 1.
#[inline]
pub fn av_ceil_log2(x: u32) -> u32 {
    av_log2(x.wrapping_sub(1) << 1)
}

/// The leading-sample discard of `discard_samples` (libavcodec/decode.c) for
/// decoders that set `avctx->delay` / `internal->skip_samples`: frames that
/// `skip` covers are dropped whole, the next one loses its head. Returns
/// false when the frame is dropped. `bytes_per_sample` is per plane (the
/// frames are planar).
pub fn discard_samples(skip: &mut usize, frame: &mut oxideav_core::AudioFrame, bytes_per_sample: usize) -> bool {
    if *skip == 0 {
        return true;
    }
    let n = frame.samples as usize;
    if n <= *skip {
        *skip -= n;
        return false;
    }
    for plane in frame.data.iter_mut() {
        plane.drain(..*skip * bytes_per_sample);
    }
    frame.samples -= *skip as u32;
    *skip = 0;
    true
}
