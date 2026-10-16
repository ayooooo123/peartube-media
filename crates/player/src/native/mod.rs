//! The native route (AetherEngine's design, reimplemented): on a backend
//! with a [`NativePresenter`](crate::backend::NativePresenter), H.264 and
//! HEVC are not decoded here. The demuxer's packets are remuxed on the fly
//! into HLS with fMP4 segments, served on 127.0.0.1, and the platform
//! player (AVPlayer) plays them: hardware decode, HDR presentation and
//! E-AC-3 Atmos passthrough. AAC, AC-3, E-AC-3, FLAC and ALAC are copied;
//! any other audio the engine decodes is bridged: decoded and encoded as
//! FLAC. Subtitles still come from the engine's subtitle pipeline, on the
//! presenter's clock.

pub(crate) mod bridge;
pub(crate) mod codec;
pub(crate) mod fmp4;
pub(crate) mod hls;
pub(crate) mod plan;
pub(crate) mod producer;
pub(crate) mod server;
pub(crate) mod session;
pub(crate) mod types;

