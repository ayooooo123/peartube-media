//! Closed captions carried in video: ATSC A/53 caption data in H.264,
//! HEVC and MPEG-1/2, and decoders for the EIA-608 and CEA-708 services it
//! carries.
//!
//! | Part | Codec id | Ported from |
//! |------|----------|-------------|
//! | [`extract_a53`], [`CcExtractor`] | — | FFmpeg `atsc_a53.c`, `itut35.c`, `h264_sei.c`, `hevc/sei.c`, `h2645_parse.c`, `mpeg12dec.c` (2da55bf) |
//! | [`CaptionTimeline`] | — | the timing FFmpeg gives the pictures (`best_effort_timestamp`) |
//! | [`eia608`] | `eia_608` | FFmpeg `ccaption_dec.c` (2da55bf) |
//! | [`cea708`] | `cea_708` | VLC `modules/codec/cea708.c`, driven as `cc.c` drives it (vlc-src 2e358f3) |
//!
//! The decoders take packets of `cc_data` triplets (a cc_valid/cc_type
//! byte and two data bytes): the bytes FFmpeg exports per picture as A/53
//! side data and its lavfi `subcc` output carries. No container tag maps
//! to them: a player feeds them the triplets it extracts from video. The
//! registered decoders emit each caption screen as a display state
//! ([`STATE_STYLE`]) the moment it changes, as VLC shows captions: EIA-608
//! in FFmpeg's `real_time` mode, CEA-708 as VLC's decoder outputs it.
//!
//! Every byte comes from untrusted peers: reads are bounds-checked,
//! buffers are capped, and malformed input yields no output or
//! `Error::InvalidData`, never a panic.
#![forbid(unsafe_code)]

pub mod a53;
pub mod cea708;
pub mod eia608;
pub mod timeline;

pub use a53::{extract_a53, CaptionCarrier, CcExtractor};
pub use timeline::CaptionTimeline;

/// The `style_ref` of a cue that is a display state, as captions are: it
/// shows from its start until the next cue of the stream replaces it, or
/// until its end (`i64::MAX`: none). An empty state clears the screen.
pub const STATE_STYLE: &str = "subs-cc:state";

/// The caption services a picture's `cc_data` triplets carry, as the
/// player lists caption tracks: valid EIA-608 pairs that are not padding
/// (cc_type 0 or 1), and valid CEA-708 data (cc_type 2 or 3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Services {
    pub eia608: bool,
    pub cea708: bool,
}

impl Services {
    pub fn of(triplets: &[[u8; 3]]) -> Services {
        let valid = |t: &&[u8; 3]| t[0] & 0x04 != 0;
        Services {
            eia608: triplets.iter().filter(valid).any(|t| t[0] & 0x03 < 2 && (t[1] & 0x7f != 0 || t[2] & 0x7f != 0)),
            cea708: triplets.iter().filter(valid).any(|t| t[0] & 0x03 >= 2),
        }
    }
}

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, MediaType, RuntimeContext};

/// A subtitle decoder's capabilities: decode-only, intra-only.
fn caps(implementation: &str) -> CodecCapabilities {
    CodecCapabilities {
        decode: true,
        encode: false,
        media_type: MediaType::Subtitle,
        intra_only: true,
        lossy: false,
        lossless: true,
        hardware_accelerated: false,
        implementation: implementation.into(),
        // Capability metadata only: `first_decoder` takes registration order.
        priority: 50,
        max_width: None,
        max_height: None,
        max_bitrate: None,
        max_sample_rate: None,
        max_channels: None,
        accepted_pixel_formats: Vec::new(),
        ..CodecCapabilities::audio(String::new())
    }
}

/// Registers the `eia_608` and `cea_708` decoders.
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(CodecInfo::new(CodecId::new(eia608::CODEC_ID)).capabilities(caps("eia_608_sw")).decoder(eia608::make_decoder));
    reg.register(CodecInfo::new(CodecId::new(cea708::CODEC_ID)).capabilities(caps("cea_708_sw")).decoder(cea708::make_decoder));
}

/// Unified registration entry point.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

oxideav_core::register!("subs-cc", register);
