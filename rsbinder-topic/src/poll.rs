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

/// A wait an event loop drives: a [`Subscriber`](crate::Subscriber) made with
/// [`Wake::Fd`](crate::Wake::Fd).
///
/// ```text
/// loop {
///     receive with try_recv until it returns Empty   (Closed: stop)
///     if prepare_wait() {
///         poll the wait_fds() for readability, with the loop's other fds
///         finish_wait()
///     }
/// }
/// ```
///
/// [`prepare_wait`](Self::prepare_wait) tells the publisher that the subscriber waits, then looks
/// once more; [`finish_wait`](Self::finish_wait) empties the fds and tells it the wait is over.
/// Between them the publisher's next commit makes one of the fds readable, and so do the death of
/// the publisher's process and an [`Interrupter`](crate::Interrupter). An interrupt ends no wait
/// of this kind: it stays for the next receive that waits, such as `recv`.
///
/// The fds are the same for the life of the subscriber. Register them once, level-triggered or
/// edge-triggered: `finish_wait` reads both until they have nothing left.
pub trait Wait: Sealed {
    /// The fds to poll for readability, a socket and an eventfd; `None` for a subscription made
    /// with [`Wake::Futex`](crate::Wake::Futex), which has none.
    fn wait_fds(&self) -> Option<[BorrowedFd<'_>; 2]>;

    /// Start a wait: `true` when the caller should poll, then call
    /// [`finish_wait`](Self::finish_wait). `false` when a record is unread or the publisher's
    /// process has died, and receiving returns at once; there is nothing to finish then.
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
/// `Ok(true)` when one of them may: a record was committed, the publisher's process died, or an
/// interrupt was sent; receive from each with `try_recv` until it returns
/// [`RecvError::Empty`](crate::RecvError::Empty). `Ok(false)` when `timeout` passed first; no
/// timeout waits without a bound, and so does one too long for an `Instant`. An error is what
/// `poll(2)` failed with.
///
/// # Panics
///
/// When one of `waits` was made with [`Wake::Futex`](crate::Wake::Futex).
pub fn wait_any(waits: &mut [&mut dyn Wait], timeout: Option<Duration>) -> io::Result<bool> {
    assert!(
        waits.iter().all(|wait| wait.wait_fds().is_some()),
        "wait_any: a subscription made with `Wake::Futex` has no file descriptor to poll"
    );
    let deadline = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
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
        .filter_map(|wait| wait.wait_fds())
        .flatten()
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
