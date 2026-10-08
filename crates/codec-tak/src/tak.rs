// TAK stream info, frame headers and CRC.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/tak.c and tak.h, and
// libavutil/crc.c's AV_CRC_24_IEEE table and byte loop.
// Copyright (c) 2012 Paul B Mahol (tak.c); LGPL-2.1-or-later (see
// LICENSE).

use std::sync::LazyLock;

use crate::bits::Bits;

pub(crate) const TAK_MAX_CHANNELS: usize = 16;
pub(crate) const TAK_FRAME_FLAG_IS_LAST: u32 = 0x1;
pub(crate) const TAK_FRAME_FLAG_HAS_INFO: u32 = 0x2;
pub(crate) const TAK_FRAME_FLAG_HAS_METADATA: u32 = 0x4;
/// `TAK_MIN_FRAME_HEADER_BYTES`
pub(crate) const TAK_MIN_FRAME_HEADER_BYTES: usize = 8;
/// `TAK_MAX_FRAME_HEADER_BYTES`
pub(crate) const TAK_MAX_FRAME_HEADER_BYTES: usize = 37;
pub(crate) const TAK_CODEC_MONO_STEREO: u32 = 2;
pub(crate) const TAK_CODEC_MULTICHANNEL: u32 = 4;

/// `frame_duration_type_quants`
const FRAME_DURATION_TYPE_QUANTS: [u32; 10] = [3, 4, 6, 8, 4096, 8192, 16384, 512, 1024, 2048];
const TAK_FST_250MS: u32 = 3;
const TAK_FRAME_DURATION_QUANT_SHIFT: u32 = 5;

/// `TAKStreamInfo`: kept across frames, as the decoder and parser keep it
/// (a frame without stream info leaves the fields it does not carry).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StreamInfo {
    pub(crate) flags: u32,
    pub(crate) codec: u32,
    pub(crate) data_type: u32,
    pub(crate) sample_rate: i32,
    pub(crate) channels: usize,
    pub(crate) bps: u32,
    pub(crate) frame_num: u32,
    pub(crate) frame_samples: i32,
    pub(crate) last_frame_samples: i32,
    /// The channel mask (`ch_layout`), one bit per `tak_channel_layouts`
    /// entry 1 to 18 (entry 0 maps to none).
    pub(crate) ch_mask: u32,
    pub(crate) samples: i64,
}

/// `tak_get_nb_samples`
fn get_nb_samples(sample_rate: i32, frame_type: u32) -> Option<i32> {
    let (nb, max) = if frame_type <= TAK_FST_250MS {
        ((sample_rate * FRAME_DURATION_TYPE_QUANTS[frame_type as usize] as i32) >> TAK_FRAME_DURATION_QUANT_SHIFT, 16384)
    } else if (frame_type as usize) < FRAME_DURATION_TYPE_QUANTS.len() {
        (
            FRAME_DURATION_TYPE_QUANTS[frame_type as usize] as i32,
            (sample_rate * FRAME_DURATION_TYPE_QUANTS[TAK_FST_250MS as usize] as i32) >> TAK_FRAME_DURATION_QUANT_SHIFT,
        )
    } else {
        return None;
    };
    (nb > 0 && nb <= max).then_some(nb)
}

/// `tak_parse_streaminfo`: false on an invalid frame size type (the other
/// fields are already stored, as in FFmpeg).
fn parse_streaminfo(s: &mut StreamInfo, gb: &mut Bits) -> bool {
    s.codec = gb.get(6);
    gb.skip(4);
    let frame_type = gb.get(4);
    s.samples = gb.get63(35) as i64;
    s.data_type = gb.get(3);
    s.sample_rate = gb.get(18) as i32 + 6000;
    s.bps = gb.get(5) + 8;
    s.channels = gb.get(4) as usize + 1;
    let mut ch_mask = 0u32;
    if gb.bit() != 0 {
        gb.skip(5);
        if gb.bit() != 0 {
            for _ in 0..s.channels {
                let value = gb.get(6);
                if (1..19).contains(&value) {
                    ch_mask |= 1 << (value - 1);
                }
            }
        }
    }
    s.ch_mask = ch_mask;
    match get_nb_samples(s.sample_rate, frame_type) {
        Some(n) => {
            s.frame_samples = n;
            true
        }
        None => false,
    }
}

/// `avpriv_tak_parse_streaminfo`: the STREAMINFO metadata block.
pub(crate) fn parse_streaminfo_block(buf: &[u8]) -> Option<StreamInfo> {
    let mut s = StreamInfo::default();
    let mut gb = Bits::new(buf);
    parse_streaminfo(&mut s, &mut gb).then_some(s)
}

/// `ff_tak_decode_frame_header`: false when the header is invalid.
pub(crate) fn decode_frame_header(gb: &mut Bits, ti: &mut StreamInfo) -> bool {
    if gb.get(16) != 0xA0FF {
        return false;
    }
    ti.flags = gb.get(3);
    ti.frame_num = gb.get(21);
    if ti.flags & TAK_FRAME_FLAG_IS_LAST != 0 {
        ti.last_frame_samples = gb.get(14) as i32 + 1;
        gb.skip(2);
    } else {
        ti.last_frame_samples = 0;
    }
    if ti.flags & TAK_FRAME_FLAG_HAS_INFO != 0 {
        if !parse_streaminfo(ti, gb) {
            return false;
        }
        if gb.get(6) != 0 {
            gb.skip(25);
        }
        gb.align();
    }
    if ti.flags & TAK_FRAME_FLAG_HAS_METADATA != 0 {
        return false;
    }
    if gb.left() < 24 {
        return false;
    }
    gb.skip(24);
    true
}

/// `AV_CRC_24_IEEE`: FFmpeg's big-endian table (entries byte-swapped).
static CRC24: LazyLock<[u32; 256]> = LazyLock::new(|| {
    const POLY: u32 = 0x86_4CFB;
    let mut t = [0u32; 256];
    for (i, e) in t.iter_mut().enumerate() {
        let mut c = (i as u32) << 24;
        for _ in 0..8 {
            c = (c << 1) ^ ((POLY << 8) & ((c as i32 >> 31) as u32));
        }
        *e = c.swap_bytes();
    }
    t
});

/// `av_crc` over the 24-bit IEEE table, in FFmpeg's register form.
pub(crate) fn crc24(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = CRC24[usize::from(crc as u8 ^ b)] ^ (crc >> 8);
    }
    crc
}

/// `ff_tak_check_crc`: the CRC of `buf` but its last 3 bytes equals them.
pub(crate) fn check_crc(buf: &[u8]) -> bool {
    if buf.len() < 4 {
        return false;
    }
    let n = buf.len() - 3;
    let stored = u32::from(buf[n]) << 16 | u32::from(buf[n + 1]) << 8 | u32::from(buf[n + 2]);
    crc24(0xCE_04B7, &buf[..n]) == stored
}
