// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Handing a token to the blocking pool (`tokio` feature), so that a wait
//! the executor cannot poll — a binder send on the RPC path, a futex wait
//! on the ring — does not hold an executor thread.
//!
//! A token settles by being dropped, so a task dropped unrun settles its
//! token with it. [`carry`] covers the one way out that skips even that: a
//! hand-off that queues the task and then unwinds — `spawn_blocking` does,
//! when the OS refuses a thread — leaves the task neither run nor dropped,
//! and `carry` then takes the token back out of the task's slot and drops it.
//!
//! [`on_pool`] hands the task over when it is called, not when the future is
//! polled, so dropping the future detaches the task. Its `Err` says only that
//! the pool did not run the task; what the task did is the token's to report.
//! It goes through [`Tokio`](crate::Tokio) rather than `spawn_blocking`
//! directly, because inside a transaction handler the work has to stay on the
//! current thread and that rule lives there.

use std::future::Future;
use std::sync::{Arc, Mutex};

use crate::error::Result;

/// A token and the task a pool is handed; the token sits in a slot so [`carry`] can take it back.
pub(super) struct Carried<R, F> {
    slot: Arc<Mutex<Option<R>>>,
    run: F,
}

impl<R, F: FnOnce(R) -> Result<()>> Carried<R, F> {
    pub(super) fn run(self) -> Result<()> {
        let token = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        match token {
            Some(token) => (self.run)(token),
            // Taken back: the hand-off failed and the token has settled.
            None => Ok(()),
        }
    }
}

struct TakeBack<R> {
    slot: Arc<Mutex<Option<R>>>,
    armed: bool,
}

impl<R> Drop for TakeBack<R> {
    fn drop(&mut self) {
        if self.armed {
            let token = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            // Dropped with the slot unlocked: settling may make binder calls or wake a futex.
            drop(token);
        }
    }
}

/// Give `hand_off` a task that runs `run` on `token`; see the module doc for how it settles.
pub(super) fn carry<R, F, H, O>(token: R, run: F, hand_off: H) -> O
where
    H: FnOnce(Carried<R, F>) -> O,
{
    let slot = Arc::new(Mutex::new(Some(token)));
    let mut guard = TakeBack {
        slot: slot.clone(),
        armed: true,
    };
    let handed = hand_off(Carried { slot, run });
    guard.armed = false;
    handed
}

/// Run `run` on `token` from the blocking pool; see the module doc for its contract.
pub(super) fn on_pool<R, F>(token: R, run: F) -> crate::BoxFuture<'static, Result<()>>
where
    R: Send + 'static,
    F: FnOnce(R) -> Result<()> + Send + 'static,
{
    carry(token, run, |task| {
        <crate::Tokio as crate::BinderAsyncPool>::spawn(
            move || task.run(),
            |result| async move { result },
        )
    })
}

/// One of two futures with the same output, awaited by [`Either::run`].
pub(super) enum Either<A, B> {
    A(A),
    B(B),
}

impl<A, B, O> Either<A, B>
where
    A: Future<Output = O>,
    B: Future<Output = O>,
{
    /// Awaits the held future; the `async fn` state machine does the pinning.
    pub(super) async fn run(self) -> O {
        match self {
            Either::A(a) => a.await,
            Either::B(b) => b.await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What settles on drop: the test's stand-in for a batch or a record.
    struct Token(Arc<AtomicUsize>);

    impl Drop for Token {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// `spawn_blocking` can queue the task and then panic, leaving it neither run nor dropped.
    #[test]
    fn a_hand_off_that_queues_and_unwinds_gives_the_token_back() {
        let settled = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(AtomicUsize::new(0));
        let counter = ran.clone();
        let mut queued = None;
        let handed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            carry(
                Token(settled.clone()),
                move |token: Token| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    drop(token);
                    Ok(())
                },
                |task| {
                    queued = Some(task);
                    std::panic::resume_unwind(Box::new("no thread to run it"));
                },
            )
        }));
        assert!(handed.is_err());
        assert_eq!(
            settled.load(Ordering::SeqCst),
            1,
            "settled by the take-back"
        );

        // The queue is drained later, by a spawn that does find a thread.
        queued.expect("queued").run().expect("a task with no token");
        assert_eq!(ran.load(Ordering::SeqCst), 0, "the token settled once");
        assert_eq!(settled.load(Ordering::SeqCst), 1);
    }
}
