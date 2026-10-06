//! PearTube's media player: plays an HTTP stream URL through OxideAV's
//! demuxers and decoders and a platform [`backend::Backend`].

pub mod backend;

pub mod clock;
pub mod engine;
pub mod headless;
pub mod source;
pub mod subs;

pub use engine::{Event, OpenError, Player, PlayerOptions, State, Track, TrackKind};
pub use headless::{AudioCapture, Capture, Headless, SubtitleCapture, VideoCapture};

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "android")]
pub use android::AndroidBackend;
