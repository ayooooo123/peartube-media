//! What a platform gives the engine: somewhere to send audio, video and
//! subtitles, and the clock that ties them together.
//!
//! The engine decodes audio itself and hands PCM to an [`AudioSink`], unless
//! the sink takes the stream compressed (E-AC-3, to keep Dolby Atmos); the
//! sink's [`Clock`] is the playback's master clock while audio plays. Video goes to
//! a [`VideoSink`], which first gets a chance to take the stream compressed
//! (a platform decoder); if it declines, or fails later, the engine decodes
//! in software and hands it frames. Either way the video is presented on
//! the master clock. Subtitles arrive as positioned RGBA images.

use oxideav_core::{ChannelLayout, CodecParameters, Packet, SampleFormat, VideoFrame};
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

/// A platform: Android, Apple, or headless for tests.
pub trait Backend: Send + Sync {
    /// The audio output for one playback. The engine creates it before the
    /// video output and keeps it for the whole playback, across audio track
    /// switches.
    fn audio(&self) -> Box<dyn AudioSink>;
    /// The video output for one playback. `clock` is the playback's clock:
    /// the audio output's clock while audio plays, a free-running one
    /// otherwise (no audio track, or after the audio ended).
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink>;
    /// Configures content cadence for this playback's video sink.
    fn video_with_frame_rate(&self, clock: Arc<dyn Clock>, _mode: FrameRateMode) -> Box<dyn VideoSink> {
        self.video(clock)
    }
    /// The subtitle overlay for one playback.
    fn subtitles(&self) -> Box<dyn SubtitleSink>;
    /// The app went to the background: release what the platform reclaims
    /// (decoders, surfaces, the audio stream). The engine pauses first.
    fn suspend(&self) {}
    /// The app is back in front.
    fn resume(&self) {}
}

/// Media time, shared by every sink of one playback.
pub trait Clock: Send + Sync {
    /// Current media position, or `None` before the first audio is heard.
    fn now(&self) -> Option<Duration>;
    /// CLOCK_MONOTONIC nanoseconds at which media time `at` will be (or was)
    /// presented, or `None` while that is unknown: before the output
    /// started, and whenever the clock stands still (paused, holding), since
    /// then nothing says when it will move again. Android uses it for
    /// `AMediaCodec_releaseOutputBufferAtTime`; the engine to wake for a
    /// frame's time.
    fn monotonic_ns_at(&self, at: Duration) -> Option<i64>;
    /// True while an audio output's clock leads this one: that output then
    /// owns the position of the timebase it drives, and others only start
    /// or stop it.
    fn follows_audio(&self) -> bool {
        false
    }
}

/// Why a sink refused a packet or frame.
#[derive(Clone, Debug, thiserror::Error)]
pub enum SinkError {
    /// A bounded enqueue made no room. Retry the same packet after applying
    /// transport changes; it has not been consumed.
    #[error("output buffer full")]
    WouldBlock,
    /// The platform decoder cannot continue with this stream; the engine
    /// switches to software decoding at the next keyframe.
    #[error("platform decoder failed: {0}")]
    Fallback(String),
    /// The output is gone (surface destroyed, device lost) until
    /// `Backend::resume` or a new surface.
    #[error("output unavailable")]
    Unavailable,
    /// Anything else; playback stops with this message.
    #[error("{0}")]
    Fatal(String),
}

/// Canonical interleaved little-endian PCM sample representation carried from
/// software decoders to an [`AudioSink`] without float narrowing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PcmFormat {
    /// Unsigned 8-bit (`[0, 255]`, bias 128).
    U8,
    /// Signed 16-bit little-endian (`i16`).
    S16,
    /// Signed 24-bit packed little-endian (`3` bytes/sample).
    S24,
    /// Signed 32-bit little-endian (`i32`, including 24-in-32 MSB-aligned lossless codecs).
    S32,
    /// IEEE-754 32-bit float little-endian (`f32` in `[-1, 1]`).
    F32,
    /// IEEE-754 64-bit float little-endian (`f64` in `[-1, 1]`).
    F64,
}

