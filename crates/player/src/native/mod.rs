//! The native route (AetherEngine's design, reimplemented): on a backend
//! with a [`NativePresenter`](crate::backend::NativePresenter), H.264 and
//! HEVC with AAC, AC-3 or E-AC-3 are not decoded here. The demuxer's packets
//! are remuxed on the fly into HLS with fMP4 segments, served on 127.0.0.1,
//! and the platform player (AVPlayer) plays them: hardware decode, HDR
//! presentation and E-AC-3 Atmos passthrough. Subtitles still come from the
//! engine's subtitle pipeline, on the presenter's clock.

pub(crate) mod codec;
pub(crate) mod fmp4;
pub(crate) mod hls;
pub(crate) mod server;
pub(crate) mod types;
