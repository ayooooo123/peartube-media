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
