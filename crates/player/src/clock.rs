use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::backend::Clock;

/// Monotonic timestamp in nanoseconds since an arbitrary epoch.
pub fn current_monotonic_ns() -> i64 {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: ts is a valid pointer to stack-allocated memory.
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        }
        (ts.tv_sec as i64) * 1_000_000_000 + (ts.tv_nsec as i64)
    }
    #[cfg(not(unix))]
    {
        static START: std::sync::LazyLock<std::time::Instant> =
            std::sync::LazyLock::new(std::time::Instant::now);
        std::time::Instant::now().duration_since(*START).as_nanos() as i64
    }
}

#[derive(Debug)]
struct ClockInner {
    base_media_time: Duration,
    start_mono_ns: Option<i64>,
    playing: bool,
    started: bool,
}

/// A free-running monotonic clock used when there is no audio output.
#[derive(Clone, Debug)]
pub struct FreeRunningClock {
    inner: Arc<Mutex<ClockInner>>,
}

impl Default for FreeRunningClock {
    fn default() -> Self {
        Self::new()
    }
}

impl FreeRunningClock {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ClockInner {
                base_media_time: Duration::ZERO,
                start_mono_ns: None,
                playing: false,
                started: false,
            })),
        }
    }

    pub fn play(&self) {
        let mut inner = self.inner.lock();
        if !inner.playing {
            inner.playing = true;
            inner.started = true;
            inner.start_mono_ns = Some(current_monotonic_ns());
        }
    }

    pub fn pause(&self) {
        let mut inner = self.inner.lock();
        if inner.playing {
            if let Some(start_ns) = inner.start_mono_ns {
                let now_ns = current_monotonic_ns();
                let elapsed_ns = (now_ns - start_ns).max(0) as u64;
                inner.base_media_time += Duration::from_nanos(elapsed_ns);
            }
            inner.playing = false;
            inner.start_mono_ns = None;
        }
    }

    pub fn seek(&self, to: Duration) {
        let mut inner = self.inner.lock();
        inner.base_media_time = to;
        if inner.playing {
            inner.start_mono_ns = Some(current_monotonic_ns());
        }
    }

    pub fn set_position(&self, to: Duration) {
        let mut inner = self.inner.lock();
        inner.base_media_time = to;
        inner.started = true;
        if inner.playing {
            inner.start_mono_ns = Some(current_monotonic_ns());
        }
    }
}

impl Clock for FreeRunningClock {
    fn now(&self) -> Option<Duration> {
        let inner = self.inner.lock();
        if !inner.started {
            return None;
        }
        if inner.playing {
            if let Some(start_ns) = inner.start_mono_ns {
                let now_ns = current_monotonic_ns();
                let elapsed_ns = (now_ns - start_ns).max(0) as u64;
                Some(inner.base_media_time + Duration::from_nanos(elapsed_ns))
            } else {
                Some(inner.base_media_time)
            }
        } else {
            Some(inner.base_media_time)
        }
    }

    fn monotonic_ns_at(&self, at: Duration) -> Option<i64> {
        let inner = self.inner.lock();
        let start_ns = inner.start_mono_ns?;
        let base_ns = inner.base_media_time.as_nanos() as i64;
        let at_ns = at.as_nanos() as i64;
        Some(start_ns + (at_ns - base_ns))
    }
}

/// The clock every sink of one playback follows. While audio plays it is
/// the audio output's clock, the media time of the sample being heard;
/// otherwise (no audio, and from a seek until the audio plays from its
/// target) a free-running clock. It stands still the moment the transport
/// stops (pause, buffering hold), even while the audio output takes a few
/// milliseconds more to stop. The free clock picks up where the audio clock
/// left off, so the position does not jump when the audio ends, fails or is
/// switched.
pub(crate) struct MasterClock {
    free: FreeRunningClock,
    lead: Mutex<Lead>,
}

struct Lead {
    /// The audio output's clock, while it leads.
    audio: Option<Arc<dyn Clock>>,
    /// The newest seek generation: only audio played from it may lead.
    generation: u64,
    /// Where the clock stands while the transport is stopped.
    held: Option<Duration>,
}

impl Lead {
    fn now(&self, free: &FreeRunningClock) -> Option<Duration> {
        self.audio.as_ref().and_then(|audio| audio.now()).or_else(|| free.now())
    }
}

