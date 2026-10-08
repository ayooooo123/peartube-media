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
//!   samples of priming span three 1024-sample frames. The first sample
//!   after it begins the presentation ([`Pcm::begins_presentation`]), even
//!   where the container's timestamps put it before zero;
//! - `discard_padding` comes off the end of the span: output that may still
//!   be padding is held back until the next span starts, the decoder is
//!   drained ([`Trimmer::drain`]) or the stream ends ([`Trimmer::finish`]).
//!   Padding larger than the samples left after priming is ignored, as in
//!   FFmpeg `decode.c`;
//! - both counts are samples per channel at `sample_rate`, rescaled to the
//!   rate the decoder actually outputs (an SBR decoder doubles it). A trim
//!   without a rate is ignored.
//!
//! Draining the decoder at the end of the stream ([`Trimmer::drain`], before
//! `Decoder::flush`) ends the last span. libavcodec returns what a drain
//! yields with the side data of the packet sent last (`last_pkt_props`), so
//! each drained frame is trimmed on its own by that packet's trims: a
//! nonzero skip starts again, and the padding comes off the frame's end only
//! when the frame is at least that long (`decode.c`). An Opus stream whose
//! last packet discards 570 samples keeps the 24 SILK samples its decoder's
//! resampler drains.
//!
//! A decoder's own start delay is a default skip that a container's skip
//! replaces, as libavcodec seeds `skip_samples` with `AVCodecContext::delay`
//! when a decoder opens. OxideAV's Opus decoder removes its OpusHead
//! pre-skip from its own output instead, so a container's skip (an MP4 edit
//! list, Matroska CodecDelay) would come off on top of it:
//! [`take_decoder_delay`] moves that delay out of the decoder's parameters
//! and into [`Trimmer::with_decoder_delay`]. OxideAV's Vorbis decoder never
//! outputs its first packet, the frame FFmpeg's outputs and drops as its
//! delay, so that packet's trims are already applied
//! ([`DecoderDelay::FirstPacket`]).
//!
//! [`Trimmer::reset`] forgets all of it: after a seek the decoder starts over
//! and the demuxer says what the new position needs. A decoder's delay does
//! not come back; libavcodec does not apply it again on a flush either. A
//! decoder that drops its first packet drops it again, as FFmpeg's Vorbis
//! decoder does after a flush.
//!
//! Untrusted counts never drive an allocation or loop. At most
//! [`MAX_QUEUED`] packets may await output, and retained padding (including
//! PCM allocation capacities) is bounded by [`MAX_HELD_BYTES`]. Neither
//! limit stops the audio; both are counted in [`Fallbacks`]. A full queue
//! drops its oldest packet and that packet's trims. Until a frame's
//! timestamp names a queued packet again, or a reset, frames then play
//! untrimmed rather than take a newer packet's trims by duration or order.
//! Retained padding past the limit plays untrimmed.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

use oxideav_core::{AudioFormat, AudioFrame, AudioTrim, CodecParameters, Packet, SampleFormat};

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
    /// This piece's first sample is the first after a start skip: the
    /// presentation the container (or decoder) declared begins here,
    /// whatever its timestamp says.
    fn begins_presentation(&mut self) {}
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

/// Trim work that could not be applied: the audio plays on untrimmed. A
/// caller reports it as a mismatch, not as a decoder failure. Counts
/// saturate and survive a seek.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fallbacks {
    /// Packets dropped from a full queue, and with them their trims.
    pub lost_packets: u64,
    /// Spans whose retained padding passed [`MAX_HELD_BYTES`] and played.
    pub released_padding_spans: u64,
}

impl Fallbacks {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }

    pub fn add(&mut self, other: Self) {
        self.lost_packets = self.lost_packets.saturating_add(other.lost_packets);
        self.released_padding_spans = self.released_padding_spans.saturating_add(other.released_padding_spans);
    }
}

