//! Runtime-font text rendering and the safe Rust libass 0.17.5 port.
//! No font files are embedded. Font discovery, shaping and outline caches
//! belong to a renderer; script and container fonts are tried before OS fonts.
#![forbid(unsafe_code)]

mod arabic_charmap;
pub mod bitmap;
pub mod drawing;
pub mod fontselect;
pub mod outline;
pub mod parse;
pub mod render;
mod sfnt;
pub mod shaper;
pub mod track;
mod utils;

pub use fontselect::FontOptions;
pub use render::{Frame, Image, Renderer};
pub use track::Track;
