//! Encoder delay and end padding, removed from decoded audio.
//!
//! Containers declare which decoded samples are not part of the
//! presentation: priming at the start (AAC encoder delay, Opus pre-skip, MP3
//! encoder delay) and padding at the end. Demuxers expose it per packet as
//! [`AudioTrim`] through `Demuxer::packet_metadata`, the counterpart of
//! FFmpeg's `AV_PKT_DATA_SKIP_SAMPLES`. A [`Trimmer`] removes it from what
//! the decoder returns, once, after decoding, the way libavcodec's
//! `decode.c` does: priming packets are still decoded, so the codec state is
//! established, and only their output is dropped. The player engine and
//! `refcheck` both trim through this crate, so a reference test checks the
//! samples the player plays.
//!
//! Per packet handed to the decoder ([`Trimmer::packet`]):
//! - a nonzero `skip_samples` replaces whatever start skip is still pending
//!   (libavcodec's rule) and comes off the next decoded samples, across as
//!   many frames as it covers: a USAC stream's 2220 samples of priming span
//!   three 1024-sample frames;
//! - `discard_padding` comes off the end of the packet's output: what the
//!   decoder returns after the packet is sent and before the next one is,
//!   or before the stream ends ([`Trimmer::finish`]), which also covers a
//!   decoder that returns its last frames only when flushed. Output that may
//!   still be padding is held back until then. Padding larger than the
//!   packet's output removes that output, not earlier packets';
//! - both counts are samples per channel at `sample_rate`, rescaled to the
//!   rate the decoder actually outputs (an SBR decoder doubles it). A trim
//!   without a rate is ignored.
//!
//! [`Trimmer::reset`] forgets all of it: after a seek the decoder starts over
//! and the demuxer says what the new position needs.
//!
//! Untrusted counts cost nothing in proportion to their size: a skip only
//! counts down, and padding holds back at most what the decoder returned for
//! one packet.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

use oxideav_core::AudioTrim;

/// Decoded audio a [`Trimmer`] cuts: consecutive samples (per channel) at
/// one rate.
pub trait Pcm: Sized {
    /// Samples per channel.
    fn samples(&self) -> usize;
    /// Samples per second per channel.
    fn rate(&self) -> u32;
    /// Removes the first `n` samples, `0 < n < samples()`; what remains
    /// starts `n` samples later.
    fn drop_front(&mut self, n: usize);
    /// Keeps the first `n` samples, `0 < n < samples()`, and returns the
    /// rest, which starts `n` samples later.
    fn split_off(&mut self, n: usize) -> Self;
}

/// A sample count, as declared or fixed at the output rate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Count {
    #[default]
    None,
    /// `count` samples at `rate`, as the container declared them.
    Declared { count: u32, rate: u32 },
    /// Samples at the decoder's output rate.
    Output(u64),
}

impl Count {
    fn declared(count: u32, rate: u32) -> Count {
        if count == 0 { Count::None } else { Count::Declared { count, rate } }
    }

    /// The count in samples at `rate`, which it stays fixed at.
    fn at(&mut self, rate: u32) -> u64 {
        let n = match *self {
            Count::None => 0,
            Count::Declared { count, rate: from } => rescale(count, from, rate),
            Count::Output(n) => n,
        };
        *self = if n == 0 { Count::None } else { Count::Output(n) };
        n
    }
}

