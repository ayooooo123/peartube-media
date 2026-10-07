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
//! Each packet handed to the decoder ([`Trimmer::packet`]) owns a span of
//! the decoder's output, in packet order, wherever in time its frames
//! arrive. A decoder may return a packet's samples only after later packets
//! were sent (or at flush), and may return them in several frames. A frame
//! starts the span of a queued packet when:
//! - it is stamped with that packet's pts (decoders stamp frames with the
//!   pts of the packet they decoded; earlier queued packets produced no
//!   output);
//! - otherwise, the current span has reached its packet's duration (at the
//!   output rate) and the oldest queued packet declares one: the spans
//!   follow the declared durations;
//! - otherwise, a later packet was sent after a packet without a duration:
//!   the frame starts the span of the packet sent last.
//!
//! Without a later packet queued, frames continue the current span past
//! its duration (an MP4's last AAC frame decodes 1024 samples although its
//! sample lasts 261). Per span:
//! - a nonzero `skip_samples` replaces whatever start skip is still pending
//!   (libavcodec's rule) and comes off the decoded samples from the span's
//!   start on, across as many frames as it covers: a USAC stream's 2220
//!   samples of priming span three 1024-sample frames;
//! - `discard_padding` comes off the end of the span: output that may still
//!   be padding is held back until the next span starts or the stream ends
//!   ([`Trimmer::finish`]). Padding larger than the span removes the span,
//!   not earlier packets' output;
//! - both counts are samples per channel at `sample_rate`, rescaled to the
//!   rate the decoder actually outputs (an SBR decoder doubles it). A trim
//!   without a rate is ignored.
//!
//! [`Trimmer::reset`] forgets all of it: after a seek the decoder starts over
//! and the demuxer says what the new position needs.
//!
//! Untrusted counts cost nothing in proportion to their size: a skip only
//! counts down, padding holds back at most one span's output, and at most
//! [`MAX_QUEUED`] packets wait for their output (the oldest are dropped, as
//! packets that produced none).

#![forbid(unsafe_code)]

use std::collections::VecDeque;

use oxideav_core::{AudioFormat, AudioFrame, AudioTrim, CodecParameters, Packet, SampleFormat};

/// Packets that may still wait for their output; beyond this the oldest
/// produced none.
pub const MAX_QUEUED: usize = 64;

/// The layout a decoded frame is read (and trimmed) in: what the decoder
/// `reported` (`Decoder::output_audio_format`), else the container's
/// declaration corrected by the frame's actual plane count and byte length.
/// Containers often declare a different format, rate or channel count than
/// the decoder produces (HE-AAC, parametric stereo, S16 decoders), and
/// reading S16 bytes as f32 yields garbage and NaNs. Without a declared
/// rate, 48 kHz.
pub fn frame_layout(reported: Option<AudioFormat>, params: &CodecParameters, frame: &AudioFrame) -> AudioFormat {
    if let Some(f) = reported {
        return f;
    }
    let sample_rate = params.sample_rate.unwrap_or(48000);
    let declared = params.sample_format;
    let planar = frame.data.len() > 1;
    let channels = if planar { frame.data.len() as u16 } else { params.channels.unwrap_or(1).max(1) };
    let per_plane = if planar { 1 } else { channels as usize };
    let samples = (frame.samples as usize).max(1);
    let bytes = frame.data.first().map_or(0, Vec::len);
    let width = bytes / (samples * per_plane);
    let fits = |f: SampleFormat| f.is_planar() == planar && f.bytes_per_sample() == width;
    let sample_format = match declared {
        Some(f) if fits(f) => f,
        _ => {
            // 4-byte samples are f32 or s32: keep the declared family.
            let float = declared.map_or(true, |f| {
                matches!(f, SampleFormat::F32 | SampleFormat::F32P | SampleFormat::F64 | SampleFormat::F64P)
            });
            match (width, planar, float) {
                (1, false, _) => SampleFormat::U8,
                (1, true, _) => SampleFormat::U8P,
                (2, false, _) => SampleFormat::S16,
                (2, true, _) => SampleFormat::S16P,
                (3, false, _) => SampleFormat::S24,
                (4, false, true) => SampleFormat::F32,
                (4, false, false) => SampleFormat::S32,
                (4, true, true) => SampleFormat::F32P,
                (4, true, false) => SampleFormat::S32P,
                (8, false, _) => SampleFormat::F64,
                (8, true, _) => SampleFormat::F64P,
                _ => declared.unwrap_or(SampleFormat::F32),
            }
        }
    };
    AudioFormat { sample_format, sample_rate, channels }
}

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

