//! The Apple native presenter: plays HLS playlists directly via AVPlayer / AVPlayerLayer.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::MainThreadMarker;
use objc2_av_foundation::{
    AVPlayer, AVPlayerItem, AVPlayerItemDidPlayToEndTimeNotification,
    AVPlayerItemFailedToPlayToEndTimeErrorKey, AVPlayerItemFailedToPlayToEndTimeNotification,
    AVPlayerItemStatus, AVPlayerLayer, AVPlayerTimeControlStatus,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMTime, CMTimeFlags, CMTimebase};
use objc2_foundation::{
    NSError, NSNotification, NSNotificationCenter, NSObjectProtocol, NSString, NSURL,
};
use parking_lot::{Mutex, RwLock};

use crate::apple::clock::AppleClock;
use crate::apple::util::SendSync;
use crate::backend::{Clock, NativePresenter, NativeStatus};

struct PresenterClock {
    current_tb: Mutex<Option<usize>>,
    inner: RwLock<Option<AppleClock>>,
}

impl PresenterClock {
    fn new() -> Self {
        Self {
            current_tb: Mutex::new(None),
            inner: RwLock::new(None),
        }
    }

    fn clear(&self) {
        *self.current_tb.lock() = None;
        *self.inner.write() = None;
    }

    fn set_timebase(&self, timebase: CFRetained<CMTimebase>) {
        let ptr = CFRetained::as_ptr(&timebase).as_ptr() as usize;
        let mut cur = self.current_tb.lock();
        if *cur == Some(ptr) {
            return;
        }
        *cur = Some(ptr);
        *self.inner.write() = Some(AppleClock::from_timebase(timebase));
    }
}

impl Clock for PresenterClock {
    fn now(&self) -> Option<Duration> {
        self.inner.read().as_ref().and_then(|c| c.now())
    }

    fn monotonic_ns_at(&self, at: Duration) -> Option<i64> {
        self.inner.read().as_ref().and_then(|c| c.monotonic_ns_at(at))
    }
}

fn retained_to_cf(value: Retained<CMTimebase>) -> CFRetained<CMTimebase> {
    let ptr = Retained::into_raw(value);
    unsafe { CFRetained::from_raw(std::ptr::NonNull::new_unchecked(ptr)) }
}

pub struct ApplePresenter {
    main: DispatchRetained<DispatchQueue>,
    player_layer: SendSync<Retained<AVPlayerLayer>>,
    player: Mutex<Option<SendSync<Retained<AVPlayer>>>>,
    muted: AtomicBool,
    seek_seq: AtomicU64,
    seek_done: AtomicU64,
    generation: AtomicU64,
    status: Mutex<NativeStatus>,
    on_change: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    clock: Arc<PresenterClock>,
    observers: Mutex<Vec<SendSync<Retained<ProtocolObject<dyn NSObjectProtocol>>>>>,
    ended: AtomicBool,
    notification_error: Mutex<Option<String>>,
    weak_self: Mutex<Option<Weak<Self>>>,
    /// An item is open: the poller reads its status. Idle, it leaves the
    /// main thread alone.
    active: AtomicBool,
}

unsafe impl Send for ApplePresenter {}
unsafe impl Sync for ApplePresenter {}

impl ApplePresenter {
    pub(crate) fn new(
        main: DispatchRetained<DispatchQueue>,
        player_layer: SendSync<Retained<AVPlayerLayer>>,
        muted: bool,
    ) -> Arc<Self> {
        let presenter = Arc::new(Self {
            main,
            player_layer,
            player: Mutex::new(None),
            muted: AtomicBool::new(muted),
            seek_seq: AtomicU64::new(0),
            seek_done: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            status: Mutex::new(NativeStatus::default()),
            on_change: Mutex::new(None),
            clock: Arc::new(PresenterClock::new()),
            observers: Mutex::new(Vec::new()),
            ended: AtomicBool::new(false),
            notification_error: Mutex::new(None),
            weak_self: Mutex::new(None),
            active: AtomicBool::new(false),
        });

        let weak = Arc::downgrade(&presenter);
        *presenter.weak_self.lock() = Some(weak.clone());

        std::thread::Builder::new()
            .name("apple-presenter-poller".into())
            .spawn(move || {
                while weak.upgrade().is_some() {
                    std::thread::sleep(Duration::from_millis(100));
                    let Some(presenter) = weak.upgrade() else { break; };
                    if !presenter.active.load(Ordering::SeqCst) {
                        continue;
                    }
                    let item_gen = presenter.generation.load(Ordering::SeqCst);
                    let weak_inner = presenter.weak();
                    presenter.main.exec_async(move || {
                        if let Some(this) = weak_inner.upgrade() {
                            this.poll_on_main(item_gen);
                        }
                    });
                }
            })
            .expect("spawn poller thread");

        presenter
    }