impl MasterClock {
    /// A clock at zero, stopped.
    pub(crate) fn new() -> Self {
        let free = FreeRunningClock::new();
        free.set_position(Duration::ZERO);
        Self {
            free,
            lead: Mutex::new(Lead {
                audio: None,
                generation: 0,
                held: Some(Duration::ZERO),
            }),
        }
    }

    /// The transport started or stopped the clock.
    pub(crate) fn set_running(&self, running: bool) {
        let mut lead = self.lead.lock();
        if running {
            lead.held = None;
            self.free.play();
        } else {
            if lead.held.is_none() {
                lead.held = lead.now(&self.free);
            }
            self.free.pause();
            if let Some(held) = lead.held {
                // A suspended device may lose its timestamp before resume.
                // Keep the fallback at the same held presentation position.
                self.free.set_position(held);
            }
        }
    }

    /// A seek to `to` (seek generation `generation`): the clock jumps there,
    /// and the free clock leads until the audio plays from it.
    pub(crate) fn seek(&self, to: Duration, generation: u64) {
        let mut lead = self.lead.lock();
        if generation < lead.generation {
            // A newer seek got here first.
            return;
        }
        lead.generation = generation;
        lead.audio = None;
        if lead.held.is_some() {
            lead.held = Some(to);
        }
        self.free.set_position(to);
    }

    /// The audio output took its first samples of seek generation
    /// `generation`: from now on its clock leads.
    pub(crate) fn follow_audio(&self, clock: Arc<dyn Clock>, generation: u64) {
        let mut lead = self.lead.lock();
        if lead.generation == generation {
            lead.audio = Some(clock);
        }
    }

    /// The audio stopped leading (its pipeline ended, failed or was
    /// replaced): the free clock carries on from where the audio was.
    pub(crate) fn release_audio(&self) {
        let mut lead = self.lead.lock();
        if let Some(now) = lead.audio.take().and_then(|audio| audio.now()) {
            self.free.set_position(now);
        }
    }
}

impl Clock for MasterClock {
    fn now(&self) -> Option<Duration> {
        let lead = self.lead.lock();
        lead.held.or_else(|| lead.now(&self.free))
    }

    fn monotonic_ns_at(&self, at: Duration) -> Option<i64> {
        let lead = self.lead.lock();
        if lead.held.is_some() {
            return None;
        }
        match &lead.audio {
            Some(audio) => audio.monotonic_ns_at(at),
            None => self.free.monotonic_ns_at(at),
        }
    }
}

