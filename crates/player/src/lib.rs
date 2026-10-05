//! PearTube's media player: plays an HTTP stream URL through OxideAV's
//! demuxers and decoders and a platform [`backend::Backend`].

pub mod backend;

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "android")]
pub use android::AndroidBackend;
