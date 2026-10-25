//! Pure RGBA straight-alpha subtitle composition. No platform code: the
//! Android subtitle sink locks its ANativeWindow and calls into this; the
//! host fuzz test exercises the same math against hostile inputs.

use crate::backend::SubtitleImage;

/// Where a `canvas_width x canvas_height` subtitle canvas goes on a surface
/// of `surface_width x surface_height`: a buffer of the surface's shape,
/// `(width, height, x, y)`, with the canvas centred at `(x, y)` keeping its
/// own shape. The platform scales the buffer to the surface evenly, so a
/// canvas the size of the video lands on the letterboxed picture and a 16:9
/// Blu-ray canvas fills a 16:9 slot. `None` when a size is empty or the
/// buffer would exceed `max_pixels`: the canvas is then drawn as it is.
pub fn fit_canvas(
    canvas_width: u32,
    canvas_height: u32,
    surface_width: u32,
    surface_height: u32,
    max_pixels: u64,
) -> Option<(u32, u32, u32, u32)> {
    let (cw, ch) = (u64::from(canvas_width), u64::from(canvas_height));
    let (sw, sh) = (u64::from(surface_width), u64::from(surface_height));
    if cw == 0 || ch == 0 || sw == 0 || sh == 0 {
        return None;
    }
    // Rounded to the nearest pixel, never below the canvas.
    let (bw, bh) = if cw * sh >= ch * sw {
        (cw, (cw * sh + sw / 2) / sw)
    } else {
        ((ch * sw + sh / 2) / sh, ch)
    };
    if bw * bh > max_pixels || bw > i32::MAX as u64 || bh > i32::MAX as u64 {
        return None;
    }
    Some((bw as u32, bh as u32, ((bw - cw) / 2) as u32, ((bh - ch) / 2) as u32))
}

/// Blends `images` (straight alpha) into an RGBA buffer of `dst.len()`
/// bytes laid out as `dst_stride`-pixel rows, `dst_height` rows. All
/// coordinates are widened to i64 before the range checks so hostile
/// `x`/`y` values (including i32::MIN/MAX) cannot overflow; the buffer
/// bounds come from the locked surface, not from the caller.
fn compose_row(dst_row: &mut [u8], target_y: i64, max_x: i64, images: &[SubtitleImage]) {
    for img in images {
        if img.width == 0 || img.height == 0 || img.rgba.is_empty() {
            continue;
        }
        let row = target_y - i64::from(img.y);
        if row < 0 || row >= i64::from(img.height) {
            continue;
        }
        let src_stride = img.width as usize * 4;
        for col in 0..i64::from(img.width) {
            let target_x = i64::from(img.x) + col;
            if target_x < 0 || target_x >= max_x {
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
                let dst_idx = target_x as usize * 4;
                if dst_idx + 4 > dst_row.len() {
                    continue;
                }
                let dst_pixel: [u8; 4] = [
                    dst_row[dst_idx],
                    dst_row[dst_idx + 1],
                    dst_row[dst_idx + 2],
                    dst_row[dst_idx + 3],
                ];
                let blended = oxideav_pixfmt::over_straight(src_pixel, dst_pixel);
                dst_row[dst_idx] = blended[0];
                dst_row[dst_idx + 1] = blended[1];
                dst_row[dst_idx + 2] = blended[2];
                dst_row[dst_idx + 3] = blended[3];
            }
        }
    }
}

pub fn compose_straight(
    dst: &mut [u8],
    dst_stride: usize,
    dst_height: usize,
    images: &[SubtitleImage],
    video_width: u32,
) {
    let max_x = dst_stride.min(video_width as usize) as i64;
    if max_x <= 0 || images.is_empty() {
        return;
    }
    for target_y in 0..dst_height {
        let Some(row_start) = target_y
            .checked_mul(dst_stride)
            .and_then(|px| px.checked_mul(4))
        else {
            break;
        };
        if row_start >= dst.len() {
            break;
        }
        let row_len = dst_stride.saturating_mul(4).min(dst.len() - row_start);
        compose_row(&mut dst[row_start..row_start + row_len], target_y as i64, max_x, images);
    }
}

/// Paints straight-alpha subtitle cues onto an already-zeroed `MaybeUninit<u8>`
/// surface buffer one wet scanline at a time — no raw pointer view needed.
#[inline]
pub fn compose_straight_scanlines(
    dst_len: usize,
    dst_stride: usize,
    dst_height: usize,
    images: &[SubtitleImage],
    video_width: u32,
    row_scratch: &mut Vec<u8>,
    mut emit_row: impl FnMut(usize, &[u8]),
) {
    let max_x = dst_stride.min(video_width as usize) as i64;
    if max_x <= 0 || images.is_empty() {
        return;
    }
    for target_y in 0..dst_height {
        let ty = target_y as i64;
        let overlaps = images.iter().any(|img| {
            img.width > 0
                && img.height > 0
                && !img.rgba.is_empty()
                && ty >= i64::from(img.y)
                && ty < i64::from(img.y) + i64::from(img.height)
        });
        if !overlaps {
            continue;
        }
        let Some(row_start) = target_y
            .checked_mul(dst_stride)
            .and_then(|px| px.checked_mul(4))
        else {
            break;
        };
        if row_start >= dst_len {
            break;
        }
        let row_bytes = dst_stride
            .saturating_mul(4)
            .min(dst_len - row_start)
            .min((max_x as usize).saturating_mul(4));
        let usable_bytes = row_bytes & !3;
        if usable_bytes == 0 {
            continue;
        }
        row_scratch.clear();
        row_scratch.resize(usable_bytes, 0);
        compose_row(row_scratch, ty, max_x, images);
        emit_row(row_start, row_scratch);
    }
}

pub fn compose_straight_uninit(
    dst: &mut [std::mem::MaybeUninit<u8>],
    dst_stride: usize,
    dst_height: usize,
    images: &[SubtitleImage],
    video_width: u32,
    row_scratch: &mut Vec<u8>,
) {
    let dst_len = dst.len();
    compose_straight_scanlines(
        dst_len,
        dst_stride,
        dst_height,
        images,
        video_width,
        row_scratch,
        |row_start, pixels| {
            for (out, &byte) in dst[row_start..row_start + pixels.len()]
                .iter_mut()
                .zip(pixels.iter())
            {
                out.write(byte);
            }
        },
    );
}