impl PcmFormat {
    /// Bytes per single-channel sample in packed interleaved storage.
    #[inline]
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::S16 => 2,
            Self::S24 => 3,
            Self::S32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }

    /// Maps a decoder's [`SampleFormat`] (packed or planar) to its lossless
    /// interleaved [`PcmFormat`]. Signed 8-bit widens losslessly to `S16`.
    #[inline]
    pub fn from_sample_format(format: SampleFormat) -> Self {
        match format {
            SampleFormat::U8 | SampleFormat::U8P => Self::U8,
            SampleFormat::S8 | SampleFormat::S16 | SampleFormat::S16P => Self::S16,
            SampleFormat::S24 => Self::S24,
            SampleFormat::S32 | SampleFormat::S32P => Self::S32,
            SampleFormat::F64 | SampleFormat::F64P => Self::F64,
            SampleFormat::F32 | SampleFormat::F32P | _ => Self::F32,
        }
    }
}

/// Audio routing policy from engine to platform [`AudioSink`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioOutputMode {
    /// Preserve native integer/float precision and discrete multichannel LPCM
    /// when the active endpoint supports the channel count; downmix `>2`
    /// channels to stereo on 2-channel endpoints. Compressed passthrough stays
    /// on platform object decoders (Apple E-AC-3/AC-3).
    #[default]
    Auto,
    /// Prefer compressed bitstream passthrough (IEC 61937 AC-3, E-AC-3, TrueHD
    /// Dolby MAT, DTS, DTS-HD MA / DTS:X HBR; Apple E-AC-3/AC-3), falling back
    /// to unmixed multichannel bit-exact LPCM.
    Passthrough,
    /// Decode in software to bit-exact LPCM, keep discrete multichannel layout
    /// when supported, and match hardware nominal sample rate on macOS.
    BitPerfectPcm,
    /// Always fold `>2` channels into stereo via `StereoMix`.
    Stereo,
}

/// Automatic display refresh-rate matching policy (Dune-style AFR).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FrameRateMode {
    /// Request seamless/adaptive frame-rate matching (`ANativeWindow` seamless
    /// on Android, ProMotion frame-rate hints on Apple) without black-screen
    /// HDMI mode switches.
    #[default]
    Seamless,
    /// Match exact content frame rate (`23.976`, `24`, `25`, `29.97`, `50`,
    /// `59.94`, `60` Hz) before releasing the priming hold, including physical
    /// display mode switches where supported.
    Exact,
    /// Leave display refresh rate untouched.
    Off,
}

/// Borrowed interleaved little-endian PCM frames in their native [`PcmFormat`].
#[derive(Clone, Copy, Debug)]
pub struct PcmSlice<'a> {
    pub format: PcmFormat,
    pub channels: u16,
    pub data: &'a [u8],
}

impl<'a> PcmSlice<'a> {
    #[inline]
    pub fn frames(&self) -> usize {
        let stride = usize::from(self.channels.max(1)) * self.format.bytes_per_sample();
        self.data.len() / stride.max(1)
    }

    #[inline]
    pub fn subslice_frames(&self, start_frame: usize, end_frame: usize) -> Self {
        let stride = usize::from(self.channels.max(1)) * self.format.bytes_per_sample();
        let total = self.frames();
        let s = start_frame.min(total) * stride;
        let e = end_frame.min(total).max(start_frame.min(total)) * stride;
        Self {
            format: self.format,
            channels: self.channels,
            data: &self.data[s..e],
        }
    }

    /// Converts a single sample byte slice in `format` to normalized `f64` in `[-1, 1]`.
    #[inline]
    pub fn sample_to_f64(format: PcmFormat, b: &[u8]) -> f64 {
        match format {
            PcmFormat::U8 => (f64::from(b[0]) - 128.0) / 128.0,
            PcmFormat::S16 => f64::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0,
            PcmFormat::S24 => {
                f64::from(i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) / 8_388_608.0
            }
            PcmFormat::S32 => {
                f64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]])) / 2_147_483_648.0
            }
            PcmFormat::F32 => f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            PcmFormat::F64 => f64::from_le_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ]),
        }
    }

    /// Converts `self` to interleaved `f32` in `[-1, 1]` for float-only sinks or SNR checks.
    pub fn to_f32_vec(&self) -> Vec<f32> {
        let w = self.format.bytes_per_sample();
        let mut out = Vec::with_capacity(self.data.len() / w);
        for b in self.data.chunks_exact(w) {
            out.push(Self::sample_to_f64(self.format, b) as f32);
        }
        out
    }
}

