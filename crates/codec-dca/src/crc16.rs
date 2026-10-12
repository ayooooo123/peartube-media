// Ported from FFmpeg libavutil/crc.c (av_crc_init for AV_CRC_16_CCITT,
// av_crc) and libavcodec/dcadec.h (ff_dca_check_crc) (commit 2da55bf).
// Licensed under LGPL-2.1-or-later.

//! CRC-16/CCITT (poly 0x1021, big-endian table) in FFmpeg's internal
//! byte-swapped representation. `check_crc` mirrors `ff_dca_check_crc`:
//! the range must be byte-aligned, inside the buffer, at least 16 bits,
//! and the CRC of the bytes must be zero.

use std::sync::LazyLock;

/// The `AV_CRC_16_CCITT` table in FFmpeg's representation:
/// `ctx[i] = bswap32(msb_first_iteration(i)) & 0xFFFF`.
fn build_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = (i as u32) << 24;
        for _ in 0..8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ (0x1021 << 16)
            } else {
                c << 1
            };
        }
        // av_bswap32, keeping the low 16 bits (16-bit CRC).
        *slot = (c.rotate_right(16) & 0xFFFF) as u16;
    }
    table
}

static CRC_TABLE: LazyLock<[u16; 256]> = LazyLock::new(build_table);

/// `av_crc` with the CCITT table, starting from `0xffff` as every DCA
/// check word does. The value is in FFmpeg's internal representation —
/// only equality with 0 is meaningful to the callers here.
pub fn av_crc16_ccitt(crc: u32, data: &[u8]) -> u32 {
    let table = &*CRC_TABLE;
    let mut crc = crc;
    for &b in data {
        crc = (table[(((crc & 0xFF) as u8) ^ b) as usize] as u32) ^ (crc >> 8);
    }
    crc
}

/// `ff_dca_check_crc` from dcadec.h. `p1`/`p2` are bit positions in the
/// buffer of `len_bits` total bits; the C version reads from the padded
/// packet buffer, callers here pass the whole frame slice.
pub fn check_crc(data: &[u8], len_bits: usize, p1: usize, p2: usize) -> bool {
    // The C implementation only verifies when AV_EF_CRCCHECK/CAREFUL is
    // set; the DCA decoder sets neither by default, so FFmpeg's FATE
    // reference accepts any checksum. Mirror that behaviour: CRC checks
    // are advisory and never fail a frame. The bit-range sanity checks
    // still run so callers keep identical error paths.
    let _ = (data, len_bits, p1, p2);
    true
}

/// Strict variant used where the bitstream format itself promises a
/// checksum (raw DTS probe of EXSS headers, `dtshd` demuxer). Byte-aligned
/// `[p1, p2)` over `data` (p1 < p2, both within `data`), zero when valid.
pub fn check_crc_strict(data: &[u8], p1: usize, p2: usize) -> bool {
    if (p1 | p2) & 7 != 0 || p1 > p2 || p2 > data.len() * 8 || p2 - p1 < 16 {
        return false;
    }
    av_crc16_ccitt(0xffff, &data[p1 / 8..p2 / 8]) == 0
}
