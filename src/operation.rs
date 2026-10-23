//! One shared permit for startup, payload updates and menu control workers.
//! Acquire before spawning a worker; Drop releases on errors and spawn failure.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::Sender,
    Arc,
};

pub(crate) struct InFlightGuard {
    flag: Arc<AtomicBool>,
    refresh: Option<Sender<()>>,
}

impl InFlightGuard {
    /// Wake rendering only AFTER releasing the permit, including early
    /// returns and a worker closure dropped by a failed thread spawn.
    pub(crate) fn wake_on_release(mut self, refresh: Option<Sender<()>>) -> Self {
        debug_assert!(
            self.refresh.is_none(),
            "wake_on_release called twice; the earlier channel would be silently dropped"
        );
        self.refresh = refresh;
        self
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
        if let Some(refresh) = &self.refresh {
            let _ = refresh.send(());
        }
    }
}

pub(crate) fn try_acquire_in_flight(flag: &Arc<AtomicBool>) -> Option<InFlightGuard> {
    flag.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .ok()
        .map(|_| InFlightGuard {
            flag: flag.clone(),
            refresh: None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendLifecycle;
    use std::sync::{atomic::AtomicUsize, Barrier};

    #[test]
    fn update_blocks_menu_restart_and_toggle_until_work_finishes() {
        let backend = BackendLifecycle::default();
        let update = try_acquire_in_flight(backend.operation_flag()).unwrap();
        // Restart and tray toggle use this exact same lifecycle-owned flag,
        // including before stop/begin_start changes the visible status.
        assert!(try_acquire_in_flight(backend.operation_flag()).is_none());
        assert!(try_acquire_in_flight(backend.operation_flag()).is_none());
        drop(update);
        let restart = try_acquire_in_flight(backend.operation_flag()).unwrap();
        assert!(try_acquire_in_flight(backend.operation_flag()).is_none());
        drop(restart);
        assert!(try_acquire_in_flight(backend.operation_flag()).is_some());
    }

    #[test]
    fn simultaneous_restart_clicks_dispatch_only_one_worker() {
        let backend = Arc::new(BackendLifecycle::default());
        let start = Arc::new(Barrier::new(3));
        let finish = Arc::new(Barrier::new(3));
        let dispatches = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let (backend, start, finish, dispatches) = (
                backend.clone(),
                start.clone(),
                finish.clone(),
                dispatches.clone(),
            );
            threads.push(std::thread::spawn(move || {
                start.wait();
                let permit = try_acquire_in_flight(backend.operation_flag());
                if permit.is_some() {
                    dispatches.fetch_add(1, Ordering::Relaxed);
                }
                finish.wait();
                drop(permit);
            }));
        }
        start.wait();
        finish.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(dispatches.load(Ordering::Relaxed), 1);
        assert!(try_acquire_in_flight(backend.operation_flag()).is_some());
    }

    #[test]
    fn cancelled_dispatch_and_panicking_worker_release_permit() {
        let backend = BackendLifecycle::default();
        let guard = try_acquire_in_flight(backend.operation_flag()).unwrap();
        let cancelled_worker = move || drop(guard);
        // A failed thread spawn drops its closure without running it.
        drop(cancelled_worker);
        let guard = try_acquire_in_flight(backend.operation_flag()).unwrap();
        let worker = std::thread::spawn(move || {
            let _guard = guard;
            panic!("simulated worker failure");
        });
        assert!(worker.join().is_err());
        assert!(try_acquire_in_flight(backend.operation_flag()).is_some());
    }

    #[test]
    fn panicking_worker_wakes_rendering_during_unwind() {
        // Production relies on the Drop-wake firing while the guard unwinds
        // out of a panicking worker (tray poll otherwise stays on 处理中…).
        let backend = BackendLifecycle::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let guard = try_acquire_in_flight(backend.operation_flag())
            .unwrap()
            .wake_on_release(Some(tx));
        let worker = std::thread::spawn(move || {
            let _guard = guard;
            panic!("simulated worker failure");
        });
        assert!(worker.join().is_err());
        rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert!(try_acquire_in_flight(backend.operation_flag()).is_some());
    }

    #[test]
    fn completion_wakes_rendering_after_permit_release() {
        let backend = BackendLifecycle::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let guard = try_acquire_in_flight(backend.operation_flag())
            .unwrap()
            .wake_on_release(Some(tx));
        assert!(rx.try_recv().is_err());
        drop(guard);
        rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert!(try_acquire_in_flight(backend.operation_flag()).is_some());
    }
}
