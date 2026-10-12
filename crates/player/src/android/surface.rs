//! Android Surface registry, linear reservations, exclusive leases, and retirement.
//!
//! Process-wide registry with a strict 16-credit capacity limit (four owner slots times
//! current + replacement video/subtitle pairs).
//!
//! Lock order (never reverse; never run arbitrary callbacks while holding any of these):
//!   1. `SurfaceRegistry.inner`
//!   2. `SurfaceBindingInner.state`
//!   3. `SurfaceBindingInner.cleanup_ticket`
//!   4. `SurfaceBindingInner.retirement_waker` / `capacity_wake`
//!   5. handle locks (`native_window`, `global_ref`)

use crate::video_owner::{try_spawn, OwnerError, OwnerTicket};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll, Waker};

#[cfg(target_os = "android")]
use jni::objects::{GlobalRef, JObject};
#[cfg(target_os = "android")]
use jni::JNIEnv;
#[cfg(target_os = "android")]
use jni::JavaVM;
#[cfg(target_os = "android")]
use ndk::native_window::NativeWindow;


pub const TOTAL_SURFACE_CREDITS: usize = 16;

static NEXT_SURFACE_ID: AtomicU64 = AtomicU64::new(1);
static GLOBAL_REGISTRY: std::sync::LazyLock<Arc<SurfaceRegistry>> =
    std::sync::LazyLock::new(|| Arc::new(SurfaceRegistry::new()));

/// Process-unique 64-bit surface identifier, never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SurfaceId(pub u64);

impl std::fmt::Display for SurfaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SurfaceId({})", self.0)
    }
}

/// Status of surface retirement observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceRetirementStatus {
    Pending,
    Retired,
    Failed(String),
}

/// Why surface admission was refused.
#[derive(Debug, thiserror::Error)]
pub enum SurfaceAdmissionError {
    #[error("surface registry capacity exhausted (16 maximum credits)")]
    Capacity,
    #[error("surface is retiring")]
    Retiring(SurfaceRetirement),
    #[error("invalid surface")]
    InvalidSurface,
    #[error("JNI error: {0}")]
    Jni(String),
}

/// Why surface retirement failed.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum SurfaceRetirementError {
    #[error("cleanup failed: {0}")]
    Failed(String),
    #[error("cleanup quarantined: {0}")]
    Quarantined(String),
}

/// Why binding a surface to a backend failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SurfaceBindError {
    #[error("surface binding is retiring")]
    Retiring,
    #[error("surface binding is retired")]
    Retired,
    #[error("surface binding is already leased exclusively")]
    AlreadyLeased,
}

/// Receipt proving native cleanup and owner exit/reap completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceRetired(pub SurfaceId);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BindingPhase {
    Active { lease_holder: Option<u64> },
    Retiring { lease_holder: Option<u64> },
    Retired,
    Quarantined(String),
}

/// Handles still charged against a registration credit until owner cleanup/reap.
struct NativeHandles {
    #[cfg(target_os = "android")]
    global_ref: Option<GlobalRef>,
    #[cfg(target_os = "android")]
    windows: Vec<NativeWindow>,
    /// Host/test stand-in: retained non-native ownership still blocks success.
    retained_marker: bool,
}

impl NativeHandles {
    fn empty() -> Self {
        Self {
            #[cfg(target_os = "android")]
            global_ref: None,
            #[cfg(target_os = "android")]
            windows: Vec::new(),
            retained_marker: false,
        }
    }

    fn is_empty(&self) -> bool {
        #[cfg(target_os = "android")]
        {
            self.global_ref.is_none() && self.windows.is_empty() && !self.retained_marker
        }
        #[cfg(not(target_os = "android"))]
        {
            !self.retained_marker
        }
    }
}

pub(crate) struct SurfaceBindingInner {
    pub(crate) id: SurfaceId,
    pub(crate) slot_index: usize,
    pub(crate) registry: Arc<SurfaceRegistry>,
    pub(crate) native_ptr: AtomicU64,
    pub(crate) state: Mutex<BindingPhase>,
    /// All native values for this binding. Moved under short locks only.
    handles: Mutex<NativeHandles>,
    pub(crate) cleanup_ticket: Mutex<Option<OwnerTicket>>,
    pub(crate) retirement_waker: Mutex<Option<Waker>>,
    /// Coalesced latest capacity/progress callback (subtitle last-show, etc.).
    capacity_wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Duplicate-native windows charged on sibling cleanup slots until reap.
    pending_dup_slots: Mutex<[Option<SurfaceId>; TOTAL_SURFACE_CREDITS]>,
    /// Test/host marker that native ownership remains.
    handles_retained: AtomicBool,
}

impl SurfaceBindingInner {
    fn phase_snapshot(&self) -> BindingPhase {
        self.state.lock().clone()
    }

    pub(crate) fn slot_is_free(&self) -> bool {
        let reg = self.registry.inner.lock();
        matches!(reg.slots[self.slot_index], SlotState::Free)
    }

    #[cfg(test)]
    pub(crate) fn force_phase_for_test(&self, phase: BindingPhase) {
        *self.state.lock() = phase;
    }

    #[cfg(test)]
    pub(crate) fn mark_native_handles_retained_for_test(&self) {
        self.handles_retained.store(true, Ordering::SeqCst);
        self.handles.lock().retained_marker = true;
    }

