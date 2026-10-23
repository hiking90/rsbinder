// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The fd wake (plan 15 §5.5a): a unix socket pair per subscription, made by the subscriber. It
//! hands the publisher one end, keeps a duplicate of that end, and polls the other.
//!
//! The publisher sends one byte on its end with `MSG_DONTWAIT | MSG_NOSIGNAL`. The flags are the
//! call's, so a subscriber that changes the file's flags changes nothing, and the send takes only
//! the receiving socket's spinlock, not the lock a `recv` holds while it copies, so a subscriber
//! that never reads, or reads into memory whose fault never ends, cannot hold the publisher. An
//! eventfd or a pipe the publisher writes would not do: a subscriber can make either write wait,
//! `O_NONBLOCK` being a flag of the open file that any holder may clear. The publisher also sets
//! the send buffer of the end it is given to the kernel's least, whatever the subscriber set.
//!
//! A wake from the subscriber's process (a death notice, an interrupt) is a byte sent on the
//! duplicate while a wait is `parked`, again until one is queued (`ENOBUFS`), and none while no
//! wait is (`Local::raise`): the polled end is the one fd to wait on. A byte stands for no event,
//! whose byte it was does not matter, and one drained unread loses nothing: a wake only ends a
//! sleep, and the waiter then looks at its bits and the ring. The duplicate keeps the socket open
//! after the publisher's process dies, so that death is told by the death notice, not by an end
//! of file; an end of file is a publisher that shut the socket down, taken as its death too.
//!
//! Linux and Android only; elsewhere the types are empty and nothing makes them.

