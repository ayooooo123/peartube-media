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

pub(crate) fn log_audio_error(message: &str) {
    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(priority: std::ffi::c_int, tag: *const std::ffi::c_char, text: *const std::ffi::c_char) -> std::ffi::c_int;
    }
    if let Ok(message) = std::ffi::CString::new(message.replace('\0', "\\0")) {
        // SAFETY: both strings are NUL-terminated and live through the call.
        unsafe { __android_log_write(5, c"peartube-audio".as_ptr(), message.as_ptr()); }
    }
}
pub use video::AndroidVideoSink;