/// A decoder's own start delay, which a container's skip replaces (see the
/// crate docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecoderDelay {
    #[default]
    None,
    /// Skipped from the decoder's first output: the OpusHead pre-skip (RFC
    /// 7845 §5.1), in 48 kHz samples, which FFmpeg's decoder declares as
    /// `AVCodecContext::delay` and OxideAV's would remove itself.
    Skip(AudioTrim),
    /// The decoder never outputs the first packet after it opens or resets:
    /// OxideAV's Vorbis decoder needs a successor packet to overlap (Vorbis I
    /// §4.3.8). FFmpeg's (`vorbisdec.c`) outputs that frame and drops it
    /// through `skip_samples`, which a container's skip on the packet
    /// replaces. FFmpeg's encoders declare exactly that frame (libvorbis: the
    /// first packet's duration; its own encoder: half a long block), so the
    /// packet's trims are already applied. A skip that differs from the
    /// frame is not corrected.
    FirstPacket,
}

/// Moves a decoder's own start delay out of `params`, the parameters the
/// decoder is made from, into the [`DecoderDelay`] a [`Trimmer`] starts with
/// ([`Trimmer::with_decoder_delay`]).
pub fn take_decoder_delay(params: &mut CodecParameters) -> DecoderDelay {
    match params.codec_id.as_str() {
        "vorbis" => DecoderDelay::FirstPacket,
        "opus" => {
            let Some(head) = params.extradata.get_mut(..12).filter(|head| head.starts_with(b"OpusHead")) else {
                return DecoderDelay::None;
            };
            let pre_skip = u16::from_le_bytes([head[10], head[11]]);
            head[10..12].fill(0);
            match pre_skip {
                0 => DecoderDelay::None,
                n => DecoderDelay::Skip(AudioTrim { skip_samples: u32::from(n), discard_padding: 0, sample_rate: 48_000 }),
            }
        }
        _ => DecoderDelay::None,
    }
}

/// Applies [`AudioTrim`]s to a decoder's output. See the crate docs.
pub struct Trimmer<P> {
    /// Leading samples still to skip.
    skip: Count,
    /// A start skip ended exactly where the last output ended: the next
    /// output begins the presentation.
    skip_ended: bool,
    queue: VecDeque<Queued>,
    current: Option<Span<P>>,
    /// Duration order cannot identify output after a queued packet is lost.
    association_lost: bool,
    fallbacks: Fallbacks,
    /// The decoder never outputs the first packet after it opens or resets
    /// ([`DecoderDelay::FirstPacket`]).
    drops_first_packet: bool,
    /// That packet has not been sent yet.
    awaiting_first_packet: bool,
    /// The trims of the packet sent last, which libavcodec stamps on the
    /// frames a drain returns (`last_pkt_props`).
    last_sent: Option<AudioTrim>,
    /// The decoder is being drained ([`Trimmer::drain`]).
    draining: bool,
}

impl<P: Pcm> Default for Trimmer<P> {
    fn default() -> Self {
        Trimmer {
            skip: Count::None, skip_ended: false, queue: VecDeque::new(), current: None,
            association_lost: false, fallbacks: Fallbacks::default(),
            drops_first_packet: false, awaiting_first_packet: false, last_sent: None, draining: false,
        }
    }
}

impl<P: Pcm> Trimmer<P> {
    pub fn new() -> Self {
        Self::default()
    }

    /// A trimmer for a decoder with its own start delay
    /// ([`take_decoder_delay`]): a skip comes off its first output unless a
    /// container's nonzero skip replaces it first.
    pub fn with_decoder_delay(delay: DecoderDelay) -> Self {
        let mut trimmer = Self::default();
        match delay {
            DecoderDelay::None => {}
            DecoderDelay::Skip(t) => {
                if t.sample_rate > 0 {
                    trimmer.skip = Count::declared(t.skip_samples, t.sample_rate);
                }
            }
            DecoderDelay::FirstPacket => {
                trimmer.drops_first_packet = true;
                trimmer.awaiting_first_packet = true;
            }
        }
        trimmer
    }

    /// Counters since the caller last collected them. This does not alter
    /// pending audio or packet association.
    pub fn take_fallbacks(&mut self) -> Fallbacks {
        std::mem::take(&mut self.fallbacks)
    }