    fn weak(&self) -> Weak<Self> {
        self.weak_self.lock().clone().unwrap_or_default()
    }

    pub(crate) fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
        let weak = self.weak();
        self.main.exec_async(move || {
            let Some(this) = weak.upgrade() else { return; };
            if let Some(player) = this.player.lock().as_ref() {
                unsafe { player.0.setMuted(muted) };
            }
        });
    }

    fn fire_on_change(&self) {
        let cb = self.on_change.lock().clone();
        if let Some(cb) = cb {
            cb();
        }
    }

    fn handle_seek_completed(&self, serial: u64) {
        self.seek_done.fetch_max(serial, Ordering::Relaxed);
        let changed = {
            let mut st = self.status.lock();
            if st.seek_done < serial {
                st.seek_done = serial;
                true
            } else {
                false
            }
        };
        if changed {
            self.fire_on_change();
        }
    }

    fn handle_ended(&self, item_gen: u64) {
        if self.generation.load(Ordering::SeqCst) != item_gen {
            return;
        }
        self.ended.store(true, Ordering::Relaxed);
        let changed = {
            let mut st = self.status.lock();
            if self.generation.load(Ordering::SeqCst) != item_gen {
                return;
            }
            if !st.ended {
                st.ended = true;
                true
            } else {
                false
            }
        };
        if changed {
            self.fire_on_change();
        }
    }

    fn handle_notification_error(&self, item_gen: u64, err: String) {
        if self.generation.load(Ordering::SeqCst) != item_gen {
            return;
        }
        *self.notification_error.lock() = Some(err.clone());
        let changed = {
            let mut st = self.status.lock();
            if self.generation.load(Ordering::SeqCst) != item_gen {
                return;
            }
            if st.failed.as_deref() != Some(&err) {
                st.failed = Some(err);
                true
            } else {
                false
            }
        };
        if changed {
            self.fire_on_change();
        }
    }

    fn poll_on_main(&self, expected_gen: u64) {
        if self.generation.load(Ordering::SeqCst) != expected_gen {
            return;
        }

        let player = match self.player.lock().as_ref() {
            Some(p) => p.0.clone(),
            None => return,
        };

        let current_item = unsafe { player.currentItem() };
        let (ready, failed_item, position) = if let Some(item) = &current_item {
            let s = unsafe { item.status() };
            let ready = s == AVPlayerItemStatus::ReadyToPlay;
            let err = unsafe { item.error() };
            let failed = if s == AVPlayerItemStatus::Failed {
                Some(
                    err.map(|e| format!("{} {}: {}", e.domain(), e.code(), e.localizedDescription()))
                        .unwrap_or_else(|| "AVPlayerItemStatusFailed".to_string()),
                )
            } else {
                err.map(|e| format!("{} {}: {}", e.domain(), e.code(), e.localizedDescription()))
            };

            let ct = unsafe { item.currentTime() };
            let pos = if ct.flags.contains(CMTimeFlags::Valid) && ct.timescale > 0 && ct.value >= 0 {
                Some(Duration::from_secs_f64(ct.value as f64 / ct.timescale as f64))
            } else {
                None
            };

            if let Some(tb) = unsafe { item.timebase() } {
                if self.generation.load(Ordering::SeqCst) == expected_gen {
                    self.clock.set_timebase(retained_to_cf(tb));
                }
            }

            (ready, failed, pos)
        } else {
            (false, None, None)
        };

        let notif_err = self.notification_error.lock().clone();
        let failed = failed_item.or(notif_err);
        let (playing, waiting) = if current_item.is_some() {
            let tcs = unsafe { player.timeControlStatus() };
            (
                tcs == AVPlayerTimeControlStatus::Playing,
                tcs == AVPlayerTimeControlStatus::WaitingToPlayAtSpecifiedRate,
            )
        } else {
            (false, false)
        };
        let ended = self.ended.load(Ordering::Relaxed);
        let seek_done = self.seek_done.load(Ordering::Relaxed);
        let picture = current_item.is_some() && unsafe { self.player_layer.0.isReadyForDisplay() };

        let new_status = NativeStatus {
            ready,
            failed,
            picture,
            playing,
            waiting,
            ended,
            seek_done,
            position,
        };

        let changed = {
            let mut st = self.status.lock();
            if self.generation.load(Ordering::SeqCst) != expected_gen {
                return;
            }
            let mut updated = new_status;
            if updated.seek_done < st.seek_done {
                updated.seek_done = st.seek_done;
            }
            if *st != updated {
                *st = updated;
                true
            } else {
                false
            }
        };

        if changed {
            self.fire_on_change();
        }
    }
}

