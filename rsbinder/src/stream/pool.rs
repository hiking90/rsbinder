// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Handing a token to another thread (`tokio` feature), so that what the
//! executor cannot poll — a binder send on the RPC path, a futex wait on the
//! ring — does not hold an executor thread.
//!
//! Two kinds of task, two kinds of thread. A binder call ([`on_pool`]) goes to
//! the runtime's blocking pool, as every outbound call of this crate does. A
//! task that only waits — a futex wait on the ring, a credit or ping park on
//! RPC ([`on_wait_thread`], [`wait_on_thread`]) — goes to [`WaitThreads`],
//! which never queues one: a wait holds its thread for as long as it lasts,
//! and on a capped pool the waits past the cap would see no item, grant or
//! deadline until another wait ended, while also taking threads the calls need.
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
//! Every hand-off goes through `__spawn_transact` rather than to a thread
//! directly, because inside a transaction handler the work has to stay on the
//! current thread and that rule lives there.
//!
//! [`on_pool_reclaimable`] also returns the task's slot, so the owner can take
//! a queued token back later, after the future is gone: a producer's `Drop`
//! does, rather than wait for a pool thread to start a batch.
//!
//! [`wait_on_thread`] is for a wait handed over at the first poll. A future
//! dropped before a thread took the token takes it back, and the task runs
//! with nothing to do. A wait that has started keeps its token: the next wake
//! ends it.

use std::future::Future;
use std::sync::{Arc, Mutex};

use crate::error::Result;
use crate::rt::WaitThreads;
use crate::{BinderAsyncPool, BoxFuture};

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

/// Run the binder call `run` on `token` from the blocking pool; see the module doc.
pub(super) fn on_pool<R, F>(token: R, run: F) -> BoxFuture<'static, Result<()>>
where
    R: Send + 'static,
    F: FnOnce(R) -> Result<()> + Send + 'static,
{
    hand_over::<crate::Tokio, R, F>(token, run).0
}

/// [`on_pool`], and the task's slot, for an owner that gives the token up while it is queued.
pub(super) fn on_pool_reclaimable<R, F>(
    token: R,
    run: F,
) -> (BoxFuture<'static, Result<()>>, Reclaim<R>)
where
    R: Send + 'static,
    F: FnOnce(R) -> Result<()> + Send + 'static,
{
    hand_over::<crate::Tokio, R, F>(token, run)
}

/// Run the wait `run` on `token` on one of [`WaitThreads`]; see the module doc.
pub(super) fn on_wait_thread<R, F>(token: R, run: F) -> BoxFuture<'static, Result<()>>
where
    R: Send + 'static,
    F: FnOnce(R) -> Result<()> + Send + 'static,
{
    hand_over::<WaitThreads, R, F>(token, run).0
}

/// [`on_wait_thread`], and the task's slot.
pub(super) fn on_wait_thread_reclaimable<R, F>(
    token: R,
    run: F,
) -> (BoxFuture<'static, Result<()>>, Reclaim<R>)
where
    R: Send + 'static,
    F: FnOnce(R) -> Result<()> + Send + 'static,
{
    hand_over::<WaitThreads, R, F>(token, run)
}

fn hand_over<P, R, F>(token: R, run: F) -> (BoxFuture<'static, Result<()>>, Reclaim<R>)
where
    P: BinderAsyncPool,
    R: Send + 'static,
    F: FnOnce(R) -> Result<()> + Send + 'static,
{
    let mut reclaim = None;
    let handed = carry(token, run, |task| {
        reclaim = Some(Reclaim(Arc::downgrade(&task.slot)));
        crate::__spawn_transact::<P, _, _, _, _, _, _>(
            move || task.run(),
            |result| async move { result },
        )
    });
    (handed, reclaim.expect("`carry` hands the task over"))
}

/// The slot of a task [`on_pool_reclaimable`] handed over.
///
/// A queued task can wait as long as the pool's other tasks run, so an owner that must not
/// wait — a `Drop`, a call past its deadline — takes the token back with [`take`](Self::take)
/// and deals with it itself; the task then runs with nothing to do. `None` once a pool thread
/// has taken the token, or ran it inline: that run is under way or over, and ends on its own.
///
/// Weak: a pool that drops the task unrun (a runtime shut down) must drop the token with it,
/// so that it settles; a strong handle here would keep it alive and unsettled.
pub(super) struct Reclaim<R>(std::sync::Weak<Mutex<Option<R>>>);

impl<R> Reclaim<R> {
    pub(super) fn take(&self) -> Option<R> {
        let slot = self.0.upgrade()?;
        let token = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        token
    }
}

/// [`on_wait_thread`] handed over at the first poll, and taken back by a future dropped before a
/// thread took it; see the module doc.
pub(super) async fn wait_on_thread<R, F>(token: R, run: F) -> Result<()>
where
    R: Send + 'static,
    F: FnOnce(R) -> Result<()> + Send + 'static,
{
    let slot = Arc::new(Mutex::new(Some(token)));
    let mut give_back = TakeBack {
        slot: slot.clone(),
        armed: true,
    };
    let task = Carried { slot, run };
    let ran = crate::__spawn_transact::<WaitThreads, _, _, _, _, _, _>(
        move || task.run(),
        |result| async move { result },
    )
    .await;
    give_back.armed = false;
    ran
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
