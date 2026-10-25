//! Android backend for PearTube: AAudio for audio output and master clock,
//! MediaCodec for hardware video decoding, ANativeWindow for video and
//! subtitle presentation.

pub mod audio;
#[forbid(unsafe_code)]
pub mod backend;
pub mod clock;
#[forbid(unsafe_code)]
mod position;
#[forbid(unsafe_code)]
pub mod subtitle;
pub mod surface;
#[forbid(unsafe_code)]
pub mod video;

#[cfg(test)]
#[forbid(unsafe_code)]
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
