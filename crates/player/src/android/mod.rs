//! Android backend for PearTube: AAudio for audio output and master clock,
//! MediaCodec for hardware video decoding, ANativeWindow for video and
//! subtitle presentation.

pub mod audio;
pub mod backend;
pub mod clock;
pub mod subtitle;
pub mod video;

pub use backend::AndroidBackend;
