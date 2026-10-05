// Ported from FFmpeg libavcodec/mlp_parse.c and libavcodec/mlp_parse.h
// (commit 2da55bf). Licensed under LGPL-2.1-or-later.

//! Major sync header parsing (`ff_mlp_read_major_sync`).

use crate::bitreader::BitReader;
use crate::common::{mlp_samplerate, truehd_channels, truehd_layout};
use crate::crc;
use oxideav_core::Error;
use crate::tables::{MLP_CHANNELS, MLP_LAYOUT, MLP_QUANTS};

/// `MLPHeaderInfo`.
#[derive(Clone, Copy, Debug, Default)]
pub struct MlpHeaderInfo {
    /// 0xBB for MLP, 0xBA for TrueHD.
    pub stream_type: u8,
    /// Size of the major sync header, in bytes.
    pub header_size: usize,

    pub group1_bits: u32,
    pub group2_bits: u32,

    pub group1_samplerate: u32,
    pub group2_samplerate: u32,

    /// The 5-bit (or 13-bit for TrueHD stream 2) channel arrangement field
    /// FFmpeg records for the `needs_reordering` check (18..=20).
    pub channel_arrangement: u32,

    /// Ratebits field: `(in & 8 ? 44100 : 48000) << (in & 7)` derives the
    /// rate; `access_unit_size = 40 << (ratebits & 7)`.
    pub ratebits: u32,

    pub channel_modifier_thd_stream0: u32,
    pub channel_modifier_thd_stream1: u32,
    pub channel_modifier_thd_stream2: u32,

    pub channels_mlp: u32,
    pub channels_thd_stream1: u32,
    pub channels_thd_stream2: u32,
    pub channel_layout_mlp: u64,
    pub channel_layout_thd_stream1: u64,
    pub channel_layout_thd_stream2: u64,

    pub access_unit_size: u32,
    pub access_unit_size_pow2: u32,

    pub is_vbr: bool,
    pub peak_bitrate: u32,

    pub num_substreams: u32,

    pub extended_substream_info: u32,
    pub substream_info: u32,
}

/// `mlp_get_major_sync_size`; a too-short buffer maps to `None`.
#[inline]
pub fn major_sync_size(buf: &[u8]) -> Option<usize> {
    if buf.len() < 28 {
        return None;
    }
    let mut size = 28usize;
    if u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) == 0xf872_6fba {
        let has_extension = buf[25] & 1 != 0;
        if has_extension {
            let extensions = buf[26] >> 4;
            size += 2 + extensions as usize * 2;
        }
    }
    Some(size)
}

/// `ff_mlp_read_major_sync`. `buf` is the packet from the major sync start
/// (the caller has checked the sync word); `gb` must be freshly initialised
/// at that same position.
pub fn read_major_sync(buf: &[u8], gb: &mut BitReader) -> oxideav_core::Result<MlpHeaderInfo> {
    let header_size = major_sync_size(buf).ok_or_else(|| {
        Error::InvalidData("mlp: packet too short for major sync".into())
    })?;
    if gb.bits_left() < header_size * 8 {
        return Err(Error::InvalidData(
            "mlp: packet too short, unable to read major sync".into(),
        ));
    }

    // ff_mlp_checksum16(gb->buffer, header_size - 2) compared against
    // AV_RL16(gb->buffer + header_size - 2).
    let checksum = crc::checksum16(&buf[..header_size - 2]);
    let stored = u16::from_le_bytes([buf[header_size - 2], buf[header_size - 1]]);
    if checksum != stored {
        return Err(Error::InvalidData(
            "mlp: major sync info header checksum error".into(),
        ));
    }

    if gb.get_bits(24) != 0x00f8_726f {
        return Err(Error::InvalidData("mlp: bad sync word".into()));
    }

    let stream_type = gb.get_bits(8) as u8;
    let mut mh = MlpHeaderInfo {
        stream_type,
        header_size,
        ..MlpHeaderInfo::default()
    };

    if mh.stream_type == 0xbb {
        mh.group1_bits = u32::from(MLP_QUANTS[gb.get_bits(4) as usize]);
        mh.group2_bits = u32::from(MLP_QUANTS[gb.get_bits(4) as usize]);

        mh.ratebits = gb.get_bits(4);
        mh.group1_samplerate = mlp_samplerate(mh.ratebits);
        mh.group2_samplerate = mlp_samplerate(gb.get_bits(4));

        gb.skip(11);

        let channel_arrangement = gb.get_bits(5);
        mh.channel_arrangement = channel_arrangement;
        mh.channels_mlp = u32::from(MLP_CHANNELS[channel_arrangement as usize]);
        mh.channel_layout_mlp = MLP_LAYOUT[channel_arrangement as usize];
    } else if mh.stream_type == 0xba {
        mh.group1_bits = 24; // TODO: Is this information actually conveyed anywhere?
        mh.group2_bits = 0;

        mh.ratebits = gb.get_bits(4);
        mh.group1_samplerate = mlp_samplerate(mh.ratebits);
        mh.group2_samplerate = 0;

        gb.skip(4);

        mh.channel_modifier_thd_stream0 = gb.get_bits(2);
        mh.channel_modifier_thd_stream1 = gb.get_bits(2);

        let channel_arrangement = gb.get_bits(5);
        mh.channel_arrangement = channel_arrangement;
        mh.channels_thd_stream1 = truehd_channels(channel_arrangement);
        mh.channel_layout_thd_stream1 = truehd_layout(channel_arrangement);

        mh.channel_modifier_thd_stream2 = gb.get_bits(2);

        let channel_arrangement = gb.get_bits(13);
        mh.channels_thd_stream2 = truehd_channels(channel_arrangement);
        mh.channel_layout_thd_stream2 = truehd_layout(channel_arrangement);
    } else {
        return Err(Error::InvalidData("mlp: unknown stream type".into()));
    }

    mh.access_unit_size = 40 << (mh.ratebits & 7);
    mh.access_unit_size_pow2 = 64 << (mh.ratebits & 7);

    gb.skip(48);

    mh.is_vbr = gb.get_bits(1) != 0;
    // FFmpeg computes `(bits15 * samplerate + 8) >> 4` in 32-bit int, which
    // overflows for CRC-valid headers (32767 × 192 kHz). Compute in u64 and
    // truncate to the same 32-bit result on wrap.
    let product = u64::from(gb.get_bits(15)) * u64::from(mh.group1_samplerate) + 8;
    mh.peak_bitrate = ((product >> 4) & 0xFFFF_FFFF) as u32;
    mh.num_substreams = gb.get_bits(4);

    gb.skip(2);
    mh.extended_substream_info = gb.get_bits(2);
    mh.substream_info = gb.get_bits(8);

    gb.skip(((header_size - 18) * 8) as u32);

    Ok(mh)
}
