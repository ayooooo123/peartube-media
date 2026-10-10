use super::backend::BackendShared;
use super::surface::owner_capacity_changed;
use crate::backend::{SubtitleImage, SubtitleSink};
use crate::subtitle_compose::compose_straight;
use crate::video_owner::{try_spawn, OwnerError, OwnerTicket};
#[cfg(target_os = "android")]
use ndk::hardware_buffer_format::HardwareBufferFormat;
use parking_lot::{Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

struct DesiredOverlay {
    images: Vec<SubtitleImage>,
    video_width: u32,
    video_height: u32,
    /// True when show([],0,0) or surface loss must clear using actual geometry (C15).
    clear_with_surface_geometry: bool,
}

struct SubtitleShared {
    desired_overlay: Mutex<Option<DesiredOverlay>>,
    wake_owner: Condvar,
    retire: AtomicBool,
    /// Owner failure retained and observed (C14).
    owner_error: Mutex<Option<String>>,
    /// Capacity wake registered on the admitted surface binding.
    capacity_registered: AtomicBool,
}

pub struct AndroidSubtitleSink {
    backend: Arc<BackendShared>,
    state: Arc<SubtitleShared>,
    owner_ticket: Option<OwnerTicket>,
    needs_spawn: bool,
}

impl AndroidSubtitleSink {
    pub fn new(backend: Arc<BackendShared>) -> Self {
        let mut sink = Self {
            backend,
            state: Arc::new(SubtitleShared {
                desired_overlay: Mutex::new(None),
                wake_owner: Condvar::new(),
                retire: AtomicBool::new(false),
                owner_error: Mutex::new(None),
                capacity_registered: AtomicBool::new(false),
            }),
            owner_ticket: None,
            needs_spawn: true,
        };
        sink.try_start_owner();
        sink
    }

    fn observe_ticket(&mut self) {
        if let Some(ticket) = &self.owner_ticket {
            match ticket.poll() {
                Poll::Ready(Ok(())) => {
                    self.owner_ticket = None;
                    self.needs_spawn = true;
                }
                Poll::Ready(Err(e)) => {
                    *self.state.owner_error.lock() = Some(e);
                    self.owner_ticket = None;
                    self.needs_spawn = false;
                }
                Poll::Pending => {}
            }
        }
    }

    fn try_start_owner(&mut self) {
        self.observe_ticket();
        if self.owner_ticket.is_none() && self.needs_spawn {
            if self.state.owner_error.lock().is_some() {
                // Dead sink after owner failure; do not silently retain forever without observation.
                return;
            }
            let shared = self.state.clone();
            let backend = self.backend.clone();
            let wake = {
                let shared_w = shared.clone();
                Arc::new(move || {
                    shared_w.wake_owner.notify_all();
                })
            };
            // Register capacity wake on current subtitle surface before rechecking (C14).
            if let Some(binding) = self.backend.subtitle_surface_binding() {
                let shared_c = self.state.clone();
                binding.0.register_capacity_wake(Arc::new(move || {
                    shared_c.wake_owner.notify_all();
                }));
                self.state
                    .capacity_registered
                    .store(true, Ordering::SeqCst);
            }
            match try_spawn(
                move || run_subtitle_owner(shared, backend),
                wake,
            ) {
                Ok(ticket) => {
                    self.owner_ticket = Some(ticket);
                    self.needs_spawn = false;
                }
                Err(OwnerError::Capacity) => {
                    self.needs_spawn = true;
                    owner_capacity_changed();
                }
                Err(OwnerError::Spawn(e)) => {
                    *self.state.owner_error.lock() = Some(e);
                    self.needs_spawn = false;
                }
            }
        }
    }

    pub fn on_surface_lost(&mut self) {
        *self.state.desired_overlay.lock() = Some(DesiredOverlay {
            images: Vec::new(),
            video_width: 0,
            video_height: 0,
            clear_with_surface_geometry: true,
        });
        self.state.capacity_registered.store(false, Ordering::SeqCst);
        self.try_start_owner();
        self.state.wake_owner.notify_all();
    }

    pub fn on_surface_changed(&mut self) {
        self.state.capacity_registered.store(false, Ordering::SeqCst);
        self.try_start_owner();
        self.state.wake_owner.notify_all();
    }
}

impl SubtitleSink for AndroidSubtitleSink {
    fn show(&mut self, images: &[SubtitleImage], video_width: u32, video_height: u32) {
        if self.backend.is_suspended() {
            return;
        }

        self.observe_ticket();

        // show([], 0, 0) must clear old pixels using actual surface geometry (C15).
        if images.is_empty() && (video_width == 0 || video_height == 0) {
            *self.state.desired_overlay.lock() = Some(DesiredOverlay {
                images: Vec::new(),
                video_width: 0,
                video_height: 0,
                clear_with_surface_geometry: true,
            });
            self.try_start_owner();
            self.state.wake_owner.notify_all();
            return;
        }

        if video_width == 0 || video_height == 0 {
            *self.state.desired_overlay.lock() = Some(DesiredOverlay {
                images: Vec::new(),
                video_width: 0,
                video_height: 0,
                clear_with_surface_geometry: true,
            });
            self.try_start_owner();
            self.state.wake_owner.notify_all();
            return;
        }

        if video_width > 16384 || video_height > 16384 {
            return;
        }
        if u64::from(video_width) * u64::from(video_height) > 8192 * 8192 {
            return;
        }

        // Coalesce to latest desired overlay only (bounded).
        *self.state.desired_overlay.lock() = Some(DesiredOverlay {
            images: images.to_vec(),
            video_width,
            video_height,
            clear_with_surface_geometry: false,
        });

        self.try_start_owner();
        self.state.wake_owner.notify_all();
    }
}

impl Drop for AndroidSubtitleSink {
    fn drop(&mut self) {
        self.state.retire.store(true, Ordering::SeqCst);
        self.state.wake_owner.notify_all();
    }
}

fn run_subtitle_owner(
    shared: Arc<SubtitleShared>,
    backend: Arc<BackendShared>,
) -> Result<(), String> {
    #[cfg(target_os = "android")]
    let mut active_lease: Option<super::surface::SurfaceBindingLease> = None;
    #[cfg(target_os = "android")]
    let mut leased_id: Option<super::surface::SurfaceId> = None;

    loop {
        let (overlay_opt, retire) = {
            let mut guard = shared.desired_overlay.lock();
            while guard.is_none() && !shared.retire.load(Ordering::SeqCst) {
                shared
                    .wake_owner
                    .wait_for(&mut guard, Duration::from_millis(50));
            }
            let retire = shared.retire.load(Ordering::SeqCst);
            (guard.take(), retire)
        };

        if retire {
            #[cfg(target_os = "android")]
            {
                if let Some(lease) = active_lease.take() {
                    lease.release_healthy();
                }
            }
            return Ok(());
        }

        #[cfg(target_os = "android")]
        if let Some(overlay) = overlay_opt {
            // Observe surface loss/replacement around native work (C3).
            let binding = backend.subtitle_surface_binding();
            match binding {
                None => {
                    if let Some(lease) = active_lease.take() {
                        lease.release_healthy();
                    }
                    leased_id = None;
                    continue;
                }
                Some(binding) => {
                    if leased_id != Some(binding.id()) {
                        if let Some(lease) = active_lease.take() {
                            lease.release_healthy();
                        }
                        leased_id = None;
                        match binding.try_acquire_lease() {
                            Ok(lease) => {
                                leased_id = Some(binding.id());
                                active_lease = Some(lease);
                            }
                            Err(_) => {
                                // Capacity/lease busy: register wake and retry via capacity seam.
                                let shared_c = shared.clone();
                                binding.0.register_capacity_wake(Arc::new(move || {
                                    shared_c.wake_owner.notify_all();
                                }));
                                owner_capacity_changed();
                                // Put overlay back as latest desired.
                                let mut guard = shared.desired_overlay.lock();
                                if guard.is_none() {
                                    *guard = Some(overlay);
                                }
                                continue;
                            }
                        }
                    }

                    let lease = match active_lease.as_ref() {
                        Some(l) => l,
                        None => continue,
                    };

                    // Cancellation/loss check before native (C3).
                    if backend.is_suspended()
                        || backend.subtitle_surface_binding().map(|b| b.id()) != leased_id
                    {
                        if let Some(lease) = active_lease.take() {
                            lease.release_healthy();
                        }
                        leased_id = None;
                        continue;
                    }

                    let window = match backend.import_window_for_lease(lease) {
                        Ok(w) => w,
                        Err(e) => return Err(e),
                    };

                    let (geom_w, geom_h, clear_only) = if overlay.clear_with_surface_geometry
                        || overlay.video_width == 0
                        || overlay.video_height == 0
                    {
                        // Actual current surface geometry for empty clear (C15).
                        let w = window.width().max(0) as u32;
                        let h = window.height().max(0) as u32;
                        if w == 0 || h == 0 {
                            continue;
                        }
                        (w, h, true)
                    } else {
                        (overlay.video_width, overlay.video_height, false)
                    };

                    // Propagate geometry failure (C4).
                    window
                        .set_buffers_geometry(
                            geom_w as i32,
                            geom_h as i32,
                            Some(HardwareBufferFormat::R8G8B8A8_UNORM),
                        )
                        .map_err(|e| format!("subtitle geometry: {e:?}"))?;

                    // Recheck after geometry.
                    if backend.is_suspended()
                        || backend.subtitle_surface_binding().map(|b| b.id()) != leased_id
                    {
                        continue;
                    }

                    let mut guard = window
                        .lock(None)
                        .map_err(|e| format!("subtitle lock: {e:?}"))?;

                    // Use actual locked buffer format/dims/pitch/bounded length (C4).
                    let Some(bpp) = guard.format().bytes_per_pixel() else {
                        return Err("subtitle locked buffer format has no bytes_per_pixel".into());
                    };
                    let dst_stride = guard.stride();
                    let dst_height = guard.height();
                    let dst_width = guard.width();
                    let Some(dst) = guard.bytes() else {
                        return Err("subtitle locked buffer has no writable bytes".into());
                    };
                    let expected = dst_stride
                        .checked_mul(dst_height)
                        .and_then(|n| n.checked_mul(bpp))
                        .ok_or_else(|| "subtitle buffer size overflow".to_string())?;
                    if dst.len() < expected {
                        return Err("subtitle locked buffer shorter than stride*height*bpp".into());
                    }

                    // Clear.
                    for b in dst.iter_mut() {
                        b.write(0);
                    }

                    if !clear_only && !overlay.images.is_empty() {
                        // compose_straight expects &mut [u8]; init the region first then transmute view.
                        let init: &mut [u8] = unsafe {
                            std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast(), expected)
                        };
                        // Pitch in pixels for compose.
                        let pitch_px = if bpp == 0 { 0 } else { dst_stride };
                        let _ = dst_width;
                        compose_straight(
                            init,
                            pitch_px,
                            dst_height,
                            &overlay.images,
                            overlay.video_width.max(1),
                        );
                    }
                    drop(guard);
                    // NativeWindowBufferLockGuard drop posts.
                }
            }
        }

        #[cfg(not(target_os = "android"))]
        {
            let _ = backend;
            let _ = overlay_opt;
        }
    }
}
