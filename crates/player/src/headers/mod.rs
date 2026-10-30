//! What a stream's codec headers say beyond the container: the H.264/HEVC
//! parameter sets and VUI (frame rate, pixel shape, colour), HDR10's
//! mastering display and content light SEI, Dolby Vision's configuration
//! from its RPU or record, and the E-AC-3 substreams behind the `dec3` box.
//! The Apple sinks read them to describe a stream to VideoToolbox and
//! CoreAudio; the engine reads the pixel shape and the frame rate.

pub(crate) mod codec;
pub(crate) mod types;