    /// Complete retirement only when native ownership is gone. Errors quarantine.
    pub(crate) fn finish_retirement(&self, result: Result<(), String>) {
        let id = self.id;
        let slot_idx = self.slot_index;
        let mut wake = None;
        let mut capacity_cb = None;


        // Lock order: registry → state → wakers. Never reverse.
        {
            let mut reg = self.registry.inner.lock();
            let mut state = self.state.lock();

            match result {
                Ok(()) => {
                    let mut duplicates_pending = false;
                    let mut duplicate_failure = None;
                    for (index, duplicate_id) in self.pending_dup_slots.lock().iter_mut().enumerate() {
                        let Some(expected_id) = *duplicate_id else { continue };
                        match &reg.slots[index] {
                            SlotState::Reserved { id, cleanup_ticket, .. } if *id == expected_id => {
                                duplicates_pending = true;
                                if let Some(Poll::Ready(Err(error))) = cleanup_ticket.as_ref().map(OwnerTicket::poll) {
                                    duplicate_failure = Some(error);
                                }
                            }
                            _ => *duplicate_id = None,
                        }
                    }
                    if let Some(error) = duplicate_failure {
                        *state = BindingPhase::Quarantined(format!("duplicate window cleanup: {error}"));
                        drop(state);
                        drop(reg);
                        let wake = self.retirement_waker.lock().take();
                        if let Some(waker) = wake {
                            waker.wake();
                        }
                        return;
                    }
                    if duplicates_pending {
                        return;
                    }
                    let handles_empty = self.handles.lock().is_empty()
                        && !self.handles_retained.load(Ordering::SeqCst);
                    if !handles_empty {
                        *state = BindingPhase::Quarantined(
                            "cleanup reported success while native handles remain".into(),
                        );
                    } else if matches!(
                        *state,
                        BindingPhase::Retiring { .. } | BindingPhase::Quarantined(_)
                    ) {
                        let free_ok = match &reg.slots[slot_idx] {
                            SlotState::Committed(existing) if existing.id == id => true,
                            SlotState::CleanupArmed {
                                id: armed_id,
                                binding,
                                ..
                            } if *armed_id == id || binding.id == id => true,
                            _ => false,
                        };
                        if free_ok {
                            reg.slots[slot_idx] = SlotState::Free;
                            *state = BindingPhase::Retired;

                        } else if matches!(*state, BindingPhase::Retiring { .. }) {
                            *state = BindingPhase::Retired;
                        }
                    }
                }
                Err(e) => {
                    if !matches!(*state, BindingPhase::Retired) {
                        *state = BindingPhase::Quarantined(e);
                    }
                }
            }

            wake = self.retirement_waker.lock().take();
            capacity_cb = self.capacity_wake.lock().clone();
        }

        if let Some(w) = wake {
            w.wake();
        }
        if let Some(cb) = capacity_cb {
            cb();
        }

    }

    /// Register coalesced capacity callback before rechecking capacity (missed-wake close).
    pub(crate) fn register_capacity_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        *self.capacity_wake.lock() = Some(wake);
    }

    pub(crate) fn take_capacity_wake(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        self.capacity_wake.lock().clone()
    }

    /// Owner-only: move existing window out briefly or import under lease.
    #[cfg(target_os = "android")]
    pub(crate) fn take_window_snapshot(&self) -> Option<NativeWindow> {
        let handles = self.handles.lock();
        handles.windows.first().cloned()
    }

    #[cfg(target_os = "android")]
    pub(crate) fn store_imported_window(&self, window: NativeWindow) {
        let ptr = window.ptr().as_ptr() as u64;
        self.native_ptr.store(ptr, Ordering::Release);
        let mut handles = self.handles.lock();
        if handles.windows.is_empty() {
            handles.windows.push(window);
        } else {
            // Extra clone is still charged on this binding until cleanup.
            handles.windows.push(window);
        }
    }

    #[cfg(target_os = "android")]
    pub(crate) fn import_window_from_global_ref(&self) -> Result<NativeWindow, String> {
        if let Some(w) = self.take_window_snapshot() {
            return Ok(w);
        }
        let gr = {
            let mut handles = self.handles.lock();
            handles.global_ref.take()
        };
        let Some(gr) = gr else {
            return Err("no GlobalRef available for NativeWindow import".into());
        };
        let vm = match self.registry.java_vm() {
            Some(vm) => vm,
            None => {
                let mut handles = self.handles.lock();
                handles.global_ref = Some(gr);
                return Err("JavaVM not available for NativeWindow import".into());
            }
        };
        let imported = (|| -> Result<NativeWindow, String> {
            let mut env = vm
                .attach_current_thread()
                .map_err(|e| format!("attach for import: {e:?}"))?;
            let raw_env = env.get_raw();
            let raw_surface = gr.as_raw();
            unsafe { NativeWindow::from_surface(raw_env.cast(), raw_surface) }
                .ok_or_else(|| "NativeWindow::from_surface failed".into())
        })();
        {
            let mut handles = self.handles.lock();
            handles.global_ref = Some(gr);
            if let Ok(w) = &imported {
                let ptr = w.ptr().as_ptr() as u64;
                self.native_ptr.store(ptr, Ordering::Release);
                handles.windows.push(w.clone());
            }
        }
        imported
    }

    fn take_handles_for_cleanup(&self) -> NativeHandles {
        let mut handles = self.handles.lock();
        let mut out = NativeHandles::empty();
        std::mem::swap(&mut *handles, &mut out);
        self.handles_retained.store(false, Ordering::SeqCst);
        out
    }

    pub(crate) fn ensure_cleanup_started(self: &Arc<Self>) {
        {
            // Admission and the native-handle move are one transaction. Two
            // observers cannot arm an empty cleanup ahead of the real handles.
            let mut reg = self.registry.inner.lock();
            let state = self.state.lock();
            if !matches!(*state, BindingPhase::Retiring { lease_holder: None }) {
                return;
            }
            if self.cleanup_ticket.lock().is_some() {
                return;
            }
            match &reg.slots[self.slot_index] {
                SlotState::Committed(existing) if existing.id == self.id => {
                    let payload = CleanupPayload {
                        binding_id: self.id,
                        slot_index: self.slot_index,
                        handles: self.take_handles_for_cleanup(),
                        registry: Arc::downgrade(&self.registry),
                        binding: Arc::downgrade(self),
                    };
                    reg.slots[self.slot_index] = SlotState::CleanupArmed {
                        id: self.id,
                        binding: Arc::clone(self),
                        payload: Some(payload),
                    };
                }
                SlotState::CleanupArmed { id, .. } if *id == self.id => {}
                _ => return,
            }
        }
        self.spawn_cleanup_from_armed_slot();
    }

    fn spawn_cleanup_from_armed_slot(self: &Arc<Self>) {
        // Keep publication atomic with admission. The pool never invokes a
        // callback under its lock or waits for this new owner; no native call
        // runs here. A capacity notification cannot miss a temporarily removed
        // payload or race another caller into spawning a second cleanup.
        let mut reg = self.registry.inner.lock();
        if self.cleanup_ticket.lock().is_some() {
            return;
        }
        let payload = match &mut reg.slots[self.slot_index] {
            SlotState::CleanupArmed { id, payload, .. } if *id == self.id => payload.take(),
            _ => None,
        };

        let Some(payload) = payload else {
            return;
        };

        // Put payload back into a side channel on the binding so Capacity cannot lose it.
        let payload = Arc::new(Mutex::new(Some(payload)));
        let payload_for_run = payload.clone();
        let inner = self.clone();
        let wake = {
            let inner_w = Arc::downgrade(&inner);
            Arc::new(move || {
                if let Some(b) = inner_w.upgrade() {
                    let w = b.retirement_waker.lock().clone();
                    if let Some(waker) = w {
                        waker.wake();
                    }
                    let capacity_wake = b.capacity_wake.lock().clone();
                    if let Some(cb) = capacity_wake {
                        cb();
                    }
                }
            })
        };

        match try_spawn(
            move || run_cleanup_payload(payload_for_run),
            wake,
        ) {
            Ok(ticket) => {
                *self.cleanup_ticket.lock() = Some(ticket.clone());
                drop(reg);
                // The owner may have been reaped before its ticket was installed.
                if let Poll::Ready(result) = ticket.poll() {
                    self.finish_retirement(result);
                }
            }
            Err(OwnerError::Capacity) => {
                // Restore payload into armed slot; credit stays charged.
                if let SlotState::CleanupArmed {
                    id,
                    payload: slot_payload,
                    ..
                } = &mut reg.slots[self.slot_index]
                {
                    if *id == self.id {
                        *slot_payload = payload.lock().take();
                    }
                }
            }
            Err(OwnerError::Spawn(e)) => {
                // Restore handles onto binding and quarantine; do not free credit.
                if let Some(p) = payload.lock().take() {
                    *self.handles.lock() = p.handles;
                }
                drop(reg);
                self.finish_retirement(Err(format!("cleanup spawn error: {e}")));
            }
        }
    }
}

