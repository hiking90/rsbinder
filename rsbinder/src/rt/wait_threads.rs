// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Threads for waits the executor cannot poll (`tokio` feature): `stream`'s and
//! `rsbinder-topic`'s futex and condvar waits, which hold a thread for as long as they last.
//!
//! On the runtime's capped blocking pool the waits past `max_blocking_threads` would queue,
//! seeing no wake or deadline until another wait ended, and would take the threads binder calls
//! need. [`WaitThreads`] never queues one behind another wait: a thread that is idle, or has just
//! ended a wait, takes it, else a new one starts. A caller that waits again as soon as it is told
//! of the last wait's end therefore keeps to one thread.

use std::collections::VecDeque;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use crate::{BinderAsyncPool, BoxFuture, StatusCode};

/// Threads of this crate's own for waits: one that is idle or has just ended a wait takes a wait,
/// else a new one starts, so no wait queues behind another; one idle for [`WAIT_THREAD_IDLE`]
/// ends. Reach it through `__spawn_transact`, which keeps a wait made inside a transaction on the
/// calling thread. Not part of the API.
#[doc(hidden)]
pub enum WaitThreads {}

/// How long an idle wait thread stays, as the blocking pool's default `thread_keep_alive`.
const WAIT_THREAD_IDLE: Duration = Duration::from_secs(10);

/// A wait, which returns what tells its caller that it ended.
type Job = Box<dyn FnOnce() -> Tell + Send>;
type Tell = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    /// Threads waiting for a job; a job is queued only while one of these or `returning` is left.
    idle: usize,
    /// Threads past a wait and on their way to look for a job, counted before the caller is told.
    returning: usize,
}

struct Threads {
    queue: Mutex<Queue>,
    work: Condvar,
    idle_for: Duration,
}

