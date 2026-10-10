//! The Apple platform backend (macOS 11+, iOS 16): one
//! `AVSampleBufferRenderSynchronizer` per playback driving an
//! `AVSampleBufferAudioRenderer` and the `AVSampleBufferDisplayLayer`, so
//! video is shown on the audio clock and the engine reads that clock from
//! the synchronizer's timebase.

pub mod audio;
pub mod clock;
pub mod subtitles;
pub mod util;
pub mod video;

#[cfg(test)]
mod video_tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(any(target_os = "macos", target_os = "ios"))]
use dispatch2::run_on_main;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2_app_kit::NSView;
#[cfg(target_os = "ios")]
use objc2_ui_kit::UIView;
use objc2_av_foundation::{AVSampleBufferAudioRenderer, AVSampleBufferRenderSynchronizer};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_quartz_core::CALayer;

use crate::apple::audio::AppleAudioSink;
use crate::apple::clock::AppleClock;
use crate::apple::subtitles::AppleSubtitleSink;
use crate::apple::util::SendSync;
use crate::apple::video::AppleVideoSink;
use crate::backend::{AudioSink, Backend, Clock, SubtitleSink, VideoSink};

/// A view container with the two stacked layers: the video
/// (`AVSampleBufferDisplayLayer`) below, the subtitle layer above. Attached
/// to the app's view hierarchy by `attach`.
///
/// On macOS `attach` takes an `NSView*`; on iOS a `UIView*` (retained by
/// the caller for the lifetime of the backend — the backend only adds a
/// sublayer to it).
pub struct AppleBackend {
    /// The current playback's synchronizer (see `PlaybackSlot`).
    playback: PlaybackSlot,
    main: DispatchRetained<DispatchQueue>,
    /// Container layer added to the parent view.
    container: Mutex<Option<SendSync<Retained<CALayer>>>>,
    /// Video layer reused across playbacks.
    video_layer: SendSync<Retained<objc2_av_foundation::AVSampleBufferDisplayLayer>>,
    subtitle_layer: SendSync<Retained<CALayer>>,
    frame: Mutex<[f64; 4]>,
    /// Audio renderers made from now on are muted (see `set_muted`).
    muted: AtomicBool,
    layer_lease: Arc<parking_lot::Mutex<video::LayerLeaseState>>,
}

/// The current playback's synchronizer: `Backend::audio` makes it, the
/// video sink made after it joins it, and the audio sink clears it when the
/// playback lets go of it.
pub(crate) type PlaybackSlot = Arc<Mutex<Option<SendSync<Retained<AVSampleBufferRenderSynchronizer>>>>>;

// SAFETY: AVF/CoreMedia objects here are documented thread-safe, and all
// AppKit/UIKit view/layer work is dispatched onto `main`.
unsafe impl Send for AppleBackend {}
unsafe impl Sync for AppleBackend {}

/// The main queue with a +1 retain, callable from any thread.
fn main_queue() -> DispatchRetained<DispatchQueue> {
    // SAFETY: dispatch_get_main_queue's object lives forever.
    unsafe { DispatchRetained::retain(std::ptr::NonNull::from(DispatchQueue::main())) }
}

impl AppleBackend {
    /// Creates the backend. Must be called on the main thread (AppKit/
    /// UIKit layer creation).
    pub fn new() -> std::sync::Arc<Self> {
        // SAFETY: plain allocation on the main thread.
        let video_layer = unsafe { objc2_av_foundation::AVSampleBufferDisplayLayer::new() };
        let subtitle_layer = CALayer::new();
        subtitle_layer.setMasksToBounds(true);
        std::sync::Arc::new(Self {
            playback: Arc::new(Mutex::new(None)),
            main: main_queue(),
            container: Mutex::new(None),
            video_layer: SendSync(video_layer),
            subtitle_layer: SendSync(subtitle_layer),
            frame: Mutex::new([0.0; 4]),
            muted: AtomicBool::new(false),
            layer_lease: Arc::new(parking_lot::Mutex::new(video::LayerLeaseState::new())),
        })
    }

