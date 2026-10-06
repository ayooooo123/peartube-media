// Ported from FFmpeg libavcodec/dca_exss.c and dca_exss.h (commit
// 2da55bf), LGPL-2.1-or-later.

//! Extension substream (EXSS) parser: audio asset descriptors for DTS-HD.
//! Only single-presentation / single-asset streams decode (matching
//! FFmpeg, which errors on more).

use crate::bitreader::BitReader;
use crate::data::FF_DCA_SAMPLING_FREQS;
use crate::dca::{count_chs_for_mask, exss_mask};

pub const DCA_EXSS_CHANNELS_MAX: usize = 8;
pub const DCA_EXSS_CHSETS_MAX: usize = 4;

/// `DCAExssAsset`.
#[derive(Clone, Debug, Default)]
pub struct ExssAsset {
    pub asset_offset: usize,
    pub asset_size: usize,
    pub asset_index: i32,

    pub pcm_bit_res: i32,
    pub max_sample_rate: i32,
    pub nchannels_total: i32,
    pub one_to_one_map_ch_to_spkr: bool,
    pub embedded_stereo: bool,
    pub embedded_6ch: bool,
    pub spkr_mask_enabled: bool,
    pub spkr_mask: u32,
    pub representation_type: i32,

    pub coding_mode: i32,
    pub extension_mask: i32,

    pub core_offset: usize,
    pub core_size: usize,
    pub xbr_offset: usize,
    pub xbr_size: usize,
    pub xxch_offset: usize,
    pub xxch_size: usize,
    pub x96_offset: usize,
    pub x96_size: usize,
    pub lbr_offset: usize,
    pub lbr_size: usize,
    pub xll_offset: usize,
    pub xll_size: usize,
    pub xll_sync_present: bool,
    pub xll_delay_nframes: u32,
    pub xll_sync_offset: usize,

    pub hd_stream_id: i32,
}

/// `DCAExssParser`.
#[derive(Debug, Default)]
pub struct ExssParser {
    pub exss_index: i32,
    pub exss_size_nbits: i32,
    pub exss_size: usize,

    pub static_fields_present: bool,
    pub npresents: i32,
    pub nassets: i32,

    pub mix_metadata_enabled: bool,
    pub nmixoutconfigs: i32,
    pub nmixoutchs: [i32; 4],

    pub assets: [ExssAsset; 1],
}

/// Errors the parse can return (message strings mirror the C av_logs).
pub type ExssResult<T> = Result<T, &'static str>;

fn parse_xll_parameters(gb: &mut BitReader, asset: &mut ExssAsset, size_nbits: u32) {
    // Size of XLL data in extension substream
    asset.xll_size = gb.get_bits(size_nbits) as usize + 1;

    // XLL sync word present flag
    asset.xll_sync_present = gb.get_bits(1) != 0;
    if asset.xll_sync_present {
        // Peak bit rate smoothing buffer size
        gb.skip(4);

        // Number of bits for XLL decoding delay
        let xll_delay_nbits = gb.get_bits(5) + 1;

        // Initial XLL decoding delay in frames
        asset.xll_delay_nframes = gb.get_bits_long(xll_delay_nbits);

        // Number of bytes offset to XLL sync
        asset.xll_sync_offset = gb.get_bits(size_nbits) as usize;
    } else {
        asset.xll_delay_nframes = 0;
        asset.xll_sync_offset = 0;
    }
}

fn parse_lbr_parameters(gb: &mut BitReader, asset: &mut ExssAsset) {
    // Size of LBR component in extension substream
    asset.lbr_size = gb.get_bits(14) as usize + 1;

    // LBR sync word present flag
    if gb.get_bits(1) != 0 {
        // LBR sync distance
        gb.skip(2);
    }
}

fn popcount(v: u32) -> i32 {
    v.count_ones() as i32
}