/// Audio output. Native sinks consume [`PcmSlice`] directly at full integer or
/// float precision; legacy callers may still push normalized `f32`.
pub trait AudioSink: Send {
    /// Configures the output for `f32` PCM. Called by legacy callers or
    /// the default [`Self::open_pcm`] delegation.
    fn open(&mut self, sample_rate: u32, layout: ChannelLayout) -> Result<(), SinkError>;
    /// Configures the output for `format` and routing `mode`. Native lossless
    /// sinks override this to keep exact integer/float sample widths and
    /// discrete multichannel speaker layouts.
    fn open_pcm(
        &mut self,
        sample_rate: u32,
        layout: ChannelLayout,
        _format: PcmFormat,
        _mode: AudioOutputMode,
    ) -> Result<(), SinkError> {
        self.open(sample_rate, layout)
    }
    /// Queues `f32` PCM whose first sample plays at media time `pts`.
    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError>;
    /// Queues native-width PCM whose first sample plays at media time `pts`.
    /// Returns how many frames it took.
    fn write_pcm(&mut self, pcm: PcmSlice<'_>, pts: Duration) -> Result<usize, SinkError> {
        let samples = pcm.to_f32_vec();
        self.write(&samples, pts)
    }
    /// Starts the output: queued audio plays and the clock runs.
    fn play(&mut self);
    /// Stops the output where it is: the clock stands still, nothing queued
    /// is lost.
    fn pause(&mut self);
    /// Drops everything queued (seek). The clock restarts at the next
    /// `write`'s `pts`.
    fn flush(&mut self);
    /// Pushes any sub-threshold coalesced tail to the device at stream end.
    /// `Ok(true)` once nothing pending remains; `Ok(false)` when a full or
    /// paused output still holds room back so the caller should retry.
    fn drain(&mut self) -> Result<bool, SinkError> {
        Ok(true)
    }
    /// Offers the stream compressed (`params`, and its first packet for the
    /// headers the setup bytes lack), to play with the platform's own decoder
    /// or IEC 61937 bitstream passthrough.
    fn open_compressed(
        &mut self,
        _params: &CodecParameters,
        _first_packet: &[u8],
    ) -> Result<bool, SinkError> {
        Ok(false)
    }
    /// Mode-aware compressed passthrough entry point.
    fn open_compressed_with_mode(
        &mut self,
        params: &CodecParameters,
        first_packet: &[u8],
        _mode: AudioOutputMode,
    ) -> Result<bool, SinkError> {
        self.open_compressed(params, first_packet)
    }
    /// Queues one compressed packet of `frames` samples per channel whose first
    /// sample plays at media time `pts`. `Ok(false)`: full or paused, nothing
    /// taken; the caller retries.
    fn write_packet(
        &mut self,
        _packet: &[u8],
        _pts: Duration,
        _frames: u32,
    ) -> Result<bool, SinkError> {
        Err(SinkError::Fatal("this output plays PCM only".into()))
    }
    /// The output's clock: the media time of the sample being heard. The
    /// playback's master clock while audio plays.
    fn clock(&self) -> Arc<dyn Clock>;
    /// Channels the output actually plays after any fold, when the sink knows.
    fn output_channels(&self) -> Option<u16> {
        None
    }
}

/// A producer-bound observation that a decoded target picture reached the
/// output. The engine supplies a fresh callback for every producer. Callbacks
/// publish observations and wake the engine; they do not change transport.
#[derive(Clone)]
pub struct PictureReady(Arc<dyn Fn(Duration) + Send + Sync>);

impl PictureReady {
    pub fn new(report: impl Fn(Duration) + Send + Sync + 'static) -> PictureReady {
        PictureReady(Arc::new(report))
    }

    /// The decoder output a picture to show at media time `pts`.
    pub fn ready(&self, pts: Duration) {
        (self.0)(pts)
    }
}

/// Engine-issued, monotonically increasing identity of one output producer,
/// including replacements within one seek. Never reused across sink lifetimes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProducerId(pub u64);

/// Sink-issued identity of one prepared frame, never reused within that sink.
/// Decisions also name its producer; timestamps are not identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedFrameId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameDecision {
    Present,
    Discard,
}

/// Cached output admission state, not a native configuration or display receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoOutput {
    pub revision: u64,
    pub available: bool,
}

/// Pipeline control shared by immutable requests. Implementations must not
/// acquire a platform sink lock or call native APIs.
pub trait VideoControl: Send + Sync {
    fn cancelled(&self, producer: ProducerId, seek_generation: u64) -> bool;
    /// Monotonic time without user pauses or waits for network bytes. Other
    /// buffering still consumes time.
    fn active_now(&self) -> Instant;
    fn wake(&self);
}