#[cfg(test)]
thread_local! {
    /// Makes [`Threads::run`] start nothing from this thread, as an OS that refuses one does.
    pub(crate) static REFUSE_WAIT_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Threads {
    fn new(idle_for: Duration) -> Self {
        Self {
            queue: Mutex::new(Queue::default()),
            work: Condvar::new(),
            idle_for,
        }
    }

    fn get() -> &'static Threads {
        static THREADS: OnceLock<Threads> = OnceLock::new();
        THREADS.get_or_init(|| Threads::new(WAIT_THREAD_IDLE))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `job` on a thread free for it or a new one; dropped unrun if no thread can start.
    fn run(&'static self, job: Job) {
        #[cfg(test)]
        if REFUSE_WAIT_THREAD.with(std::cell::Cell::get) {
            return;
        }
        {
            let mut queue = self.lock();
            if queue.jobs.len() < queue.idle + queue.returning {
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
            let tell = job();
            self.lock().returning += 1;
            // Caught: a job may be queued for this thread from here on.
            let _ = catch_unwind(AssertUnwindSafe(tell));
            match self.next() {
                Some(next) => job = next,
                None => return,
            }
        }
    }

    /// The job of a thread that has returned from a wait, or `None` once it has idled out.
    fn next(&self) -> Option<Job> {
        let mut queue = self.lock();
        queue.returning -= 1;
        loop {
            if let Some(job) = queue.jobs.pop_front() {
                return Some(job);
            }
            queue.idle += 1;
            let (guard, waited) = self
                .work
                .wait_timeout(queue, self.idle_for)
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
        Threads::get().run(Box::new(move || -> Tell {
            // Caught, so the thread lives on; resumed in the caller, as `join_error_status` does.
            let ran = catch_unwind(AssertUnwindSafe(spawn_me));
            Box::new(move || {
                let _ = done.send(ran);
            })
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::mpsc;
    use std::thread::{self, ThreadId};

    use super::*;

    /// The bound of every wait a test makes, so that a broken pool fails rather than hangs.
    const BOUND: Duration = Duration::from_secs(5);

    thread_local! {
        /// Dropped as the thread ends, which tells the receiving end.
        static ENDS: RefCell<Option<mpsc::Sender<()>>> = const { RefCell::new(None) };
    }

    fn threads(idle_for: Duration) -> &'static Threads {
        Box::leak(Box::new(Threads::new(idle_for)))
    }

    /// A job that says which thread it ran on and has nothing to tell.
    fn reports(ran_on: &mpsc::Sender<ThreadId>) -> Job {
        let ran_on = ran_on.clone();
        Box::new(move || -> Tell {
            ran_on.send(thread::current().id()).unwrap();
            Box::new(|| ())
        })
    }

    /// A job whose telling says it began and holds its thread until `release`, then runs `after`.
    fn tells_until(
        ran_on: &mpsc::Sender<ThreadId>,
        telling: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        after: impl FnOnce() + Send + 'static,
    ) -> Job {
        let ran_on = ran_on.clone();
        Box::new(move || -> Tell {
            ran_on.send(thread::current().id()).unwrap();
            Box::new(move || {
                telling.send(()).unwrap();
                release.recv().unwrap();
                after();
            })
        })
    }

    #[test]
    fn a_wait_made_as_the_last_one_is_told_goes_to_the_thread_that_told() {
        let threads = threads(WAIT_THREAD_IDLE);
        let (ran_on, ran) = mpsc::channel();
        let (telling, told) = mpsc::channel();
        let (go, release) = mpsc::channel();
        threads.run(tells_until(&ran_on, telling, release, || ()));
        told.recv_timeout(BOUND).unwrap();
        threads.run(reports(&ran_on));
        go.send(()).unwrap();
        let first = ran.recv_timeout(BOUND).unwrap();
        assert_eq!(ran.recv_timeout(BOUND), Ok(first));
    }

    #[test]
    fn a_panic_as_a_wait_is_told_leaves_its_thread_to_take_the_next() {
        let threads = threads(WAIT_THREAD_IDLE);
        let (ran_on, ran) = mpsc::channel();
        let (telling, told) = mpsc::channel();
        let (go, release) = mpsc::channel();
        let panics = || panic!("a value dropped unreceived panicked");
        threads.run(tells_until(&ran_on, telling, release, panics));
        told.recv_timeout(BOUND).unwrap();
        threads.run(reports(&ran_on));
        go.send(()).unwrap();
        let first = ran.recv_timeout(BOUND).unwrap();
        assert_eq!(ran.recv_timeout(BOUND), Ok(first));
    }

    #[test]
    fn a_wait_does_not_queue_behind_a_wait_that_has_not_ended() {
        let threads = threads(WAIT_THREAD_IDLE);
        let (ran_on, ran) = mpsc::channel();
        let (go, release) = mpsc::channel::<()>();
        let waiting = ran_on.clone();
        threads.run(Box::new(move || -> Tell {
            waiting.send(thread::current().id()).unwrap();
            release.recv().unwrap();
            Box::new(|| ())
        }));
        let first = ran.recv_timeout(BOUND).unwrap();
        threads.run(reports(&ran_on));
        let second = ran.recv_timeout(BOUND).unwrap();
        assert_ne!(first, second);
        go.send(()).unwrap();
    }

    #[test]
    fn a_thread_that_idled_out_is_not_counted_on_for_the_next_wait() {
        let threads = threads(Duration::from_millis(1));
        let (ran_on, ran) = mpsc::channel();
        let (ending, ended) = mpsc::channel();
        let reporting = ran_on.clone();
        threads.run(Box::new(move || -> Tell {
            ENDS.with_borrow_mut(|ends| *ends = Some(ending));
            reporting.send(thread::current().id()).unwrap();
            Box::new(|| ())
        }));
        let first = ran.recv_timeout(BOUND).unwrap();
        assert_eq!(
            ended.recv_timeout(BOUND),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        threads.run(reports(&ran_on));
        let second = ran
            .recv_timeout(BOUND)
            .expect("a new thread takes the wait");
        assert_ne!(second, first);
    }
}