struct CleanupPayload {
    binding_id: SurfaceId,
    slot_index: usize,
    handles: NativeHandles,
    registry: Weak<SurfaceRegistry>,
    binding: Weak<SurfaceBindingInner>,
}

fn run_cleanup_payload(payload_cell: Arc<Mutex<Option<CleanupPayload>>>) -> Result<(), String> {
    let mut payload = payload_cell
        .lock()
        .take()
        .ok_or_else(|| "cleanup payload missing".to_string())?;

    #[cfg(target_os = "android")]
    {
        // Drop windows first (may post/unlock paths).
        payload.handles.windows.clear();

        if let Some(gr) = payload.handles.global_ref.take() {
            let registry = payload
                .registry
                .upgrade()
                .ok_or_else(|| "registry gone during GlobalRef release".to_string())?;
            let vm = registry
                .java_vm()
                .ok_or_else(|| "cannot release GlobalRef: JavaVM not available".to_string())?;
            let mut _env = vm
                .attach_current_thread()
                .map_err(|e| format!("cannot attach JVM for GlobalRef release: {e:?}"))?;
            drop(gr);
            drop(_env);
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        payload.handles.retained_marker = false;
    }

    if let Some(binding) = payload.binding.upgrade() {
        // Ensure handles empty on binding view.
        binding.handles_retained.store(false, Ordering::SeqCst);
        *binding.handles.lock() = NativeHandles::empty();
    }

    let _ = (payload.binding_id, payload.slot_index);
    Ok(())
}

/// Committed, ref-counted surface binding.
pub struct SurfaceBinding(pub(crate) Arc<SurfaceBindingInner>);

impl SurfaceBinding {
    pub fn id(&self) -> SurfaceId {
        self.0.id
    }

    pub fn is_retiring(&self) -> bool {
        !matches!(*self.0.state.lock(), BindingPhase::Active { .. })
    }

    pub fn is_retired(&self) -> bool {
        matches!(*self.0.state.lock(), BindingPhase::Retired)
    }

    /// Atomic admit for backend publication under the caller's surface lock.
    pub(crate) fn admit_for_publish(&self) -> Result<(), SurfaceBindError> {
        let state = self.0.state.lock();
        match *state {
            BindingPhase::Active { .. } => Ok(()),
            BindingPhase::Retiring { .. } => Err(SurfaceBindError::Retiring),
            BindingPhase::Retired | BindingPhase::Quarantined(_) => Err(SurfaceBindError::Retired),
        }
    }

    pub fn retire(self: &Arc<Self>) -> SurfaceRetirement {
        let (was_active, lease_held) = {
            let mut state = self.0.state.lock();
            match *state {
                BindingPhase::Active { lease_holder } => {
                    *state = BindingPhase::Retiring { lease_holder };
                    (true, lease_holder.is_some())
                }
                BindingPhase::Retiring { lease_holder } => (false, lease_holder.is_some()),
                BindingPhase::Retired | BindingPhase::Quarantined(_) => (false, true),
            }
        };

        if was_active {
            // Invalidate publishers by binding ID outside registry/state locks.
            self.0.registry.notify_retire_invalidation(self.id());
        }

        if !lease_held {
            self.0.ensure_cleanup_started();
        }

        SurfaceRetirement {
            binding: self.clone(),
        }
    }

    pub fn try_acquire_lease(&self) -> Result<SurfaceBindingLease, SurfaceBindError> {
        let mut state = self.0.state.lock();
        match &mut *state {
            BindingPhase::Active { lease_holder } => {
                if lease_holder.is_some() {
                    Err(SurfaceBindError::AlreadyLeased)
                } else {
                    *lease_holder = Some(1);
                    Ok(SurfaceBindingLease {
                        binding: self.0.clone(),
                        released: false,
                    })
                }
            }
            BindingPhase::Retiring { .. } => Err(SurfaceBindError::Retiring),
            BindingPhase::Retired | BindingPhase::Quarantined(_) => Err(SurfaceBindError::Retired),
        }
    }
}

/// Exclusive lease over a SurfaceBinding. Dropping without explicit release quarantines.
pub struct SurfaceBindingLease {
    binding: Arc<SurfaceBindingInner>,
    released: bool,
}

impl SurfaceBindingLease {
    pub fn binding_id(&self) -> SurfaceId {
        self.binding.id
    }

    pub fn binding(&self) -> Arc<SurfaceBinding> {
        Arc::new(SurfaceBinding(self.binding.clone()))
    }

    /// Verified release after all associated native codec/window work finished.
    pub fn release_healthy(mut self) {
        self.released = true;
        let should_cleanup = {
            let mut state = self.binding.state.lock();
            match &mut *state {
                BindingPhase::Active { lease_holder } => {
                    *lease_holder = None;
                    false
                }
                BindingPhase::Retiring { lease_holder } => {
                    *lease_holder = None;
                    true
                }
                _ => false,
            }
        };
        if should_cleanup {
            self.binding.ensure_cleanup_started();
        }
        // Progress pending retirement waiters and capacity subscribers.
        let wake = self.binding.retirement_waker.lock().clone();
        let cap = self.binding.capacity_wake.lock().clone();
        if let Some(w) = wake {
            w.wake();
        }
        if let Some(cb) = cap {
            cb();
        }
        owner_capacity_changed();
    }

    #[cfg(target_os = "android")]
    pub(crate) fn ensure_window(&self) -> Result<NativeWindow, String> {
        self.binding.import_window_from_global_ref()
    }
}

impl Drop for SurfaceBindingLease {
    fn drop(&mut self) {
        if !self.released {
            {
                let mut state = self.binding.state.lock();
                *state = BindingPhase::Quarantined(
                    "lease dropped without verified native cleanup".into(),
                );
            }
            let wake = self.binding.retirement_waker.lock().take();
            let cap = self.binding.capacity_wake.lock().clone();
            if let Some(w) = wake {
                w.wake();
            }
            if let Some(cb) = cap {
                cb();
            }
        }
    }
}

/// Observable ticket monitoring surface retirement completion.
#[derive(Clone)]
pub struct SurfaceRetirement {
    binding: Arc<SurfaceBinding>,
}

impl std::fmt::Debug for SurfaceRetirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurfaceRetirement").field("id", &self.id()).finish()
    }
}

