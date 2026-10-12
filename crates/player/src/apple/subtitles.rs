//! Subtitles: a `CALayer` above the video layer whose `contents` is a
//! `CGImage` of the composed RGBA images, scaled with the video rect.

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage,
    CGImageAlphaInfo,
};
use objc2_foundation::{NSData, NSString};
use objc2_quartz_core::CALayer;

use crate::backend::SubtitleImage;

pub struct AppleSubtitleSink {
    layer: SendPtrHolder,
    main: dispatch2::DispatchRetained<DispatchQueue>,
}

// SAFETY: only `main` touches the layer; `show` builds the CGImage on the
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
    pub fn new(layer: Retained<CALayer>) -> Self {
        // CALayer mutations must happen on the main queue (AppKit/UIKit
        // layers are main-thread-only). Hand the +1 across as a raw pointer
        // and reclaim it here afterwards.
        let setup_ptr = std::sync::Arc::new(SendPtr(Retained::into_raw(layer)));
        let main = unsafe {
            dispatch2::DispatchRetained::retain(std::ptr::NonNull::from(DispatchQueue::main()))
        };
        let slot = setup_ptr.clone();
        main.exec_sync(move || {
            let layer = unsafe { Retained::from_raw(slot.0) }.expect("layer null");
            layer.setMasksToBounds(true);
            // Contents gravity "resizeAspect" keeps the composed bitmap
            // scaled with the video rect.
            let gravity = NSString::from_str("resizeAspect");
            layer.setContentsGravity(&gravity);
            // Hand the +1 back to the caller thread via the Arc slot.
            std::mem::forget(layer);
        });
        let layer = unsafe {
            Retained::from_raw(setup_ptr.0)
        }
        .expect("layer null");
        Self {
            layer: SendPtrHolder(layer),
            main,
        }
    }
}

impl crate::backend::SubtitleSink for AppleSubtitleSink {
    fn show(&mut self, images: &[SubtitleImage], video_width: u32, video_height: u32) {
        // Compose all positioned images onto one RGBA bitmap the size of
        // the video, then set it as the layer contents on the main queue.
        let composed = compose(images, video_width, video_height);
        // SAFETY: dereferenced only on the main queue; +1 retain until then.
        let layer_ptr = std::sync::Arc::new(SendPtr(Retained::into_raw(
            self.layer.0.clone(),
        )));
        self.main.exec_async(move || {
            // SAFETY: non-null by construction.
            let layer = unsafe { Retained::from_raw(layer_ptr.0) }
                .expect("layer pointer null");
            unsafe {
                let contents: Option<&objc2::runtime::AnyObject> = composed
                    .as_deref()
                    .map(|i| i.as_ref() as &objc2::runtime::AnyObject);
                layer.setContents(contents);
                layer.setNeedsDisplay();
            }
        });
    }
}

/// Renders the positioned RGBA images into one video-sized RGBA bitmap.
fn compose(
    images: &[SubtitleImage],
    video_width: u32,
    video_height: u32,
) -> Option<CFRetained<CGImage>> {
    if images.is_empty() || video_width == 0 || video_height == 0 {
        return None;
    }
    let w = video_width as usize;
    let h = video_height as usize;
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
        // Straight → premultiplied alpha, then blit with clipping.
        let x0 = img.x.max(0) as usize;
        let y0 = img.y.max(0) as usize;
        for row in 0..img.height as usize {
            let dy = y0 + row;
            if dy >= h {
                break;
            }
            for col in 0..img.width as usize {
                let dx = x0 + col;
                if dx >= w {
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
    // Wrap in a CGImage; the NSData copy keeps the pixels alive.
    let data = NSData::with_bytes(&buf);
    // SAFETY: the NSData pointer is a valid CFData (toll-free bridged).
    let cf_data: &objc2_core_foundation::CFData =
        unsafe { &*(std::ptr::from_ref::<NSData>(&data).cast()) };
    let provider = CGDataProvider::with_cf_data(Some(cf_data)).expect("CGDataProvider");
    let space = CGColorSpace::new_device_rgb().expect("device RGB color space");
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
    Some(CFRetained::from(image))
}
