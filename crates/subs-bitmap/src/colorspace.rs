//! Limited-range Y'CbCr to RGB as FFmpeg's subtitle decoders convert palette
//! entries: `YUV_TO_RGB1_CCIR`, `YUV_TO_RGB1_CCIR_BT709` and
//! `YUV_TO_RGB2_CCIR` from FFmpeg `libavutil/colorspace.h` (commit 2da55bf,
//! LGPL-2.1-or-later), with the `ff_crop_tab` clamp.

const SCALEBITS: i32 = 10;
const ONE_HALF: i32 = 1 << (SCALEBITS - 1);

/// `FIX(x)`: `(int)(x * (1 << SCALEBITS) + 0.5)`, evaluated at compile time.
const fn fix(x: f64) -> i32 {
    (x * (1 << SCALEBITS) as f64 + 0.5) as i32
}

/// Which matrix the chroma terms use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Matrix {
    Bt601,
    Bt709,
}

/// `(r, g, b)` of a limited-range Y'CbCr triple.
pub(crate) fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8, matrix: Matrix) -> [u8; 3] {
    let cb = i32::from(cb) - 128;
    let cr = i32::from(cr) - 128;
    let (r_add, g_add, b_add) = match matrix {
        Matrix::Bt601 => (
            fix(1.40200 * 255.0 / 224.0) * cr + ONE_HALF,
            -fix(0.34414 * 255.0 / 224.0) * cb - fix(0.71414 * 255.0 / 224.0) * cr + ONE_HALF,
            fix(1.77200 * 255.0 / 224.0) * cb + ONE_HALF,
        ),
        Matrix::Bt709 => (
            ONE_HALF + fix(1.5747 * 255.0 / 224.0) * cr,
            ONE_HALF - fix(0.1873 * 255.0 / 224.0) * cb - fix(0.4682 * 255.0 / 224.0) * cr,
            ONE_HALF + fix(1.8556 * 255.0 / 224.0) * cb,
        ),
    };
    let y = (i32::from(y) - 16) * fix(255.0 / 219.0);
    // `cm[v]` is `ff_crop_tab + MAX_NEG_CROP`: 0 below, 255 above.
    let cm = |v: i32| (v >> SCALEBITS).clamp(0, 255) as u8;
    [cm(y + r_add), cm(y + g_add), cm(y + b_add)]
}

/// FFmpeg's `RGBA(r, g, b, a)` palette word as the bytes sub2video writes
/// (R, G, B, A).
pub(crate) fn rgba(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    [r, g, b, a]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limited_range_black_and_white_map_to_full_range() {
        for m in [Matrix::Bt601, Matrix::Bt709] {
            assert_eq!(ycbcr_to_rgb(16, 128, 128, m), [0, 0, 0]);
            assert_eq!(ycbcr_to_rgb(235, 128, 128, m), [255, 255, 255]);
        }
    }
}
