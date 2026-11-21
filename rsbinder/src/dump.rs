// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! [`DumpWriter`]: writes a dump to the caller's fd on a thread of its own.

use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use rustix::net::{send, SendFlags};

use crate::{CallingContext, Result, StatusCode};

/// A write that takes no byte for this long is given up; a pager a person reads keeps it.
const DEFAULT_STALL: Duration = Duration::from_secs(30);
/// Never waits in `send`; Apple has no `MSG_NOSIGNAL`, where the thread's mask is the guard.
#[cfg(not(target_vendor = "apple"))]
const SEND_FLAGS: SendFlags = SendFlags::DONTWAIT.union(SendFlags::NOSIGNAL);
#[cfg(target_vendor = "apple")]
const SEND_FLAGS: SendFlags = SendFlags::DONTWAIT;
/// A caller that retries once a second writes one refusal line a minute, not sixty.
const REFUSED_LOG_EVERY: Duration = Duration::from_secs(60);

/// Writes the text of a dump to the caller's fd on a thread of its own, one
/// dump at a time.
///
/// [`Interface::dump`](crate::Interface::dump) writes on the binder thread. A
/// caller that passes a pipe and does not read it stops that thread once the
/// pipe is full, and with it every transaction queued behind the dump. A
/// service that holds a `DumpWriter` overrides
/// [`Interface::dump_fd`](crate::Interface::dump_fd) and answers from
/// [`send_with`](Self::send_with), which returns once the text is made:
///
/// ```no_run
/// # use std::os::fd::OwnedFd;
/// # use rsbinder::{DumpWriter, Interface, Result};
/// struct Service {
///     dumps: DumpWriter,
/// }
///
/// impl Interface for Service {
///     fn dump_fd(&self, fd: OwnedFd, _args: &[String]) -> Result<()> {
///         self.dumps.send_with(fd, || Ok(b"state: ok\n".to_vec()))
///     }
/// }
/// ```
///
/// # One dump at a time
///
/// A `DumpWriter` writes for one caller at a time. A `send_with` that arrives
/// while a dump is being written answers [`StatusCode::WouldBlock`] without
/// making its text, so a caller that repeats the call costs the binder thread
/// one lock and one `close`. The writer thread exists only while a dump is
/// being written.
///
/// # The stall
///
/// The writer gives a dump up once the fd takes no byte for the stall, 30
/// seconds unless [`with_stall`](Self::with_stall) says otherwise, and closes
/// the fd; the caller reads a truncated dump and then end-of-file. The stall
/// is time without progress, not the time the whole dump may take: a reader
/// that takes a little at a time receives all of it.
///
/// The fd shares its open file description with the caller, so the writer
/// leaves its flags alone and waits with `poll` before each write of at most
/// `PIPE_BUF` bytes. That bounds the wait on a caller that merely does not
/// read a pipe or a socket, such as a pager left open. It does not bound:
///
/// - a caller that fills the same pipe from its own copy of the fd between
///   the `poll` and the `write`;
/// - a caller that passes an fd whose writes it answers itself, such as a
///   FUSE file it serves;
/// - a terminal that takes no output (a pty nobody reads, or one stopped with
///   Ctrl-S): it polls writable while it has room for less than the piece,
///   and the `write` then waits for the rest.
///
/// In each the writer thread, and so this `DumpWriter`, stays held until the
/// caller closes the fd or exits. Every other transaction of the service is
/// still answered, and what the caller holds is the dump it was allowed to
/// ask for.
///
/// # `SIGPIPE`
///
/// A caller that closed its end makes the write fail with `EPIPE`, which ends
/// the dump. A socket is sent to with `MSG_NOSIGNAL` where the platform has
/// it. The writer thread also blocks `SIGPIPE` for itself, so a process that
/// keeps the signal at its default disposition is not ended by it.
pub struct DumpWriter {
    state: Arc<Mutex<State>>,
    stall: Duration,
}

#[derive(Default)]
struct State {
    /// The caller whose dump is being written, as `uid=.. (pid=..)`; `None` when idle.
    holder: Option<String>,
    /// When a refusal was last logged, and the refusals since that wrote no line.
    refused_at: Option<Instant>,
    unlogged: u64,
}

