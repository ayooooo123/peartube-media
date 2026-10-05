//! What a platform gives the engine: somewhere to send audio, video and
//! subtitles, and the clock that ties them together.
//!
//! The engine decodes audio itself and hands PCM to an [`AudioSink`], which
//! owns the master [`Clock`]. Video goes to a [`VideoSink`], which first gets
//! a chance to take the stream compressed (a platform decoder); if it
//! declines, or fails later, the engine decodes in software and hands it
//! frames. Subtitles arrive as positioned RGBA images.

use oxideav_core::{CodecParameters, Packet, VideoFrame};
use std::sync::Arc;
use std::time::Duration;

/// A platform: Android, Apple, or headless for tests.
pub trait Backend: Send + Sync {
    /// The audio output for one playback.
    fn audio(&self) -> Box<dyn AudioSink>;
    /// The video output for one playback. `clock` is the audio sink's clock
    /// (or a free-running one when there is no audio).
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
    /// presented, or `None` while unknown. Android uses it for
    /// `AMediaCodec_releaseOutputBufferAtTime`.
    fn monotonic_ns_at(&self, at: Duration) -> Option<i64>;
}

/// Why a sink refused a packet or frame.
#[derive(Debug, thiserror::Error)]
pub enum SinkError {
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
    /// the output's buffer is full; returns how many frames it took.
    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError>;
    fn play(&mut self);
    fn pause(&mut self);
    /// Drops everything queued (seek). The clock restarts at the next
    /// `write`'s `pts`.
    fn flush(&mut self);
    /// The master clock for this playback.
    fn clock(&self) -> Arc<dyn Clock>;
}

/// Video output.
pub trait VideoSink: Send {
    /// Offers the stream compressed. Return `true` to take it: the engine then
    /// sends packets to `push_packet`. Return `false` to have the engine
    /// decode in software and call `push_frame`.
    fn open_compressed(&mut self, params: &CodecParameters) -> bool;
    /// One compressed access unit, in decode order; `pts` is its media
    /// presentation time. The sink decodes it and presents it on the clock.
    fn push_packet(&mut self, packet: &Packet, pts: Duration) -> Result<(), SinkError>;
    /// Prepares for software frames of this stream.
    fn open_frames(&mut self, params: &CodecParameters) -> Result<(), SinkError>;
    /// One decoded frame, in presentation order, to show at media time `pts`.
    /// The engine calls it shortly before `pts`; the sink shows it at `pts`.
    fn push_frame(&mut self, frame: &VideoFrame, pts: Duration) -> Result<(), SinkError>;
    /// Drops everything queued and decoder state (seek).
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
