//! Test-only crate for the forked `oxideav-h263` decoder (H.263, H.263+
//! and Intel H.263).
//!
//! The acceptance surface is `tests/reference.rs` (every frame equal to
//! FFmpeg's) and `tests/damage.rs` (damaged input); this crate exists so
//! those tests run inside the peartube-media workspace, through the
//! player's registry, without shipping a decoder crate.

#![forbid(unsafe_code)]
