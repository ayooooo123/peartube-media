//! Subtitles: a `CALayer` above the video layer holding a tight-bounded child
//! `CALayer` whose `contents` is a zero-copy `CGImage` of only the active
//! subtitle union rect, positioned inside the aspect-fitted video rect.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2_core_foundation::{CGPoint, CGRect, CGSize, CFRetained};
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage,
    CGImageAlphaInfo,
};
use objc2_foundation::NSString;
use objc2_quartz_core::{CALayer, CATransaction};

use crate::backend::SubtitleImage;

/// Tight subtitle bounding box in video pixel coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SubtitlePlacement {
    pub video_w: u32,
    pub video_h: u32,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Computes the child layer frame inside `parent_bounds` matching `resizeAspect`
/// video placement for a tight subtitle bitmap at `p`.
pub(crate) fn aspect_fit_subrect(
    parent_bounds: CGRect,
    flipped: bool,
    p: SubtitlePlacement,
) -> CGRect {
    let cw = parent_bounds.size.width;
    let ch = parent_bounds.size.height;
    let vw = f64::from(p.video_w);
    let vh = f64::from(p.video_h);
    if cw <= 0.0 || ch <= 0.0 || vw <= 0.0 || vh <= 0.0 || p.w == 0 || p.h == 0 {
        return CGRect::ZERO;
    }
    let scale = (cw / vw).min(ch / vh);
    let fit_w = vw * scale;
    let fit_h = vh * scale;
    let fit_x = parent_bounds.origin.x + (cw - fit_w) * 0.5;
    let fit_y = parent_bounds.origin.y + (ch - fit_h) * 0.5;
    let rx = fit_x + f64::from(p.x) * scale;
    let rw = f64::from(p.w) * scale;
    let rh = f64::from(p.h) * scale;
    let ry = if flipped {
        fit_y + f64::from(p.y) * scale
    } else {
        fit_y + fit_h - f64::from(p.y.saturating_add(p.h)) * scale
    };
    CGRect {
        origin: CGPoint { x: rx, y: ry },
        size: CGSize { width: rw, height: rh },
    }
}

pub struct AppleSubtitleSink {
    parent_layer: SendPtrHolder,
    image_layer: SendPtrHolder,
    placement: Arc<Mutex<Option<SubtitlePlacement>>>,
    main: dispatch2::DispatchRetained<DispatchQueue>,
}

// SAFETY: only `main` touches the layers; `show` builds the CGImage on the
// engine thread (CoreGraphics is thread-safe) and hands it over.
unsafe impl Send for AppleSubtitleSink {}
unsafe impl Sync for AppleSubtitleSink {}

/// Raw-pointer wrapper that is `Send` so a +1-retained layer pointer can
/// travel to the main queue inside a closure.
struct SendPtr<T>(*mut T);
// SAFETY: only dereferenced on the main queue.
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

/// A retained layer stored behind SendSync (all uses on the main queue).
struct SendPtrHolder(Retained<CALayer>);
// SAFETY: every use runs on the main queue.
unsafe impl Send for SendPtrHolder {}
unsafe impl Sync for SendPtrHolder {}

impl AppleSubtitleSink {
    pub(crate) fn new(
        parent_layer: Retained<CALayer>,
        image_layer: Retained<CALayer>,
        placement: Arc<Mutex<Option<SubtitlePlacement>>>,
    ) -> Self {
        // CALayer mutations happen on the main queue (AppKit/UIKit layers
        // are main-thread-only), asynchronously: a playback can be made, or
        // its subtitles switched, while the main thread waits for it (a
        // `Player` dropped on main joins its pipeline thread). `show` runs
        // on the same serial queue, after this.
        let main = unsafe {
            dispatch2::DispatchRetained::retain(std::ptr::NonNull::from(DispatchQueue::main()))
        };
        let parent_setup = SendPtr(Retained::into_raw(parent_layer.clone()));
        let image_setup = SendPtr(Retained::into_raw(image_layer.clone()));
        main.exec_async(move || {
            let parent_setup = parent_setup;
            let image_setup = image_setup;
            // SAFETY: the +1 taken above, reclaimed once here.
            let parent = unsafe { Retained::from_raw(parent_setup.0) }.expect("layer null");
            let image = unsafe { Retained::from_raw(image_setup.0) }.expect("image layer null");
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            parent.setMasksToBounds(true);
            let gravity = NSString::from_str("resize");
            image.setContentsGravity(&gravity);
            CATransaction::commit();
        });
        Self {
            parent_layer: SendPtrHolder(parent_layer),
            image_layer: SendPtrHolder(image_layer),
            placement,
            main,
        }
    }
}