/// macOS can coalesce ordinary timed waits well beyond an audio buffer.
/// Elevate only paced waits/device writes, never bulk codec decoding.
#[cfg(target_os = "macos")]
#[allow(deprecated)] // The existing libc dependency binds these Mach calls.
pub(crate) mod timing {
    use std::marker::PhantomData;
    use std::rc::Rc;
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    static POLICY: LazyLock<Result<libc::thread_time_constraint_policy, i32>> = LazyLock::new(|| {
        let mut scale = libc::mach_timebase_info { numer: 0, denom: 0 };
        let result = unsafe { libc::mach_timebase_info(&mut scale) };
        if result != libc::KERN_SUCCESS { return Err(result); }
        if scale.numer == 0 { return Err(libc::KERN_INVALID_ARGUMENT); }
        let period = u32::try_from(20_000_000u64 * u64::from(scale.denom) / u64::from(scale.numer))
            .map_err(|_| libc::KERN_INVALID_ARGUMENT)?;
        let computation = period / 20;
        if computation == 0 { return Err(libc::KERN_INVALID_ARGUMENT); }
        // Mach raises computation to constraint/2. A 2 ms constraint keeps
        // the effective budget at 1 ms per 20 ms, not 10 ms per 20 ms.
        Ok(libc::thread_time_constraint_policy {
            period, computation, constraint: computation * 2, preemptible: 1,
        })
    });
    static REPORTED_FAILURE: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" {
        fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> libc::kern_return_t;
    }

    struct Port(libc::mach_port_t);
    impl Drop for Port {
        fn drop(&mut self) {
            unsafe { mach_port_deallocate(libc::mach_task_self(), self.0); }
        }
    }

    /// Called once, only at the entry of an engine-owned realtime worker.
    /// Mach real-time policy irreversibly opts a pthread out of QoS. Make
    /// that lifetime decision before any work, then restore ordinary Mach
    /// scheduling between paced regions. Never call this on a borrowed
    /// application/main/dispatch thread.
    pub(crate) fn initialize_worker() -> Result<(), i32> {
        let thread = unsafe { libc::pthread_self() };
        let mut policy = 0;
        // sched_param contains an integer and opaque bytes, all valid at zero.
        let mut params: libc::sched_param = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::pthread_getschedparam(thread, &mut policy, &mut params) };
        if result != 0 { return Err(result); }
        let result = unsafe { libc::pthread_setschedparam(thread, policy, &params) };
        if result != 0 { return Err(result); }
        Ok(())
    }

    #[derive(Debug)]
    struct Saved {
        timeshare: libc::boolean_t,
        importance: libc::integer_t,
        qos: libc::qos_class_t,
        relative: libc::c_int,
    }

    impl Saved {
        fn capture(thread: libc::mach_port_t) -> Result<Self, i32> {
            let mut saved = Self {
                timeshare: 0, importance: 0,
                qos: libc::qos_class_t::QOS_CLASS_UNSPECIFIED, relative: 0,
            };
            let mut count = libc::THREAD_EXTENDED_POLICY_COUNT;
            let mut default = 0;
            let result = unsafe { libc::thread_policy_get(
                thread, libc::THREAD_EXTENDED_POLICY as u32,
                std::ptr::from_mut(&mut saved.timeshare).cast(), &mut count, &mut default,
            ) };
            if result != libc::KERN_SUCCESS { return Err(result); }
            count = libc::THREAD_PRECEDENCE_POLICY_COUNT;
            default = 0;
            let result = unsafe { libc::thread_policy_get(
                thread, libc::THREAD_PRECEDENCE_POLICY as u32,
                &mut saved.importance, &mut count, &mut default,
            ) };
            if result != libc::KERN_SUCCESS { return Err(result); }
            let result = unsafe { libc::pthread_get_qos_class_np(
                libc::pthread_self(), &mut saved.qos, &mut saved.relative,
            ) };
            if result != 0 { return Err(result); }
            Ok(saved)
        }
    }

    fn constraint(thread: libc::mach_port_t) -> Result<(libc::thread_time_constraint_policy, bool), i32> {
        let mut policy = libc::thread_time_constraint_policy {
            period: 0, computation: 0, constraint: 0, preemptible: 0,
        };
        let mut count = libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT;
        let mut default = 0;
        let result = unsafe { libc::thread_policy_get(
            thread, libc::THREAD_TIME_CONSTRAINT_POLICY as u32,
            std::ptr::from_mut(&mut policy).cast(), &mut count, &mut default,
        ) };
        if result != libc::KERN_SUCCESS { return Err(result); }
        Ok((policy, default != 0))
    }

    pub(crate) struct Guard {
        thread: Port,
        saved: Saved,
        // The timed region and its restoration belong to one worker.
        _same_thread: PhantomData<Rc<()>>,
    }

    impl Guard {
        pub(crate) fn enter() -> Option<Self> {
            match Self::try_enter() {
                Ok(guard) => guard,
                Err(error) => {
                    if !REPORTED_FAILURE.swap(true, Ordering::Relaxed) {
                        eprintln!("macOS playback scheduling unavailable ({error}); using normal scheduling");
                    }
                    None
                }
            }
        }

        fn try_enter() -> Result<Option<Self>, i32> {
            let thread = Port(unsafe { libc::mach_thread_self() });
            if !constraint(thread.0)?.1 {
                // Already real-time (including a nested guard): leave the
                // caller's policy and budget completely unchanged.
                return Ok(None);
            }
            let saved = Saved::capture(thread.0)?;
            // A borrowed QoS-managed thread cannot be restored after Mach
            // policy changes. Only initialized workers may enter.
            if saved.qos as u32 != 0 { return Err(libc::EPERM); }
            let mut policy = (*POLICY)?;
            let result = unsafe { libc::thread_policy_set(
                thread.0, libc::THREAD_TIME_CONSTRAINT_POLICY as u32,
                std::ptr::from_mut(&mut policy).cast(), libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
            ) };
            if result != libc::KERN_SUCCESS { return Err(result); }
            Ok(Some(Self { thread, saved, _same_thread: PhantomData }))
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let mode = unsafe { libc::thread_policy_set(
                self.thread.0, libc::THREAD_EXTENDED_POLICY as u32,
                std::ptr::from_mut(&mut self.saved.timeshare).cast(), libc::THREAD_EXTENDED_POLICY_COUNT,
            ) };
            let precedence = unsafe { libc::thread_policy_set(
                self.thread.0, libc::THREAD_PRECEDENCE_POLICY as u32,
                &mut self.saved.importance, libc::THREAD_PRECEDENCE_POLICY_COUNT,
            ) };
            if mode != libc::KERN_SUCCESS || precedence != libc::KERN_SUCCESS {
                eprintln!("macOS playback scheduling restoration failed: mode={mode} precedence={precedence}");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // Risks: incorrect Mach units/unbounded budget, losing a caller's
        // scheduling mode or QoS, and leaking real-time policy on unwind.
        #[test]
        fn bounded_policy_restores_after_unwind() {
            std::thread::spawn(|| {
                initialize_worker().unwrap();
                let thread = Port(unsafe { libc::mach_thread_self() });
                let before = Saved::capture(thread.0).unwrap();
                let guard = Guard::enter().expect("Mach playback scheduling unavailable");
                let (active, is_default) = constraint(thread.0).unwrap();
                assert!(!is_default, "real-time policy was not applied");
                assert!(active.computation > 0 && active.computation <= active.period / 20);
                assert!(active.constraint <= active.computation * 2);
                assert!(Guard::enter().is_none(), "must not replace a caller's real-time policy");
                let result = std::panic::catch_unwind(move || {
                    let _guard = guard;
                    panic!("exercise unwind restoration");
                });
                assert!(result.is_err());
                let after = Saved::capture(thread.0).unwrap();
                assert_eq!(after.timeshare, before.timeshare);
                assert_eq!(after.importance, before.importance);
                assert_eq!(after.qos as u32, before.qos as u32);
                assert_eq!(after.relative, before.relative);
                assert!(constraint(thread.0).unwrap().1, "real-time scheduling leaked");
            }).join().unwrap();
        }

        #[test]
        fn refuses_to_opt_out_a_borrowed_qos_thread() {
            assert_eq!(unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_DEFAULT, 0)
            }, 0);
            let thread = Port(unsafe { libc::mach_thread_self() });
            let before = Saved::capture(thread.0).unwrap();
            assert!(matches!(Guard::try_enter(), Err(libc::EPERM)));
            let after = Saved::capture(thread.0).unwrap();
            assert_eq!(after.timeshare, before.timeshare);
            assert_eq!(after.importance, before.importance);
            assert_eq!(after.qos as u32, before.qos as u32);
            assert_eq!(after.relative, before.relative);
            assert!(constraint(thread.0).unwrap().1);
        }
    }
}

#[cfg(test)]
mod master_tests {
    use super::*;

    // A device can lose its timestamp while suspended. The paused position
    // must survive that loss, resume, and the eventual audio hand-back.
    #[test]
    fn resume_without_a_device_timestamp_preserves_the_held_position() {
        struct DeviceClock(Mutex<Option<Duration>>);
        impl Clock for DeviceClock {
            fn now(&self) -> Option<Duration> { *self.0.lock() }
            fn monotonic_ns_at(&self, _: Duration) -> Option<i64> { None }
        }
        let master = MasterClock::new();
        let position = Duration::from_secs(42);
        let device = Arc::new(DeviceClock(Mutex::new(Some(position))));
        master.follow_audio(device.clone(), 0);
        master.set_running(true);
        master.set_running(false);
        *device.0.lock() = None;
        assert_eq!(master.now(), Some(position));
        master.set_running(true);
        let resumed = master.now().unwrap();
        assert!(resumed >= position && resumed < position + Duration::from_secs(1),
            "fallback jumped from {position:?} to {resumed:?}");
        master.release_audio();
        let released = master.now().unwrap();
        assert!(released >= resumed && released < position + Duration::from_secs(1));
    }
}