/// `count` samples at `from` Hz as samples at `to` Hz, rounded to nearest
/// with halves away from zero (`av_rescale`).
fn rescale(count: u32, from: u32, to: u32) -> u64 {
    if from == to || to == 0 {
        return u64::from(count);
    }
    let n = (u128::from(count) * u128::from(to) + u128::from(from / 2)) / u128::from(from);
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Applies [`AudioTrim`]s to a decoder's output. See the crate docs.
pub struct Trimmer<P> {
    /// Leading samples still to skip.
    skip: Count,
    /// The end padding of the packet sent last.
    padding: Count,
    /// The newest output, while it may still be that padding.
    held: VecDeque<P>,
    held_samples: u64,
}

impl<P: Pcm> Default for Trimmer<P> {
    fn default() -> Self {
        Trimmer { skip: Count::None, padding: Count::None, held: VecDeque::new(), held_samples: 0 }
    }
}

impl<P: Pcm> Trimmer<P> {
    pub fn new() -> Self {
        Self::default()
    }

    /// A packet went to the decoder; `trim` is its `audio_trim`. The
    /// previous packet's output is complete, so its padding goes.
    pub fn packet(&mut self, trim: Option<AudioTrim>) {
        self.drop_held();
        self.padding = Count::None;
        let Some(trim) = trim.filter(|t| t.sample_rate > 0) else { return };
        if trim.skip_samples > 0 {
            self.skip = Count::declared(trim.skip_samples, trim.sample_rate);
        }
        self.padding = Count::declared(trim.discard_padding, trim.sample_rate);
    }

    /// One decoded frame, in output order. Appends what plays to `out`: the
    /// frame without its skipped start, and output held back as possible
    /// padding once later output shows it is not.
    pub fn frame(&mut self, mut pcm: P, out: &mut Vec<P>) {
        let n = pcm.samples() as u64;
        if n == 0 {
            out.push(pcm);
            return;
        }
        if self.skip != Count::None {
            let skip = self.skip.at(pcm.rate());
            if skip >= n {
                self.skip = if skip > n { Count::Output(skip - n) } else { Count::None };
                return;
            }
            pcm.drop_front(skip as usize);
            self.skip = Count::None;
        }
        if self.padding == Count::None {
            out.push(pcm);
            return;
        }
        let padding = self.padding.at(pcm.rate());
        self.held_samples += pcm.samples() as u64;
        self.held.push_back(pcm);
        while self.held_samples > padding {
            let excess = self.held_samples - padding;
            let Some(front) = self.held.front_mut() else { break };
            let len = front.samples() as u64;
            if len <= excess {
                self.held_samples -= len;
                out.extend(self.held.pop_front());
            } else {
                let rest = front.split_off(excess as usize);
                out.push(std::mem::replace(front, rest));
                self.held_samples -= excess;
            }
        }
    }

    /// The decoder is drained: what is held back is the stream's end
    /// padding.
    pub fn finish(&mut self) {
        self.drop_held();
        self.padding = Count::None;
    }

    /// The decoder starts over (a seek): nothing pending carries over.
    pub fn reset(&mut self) {
        self.skip = Count::None;
        self.finish();
    }

    fn drop_held(&mut self) {
        self.held.clear();
        self.held_samples = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Output sample indices at 48 kHz.
    #[derive(Debug, PartialEq)]
    struct Run(std::ops::Range<u64>);

    impl Pcm for Run {
        fn samples(&self) -> usize {
            (self.0.end - self.0.start) as usize
        }
        fn rate(&self) -> u32 {
            48000
        }
        fn drop_front(&mut self, n: usize) {
            self.0.start += n as u64;
        }
        fn split_off(&mut self, n: usize) -> Self {
            let at = self.0.start + n as u64;
            let rest = Run(at..self.0.end);
            self.0.end = at;
            rest
        }
    }

    fn trim(skip: u32, discard: u32) -> Option<AudioTrim> {
        Some(AudioTrim { skip_samples: skip, discard_padding: discard, sample_rate: 48000 })
    }

    /// Sends one packet per frame; returns what plays.
    fn run(t: &mut Trimmer<Run>, packets: &[(Option<AudioTrim>, std::ops::Range<u64>)]) -> Vec<Run> {
        let mut out = Vec::new();
        for (meta, frame) in packets {
            t.packet(*meta);
            t.frame(Run(frame.clone()), &mut out);
        }
        out
    }

    #[test]
    fn mid_stream_padding_goes_when_the_next_packet_is_sent() {
        // Matroska DiscardPadding on a Block before the last one.
        let mut t = Trimmer::new();
        let out = run(&mut t, &[(None, 0..1024), (trim(0, 100), 1024..2048), (None, 2048..3072)]);
        t.finish();
        assert_eq!(out, [Run(0..1024), Run(1024..1948), Run(2048..3072)]);
    }

    #[test]
    fn reset_forgets_a_pending_skip() {
        // 3000 samples of priming, one 1024-sample frame decoded...
        let mut t = Trimmer::new();
        assert_eq!(run(&mut t, &[(trim(3000, 0), 0..1024)]), []);
        // ...then a seek: the decoder restarts at a later packet, which
        // carries no trim and plays whole.
        t.reset();
        assert_eq!(run(&mut t, &[(None, 9216..10240)]), [Run(9216..10240)]);
    }

    #[test]
    fn empty_frames_pass_through_without_touching_the_skip() {
        let mut t = Trimmer::new();
        let mut out = Vec::new();
        t.packet(trim(100, 0));
        t.frame(Run(0..0), &mut out);
        t.frame(Run(0..1024), &mut out);
        assert_eq!(out, [Run(0..0), Run(100..1024)]);
    }
}
