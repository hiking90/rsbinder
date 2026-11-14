// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Threads for waits the executor cannot poll (`tokio` feature): `stream`'s and
//! `rsbinder-topic`'s futex and condvar waits, which hold a thread for as long as they last.
//!
//! On the runtime's capped blocking pool the waits past `max_blocking_threads` would queue,
//! seeing no wake or deadline until another wait ended, and would take the threads binder calls
//! need. [`WaitThreads`] never queues one: an idle thread takes it, else a new one starts.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use crate::{BinderAsyncPool, BoxFuture, StatusCode};

/// Threads of this crate's own for waits: an idle one takes a wait, else a new one starts, so no
/// wait queues; one idle for [`WAIT_THREAD_IDLE`] ends. Reach it through `__spawn_transact`,
/// which keeps a wait made inside a transaction on the calling thread. Not part of the API.
#[doc(hidden)]
pub enum WaitThreads {}

/// How long an idle wait thread stays, as the blocking pool's default `thread_keep_alive`.
const WAIT_THREAD_IDLE: Duration = Duration::from_secs(10);

type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    /// Threads waiting for a job; a job is queued only while one of them is left to take it.
    idle: usize,
}

struct Threads {
    queue: Mutex<Queue>,
    work: Condvar,
}

#[cfg(test)]
thread_local! {
    /// Makes [`Threads::run`] start nothing from this thread, as an OS that refuses one does.
    pub(crate) static REFUSE_WAIT_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Threads {
    fn get() -> &'static Threads {
        static THREADS: OnceLock<Threads> = OnceLock::new();
        THREADS.get_or_init(|| Threads {
            queue: Mutex::new(Queue::default()),
            work: Condvar::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `job` on an idle thread or a new one; dropped unrun if no thread can start.
    fn run(&'static self, job: Job) {
        #[cfg(test)]
        if REFUSE_WAIT_THREAD.with(std::cell::Cell::get) {
            return;
        }
        {
            let mut queue = self.lock();
            if queue.jobs.len() < queue.idle {
                queue.jobs.push_back(job);
                drop(queue);
                self.work.notify_one();
                return;
            }
        }
        let spawned = std::thread::Builder::new()
            .name("rsbinder-wait".into())
            .spawn(move || self.work_from(job));
        if let Err(e) = spawned {
            log::error!("no thread for a wait: {e}");
        }
    }

    fn work_from(&self, mut job: Job) {
        loop {
            job();
            match self.next() {
                Some(next) => job = next,
                None => return,
            }
        }
    }

    fn next(&self) -> Option<Job> {
        let mut queue = self.lock();
        loop {
            if let Some(job) = queue.jobs.pop_front() {
                return Some(job);
            }
            queue.idle += 1;
            let (guard, waited) = self
                .work
                .wait_timeout(queue, WAIT_THREAD_IDLE)
                .unwrap_or_else(|e| e.into_inner());
            queue = guard;
            queue.idle -= 1;
            if waited.timed_out() && queue.jobs.is_empty() {
                return None;
            }
        }
    }
}

impl BinderAsyncPool for WaitThreads {
    fn spawn<'a, F1, F2, Fut, A, B, E>(spawn_me: F1, after_spawn: F2) -> BoxFuture<'a, Result<B, E>>
    where
        F1: FnOnce() -> A,
        F2: FnOnce(A) -> Fut,
        Fut: Future<Output = Result<B, E>>,
        F1: Send + 'static,
        F2: Send + 'a,
        Fut: Send + 'a,
        A: Send + 'static,
        B: Send + 'a,
        E: From<StatusCode>,
    {
        let (done, ran) = tokio::sync::oneshot::channel();
        Threads::get().run(Box::new(move || {
            // Caught, so the thread lives on; resumed in the caller, as `join_error_status` does.
            let _ = done.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                spawn_me,
            )));
        }));
        Box::pin(async move {
            match ran.await {
                Ok(Ok(a)) => after_spawn(a).await,
                Ok(Err(panic)) => std::panic::resume_unwind(panic),
                // Dropped unrun: no thread could start, as a cancelled `JoinError` maps.
                Err(_) => Err(StatusCode::FailedTransaction.into()),
            }
        })
    }
}
