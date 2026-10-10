//! Android backend for PearTube: AAudio for audio output and master clock,
//! MediaCodec for hardware video decoding, ANativeWindow for video and
//! subtitle presentation.

pub mod audio;
pub mod backend;
pub mod clock;
mod position;
pub mod subtitle;
pub mod surface;
pub mod video;

#[cfg(test)]
mod tests;

pub use backend::AndroidBackend;
pub use subtitle::AndroidSubtitleSink;
pub use surface::{
    JavaSurfaceReservation, NativeSurfaceReservation, SurfaceAdmissionError, SurfaceBindError,
    SurfaceBinding, SurfaceBindingLease, SurfaceId, SurfaceRegistry, SurfaceRetired,
    SurfaceRetirement, SurfaceRetirementError, SurfaceRetirementStatus,
    TOTAL_SURFACE_CREDITS,
};
pub use video::AndroidVideoSink;
