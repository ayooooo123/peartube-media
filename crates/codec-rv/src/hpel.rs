//! Half-pel motion compensation, ported from FFmpeg libavcodec/hpeldsp.c
//! and pel_template.c (commit 2da55bf), limited to the functions the
//! RealVideo H.263 path uses: put/avg/no_rnd for 16x16, 16x8, 8x8 at
//! full-pel, x2, y2, xy2 sub-positions (dxy index 0..3 like FFmpeg).
//!
//! License: GNU Lesser General Public License, version 2.1 or later.

#![forbid(unsafe_code)]

#[inline]
fn byte_vec32(c: u32) -> u32 {
    c.wrapping_mul(0x0101_0101)
}

#[inline]
pub fn rnd_avg32(a: u32, b: u32) -> u32 {
    (a | b).wrapping_sub(((a ^ b) & !byte_vec32(0x01)) >> 1)
}

#[inline]
pub fn no_rnd_avg32(a: u32, b: u32) -> u32 {
    (a & b).wrapping_add(((a ^ b) & !byte_vec32(0x01)) >> 1)
}

#[inline]
fn avg4(a: u32, b: u32, c: u32, d: u32) -> u32 {
    let a = a as u64;
    let b = b as u64;
    let c = c as u64;
    let d = d as u64;
    let h0 = (a & 0x0303_0303) + (b & 0x0303_0303) + (c & 0x0303_0303) + (d & 0x0303_0303) + 0x0202_0202;
    let h1 = ((a & !0x0303_0303) >> 2)
        + ((b & !0x0303_0303) >> 2)
        + ((c & !0x0303_0303) >> 2)
        + ((d & !0x0303_0303) >> 2);
    (h1 + (h0 >> 2)) as u32
}

/// The real `pix_op[dxy]` dispatch: writes `size x h` block.
pub fn op_pixels(
    dst: &mut [u8],
    dst_off: usize,
    src: &[u8],
    src_x: i32,
    src_y: i32,
    width: usize,
    height: usize,
    stride: usize,
    h: usize,
    size: usize,
    dxy: u32,
    avg: bool,
    no_rnd: bool,
) {
    debug_assert!(size == 16 || size == 8 || size == 4 || size == 2);
    let px = |r: usize, c: usize| -> u8 {
        let x = (src_x + c as i32).clamp(0, width as i32 - 1) as usize;
        let y = (src_y + r as i32).clamp(0, height as i32 - 1) as usize;
        let ro = y * stride + x;
        if ro < src.len() {
            src[ro]
        } else {
            0
        }
    };
    let w = |r: usize, c: usize, v: u8, dst: &mut [u8]| {
        let doff = dst_off + r * stride + c;
        if doff < dst.len() {
            dst[doff] = if avg {
                ((dst[doff] as u32 + v as u32 + 1) >> 1) as u8
            } else {
                v
            };
        }
    };
    for i in 0..h {
        match dxy {
            0 => {
                for c in 0..size {
                    let v = px(i, c);
                    w(i, c, v, dst);
                }
            }
            1 => {
                for c in 0..size {
                    let a = px(i, c) as u32;
                    let b = px(i, c + 1) as u32;
                    let v = ((a + b + 1) >> 1) as u8;
                    w(i, c, v, dst);
                }
            }
            2 => {
                for c in 0..size {
                    let a = px(i, c) as u32;
                    let b = px(i + 1, c) as u32;
                    let v = ((a + b + 1) >> 1) as u8;
                    w(i, c, v, dst);
                }
            }
            _ => {
                for c in 0..size {
                    let a = px(i, c) as u32;
                    let b = px(i, c + 1) as u32;
                    let cc = px(i + 1, c) as u32;
                    let d = px(i + 1, c + 1) as u32;
                    let v = if no_rnd {
                        ((a + b + cc + d + 1) >> 2) as u8
                    } else {
                        ((a + b + cc + d + 2) >> 2) as u8
                    };
                    w(i, c, v, dst);
                }
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_pel_put() {
        let src = vec![7u8; 64];
        let mut dst = vec![0u8; 64];
        op_pixels(&mut dst, 0, &src, 0, 0, 8, 8, 8, 8, 8, 0, false, false);
        assert!(dst.iter().all(|&v| v == 7));
    }

    #[test]
    fn half_x_put() {
        let mut src = vec![0u8; 64];
        for r in 0..8 {
            src[r * 8] = 10;
            src[r * 8 + 1] = 20;
        }
        let mut dst = vec![0u8; 64];
        op_pixels(&mut dst, 0, &src, 0, 0, 8, 8, 8, 8, 8, 1, false, false);
        assert_eq!(dst[0], 15); // (10+20+1)>>1
        assert_eq!(dst[1], 10); // (20+0+1)>>1 = 10 (21>>1)
    }

    #[test]
    fn avg_put() {
        let src = vec![10u8; 64];
        let mut dst = vec![20u8; 64];
        op_pixels(&mut dst, 0, &src, 0, 0, 8, 8, 8, 8, 8, 0, true, false);
        assert_eq!(dst[0], 15);
    }

    #[test]
    fn xy2_put() {
        let mut src = vec![0u8; 100];
        for r in 0..10 {
            for c in 0..10 {
                src[r * 10 + c] = 4;
            }
        }
        let mut dst = vec![0u8; 64];
        op_pixels(&mut dst, 0, &src, 0, 0, 10, 10, 10, 8, 8, 3, false, false);
        assert_eq!(dst[0], 4);
    }
}