    /// `packet` went to the decoder; `trim` is its `audio_trim`. Its output
    /// may come later (see the crate docs).
    pub fn packet(&mut self, packet: &Packet, trim: Option<AudioTrim>) {
        let trim = trim.filter(|t| t.sample_rate > 0);
        self.last_sent = trim;
        if std::mem::take(&mut self.awaiting_first_packet) {
            // Its output never comes, and its trims are the decoder's.
            return;
        }
        if self.queue.len() == MAX_QUEUED {
            self.queue.pop_front();
            self.association_lost = true;
            self.fallbacks.lost_packets = self.fallbacks.lost_packets.saturating_add(1);
        }
        let tb = packet.time_base;
        self.queue.push_back(Queued {
            trim,
            pts: packet.pts,
            duration: packet.duration.filter(|&d| d > 0).map(|ticks| Duration { ticks, num: tb.num(), den: tb.den() }),
        });
    }

    /// One decoded frame, in output order, with the pts the decoder stamped
    /// it with. Appends what plays to `out`: the frame without its skipped
    /// start, and output held back as possible padding once it is known not
    /// to be.
    pub fn frame(&mut self, mut pcm: P, mut pts: Option<i64>, out: &mut Vec<P>) {
        if pcm.samples() == 0 {
            out.push(pcm);
            return;
        }
        if self.draining {
            self.drained(pcm, out);
            return;
        }
        loop {
            let rate = pcm.rate();
            if let Some(index) = self.starting(pts, rate) {
                self.end_span(out);
                self.association_lost = false;
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
            } else if self.association_lost
                && self.current.as_ref().is_none_or(|span| pts.is_none() || pts != span.pts)
            {
                // A late frame may belong to an evicted packet. Do not
                // attach a newer packet's trims to it by duration/order.
                if let Some(span) = self.current.take() {
                    out.extend(span.held);
                }
                self.skip = Count::None;
                self.skip_ended = false;
                out.push(pcm);
                return;
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
            self.apply(pcm, out);
            let Some(next) = rest else { return };
            pcm = next;
            pts = None;
        }
    }

    fn apply(&mut self, mut pcm: P, out: &mut Vec<P>) {
        let n = pcm.samples() as u64;
        let rate = pcm.rate();
        let Some(span) = self.current.as_mut() else {
            out.push(pcm);
            return;
        };
        span.produced = span.produced.saturating_add(n);
        if !skip_front(&mut self.skip, &mut self.skip_ended, &mut pcm) {
            return;
        }
        if span.padding == Count::None {
            out.push(pcm);
            return;
        }
        let padding = span.padding.at(rate);
        let charge = |p: &P| p.retained_bytes().saturating_add(std::mem::size_of::<P>());
        span.held_samples = span.held_samples.saturating_add(pcm.samples() as u64);
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
            out.extend(span.held.drain(..));
            span.held_samples = 0;
            span.held_bytes = 0;
            span.padding = Count::None;
            self.fallbacks.released_padding_spans = self.fallbacks.released_padding_spans.saturating_add(1);
        }
    }

