//! Test-only helpers for the `check-mpeg4` reference tests.
//!
//! The acceptance surface is the FATE/FFmpeg reference comparisons in
//! `tests/reference.rs`; this crate exists so those tests run inside the
//! peartube-media workspace without shipping a decoder crate.

#![forbid(unsafe_code)]
