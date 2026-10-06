//! Bitmap subtitle decoders for peartube-media.

#![forbid(unsafe_code)]

use oxideav_core::RuntimeContext;

/// Registers the bitmap subtitle decoders.
pub fn register(_ctx: &mut RuntimeContext) {}

oxideav_core::register!("subs-bitmap", register);
