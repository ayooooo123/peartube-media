//! Four native owners, with retirement observed only after actual thread exit.
//! Native values must be created inside the submitted body. A failed body or
//! uncertain teardown quarantines capacity rather than admitting replacements.

use parking_lot::{Condvar, Mutex};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, OnceLock};
use std::task::Poll;
use std::thread::JoinHandle;

const CAPACITY: usize = 4;
type Wake = Arc<dyn Fn() + Send + Sync>;
type Outcome = Result<(), String>;

#[derive(Debug)]
pub(crate) enum OwnerError {
    Capacity,
    Spawn(String),
}

#[derive(Clone)]
pub(crate) struct OwnerTicket(Arc<Ticket>);

struct Ticket {
    result: OnceLock<Outcome>,
    wake: Wake,
}

impl OwnerTicket {
    pub(crate) fn poll(&self) -> Poll<Outcome> {
        match self.0.result.get() {
            Some(result) => Poll::Ready(result.clone()),
            None => Poll::Pending,
        }
    }
}

enum Slot {
    Free,
    Reserved,
    Running { thread: JoinHandle<()>, ticket: Arc<Ticket> },
    Reaping,
    Quarantined,
}

struct State {
    slots: [Slot; CAPACITY],
    completed: [Option<Outcome>; CAPACITY],
    poisoned: [bool; CAPACITY],
    closing: bool,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

struct Pool(Arc<Inner>);

impl Pool {
    fn new() -> Result<Self, String> {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                slots: std::array::from_fn(|_| Slot::Free),
                completed: std::array::from_fn(|_| None),
                poisoned: [false; CAPACITY],
                closing: false,
            }),
            changed: Condvar::new(),
        });
        // One fixed joiner per slot prevents a stuck thread-local destructor
        // from withholding the retirement receipts of the other owners.
        for index in 0..CAPACITY {
            let reaper = inner.clone();
            if let Err(error) = std::thread::Builder::new()
                .name(format!("video-reaper-{index}"))
                .spawn(move || reap(reaper, index))
            {
                inner.state.lock().closing = true;
                inner.changed.notify_all();
                return Err(format!("starting video reaper: {error}"));
            }
        }
        Ok(Self(inner))
    }

    fn spawn<F>(&self, run: F, wake: Wake) -> Result<OwnerTicket, OwnerError>
    where
        F: FnOnce() -> Outcome + Send + 'static,
    {
        let index = {
            let mut state = self.0.state.lock();
            let Some(index) = state.slots.iter().position(|slot| matches!(slot, Slot::Free))
                .filter(|_| !state.closing)
            else { return Err(OwnerError::Capacity) };
            state.slots[index] = Slot::Reserved;
            index
        };
        let ticket = Arc::new(Ticket { result: OnceLock::new(), wake });
        let inner = self.0.clone();
        let thread = std::thread::Builder::new()
            .name(format!("video-owner-{index}"))
            .spawn(move || {
                let outcome = match catch_unwind(AssertUnwindSafe(run)) {
                    Ok(outcome) => outcome,
                    Err(payload) => {
                        // Arbitrary panic payloads can themselves panic in Drop.
                        // This owner permanently quarantines its capacity.
                        std::mem::forget(payload);
                        Err("native video owner panicked".into())
                    }
                };
                inner.state.lock().completed[index] = Some(outcome);
                inner.changed.notify_all();
                // This is only body completion. Thread-local teardown may
                // still run; the reaper publishes no receipt until join ends.
            });
        match thread {
            Ok(thread) => {
                self.0.state.lock().slots[index] = Slot::Running { thread, ticket: ticket.clone() };
                self.0.changed.notify_all();
                Ok(OwnerTicket(ticket))
            }
            Err(error) => {
                let mut state = self.0.state.lock();
                state.slots[index] = if state.poisoned[index] { Slot::Quarantined } else { Slot::Free };
                drop(state);
                self.0.changed.notify_all();
                Err(OwnerError::Spawn(error.to_string()))
            }
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.0.state.lock().closing = true;
        self.0.changed.notify_all();
    }
}