impl SurfaceRetirement {
    pub fn id(&self) -> SurfaceId {
        self.binding.id()
    }

    pub fn status(&self) -> SurfaceRetirementStatus {
        match self.binding.0.phase_snapshot() {
            BindingPhase::Active { .. } | BindingPhase::Retiring { .. } => {
                SurfaceRetirementStatus::Pending
            }
            BindingPhase::Retired => SurfaceRetirementStatus::Retired,
            BindingPhase::Quarantined(reason) => SurfaceRetirementStatus::Failed(reason),
        }
    }

    pub fn is_retired(&self) -> bool {
        matches!(self.binding.0.phase_snapshot(), BindingPhase::Retired)
    }

    pub fn poll(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<SurfaceRetired, SurfaceRetirementError>> {
        // Register waker BEFORE observing ticket completion (lost-wake close).
        {
            let mut waker_guard = self.binding.0.retirement_waker.lock();
            *waker_guard = Some(cx.waker().clone());
        }

        self.binding.0.ensure_cleanup_started();

        let ticket_ready = {
            let ticket_guard = self.binding.0.cleanup_ticket.lock();
            ticket_guard.as_ref().map(|t| t.poll())
        };

        match ticket_ready {
            Some(Poll::Ready(Ok(()))) => {
                self.binding.0.finish_retirement(Ok(()));
            }
            Some(Poll::Ready(Err(e))) => {
                self.binding.0.finish_retirement(Err(e));
            }
            Some(Poll::Pending) | None => {}
        }

        match self.binding.0.phase_snapshot() {
            BindingPhase::Retired => Poll::Ready(Ok(SurfaceRetired(self.binding.id()))),
            BindingPhase::Quarantined(reason) => {
                Poll::Ready(Err(SurfaceRetirementError::Quarantined(reason)))
            }
            BindingPhase::Active { .. } | BindingPhase::Retiring { .. } => Poll::Pending,
        }
    }
}

pub(crate) trait BackendRetireObserver: Send + Sync {
    fn on_surface_retired(&self, id: SurfaceId);
}

enum SlotState {
    Free,
    /// Credit charged; may hold a GlobalRef before commit or during rollback cleanup.
    Reserved {
        id: SurfaceId,
        #[cfg(target_os = "android")]
        global_ref: Option<GlobalRef>,
        #[cfg(target_os = "android")]
        pending_window: Option<NativeWindow>,
        /// Arc-owned rollback handles charged in-slot until owner cleanup takes them.
        /// Survives Capacity/Spawn reject without dropping natives on the caller.
        #[cfg(target_os = "android")]
        rollback: Option<Arc<Mutex<Option<ReservedRollback>>>>,
        /// Rollback/dup cleanup ticket retained until reap.
        cleanup_ticket: Option<OwnerTicket>,
    },
    Committed(Arc<SurfaceBindingInner>),
    /// Handles moved here before try_spawn; credit remains until cleanup/reap.
    CleanupArmed {
        id: SurfaceId,
        binding: Arc<SurfaceBindingInner>,
        payload: Option<CleanupPayload>,
    },
}

/// Native values for a reserved-slot rollback, owned via Arc in the charged slot.
#[cfg(target_os = "android")]
struct ReservedRollback {
    global_ref: Option<GlobalRef>,
    window: Option<NativeWindow>,
}

#[cfg(target_os = "android")]
impl ReservedRollback {
    fn is_empty(&self) -> bool {
        self.global_ref.is_none() && self.window.is_none()
    }
}

struct RegistryInner {
    slots: [SlotState; TOTAL_SURFACE_CREDITS],
    observers: Vec<Weak<dyn BackendRetireObserver>>,
}

impl RegistryInner {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| SlotState::Free),
            observers: Vec::new(),
        }
    }

    fn alloc_free_locked(&mut self, id: SurfaceId) -> Result<usize, SurfaceAdmissionError> {
        let free_idx = self
            .slots
            .iter()
            .position(|s| matches!(s, SlotState::Free))
            .ok_or(SurfaceAdmissionError::Capacity)?;
        self.slots[free_idx] = SlotState::Reserved {
            id,
            #[cfg(target_os = "android")]
            global_ref: None,
            #[cfg(target_os = "android")]
            pending_window: None,
            #[cfg(target_os = "android")]
            rollback: None,
            cleanup_ticket: None,
        };
        Ok(free_idx)
    }
}

