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
//! - it is stamped with that packet's pts, except while a same-PTS span
//!   still has samples remaining (laced packets need duration/order too);
//! - otherwise, the current span has reached its packet's duration (at the
//!   output rate) and the oldest queued packet declares one: the spans
//!   follow the declared durations;
//! - otherwise, a later packet was sent after a packet without a duration:
//!   the frame starts the span of the packet sent last.
//! Frames crossing known packet boundaries are split before trimming.
//! Without reliable durations or distinct frame timestamps, delayed output
//! cannot always be associated. In particular an unstamped decoder that
//! both delays and silently drops output needs a decoder-side packet token;
//! this API cannot infer one from samples. The duration-less fallback is
//! for synchronous decoders, not proof of support for that ambiguous case.
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
//! Untrusted counts never drive an allocation or loop. At most
//! [`MAX_QUEUED`] packets may await output, and retained padding (including
//! PCM allocation capacities) is bounded by [`MAX_HELD_BYTES`]. Exceeding
//! either bound returns `InvalidData`, rather than losing packet identity.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

use oxideav_core::{AudioFormat, AudioFrame, AudioTrim, CodecParameters, Error, Packet, Result, SampleFormat};

/// Maximum number of packets awaiting decoder output.
pub const MAX_QUEUED: usize = 64;
/// Maximum retained PCM allocation bytes plus one object charge per piece.
pub const MAX_HELD_BYTES: usize = 32 * 1024 * 1024;

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
    /// Heap allocation capacity owned by this piece, in bytes.
    fn retained_bytes(&self) -> usize;
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
        let base = self.ticks as u128 * self.num as u128;
        // Overflow here implies a result above u64::MAX even after division
        // by the largest possible positive i64 denominator.
        let Some(n) = base.checked_mul(u128::from(rate)) else { return u64::MAX };
        let d = self.den as u128;
        u64::try_from(n.saturating_add(d / 2) / d).unwrap_or(u64::MAX)
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
    pts: Option<i64>,
    padding: Count,
    duration: Option<Duration>,
    /// Samples the decoder returned for the packet, skipped ones included.
    produced: u64,
    /// The newest output, while it may still be the padding.
    held: VecDeque<P>,
    held_samples: u64,
    held_bytes: usize,
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
    pub fn packet(&mut self, packet: &Packet, trim: Option<AudioTrim>) -> Result<()> {
        if self.queue.len() == MAX_QUEUED {
            return Err(Error::invalid("audio trim: too many packets awaiting output"));
        }
        let tb = packet.time_base;
        self.queue.push_back(Queued {
            trim: trim.filter(|t| t.sample_rate > 0),
            pts: packet.pts,
            duration: packet.duration.filter(|&d| d > 0).map(|ticks| Duration { ticks, num: tb.num(), den: tb.den() }),
        });
        Ok(())
    }

    /// One decoded frame, in output order, with the pts the decoder stamped
    /// it with. Appends what plays to `out`: the frame without its skipped
    /// start, and output held back as possible padding once it is known not
    /// to be.
    pub fn frame(&mut self, mut pcm: P, mut pts: Option<i64>, out: &mut Vec<P>) -> Result<()> {
        if pcm.samples() == 0 {
            out.push(pcm);
            return Ok(());
        }
        loop {
            let rate = pcm.rate();
            if let Some(index) = self.starting(pts, rate) {
                self.end_span();
                // Earlier packets produced no output, but their skip can
                // still cover the first samples that do arrive.
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
                self.current = Some(Span {
                    pts: q.pts, padding, duration: q.duration, produced: 0,
                    held: VecDeque::new(), held_samples: 0, held_bytes: 0,
                });
            }
            // A decoder can coalesce multiple packets into one frame. Cut
            // at known span boundaries, but leave the final span open: its
            // decoded tail can exceed a shortened container duration.
            let boundary = self.current.as_ref().and_then(|span| {
                (!self.queue.is_empty()).then_some(())?;
                let left = span.duration?.at(rate).checked_sub(span.produced)?;
                (left > 0 && left < pcm.samples() as u64).then_some(left as usize)
            });
            let rest = boundary.map(|n| pcm.split_off(n));
            self.apply(pcm, out)?;
            let Some(next) = rest else { return Ok(()) };
            pcm = next;
            pts = None;
        }
    }

    fn apply(&mut self, mut pcm: P, out: &mut Vec<P>) -> Result<()> {
        let n = pcm.samples() as u64;
        let rate = pcm.rate();
        let Some(span) = self.current.as_mut() else {
            out.push(pcm);
            return Ok(());
        };
        span.produced = span.produced.saturating_add(n);
        if self.skip != Count::None {
            let skip = self.skip.at(rate);
            if skip >= n {
                self.skip = if skip > n { Count::Output(skip - n) } else { Count::None };
                return Ok(());
            }
            pcm.drop_front(skip as usize);
            self.skip = Count::None;
        }
        if span.padding == Count::None {
            out.push(pcm);
            return Ok(());
        }
        let padding = span.padding.at(rate);
        let charge = |p: &P| p.retained_bytes().saturating_add(std::mem::size_of::<P>());
        span.held_samples += pcm.samples() as u64;
        span.held_bytes = span.held_bytes.saturating_add(charge(&pcm));
        span.held.push_back(pcm);
        while span.held_samples > padding {
            let excess = span.held_samples - padding;
            let Some(front) = span.held.front_mut() else { break };
            let len = front.samples() as u64;
            span.held_bytes -= charge(front);
            if len <= excess {
                span.held_samples -= len;
                out.extend(span.held.pop_front());
            } else {
                let rest = front.split_off(excess as usize);
                out.push(std::mem::replace(front, rest));
                span.held_bytes = span.held_bytes.saturating_add(charge(front));
                span.held_samples -= excess;
            }
        }
        if span.held_bytes > MAX_HELD_BYTES {
            self.reset();
            return Err(Error::invalid("audio trim: retained padding exceeds 32 MiB"));
        }
        Ok(())
    }

    /// The queued packet whose span a frame stamped `pts` at `rate` starts,
    /// if any (see the crate docs).
    fn starting(&self, pts: Option<i64>, rate: u32) -> Option<usize> {
        let last = self.queue.len().checked_sub(1)?;
        // Equal PTS are not packet identities: multiple laces and all the
        // frames split from one packet may carry the same timestamp.
        let continuing = self.current.as_ref().is_some_and(|span| {
            pts.is_some() && pts == span.pts && span.duration.is_some_and(|d| span.produced < d.at(rate))
        });
        if continuing {
            return None;
        }
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
        fn retained_bytes(&self) -> usize {
            self.samples().saturating_mul(4)
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
            t.packet(&packet(frame.start as i64), *meta).unwrap();
            t.frame(Run(frame.clone()), None, &mut out).unwrap();
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
        t.packet(&packet(0), trim(100, 0)).unwrap();
        t.frame(Run(0..0), None, &mut out).unwrap();
        t.frame(Run(0..1024), None, &mut out).unwrap();
        assert_eq!(out, [Run(0..0), Run(100..1024)]);
    }

    #[test]
    fn equal_pts_laces_keep_padding_on_their_own_split_output() {
        let mut t = Trimmer::new();
        let mut out = Vec::new();
        t.packet(&packet(0), trim(0, 700)).unwrap();
        t.packet(&packet(0), None).unwrap();
        t.frame(Run(0..512), Some(0), &mut out).unwrap();
        t.frame(Run(512..1024), Some(0), &mut out).unwrap();
        t.frame(Run(1024..2048), Some(0), &mut out).unwrap();
        t.finish();
        assert_eq!(out, [Run(0..324), Run(1024..2048)]);
    }

    #[test]
    fn a_frame_crossing_packet_spans_preserves_each_packets_padding() {
        let mut t = Trimmer::new();
        let mut out = Vec::new();
        t.packet(&packet(0), trim(0, 100)).unwrap();
        t.packet(&packet(1024), trim(0, 200)).unwrap();
        t.frame(Run(0..2048), Some(0), &mut out).unwrap();
        t.finish();
        assert_eq!(out, [Run(0..924), Run(1024..1848)]);
    }

    #[test]
    fn hostile_duration_does_not_overflow_or_move_padding_to_another_packet() {
        let mut t = Trimmer::new();
        let mut out = Vec::new();
        let mut p = packet(0);
        p.time_base = oxideav_core::TimeBase::new(i64::MAX, 1);
        p.duration = Some(i64::MAX);
        t.packet(&p, trim(0, 100)).unwrap();
        t.frame(Run(0..512), None, &mut out).unwrap();
        t.packet(&packet(1024), None).unwrap();
        t.frame(Run(512..1024), None, &mut out).unwrap();
        t.finish();
        assert_eq!(out.into_iter().flat_map(|run| run.0).collect::<Vec<_>>(), (0..924).collect::<Vec<_>>());
    }

    #[test]
    fn too_many_delayed_packets_are_rejected_without_losing_the_first_trim() {
        let mut t = Trimmer::new();
        let mut out = Vec::new();
        t.packet(&packet(0), trim(100, 0)).unwrap();
        for i in 1..MAX_QUEUED {
            t.packet(&packet(i as i64 * 1024), None).unwrap();
        }
        assert!(matches!(t.packet(&packet(65536), None), Err(Error::InvalidData(_))));
        t.frame(Run(0..1024), Some(0), &mut out).unwrap();
        assert_eq!(out, [Run(100..1024)]);
    }

    #[test]
    fn hostile_padding_is_rejected_at_the_memory_bound_and_can_reset() {
        let mut t = Trimmer::new();
        let mut out = Vec::new();
        t.packet(&packet(0), trim(0, u32::MAX)).unwrap();
        let samples = MAX_HELD_BYTES as u64 / 4;
        assert!(matches!(t.frame(Run(0..samples), None, &mut out), Err(Error::InvalidData(_))));
        assert!(out.is_empty(), "no padding reaches the sink");
        t.reset();
        t.packet(&packet(0), None).unwrap();
        t.frame(Run(0..1024), None, &mut out).unwrap();
        assert_eq!(out, [Run(0..1024)]);
    }
}
