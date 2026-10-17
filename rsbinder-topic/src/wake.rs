// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The fd wake (plan 15 §5.5a): a unix socket pair per subscription, made by the publisher, and
//! an eventfd of the subscriber's own for the wakes from its process.
//!
//! The publisher sends one byte on its end with `MSG_DONTWAIT | MSG_NOSIGNAL`. The flags are the
//! call's, so a subscriber that changes the file's flags changes nothing, and the send takes only
//! the receiving socket's spinlock, not the lock a `recv` holds while it copies, so a subscriber
//! that never reads, or reads into memory whose fault never ends, cannot hold the publisher. An
//! eventfd or a pipe the publisher writes would not do: a subscriber can make either write wait.
//!
//! A wake from the subscriber's process (a death notice, an interrupt) cannot go on the socket,
//! where it would look like the publisher's; it adds to the eventfd, whose count stays until the
//! wait drains it, so one lands whenever it is sent.
//!
//! Linux and Android only; elsewhere the types are empty and nothing makes them.

pub(crate) use imp::*;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::io;
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
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

    /// A socket pair for one subscription: the publisher's end, then the subscriber's.
    pub(crate) fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
        let (publisher, subscriber) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            None,
        )?;
        // The kernel's least: bounds what a subscriber that never reads makes this process keep.
        sockopt::set_socket_send_buffer_size(&publisher, 0)?;
        Ok((publisher, subscriber))
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

    /// A subscriber's two fds: its end of the publisher's pair, and its eventfd.
    pub(crate) struct FdWake {
        socket: OwnedFd,
        event: OwnedFd,
    }

    impl FdWake {
        /// The wake around the reply's `socket`; `None` when it is not a unix stream socket.
        pub(crate) fn new(socket: OwnedFd) -> io::Result<Option<Self>> {
            let unix = sockopt::socket_domain(&socket) == Ok(AddressFamily::UNIX);
            if !unix || sockopt::socket_type(&socket) != Ok(SocketType::STREAM) {
                return Ok(None);
            }
            let event = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
            Ok(Some(Self { socket, event }))
        }

        /// The socket, then the eventfd.
        pub(crate) fn fds(&self) -> [BorrowedFd<'_>; 2] {
            [self.socket.as_fd(), self.event.as_fd()]
        }

        /// Make the next poll of these fds return.
        pub(crate) fn poke(&self) {
            // Fails only with the count at `u64::MAX - 1`, which a poll returns for anyway.
            let _ = rustix::io::write(&self.event, &1u64.to_ne_bytes());
        }

        /// Empty both fds; `true` when the publisher's end is closed.
        pub(crate) fn drain(&self) -> bool {
            let mut bytes = [0u8; 64];
            let hung_up = loop {
                match recv(&self.socket, &mut bytes, RecvFlags::DONTWAIT) {
                    Ok((_, 0)) => break true,
                    Ok(_) => {}
                    Err(Errno::INTR) => {}
                    Err(_) => break false,
                }
            };
            let mut count = [0u8; 8];
            let _ = rustix::io::read(&self.event, &mut count);
            hung_up
        }

        /// Sleep until either fd is readable; `false` when `deadline` passed first.
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
                let mut fds = [
                    PollFd::new(&self.socket, PollFlags::IN),
                    PollFd::new(&self.event, PollFlags::IN),
                ];
                match poll(&mut fds, timeout.as_ref()) {
                    // A timeout: the next turn finds the deadline passed, or polls for the rest.
                    Ok(0) | Err(Errno::INTR) => {}
                    // Any other failure is a sleep that did not happen: the caller looks again.
                    Ok(_) | Err(_) => return true,
                }
            }
        }
    }

    /// A name subscriber's two fds, the same through its reconnections: an epoll instance that
    /// holds the current connection's fds, and an eventfd its changes of state add to.
    pub(crate) struct NamedFds {
        epoll: OwnedFd,
        event: OwnedFd,
    }

    impl NamedFds {
        pub(crate) fn new() -> io::Result<Self> {
            Ok(Self {
                epoll: epoll::create(epoll::CreateFlags::CLOEXEC)?,
                event: eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?,
            })
        }

        /// The epoll instance, then the eventfd.
        pub(crate) fn fds(&self) -> [BorrowedFd<'_>; 2] {
            [self.epoll.as_fd(), self.event.as_fd()]
        }

        /// Make the next poll of these fds return.
        pub(crate) fn poke(&self) {
            // Fails only with the count at `u64::MAX - 1`, which a poll returns for anyway.
            let _ = rustix::io::write(&self.event, &1u64.to_ne_bytes());
        }

        /// Empty the eventfd; the connection's fds are the connection's to empty.
        pub(crate) fn drain(&self) {
            let mut count = [0u8; 8];
            let _ = rustix::io::read(&self.event, &mut count);
        }

        /// Put a connection's fds in the epoll instance, both or neither.
        pub(crate) fn join(&self, fds: [BorrowedFd<'_>; 2]) -> io::Result<()> {
            let add = |fd| epoll::add(&self.epoll, fd, EventData::new_u64(0), EventFlags::IN);
            add(fds[0])?;
            if let Err(error) = add(fds[1]) {
                self.leave(fds);
                return Err(error.into());
            }
            Ok(())
        }

        /// Take a connection's fds out of the epoll instance.
        pub(crate) fn leave(&self, fds: [BorrowedFd<'_>; 2]) {
            for fd in fds {
                // Closing them takes them out too: a failure here leaves nothing behind.
                let _ = epoll::delete(&self.epoll, fd);
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod imp {
    use std::io;
    use std::os::fd::{BorrowedFd, OwnedFd};
    use std::time::Instant;

    /// No socket pair here: a publisher is never made.
    pub(crate) fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
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
        pub(crate) fn new(_: OwnedFd) -> io::Result<Option<Self>> {
            Err(io::ErrorKind::Unsupported.into())
        }

        pub(crate) fn fds(&self) -> [BorrowedFd<'_>; 2] {
            match *self {}
        }

        pub(crate) fn poke(&self) {
            match *self {}
        }

        pub(crate) fn drain(&self) -> bool {
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

        pub(crate) fn fds(&self) -> [BorrowedFd<'_>; 2] {
            match *self {}
        }

        pub(crate) fn poke(&self) {
            match *self {}
        }

        pub(crate) fn drain(&self) {
            match *self {}
        }

        pub(crate) fn join(&self, _: [BorrowedFd<'_>; 2]) -> io::Result<()> {
            match *self {}
        }

        pub(crate) fn leave(&self, _: [BorrowedFd<'_>; 2]) {
            match *self {}
        }
    }
}
