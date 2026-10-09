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
use std::time::Duration;

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
#[derive(Debug, thiserror::Error)]
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

/// How a sink that takes a stream compressed tells the engine its decoder
/// has output a picture it will show: one at or after
/// [`VideoSink::present_from`]. The engine holds the playback's clock until
/// the first such picture after each start or seek is there, so the video
/// starts with the audio instead of behind it while the decoder works
/// through the pictures before it. Callable from any thread, for every
/// picture or only the first.
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

/// Video output.
pub trait VideoSink: Send {
    /// Offers the stream compressed. Return `true` to take it: the engine then
    /// sends packets to `push_packet`, and the sink reports its decoder's
    /// pictures to `ready`. Return `false` to have the engine decode in
    /// software and call `push_frame`.
    fn open_compressed(&mut self, params: &CodecParameters, ready: PictureReady) -> bool;
    /// One compressed access unit, in decode order; `pts` is its media
    /// presentation time. `random_access` combines the parser keyframe flag
    /// and the container's independent random-access indication. The sink
    /// decodes the unit and presents it on the clock.
    fn push_packet(&mut self, packet: &Packet, pts: Duration, random_access: bool) -> Result<(), SinkError>;
    /// Compressed input: the pictures to show start at media time `start`.
    /// The decoder still decodes those before it, which later pictures
    /// predict from, but they are never shown: after a seek the ones before
    /// the target, and the incomplete ones before a recovery point has
    /// recovered. The engine sets it after `open_compressed` and each
    /// `flush`, before the packets it applies to, and raises it when it
    /// learns that a recovery point recovers later. It applies to the
    /// pictures the sink has not yet shown, including decoded output still
    /// waiting for presentation.
    fn present_from(&mut self, start: Duration);
    /// Prepares for software frames of this stream.
    fn open_frames(&mut self, params: &CodecParameters) -> Result<(), SinkError>;
    /// One decoded frame, in presentation order, to show at media time `pts`.
    /// The engine calls it `frame_lead` before `pts` on the playback's clock;
    /// the sink shows it at `pts`.
    fn push_frame(&mut self, frame: &VideoFrame, pts: Duration) -> Result<(), SinkError>;
    /// How long before its `pts` a decoded frame should reach `push_frame`.
    /// A sink that presents frames by their timestamps (a layer timed by
    /// the audio clock) takes them early; one that shows a frame as soon as
    /// it has it takes it on time, early by only the work it does before
    /// the frame is on screen.
    fn frame_lead(&self) -> Duration;
    /// Signals end of compressed input so a platform decoder releases its
    /// reordered tail. Every sink states what it does; a frame-only output
    /// has nothing to drain.
    fn finish(&mut self) -> Result<(), SinkError>;
    /// Drops everything queued and decoder state (seek). Once it returns,
    /// the sink reports no picture pushed before it to `PictureReady`.
    fn flush(&mut self);
    /// Pause/resume presentation (the clock stops with the audio).
    fn set_playing(&mut self, playing: bool);
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