fn parse_descriptor(gb: &mut BitReader, asset: &mut ExssAsset, parser: &ExssParser) -> ExssResult<()> {
    let descr_pos = gb.bits_read();

    // Size of audio asset descriptor in bytes
    let descr_size = gb.get_bits(9) as usize + 1;

    // Audio asset identifier
    asset.asset_index = gb.get_bits(3) as i32;

    // Per stream static metadata
    if parser.static_fields_present {
        // Asset type descriptor presence
        if gb.get_bits(1) != 0 {
            // Asset type descriptor
            gb.skip(4);
        }

        // Language descriptor presence
        if gb.get_bits(1) != 0 {
            // Language descriptor
            gb.skip(24);
        }

        // Additional textual information presence
        if gb.get_bits(1) != 0 {
            // Byte size of additional text info
            let text_size = gb.get_bits(10) as usize + 1;

            // Sanity check available size
            if (gb.bits_left() as usize) < text_size * 8 {
                return Err("additional text info too long");
            }

            // Additional textual information string
            gb.skip((text_size * 8) as u32);
        }

        // PCM bit resolution
        asset.pcm_bit_res = gb.get_bits(5) as i32 + 1;

        // Maximum sample rate
        asset.max_sample_rate = FF_DCA_SAMPLING_FREQS[gb.get_bits(4) as usize] as i32;

        // Total number of channels
        asset.nchannels_total = gb.get_bits(8) as i32 + 1;

        // One to one map channel to speakers
        asset.one_to_one_map_ch_to_spkr = gb.get_bits(1) != 0;
        if asset.one_to_one_map_ch_to_spkr {
            let mut spkr_mask_nbits = 0u32;

            // Embedded stereo flag
            asset.embedded_stereo = asset.nchannels_total > 2 && gb.get_bits(1) != 0;

            // Embedded 6 channels flag
            asset.embedded_6ch = asset.nchannels_total > 6 && gb.get_bits(1) != 0;

            // Speaker mask enabled flag
            asset.spkr_mask_enabled = gb.get_bits(1) != 0;
            if asset.spkr_mask_enabled {
                // Number of bits for speaker activity mask
                spkr_mask_nbits = (gb.get_bits(2) + 1) << 2;

                // Loudspeaker activity mask
                asset.spkr_mask = gb.get_bits(spkr_mask_nbits);
            }

            // Number of speaker remapping sets
            let spkr_remap_nsets = gb.get_bits(3);
            if spkr_remap_nsets != 0 && spkr_mask_nbits == 0 {
                return Err("speaker mask disabled yet there are remapping sets");
            }

            // Standard loudspeaker layout mask
            let mut nspeakers = [0i32; 8];
            for item in nspeakers.iter_mut().take(spkr_remap_nsets as usize) {
                *item = count_chs_for_mask(gb.get_bits(spkr_mask_nbits));
            }

            for &nspk in nspeakers.iter().take(spkr_remap_nsets as usize) {
                // Number of channels to be decoded for speaker remapping
                let nch_for_remaps = gb.get_bits(5);

                for _ in 0..nspk {
                    // Decoded channels to output speaker mapping mask
                    let remap_ch_mask = gb.get_bits_long(nch_for_remaps);

                    // Loudspeaker remapping codes
                    gb.skip((popcount(remap_ch_mask) * 5) as u32);
                }
            }
        } else {
            asset.embedded_stereo = false;
            asset.embedded_6ch = false;
            asset.spkr_mask_enabled = false;
            asset.spkr_mask = 0;

            // Representation type
            asset.representation_type = gb.get_bits(3) as i32;
        }
    }

    // DRC, DNC and mixing metadata

    // Dynamic range coefficient presence flag
    let drc_present = gb.get_bits(1) != 0;

    // Code for dynamic range coefficient
    if drc_present {
        gb.skip(8);
    }

    // Dialog normalization presence flag
    if gb.get_bits(1) != 0 {
        // Dialog normalization code
        gb.skip(5);
    }

    // DRC for stereo downmix
    if drc_present && asset.embedded_stereo {
        gb.skip(8);
    }

    // Mixing metadata presence flag
    if parser.mix_metadata_enabled && gb.get_bits(1) != 0 {
        // External mixing flag
        gb.skip(1);

        // Post mixing / replacement gain adjustment
        gb.skip(6);

        // DRC prior to mixing
        if gb.get_bits(2) == 3 {
            // Custom code for mixing DRC
            gb.skip(8);
        } else {
            // Limit for mixing DRC
            gb.skip(3);
        }

        // Scaling type for channels of main audio
        // Scaling parameters of main audio
        if gb.get_bits(1) != 0 {
            for i in 0..parser.nmixoutconfigs as usize {
                gb.skip((6 * parser.nmixoutchs[i]) as u32);
            }
        } else {
            gb.skip((6 * parser.nmixoutconfigs) as u32);
        }

        let mut nchannels_dmix = asset.nchannels_total;
        if asset.embedded_6ch {
            nchannels_dmix += 6;
        }
        if asset.embedded_stereo {
            nchannels_dmix += 2;
        }

        for i in 0..parser.nmixoutconfigs as usize {
            if parser.nmixoutchs[i] == 0 {
                return Err("invalid speaker layout mask for mixing configuration");
            }
            for _ in 0..nchannels_dmix {
                // Mix output mask
                let mix_map_mask = gb.get_bits(parser.nmixoutchs[i] as u32);

                // Mixing coefficients
                gb.skip((popcount(mix_map_mask) * 6) as u32);
            }
        }
    }

    // Decoder navigation data

    // Coding mode for the asset
    asset.coding_mode = gb.get_bits(2) as i32;

    // Coding components used in asset
    match asset.coding_mode {
        0 => {
            // Coding mode that may contain multiple coding components
            asset.extension_mask = gb.get_bits(12) as i32;

            if asset.extension_mask & exss_mask::CORE != 0 {
                // Size of core component in extension substream
                asset.core_size = gb.get_bits(14) as usize + 1;
                // Core sync word present flag
                let sync = gb.get_bits(1);
                if sync != 0 {
                    // Core sync distance
                    gb.skip(2);
                }
            }

            if asset.extension_mask & exss_mask::XBR != 0 {
                // Size of XBR extension in extension substream
                asset.xbr_size = gb.get_bits(14) as usize + 1;
            }

            if asset.extension_mask & exss_mask::EXSS_XXCH != 0 {
                // Size of XXCH extension in extension substream
                asset.xxch_size = gb.get_bits(14) as usize + 1;
            }

            if asset.extension_mask & exss_mask::EXSS_X96 != 0 {
                // Size of X96 extension in extension substream
                asset.x96_size = gb.get_bits(12) as usize + 1;
            }

            if asset.extension_mask & exss_mask::LBR != 0 {
                parse_lbr_parameters(gb, asset);
            }

            if asset.extension_mask & exss_mask::XLL != 0 {
                parse_xll_parameters(gb, asset, parser.exss_size_nbits as u32);
            }

            if asset.extension_mask & exss_mask::RSV1 != 0 {
                gb.skip(16);
            }

            if asset.extension_mask & exss_mask::RSV2 != 0 {
                gb.skip(16);
            }
        }
        1 => {
            // Loss-less coding mode without CBR component
            asset.extension_mask = exss_mask::XLL;
            parse_xll_parameters(gb, asset, parser.exss_size_nbits as u32);
        }
        2 => {
            // Low bit rate mode
            asset.extension_mask = exss_mask::LBR;
            parse_lbr_parameters(gb, asset);
        }
        _ => {
            // Auxiliary coding mode
            asset.extension_mask = 0;

            // Size of auxiliary coded data
            gb.skip(14);

            // Auxiliary codec identification
            gb.skip(8);

            // Aux sync word present flag
            if gb.get_bits(1) != 0 {
                // Aux sync distance
                gb.skip(3);
            }
        }
    }

    if asset.extension_mask & exss_mask::XLL != 0 {
        // DTS-HD stream ID
        asset.hd_stream_id = gb.get_bits(3) as i32;
    }

    // One to one mixing flag, per channel main audio scaling flag, main
    // audio scaling codes, decode asset in secondary decoder flag,
    // revision 2 DRC metadata, reserved, zero pad
    if !gb.seek_bits(descr_pos + descr_size * 8) {
        return Err("read past end of EXSS asset descriptor");
    }

    Ok(())
}