fn reap(inner: Arc<Inner>, index: usize) {
    loop {
        let (thread, ticket, outcome) = {
            let mut state = inner.state.lock();
            loop {
                if state.completed[index].is_some() && matches!(state.slots[index], Slot::Running { .. }) {
                    let Slot::Running { thread, ticket } =
                        std::mem::replace(&mut state.slots[index], Slot::Reaping)
                    else { unreachable!() };
                    break (thread, ticket, state.completed[index].take().unwrap());
                }
                if state.closing && matches!(state.slots[index], Slot::Free | Slot::Quarantined) {
                    return;
                }
                inner.changed.wait(&mut state);
            }
        };
        // No frontend/shared lock is held during this potentially unbounded
        // join. The occupied slot remains unavailable throughout it.
        let outcome = match thread.join() {
            Ok(()) => outcome,
            Err(payload) => {
                std::mem::forget(payload);
                Err("native video owner panicked during thread teardown".into())
            }
        };
        {
            let mut state = inner.state.lock();
            state.slots[index] = if outcome.is_ok() && !state.poisoned[index] {
                Slot::Free
            } else {
                Slot::Quarantined
            };
            ticket.result.set(outcome).expect("owner ticket completed twice");
        }
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
            (ticket.wake)();
            // Android's fixed registration slots also own capacity-pending
            // cleanup and subtitle work. Notify only after reap/publication,
            // outside pool locks, under the same callback unwind guard.
            #[cfg(target_os = "android")]
            crate::android::surface::owner_capacity_changed();
        })) {
            // Preserve the reaper even when a consumer wake unwinds. A new
            // owner may already occupy this slot; reap it normally, but never
            // admit another. Thus retained panic payloads remain bounded by
            // the fixed slots and at most one already-admitted replacement.
            std::mem::forget(payload);
            let mut state = inner.state.lock();
            state.poisoned[index] = true;
            if matches!(state.slots[index], Slot::Free) {
                state.slots[index] = Slot::Quarantined;
            }
            drop(state);
            inner.changed.notify_all();
        }
    }
}