/// Process-wide registry for Android Surface bindings and linear reservations.
pub struct SurfaceRegistry {
    inner: Mutex<RegistryInner>,
    #[cfg(target_os = "android")]
    java_vm: Mutex<Option<Arc<JavaVM>>>,
}

impl SurfaceRegistry {
    pub fn global() -> Arc<Self> {
        GLOBAL_REGISTRY.clone()
    }

    /// Process-global only; not a public alternate registry escape.
    fn new() -> Self {
        Self {
            inner: Mutex::new(RegistryInner::new()),
            #[cfg(target_os = "android")]
            java_vm: Mutex::new(None),
        }
    }

    fn next_id(&self) -> SurfaceId {
        SurfaceId(NEXT_SURFACE_ID.fetch_add(1, Ordering::Relaxed))
    }

    pub(crate) fn register_observer(&self, observer: Weak<dyn BackendRetireObserver>) {
        let mut inner = self.inner.lock();
        inner.observers.retain(|w| w.upgrade().is_some());
        if inner.observers.len() < TOTAL_SURFACE_CREDITS {
            inner.observers.push(observer);
        }
    }

    pub(crate) fn notify_retire_invalidation(&self, id: SurfaceId) {
        let observers: Vec<Arc<dyn BackendRetireObserver>> = {
            let inner = self.inner.lock();
            inner
                .observers
                .iter()
                .filter_map(|w| w.upgrade())
                .collect()
        };
        for obs in observers {
            obs.on_surface_retired(id);
        }
    }

    #[cfg(target_os = "android")]
    pub(crate) fn java_vm(&self) -> Option<Arc<JavaVM>> {
        self.java_vm.lock().clone()
    }

    #[cfg(test)]
    pub(crate) fn install_test_binding(
        self: &Arc<Self>,
        id: SurfaceId,
    ) -> Result<usize, SurfaceAdmissionError> {
        let mut inner = self.inner.lock();
        let slot = inner.alloc_free_locked(id)?;
        let binding_inner = Arc::new(SurfaceBindingInner {
            id,
            slot_index: slot,
            registry: self.clone(),
            native_ptr: AtomicU64::new(0),
            state: Mutex::new(BindingPhase::Active { lease_holder: None }),
            handles: Mutex::new(NativeHandles::empty()),
            cleanup_ticket: Mutex::new(None),
            retirement_waker: Mutex::new(None),
            capacity_wake: Mutex::new(None),
            pending_dup_slots: Mutex::new([None; TOTAL_SURFACE_CREDITS]),
            handles_retained: AtomicBool::new(false),
        });
        inner.slots[slot] = SlotState::Committed(binding_inner);
        Ok(slot)
    }