fn set_exss_offsets(asset: &mut ExssAsset) -> ExssResult<()> {
    let mut offs = asset.asset_offset;
    let mut size = asset.asset_size;

    if asset.extension_mask & exss_mask::CORE != 0 {
        asset.core_offset = offs;
        if asset.core_size > size {
            return Err("invalid core size in EXSS asset");
        }
        offs += asset.core_size;
        size -= asset.core_size;
    }

    if asset.extension_mask & exss_mask::XBR != 0 {
        asset.xbr_offset = offs;
        if asset.xbr_size > size {
            return Err("invalid XBR size in EXSS asset");
        }
        offs += asset.xbr_size;
        size -= asset.xbr_size;
    }

    if asset.extension_mask & exss_mask::EXSS_XXCH != 0 {
        asset.xxch_offset = offs;
        if asset.xxch_size > size {
            return Err("invalid XXCH size in EXSS asset");
        }
        offs += asset.xxch_size;
        size -= asset.xxch_size;
    }

    if asset.extension_mask & exss_mask::EXSS_X96 != 0 {
        asset.x96_offset = offs;
        if asset.x96_size > size {
            return Err("invalid X96 size in EXSS asset");
        }
        offs += asset.x96_size;
        size -= asset.x96_size;
    }

    if asset.extension_mask & exss_mask::LBR != 0 {
        asset.lbr_offset = offs;
        if asset.lbr_size > size {
            return Err("invalid LBR size in EXSS asset");
        }
        offs += asset.lbr_size;
        size -= asset.lbr_size;
    }

    if asset.extension_mask & exss_mask::XLL != 0 {
        asset.xll_offset = offs;
        if asset.xll_size > size {
            return Err("invalid XLL size in EXSS asset");
        }
        // offs += asset.xll_size;  // last component; offs not read again
    }

    Ok(())
}