    /// Attaches the container layer to `parent` (an `NSView*` on macOS).
    /// The caller retains the parent; the backend only adds a sublayer.
    ///
    /// # Safety
    /// `parent` must point to a live NSView.
    #[cfg(target_os = "macos")]
    pub unsafe fn attach(&self, parent: *mut std::ffi::c_void) {
        let parent: *mut NSView = parent.cast();
        assert!(!parent.is_null(), "null NSView");
        let view = unsafe { Retained::retain(parent) }.expect("parent must be a live NSView");
        self.attach_to_view(&view);
    }

    /// Attaches the container layer to `parent` (a `UIView*` on iOS).
    ///
    /// # Safety
    /// `parent` must point to a live UIView.
    #[cfg(target_os = "ios")]
    pub unsafe fn attach(&self, parent: *mut std::ffi::c_void) {
        let parent: *mut UIView = parent.cast();
        assert!(!parent.is_null(), "null UIView");
        let view = unsafe { Retained::retain(parent) }.expect("parent must be a live UIView");
        self.attach_to_view_ios(&view);
    }

    #[cfg(target_os = "macos")]
    fn attach_to_view(&self, view: &NSView) {
        let container = CALayer::new();
        container.setMasksToBounds(true);
        container.setBackgroundColor(Some(&black_background()));
        container.addSublayer(&*self.video_layer);
        container.addSublayer(&*self.subtitle_layer);
        // Layer-backed view so our layer tree composites.
        view.setWantsLayer(true);
        if let Some(view_layer) = view.layer() {
            view_layer.addSublayer(&container);
        }
        let frame = *self.frame.lock().expect("frame lock");
        self.apply_frame_now(&container, frame);
        *self.container.lock().expect("container lock") = Some(SendSync(container));
    }

    #[cfg(target_os = "ios")]
    fn attach_to_view_ios(&self, view: &UIView) {
        let container = CALayer::new();
        container.setMasksToBounds(true);
        container.setBackgroundColor(Some(&black_background()));
        container.addSublayer(&*self.video_layer);
        container.addSublayer(&*self.subtitle_layer);
        view.layer().addSublayer(&container);
        let frame = *self.frame.lock().expect("frame lock");
        self.apply_frame_now(&container, frame);
        *self.container.lock().expect("container lock") = Some(SendSync(container));
    }

    /// Sets the container's frame in parent coordinates (points).
    pub fn set_frame(&self, rect: [f64; 4]) {
        *self.frame.lock().expect("frame lock") = rect;
        let container = self
            .container
            .lock()
            .expect("container lock")
            .as_ref()
            .map(|c| SendSync(c.0.clone()));
        if let Some(container) = container {
            self.apply_frame_now(&container, rect);
        }
    }

    fn apply_frame_now(&self, container: &CALayer, rect: [f64; 4]) {
        let cg = CGRect {
            origin: CGPoint { x: rect[0], y: rect[1] },
            size: CGSize { width: rect[2], height: rect[3] },
        };
        // SAFETY: setFrame is not inherently unsafe; the objc2 binding is
        // declared safe on CALayer.
        container.setFrame(cg);
        let _ = &self.main;
    }

    /// Removes the container layer from its parent and drops playback
    /// objects. Safe to call from any thread; layer work lands on main.
    pub fn detach(&self) {
        let container = self.container.lock().expect("container lock").take();
        if let Some(container) = container {
            let ptr = SendPtr(Retained::into_raw(container.0));
            self.main.exec_async(move || {
                // SAFETY: non-null by construction; +1 held until here.
                let layer = unsafe { Retained::from_raw(ptr.get()) }
                    .expect("layer null");
                layer.removeFromSuperlayer();
            });
        }
        *self.playback.lock().expect("playback lock") = None;
    }
}

/// Raw-pointer wrapper that is `Send + Sync`; the pointee is only
/// dereferenced on the main queue.
struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}
impl<T> SendPtr<T> {
    fn get(&self) -> *mut T {
        self.0
    }
}

/// `CFRetained` wrapper that is `Send + Sync` (CoreMedia CMTimebase is
/// documented thread-safe).
struct SendCF<T>(objc2_core_foundation::CFRetained<T>);
unsafe impl<T> Send for SendCF<T> {}
unsafe impl<T> Sync for SendCF<T> {}
impl<T> SendCF<T> {
    fn into_inner(self) -> objc2_core_foundation::CFRetained<T> {
        self.0
    }
}

