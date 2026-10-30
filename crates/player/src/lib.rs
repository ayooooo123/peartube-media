//! PearTube's media player: plays an HTTP stream URL through OxideAV's
//! demuxers and decoders and a platform [`backend::Backend`].

#[forbid(unsafe_code)]
mod ass;
#[cfg(any(target_os = "android", target_os = "macos", target_os = "ios", test))]
#[forbid(unsafe_code)]
mod audio_mix;
#[forbid(unsafe_code)]
pub mod annexb;
#[forbid(unsafe_code)]
pub mod backend;
#[forbid(unsafe_code)]
pub mod subtitle_compose;

pub mod clock;
#[forbid(unsafe_code)]
pub mod engine;
#[forbid(unsafe_code)]
pub mod headless;
#[forbid(unsafe_code)]
mod headers;
#[forbid(unsafe_code)]
pub mod source;
#[forbid(unsafe_code)]
pub mod subs;
#[forbid(unsafe_code)]
mod webvtt;
#[forbid(unsafe_code)]
mod video_owner;
#[forbid(unsafe_code)]
mod webvtt_text;

pub use engine::{Event, OpenError, Player, PlayerOptions, State, Track, TrackKind, CAPTIONS_608, CAPTIONS_708};
pub use headless::{AudioCapture, Capture, Headless, SubtitleCapture, VideoCapture};
pub use subs_render::FontOptions;

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "android")]
pub use android::AndroidBackend;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod apple;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use apple::AppleBackend;
#[cfg(target_os = "macos")]
pub use apple::{AppleWindowSlot, WindowSlotMetrics};
