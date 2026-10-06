// Ported from FFmpeg libavformat/rmsipr.c, commit 2da55bf
// License: GNU Lesser General Public License (LGPL) version 2.1 or later

pub const FF_SIPR_SUBPK_SIZE: [usize; 4] = [29, 19, 37, 20];

const SIPR_SWAPS: [[usize; 2]; 38] = [
    [0, 63],
    [1, 22],
    [2, 44],
    [3, 90],
    [5, 81],
    [7, 31],
    [8, 86],
    [9, 58],
    [10, 36],
    [12, 68],
    [13, 39],
    [14, 73],
    [15, 53],
    [16, 69],
    [17, 57],
    [19, 88],
    [20, 34],
    [21, 71],
    [24, 46],
    [25, 94],
    [26, 54],
    [28, 75],
    [29, 50],
    [32, 70],
    [33, 92],
    [35, 74],
    [38, 85],
    [40, 56],
    [42, 87],
    [43, 65],
    [45, 59],
    [48, 79],
    [49, 93],
    [51, 89],
    [55, 95],
    [61, 76],
    [67, 83],
    [77, 80],
];

/// Reorder SIPR data according to the RealMedia descrambling table.
pub fn rm_reorder_sipr_data(buf: &mut [u8], sub_packet_h: usize, framesize: usize) {
    let bs = sub_packet_h * framesize * 2 / 96; // nibbles per subpacket
    if bs == 0 {
        return;
    }

    for swap in &SIPR_SWAPS {
        let mut i = bs * swap[0];
        let mut o = bs * swap[1];

        for _ in 0..bs {
            let i_byte = i >> 1;
            let o_byte = o >> 1;
            if i_byte >= buf.len() || o_byte >= buf.len() {
                break;
            }

            let i_shift = (i & 1) * 4;
            let o_shift = (o & 1) * 4;

            let x = (buf[i_byte] >> i_shift) & 0x0F;
            let y = (buf[o_byte] >> o_shift) & 0x0F;

            let o_keep_mask = if (o & 1) == 0 { 0xF0 } else { 0x0F };
            buf[o_byte] = (x << o_shift) | (buf[o_byte] & o_keep_mask);

            let i_keep_mask = if (i & 1) == 0 { 0xF0 } else { 0x0F };
            buf[i_byte] = (y << i_shift) | (buf[i_byte] & i_keep_mask);

            i += 1;
            o += 1;
        }
    }
}