    #[cfg(test)]
    pub(crate) fn reinstall_test_binding(
        self: &Arc<Self>,
        slot: usize,
        id: SurfaceId,
    ) -> Result<(), SurfaceAdmissionError> {
        let mut inner = self.inner.lock();
        if slot >= TOTAL_SURFACE_CREDITS {
            return Err(SurfaceAdmissionError::Capacity);
        }
        let binding_inner = Arc::new(SurfaceBindingInner {
            id,
            slot_index: slot,
            registry: self.clone(),
            native_ptr: AtomicU64::new(0),
            state: Mutex::new(BindingPhase::Active { lease_holder: None }),
            handles: Mutex::new(NativeHandles::empty()),
            cleanup_ticket: Mutex::new(None),
            retirement_waker: Mutex::new(None),
            capacity_wake: Mutex::new(None),
            pending_dup_slots: Mutex::new([None; TOTAL_SURFACE_CREDITS]),
            handles_retained: AtomicBool::new(false),
        });
        inner.slots[slot] = SlotState::Committed(binding_inner);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn binding_at_slot(self: &Arc<Self>, slot: usize) -> Option<Arc<SurfaceBinding>> {
        let inner = self.inner.lock();
        match &inner.slots[slot] {
            SlotState::Committed(b) | SlotState::CleanupArmed { binding: b, .. } => {
                Some(Arc::new(SurfaceBinding(b.clone())))
            }
            _ => None,
        }
    }

    #[cfg(target_os = "android")]
    pub fn reserve_java(
        self: &Arc<Self>,
        env: &mut JNIEnv<'_>,
        surface: &JObject<'_>,
    ) -> Result<JavaSurfaceReservation, SurfaceAdmissionError> {
        if surface.as_raw().is_null() {
            return Err(SurfaceAdmissionError::InvalidSurface);
        }

        // Under the registry lock: identity scan + credit reservation are one critical section.
        // GlobalRef allocation happens after unlock; we revalidate before publish.
        let provisional_id = self.next_id();
        let slot_index = {
            let mut inner = self.inner.lock();

            for slot in inner.slots.iter() {
                match slot {
                    SlotState::Committed(binding) | SlotState::CleanupArmed { binding, .. } => {
                        let same = {
                            let handles = binding.handles.lock();
                            if let Some(existing_ref) = handles.global_ref.as_ref() {
                                env.is_same_object(surface, existing_ref.as_obj())
                                    .map_err(|e| SurfaceAdmissionError::Jni(e.to_string()))?
                            } else {
                                false
                            }
                        };
                        if same {
                            let phase = binding.state.lock().clone();
                            return match phase {
                                BindingPhase::Active { .. } => Ok(JavaSurfaceReservation {
                                    id: binding.id,
                                    slot_index: binding.slot_index,
                                    reuse_binding: Some(Arc::new(SurfaceBinding(binding.clone()))),
                                    committed: false,
                                    registry: self.clone(),
                                    owns_credit: false,
                                }),
                                BindingPhase::Retiring { .. }
                                | BindingPhase::Retired
                                | BindingPhase::Quarantined(_) => {
                                    let ret = Arc::new(SurfaceBinding(binding.clone())).retire();
                                    Err(SurfaceAdmissionError::Retiring(ret))
                                }
                            };
                        }
                    }
                    SlotState::Reserved {
                        global_ref: Some(gr),
                        id,
                        ..
                    } => {
                        let same = env
                            .is_same_object(surface, gr.as_obj())
                            .map_err(|e| SurfaceAdmissionError::Jni(e.to_string()))?;
                        if same {
                            // Another reservation already owns this identity.
                            let _ = id;
                            return Err(SurfaceAdmissionError::Capacity);
                        }
                    }
                    _ => {}
                }
            }

            inner.alloc_free_locked(provisional_id)?
        };

        let global_ref = match env.new_global_ref(surface) {
            Ok(gr) => gr,
            Err(e) => {
                let mut inner = self.inner.lock();
                if let SlotState::Reserved { id, .. } = &inner.slots[slot_index] {
                    if *id == provisional_id {
                        inner.slots[slot_index] = SlotState::Free;
                    }
                }
                return Err(SurfaceAdmissionError::Jni(e.to_string()));
            }
        };

        if self.java_vm.lock().is_none() {
            if let Ok(vm) = env.get_java_vm() {
                *self.java_vm.lock() = Some(Arc::new(vm));
            }
        }

        // Revalidate identity under lock before publishing the GlobalRef into the slot.
        {
            let mut inner = self.inner.lock();
            for index in 0..inner.slots.len() {
                if let SlotState::Committed(binding) | SlotState::CleanupArmed { binding, .. } = &inner.slots[index]
                {
                    let same = {
                        let handles = binding.handles.lock();
                        if let Some(existing_ref) = handles.global_ref.as_ref() {
                            env.is_same_object(global_ref.as_obj(), existing_ref.as_obj())
                                .unwrap_or(false)
                        } else {
                            false
                        }
                    };
                    if same {
                        let binding = Arc::clone(binding);
                        // Keep our credit charged until GlobalRef cleanup/reap on this slot.
                        if let SlotState::Reserved {
                            id,
                            global_ref: slot_gr,
                            cleanup_ticket,
                            ..
                        } = &mut inner.slots[slot_index]
                        {
                            if *id == provisional_id {
                                *slot_gr = Some(global_ref);
                                // Arm rollback cleanup without freeing credit.
                                drop(inner);
                                self.arm_reserved_rollback(slot_index, provisional_id);
                                let phase = binding.state.lock().clone();
                                return match phase {
                                    BindingPhase::Active { .. } => Ok(JavaSurfaceReservation {
                                        id: binding.id,
                                        slot_index: binding.slot_index,
                                        reuse_binding: Some(Arc::new(SurfaceBinding(
                                            binding,
                                        ))),
                                        committed: false,
                                        registry: self.clone(),
                                        owns_credit: false,
                                    }),
                                    _ => {
                                        let ret =
                                            Arc::new(SurfaceBinding(binding)).retire();
                                        Err(SurfaceAdmissionError::Retiring(ret))
                                    }
                                };
                            }
                        }
                    }
                }
            }

            if let SlotState::Reserved {
                id,
                global_ref: slot_gr,
                ..
            } = &mut inner.slots[slot_index]
            {
                if *id == provisional_id {
                    *slot_gr = Some(global_ref);
                }
            }
        }

        Ok(JavaSurfaceReservation {
            id: provisional_id,
            slot_index,
            reuse_binding: None,
            committed: false,
            registry: self.clone(),
            owns_credit: true,
        })
    }

    #[cfg(target_os = "android")]
    fn arm_reserved_rollback(self: &Arc<Self>, slot_index: usize, id: SurfaceId) {
        // Install Arc-owned payload in the charged slot BEFORE try_spawn so a
        // Capacity/Spawn reject cannot drop natives with the rejected closure.
        let mut inner = self.inner.lock();
        let payload = match &mut inner.slots[slot_index] {
                SlotState::Reserved {
                    id: sid,
                    global_ref,
                    pending_window,
                    rollback,
                    cleanup_ticket,
                } if *sid == id => {
                    if cleanup_ticket.is_some() {
                        return;
                    }
                    if let Some(existing) = rollback.as_ref() {
                        // Already armed; retry spawn with the same Arc if payload remains.
                        if existing.lock().is_some() {
                            Some(existing.clone())
                        } else {
                            None
                        }
                    } else if global_ref.is_none() && pending_window.is_none() {
                        None
                    } else {
                        let cell = Arc::new(Mutex::new(Some(ReservedRollback {
                            global_ref: global_ref.take(),
                            window: pending_window.take(),
                        })));
                        *rollback = Some(cell.clone());
                        Some(cell)
                    }
                }
                _ => None,
        };
        let Some(payload) = payload else {
            return;
        };

        let registry = self.clone();
        let payload_for_run = payload.clone();
        let wake = Arc::new(|| {});
        match try_spawn(
            move || {
                // Take from Arc storage inside the owner only.
                let mut held = payload_for_run
                    .lock()
                    .take()
                    .ok_or_else(|| "rollback payload already taken".to_string())?;
                if let Some(w) = held.window.take() {
                    drop(w);
                }
                if let Some(gr) = held.global_ref.take() {
                    let vm = registry.java_vm().ok_or_else(|| {
                        "JavaVM unavailable for rollback GlobalRef".to_string()
                    })?;
                    let mut _env = vm
                        .attach_current_thread()
                        .map_err(|e| format!("attach for rollback: {e:?}"))?;
                    drop(gr);
                }
                Ok(())
            },
            wake,
        ) {
            Ok(ticket) => {
                if let SlotState::Reserved { id: sid, cleanup_ticket, .. } =
                    &mut inner.slots[slot_index]
                {
                    if *sid == id {
                        *cleanup_ticket = Some(ticket.clone());
                    }
                }
                drop(inner);
                if ticket.poll().is_ready() {
                    self.poll_reserved_cleanups();
                    self.observe_completed_bindings();
                }
            }
            Err(OwnerError::Capacity) | Err(OwnerError::Spawn(_)) => {
                // Rejected closure drops only its Arc clone. Payload stays in the
                // charged slot's Arc for owner_capacity_changed retry.
            }
        }
    }

    #[cfg(target_os = "android")]
    pub fn reserve_native(
        self: &Arc<Self>,
    ) -> Result<NativeSurfaceReservation, SurfaceAdmissionError> {
        let id = self.next_id();
        let mut inner = self.inner.lock();
        let free_idx = inner.alloc_free_locked(id)?;
        Ok(NativeSurfaceReservation {
            id,
            slot_index: free_idx,
            committed: false,
            registry: self.clone(),
        })
    }

    /// Drain finished rollback tickets and free only after proven cleanup.
    fn poll_reserved_cleanups(&self) {
        let mut inner = self.inner.lock();
        for slot in &mut inner.slots {
            let free = match slot {
                SlotState::Reserved {
                    global_ref, pending_window, rollback, cleanup_ticket, ..
                } => {
                    matches!(cleanup_ticket.as_ref().map(OwnerTicket::poll), Some(Poll::Ready(Ok(()))))
                        && global_ref.is_none()
                        && pending_window.is_none()
                        && rollback.as_ref().is_none_or(|cell| {
                            cell.lock().as_ref().is_none_or(ReservedRollback::is_empty)
                        })
                }
                _ => false,
            };
            // A failed receipt stays charged and observable to the original binding.
            if free {
                *slot = SlotState::Free;
            }
        }
    }

    fn observe_completed_bindings(&self) {
        let bindings: [Option<Arc<SurfaceBindingInner>>; TOTAL_SURFACE_CREDITS] = {
            let inner = self.inner.lock();
            std::array::from_fn(|index| match &inner.slots[index] {
                SlotState::Committed(binding) | SlotState::CleanupArmed { binding, .. } => Some(Arc::clone(binding)),
                _ => None,
            })
        };
        for binding in bindings.into_iter().flatten() {
            let completion = binding.cleanup_ticket.lock().as_ref().map(OwnerTicket::poll);
            if let Some(Poll::Ready(result)) = completion {
                binding.finish_retirement(result);
            }
        }
    }
}

/// Parent-owned reaper calls this after publishing a completed owner ticket.
/// Bounded: visits at most the sixteen registration slots. No new threads.
pub(crate) fn owner_capacity_changed() {
    let registry = SurfaceRegistry::global();
    registry.poll_reserved_cleanups();

    // Snapshot at most 16 slots, release locks, then wake/spawn.
    let mut cleanup_targets: Vec<Arc<SurfaceBindingInner>> = Vec::new();
    let mut capacity_callbacks: Vec<Arc<dyn Fn() + Send + Sync>> = Vec::new();
    {
        let inner = registry.inner.lock();
        for slot in inner.slots.iter() {
            match slot {
                SlotState::Committed(binding) | SlotState::CleanupArmed { binding, .. } => {
                    let phase = binding.state.lock().clone();
                    if matches!(
                        phase,
                        BindingPhase::Retiring {
                            lease_holder: None
                        }
                    ) {
                        cleanup_targets.push(binding.clone());
                    }
                    if let Some(cb) = binding.capacity_wake.lock().clone() {
                        capacity_callbacks.push(cb);
                    }
                }
                SlotState::Reserved {
                    cleanup_ticket: None,
                    ..
                } => {
                    // May still hold live handles or an Arc rollback payload.
                }
                _ => {}
            }
        }
    }

    for binding in cleanup_targets {
        binding.ensure_cleanup_started();
        let completion = {
            let ticket = binding.cleanup_ticket.lock();
            ticket.as_ref().map(OwnerTicket::poll)
        };
        match completion {
            Some(Poll::Ready(result)) => binding.finish_retirement(result),
            Some(Poll::Pending) => {}
            None => binding.spawn_cleanup_from_armed_slot(),
        }
    }

    // Retry reserved rollbacks that still own natives without a ticket.
    {
        let mut to_arm = Vec::new();
        {
            let inner = registry.inner.lock();
            for (idx, slot) in inner.slots.iter().enumerate() {
                if let SlotState::Reserved {
                    id,
                    global_ref,
                    pending_window,
                    rollback,
                    cleanup_ticket: None,
                } = slot
                {
                    #[cfg(target_os = "android")]
                    {
                        let payload_pending = rollback
                            .as_ref()
                            .map(|c| c.lock().is_some())
                            .unwrap_or(false);
                        if global_ref.is_some()
                            || pending_window.is_some()
                            || payload_pending
                        {
                            to_arm.push((idx, *id));
                        }
                    }
                    #[cfg(not(target_os = "android"))]
                    {
                        let _ = (idx, id, global_ref, pending_window, rollback);
                    }
                }
            }
        }
        #[cfg(target_os = "android")]
        for (idx, id) in to_arm {
            registry.arm_reserved_rollback(idx, id);
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = to_arm;
        }
    }

    for cb in capacity_callbacks {
        cb();
    }
}

/// Linear reservation for a Java Surface. Dropping without committing keeps credit
/// charged until actual GlobalRef cleanup/reap.
#[cfg(target_os = "android")]
pub struct JavaSurfaceReservation {
    id: SurfaceId,
    slot_index: usize,
    reuse_binding: Option<Arc<SurfaceBinding>>,
    committed: bool,
    registry: Arc<SurfaceRegistry>,
    owns_credit: bool,
}

#[cfg(target_os = "android")]
impl JavaSurfaceReservation {
    pub fn id(&self) -> SurfaceId {
        self.id
    }