pub(crate) fn try_spawn<F>(run: F, wake: Wake) -> Result<OwnerTicket, OwnerError>
where
    F: FnOnce() -> Outcome + Send + 'static,
{
    static POOL: OnceLock<Result<Pool, String>> = OnceLock::new();
    match POOL.get_or_init(Pool::new) {
        Ok(pool) => pool.spawn(run, wake),
        Err(error) => Err(OwnerError::Spawn(error.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::{Condvar, Mutex};
    use std::cell::RefCell;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct Gate(Mutex<bool>, Condvar);
    impl Gate {
        fn wait(&self) {
            let mut open = self.0.lock();
            while !*open { self.1.wait(&mut open); }
        }
        fn open(&self) { *self.0.lock() = true; self.1.notify_all(); }
    }
    struct Release(Arc<Gate>);
    impl Drop for Release { fn drop(&mut self) { self.0.open(); } }

    fn done(ticket: &OwnerTicket) -> Result<(), String> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Poll::Ready(result) = ticket.poll() { return result; }
            assert!(Instant::now() < until, "owner did not retire");
            std::thread::yield_now();
        }
    }
    fn wake() -> Arc<dyn Fn() + Send + Sync> { Arc::new(|| {}) }

    #[test]
    fn occupied_slots_reject_work_without_running_it() {
        let pool = Pool::new().unwrap();
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let mut tickets = Vec::new();
        for _ in 0..4 {
            let gate = gate.clone();
            tickets.push(pool.spawn(move || { gate.wait(); Ok(()) }, wake()).unwrap());
        }
        assert!(matches!(pool.spawn(|| panic!("unadmitted work ran"), wake()), Err(OwnerError::Capacity)));
        assert!(tickets.iter().all(|ticket| ticket.poll().is_pending()));
        gate.open();
        for ticket in tickets { done(&ticket).unwrap(); }
        done(&pool.spawn(|| Ok(()), wake()).unwrap()).unwrap();
    }

    struct ExitGate { gate: Arc<Gate>, entered: mpsc::Sender<()> }
    impl Drop for ExitGate {
        fn drop(&mut self) { self.entered.send(()).unwrap(); self.gate.wait(); }
    }
    thread_local! { static EXIT_GATE: RefCell<Option<ExitGate>> = const { RefCell::new(None) }; }

    #[test]
    fn successful_body_does_not_release_capacity_before_thread_exit() {
        let pool = Pool::new().unwrap();
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let (tx, rx) = mpsc::channel();
        let exit_gate = gate.clone();
        let retiring = pool.spawn(move || {
            EXIT_GATE.with(|slot| *slot.borrow_mut() = Some(ExitGate { gate: exit_gate, entered: tx }));
            Ok(())
        }, wake()).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let other_gate = Arc::new(Gate::default());
        let _release_other = Release(other_gate.clone());
        let mut other = Vec::new();
        for _ in 0..3 {
            let gate = other_gate.clone();
            other.push(pool.spawn(move || { gate.wait(); Ok(()) }, wake()).unwrap());
        }
        assert!(retiring.poll().is_pending(), "body completion is not retirement");
        assert!(matches!(pool.spawn(|| Ok(()), wake()), Err(OwnerError::Capacity)));
        other_gate.open();
        for ticket in other { done(&ticket).unwrap(); }
        assert!(retiring.poll().is_pending(), "unrelated receipts must not release the blocked owner");
        done(&pool.spawn(|| Ok(()), wake()).unwrap()).unwrap();
        gate.open();
        done(&retiring).unwrap();
    }

    #[test]
    fn failed_cleanup_and_panics_quarantine_their_slots() {
        let pool = Pool::new().unwrap();
        let failed = pool.spawn(|| Err("delete not proved".into()), wake()).unwrap();
        assert_eq!(done(&failed), Err("delete not proved".into()));
        let panicked = pool.spawn(|| panic!("owner failed"), wake()).unwrap();
        assert!(done(&panicked).unwrap_err().contains("panicked"));
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let mut live = Vec::new();
        for _ in 0..2 {
            let gate = gate.clone();
            live.push(pool.spawn(move || { gate.wait(); Ok(()) }, wake()).unwrap());
        }
        assert!(matches!(pool.spawn(|| Ok(()), wake()), Err(OwnerError::Capacity)));
        gate.open();
        for ticket in live { done(&ticket).unwrap(); }
        assert!(failed.poll().is_ready());
        assert!(panicked.poll().is_ready());
    }

    #[test]
    fn panicking_wake_does_not_strand_a_replacement_already_admitted() {
        let pool = Pool::new().unwrap();
        let callback_gate = Arc::new(Gate::default());
        let _release_callback = Release(callback_gate.clone());
        let (entered, arrived) = mpsc::channel();
        let first = pool.spawn(|| Ok(()), Arc::new(move || {
            entered.send(()).unwrap();
            callback_gate.wait();
            panic!("wake failed");
        })).unwrap();
        arrived.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first.poll(), Poll::Ready(Ok(())));
        let work_gate = Arc::new(Gate::default());
        let _release_work = Release(work_gate.clone());
        let mut replacements = Vec::new();
        for _ in 0..CAPACITY {
            let gate = work_gate.clone();
            replacements.push(pool.spawn(move || { gate.wait(); Ok(()) }, wake()).unwrap());
        }
        _release_callback.0.open();
        work_gate.open();
        for ticket in replacements { done(&ticket).unwrap(); }

        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let mut usable = Vec::new();
        for _ in 0..CAPACITY - 1 {
            let gate = gate.clone();
            usable.push(pool.spawn(move || { gate.wait(); Ok(()) }, wake()).unwrap());
        }
        assert!(matches!(pool.spawn(|| Ok(()), wake()), Err(OwnerError::Capacity)));
        gate.open();
        for ticket in usable { done(&ticket).unwrap(); }
    }

    #[test]
    fn panic_payload_destructor_cannot_prevent_failure_receipt() {
        struct BadPayload;
        impl Drop for BadPayload {
            fn drop(&mut self) { panic!("panic payload destructor ran"); }
        }
        let pool = Pool::new().unwrap();
        let ticket = pool.spawn(|| std::panic::panic_any(BadPayload), wake()).unwrap();
        assert_eq!(done(&ticket), Err("native video owner panicked".into()));
        done(&pool.spawn(|| Ok(()), wake()).unwrap()).unwrap();
    }
}