pub(crate) use imp::*;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::io;
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use rustix::event::epoll::{self, EventData, EventFlags};
    use rustix::event::{eventfd, poll, EventfdFlags, PollFd, PollFlags, Timespec};
    use rustix::io::Errno;
    use rustix::net::{
        recv, send, socketpair, sockopt, AddressFamily, RecvFlags, SendFlags, SocketFlags,
        SocketType,
    };

    /// The longest single `poll` timeout; a longer wait polls again. `i32` seconds fit every ABI.
    const MAX_POLL: Duration = Duration::from_secs(i32::MAX as u64);

    /// The reads of one [`FdWake::drain`], 4 KiB in all.
    const DRAIN_READS: usize = 16;

    /// A socket pair for one subscription, which the subscriber makes: the end the publisher
    /// sends on, then the end the subscriber polls.
    pub(crate) fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
        let (publisher, subscriber) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            None,
        )?;
        limit_send_buffer(&publisher)?;
        Ok((publisher, subscriber))
    }

    /// Whether `fd` is a unix stream socket, the kind a wake is sent on.
    pub(crate) fn is_stream_socket(fd: BorrowedFd<'_>) -> bool {
        sockopt::socket_domain(fd) == Ok(AddressFamily::UNIX)
            && sockopt::socket_type(fd) == Ok(SocketType::STREAM)
    }

    /// The kernel's least send buffer: bounds the unread wakes a subscriber can make queue.
    pub(crate) fn limit_send_buffer(fd: impl AsFd) -> io::Result<()> {
        Ok(sockopt::set_socket_send_buffer_size(fd, 0)?)
    }

    /// How a wake byte went.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Sent {
        /// Queued, or a byte the subscriber has not read is queued already.
        Pending,
        /// The subscriber's end is closed.
        Closed,
        /// Not queued, for want of memory: a later commit sends again.
        Failed,
    }

    /// Send the publisher's wake on its end of the pair.
    pub(crate) fn send_wake(socket: BorrowedFd<'_>) -> Sent {
        match send(socket, &[1], SendFlags::DONTWAIT | SendFlags::NOSIGNAL) {
            Ok(_) | Err(Errno::AGAIN) => Sent::Pending,
            Err(Errno::PIPE | Errno::CONNRESET) => Sent::Closed,
            Err(_) => Sent::Failed,
        }
    }

    /// A subscriber's two fds, both of its own pair: the end it polls, and a duplicate of the
    /// end the publisher sends on, on which this process sends its own wakes. Both kinds of byte
    /// make the one fd readable, and stay until a wait drains them.
    pub(crate) struct FdWake {
        socket: OwnedFd,
        local: OwnedFd,
        /// The last drain stopped at its bound: bytes are left that no new edge announces.
        unread: AtomicBool,
        /// Set by a test: this many sends of this process queue no byte, as at `ENOBUFS`.
        #[cfg(test)]
        pub(crate) failing: std::sync::atomic::AtomicU32,
    }

    impl FdWake {
        /// The end polled, and a duplicate of the publisher's end.
        pub(crate) fn new(socket: OwnedFd, local: OwnedFd) -> io::Result<Self> {
            Ok(Self {
                socket,
                local,
                unread: AtomicBool::new(false),
                #[cfg(test)]
                failing: std::sync::atomic::AtomicU32::new(0),
            })
        }

        /// The fd to poll.
        pub(crate) fn fd(&self) -> BorrowedFd<'_> {
            self.socket.as_fd()
        }

        /// Make the next poll return; `false` when no byte was queued (`ENOBUFS`, `ENOMEM`).
        pub(crate) fn poke(&self) -> bool {
            #[cfg(test)]
            if self.failing.load(Ordering::Relaxed) > 0 {
                self.failing.fetch_sub(1, Ordering::Relaxed);
                return false;
            }
            // `EAGAIN`: a byte is queued already, and the fd readable.
            match send(&self.local, &[1], SendFlags::DONTWAIT | SendFlags::NOSIGNAL) {
                Ok(_) | Err(Errno::AGAIN) => true,
                // Shut down by the publisher (`SHUT_WR`): the polled end reads end of file.
                Err(Errno::PIPE) => true,
                Err(_) => false,
            }
        }

        /// Empty the fd; `true` when the socket was shut down, which only a publisher that does
        /// not follow the protocol does.
        ///
        /// At most [`DRAIN_READS`] reads: a publisher that keeps the socket full would hold the
        /// thread past any deadline. The bytes left keep the fd readable, and
        /// [`unread`](Self::unread) tells the next wait so, for a reactor or an edge-triggered
        /// poll that reports no readiness the fd had before. The kernel's least send buffer, the
        /// one a publisher that follows the protocol keeps, holds a few one-byte sends: one read.
        pub(crate) fn drain(&self) -> bool {
            let mut bytes = [0u8; 256];
            for _ in 0..DRAIN_READS {
                let hung_up = match recv(&self.socket, &mut bytes, RecvFlags::DONTWAIT) {
                    Ok((_, 0)) => true,
                    Ok(_) | Err(Errno::INTR) => continue,
                    Err(_) => false,
                };
                self.unread.store(false, Ordering::Relaxed);
                return hung_up;
            }
            self.unread.store(true, Ordering::Relaxed);
            false
        }

        /// Whether the last [`drain`](Self::drain) left bytes: the next wait drains, not sleeps.
        pub(crate) fn unread(&self) -> bool {
            self.unread.load(Ordering::Relaxed)
        }

        /// The duplicate of the publisher's end, for a test that sends as a publisher does.
        #[cfg(all(test, not(loom), not(miri)))]
        pub(crate) fn sender(&self) -> BorrowedFd<'_> {
            self.local.as_fd()
        }

        /// Sleep until the fd is readable; `false` when `deadline` passed first.
        pub(crate) fn sleep(&self, deadline: Option<Instant>) -> bool {
            loop {
                let timeout = match deadline {
                    None => None,
                    Some(deadline) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            return false;
                        }
                        Timespec::try_from(left.min(MAX_POLL)).ok()
                    }
                };
                let mut fds = [PollFd::new(&self.socket, PollFlags::IN)];
                match poll(&mut fds, timeout.as_ref()) {
                    // A timeout: the next turn finds the deadline passed, or polls for the rest.
                    Ok(0) | Err(Errno::INTR) => {}
                    // Any other failure is a sleep that did not happen: the caller looks again.
                    Ok(_) | Err(_) => return true,
                }
            }
        }
    }

    /// A name subscriber's fd, the same through its reconnections: an epoll instance that holds
    /// an eventfd its changes of state add to, and the current connection's fd.
    pub(crate) struct NamedFds {
        epoll: OwnedFd,
        event: OwnedFd,
    }

    impl NamedFds {
        pub(crate) fn new() -> io::Result<Self> {
            let fds = Self {
                epoll: epoll::create(epoll::CreateFlags::CLOEXEC)?,
                event: eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?,
            };
            epoll::add(
                &fds.epoll,
                &fds.event,
                EventData::new_u64(0),
                EventFlags::IN,
            )?;
            Ok(fds)
        }

        /// The epoll instance, the fd to poll.
        pub(crate) fn fd(&self) -> BorrowedFd<'_> {
            self.epoll.as_fd()
        }

        /// Make the next poll of the fd return.
        pub(crate) fn poke(&self) {
            // Fails only with the count at `u64::MAX - 1`, which a poll returns for anyway.
            let _ = rustix::io::write(&self.event, &1u64.to_ne_bytes());
        }

        /// Empty the eventfd; the connection's fds are the connection's to empty.
        pub(crate) fn drain(&self) {
            let mut count = [0u8; 8];
            let _ = rustix::io::read(&self.event, &mut count);
        }

        /// Put a connection's fd in the epoll instance.
        pub(crate) fn join(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
            Ok(epoll::add(
                &self.epoll,
                fd,
                EventData::new_u64(1),
                EventFlags::IN,
            )?)
        }

        /// Take a connection's fd out of the epoll instance.
        pub(crate) fn leave(&self, fd: BorrowedFd<'_>) {
            // Closing it takes it out too: a failure here leaves nothing behind.
            let _ = epoll::delete(&self.epoll, fd);
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod imp {
    use std::io;
    use std::os::fd::{BorrowedFd, OwnedFd};
    use std::time::Instant;

    /// No socket pair here: a subscriber is never made.
    pub(crate) fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
        Err(io::ErrorKind::Unsupported.into())
    }

    /// No wake socket here: a publisher takes no subscription.
    pub(crate) fn is_stream_socket(_: BorrowedFd<'_>) -> bool {
        false
    }

    pub(crate) fn limit_send_buffer(_: impl std::os::fd::AsFd) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Sent {
        #[expect(
            dead_code,
            reason = "no socket pair is made here, so no send queues a byte"
        )]
        Pending,
        #[expect(
            dead_code,
            reason = "no socket pair is made here, so no send finds one closed"
        )]
        Closed,
        Failed,
    }

    pub(crate) fn send_wake(_: BorrowedFd<'_>) -> Sent {
        Sent::Failed
    }

    /// None exists here: a subscriber is never made.
    pub(crate) enum FdWake {}

    impl FdWake {
        pub(crate) fn new(_: OwnedFd, _: OwnedFd) -> io::Result<Self> {
            Err(io::ErrorKind::Unsupported.into())
        }

        pub(crate) fn fd(&self) -> BorrowedFd<'_> {
            match *self {}
        }

        pub(crate) fn poke(&self) -> bool {
            match *self {}
        }

        pub(crate) fn drain(&self) -> bool {
            match *self {}
        }

        pub(crate) fn unread(&self) -> bool {
            match *self {}
        }

        pub(crate) fn sleep(&self, _: Option<Instant>) -> bool {
            match *self {}
        }
    }

    /// None exists here: a name subscriber connects to nothing.
    pub(crate) enum NamedFds {}

    impl NamedFds {
        pub(crate) fn new() -> io::Result<Self> {
            Err(io::ErrorKind::Unsupported.into())
        }

        pub(crate) fn fd(&self) -> BorrowedFd<'_> {
            match *self {}
        }

        pub(crate) fn poke(&self) {
            match *self {}
        }

        pub(crate) fn drain(&self) {
            match *self {}
        }

        pub(crate) fn join(&self, _: BorrowedFd<'_>) -> io::Result<()> {
            match *self {}
        }

        pub(crate) fn leave(&self, _: BorrowedFd<'_>) {
            match *self {}
        }
    }
}
