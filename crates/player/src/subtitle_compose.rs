//! Pure RGBA straight-alpha subtitle composition. No platform code: the
//! Android subtitle sink locks its ANativeWindow and calls into this; the
//! host fuzz test exercises the same math against hostile inputs.

use crate::backend::SubtitleImage;

/// Blends `images` (straight alpha) into an RGBA buffer of `dst.len()`
/// bytes laid out as `dst_stride`-pixel rows, `dst_height` rows. All
/// coordinates are widened to i64 before the range checks so hostile
/// `x`/`y` values (including i32::MIN/MAX) cannot overflow; the buffer
/// bounds come from the locked surface, not from the caller.
pub fn compose_straight(
    dst: &mut [u8],
    dst_stride: usize,
    dst_height: usize,
    images: &[SubtitleImage],
    video_width: u32,
) {
    for img in images {
        if img.width == 0 || img.height == 0 || img.rgba.is_empty() {
            continue;
        }
        let src_stride = img.width as usize * 4;
        for row in 0..img.height as i64 {
            let target_y = img.y as i64 + row;
            if target_y < 0 || target_y >= dst_height as i64 {
                continue;
            }
            for col in 0..img.width as i64 {
                let target_x = img.x as i64 + col;
                if target_x < 0
                    || target_x >= dst_stride as i64
                    || target_x >= video_width as i64
                {
                    continue;
                }
                let src_idx = row as usize * src_stride + col as usize * 4;
                if src_idx + 4 <= img.rgba.len() {
                    let src_pixel: [u8; 4] = [
                        img.rgba[src_idx],
                        img.rgba[src_idx + 1],
                        img.rgba[src_idx + 2],
                        img.rgba[src_idx + 3],
                    ];
                    let dst_idx = (target_y as usize * dst_stride + target_x as usize) * 4;
                    if dst_idx + 4 > dst.len() {
                        continue;
                    }
                    let dst_pixel: [u8; 4] =
                        [dst[dst_idx], dst[dst_idx + 1], dst[dst_idx + 2], dst[dst_idx + 3]];
                    let blended = oxideav_pixfmt::over_straight(src_pixel, dst_pixel);
                    dst[dst_idx] = blended[0];
                    dst[dst_idx + 1] = blended[1];
                    dst[dst_idx + 2] = blended[2];
                    dst[dst_idx + 3] = blended[3];
                }
            }
        }
    }
}
