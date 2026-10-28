// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Waiting with file descriptors (plan 15 §5.5a): the three calls an event loop needs, and
//! [`wait_any`] over `poll(2)` for a thread that waits for several subscriptions.

use std::io;
use std::os::fd::BorrowedFd;
use std::time::{Duration, Instant};

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::io::Errno;

/// The longest single `poll` timeout; a longer wait polls again. `i32` seconds fit every ABI.
const MAX_POLL: Duration = Duration::from_secs(i32::MAX as u64);

/// Keeps [`Wait`] to this crate's types.
pub trait Sealed {}

/// A wait an event loop drives: a [`Subscriber`](crate::Subscriber) or a
/// [`NamedSubscriber`](crate::NamedSubscriber) made with [`Wake::Fd`](crate::Wake::Fd).
///
/// ```text
/// loop {
///     receive with try_recv until it returns Empty   (Closed: stop)
///     if prepare_wait() {
///         poll the wait_fd() for readability, with the loop's other fds
///         finish_wait()
///     }
/// }
/// ```
///
/// [`prepare_wait`](Self::prepare_wait) tells the publisher that the subscriber waits, then looks
/// once more; [`finish_wait`](Self::finish_wait) drains the fd and tells it the wait is over.
/// Between them the publisher's next commit makes the fd readable, and so do the death of the
/// publisher's process, the publisher taking the subscription out (`try_recv` then returns
/// [`Closed::Evicted`](crate::Closed::Evicted)), and an [`Interrupter`](crate::Interrupter). An
/// interrupt makes the next `prepare_wait` return `false`, and each wake it sends ends the poll in
/// progress or, sent after that poll drained the fd, a later one at once ([`wait_any`] counts the
/// calls that return for it). It is not taken: it stays for the next receive that waits, such as
/// `recv`, which returns [`RecvError::Cancelled`](crate::RecvError::Cancelled). `try_recv` does
/// not see it.
///
/// The fd is the same for the life of the subscriber, a `NamedSubscriber`'s through every
/// reconnection. Register it once, level-triggered or edge-triggered: bytes a `finish_wait` leaves
/// make the next `prepare_wait` return `false`, so no poll sleeps on readiness it was told of
/// already. A `NamedSubscriber` also ends a poll when its connection changes: a notice to return,
/// a connection to take; its `try_recv` then returns it. The connection's wake can land after a
/// receive took the connection, and then ends one poll whose `try_recv` returns `Empty`.
pub trait Wait: Sealed {
    /// The fd to poll for readability: a `Subscriber`'s end of its wake socket, or a
    /// `NamedSubscriber`'s epoll instance, which holds its current connection's fd and an
    /// eventfd of its own. `None` for a subscription made with
    /// [`Wake::Futex`](crate::Wake::Futex), which has none.
    fn wait_fd(&self) -> Option<BorrowedFd<'_>>;

    /// Start a wait: `true` when the caller should poll, then call
    /// [`finish_wait`](Self::finish_wait). `false` when a record is unread, the publisher's
    /// process has died, or the publisher took the subscription out, and receiving returns at
    /// once; when an interrupt came that no `prepare_wait` has returned `false` for, also after a
    /// poll the interrupt's wake ended, and then this call is that one; or while a publisher
    /// keeps the fd fuller than one `finish_wait` empties, as each call empties a part, so that
    /// no wait takes the thread for longer than a few reads. There is nothing to finish then.
    ///
    /// # Panics
    ///
    /// On a subscription made with [`Wake::Futex`](crate::Wake::Futex).
    fn prepare_wait(&mut self) -> bool;

    /// End the wait [`prepare_wait`](Self::prepare_wait) started, after the poll returned for
    /// any reason, or instead of polling.
    ///
    /// # Panics
    ///
    /// On a subscription made with [`Wake::Futex`](crate::Wake::Futex).
    fn finish_wait(&mut self);
}

/// Wait until one of `waits` may have a record to receive, or `timeout` passes.
///
/// `Ok(true)` when one of them may: a record was committed, the publisher's process died, the
/// publisher took the subscription out, or an interrupt was sent; receive from each with
/// `try_recv` until it returns [`RecvError::Empty`](crate::RecvError::Empty). `Ok(false)` when
/// `timeout` passed first; no timeout waits without a bound, and so does one too long for an
/// `Instant`. An error is what `poll(2)` failed with.
///
/// One interrupt makes at most two calls return for a [`Subscriber`](crate::Subscriber) and
/// three for a [`NamedSubscriber`](crate::NamedSubscriber): the one whose `prepare_wait` returns
/// `false` for it, and one for each wake it sends, which ends the call in progress or, sent after
/// that call drained the fd, a later one at once. A `Subscriber` is sent at most one wake, only
/// while a wait of it is armed, from `prepare_wait` to `finish_wait`; a `NamedSubscriber` two,
/// its own, sent whether or not a call is in progress, and its connection's. The call after them
/// sleeps until something else happens.
///
/// # Panics
///
/// When one of `waits` was made with [`Wake::Futex`](crate::Wake::Futex), and when `waits` is
/// empty and the wait has no bound: nothing would end it. Empty with a bound, it sleeps until
/// `timeout` passes and returns `Ok(false)`.
pub fn wait_any(waits: &mut [&mut dyn Wait], timeout: Option<Duration>) -> io::Result<bool> {
    assert!(
        waits.iter().all(|wait| wait.wait_fd().is_some()),
        "wait_any: a subscription made with `Wake::Futex` has no file descriptor to poll"
    );
    let deadline = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
    assert!(
        !waits.is_empty() || deadline.is_some(),
        "wait_any: no subscription to wait for, and no bound on the wait"
    );
    let mut armed = 0;
    while armed < waits.len() && waits[armed].prepare_wait() {
        armed += 1;
    }
    let polled = if armed < waits.len() {
        Ok(true)
    } else {
        poll_all(waits, deadline)
    };
    for wait in &mut waits[..armed] {
        wait.finish_wait();
    }
    polled
}

/// `poll` every fd of `waits` until one is readable or `deadline` passes.
fn poll_all(waits: &[&mut dyn Wait], deadline: Option<Instant>) -> io::Result<bool> {
    let mut fds: Vec<PollFd<'_>> = waits
        .iter()
        .filter_map(|wait| wait.wait_fd())
        .map(|fd| PollFd::from_borrowed_fd(fd, PollFlags::IN))
        .collect();
    loop {
        let timeout = match deadline {
            None => None,
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(false);
                }
                Timespec::try_from(left.min(MAX_POLL)).ok()
            }
        };
        match poll(&mut fds, timeout.as_ref()) {
            // A timeout: the next turn finds the deadline passed, or polls for the rest.
            Ok(0) | Err(Errno::INTR) => {}
            Ok(_) => return Ok(true),
            Err(error) => return Err(error.into()),
        }
    }
}
