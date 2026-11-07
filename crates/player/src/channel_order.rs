//! The decoders follow FFmpeg's channel order (speaker-mask bit order: FL FR
//! FC LFE BL BR FLC FRC BC SL SR ...). The core's [`ChannelLayout`] names a
//! different order for three layouts, so the engine rewrites those frames
//! once, and every consumer (sinks, stereo mix, captures) can trust
//! `layout.positions()`.

use oxideav_core::ChannelLayout;

/// Source channel, in FFmpeg's order, for each slot of `layout`'s core order.
/// `None` where both orders agree.
pub fn from_ffmpeg_order(layout: ChannelLayout) -> Option<&'static [u8]> {
    match layout {
        // core FL FR FC LFE SL SR BL BR, FFmpeg 7.1 FL FR FC LFE BL BR SL SR
        ChannelLayout::Surround71 => Some(&[0, 1, 2, 3, 6, 7, 4, 5]),
        // core FL FR FC SL SR BL BR, FFmpeg 7.0 FL FR FC BL BR SL SR
        ChannelLayout::Surround70 => Some(&[0, 1, 2, 5, 6, 3, 4]),
        // core FL FR FC BC LFE, FFmpeg 4.1 FL FR FC LFE BC
        ChannelLayout::Surround41 => Some(&[0, 1, 2, 4, 3]),
        _ => None,
    }
}

/// Rewrites interleaved frames in place so slot `i` carries source channel
/// `order[i]`. `order` has one entry per channel, at most 8.
pub fn reorder_interleaved(data: &mut [u8], sample_bytes: usize, order: &[u8]) {
    let stride = sample_bytes * order.len();
    let mut frame = [0u8; 64];
    assert!(stride <= frame.len(), "reorder of {} channels x {sample_bytes} bytes", order.len());
    for chunk in data.chunks_exact_mut(stride) {
        frame[..stride].copy_from_slice(chunk);
        for (slot, &src) in order.iter().enumerate() {
            let from = usize::from(src) * sample_bytes;
            chunk[slot * sample_bytes..(slot + 1) * sample_bytes].copy_from_slice(&frame[from..from + sample_bytes]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::ChannelPosition::*;

    #[test]
    fn ffmpeg_orders_map_onto_core_positions() {
        // FFmpeg's order for the same speakers; reordering it must give the core's positions.
        let cases = [
            (ChannelLayout::Surround71, [FrontLeft, FrontRight, FrontCenter, LowFrequency, BackLeft, BackRight, SideLeft, SideRight].as_slice()),
            (ChannelLayout::Surround70, &[FrontLeft, FrontRight, FrontCenter, BackLeft, BackRight, SideLeft, SideRight]),
            (ChannelLayout::Surround41, &[FrontLeft, FrontRight, FrontCenter, LowFrequency, BackCenter]),
        ];
        for (layout, ffmpeg) in cases {
            let order = from_ffmpeg_order(layout).unwrap();
            let mapped: Vec<_> = order.iter().map(|&i| ffmpeg[usize::from(i)]).collect();
            assert_eq!(mapped, layout.positions(), "{layout:?}");
        }
        assert_eq!(from_ffmpeg_order(ChannelLayout::Surround51), None);
    }

    #[test]
    fn reorder_interleaved_moves_whole_samples_per_frame() {
        // Two S16 frames tagged frame*100 + channel.
        let mut data: Vec<u8> = (0..2u16).flat_map(|f| (0..8u16).flat_map(move |c| (f * 100 + c).to_le_bytes())).collect();
        reorder_interleaved(&mut data, 2, &[0, 1, 2, 3, 6, 7, 4, 5]);
        let got: Vec<u16> = data.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(got, [0, 1, 2, 3, 6, 7, 4, 5, 100, 101, 102, 103, 106, 107, 104, 105]);
    }
}
