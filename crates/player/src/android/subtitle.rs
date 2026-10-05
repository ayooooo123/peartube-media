use super::backend::BackendShared;
use crate::backend::{SubtitleImage, SubtitleSink};
use ndk::hardware_buffer_format::HardwareBufferFormat;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub struct AndroidSubtitleSink {
    backend: Arc<BackendShared>,
}

impl AndroidSubtitleSink {
    pub fn new(backend: Arc<BackendShared>) -> Self {
        Self { backend }
    }
}

impl SubtitleSink for AndroidSubtitleSink {
    fn show(&mut self, images: &[SubtitleImage], video_width: u32, video_height: u32) {
        if self.backend.is_suspended.load(Ordering::SeqCst) {
            return;
        }

        let window = match self.backend.subtitle_window.read().clone() {
            Some(w) => w,
            None => return,
        };

        if video_width == 0 || video_height == 0 {
            return;
        }

        let _ = window.set_buffers_geometry(
            video_width as i32,
            video_height as i32,
            Some(HardwareBufferFormat::R8G8B8A8_UNORM),
        );

        let mut guard = match window.lock(None) {
            Ok(g) => g,
            Err(_) => return,
        };

        let dst_stride = guard.stride();
        let dst_height = guard.height();
        let dst_bits = guard.bits() as *mut u8;
        let dst_total_bytes = dst_stride * dst_height * 4;

        // Clear buffer to transparent
        unsafe {
            std::ptr::write_bytes(dst_bits, 0, dst_total_bytes);
        }

        // Compose subtitle images
        for img in images {
            if img.width == 0 || img.height == 0 || img.rgba.is_empty() {
                continue;
            }
            let src_stride = img.width as usize * 4;
            for row in 0..img.height as i32 {
                let target_y = img.y + row;
                if target_y < 0 || target_y >= dst_height as i32 {
                    continue;
                }
                for col in 0..img.width as i32 {
                    let target_x = img.x + col;
                    if target_x < 0
                        || target_x >= dst_stride as i32
                        || target_x >= video_width as i32
                    {
                        continue;
                    }
                    let src_idx = (row as usize * src_stride) + (col as usize * 4);
                    if src_idx + 4 <= img.rgba.len() {
                        let src_pixel: [u8; 4] = [
                            img.rgba[src_idx],
                            img.rgba[src_idx + 1],
                            img.rgba[src_idx + 2],
                            img.rgba[src_idx + 3],
                        ];
                        let dst_idx = ((target_y as usize * dst_stride) + target_x as usize) * 4;
                        unsafe {
                            let dst_pixel: [u8; 4] = [
                                *dst_bits.add(dst_idx),
                                *dst_bits.add(dst_idx + 1),
                                *dst_bits.add(dst_idx + 2),
                                *dst_bits.add(dst_idx + 3),
                            ];
                            let blended = oxideav_pixfmt::over_straight(src_pixel, dst_pixel);
                            *dst_bits.add(dst_idx) = blended[0];
                            *dst_bits.add(dst_idx + 1) = blended[1];
                            *dst_bits.add(dst_idx + 2) = blended[2];
                            *dst_bits.add(dst_idx + 3) = blended[3];
                        }
                    }
                }
            }
        }
        // Dropping guard unlocks and posts
    }
}
