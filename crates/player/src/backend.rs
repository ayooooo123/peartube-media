//! What a platform gives the engine: somewhere to send audio, video and
//! subtitles, and the clock that ties them together.
//!
//! The engine decodes audio itself and hands PCM to an [`AudioSink`], whose
//! [`Clock`] is the playback's master clock while audio plays. Video goes to
//! a [`VideoSink`], which first gets a chance to take the stream compressed
//! (a platform decoder); if it declines, or fails later, the engine decodes
//! in software and hands it frames. Either way the video is presented on
//! the master clock. Subtitles arrive as positioned RGBA images.

use oxideav_core::{CodecParameters, Packet, VideoFrame};
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

/// Audio output. PCM is interleaved f32 in [-1, 1].
pub trait AudioSink: Send {
    /// Configures the output. Called before the first `write` and again when
    /// the format changes.
    fn open(&mut self, sample_rate: u32, channels: u16) -> Result<(), SinkError>;
    /// Queues PCM whose first sample plays at media time `pts`. Blocks while
    /// a playing output's buffer is full; a paused output that is full takes
    /// nothing (0). Returns how many frames it took.
    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError>;
    /// Starts the output: queued audio plays and the clock runs.
    fn play(&mut self);
    /// Stops the output where it is: the clock stands still, nothing queued
    /// is lost.
    fn pause(&mut self);
    /// Drops everything queued (seek). The clock restarts at the next
    /// `write`'s `pts`.
    fn flush(&mut self);
    /// The output's clock: the media time of the sample being heard. The
    /// playback's master clock while audio plays.
    fn clock(&self) -> Arc<dyn Clock>;
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
    /// Monotonic time with user pauses removed. Buffering still consumes time.
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
    #[error("compressed video unsupported")]
    Unsupported,
    #[error(transparent)]
    Sink(#[from] SinkError),
}

/// Polling and admission do no native work and never wait for native cleanup.
/// Native backends use bounded owners; synchronous capture can stay inline.
pub trait VideoSink: Send {
    fn output(&self) -> VideoOutput;
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