/// `ff_dca_exss_parse`. `data` starts at the EXSS sync word.
pub fn exss_parse(parser: &mut ExssParser, data: &[u8]) -> ExssResult<()> {
    let mut gb = BitReader::new(data);

    // Extension substream sync word
    gb.skip(32);

    // User defined bits
    gb.skip(8);

    // Extension substream index
    parser.exss_index = gb.get_bits(2) as i32;

    // Flag indicating short or long header size
    let wide_hdr = gb.get_bits(1) != 0;

    // Extension substream header length
    let header_size = gb.get_bits(8 + u32::from(wide_hdr) * 4) as usize + 1;

    parser.exss_size_nbits = 16 + i32::from(wide_hdr) * 4;

    // Number of bytes of extension substream
    parser.exss_size = gb.get_bits(parser.exss_size_nbits as u32) as usize + 1;
    if parser.exss_size > data.len() {
        return Err("packet too short for EXSS frame");
    }

    // Per stream static fields presence flag
    parser.static_fields_present = gb.get_bits(1) != 0;
    if parser.static_fields_present {
        let mut active_exss_mask = [0u32; 8];

        // Reference clock code
        gb.skip(2);

        // Extension substream frame duration
        gb.skip(3);

        // Timecode presence flag
        if gb.get_bits(1) != 0 {
            // Timecode data
            gb.skip(36);
        }

        // Number of defined audio presentations
        parser.npresents = gb.get_bits(3) as i32 + 1;
        if parser.npresents > 1 {
            return Err("multiple audio presentations not supported");
        }

        // Number of audio assets in extension substream
        parser.nassets = gb.get_bits(3) as i32 + 1;
        if parser.nassets > 1 {
            return Err("multiple audio assets not supported");
        }

        // Active extension substream mask for audio presentation
        for item in active_exss_mask.iter_mut().take(parser.npresents as usize) {
            *item = gb.get_bits(parser.exss_index as u32 + 1);
        }

        // Active audio asset mask
        for &mask in active_exss_mask.iter().take(parser.npresents as usize) {
            gb.skip((popcount(mask) * 8) as u32);
        }

        // Mixing metadata enable flag
        parser.mix_metadata_enabled = gb.get_bits(1) != 0;
        if parser.mix_metadata_enabled {
            // Mixing metadata adjustment level
            gb.skip(2);

            // Number of bits for mixer output speaker activity mask
            let spkr_mask_nbits = (gb.get_bits(2) + 1) << 2;

            // Number of mixing configurations
            parser.nmixoutconfigs = gb.get_bits(2) as i32 + 1;

            // Speaker layout mask for mixer output channels
            for i in 0..parser.nmixoutconfigs as usize {
                parser.nmixoutchs[i] = count_chs_for_mask(gb.get_bits(spkr_mask_nbits));
            }
        }
    } else {
        parser.npresents = 1;
        parser.nassets = 1;
    }

    // Size of encoded asset data in bytes
    let mut offset = header_size;
    for i in 0..parser.nassets as usize {
        parser.assets[i].asset_offset = offset;
        parser.assets[i].asset_size = gb.get_bits(parser.exss_size_nbits as u32) as usize + 1;
        offset += parser.assets[i].asset_size;
        if offset > parser.exss_size {
            return Err("EXSS asset out of bounds");
        }
    }

    // Audio asset descriptor
    for i in 0..parser.nassets as usize {
        let mut asset = std::mem::take(&mut parser.assets[i]);
        let res = parse_descriptor(&mut gb, &mut asset, parser).and_then(|()| set_exss_offsets(&mut asset));
        parser.assets[i] = asset;
        res?;
    }

    // Backward compatible core present, core substream index, core asset
    // index, reserved, byte align, CRC16 of extension substream header
    if !gb.seek_bits(header_size * 8) {
        return Err("read past end of EXSS header");
    }

    Ok(())
}

/// `DCAExssAsset` with per-component byte offsets resolved.
impl ExssAsset {
    pub fn has_xll(&self) -> bool {
        self.extension_mask & crate::dca::exss_mask::XLL != 0
    }
    pub fn has_lbr(&self) -> bool {
        self.extension_mask & crate::dca::exss_mask::LBR != 0
    }
}
