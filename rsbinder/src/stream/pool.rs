// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Handing a token to the blocking pool (`tokio` feature), so that a wait
//! the executor cannot poll — a binder send on the RPC path, a futex wait
//! on the ring — does not hold an executor thread.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use crate::error::Result;

/// A token and what to do with it, as the task a pool is handed. The
/// token sits in a slot rather than in the task so that [`carry`] can
/// take it back.
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
            // Dropped with the slot unlocked: settling may make binder
            // calls or wake a futex.
            drop(token);
        }
    }
}

/// Give `hand_off` a task that runs `run` on `token`. A task dropped unrun
/// drops the token with it, which is how a token settles. The one way out
/// that skips even that is a hand-off that queues the task and then
/// unwinds — `spawn_blocking` does, when the OS refuses a thread — leaving
/// the task neither run nor dropped; the token is dropped here instead.
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

/// Run `run` on `token` from the blocking pool. Handed over when this is
/// called, not when the future is polled, so dropping the future detaches
/// the task. `Err` says only that the pool did not run it; what the task
/// did is the token's to report. Through [`Tokio`](crate::Tokio) rather
/// than `spawn_blocking` directly, because inside a transaction handler
/// the work has to stay on the current thread and that rule lives there.
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

/// One of two futures with the same output, for a method that returns
/// `impl Future` and has two transports to pick from.
pub(super) enum Either<A, B> {
    A(A),
    B(B),
}

impl<A, B, O> Future for Either<A, B>
where
    A: Future<Output = O>,
    B: Future<Output = O>,
{
    type Output = O;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<O> {
        // SAFETY: structural pinning of whichever variant is in use. The
        // variant is never moved out of the enum, `Either` has no `Drop`
        // impl, and its `Unpin` is the automatic one (both variants
        // `Unpin`), so a pinned `Either` pins its variant.
        unsafe {
            match self.get_unchecked_mut() {
                Either::A(a) => Pin::new_unchecked(a).poll(cx),
                Either::B(b) => Pin::new_unchecked(b).poll(cx),
            }
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

    /// `spawn_blocking` queues the task before it finds the OS will not
    /// give it a thread, and then panics: the task is neither run nor
    /// dropped. Nothing may be left waiting on a token inside it.
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