/// Pure black background for the container layer.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn black_background() -> Retained<objc2_core_graphics::CGColor> {
    use objc2_core_graphics::{CGColor, CGColorSpace};
    let space = CGColorSpace::new_device_rgb().expect("device RGB color space");
    let components = [0.0f64, 0.0, 0.0, 1.0];
    // SAFETY: space/components valid for the call (CGColor copies them).
    let color = unsafe { CGColor::new(Some(&space), components.as_ptr()) }
        .expect("black CGColor");
    // SAFETY: CFRetained/Retained share representation and +1 semantics.
    unsafe { Retained::from_raw(CFRetainedColor::into_raw(color).as_ptr()) }
        .expect("CGColor null")
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
type CFRetainedColor = objc2_core_foundation::CFRetained<objc2_core_graphics::CGColor>;

impl AppleBackend {
    /// Mutes the audio renderers of the playbacks opened from now on: the
    /// audio still runs the clock, nothing is heard (verification
    /// harnesses on a shared machine).
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }
}

impl Backend for AppleBackend {
    fn audio(&self) -> Box<dyn AudioSink> {
        // One synchronizer + renderer per playback, created on main.
        // The new objects are `!Send`, so they stay on the main thread and
        // cross back as raw +1 pointers.
        let muted = self.muted.load(Ordering::Relaxed);
        let ptrs = run_on_main(move |_mtm| {
            // SAFETY: object allocation and the method calls; all run on
            // the main thread.
            let synchronizer = unsafe { AVSampleBufferRenderSynchronizer::new() };
            let renderer = unsafe { AVSampleBufferAudioRenderer::new() };
            let proto: Retained<objc2::runtime::ProtocolObject<
                dyn objc2_av_foundation::AVQueuedSampleBufferRendering,
            >> = objc2::runtime::ProtocolObject::from_retained(renderer.clone());
            unsafe {
                renderer.setMuted(muted);
                // The engine holds for decoded output and buffered media.
                // A second native hold can look like drained audio at EOF.
                synchronizer.setDelaysRateChangeUntilHasSufficientMediaData(false);
                synchronizer.addRenderer(&proto);
                let timebase: Retained<objc2_core_media::CMTimebase> =
                    objc2::msg_send![&*synchronizer, timebase];
                (
                    SendPtr(Retained::into_raw(synchronizer)),
                    SendPtr(Retained::into_raw(renderer)),
                    SendCF(retained_to_cf(timebase)),
                )
            }
        });
        let synchronizer = unsafe {
            Retained::from_raw(ptrs.0 .0).expect("synchronizer null")
        };
        let renderer = unsafe { Retained::from_raw(ptrs.1 .0).expect("renderer null") };
        let clock = Arc::new(AppleClock::from_timebase(ptrs.2.into_inner()));
        *self.playback.lock().expect("playback lock") = Some(SendSync(synchronizer.clone()));
        Box::new(AppleAudioSink::new(
            synchronizer,
            renderer,
            clock,
            Arc::clone(&self.playback),
        ))
    }

    /// The playback's audio output, made first, left its synchronizer here:
    /// the layer joins it and shows video on the audio clock. Without audio
    /// the layer follows `clock` on a timebase of its own.
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
        let synchronizer = self
            .playback
            .lock()
            .expect("playback lock")
            .as_ref()
            .map(|synchronizer| synchronizer.0.clone());
        Box::new(AppleVideoSink::new(
            self.video_layer.0.clone(),
            synchronizer,
            clock,
            Arc::clone(&self.layer_lease),
        ))
    }

    fn subtitles(&self) -> Box<dyn SubtitleSink> {
        let layer = self.subtitle_layer.0.clone();
        let ptr = SendPtr(Retained::into_raw(layer));
        let layer = unsafe { Retained::from_raw(ptr.0) }.expect("layer null");
        Box::new(AppleSubtitleSink::new(layer))
    }

    fn suspend(&self) {
        // Backgrounded: the engine pauses first; layers keep their content
        // and audio stops via rate 0.
    }

    fn resume(&self) {}
}

/// `Retained<T>` and `CFRetained<T>` share representation (+1 pointer).
fn retained_to_cf(
    value: Retained<objc2_core_media::CMTimebase>,
) -> objc2_core_foundation::CFRetained<objc2_core_media::CMTimebase> {
    let ptr = Retained::into_raw(value);
    // SAFETY: same +1 count, same pointer representation.
    unsafe {
        objc2_core_foundation::CFRetained::from_raw(std::ptr::NonNull::new_unchecked(ptr))
    }
}