impl crate::backend::SubtitleSink for AppleSubtitleSink {
    fn show(&mut self, images: &[SubtitleImage], video_width: u32, video_height: u32) {
        // Compose only the tight union rect of positioned images into one
        // zero-copy RGBA CGImage, then update the child layer on main.
        let composed = compose(images, video_width, video_height);
        let next_placement = composed.as_ref().map(|(_, p)| *p);
        *self.placement.lock().expect("subtitle placement lock") = next_placement;
        // SAFETY: dereferenced only on the main queue; +1 retain until then.
        let parent_ptr = Arc::new(SendPtr(Retained::into_raw(self.parent_layer.0.clone())));
        let image_ptr = Arc::new(SendPtr(Retained::into_raw(self.image_layer.0.clone())));
        self.main.exec_async(move || {
            // SAFETY: non-null by construction.
            let parent = unsafe { Retained::from_raw(parent_ptr.0) }.expect("parent layer null");
            let layer = unsafe { Retained::from_raw(image_ptr.0) }.expect("image layer null");
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            unsafe {
                if let Some((img, placement)) = composed.as_ref() {
                    let rect =
                        aspect_fit_subrect(parent.bounds(), parent.isGeometryFlipped(), *placement);
                    layer.setFrame(rect);
                    let contents: &objc2::runtime::AnyObject =
                        img.as_ref() as &objc2::runtime::AnyObject;
                    // No setNeedsDisplay: the layer has no drawing of its own,
                    // and a display pass would replace these contents.
                    layer.setContents(Some(contents));
                    layer.setHidden(false);
                } else {
                    layer.setContents(None);
                    layer.setHidden(true);
                }
            }
            CATransaction::commit();
        });
    }
}

unsafe extern "C-unwind" fn release_boxed_bytes(
    info: *mut c_void,
    _data: NonNull<c_void>,
    size: usize,
) {
    if !info.is_null() {
        // SAFETY: `info` and `size` came from `Box::<[u8]>::into_raw` in `compose`.
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                info.cast::<u8>(),
                size,
            )));
        }
    }
}

/// Computes the clipped union bounding box `(x0, y0, x1, y1)` of all valid images.
fn tight_bounds(images: &[SubtitleImage], video_w: usize, video_h: usize) -> Option<(usize, usize, usize, usize)> {
    let mut min_x = usize::MAX;
    let mut min_y = usize::MAX;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    for img in images {
        if img.width == 0 || img.height == 0 {
            continue;
        }
        let stride = img.width as usize * 4;
        if img.rgba.len() < stride.saturating_mul(img.height as usize) {
            continue;
        }
        let x0 = img.x.max(0) as usize;
        let y0 = img.y.max(0) as usize;
        let x1 = x0.saturating_add(img.width as usize).min(video_w);
        let y1 = y0.saturating_add(img.height as usize).min(video_h);
        if x0 < x1 && y0 < y1 {
            min_x = min_x.min(x0);
            min_y = min_y.min(y0);
            max_x = max_x.max(x1);
            max_y = max_y.max(y1);
        }
    }
    (min_x < max_x && min_y < max_y).then_some((min_x, min_y, max_x, max_y))
}

