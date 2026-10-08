//! PearTube's media player: plays an HTTP stream URL through OxideAV's
//! demuxers and decoders and a platform [`backend::Backend`].

pub mod annexb;
pub mod backend;
pub mod subtitle_compose;

pub mod clock;
pub mod engine;
pub mod headless;
pub mod source;
pub mod subs;
mod webvtt;
mod webvtt_text;

pub use engine::{Event, OpenError, Player, PlayerOptions, State, Track, TrackKind, CAPTIONS_608, CAPTIONS_708};
pub use headless::{AudioCapture, Capture, Headless, SubtitleCapture, VideoCapture};

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "android")]
pub use android::AndroidBackend;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod apple;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use apple::AppleBackend;