    /// A frame the drain returned, trimmed as libavcodec trims it with the
    /// side data of the packet sent last (`decode.c` `discard_samples`).
    fn drained(&mut self, mut pcm: P, out: &mut Vec<P>) {
        let trim = self.last_sent;
        if let Some(t) = trim.filter(|t| t.skip_samples > 0) {
            self.skip = Count::declared(t.skip_samples, t.sample_rate);
        }
        if !skip_front(&mut self.skip, &mut self.skip_ended, &mut pcm) {
            return;
        }
        if let Some(t) = trim {
            let n = pcm.samples() as u64;
            let padding = Count::declared(t.discard_padding, t.sample_rate).at(pcm.rate());
            if padding > 0 && padding <= n {
                if padding == n {
                    return;
                }
                let _padding = pcm.split_off((n - padding) as usize);
            }
        }
        out.push(pcm);
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
        if self.association_lost {
            return None;
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

    /// A span is complete: what it holds back is its padding. Padding larger
    /// than the samples left after its priming is ignored instead, as
    /// libavcodec ignores padding longer than a frame (`decode.c`).
    fn end_span(&mut self, out: &mut Vec<P>) {
        if let Some(span) = self.current.take() {
            if let Count::Output(padding) = span.padding {
                if padding > span.held_samples {
                    out.extend(span.held);
                }
            }
        }
    }

    /// The decoder is about to be drained (`Decoder::flush`): the span of
    /// the packet decoded last ends, its padding off the output it produced,
    /// and packets still queued produced nothing. What the drain returns is
    /// trimmed frame by frame (see the crate docs).
    pub fn drain(&mut self, out: &mut Vec<P>) {
        self.end_span(out);
        self.queue.clear();
        self.association_lost = false;
        self.draining = true;
    }

    /// The decoder is drained: what the last span holds back is the
    /// stream's end padding (unless longer than the span, see `end_span`),
    /// and queued packets produced nothing.
    pub fn finish(&mut self, out: &mut Vec<P>) {
        self.end_span(out);
        self.queue.clear();
        self.skip_ended = false;
        self.association_lost = false;
        self.draining = false;
    }

    /// The decoder starts over (a seek). Stale held PCM never plays.
    pub fn reset(&mut self) {
        self.skip = Count::None;
        self.skip_ended = false;
        self.current = None;
        self.queue.clear();
        self.association_lost = false;
        self.awaiting_first_packet = self.drops_first_packet;
        self.last_sent = None;
        self.draining = false;
    }
}

/// Takes a start skip still pending (`skip`) off the front of `pcm`, and
/// marks where the presentation begins. False when the skip covers all of
/// `pcm`.
fn skip_front<P: Pcm>(skip: &mut Count, skip_ended: &mut bool, pcm: &mut P) -> bool {
    if *skip != Count::None {
        let n = pcm.samples() as u64;
        let at = skip.at(pcm.rate());
        if at >= n {
            *skip = if at > n { Count::Output(at - n) } else { Count::None };
            *skip_ended = at == n;
            return false;
        }
        pcm.drop_front(at as usize);
        *skip = Count::None;
        *skip_ended = true;
    }
    if std::mem::take(skip_ended) {
        pcm.begins_presentation();
    }
    true
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
            t.packet(&packet(frame.start as i64), *meta);
            t.frame(Run(frame.clone()), None, &mut out);
        }
        out
    }

