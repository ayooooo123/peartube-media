//! PearTube's media player: plays an HTTP stream URL through OxideAV's
//! demuxers and decoders and a platform [`backend::Backend`].

pub mod backend;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod apple;