/// Frees the slot when dropped, so a `render` that panics does not leave it held.
struct Held(Arc<Mutex<State>>);

impl Drop for Held {
    fn drop(&mut self) {
        lock(&self.0).holder = None;
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Default for DumpWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for DumpWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DumpWriter")
            .field("writing_for", &lock(&self.state).holder)
            .field("stall", &self.stall)
            .finish()
    }
}

impl DumpWriter {
    /// A writer that gives a dump up after 30 seconds without a byte taken.
    pub fn new() -> Self {
        Self::with_stall(DEFAULT_STALL)
    }

    /// A writer that gives a dump up after `stall` without a byte taken.
    pub fn with_stall(stall: Duration) -> Self {
        Self {
            state: Arc::default(),
            stall,
        }
    }

    /// Makes the text with `render` and hands it and `fd` to a writer thread.
    ///
    /// Call it on the binder thread, from
    /// [`Interface::dump_fd`](crate::Interface::dump_fd). `render` runs on that
    /// thread before `send_with` returns, so it sees the caller of the
    /// transaction and may take the locks a handler takes. The return is not
    /// the end of the dump: the writer thread closes `fd` when the text is
    /// written, and that end-of-file is what the caller waits for.
    ///
    /// # Errors
    ///
    /// - [`StatusCode::WouldBlock`] while the dump of another call is being
    ///   written. `render` is not called and `fd` is closed.
    /// - The error of `render`, with `fd` closed and nothing written.
    /// - [`StatusCode::NoMemory`] when the writer thread cannot be created.
    ///
    /// A write that fails or stalls is logged and is not an error of this
    /// call, which has returned by then.
    pub fn send_with(&self, fd: OwnedFd, render: impl FnOnce() -> Result<Vec<u8>>) -> Result<()> {
        let caller = CallingContext::current();
        let who = format!("uid={} (pid={})", caller.uid, caller.pid);
        {
            let mut state = lock(&self.state);
            let State {
                holder,
                refused_at,
                unlogged,
            } = &mut *state;
            if let Some(holder) = holder.as_deref() {
                let now = Instant::now();
                if refused_at
                    .is_some_and(|at| now.saturating_duration_since(at) < REFUSED_LOG_EVERY)
                {
                    *unlogged = unlogged.saturating_add(1);
                } else {
                    let more = std::mem::take(unlogged);
                    *refused_at = Some(now);
                    log::warn!(
                        "dump: refused {who}, the dump of {holder} is still being written \
                         ({more} more refused since the last line)"
                    );
                }
                return Err(StatusCode::WouldBlock);
            }
            *holder = Some(who.clone());
        }
        let held = Held(Arc::clone(&self.state));
        let text = render()?;
        let stall = self.stall;
        let spawned = std::thread::Builder::new()
            .name("rsbinder:dump".to_owned())
            .spawn(move || {
                block_sigpipe();
                if let Err(e) = write_within(&fd, &text, stall) {
                    log::error!("dump: writing to the fd of {who} failed: {e}");
                }
                // Closed before the slot frees: the caller's end-of-file is the dump's end.
                drop(fd);
                drop(held);
            });
        if let Err(e) = spawned {
            log::error!("dump: cannot spawn the writer thread: {e}");
            return Err(StatusCode::NoMemory);
        }
        Ok(())
    }

    #[cfg(test)]
    fn is_idle(&self) -> bool {
        lock(&self.state).holder.is_none()
    }
}

