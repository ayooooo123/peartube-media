// Ported from FFmpeg libavcodec/mlp.c and libavutil/crc.c (commit 2da55bf).
// Licensed under LGPL-2.1-or-later.

//! MLP checksums: `ff_mlp_checksum8/16`, `ff_mlp_restart_checksum`,
//! `ff_mlp_calculate_parity`, `xor_32_to_8`.

use crate::crc::{calculate_parity as raw_parity, restart_checksum, CRC_1D};

/// `ff_mlp_restart_checksum(buf, bit_size)`.
#[inline]
pub fn mlp_restart_checksum(buf: &[u8], bit_size: usize) -> u8 {
    restart_checksum(buf, bit_size)
}

/// `ff_mlp_calculate_parity(buf, buf_size)`.
#[inline]
pub fn mlp_calculate_parity(buf: &[u8]) -> u8 {
    raw_parity(buf)
}

/// `xor_32_to_8`: XOR four bytes of a value into one. FFmpeg's version uses
/// native-endian byte extraction, which for the checksum purposes is the
/// same on every platform this runs on.
#[inline]
pub fn xor_32_to_8(value: u32) -> u8 {
    let b = value.to_le_bytes();
    b[0] ^ b[1] ^ b[2] ^ b[3]
}

/// The `AV_CRC_8_EBU` table (poly 0x1D), exposed for tests of the restart
/// checksum against FFmpeg.
pub const CRC_8_EBU: &[u32; 256] = &CRC_1D;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_is_xor_of_bytes() {
        assert_eq!(mlp_calculate_parity(&[0xf0, 0x45, 0xff, 0xd8]), 0xf0 ^ 0x45 ^ 0xff ^ 0xd8);
        // FFmpeg's 4-byte-aligned shortcut must agree with the simple loop.
        let buf: Vec<u8> = (0..32u8).collect();
        assert_eq!(mlp_calculate_parity(&buf), buf.iter().fold(0u8, |a, b| a ^ b));
    }
}