/// A packet's duration in its time base: its span's length when declared.
#[derive(Clone, Copy, Debug)]
struct Duration {
    ticks: i64,
    num: i64,
    den: i64,
}

impl Duration {
    /// In samples at `rate`, rounded to nearest; 0 for a meaningless time
    /// base.
    fn at(&self, rate: u32) -> u64 {
        if self.ticks <= 0 || self.num <= 0 || self.den <= 0 {
            return 0;
        }
        let n = i128::from(self.ticks) * i128::from(self.num) * i128::from(rate);
        let d = i128::from(self.den);
        u64::try_from((n + d / 2) / d).unwrap_or(u64::MAX)
    }
}

/// A packet sent to the decoder whose output has not started.
#[derive(Clone, Copy, Debug)]
struct Queued {
    trim: Option<AudioTrim>,
    pts: Option<i64>,
    duration: Option<Duration>,
}

/// The output of one packet so far.
struct Span<P> {
    padding: Count,
    duration: Option<Duration>,
    /// Samples the decoder returned for the packet, skipped ones included.
    produced: u64,
    /// The newest output, while it may still be the padding.
    held: VecDeque<P>,
    held_samples: u64,
}

/// Applies [`AudioTrim`]s to a decoder's output. See the crate docs.
pub struct Trimmer<P> {
    /// Leading samples still to skip.
    skip: Count,
    queue: VecDeque<Queued>,
    current: Option<Span<P>>,
}

impl<P: Pcm> Default for Trimmer<P> {
    fn default() -> Self {
        Trimmer { skip: Count::None, queue: VecDeque::new(), current: None }
    }
}

impl<P: Pcm> Trimmer<P> {
    pub fn new() -> Self {
        Self::default()
    }

    /// `packet` went to the decoder; `trim` is its `audio_trim`. Its output
    /// may come later (see the crate docs).
    pub fn packet(&mut self, packet: &Packet, trim: Option<AudioTrim>) {
        if self.queue.len() == MAX_QUEUED {
            self.queue.pop_front();
        }
        let tb = packet.time_base;
        self.queue.push_back(Queued {
            trim: trim.filter(|t| t.sample_rate > 0),
            pts: packet.pts,
            duration: packet.duration.filter(|&d| d > 0).map(|ticks| Duration { ticks, num: tb.num(), den: tb.den() }),
        });
    }

