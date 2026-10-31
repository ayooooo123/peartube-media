//! VideoToolbox's supplemental decoders, for the decode route (`video.rs`).
//!
//! VP9 and AV1 decoders are "supplemental": VideoToolbox lists them only
//! after `VTRegisterSupplementalVideoDecoderIfAvailable`. That function
//! exists on macOS 11+ and iOS/iPadOS 26.2+, so it is looked up at run time
//! instead of linked: on an older iOS the lookup fails and nothing is
//! registered.

use std::ffi::c_void;

use parking_lot::Mutex;

/// `kCMVideoCodecType_VP9`.
pub(crate) const CODEC_VP9: u32 = u32::from_be_bytes(*b"vp09");
/// `kCMVideoCodecType_AV1`.
pub(crate) const CODEC_AV1: u32 = u32::from_be_bytes(*b"av01");

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTIsHardwareDecodeSupported(codec: u32) -> u8;
}

/// Codec types already passed to the registration function (or found to
/// have none to pass to). Registration is idempotent; this only avoids
/// repeating the symbol lookup and the call.
static REGISTERED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Makes a supplemental decoder for `codec` visible to VideoToolbox when the
/// OS has one. Safe to call repeatedly and from any thread.
pub(crate) fn register_supplemental(codec: u32) {
    let mut registered = REGISTERED.lock();
    if registered.contains(&codec) {
        return;
    }
    registered.push(codec);
    // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name only reads.
    let symbol = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            c"VTRegisterSupplementalVideoDecoderIfAvailable".as_ptr(),
        )
    };
    if symbol.is_null() {
        return;
    }
    // SAFETY: the symbol is VideoToolbox's
    // `void VTRegisterSupplementalVideoDecoderIfAvailable(CMVideoCodecType)`.
    let register: unsafe extern "C" fn(u32) = unsafe { std::mem::transmute::<*mut c_void, _>(symbol) };
    unsafe { register(codec) };
}

/// Whether this device has a hardware decoder for `codec`. Registers a supplemental decoder
/// first, since VP9 and AV1 report false until then.
pub(crate) fn hardware_decode_supported(codec: u32) -> bool {
    register_supplemental(codec);
    // SAFETY: plain query of a codec type; no pointers involved.
    unsafe { VTIsHardwareDecodeSupported(codec) != 0 }
}