    pub fn commit(mut self) -> Arc<SurfaceBinding> {
        self.committed = true;
        if let Some(binding) = self.reuse_binding.take() {
            return binding;
        }

        let mut inner = self.registry.inner.lock();
        let global_ref = match &mut inner.slots[self.slot_index] {
            SlotState::Reserved {
                id,
                global_ref,
                ..
            } if *id == self.id => global_ref.take(),
            _ => None,
        };

        let binding_inner = Arc::new(SurfaceBindingInner {
            id: self.id,
            slot_index: self.slot_index,
            registry: self.registry.clone(),
            native_ptr: AtomicU64::new(0),
            state: Mutex::new(BindingPhase::Active { lease_holder: None }),
            handles: Mutex::new(NativeHandles {
                global_ref,
                windows: Vec::new(),
                retained_marker: false,
            }),
            cleanup_ticket: Mutex::new(None),
            retirement_waker: Mutex::new(None),
            capacity_wake: Mutex::new(None),
            pending_dup_slots: Mutex::new([None; TOTAL_SURFACE_CREDITS]),
            handles_retained: AtomicBool::new(false),
        });

        inner.slots[self.slot_index] = SlotState::Committed(binding_inner.clone());
        Arc::new(SurfaceBinding(binding_inner))
    }
}

#[cfg(target_os = "android")]
impl Drop for JavaSurfaceReservation {
    fn drop(&mut self) {
        if self.committed || !self.owns_credit || self.reuse_binding.is_some() {
            return;
        }
        // Keep credit charged until cleanup/reap of any GlobalRef.
        self.registry
            .arm_reserved_rollback(self.slot_index, self.id);
        // Free only when nothing remains charged (no handles, no Arc payload, no ticket).
        let mut inner = self.registry.inner.lock();
        if let SlotState::Reserved {
            id,
            global_ref: None,
            pending_window: None,
            rollback: None,
            cleanup_ticket: None,
        } = &inner.slots[self.slot_index]
        {
            if *id == self.id {
                inner.slots[self.slot_index] = SlotState::Free;
            }
        }
    }
}

/// Linear reservation for a NativeWindow.
#[cfg(target_os = "android")]
pub struct NativeSurfaceReservation {
    id: SurfaceId,
    slot_index: usize,
    committed: bool,
    registry: Arc<SurfaceRegistry>,
}

#[cfg(target_os = "android")]
impl std::fmt::Debug for NativeSurfaceReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSurfaceReservation")
            .field("id", &self.id)
            .field("slot_index", &self.slot_index)
            .field("committed", &self.committed)
            .finish()
    }
}