#[cfg(target_os = "macos")]
impl AppleBackend {
    /// The video layer, for examples that inspect playback state.
    pub fn video_layer(&self) -> &Retained<objc2_av_foundation::AVSampleBufferDisplayLayer> {
        &self.video_layer.0
    }

    /// Blocks until the video layer reports `isReadyForDisplay` or
    /// `timeout_ms` elapses. Sequencing helper for display verification.
    pub fn wait_layer_ready(&self, timeout_ms: u64) -> bool {
        let video_layer = std::sync::Arc::new(SendPtr(Retained::into_raw(
            self.video_layer.0.clone(),
        )));
        run_on_main(move |_mtm| {
            // SAFETY: non-null by construction (+1 held for the hop); the
            // layer is dropped here on main at the end of the hop.
            let video_layer = unsafe { Retained::from_raw(video_layer.0) }
                .expect("video layer null");
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            // SAFETY: main-thread AVF call.
            while std::time::Instant::now() < deadline {
                if unsafe { video_layer.isReadyForDisplay() } {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            false
        })
    }

    /// Reads the currently displayed pixel buffer back through the layer's
    /// `AVSampleBufferVideoRenderer` (macOS 14+) and verifies it is not all
    /// black and matches the expected size. Runs on the main thread.
    /// Returns the displayed buffer's (width, height).
    pub fn verify_displayed(
        &self,
        expect_w: u32,
        expect_h: u32,
    ) -> Result<(usize, usize), String> {
        // The layer is !Send; hand the +1 across as a raw pointer inside an
        // Arc (SendPtr is Send + Sync).
        let video_layer = std::sync::Arc::new(SendPtr(Retained::into_raw(
            self.video_layer.0.clone(),
        )));
        let (w, h, non_black) = run_on_main(move |_mtm| {
            // SAFETY: non-null by construction (+1 held for the hop).
            let video_layer = unsafe {
                Retained::from_raw(video_layer.0)
            }
            .expect("video layer null");
            // SAFETY: copyDisplayedPixelBuffer follows the Get rule
            // (autoreleased) but the message send itself is unsafe.
            // SAFETY: message send on a main-thread AVF object.
            let renderer = unsafe { video_layer.sampleBufferRenderer() };
            let pb: Option<Retained<objc2_core_video::CVPixelBuffer>> = unsafe {
                objc2::msg_send![&*renderer, copyDisplayedPixelBuffer]
            };
            let Some(pb) = pb else {
                return (0usize, 0usize, 0u64);
            };
            // SAFETY: CVPixelBuffer accessors are safe with a valid buffer;
            // the lock/unlock pair guards the base address.
            unsafe {
                let (w, h) = (
                    objc2_core_video::CVPixelBufferGetWidth(&pb),
                    objc2_core_video::CVPixelBufferGetHeight(&pb),
                );
                if objc2_core_video::CVPixelBufferLockBaseAddress(
                    &pb,
                    objc2_core_video::CVPixelBufferLockFlags(0),
                ) != 0
                {
                    return (w, h, 0u64);
                }
                let base = objc2_core_video::CVPixelBufferGetBaseAddress(&pb);
                let stride = objc2_core_video::CVPixelBufferGetBytesPerRow(&pb);
                let mut non_black = 0u64;
                if !base.is_null() {
                    'outer: for row in 0..h {
                        for px in 0..w {
                            let off = row as usize * stride + px as usize * 4;
                            let b = std::slice::from_raw_parts(
                                (base as *const u8).add(off),
                                4,
                            );
                            if b.iter().any(|&v| v > 8) {
                                non_black += 1;
                                if non_black > 64 {
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
                objc2_core_video::CVPixelBufferUnlockBaseAddress(
                    &pb,
                    objc2_core_video::CVPixelBufferLockFlags(0),
                );
                (w, h, non_black)
            }
        });
        if w == 0 || h == 0 {
            return Err("renderer displayed no pixel buffer".into());
        }
        if expect_w != 0 && (w as u32, h as u32) != (expect_w, expect_h) {
            return Err(format!(
                "displayed {w}x{h} does not match stream {expect_w}x{expect_h}"
            ));
        }
        if non_black <= 64 {
            return Err("displayed buffer is entirely black".into());
        }
        Ok((w, h))
    }
}
