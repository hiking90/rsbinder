// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Condvar waits with an optional deadline, the signal that wakes a thread's wait and a future's
//! together, and the gate a pool hand-off's owner joins. Shared with `rsbinder-topic` through
//! the hidden `__sync` module.
//!
//! A future waits on a [`Signal`] by making its `Notified` before it looks at the state it waits
//! for, then awaiting it: `Notify::notify_waiters` reaches every `Notified` from its creation
//! on, polled or not, so a change that lands between the look and the await still wakes it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, LockResult, Mutex, MutexGuard, PoisonError};
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

    /// A future's wait until `done`, which looks at the state; each look comes after its
    /// `Notified` is made, so a wake between a look and its await is not missed.
    #[cfg(feature = "tokio")]
    pub async fn wait_async(&self, mut done: impl FnMut() -> bool) {
        loop {
            let notified = self.notified();
            if done() {
                return;
            }
            notified.await;
        }
    }
}

/// Whether a task handed to the blocking pool has not settled, and the book it settles into.
///
/// The owner raises the gate with [`begin`](Self::begin) before it hands a task over, and only
/// while no task is out, so no [`settle`](Self::settle) or [`join`](Self::join) runs meanwhile
/// and `begin` needs no lock. The task lowers it in `settle` as it ends, after writing its
/// outcome into the book, in one critical section: a caller that sees the gate down sees the
/// outcome. A `join` holds the book's lock from its look at the gate until the condvar releases
/// it, so a `settle` cannot land between the look and the sleep.
///
/// The gate's loads and stores are `SeqCst`. The argument needs `Release` on the lowering store
/// and `Acquire` on the look; `SeqCst` is the stronger order, kept until a measurement says the
/// difference matters.
pub struct PoolGate<S> {
    busy: AtomicBool,
    book: Mutex<S>,
    signal: Signal,
    /// A test's step in a join that saw the gate up, before it sleeps.
    #[cfg(test)]
    in_join: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl<S: Default> Default for PoolGate<S> {
    fn default() -> Self {
        PoolGate {
            busy: AtomicBool::new(false),
            book: Mutex::default(),
            signal: Signal::default(),
            #[cfg(test)]
            in_join: Mutex::new(None),
        }
    }
}

impl<S> PoolGate<S> {
    /// The book; a poisoned lock is recovered, as a task that unwound settles what it can.
    pub fn lock(&self) -> MutexGuard<'_, S> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Raise the gate before handing a task over: by the owner, with no task out.
    pub fn begin(&self) {
        self.busy.store(true, Ordering::SeqCst);
    }

    /// Whether a handed-over task has not settled.
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// Write the task's outcome with `write` and lower the gate in one critical section, then
    /// wake every join.
    pub fn settle<R>(&self, write: impl FnOnce(&mut S) -> R) -> R {
        let written = {
            let mut book = self.lock();
            let written = write(&mut book);
            self.busy.store(false, Ordering::SeqCst);
            written
        };
        self.signal.wake_all();
        written
    }

    /// Block until no task is out; `false` if `deadline` passes first.
    pub fn join(&self, deadline: Option<Instant>) -> bool {
        let mut book = self.lock();
        while self.busy() {
            #[cfg(test)]
            if let Some(step) = self
                .in_join
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
            {
                step();
            }
            book = match self
                .signal
                .wait_until(book, deadline)
                .unwrap_or_else(PoisonError::into_inner)
            {
                Ok(book) => book,
                Err(_) => return false,
            };
        }
        true
    }

    /// [`join`](Self::join) for a future, without a deadline.
    #[cfg(feature = "tokio")]
    pub async fn join_async(&self) {
        self.signal.wait_async(|| !self.busy()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    /// A task that settles after a join saw it out, before the join sleeps, wakes that join.
    #[test]
    fn a_settle_as_a_join_goes_to_sleep_wakes_the_join() {
        let gate = Arc::new(PoolGate::<()>::default());
        gate.begin();
        let (at_join, joining) = mpsc::channel();
        let (ended, settled) = mpsc::channel::<()>();
        let hold = Mutex::new(Some((at_join, settled)));
        *gate.in_join.lock().unwrap() = Some(Box::new(move || {
            if let Some((at_join, settled)) = hold.lock().unwrap().take() {
                at_join.send(()).unwrap();
                // A `settle` that takes the join's lock cannot end before the join sleeps.
                let _ = settled.recv_timeout(Duration::from_millis(500));
            }
        }));
        let joining_thread = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let joined = gate.join(Some(started + Duration::from_secs(5)));
                (joined, started.elapsed())
            })
        };
        joining.recv().unwrap();
        gate.settle(|()| ());
        let _ = ended.send(());
        let (joined, took) = joining_thread.join().unwrap();
        assert!(joined);
        assert!(took < Duration::from_secs(3), "missed the wake: {took:?}");
    }

    /// The gate stays up while the settle writes, so a look that sees it down sees the book.
    #[test]
    fn the_gate_stays_up_until_the_book_is_written() {
        let gate = Arc::new(PoolGate::<u32>::default());
        gate.begin();
        let (writing, inside) = mpsc::channel();
        let (go, proceed) = mpsc::channel::<()>();
        let settling = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.settle(|book| {
                    writing.send(()).unwrap();
                    proceed.recv().unwrap();
                    *book = 7;
                })
            })
        };
        inside.recv().unwrap();
        assert!(gate.busy(), "lowered before the book was written");
        go.send(()).unwrap();
        settling.join().unwrap();
        assert!(gate.join(None));
        assert_eq!(*gate.lock(), 7);
    }

    /// A join whose deadline passes with the task out says so.
    #[test]
    fn a_join_past_its_deadline_returns_false() {
        let gate = PoolGate::<()>::default();
        gate.begin();
        assert!(!gate.join(Some(Instant::now() + Duration::from_millis(10))));
        assert!(gate.busy());
    }

    /// A change between `wait_async`'s look and its await ends the wait (the look makes it here).
    #[cfg(feature = "tokio")]
    #[test]
    fn a_wake_between_the_look_and_the_await_ends_wait_async() {
        let signal = Signal::default();
        let done = AtomicBool::new(false);
        let wait = std::pin::pin!(signal.wait_async(|| {
            let was = done.swap(true, Ordering::SeqCst);
            if !was {
                signal.wake_all();
            }
            was
        }));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(wait, &mut cx).is_ready());
    }

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