/// For this thread only: a `write` to a pipe whose reader is gone then fails with `EPIPE` alone.
fn block_sigpipe() {
    // SAFETY: `set` is zeroed, then initialized by `sigemptyset` before it is read.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGPIPE);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// Waits up to `wait` for `fd` to take a write; an error condition counts, the write reports it.
fn writable(fd: &OwnedFd, wait: Duration) -> std::io::Result<bool> {
    let mut fds = [PollFd::new(fd, PollFlags::OUT)];
    let wait = Timespec::try_from(wait).ok();
    match poll(&mut fds, wait.as_ref()) {
        Ok(ready) => Ok(ready > 0),
        Err(Errno::INTR) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// All of `text`, or an error once `stall` passes with no byte taken; the fd's flags stay as
/// the caller, who shares its open file description, set them.
fn write_within(fd: &OwnedFd, text: &[u8], stall: Duration) -> std::io::Result<()> {
    let tick = (stall / 4).clamp(Duration::from_millis(10), Duration::from_secs(1));
    let mut rest = text;
    let mut is_socket = true;
    let mut progressed = Instant::now();
    while !rest.is_empty() {
        let wrote = if is_socket {
            match send(fd, rest, SEND_FLAGS) {
                Err(Errno::NOTSOCK) => {
                    is_socket = false;
                    continue;
                }
                Err(Errno::AGAIN) => writable(fd, tick).and(Err(Errno::AGAIN.into())),
                other => other.map_err(Into::into),
            }
        } else if writable(fd, tick)? {
            // At most `PIPE_BUF`: a pipe that polls writable takes that much without waiting.
            let piece = rest.get(..rustix::pipe::PIPE_BUF).unwrap_or(rest);
            rustix::io::write(fd, piece).map_err(Into::into)
        } else {
            Err(Errno::AGAIN.into())
        };
        match wrote {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                rest = rest.get(n..).unwrap_or_default();
                progressed = Instant::now();
            }
            Err(e) => {
                let e: std::io::Error = e;
                let waiting = matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                );
                if !waiting {
                    return Err(e);
                }
                if progressed.elapsed() >= stall {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("the reader took nothing for {stall:?}"),
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const STALL: Duration = Duration::from_millis(200);

    fn text(bytes: Vec<u8>) -> impl FnOnce() -> Result<Vec<u8>> {
        move || Ok(bytes)
    }

    fn wait_until_idle(writer: &DumpWriter) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !writer.is_idle() {
            assert!(Instant::now() < deadline, "the dump writer never finished");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The caller returns while the writer waits on a reader, and a second dump is not rendered.
    #[test]
    fn a_dump_to_an_unread_fd_holds_only_its_writer() {
        const LEN: usize = 4 << 20;
        let writer = DumpWriter::new();
        let (mut reader, fd) = UnixStream::pair().expect("socketpair");
        assert_eq!(writer.send_with(fd.into(), text(vec![b'x'; LEN])), Ok(()));

        let rendered = AtomicUsize::new(0);
        let (_other_reader, other) = UnixStream::pair().expect("socketpair");
        let reply = writer.send_with(other.into(), || {
            rendered.fetch_add(1, Ordering::Relaxed);
            Ok(b"y".to_vec())
        });
        assert_eq!(reply, Err(StatusCode::WouldBlock));
        assert_eq!(
            rendered.load(Ordering::Relaxed),
            0,
            "a refused dump made its text"
        );

        let mut got = Vec::new();
        reader.read_to_end(&mut got).expect("read the dump");
        assert_eq!(got.len(), LEN);
        wait_until_idle(&writer);

        let (mut reader, fd) = UnixStream::pair().expect("socketpair");
        assert_eq!(writer.send_with(fd.into(), text(b"y".to_vec())), Ok(()));
        let mut got = Vec::new();
        reader.read_to_end(&mut got).expect("read the dump");
        assert_eq!(got, b"y");
    }

    /// A caller that never reads would otherwise hold the one writer for good.
    #[test]
    fn a_dump_nobody_reads_is_given_up_and_the_next_is_written() {
        let unread_socket = || {
            let (reader, writer) = UnixStream::pair().expect("socketpair");
            (OwnedFd::from(reader), OwnedFd::from(writer))
        };
        let unread_pipe = || rustix::pipe::pipe().expect("pipe");
        for (what, (_reader, fd)) in [("socket", unread_socket()), ("pipe", unread_pipe())] {
            let writer = DumpWriter::with_stall(STALL);
            let began = Instant::now();
            // More than a pipe (64 KiB) or a socket buffer holds: the write has to wait.
            let sent = writer.send_with(fd, text(vec![b'x'; 4 << 20]));
            assert_eq!(sent, Ok(()), "{what}");
            wait_until_idle(&writer);
            assert!(
                began.elapsed() >= STALL,
                "{what}: given up before the stall"
            );

            let (mut reader, fd) = UnixStream::pair().expect("socketpair");
            let sent = writer.send_with(fd.into(), text(b"y".to_vec()));
            assert_eq!(sent, Ok(()), "{what}: the writer is free again");
            let mut got = Vec::new();
            reader.read_to_end(&mut got).expect("read the dump");
            assert_eq!(got, b"y", "{what}");
        }
    }

    /// The stall is time without a byte taken, not the time the whole dump may take.
    #[test]
    fn a_dump_read_slowly_arrives_whole() {
        const LEN: usize = 2 << 20;
        let socket = || {
            let (reader, writer) = UnixStream::pair().expect("socketpair");
            (
                std::fs::File::from(OwnedFd::from(reader)),
                OwnedFd::from(writer),
            )
        };
        let pipe = || {
            let (reader, writer) = rustix::pipe::pipe().expect("pipe");
            (std::fs::File::from(reader), writer)
        };
        for (what, (mut reader, fd)) in [("socket", socket()), ("pipe", pipe())] {
            let writer = DumpWriter::with_stall(STALL);
            let began = Instant::now();
            assert_eq!(
                writer.send_with(fd, text(vec![b'x'; LEN])),
                Ok(()),
                "{what}"
            );
            let mut got = 0;
            let mut chunk = vec![0; 64 << 10];
            loop {
                let n = reader.read(&mut chunk).expect("read the dump");
                if n == 0 {
                    break;
                }
                got += n;
                std::thread::sleep(Duration::from_millis(25));
            }
            assert_eq!(got, LEN, "{what}");
            assert!(
                began.elapsed() > STALL * 2,
                "{what}: the read did outlast the stall"
            );
        }
    }

    /// A broken caller fd answers OK: `DEAD_OBJECT` (`-EPIPE`) would mark the service dead in AOSP.
    #[test]
    fn a_dump_to_a_broken_fd_answers_ok() {
        let writer = DumpWriter::new();
        let (reader, fd) = UnixStream::pair().expect("socketpair");
        drop(reader);
        assert_eq!(writer.send_with(fd.into(), text(b"y".to_vec())), Ok(()));
        wait_until_idle(&writer);
    }

    /// A `render` that fails or panics frees the slot, and its fd reads end-of-file.
    #[test]
    fn a_render_that_fails_leaves_the_writer_free() {
        let writer = DumpWriter::new();
        let (mut reader, fd) = UnixStream::pair().expect("socketpair");
        let reply = writer.send_with(fd.into(), || Err(StatusCode::PermissionDenied));
        assert_eq!(reply, Err(StatusCode::PermissionDenied));
        let mut got = Vec::new();
        reader.read_to_end(&mut got).expect("end-of-file");
        assert!(got.is_empty() && writer.is_idle());

        let (_reader, fd) = UnixStream::pair().expect("socketpair");
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = writer.send_with(fd.into(), || panic!("render"));
        }));
        assert!(panicked.is_err() && writer.is_idle());
    }

    /// In a child with `SIGPIPE` at `SIG_DFL`: a pipe whose reader is gone ends the dump only.
    #[test]
    // Not on s390x: CI runs it under qemu-user with no binfmt entry, so the child cannot start.
    #[cfg(not(target_arch = "s390x"))]
    fn a_dump_to_a_closed_pipe_raises_no_signal() {
        use std::os::unix::process::ExitStatusExt;
        const CHILD: &str = "RSB_DUMP_SIGPIPE_CHILD";
        const NAME: &str = "dump::tests::a_dump_to_a_closed_pipe_raises_no_signal";
        if std::env::var_os(CHILD).is_some() {
            // SAFETY: restores the default disposition; this child runs no other test.
            unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
            let writer = DumpWriter::new();
            let (reader, fd) = rustix::pipe::pipe().expect("pipe");
            drop(reader);
            assert_eq!(writer.send_with(fd, text(b"y".to_vec())), Ok(()));
            wait_until_idle(&writer);
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().expect("current_exe"))
            .args(["--exact", NAME, "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .expect("spawn the child");
        assert_eq!(out.status.signal(), None, "a signal in the child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        // Zero tests run would also exit 0: the filter must have found this test.
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{:?}\n{stdout}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
