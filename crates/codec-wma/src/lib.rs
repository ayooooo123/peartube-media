// Windows Media Audio decoders (wmav1, wmav2, wmapro, wmalossless, wmavoice).
// Ported from FFmpeg (commit 2da55bf): libavcodec/wmadec.c, wma.c, wma_common.c,
// wmaprodec.c, wmalosslessdec.c, wmavoice.c and their tables/support files.
// GNU Lesser General Public License 2.1 or later.

#![forbid(unsafe_code)]

pub mod bits;
pub mod celp;
pub mod fft;
pub mod getbits;
pub mod tables;
pub mod tx;
pub mod vlc;
pub mod wma;
pub mod wma_common;
pub mod wmalossless;
pub mod wmapro;
pub mod wmapro_tables;
pub mod wmavoice;
pub mod wmavoice_tables;

pub mod lib_registration;
pub use lib_registration::register;

oxideav_core::register!("codec-wma", register);