impl NativePresenter for ApplePresenter {
    fn open(&self, url: &str, start: Duration) {
        let serial = self.seek_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let item_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        // Synchronously reset stored NativeStatus fields before returning
        self.ended.store(false, Ordering::Release);
        *self.notification_error.lock() = None;
        self.clock.clear();
        self.active.store(true, Ordering::SeqCst);

        let changed = {
            let mut st = self.status.lock();
            let prev = st.clone();
            st.ready = false;
            st.failed = None;
            st.ended = false;
            st.picture = false;
            st.playing = false;
            st.waiting = false;
            st.position = None;
            *st != prev
        };
        if changed {
            self.fire_on_change();
        }

        let url_string = url.to_string();
        let weak = self.weak();
        self.main.exec_async(move || {
            let Some(this) = weak.upgrade() else { return; };
            if this.generation.load(Ordering::SeqCst) != item_gen {
                return;
            }
            let mtm = match MainThreadMarker::new() {
                Some(m) => m,
                None => return,
            };

            let mut player_guard = this.player.lock();
            let player = match player_guard.as_ref() {
                Some(p) => p.0.clone(),
                None => {
                    let p = unsafe { AVPlayer::new(mtm) };
                    unsafe {
                        p.setAllowsExternalPlayback(false);
                        let muted = this.muted.load(Ordering::Relaxed);
                        p.setMuted(muted);
                        this.player_layer.0.setPlayer(Some(&p));
                    }
                    *player_guard = Some(SendSync(p.clone()));
                    p
                }
            };
            drop(player_guard);

            let old_observers = std::mem::take(&mut *this.observers.lock());
            for obs in old_observers {
                unsafe {
                    NSNotificationCenter::defaultCenter().removeObserver(obs.0.as_ref());
                }
            }

            let ns_str = NSString::from_str(&url_string);
            let Some(ns_url) = NSURL::URLWithString(&ns_str) else {
                let err = format!("Invalid URL: {}", url_string);
                this.handle_notification_error(item_gen, err);
                return;
            };

            let item = unsafe { AVPlayerItem::playerItemWithURL(&ns_url, mtm) };
            unsafe {
                item.setPreferredForwardBufferDuration(30.0);
            }

            let weak_ended = weak.clone();
            let ended_obs = unsafe {
                NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                    Some(AVPlayerItemDidPlayToEndTimeNotification),
                    Some(item.as_ref()),
                    None,
                    &RcBlock::new(move |_note: NonNull<NSNotification>| {
                        if let Some(t) = weak_ended.upgrade() {
                            t.handle_ended(item_gen);
                        }
                    }),
                )
            };

            let weak_failed = weak.clone();
            let failed_obs = unsafe {
                NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                    Some(AVPlayerItemFailedToPlayToEndTimeNotification),
                    Some(item.as_ref()),
                    None,
                    &RcBlock::new(move |note: NonNull<NSNotification>| {
                        if let Some(t) = weak_failed.upgrade() {
                            let note_ref = note.as_ref();
                            let mut err_msg = None;
                            if let Some(user_info) = note_ref.userInfo() {
                                let key = AVPlayerItemFailedToPlayToEndTimeErrorKey;
                                let obj: Option<Retained<AnyObject>> =
                                    objc2::msg_send![&*user_info, objectForKey: &**key];
                                if let Some(obj) = obj {
                                    if let Ok(err) = obj.downcast::<NSError>() {
                                        err_msg = Some(format!(
                                            "{} {}: {}",
                                            err.domain(),
                                            err.code(),
                                            err.localizedDescription()
                                        ));
                                    }
                                }
                            }
                            let err_str = err_msg.unwrap_or_else(|| {
                                "AVPlayerItemFailedToPlayToEndTimeNotification".to_string()
                            });
                            t.handle_notification_error(item_gen, err_str);
                        }
                    }),
                )
            };

            this.observers.lock().push(SendSync(ended_obs));
            this.observers.lock().push(SendSync(failed_obs));

            unsafe {
                player.replaceCurrentItemWithPlayerItem(Some(&item));
                player.pause();
            }

            let target_cm = CMTime {
                value: start.as_nanos().min(i64::MAX as u128) as i64,
                timescale: 1_000_000_000,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            };
            let zero_cm = unsafe { objc2_core_media::kCMTimeZero };
            let weak_seek = weak.clone();
            let completion = RcBlock::new(move |_finished: Bool| {
                if let Some(t) = weak_seek.upgrade() {
                    t.handle_seek_completed(serial);
                }
            });

            unsafe {
                player.seekToTime_toleranceBefore_toleranceAfter_completionHandler(
                    target_cm,
                    zero_cm,
                    zero_cm,
                    &completion,
                );
            }
        });
    }

    fn play(&self) {
        let weak = self.weak();
        let item_gen = self.generation.load(Ordering::SeqCst);
        self.main.exec_async(move || {
            let Some(this) = weak.upgrade() else { return; };
            if this.generation.load(Ordering::SeqCst) != item_gen {
                return;
            }
            if let Some(player) = this.player.lock().as_ref() {
                unsafe { player.0.play() };
            }
            this.poll_on_main(item_gen);
        });
    }

    fn pause(&self) {
        let weak = self.weak();
        let item_gen = self.generation.load(Ordering::SeqCst);
        self.main.exec_async(move || {
            let Some(this) = weak.upgrade() else { return; };
            if this.generation.load(Ordering::SeqCst) != item_gen {
                return;
            }
            if let Some(player) = this.player.lock().as_ref() {
                unsafe { player.0.pause() };
            }
            this.poll_on_main(item_gen);
        });
    }

    fn seek(&self, to: Duration) -> u64 {
        let serial = self.seek_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let weak = self.weak();
        let item_gen = self.generation.load(Ordering::SeqCst);
        self.main.exec_async(move || {
            let Some(this) = weak.upgrade() else { return; };
            if this.generation.load(Ordering::SeqCst) != item_gen {
                this.handle_seek_completed(serial);
                return;
            }
            let Some(player) = this.player.lock().as_ref().map(|p| p.0.clone()) else {
                this.handle_seek_completed(serial);
                return;
            };
            let target_cm = CMTime {
                value: to.as_nanos().min(i64::MAX as u128) as i64,
                timescale: 1_000_000_000,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            };
            let zero_cm = unsafe { objc2_core_media::kCMTimeZero };
            let weak_seek = weak.clone();
            let completion = RcBlock::new(move |_finished: Bool| {
                if let Some(t) = weak_seek.upgrade() {
                    t.handle_seek_completed(serial);
                }
            });
            unsafe {
                player.seekToTime_toleranceBefore_toleranceAfter_completionHandler(
                    target_cm,
                    zero_cm,
                    zero_cm,
                    &completion,
                );
            }
        });
        serial
    }

    fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }

    fn status(&self) -> NativeStatus {
        let mut st = self.status.lock().clone();
        let done = self.seek_done.load(Ordering::Relaxed);
        if done > st.seek_done {
            st.seek_done = done;
        }
        if self.ended.load(Ordering::Relaxed) {
            st.ended = true;
        }
        if st.failed.is_none() {
            if let Some(err) = &*self.notification_error.lock() {
                st.failed = Some(err.clone());
            }
        }
        st
    }

    fn set_on_change(&self, on_change: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.on_change.lock() = on_change;
    }

    fn close(&self) {
        let item_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        // Synchronously reset stored NativeStatus fields before returning
        self.clock.clear();
        self.ended.store(false, Ordering::Release);
        *self.notification_error.lock() = None;
        self.active.store(false, Ordering::SeqCst);

        let changed = {
            let mut st = self.status.lock();
            let prev = st.clone();
            st.ready = false;
            st.failed = None;
            st.ended = false;
            st.picture = false;
            st.playing = false;
            st.waiting = false;
            st.position = None;
            *st != prev
        };
        if changed {
            self.fire_on_change();
        }

        let weak = self.weak();
        self.main.exec_async(move || {
            let Some(this) = weak.upgrade() else { return; };
            if this.generation.load(Ordering::SeqCst) != item_gen {
                return;
            }
            let old_observers = std::mem::take(&mut *this.observers.lock());
            for obs in old_observers {
                unsafe {
                    NSNotificationCenter::defaultCenter().removeObserver(obs.0.as_ref());
                }
            }
            if let Some(player) = this.player.lock().as_ref() {
                unsafe {
                    player.0.pause();
                    player.0.replaceCurrentItemWithPlayerItem(None);
                }
            }
        });
    }
}

impl Drop for ApplePresenter {
    fn drop(&mut self) {
        let old_observers = std::mem::take(&mut *self.observers.lock());
        if !old_observers.is_empty() {
            self.main.exec_async(move || {
                for obs in old_observers {
                    unsafe {
                        NSNotificationCenter::defaultCenter().removeObserver(obs.0.as_ref());
                    }
                }
            });
        }
    }
}
