// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Condvar waits with an optional deadline, and the signal that wakes a thread's wait and a
//! future's together. Shared with `rsbinder-topic` through the hidden `__sync` module.
//!
//! A future waits on a [`Signal`] by making its `Notified` before it looks at the state it waits
//! for, then awaiting it: `Notify::notify_waiters` reaches every `Notified` from its creation
//! on, polled or not, so a change that lands between the look and the await still wakes it.

use std::sync::{Condvar, LockResult, MutexGuard, PoisonError};
use std::time::Instant;

/// What one wait hands back: `Ok` after a wake, a timeout or a spurious return, which the
/// caller tells apart by looking again; `Err` unwaited, once the deadline has passed.
pub type Waited<'a, T> = Result<MutexGuard<'a, T>, MutexGuard<'a, T>>;

/// One wait on `cv`, without a bound for `None`.
///
/// The `LockResult` is the condvar's, so each caller keeps its own poison policy.
pub fn wait_until<'a, T>(
    cv: &Condvar,
    guard: MutexGuard<'a, T>,
    deadline: Option<Instant>,
) -> LockResult<Waited<'a, T>> {
    let Some(deadline) = deadline else {
        return match cv.wait(guard) {
            Ok(guard) => Ok(Ok(guard)),
            Err(poisoned) => Err(PoisonError::new(Ok(poisoned.into_inner()))),
        };
    };
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Ok(Err(guard));
    }
    match cv.wait_timeout(guard, left) {
        Ok((guard, _)) => Ok(Ok(guard)),
        Err(poisoned) => Err(PoisonError::new(Ok(poisoned.into_inner().0))),
    }
}

/// A condvar for threads and, with `tokio`, a `Notify` for futures, woken together.
///
/// The mutex is the caller's: a waker changes the state under it, releases it, then wakes.
#[derive(Default)]
pub struct Signal {
    cv: Condvar,
    #[cfg(feature = "tokio")]
    notify: tokio::sync::Notify,
}

impl Signal {
    /// Wake every waiting thread and every future that made its `Notified`.
    pub fn wake_all(&self) {
        self.cv.notify_all();
        #[cfg(feature = "tokio")]
        self.notify.notify_waiters();
    }

    /// Wake the waiting threads only.
    ///
    /// For a wake that concerns a thread's wait alone: a future that made its `Notified` to
    /// wait for the state, and holds it across a step that may trigger this wake, would see it
    /// complete at once and look again for nothing.
    pub fn wake_threads(&self) {
        self.cv.notify_all();
    }

    /// One thread's wait on this signal; see [`wait_until`].
    pub fn wait_until<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        deadline: Option<Instant>,
    ) -> LockResult<Waited<'a, T>> {
        wait_until(&self.cv, guard, deadline)
    }

    /// A future's wait: make this before looking at the state, then await it (module doc).
    #[cfg(feature = "tokio")]
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    #[test]
    fn a_passed_deadline_hands_the_guard_back_unwaited() {
        let (mutex, cv) = (Mutex::new(7), Condvar::new());
        let past = Instant::now();
        let waited = wait_until(&cv, mutex.lock().unwrap(), Some(past)).unwrap();
        assert_eq!(
            *waited.expect_err("no wait once the deadline has passed"),
            7
        );
    }

    #[test]
    fn a_wait_with_a_deadline_returns_by_it() {
        let (mutex, cv) = (Mutex::new(()), Condvar::new());
        let started = Instant::now();
        let deadline = started + Duration::from_millis(20);
        let mut guard = mutex.lock().unwrap();
        while let Ok(again) = wait_until(&cv, guard, Some(deadline)).unwrap() {
            guard = again;
        }
        assert!(started.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn a_wake_ends_a_wait_without_a_bound() {
        let state = std::sync::Arc::new((Mutex::new(false), Signal::default()));
        let waker = {
            let state = state.clone();
            std::thread::spawn(move || {
                *state.0.lock().unwrap() = true;
                state.1.wake_all();
            })
        };
        let mut done = state.0.lock().unwrap();
        while !*done {
            done = state
                .1
                .wait_until(done, None)
                .unwrap()
                .expect("no deadline");
        }
        waker.join().unwrap();
    }

    #[test]
    fn a_poisoned_wait_hands_the_guard_to_the_caller() {
        let mutex = std::sync::Arc::new(Mutex::new(1));
        let poisoner = mutex.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        let cv = Condvar::new();
        let guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
        let waited = wait_until(&cv, guard, Some(Instant::now() + Duration::from_millis(1)));
        let guard = waited.unwrap_or_else(PoisonError::into_inner);
        assert_eq!(*guard.unwrap_or_else(|g| g), 1);
    }
}