#[derive(Clone)]
pub enum VideoTarget {
    Compressed {
        params: Arc<CodecParameters>,
        ready: PictureReady,
        present_from: Duration,
    },
    Frames {
        params: Arc<CodecParameters>,
        ready: PictureReady,
        /// False only for a software format change: finish accepted input and
        /// preserve already queued pictures and the clock anchor.
        reset: bool,
    },
    Retired,
}

/// A producer names immutable inputs. Repeated polling must not restart work,
/// renew the deadline, reset presentation thresholds or queue another command.
#[derive(Clone)]
pub struct VideoRequest {
    pub producer: ProducerId,
    pub seek_generation: u64,
    pub output_revision: u64,
    pub target: VideoTarget,
    pub deadline: Instant,
    pub control: Arc<dyn VideoControl>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoMode {
    Compressed,
    Frames,
    Retired,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum VideoError {
    #[error("video producer superseded")]
    Superseded,
    #[error("video operation unsupported")]
    Unsupported,
    #[error(transparent)]
    Sink(#[from] SinkError),
}

/// Polling and admission do no native work and never wait for native cleanup.
/// Native backends use bounded owners; synchronous capture can stay inline.
pub trait VideoSink: Send {
    fn output(&self) -> VideoOutput;
    /// The decoder behind compressed input, as `State::video_decoder` names
    /// it (`MediaCodec`, `VideoToolbox`); software decoding is named by the
    /// engine.
    fn platform_decoder(&self) -> &'static str {
        "platform decoder"
    }
    /// Continues to expose producer-tagged failures after configuration, even
    /// after the last input. Pending is not unsupported or a fallback request.
    /// A retired result requires cleanup and owner exit/reaping, not a stop flag.
    fn poll_transition(&mut self, request: &VideoRequest) -> Poll<Result<VideoMode, VideoError>>;
    /// Success takes the input exactly once. Every error leaves it untouched.
    /// Credit includes owner-held input, not just the mailbox. Success proves
    /// admission, not decoded output, renderer enqueue or physical display.
    fn push_packet(&mut self, producer: ProducerId, packet: &mut Option<Packet>,
        pts: Duration, random_access: bool) -> Result<(), VideoError>;
    fn push_frame(&mut self, producer: ProducerId, frame: &mut Option<VideoFrame>,
        pts: Duration) -> Result<(), VideoError>;
    /// Optionally take a frame for preparation before the engine's due-time
    /// wait. Success consumes it and keeps the shared input credit charged.
    /// No picture may be posted until `decide_frame` grants Present. Every
    /// error, including Unsupported, leaves the input untouched; Unsupported
    /// selects the ordinary wait-then-push path, not decoder fallback.
    fn prepare_frame(&mut self, _producer: ProducerId, _frame: &mut Option<VideoFrame>,
        _pts: Duration) -> Result<PreparedFrameId, VideoError> {
        Err(VideoError::Unsupported)
    }
    /// Nonblocking decision for exactly one prepared input. Repeating the same
    /// decision is idempotent while that input remains owned; stale tokens must
    /// not affect newer frames, including frames with identical timestamps.
    /// Present grants submission, not display. Discard posts no picture and
    /// reports no readiness; the owner releases its credit after discarding.
    fn decide_frame(&mut self, _producer: ProducerId, _frame: PreparedFrameId,
        _decision: FrameDecision) -> Result<(), VideoError> {
        Err(VideoError::Unsupported)
    }
    /// Keyed controls do not create a new producer or renew its deadline.
    fn present_from(&mut self, producer: ProducerId, start: Duration) -> Result<(), VideoError>;
    fn set_playing(&mut self, producer: ProducerId, playing: bool) -> Result<(), VideoError>;
    fn frame_lead(&self) -> Duration;
    /// Seals input once and acknowledges all accepted input and delayed output.
    /// Drained does not mean displayed or retired. Drop requests retirement
    /// without waiting; outstanding native resources remain accounted for.
    fn poll_finish(&mut self, producer: ProducerId) -> Poll<Result<(), VideoError>>;
}

/// Subtitle overlay. Coordinates are in video pixels; the sink scales them
/// with the video.
pub trait SubtitleSink: Send {
    /// Replaces what is on screen. An empty slice clears it.
    fn show(&mut self, images: &[SubtitleImage], video_width: u32, video_height: u32);
}

/// One positioned RGBA bitmap (straight alpha, row-major, `width * 4` stride).
#[derive(Clone, Debug)]
pub struct SubtitleImage {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}
