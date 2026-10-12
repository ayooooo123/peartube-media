// The AudioSpecificConfig and ALSSpecificConfig an ALS stream starts from.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/mpeg4audio.c
// (ff_mpeg4audio_get_config_gb and parse_config_ALS: the offset of the
// ALS config and the rate and channels it overrides) and
// libavcodec/alsdec.c (read_specific_config).
// Copyright (c) 2008 Baptiste Coudurier (mpeg4audio.c), (c) 2009 Thilo
// Borgmann (alsdec.c); LGPL-2.1-or-later (see LICENSE).

use oxideav_core::{Error, Result};

use crate::bits::{Bits, av_ceil_log2};

/// `AOT_ALS`
pub(crate) const AOT_ALS: u32 = 36;
/// The most channels the decoder takes (the contract's cap; FFmpeg's is
/// 512).
pub(crate) const MAX_CHANNELS: usize = 64;

/// `ff_mpeg4audio_sample_rates`
const SAMPLE_RATES: [i32; 16] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350, 0, 0, 0];
/// `ff_mpeg4audio_channels`
const CHANNELS: [i32; 15] = [0, 1, 2, 3, 4, 5, 6, 8, 0, 0, 0, 7, 8, 24, 8];

fn invalid(what: &str) -> Error {
    Error::invalid(format!("als: {what}"))
}

/// `get_object_type`
fn object_type(gb: &mut Bits) -> u32 {
    let t = gb.get(5);
    if t == 31 { 32 + gb.get(6) } else { t }
}

/// `get_sample_rate`
fn sample_rate(gb: &mut Bits) -> i32 {
    let index = gb.get(4) as usize;
    if index == 15 { gb.get(24) as i32 } else { SAMPLE_RATES[index] }
}

/// The object type of an AudioSpecificConfig (for tag resolution), after
/// an SBR/PS extension as FFmpeg reads one.
pub(crate) fn audio_object_type(asc: &[u8]) -> Option<u32> {
    if asc.is_empty() {
        return None;
    }
    let mut gb = Bits::new(asc);
    let mut t = object_type(&mut gb);
    sample_rate(&mut gb);
    let chan_config = gb.get(4);
    if chan_config as usize >= CHANNELS.len() {
        return None;
    }
    if t == 5 || (t == 29 && !(gb.show(3) & 0x03 != 0 && gb.show(9) & 0x3F == 0)) {
        sample_rate(&mut gb);
        t = object_type(&mut gb);
    }
    Some(t)
}

/// `avpriv_mpeg4audio_get_config2` as alsdec.c uses it: the bit offset of
/// the object's specific config, and the rate and channels (an ALS config
/// overrides the AudioSpecificConfig's).
fn mpeg4audio_config(asc: &[u8]) -> Result<(usize, i32, i32)> {
    if asc.is_empty() {
        return Err(invalid("no AudioSpecificConfig"));
    }
    let mut gb = Bits::new(asc);
    let mut t = object_type(&mut gb);
    let mut rate = sample_rate(&mut gb);
    let chan_config = gb.get(4) as usize;
    let Some(&table_channels) = CHANNELS.get(chan_config) else {
        return Err(invalid(&format!("invalid chan_config {chan_config}")));
    };
    let mut channels = table_channels;
    if t == 5 || (t == 29 && !(gb.show(3) & 0x03 != 0 && gb.show(9) & 0x3F == 0)) {
        sample_rate(&mut gb);
        t = object_type(&mut gb);
        if t == 22 {
            gb.get(4);
        }
    }
    let mut specific = gb.count();
    if t == AOT_ALS {
        gb.skip(5);
        if gb.show(24) != 0x0041_4C53 {
            gb.skip(24);
        }
        specific = gb.count();
        // parse_config_ALS
        if gb.left() < 112 {
            return Err(invalid("ALS config too short"));
        }
        if gb.get_long(32) != u32::from_be_bytes(*b"ALS\0") {
            return Err(invalid("no ALS config"));
        }
        rate = gb.get_long(32) as i32;
        if rate <= 0 {
            return Err(invalid(&format!("invalid sample rate {rate}")));
        }
        gb.skip(32);
        channels = gb.get(16) as i32 + 1;
    }
    Ok((specific, rate, channels))
}

