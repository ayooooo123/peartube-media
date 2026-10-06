use super::backend::BackendShared;
use crate::backend::{SubtitleImage, SubtitleSink};
use crate::subtitle_compose::compose_straight;
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
        // Contract allocation limits for untrusted input: reject video
        // surfaces beyond the video caps (16384/side, 8192x8192 area).
        if video_width > 16384 || video_height > 16384 {
            return;
        }
        if u64::from(video_width) * u64::from(video_height) > 8192 * 8192 {
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

        // Compose subtitle images (all math bounds-checked; input comes
        // from untrusted peers).
        compose_straight(
            unsafe { std::slice::from_raw_parts_mut(dst_bits, dst_total_bytes) },
            dst_stride,
            dst_height,
            images,
            video_width,
        );
        // Dropping guard unlocks and posts
    }
}