    /// One decoded frame, in output order, with the pts the decoder stamped
    /// it with. Appends what plays to `out`: the frame without its skipped
    /// start, and output held back as possible padding once it is known not
    /// to be.
    pub fn frame(&mut self, mut pcm: P, pts: Option<i64>, out: &mut Vec<P>) {
        let n = pcm.samples() as u64;
        if n == 0 {
            out.push(pcm);
            return;
        }
        let rate = pcm.rate();
        if let Some(index) = self.starting(pts, rate) {
            self.end_span();
            // Packets before it produced no output; their skips still
            // count, as libavcodec's would for the frames they lack.
            for q in self.queue.drain(..index) {
                if let Some(t) = q.trim.filter(|t| t.skip_samples > 0) {
                    self.skip = Count::declared(t.skip_samples, t.sample_rate);
                }
            }
            let q = self.queue.pop_front().expect("`starting` names a queued packet");
            let (skip, padding) = q.trim.map_or((Count::None, Count::None), |t| {
                (Count::declared(t.skip_samples, t.sample_rate), Count::declared(t.discard_padding, t.sample_rate))
            });
            if skip != Count::None {
                self.skip = skip;
            }
            self.current =
                Some(Span { padding, duration: q.duration, produced: 0, held: VecDeque::new(), held_samples: 0 });
        }
        let Some(span) = self.current.as_mut() else {
            // No packet sent since the decoder (re)started: nothing to trim.
            out.push(pcm);
            return;
        };
        span.produced = span.produced.saturating_add(n);
        if self.skip != Count::None {
            let skip = self.skip.at(rate);
            if skip >= n {
                self.skip = if skip > n { Count::Output(skip - n) } else { Count::None };
                return;
            }
            pcm.drop_front(skip as usize);
            self.skip = Count::None;
        }
        if span.padding == Count::None {
            out.push(pcm);
            return;
        }
        let padding = span.padding.at(rate);
        span.held_samples += pcm.samples() as u64;
        span.held.push_back(pcm);
        while span.held_samples > padding {
            let excess = span.held_samples - padding;
            let Some(front) = span.held.front_mut() else { break };
            let len = front.samples() as u64;
            if len <= excess {
                span.held_samples -= len;
                out.extend(span.held.pop_front());
            } else {
                let rest = front.split_off(excess as usize);
                out.push(std::mem::replace(front, rest));
                span.held_samples -= excess;
            }
        }
    }

    /// The queued packet whose span a frame stamped `pts` at `rate` starts,
    /// if any (see the crate docs).
    fn starting(&self, pts: Option<i64>, rate: u32) -> Option<usize> {
        let last = self.queue.len().checked_sub(1)?;
        if let Some(index) = pts.and_then(|p| self.queue.iter().position(|q| q.pts == Some(p))) {
            return Some(index);
        }
        let complete = self.current.as_ref().is_none_or(|span| match span.duration {
            Some(d) => span.produced >= d.at(rate),
            None => true,
        });
        if !complete {
            None
        } else if self.queue[0].duration.is_some() {
            Some(0)
        } else {
            Some(last)
        }
    }

    /// The current span is over: what it held back is its padding.
    fn end_span(&mut self) {
        self.current = None;
    }

    /// The decoder is drained: what the last span holds back is the
    /// stream's end padding, and queued packets produced nothing.
    pub fn finish(&mut self) {
        self.end_span();
        self.queue.clear();
    }

    /// The decoder starts over (a seek): nothing pending carries over.
    pub fn reset(&mut self) {
        self.skip = Count::None;
        self.finish();
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

    /// A 48 kHz packet at `pts`, 1024 samples long.
    fn packet(pts: i64) -> Packet {
        let mut p = Packet::new(0, oxideav_core::TimeBase::new(1, 48000), Vec::new());
        p.pts = Some(pts);
        p.duration = Some(1024);
        p
    }

    /// Sends one packet per frame (the decoder stamps no pts); returns what
    /// plays.
    fn run(t: &mut Trimmer<Run>, packets: &[(Option<AudioTrim>, std::ops::Range<u64>)]) -> Vec<Run> {
        let mut out = Vec::new();
        for (meta, frame) in packets {
            t.packet(&packet(frame.start as i64), *meta);
            t.frame(Run(frame.clone()), None, &mut out);
        }
        out
    }

    #[test]
    fn mid_stream_padding_goes_when_the_next_packets_output_starts() {
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
        t.packet(&packet(0), trim(100, 0));
        t.frame(Run(0..0), None, &mut out);
        t.frame(Run(0..1024), None, &mut out);
        assert_eq!(out, [Run(0..0), Run(100..1024)]);
    }
}