/// `ALSSpecificConfig`, with the rate and channel count the decoder uses.
#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) sample_rate: u32,
    pub(crate) channels: usize,
    pub(crate) samples: u32,
    pub(crate) resolution: u32,
    pub(crate) floating: bool,
    pub(crate) frame_length: u32,
    pub(crate) ra_distance: u32,
    pub(crate) ra_flag: u32,
    pub(crate) adapt_order: bool,
    pub(crate) coef_table: u32,
    pub(crate) long_term_prediction: bool,
    pub(crate) max_order: u32,
    pub(crate) block_switching: u32,
    pub(crate) bgmc: bool,
    pub(crate) sb_part: bool,
    pub(crate) joint_stereo: bool,
    pub(crate) mc_coding: bool,
    pub(crate) rlslms: bool,
    /// `chan_pos` when channel sorting is on and valid (`cs_switch`).
    pub(crate) chan_pos: Option<Vec<usize>>,
}

/// `read_specific_config`
pub(crate) fn read_specific_config(extradata: &[u8]) -> Result<Config> {
    let (offset, rate, channels) = mpeg4audio_config(extradata)?;
    let mut gb = Bits::new(extradata);
    gb.skip(offset as i64);
    if gb.left() < 30 << 3 {
        return Err(invalid("ALS config too short"));
    }
    let als_id = gb.get_long(32);
    gb.skip(32);
    let samples = gb.get_long(32);
    gb.skip(16);
    gb.skip(3);
    let resolution = gb.get(3);
    let floating = gb.bit() != 0;
    let _msb_first = gb.bit();
    let frame_length = gb.get(16) + 1;
    let ra_distance = gb.get(8);
    let ra_flag = gb.get(2);
    let adapt_order = gb.bit() != 0;
    let coef_table = gb.get(2);
    let long_term_prediction = gb.bit() != 0;
    let max_order = gb.get(10);
    let block_switching = gb.get(2);
    let bgmc = gb.bit() != 0;
    let sb_part = gb.bit() != 0;
    let joint_stereo = gb.bit() != 0;
    let mc_coding = gb.bit() != 0;
    let chan_config = gb.bit() != 0;
    let chan_sort = gb.bit() != 0;
    let crc_enabled = gb.bit() != 0;
    let rlslms = gb.bit() != 0;
    gb.skip(5);
    gb.skip(1);

    if als_id != u32::from_be_bytes(*b"ALS\0") {
        return Err(invalid("no ALS config"));
    }
    if channels <= 0 {
        return Err(invalid("no channels"));
    }
    let channels = channels as usize;
    if channels > MAX_CHANNELS {
        return Err(Error::unsupported(format!("als: {channels} channels (at most {MAX_CHANNELS})")));
    }
    if chan_config {
        gb.get(16);
    }
    let mut chan_pos = None;
    if chan_sort && channels > 1 {
        let bits = av_ceil_log2(channels as u32);
        if gb.left() < (channels as u32 * bits + 7) as i64 {
            return Err(invalid("channel positions past the config"));
        }
        let mut pos = vec![usize::MAX; channels];
        let mut valid = true;
        for i in 0..channels {
            let idx = gb.get_long(bits) as usize;
            if idx >= channels || pos[idx] != usize::MAX {
                // "Invalid channel reordering": FFmpeg plays them unsorted.
                valid = false;
                break;
            }
            pos[idx] = i;
        }
        if valid {
            chan_pos = Some(pos);
        }
        gb.align();
    }
    if gb.left() < 64 {
        return Err(invalid("ALS config too short"));
    }
    let header_size = gb.get_long(32);
    let trailer_size = gb.get_long(32);
    let header_size = if header_size == u32::MAX { 0 } else { u64::from(header_size) };
    let trailer_size = if trailer_size == u32::MAX { 0 } else { u64::from(trailer_size) };
    let ht_size = (header_size + trailer_size) << 3;
    if gb.left() < ht_size as i64 {
        return Err(invalid("header and trailer past the config"));
    }
    if ht_size > i32::MAX as u64 {
        return Err(Error::unsupported("als: header and trailer too large"));
    }
    gb.skip(ht_size as i64);
    if crc_enabled && gb.left() < 32 {
        return Err(invalid("CRC past the config"));
    }
    if rlslms {
        return Err(Error::unsupported("als: adaptive RLS-LMS prediction"));
    }
    if !floating && (resolution + 1) * 8 > 32 {
        return Err(invalid("bits per raw sample larger than 32"));
    }
    Ok(Config {
        sample_rate: rate as u32,
        channels,
        samples,
        resolution,
        floating,
        frame_length,
        ra_distance,
        ra_flag,
        adapt_order,
        coef_table,
        long_term_prediction,
        max_order,
        block_switching,
        bgmc,
        sb_part,
        joint_stereo,
        mc_coding,
        rlslms,
        chan_pos,
    })
}
