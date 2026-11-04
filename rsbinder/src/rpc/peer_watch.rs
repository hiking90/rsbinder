// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A thread that learns of a peer's close without reading the connection.
//!
//! A session that nothing reads between its calls (no incoming connection, no serve loop) hears of
//! its peer's end only when a later call fails. `RpcProxy::link_to_death` starts this thread on
//! such a session; `session` module doc "A peer's close" has what follows the signal.
//!
//! The thread `poll`s a duplicate of each `Outgoing` connection's socket for `POLLRDHUP`, with
//! `POLLERR` and `POLLHUP`, which `poll` always reports. It asks for no `POLLIN` on a socket, so
//! frames arriving for the calls of other threads do not wake it. It has a `Sender` to tell the
//! session's reaper and the fds, and refers to no session, so it keeps none alive; a `tests/`
//! scan holds that to this file.
//!
//! It ends at the first signal, since a socket with its flags up would have `poll` return at once
//! for good, or when [`PeerWatch`] drops, whose write end is what wakes it. A fd it holds is not
//! closed until it wakes: `close` from another thread does not wake a `poll` on that fd.
//!
//! A socket pair rather than a pipe: once the thread has ended, a pipe's `write` raises `SIGPIPE`,
//! and a process that left that signal at `SIG_DFL`, such as a C host, dies of it.

use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use rustix::event::{poll, PollFd, PollFlags};
use rustix::net::SendFlags;

/// What the session keeps of its watcher; its drop ends the thread.
pub(super) struct PeerWatch {
    /// The thread's wake: a byte says `added` has more, and the close says stop.
    wake: UnixStream,
    /// Duplicates of the sockets of slots added since the thread began.
    added: Arc<Mutex<Vec<OwnedFd>>>,
    /// How many times `poll` returned, for a test that frames do not wake it.
    #[cfg(test)]
    pub(super) polls: Arc<std::sync::atomic::AtomicUsize>,
}

impl PeerWatch {
    /// A watch of `sockets`, and what its thread runs; `signal` runs once if a socket's peer goes.
    pub(super) fn new(
        sockets: Vec<OwnedFd>,
        signal: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<(Self, impl FnOnce() + Send + 'static)> {
        let (wake, woken) = UnixStream::pair()?;
        woken.set_nonblocking(true)?;
        let added = Arc::new(Mutex::new(Vec::new()));
        #[cfg(test)]
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let body = {
            let added = added.clone();
            #[cfg(test)]
            let polls = polls.clone();
            move || {
                #[cfg(test)]
                let count = move || {
                    polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                };
                #[cfg(not(test))]
                let count = || {};
                watch(woken, &added, sockets, signal, count)
            }
        };
        let this = PeerWatch {
            wake,
            added,
            #[cfg(test)]
            polls,
        };
        Ok((this, body))
    }

    /// Have the thread watch `socket` too, if it still runs; no wait, no signal.
    pub(super) fn add(&self, socket: OwnedFd) {
        self.added
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(socket);
        // `EAGAIN`: a byte is there to be read. `EPIPE`: the thread ended.
        let flags = SendFlags::DONTWAIT | SendFlags::NOSIGNAL;
        let _ = rustix::net::send(&self.wake, &[1], flags);
    }

    /// Whether the thread has ended, which closed its end of the wake.
    #[cfg(all(test, not(target_arch = "s390x")))]
    pub(super) fn thread_ended(&self) -> bool {
        let wait = Some(std::time::Duration::ZERO);
        super::transport::poll_one(self.wake.as_fd(), PollFlags::RDHUP, wait)
            .is_ok_and(|seen| !seen.is_empty())
    }
}

/// The thread: until a socket's peer goes, or the session lets go of its [`PeerWatch`].
fn watch(
    mut woken: UnixStream,
    added: &Mutex<Vec<OwnedFd>>,
    mut sockets: Vec<OwnedFd>,
    signal: impl FnOnce(),
    count: impl Fn(),
) {
    loop {
        let mut set = Vec::with_capacity(sockets.len() + 1);
        set.push(PollFd::new(&woken, PollFlags::IN));
        set.extend(
            sockets
                .iter()
                .map(|s| PollFd::from_borrowed_fd(s.as_fd(), PollFlags::RDHUP)),
        );
        match poll(&mut set, None) {
            Ok(_) => count(),
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                log::error!("RPC: watching a session's connections failed ({e}); nothing will");
                return;
            }
        }
        if set[1..].iter().any(|s| !s.revents().is_empty()) {
            signal();
            return;
        }
        let wake = set[0].revents();
        drop(set);
        if !wake.is_empty() {
            let mut bytes = [0u8; 16];
            match woken.read(&mut bytes) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    log::error!("RPC: a session's watcher lost its wake ({e}); nothing will watch");
                    return;
                }
            }
        }
        let mut more = added
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sockets.append(&mut more);
    }
}