#[cfg(target_os = "android")]
impl std::fmt::Debug for JavaSurfaceReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JavaSurfaceReservation")
            .field("id", &self.id)
            .field("slot_index", &self.slot_index)
            .field("committed", &self.committed)
            .field("owns_credit", &self.owns_credit)
            .finish()
    }
}

#[cfg(target_os = "android")]
impl NativeSurfaceReservation {
    pub fn id(&self) -> SurfaceId {
        self.id
    }

    pub fn commit(mut self, window: NativeWindow) -> Arc<SurfaceBinding> {
        self.committed = true;
        let ptr = window.ptr().as_ptr() as u64;

        // Identity match under registry lock; never mint a second active lease for same ptr.
        let mut inner = self.registry.inner.lock();
        for index in 0..inner.slots.len() {
            match &inner.slots[index] {
                SlotState::Committed(existing) | SlotState::CleanupArmed { binding: existing, .. } => {
                    if existing.native_ptr.load(Ordering::Acquire) == ptr && ptr != 0 {
                        let existing = Arc::clone(existing);
                        if let SlotState::Reserved { id, pending_window, .. } =
                            &mut inner.slots[self.slot_index]
                        {
                            if *id == self.id {
                                *pending_window = Some(window);
                            }
                        }
                        // Publish the sibling's identity before its cleanup can finish.
                        // Reuse of a slot cannot be mistaken for this window's receipt.
                        existing.pending_dup_slots.lock()[self.slot_index] = Some(self.id);
                        drop(inner);
                        self.registry.arm_reserved_rollback(self.slot_index, self.id);
                        return Arc::new(SurfaceBinding(existing));
                    }
                }
                _ => {}
            }
        }

        let binding_inner = Arc::new(SurfaceBindingInner {
            id: self.id,
            slot_index: self.slot_index,
            registry: self.registry.clone(),
            native_ptr: AtomicU64::new(ptr),
            state: Mutex::new(BindingPhase::Active { lease_holder: None }),
            handles: Mutex::new(NativeHandles {
                global_ref: None,
                windows: vec![window],
                retained_marker: false,
            }),
            cleanup_ticket: Mutex::new(None),
            retirement_waker: Mutex::new(None),
            capacity_wake: Mutex::new(None),
            pending_dup_slots: Mutex::new([None; TOTAL_SURFACE_CREDITS]),
            handles_retained: AtomicBool::new(false),
        });

        inner.slots[self.slot_index] = SlotState::Committed(binding_inner.clone());
        Arc::new(SurfaceBinding(binding_inner))
    }
}

#[cfg(target_os = "android")]
impl Drop for NativeSurfaceReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut inner = self.registry.inner.lock();
        if let SlotState::Reserved {
            id,
            global_ref: None,
            pending_window: None,
            rollback: None,
            cleanup_ticket: None,
        } = &inner.slots[self.slot_index]
        {
            if *id == self.id {
                inner.slots[self.slot_index] = SlotState::Free;
            }
        }
    }
}