    #[test]
    fn mid_stream_padding_goes_when_the_next_packets_output_starts() {
        // Matroska DiscardPadding on a Block before the last one.
        let mut t = Trimmer::new();
        let mut out = run(&mut t, &[(None, 0..1024), (trim(0, 100), 1024..2048), (None, 2048..3072)]);
        t.finish(&mut out);
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

    #[test]
    fn equal_pts_laces_keep_padding_on_their_own_split_output() {
        let mut t = Trimmer::new();
        let mut out = Vec::new();
        t.packet(&packet(0), trim(0, 700));
        t.packet(&packet(0), None);
        t.frame(Run(0..512), Some(0), &mut out);
        t.frame(Run(512..1024), Some(0), &mut out);
        t.frame(Run(1024..2048), Some(0), &mut out);
        t.finish(&mut out);
        assert_eq!(out, [Run(0..324), Run(1024..2048)]);
    }

    #[test]
    fn a_frame_crossing_packet_spans_preserves_each_packets_padding() {
        let mut t = Trimmer::new();
        let mut out = Vec::new();
        t.packet(&packet(0), trim(0, 100));
        t.packet(&packet(1024), trim(0, 200));
        t.frame(Run(0..2048), Some(0), &mut out);
        t.finish(&mut out);
        assert_eq!(out, [Run(0..924), Run(1024..1848)]);
    }

    #[test]
    fn hostile_duration_does_not_overflow_or_move_padding_to_another_packet() {
        let mut t = Trimmer::new();
        let mut out = Vec::new();
        let mut p = packet(0);
        p.time_base = oxideav_core::TimeBase::new(i64::MAX, 1);
        p.duration = Some(i64::MAX);
        t.packet(&p, trim(0, 100));
        t.frame(Run(0..512), None, &mut out);
        t.packet(&packet(1024), None);
        t.frame(Run(512..1024), None, &mut out);
        t.finish(&mut out);
        assert_eq!(out.into_iter().flat_map(|run| run.0).collect::<Vec<_>>(), (0..924).collect::<Vec<_>>());
    }

    #[test]
    fn full_association_queue_keeps_lost_output_untrimmed() {
        let mut t = Trimmer::new();
        let mut out = Vec::new();
        t.packet(&packet(0), trim(100, 0));
        for i in 1..=MAX_QUEUED {
            t.packet(&packet(i as i64 * 1024), None);
        }
        assert_eq!(t.queue.len(), MAX_QUEUED);
        t.frame(Run(0..1024), Some(0), &mut out);
        assert_eq!(out, [Run(0..1024)]);
        assert_eq!(t.take_fallbacks(), Fallbacks { lost_packets: 1, released_padding_spans: 0 });
    }

    #[test]
    fn a_queued_packets_timestamp_restores_association_after_a_loss() {
        let mut t = Trimmer::new();
        let mut out = Vec::new();
        for i in 0..=MAX_QUEUED {
            let meta = match i { 1 => trim(300, 0), 2 => trim(0, 100), _ => None };
            t.packet(&packet(i as i64 * 1024), meta);
        }
        // Packet 0 is lost. An unstamped frame cannot be placed: it plays
        // whole, without taking packet 1's skip by order.
        t.frame(Run(0..1024), None, &mut out);
        // Stamped frames name their packets again, with their own trims.
        for n in 1..4u64 {
            t.frame(Run(n * 1024..(n + 1) * 1024), Some(n as i64 * 1024), &mut out);
        }
        assert_eq!(out, [Run(0..1024), Run(1324..2048), Run(2048..2972), Run(3072..4096)]);
    }

    #[test]
    fn memory_limit_releases_padding_and_keeps_later_audio() {
        let mut t = Trimmer::new();
        let mut out = Vec::new();
        t.packet(&packet(0), trim(0, u32::MAX));
        let samples = MAX_HELD_BYTES as u64 / 4;
        t.frame(Run(0..samples), Some(0), &mut out);
        assert_eq!(out, [Run(0..samples)]);
        assert_eq!(t.current.as_ref().unwrap().held_bytes, 0);
        t.packet(&packet(samples as i64), None);
        t.frame(Run(samples..samples + 1024), Some(samples as i64), &mut out);
        assert_eq!(out, [Run(0..samples), Run(samples..samples + 1024)]);
        assert_eq!(t.take_fallbacks(), Fallbacks { lost_packets: 0, released_padding_spans: 1 });
    }

    #[test]
    fn a_decoder_delay_is_the_first_skip_unless_a_container_skip_replaces_it() {
        let delay = DecoderDelay::Skip(AudioTrim { skip_samples: 312, discard_padding: 0, sample_rate: 48000 });
        let mut t = Trimmer::with_decoder_delay(delay);
        assert_eq!(run(&mut t, &[(None, 0..1024), (None, 1024..2048)]), [Run(312..1024), Run(1024..2048)]);
        // An MP4 edit list's or Matroska CodecDelay's skip replaces it.
        let mut t = Trimmer::with_decoder_delay(delay);
        assert_eq!(run(&mut t, &[(trim(500, 0), 0..1024)]), [Run(500..1024)]);
        // A seek does not bring it back.
        t.reset();
        assert_eq!(run(&mut t, &[(None, 9216..10240)]), [Run(9216..10240)]);
    }

    #[test]
    fn a_first_packet_the_decoder_drops_keeps_its_skip_off_later_output() {
        let mut t = Trimmer::with_decoder_delay(DecoderDelay::FirstPacket);
        let mut out = Vec::new();
        for start in [0, 9216] {
            // A CodecDelay skip on the dropped first packet; the first output
            // is the second packet's, by duration order, and plays whole.
            t.packet(&packet(start as i64 - 1024), trim(1024, 0));
            t.packet(&packet(start as i64), None);
            t.frame(Run(start..start + 1024), None, &mut out);
            t.packet(&packet(start as i64 + 1024), trim(0, 100));
            t.frame(Run(start + 1024..start + 2048), None, &mut out);
            t.finish(&mut out);
            assert_eq!(out.drain(..).collect::<Vec<_>>(), [Run(start..start + 1024), Run(start + 1024..start + 1948)]);
            // After a seek the new decoder drops its first packet again.
            t.reset();
        }
    }

    /// A run that records whether it begins the presentation.
    #[derive(Debug, PartialEq)]
    struct Marked(std::ops::Range<u64>, bool);

    impl Pcm for Marked {
        fn samples(&self) -> usize {
            (self.0.end - self.0.start) as usize
        }
        fn rate(&self) -> u32 {
            48000
        }
        fn retained_bytes(&self) -> usize {
            0
        }
        fn drop_front(&mut self, n: usize) {
            self.0.start += n as u64;
        }
        fn split_off(&mut self, n: usize) -> Self {
            let at = self.0.start + n as u64;
            let rest = Marked(at..self.0.end, false);
            self.0.end = at;
            rest
        }
        fn begins_presentation(&mut self) {
            self.1 = true;
        }
    }

    #[test]
    fn the_first_sample_after_a_start_skip_begins_the_presentation() {
        // Within a frame, at a frame boundary, and without a skip.
        for (skip, first) in [(1500, Some(1500)), (1024, Some(1024)), (0, None)] {
            let mut t = Trimmer::new();
            let mut out = Vec::new();
            for n in 0..3u64 {
                t.packet(&packet(n as i64 * 1024), if n == 0 { trim(skip, 0) } else { None });
                t.frame(Marked(n * 1024..(n + 1) * 1024, false), None, &mut out);
            }
            let begins: Vec<u64> = out.iter().filter(|m| m.1).map(|m| m.0.start).collect();
            assert_eq!(begins, first.into_iter().collect::<Vec<_>>(), "skip {skip}");
        }
    }

    #[test]
    fn only_an_opus_pre_skip_moves_out_of_the_decoder_parameters() {
        let head = |pre_skip: u16| {
            [&b"OpusHead\x01\x02"[..], &pre_skip.to_le_bytes(), &48000u32.to_le_bytes(), &[0, 0, 0]].concat()
        };
        let mut opus = CodecParameters::audio(oxideav_core::CodecId::new("opus"));
        opus.extradata = head(312);
        assert_eq!(take_decoder_delay(&mut opus),
            DecoderDelay::Skip(AudioTrim { skip_samples: 312, discard_padding: 0, sample_rate: 48000 }));
        assert_eq!(opus.extradata, head(0));
        assert_eq!(take_decoder_delay(&mut opus), DecoderDelay::None, "nothing left to move");
        let mut aac = CodecParameters::audio(oxideav_core::CodecId::new("aac"));
        aac.extradata = head(312);
        assert_eq!(take_decoder_delay(&mut aac), DecoderDelay::None);
        assert_eq!(aac.extradata, head(312));
        let mut short = CodecParameters::audio(oxideav_core::CodecId::new("opus"));
        short.extradata = head(312)[..11].to_vec();
        assert_eq!(take_decoder_delay(&mut short), DecoderDelay::None);
        let mut vorbis = CodecParameters::audio(oxideav_core::CodecId::new("vorbis"));
        assert_eq!(take_decoder_delay(&mut vorbis), DecoderDelay::FirstPacket);
    }

    #[test]
    fn padding_larger_than_the_samples_after_priming_is_ignored() {
        for padding in [223, 224, 225] {
            let mut t = Trimmer::new();
            let mut out = Vec::new();
            t.packet(&packet(0), trim(800, padding));
            t.frame(Run(0..1024), Some(0), &mut out);
            t.finish(&mut out);
            let end = if padding > 224 { 1024 } else { 1024 - u64::from(padding) };
            let expected: Vec<_> = (800..end).collect();
            assert_eq!(out.into_iter().flat_map(|run| run.0).collect::<Vec<_>>(), expected, "padding={padding}");
        }
    }

    #[test]
    fn a_drain_keeps_a_frame_shorter_than_the_last_padding() {
        // An Opus decoder's resampler drains 24 delayed samples after the
        // last packet, which discards 300: libavcodec cuts the padding from
        // that packet's frame and keeps the 24, too short for it.
        let mut t = Trimmer::new();
        let mut out = run(&mut t, &[(None, 0..1024), (trim(0, 300), 1024..2048)]);
        t.drain(&mut out);
        t.frame(Run(2048..2072), None, &mut out);
        t.finish(&mut out);
        assert_eq!(out, [Run(0..1024), Run(1024..1748), Run(2048..2072)]);
    }

    #[test]
    fn a_drained_frame_at_least_as_long_as_the_last_padding_loses_it() {
        // Every drained frame carries the last packet's side data in
        // libavcodec: one exactly the padding's length goes, a longer one
        // loses its end.
        let mut t = Trimmer::new();
        let mut out = run(&mut t, &[(trim(0, 300), 0..1024)]);
        t.drain(&mut out);
        t.frame(Run(1024..1324), None, &mut out);
        t.frame(Run(1324..2348), None, &mut out);
        t.finish(&mut out);
        assert_eq!(out, [Run(0..724), Run(1324..2048)]);
    }
}
