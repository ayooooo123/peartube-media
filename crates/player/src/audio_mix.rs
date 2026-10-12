use oxideav_core::{ChannelLayout, ChannelPosition};

/// Stereo speaker mix in the core's canonical channel order. Center and side
/// channels use -3 dB, rear center -6 dB; LFE is omitted from the full-range mix.
/// One common gain preserves balance while reserving correlated-signal headroom.
pub(crate) struct StereoMix {
    channels: usize,
    gains: [[f32; 2]; 64],
}

impl StereoMix {
    pub(crate) fn new(layout: ChannelLayout) -> Result<Self, &'static str> {
        let channels = usize::from(layout.channel_count());
        if channels == 0 || channels > 64 { return Err("invalid audio channel count"); }
        let mut gains = [[0.0; 2]; 64];
        let surround = std::f32::consts::FRAC_1_SQRT_2;
        for (index, gain) in gains[..channels].iter_mut().enumerate() {
            use ChannelPosition::*;
            *gain = match layout.position(index) {
                Some(FrontLeft) => [1.0, 0.0],
                Some(FrontRight) => [0.0, 1.0],
                Some(FrontCenter) if channels == 1 => [1.0, 1.0],
                Some(FrontCenter) => [surround, surround],
                Some(SideLeft | BackLeft | FrontLeftOfCenter) => [surround, 0.0],
                Some(SideRight | BackRight | FrontRightOfCenter) => [0.0, surround],
                Some(BackCenter) => [0.5, 0.5],
                Some(LowFrequency) => [0.0, 0.0],
                // Unknown speaker positions have no honest left/right assignment.
                // Preserve their content equally rather than discard extra lanes.
                _ => [1.0 / channels as f32; 2],
            };
        }
        let mut sums = [0.0_f32; 2];
        for gain in &gains[..channels] {
            sums[0] += gain[0];
            sums[1] += gain[1];
        }
        let scale = 1.0 / sums[0].max(sums[1]).max(1.0);
        for gain in &mut gains[..channels] {
            gain[0] *= scale;
            gain[1] *= scale;
        }
        Ok(Self { channels, gains })
    }

    /// Returns input frames represented in `output`; never consumes a partial frame.
    pub(crate) fn mix(&self, pcm: &[f32], output: &mut [f32]) -> usize {
        let frames = (pcm.len() / self.channels).min(output.len() / 2);
        for (input, stereo) in pcm.chunks_exact(self.channels)
            .take(frames).zip(output.chunks_exact_mut(2))
        {
            let mut mixed = [0.0; 2];
            for (&sample, gain) in input.iter().zip(&self.gains) {
                mixed[0] += sample * gain[0];
                mixed[1] += sample * gain[1];
            }
            stereo.copy_from_slice(&mixed);
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::ChannelLayout;

    // Failure modes: lost center, swapped surrounds, LFE mistaken for dialogue,
    // correlated-channel clipping, stale/overrun scratch, and dropped unknown lanes.
    #[test]
    fn center_dialogue_reaches_both_stereo_channels() {
        for layout in [ChannelLayout::Surround30, ChannelLayout::Surround51, ChannelLayout::Surround71] {
            let mix = StereoMix::new(layout).unwrap();
            let mut input = vec![0.0; usize::from(layout.channel_count()) * 2];
            input[2] = 0.5;
            input[usize::from(layout.channel_count()) + 2] = -0.25;
            let mut output = [0.0; 4];
            assert_eq!(mix.mix(&input, &mut output), 2);
            assert!(output[0] > 0.1 && output[0] < 0.5);
            assert_eq!(output[0], output[1]);
            assert_eq!(output[2], output[3]);
            assert_eq!(output[2], -output[0] * 0.5);
        }
    }

    #[test]
    fn surround_side_and_center_positions_are_not_channel_count_guesses() {
        let mut out = [0.0; 2];
        StereoMix::new(ChannelLayout::Stereo21).unwrap().mix(&[0.0, 0.0, 1.0], &mut out);
        assert_eq!(out, [0.0, 0.0]); // LFE is not center dialogue.
        StereoMix::new(ChannelLayout::Surround30).unwrap().mix(&[0.0, 0.0, 1.0], &mut out);
        assert!(out[0] > 0.0 && out[0] == out[1]);
        let mix = StereoMix::new(ChannelLayout::Surround51).unwrap();
        mix.mix(&[0.0, 0.0, 0.0, 0.0, 0.5, 0.0], &mut out);
        assert!(out[0] > 0.0);
        assert_eq!(out[1], 0.0);
        mix.mix(&[0.0, 0.0, 0.0, 0.0, 0.0, 0.5], &mut out);
        assert_eq!(out[0], 0.0);
        assert!(out[1] > 0.0);
    }

    #[test]
    fn coherent_surround_input_has_headroom_and_bounded_frame_progress() {
        let mix = StereoMix::new(ChannelLayout::Surround71).unwrap();
        let mut output = [99.0; 5];
        assert_eq!(mix.mix(&[1.0; 24], &mut output), 2);
        for sample in &output[..4] { assert!((*sample - 1.0).abs() < 1e-6); }
        assert_eq!(output[4], 99.0);
        assert_eq!(mix.mix(&[-0.5; 8], &mut output), 1);
        assert!((output[0] + 0.5).abs() < 1e-6);
        assert_eq!(output[0], output[1]);
        assert_eq!(output[4], 99.0);
    }

    #[test]
    fn stereo_is_unchanged_and_discrete_channels_are_not_discarded() {
        let mut output = [0.0; 4];
        StereoMix::new(ChannelLayout::Stereo).unwrap().mix(&[0.25, -0.5, -0.75, 1.0], &mut output);
        assert_eq!(output, [0.25, -0.5, -0.75, 1.0]);
        let mix = StereoMix::new(ChannelLayout::DiscreteN(9)).unwrap();
        let mut input = [0.0; 9];
        input[8] = 0.9;
        assert_eq!(mix.mix(&input, &mut output), 1);
        assert!((output[0] - 0.1).abs() < 1e-6);
        assert_eq!(output[0], output[1]);
        assert!(StereoMix::new(ChannelLayout::DiscreteN(0)).is_err());
        assert!(StereoMix::new(ChannelLayout::DiscreteN(65)).is_err());
    }
}