/// Renders the positioned RGBA images into one tight-bounded RGBA bitmap without
/// allocating a full-frame canvas or copying through `NSData`.
fn compose(
    images: &[SubtitleImage],
    video_width: u32,
    video_height: u32,
) -> Option<(CFRetained<CGImage>, SubtitlePlacement)> {
    if images.is_empty() || video_width == 0 || video_height == 0 {
        return None;
    }
    let vw = video_width as usize;
    let vh = video_height as usize;
    let (bx0, by0, bx1, by1) = tight_bounds(images, vw, vh)?;
    let w = bx1 - bx0;
    let h = by1 - by0;
    // Cap the composition surface (subtitle coordinates are untrusted).
    if w.checked_mul(h).map(|px| px * 4 > 256 * 1024 * 1024).unwrap_or(true) {
        return None;
    }
    let mut buf = vec![0u8; w * h * 4];
    for img in images {
        if img.width == 0 || img.height == 0 {
            continue;
        }
        let stride = img.width as usize * 4;
        if img.rgba.len() < stride * img.height as usize {
            continue;
        }
        // Straight → premultiplied alpha, then blit with clipping into tight rect.
        let x0 = img.x.max(0) as usize;
        let y0 = img.y.max(0) as usize;
        for row in 0..img.height as usize {
            let vy = y0 + row;
            if vy >= by1 {
                break;
            }
            let dy = vy - by0;
            for col in 0..img.width as usize {
                let vx = x0 + col;
                if vx >= bx1 {
                    break;
                }
                let s = row * stride + col * 4;
                let (r, g, b, a) = (
                    img.rgba[s] as u16,
                    img.rgba[s + 1] as u16,
                    img.rgba[s + 2] as u16,
                    img.rgba[s + 3] as u16,
                );
                if a == 0 {
                    continue;
                }
                let dx = vx - bx0;
                let d = dy * w * 4 + dx * 4;
                if a == 255 {
                    buf[d] = r as u8;
                    buf[d + 1] = g as u8;
                    buf[d + 2] = b as u8;
                    buf[d + 3] = 255;
                } else {
                    // Premultiplied "over" onto the pixel below.
                    let da = buf[d + 3] as u16;
                    let out_a = a + da * (255 - a) / 255;
                    let blend = |fg: u16, bg: u16| -> u8 {
                        if out_a == 0 {
                            0
                        } else {
                            ((fg * a + bg * da * (255 - a) / 255) / out_a) as u8
                        }
                    };
                    buf[d] = blend(r, buf[d] as u16);
                    buf[d + 1] = blend(g, buf[d + 1] as u16);
                    buf[d + 2] = blend(b, buf[d + 2] as u16);
                    buf[d + 3] = out_a as u8;
                }
            }
        }
    }
    // Hand buffer ownership directly to CoreGraphics without an NSData copy.
    let boxed = buf.into_boxed_slice();
    let len = boxed.len();
    let raw = Box::into_raw(boxed).cast::<u8>();
    let provider = unsafe {
        CGDataProvider::with_data(
            raw.cast::<c_void>(),
            raw.cast::<c_void>(),
            len,
            Some(release_boxed_bytes),
        )
    };
    let Some(provider) = provider else {
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(raw, len)));
        }
        return None;
    };
    // Tag subtitle bitmaps explicitly as sRGB so compositing inside an extendedSRGB HDR window
    // pins caption white cleanly to 1.0 SDR reference white without clamping XDR highlights.
    let space = unsafe { CGColorSpace::with_name(Some(kCGColorSpaceSRGB)) }
        .or_else(CGColorSpace::new_device_rgb)
        .expect("subtitle RGB color space");
    // SAFETY: provider/space valid; bitmap info matches the RGBA bytes.
    let image = unsafe {
        CGImage::new(
            w,
            h,
            8,
            32,
            w * 4,
            Some(&space),
            CGBitmapInfo(CGImageAlphaInfo::PremultipliedLast.0),
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }?;
    let placement = SubtitlePlacement {
        video_w: video_width,
        video_h: video_height,
        x: bx0 as u32,
        y: by0 as u32,
        w: w as u32,
        h: h as u32,
    };
    Some((CFRetained::from(image), placement))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tight_bounds_crops_4k_caption_to_active_rect() {
        let img = SubtitleImage {
            x: 960,
            y: 1900,
            width: 1920,
            height: 160,
            rgba: vec![255; 1920 * 160 * 4],
        };
        assert_eq!(tight_bounds(&[img], 3840, 2160), Some((960, 1900, 2880, 2060)));
    }

    #[test]
    fn aspect_fit_subrect_matches_macos_and_ios_letterbox() {
        let parent = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize { width: 1512.0, height: 949.0 },
        };
        let p = SubtitlePlacement {
            video_w: 3840,
            video_h: 1600,
            x: 960,
            y: 1400,
            w: 1920,
            h: 160,
        };
        let scale = 1512.0 / 3840.0;
        let fit_h = 1600.0 * scale;
        let fit_y = (949.0 - fit_h) * 0.5;
        let mac = aspect_fit_subrect(parent, false, p);
        assert!((mac.origin.x - 960.0 * scale).abs() < 1e-6);
        assert!((mac.size.width - 1920.0 * scale).abs() < 1e-6);
        assert!((mac.size.height - 160.0 * scale).abs() < 1e-6);
        assert!((mac.origin.y - (fit_y + fit_h - 1560.0 * scale)).abs() < 1e-6);

        let ios = aspect_fit_subrect(parent, true, p);
        assert!((ios.origin.y - (fit_y + 1400.0 * scale)).abs() < 1e-6);
    }
}
